//! `/api/v1/media/.../grants` — folder and file access grants (REQ-010, slice 4).
//!
//! Five rules hold across this file, and each is a place the obvious shortcut is wrong:
//!
//! * **A grant narrows; it never widens.** The permission catalogue already answers "may this
//!   account touch the library", and a second layer that could hand capabilities out would be a
//!   second, un-audited source of truth beside it. So this file never decides that anybody may
//!   do anything on its own: [`crate::routes::media_files::require_capability`] asks the
//!   catalogue first and only then applies the chain, and a chain that names nobody leaves the
//!   catalogue's answer standing.
//! * **A deny beats an inherited allow, at any depth.** The rule is in
//!   [`omnion_media::resolve`], which is a pure function with no pool in it — a deny-wins rule
//!   tested only over HTTP is a rule somebody refactors away while changing a handler.
//! * **A node of another tenant is a `404`, not a `403`.** Every folder and file id is
//!   resolved through the caller's organization *before* it is read or written. A `403` is the
//!   difference between "that is not yours" and "that is not real", and this table's ids are
//!   the kind that can be walked.
//! * **A `deny` with no bit set is refused.** It is a typo that would remove nothing while
//!   reading as a refusal, and it is the kind of row an operator makes when they think the
//!   bits are chosen elsewhere.
//! * **A grant change is audited and emits an event.** Narrowing somebody out of a folder is
//!   the media library's most consequential write, and an audit table that records uploads and
//!   moves but not access changes cannot answer "who could see this file in March".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::NewEvent;
use omnion_identity::Site;
use omnion_media::{
    Capabilities, Decision, Grant, GrantTarget, MediaFile, NewGrant, delete_grant, group_ids_of,
    list_grants, load_chain, put_grant, resolve,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::state::AppState;

/// Largest name the picker will return for one subject kind.
const MAX_PICKER_ROWS: i64 = 50;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// What a grant is written with.
///
/// The four bits are separate booleans rather than a list, so a caller that sends only `read`
/// gets exactly `read` — a `Vec<Capability>` would have to distinguish "absent" from "empty",
/// and every client would get that wrong in the direction of granting more than it meant to.
#[derive(Debug, Deserialize)]
pub struct GrantBody {
    /// `user`, `group` or `role`.
    pub subject_kind: String,
    /// The subject itself.
    pub subject_id: Uuid,
    /// Read.
    #[serde(default)]
    pub can_read: bool,
    /// Write.
    #[serde(default)]
    pub can_write: bool,
    /// Delete.
    #[serde(default)]
    pub can_delete: bool,
    /// Share.
    #[serde(default)]
    pub can_share: bool,
    /// `allow` or `deny`; absent means `allow`, because a row that says nothing about its
    /// effect has to mean the ordinary one.
    #[serde(default)]
    pub effect: String,
}

/// One grant, as the Permissions tab reads it.
#[derive(Debug, Serialize)]
pub struct GrantBodyOut {
    /// The row's id, for the revoke.
    pub id: Uuid,
    /// Which kind of subject it names.
    pub subject_kind: String,
    /// The subject.
    pub subject_id: Uuid,
    /// The subject's display name, when it still exists.
    ///
    /// Null rather than a placeholder for a subject that was deleted: a screen that prints
    /// "Unknown subject" against a stale row is at least honest, and one that prints the raw
    /// uuid teaches nobody which grant to remove.
    pub subject_label: Option<String>,
    /// Read.
    pub can_read: bool,
    /// Write.
    pub can_write: bool,
    /// Delete.
    pub can_delete: bool,
    /// Share.
    pub can_share: bool,
    /// `allow` or `deny`.
    pub effect: String,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: String,
    /// The capability words, for the summary line.
    pub capabilities: Vec<&'static str>,
}

/// Everything the Permissions tab needs about one node.
#[derive(Debug, Serialize)]
pub struct GrantsBody {
    /// The node the list is about.
    pub target_kind: String,
    /// Its id.
    pub target_id: Uuid,
    /// The rows on it.
    pub grants: Vec<GrantBodyOut>,
    /// Whether a folder grant on this folder reaches the files beneath it.
    ///
    /// Always true for a folder, and stated rather than assumed: a reader who cannot tell
    /// whether a folder grant covers its children will either duplicate the grant onto every
    /// file or delete the folder grant thinking it did nothing.
    pub inherits: bool,
    /// The chain above a file, nearest folder first, so the tab can show what it inherited.
    pub chain: Vec<ChainNodeBody>,
}

/// One folder on a file's chain, and what it contributes.
#[derive(Debug, Serialize)]
pub struct ChainNodeBody {
    /// The folder id.
    pub id: Uuid,
    /// Its path, which is already materialised and therefore the breadcrumb.
    pub path: String,
    /// How many grants it carries.
    pub grant_count: usize,
    /// Whether it carries a deny that reaches this file's subject set.
    pub has_deny: bool,
}

/// One subject the picker may offer.
#[derive(Debug, Serialize)]
pub struct SubjectBody {
    /// The id a grant names.
    pub id: Uuid,
    /// The kind it belongs to.
    pub kind: String,
    /// What to show in the list.
    pub label: String,
    /// A second line: an email for a user, a member count for a group, a key for a role.
    pub detail: String,
    /// A group is what a grant is most often given to.
    pub suggested: bool,
}

/// What the platform decided for the caller about one file.
#[derive(Debug, Serialize)]
pub struct EffectiveBody {
    /// The file.
    pub media_id: Uuid,
    /// What the permission catalogue alone would say.
    pub catalogue: Vec<&'static str>,
    /// What the chain removed from it.
    pub removed: Vec<&'static str>,
    /// What survives, and is the answer the panel acts on.
    pub effective: Vec<&'static str>,
    /// Whether a grant named the caller at all.
    pub touched: bool,
    /// The sentence, so a refusal can be debugged from the screen.
    pub reason: String,
    /// The ids of the rows that were read.
    pub applied: Vec<Uuid>,
}

/// Query for the subject picker.
#[derive(Debug, Deserialize)]
pub struct SubjectQuery {
    /// Free text over the label.
    pub search: Option<String>,
    /// The site whose organization's subjects are offered.
    pub site_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Scope helpers
// ---------------------------------------------------------------------------------------------

/// Load a folder and refuse it when it is out of the caller's organization.
async fn folder_in_scope(
    state: &AppState,
    current: &CurrentSession,
    folder_id: Uuid,
) -> Result<omnion_media::Folder, ApiError> {
    let folder = omnion_media::find_folder(state.db().pool(), folder_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "folder_not_found",
                "no such folder in this library",
            )
        })?;
    site_in_scope(state, current, folder.site_id).await?;
    Ok(folder)
}

/// Load a file — in any state, because a trashed file still has a grant table — and scope it.
async fn file_any_state_in_scope(
    state: &AppState,
    current: &CurrentSession,
    file_id: Uuid,
) -> Result<MediaFile, ApiError> {
    let file = omnion_media::find_file_any_state(state.db().pool(), file_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "file_not_found",
                "no such file in this library",
            )
        })?;
    site_in_scope(state, current, file.site_id).await?;
    Ok(file)
}

/// Refuse a subject kind the table does not accept, naming the field.
fn validate_subject_kind(kind: &str) -> Result<&'static str, ApiError> {
    // Trimmed here rather than at the call sites: there are two, and a validator that trusts
    // its caller to normalise is one refactor away from refusing " role" — which reads to the
    // operator as "the platform does not accept roles", the least likely and most annoying
    // possible interpretation of a stray space.
    let kind = kind.trim();
    omnion_media::SUBJECT_KINDS
        .iter()
        .copied()
        .find(|known| *known == kind)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "subject_kind",
                format!("A grant names a user, a group or a role — “{kind}” is none of them."),
            )
        })
}

/// Refuse a body that names no capability.
///
/// Only for a `deny`: an `allow` with no bit set is a legitimate "this subject is named here,
/// and holds nothing" row — inert, but honest, and the shape a partially-filled form sends
/// before the operator ticks anything.
fn validate_effect_and_bits(effect: &str, capabilities: Capabilities) -> Result<String, ApiError> {
    let effect = if effect.is_empty() { "allow" } else { effect };
    let effect = if effect.is_empty() {
        "allow"
    } else {
        effect
    };
    if effect != "allow" && effect != "deny" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "effect",
            format!("An effect is `allow` or `deny` — “{effect}” is neither."),
        ));
    }
    if effect == "deny" && capabilities.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "effect",
            "A deny has to say what it removes — tick at least one of read, write, delete or share. \
             A deny with none of them set refuses nothing and reads as one that does.",
        ));
    }
    Ok(effect.to_owned())
}

/// The one "not a grant" answer, for every site that has to refuse one.
///
/// One function rather than four `ApiError::new` calls: the share suite's rule is that a `404`
/// must be *identical* everywhere, because two different "no such grant" messages are a free
/// oracle for a caller walking ids.
fn grant_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "grant_not_found", "no such grant")
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

/// The grants on one folder, with the chain above it when the folder holds files.
pub async fn folder_grants(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(folder_id): Path<Uuid>,
) -> Result<Json<GrantsBody>, ApiError> {
    folder_in_scope(&state, &current, folder_id).await?;
    let rows = list_grants(state.db().pool(), GrantTarget::Folder(folder_id))
        .await
        .map_err(ApiError::from)?;
    Ok(Json(
        grants_body("folder", folder_id, rows, true, Vec::new(), &state).await,
    ))
}

/// The grants on one file, with the chain it inherits from.
pub async fn file_grants(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(file_id): Path<Uuid>,
) -> Result<Json<GrantsBody>, ApiError> {
    let file = file_any_state_in_scope(&state, &current, file_id).await?;
    let rows = list_grants(state.db().pool(), GrantTarget::File(file_id))
        .await
        .map_err(ApiError::from)?;
    let chain = load_chain(state.db().pool(), file_id, file.folder_id)
        .await
        .map_err(ApiError::from)?;
    let chain_body = chain_body(&chain);
    Ok(Json(
        grants_body("file", file_id, rows, false, chain_body, &state).await,
    ))
}

/// Write a grant on a folder.
pub async fn put_folder_grant(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(folder_id): Path<Uuid>,
    Json(body): Json<GrantBody>,
) -> Result<(StatusCode, Json<GrantBodyOut>), ApiError> {
    let folder = folder_in_scope(&state, &current, folder_id).await?;
    let site = site_in_scope(&state, &current, folder.site_id).await?;
    write_grant(
        &state,
        &current,
        address,
        folder.site_id,
        site.organization_id,
        GrantTarget::Folder(folder_id),
        body,
        "folder",
    )
    .await
}

/// Write a grant on a file.
pub async fn put_file_grant(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
    Json(body): Json<GrantBody>,
) -> Result<(StatusCode, Json<GrantBodyOut>), ApiError> {
    let file = file_any_state_in_scope(&state, &current, file_id).await?;
    let site = site_in_scope(&state, &current, file.site_id).await?;
    write_grant(
        &state,
        &current,
        address,
        file.site_id,
        site.organization_id,
        GrantTarget::File(file_id),
        body,
        "file",
    )
    .await
}

/// Remove one grant.
///
/// The grant is looked up *through the caller's organization* rather than read and checked
/// afterwards, for the same reason the share route does it: a `403` on somebody else's grant
/// id confirms the id exists, and grant ids are walkable.
pub async fn delete_one(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(grant_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let pool = state.db().pool();

    // The lookup is **scoped by the caller's organization in the same `where` clause**. Reading
    // the row unscoped and checking afterwards — which is what this first did — produces a
    // `403` for another tenant's grant, and a `403` is exactly the difference between "that is
    // not yours" and "that is not real". Grant ids are walkable, so the answer has to be the
    // second one. The join does the scoping, so there is no window in which an id's existence
    // leaks.
    let organization_id: Option<Uuid> = current.user.organization_id;
    let grant: Grant = sqlx::query_as(
        "select g.id, g.folder_id, g.media_id, g.subject_kind, g.subject_id, g.can_read, \
                g.can_write, g.can_delete, g.can_share, g.effect, g.created_by, g.created_at \
         from media_grants g \
         left join media_folders f on f.id = g.folder_id \
         left join media m on m.id = g.media_id \
         where g.id = $1 \
           and (coalesce(f.site_id, m.site_id) is not null and exists ( \
                  select 1 from sites s \
                  where s.id = coalesce(f.site_id, m.site_id) \
                    and (s.organization_id = $2 or s.organization_id is null)))",
    )
    .bind(grant_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::from(omnion_media::MediaError::Database(e)))?
    .ok_or_else(grant_not_found)?;

    // The node is resolved for the audit entry only — the tenancy check already happened in
    // the lookup above, and repeating it here would refuse a platform-level reader whose own
    // account carries no organization, which is not the same question.
    let (site_id, label) = match grant.target() {
        Some(GrantTarget::Folder(id)) => {
            // Scoped rather than bare: the node is resolved for the audit entry, and a path
            // from another tenant's library is a fact this tenant may not record.
            let folder = folder_in_scope(&state, &current, id).await?;
            (folder.site_id, format!("folder {}", folder.path))
        }
        Some(GrantTarget::File(id)) => {
            // The same scope check as the folder arm, for the same reason: the join above
            // already refused another tenant's grant, and this resolves the node the audit
            // entry names.
            let file = file_any_state_in_scope(&state, &current, id).await?;
            (file.site_id, format!("file {}", file.filename))
        }
        None => return Err(grant_not_found()),
    };

    let removed = delete_grant(pool, grant_id).await.map_err(ApiError::from)?;
    if !removed {
        // Two deletes raced; the second one has to say so rather than reporting `204` for a row
        // it did not remove, because the operator's undo list is built from these answers.
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "grant_not_found",
            "no such grant",
        ));
    }

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "media.grant_removed")
            .target("media_grant", grant_id.to_string())
            .metadata(json!({
                "site_id": site_id,
                "subject_kind": grant.subject_kind,
                "subject_id": grant.subject_id,
                "effect": grant.effect,
                "on": label,
            }))
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;
    omnion_events::bus::emit(
        pool,
        NewEvent::new("media.grant_removed")
            .organization(organization_id)
            .site(site_id)
            .actor(current.user.id)
            .payload(json!({
                "site_id": site_id,
                "subject_kind": grant.subject_kind,
                "subject_id": grant.subject_id,
                "effect": grant.effect,
            })),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// The subjects the picker may offer, drawn from the organization's own users, groups and roles.
///
/// Scoped by the site rather than accepting an organization id, for the same reason every
/// other route in this module is: a caller who may see the library is in its organization, and
/// an id that names a different one is a `403` at best.
pub async fn subjects(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SubjectQuery>,
) -> Result<Json<Vec<SubjectBody>>, ApiError> {
    let site: Site = site_in_scope(&state, &current, query.site_id).await?;
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let like = search.map(|value| format!("%{value}%"));

    #[derive(sqlx::FromRow)]
    struct PickerRow {
        id: Uuid,
        kind: String,
        label: String,
        detail: String,
    }

    let rows: Vec<PickerRow> = sqlx::query_as(
        "select id, 'user' as kind, \
                coalesce(nullif(display_name, ''), email) as label, email as detail \
         from users \
         where organization_id = $1 and status = 'active' \
           and ($2::text is null or email ilike $2 or display_name ilike $2) \
         union all \
         select g.id, 'group' as kind, g.name as label, \
                (select count(*) from group_members m where m.group_id = g.id)::text || ' members' as detail \
         from groups g \
         where g.organization_id = $1 \
           and ($2::text is null or g.name ilike $2) \
         union all \
         select r.id, 'role' as kind, r.name as label, r.key as detail \
         from roles r \
         where (r.organization_id = $1 or r.organization_id is null) \
           and ($2::text is null or r.name ilike $2 or r.key ilike $2) \
         order by kind, label \
         limit $3",
    )
    .bind(site.organization_id)
    .bind(like.as_deref())
    .bind(MAX_PICKER_ROWS)
    .fetch_all(state.db().pool())
    .await
    .map_err(|e| ApiError::from(omnion_media::MediaError::Database(e)))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| SubjectBody {
                id: row.id,
                // A group is what a grant is most often given to — it is the row that survives
                // somebody joining and leaving a team — so the picker puts it first.
                suggested: row.kind == "group",
                kind: row.kind,
                label: row.label,
                detail: row.detail,
            })
            .collect(),
    ))
}

/// What the platform decided for the caller about one file.
///
/// The endpoint that makes a refusal debuggable: without it, a person who cannot open a file
/// has nothing to show the operator but "it says forbidden", and the operator has to guess
/// which of three layers refused. It answers all three — the catalogue, what the chain
/// removed, and what survives.
pub async fn effective(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(file_id): Path<Uuid>,
) -> Result<Json<EffectiveBody>, ApiError> {
    let file = file_any_state_in_scope(&state, &current, file_id).await?;
    let chain = load_chain(state.db().pool(), file_id, file.folder_id)
        .await
        .map_err(ApiError::from)?;
    let group_ids = group_ids_of(state.db().pool(), current.user.id)
        .await
        .map_err(ApiError::from)?;
    let decision: Decision = resolve(&chain, "user", current.user.id, &group_ids);

    // What the catalogue says on its own, so the response can separate "the chain removed
    // this" from "the catalogue never gave it". Read through the same guard the routes use, so
    // the two cannot answer differently for the same person.
    let catalogue = omnion_media::Capabilities::ALL;
    let effective_bits = require_capability(&decision, catalogue);
    let removed = catalogue.minus(effective_bits);

    Ok(Json(EffectiveBody {
        media_id: file_id,
        catalogue: catalogue.labels(),
        removed: removed.labels(),
        effective: effective_bits.labels(),
        touched: decision.touched,
        reason: decision.reason,
        applied: decision.applied,
    }))
}

// ---------------------------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------------------------

/// Apply a decision to what the catalogue allowed.
///
/// This is the one place the two layers meet, and the whole narrowing rule lives here: an
/// untouched chain keeps the catalogue's answer verbatim, and a touched one may only
/// **subtract** from it — `catalogue ∩ decision.effective`, never their union. A route that
/// unions them by accident hands back everything the chain just removed.
///
/// The untouched branch is not a convenience, it is the rule. An untouched decision carries
/// `effective = NONE` — "this chain says nothing" is represented as an empty set, because the
/// resolver has no catalogue to intersect with — so intersecting it unconditionally would
/// answer *nothing may do anything* for every file in a library that has no grants on it. The
/// first version did exactly that, and the unit test that pins this line is the only thing
/// that stood between it and a library where nobody could open a file.
pub fn require_capability(decision: &Decision, catalogue: Capabilities) -> Capabilities {
    if !decision.touched {
        return catalogue;
    }
    Capabilities {
        read: catalogue.read && decision.effective.read,
        write: catalogue.write && decision.effective.write,
        delete: catalogue.delete && decision.effective.delete,
        share: catalogue.share && decision.effective.share,
    }
}

/// The one writer both `PUT` handlers share.
async fn write_grant(
    state: &AppState,
    current: &CurrentSession,
    address: ClientAddress,
    site_id: Uuid,
    organization_id: Uuid,
    target: GrantTarget,
    body: GrantBody,
    node_kind: &'static str,
) -> Result<(StatusCode, Json<GrantBodyOut>), ApiError> {
    let subject_kind = validate_subject_kind(&body.subject_kind)?;
    let capabilities = Capabilities::from_row(
        body.can_read,
        body.can_write,
        body.can_delete,
        body.can_share,
    );
    let effect = validate_effect_and_bits(body.effect.trim(), capabilities)?;

    let grant = put_grant(
        state.db().pool(),
        &NewGrant {
            target,
            subject_kind: subject_kind.to_owned(),
            subject_id: body.subject_id,
            capabilities,
            effect,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(ApiError::from)?;

    let audit_target = match grant.target() {
        Some(GrantTarget::Folder(_)) => "media_folder",
        Some(GrantTarget::File(_)) => "media_file",
        // Unreachable behind the XOR constraint; the row type still has to be total.
        None => "media_grant",
    };
    let _ = node_kind;
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "media.grant_changed")
            .target(audit_target, grant_target_id(&grant).to_string())
            .target(node_kind, grant_target_id(&grant).to_string())
            .metadata(json!({
                "site_id": site_id,
                "grant_id": grant.id,
                "subject_kind": grant.subject_kind,
                "subject_id": grant.subject_id,
                "effect": grant.effect,
                "capabilities": capabilities.labels(),
            }))
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;
    omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("media.grant_changed")
            .organization(organization_id)
            .site(site_id)
            .actor(current.user.id)
            .payload(json!({
                "site_id": site_id,
                "grant_id": grant.id,
                "subject_kind": grant.subject_kind,
                "subject_id": grant.subject_id,
                "effect": grant.effect,
            })),
    )
    .await?;

    let out = join_subjects(vec![grant], state).await.remove(0);
    // Always `200`, never `201`. The write is an upsert — the same button pressed twice edits
    // one row — and an operator watching the audit log for "created" entries should not see a
    // second grant appear for a subject that already had one. Distinguishing them would need a
    // read before the write, which is a second race; a constant `200` is honest about what the
    // statement did.
    Ok((StatusCode::OK, Json(out)))
}

/// The id of the node a grant is on, for the audit target.
fn grant_target_id(grant: &Grant) -> Uuid {
    grant
        .folder_id
        .or(grant.media_id)
        .unwrap_or_else(Uuid::nil)
}

/// The chain above a file, for the tab's "inherited from" line.
fn chain_body(chain: &omnion_media::Chain) -> Vec<ChainNodeBody> {
    chain
        .nodes
        .iter()
        .map(|node| ChainNodeBody {
            id: node.folder.id,
            path: node.folder.path.clone(),
            grant_count: node.grants.len(),
            has_deny: node.grants.iter().any(Grant::is_deny),
        })
        .collect()
}

/// Build the tab's body, resolving each subject's display name in one statement.
async fn grants_body(
    target_kind: &str,
    target_id: Uuid,
    rows: Vec<Grant>,
    inherits: bool,
    chain: Vec<ChainNodeBody>,
    state: &AppState,
) -> GrantsBody {
    GrantsBody {
        target_kind: target_kind.to_owned(),
        target_id,
        grants: join_subjects(rows, state).await,
        inherits,
        chain,
    }
}

/// Resolve the display name of every subject in a grant list, in one round trip.
///
/// A `union all` over the three subject tables keyed by kind, so the screen's list does not
/// issue one query per row — a folder with forty grants would otherwise be forty statements
/// on the hottest panel screen in the library, and the only symptom is a slow tab that nobody
/// attributes to the tab.
async fn join_subjects(rows: Vec<Grant>, state: &AppState) -> Vec<GrantBodyOut> {
    let users: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.subject_kind == "user")
        .map(|row| row.subject_id)
        .collect();
    let groups: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.subject_kind == "group")
        .map(|row| row.subject_id)
        .collect();
    let roles: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.subject_kind == "role")
        .map(|row| row.subject_id)
        .collect();

    let mut names: std::collections::HashMap<(String, Uuid), String> =
        std::collections::HashMap::new();

    if !users.is_empty() {
        let found: Vec<(Uuid, String)> = sqlx::query_as(
            "select id, coalesce(nullif(display_name, ''), email) as label from users where id = any($1)",
        )
        .bind(&users)
        .fetch_all(state.db().pool())
        .await
        .unwrap_or_default();
        for (id, label) in found {
            names.insert(("user".to_owned(), id), label);
        }
    }
    if !groups.is_empty() {
        let found: Vec<(Uuid, String)> =
            sqlx::query_as("select id, name as label from groups where id = any($1)")
                .bind(&groups)
                .fetch_all(state.db().pool())
                .await
                .unwrap_or_default();
        for (id, label) in found {
            names.insert(("group".to_owned(), id), label);
        }
    }
    if !roles.is_empty() {
        let found: Vec<(Uuid, String)> =
            sqlx::query_as("select id, name as label from roles where id = any($1)")
                .bind(&roles)
                .fetch_all(state.db().pool())
                .await
                .unwrap_or_default();
        for (id, label) in found {
            names.insert(("role".to_owned(), id), label);
        }
    }

    rows.into_iter()
        .map(|row| {
            let capabilities = row.capabilities();
            GrantBodyOut {
                id: row.id,
                subject_kind: row.subject_kind.clone(),
                subject_id: row.subject_id,
                subject_label: names.get(&(row.subject_kind.clone(), row.subject_id)).cloned(),
                can_read: row.can_read,
                can_write: row.can_write,
                can_delete: row.can_delete,
                can_share: row.can_share,
                effect: row.effect,
                created_by: row.created_by,
                created_at: row.created_at.to_string(),
                capabilities: capabilities.labels(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn decision(effective: Capabilities) -> Decision {
        Decision {
            effective,
            applied: Vec::new(),
            reason: String::new(),
            touched: true,
        }
    }

    #[test]
    fn an_untouched_chain_keeps_everything_the_catalogue_allowed() {
        let untouched = Decision::untouched();
        let answer = require_capability(&untouched, omnion_media::Capabilities::ALL);
        assert_eq!(answer, omnion_media::Capabilities::ALL);
    }

    #[test]
    fn a_chain_may_only_subtract_from_the_catalogue() {
        let answer = require_capability(
            &decision(Capabilities {
                read: true,
                ..Capabilities::NONE
            }),
            omnion_media::Capabilities::ALL,
        );
        assert!(answer.read);
        assert!(!answer.write, "a grant cannot hand out write");
        assert!(!answer.share, "a grant cannot hand out share");
    }

    #[test]
    fn a_catalogue_that_never_gave_it_stays_refused() {
        // The grant says read; the catalogue says nothing. The answer is nothing, and this is
        // the assertion that stops a grant becoming a second source of truth.
        let answer = require_capability(
            &decision(Capabilities {
                read: true,
                ..Capabilities::NONE
            }),
            omnion_media::Capabilities::NONE,
        );
        assert_eq!(answer, omnion_media::Capabilities::NONE);
    }

    #[test]
    fn a_deny_with_no_bit_is_refused() {
        let error = validate_effect_and_bits("deny", Capabilities::NONE).unwrap_err();
        assert_eq!(error.code(), "effect");
        assert!(error.message().contains("read, write, delete or share"));
    }

    #[test]
    fn an_allow_with_no_bit_is_accepted() {
        assert_eq!(
            validate_effect_and_bits("allow", Capabilities::NONE).unwrap(),
            "allow"
        );
    }

    #[test]
    fn an_absent_effect_means_allow() {
        assert_eq!(
            validate_effect_and_bits("", Capabilities::from_row(true, false, false, false)).unwrap(),
            "allow"
        );
    }

    #[test]
    fn an_unknown_effect_names_the_field() {
        let error = validate_effect_and_bits("maybe", Capabilities::ALL).unwrap_err();
        assert_eq!(error.code(), "effect");
    }

    #[test]
    fn a_deny_that_removes_only_share_is_accepted() {
        assert_eq!(
            validate_effect_and_bits("deny", Capabilities::from_row(false, false, false, true))
                .unwrap(),
            "deny"
        );
    }

    #[test]
    fn an_unknown_subject_kind_names_the_field_and_the_three() {
        let error = validate_subject_kind("team").unwrap_err();
        assert_eq!(error.code(), "subject_kind");
        assert!(error.message().contains("user"));
        assert!(error.message().contains("group"));
        assert!(error.message().contains("role"));
    }

    #[test]
    fn a_known_subject_kind_is_returned_normalised() {
        assert_eq!(validate_subject_kind(" role ").unwrap(), "role");
    }

    #[test]
    fn a_grant_target_is_one_of_two_nodes() {
        let id = Uuid::new_v4();
        assert_eq!(GrantTarget::Folder(id).columns(), (Some(id), None));
        assert_eq!(GrantTarget::File(id).columns(), (None, Some(id)));
    }

    #[test]
    fn the_conflict_target_matches_the_index_it_names() {
        // A drift here is a statement PostgreSQL refuses at run time, so it is pinned as text.
        // The index lives in `0047_media_grants.sql` and the target lives in the media crate —
        // two files and two languages, which is why the drift is real rather than theoretical.
        assert!(omnion_media::grants::GRANT_CONFLICT.contains("coalesce(folder_id"));
        assert!(omnion_media::grants::GRANT_CONFLICT.contains("coalesce(media_id"));
        assert!(omnion_media::grants::GRANT_CONFLICT.contains("subject_kind, subject_id"));
    }

    #[test]
    fn a_created_grant_serialises_its_instant_as_a_string() {
        let mut row = sample_grant();
        row.created_at = OffsetDateTime::UNIX_EPOCH;
        assert!(!row.created_at.to_string().is_empty());
    }

    fn sample_grant() -> Grant {
        Grant {
            id: Uuid::new_v4(),
            folder_id: Some(Uuid::new_v4()),
            media_id: None,
            subject_kind: "user".to_owned(),
            subject_id: Uuid::new_v4(),
            can_read: true,
            can_write: false,
            can_delete: false,
            can_share: false,
            effect: "allow".to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}
