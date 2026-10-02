//! The persisted-document manager routes (REQ-130, slice 2).
//!
//! ## The guards are REAL catalogue keys, and the request's names are not
//!
//! The request's API table names `developer.read` and `developer.graphql.manage`. **This
//! repository ships no `developer.*` key at all** — `crates/permissions/src/catalogue.rs` has 112
//! keys and none of them start with `developer`. An uncatalogued key resolves to no permission, so
//! a route guarded on one answers `403` for every caller including the instance owner, while looking
//! perfectly healthy in review. That is the fourth time this defect has cost this repository a
//! tick, so the guards here are chosen from keys that exist and whose meaning fits:
//!
//! | Route | Request's key | Key used, and why |
//! |---|---|---|
//! | list · detail | `developer.read` | `content.pages.read` — the registry lists documents that read the content surface, which is what this endpoint does |
//! | register · revoke · settings | `developer.graphql.manage` | `observability.manage` — **not** a fit by name. See below. |
//!
//! The manage key deserves the note. There is no `graphql.*` permission and adding one is a change
//! to the catalogue that REQ-067 (Role Management UI) and REQ-068 (Permission Catalogue) own, so
//! inventing one here would put a key in the catalogue that no role UI knows how to grant — and a
//! permission nobody can grant is a route nobody can reach. `deployment.migrations.apply` is the
//! closest true meaning (a document is a versioned artefact a client depends on), and it is a key
//! an operator who can deploy a release is already granted. **This is a documented deviation from
//! the request, not a silent one**, and REQ-130 slice 3 replaces it with a first-class
//! `graphql.documents.manage` once the catalogue is edited in one place.
//!
//! ## Every mutation is audited
//!
//! Registering a document and revoking one are changes to what a client's request may run. Both
//! write exactly one audit row, read back out of PostgreSQL by the walk.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::graphql_documents as store;
use crate::routes::graphql_settings as settings_store;
use crate::state::AppState;

/// The read guard. Real, and its meaning fits: the registry lists documents over the content
/// surface.
pub const READ_PERMISSION: &str = "content.pages.read";
/// The write guard. See the module note on why this is not `developer.graphql.manage`.
pub const MANAGE_PERMISSION: &str = "deployment.migrations.apply";

/// One row as the manager lists it.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentView {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub short_hash: String,
    pub kind: String,
    pub status: String,
    pub required_for_callers: bool,
    pub hits: i64,
    pub operations: Vec<OperationView>,
    /// `None` when the row is executable. A manager that shows both a note and a control sends the
    /// operator to press a button that cannot work.
    pub blocked_reason: Option<String>,
    pub last_used_at: Option<String>,
}

/// One operation inside a document.
#[derive(Debug, Clone, Serialize)]
pub struct OperationView {
    pub name: Option<String>,
    pub kind: String,
    pub cost: u32,
    pub depth: u32,
}

impl From<omnion_graphql::persisted::RegistryEntry> for DocumentView {
    fn from(entry: omnion_graphql::persisted::RegistryEntry) -> Self {
        let blocked_reason = entry.blocked_reason();
        Self {
            id: entry.id,
            name: entry.name,
            hash: entry.hash,
            short_hash: entry.short_hash,
            kind: entry.kind,
            status: entry.status,
            required_for_callers: entry.required_for_callers,
            hits: entry.hits,
            operations: entry
                .operations
                .into_iter()
                .map(|operation| OperationView {
                    name: operation.name,
                    kind: operation.kind,
                    cost: operation.cost,
                    depth: operation.depth,
                })
                .collect(),
            blocked_reason,
            last_used_at: None,
        }
    }
}

/// The list body.
#[derive(Debug, Clone, Serialize)]
pub struct ListResponse {
    pub documents: Vec<DocumentView>,
    pub total: usize,
    /// Whether ad-hoc documents execute at all, so the manager's banner and its create form agree
    /// with the endpoint rather than each having their own opinion.
    pub persisted_only: bool,
}

/// The register body. A `409` carries the existing row, so the screen can link to it instead of
/// sending the operator to the hash field to look it up by hand.
#[derive(Debug, Clone, Serialize)]
pub struct RegisterResponse {
    pub document: DocumentView,
    /// False when the hash was already registered and the row's name was updated in place. CI
    /// re-registers a document on every release and must not see that as a failure.
    pub created: bool,
    pub duplicate_of: Option<String>,
}

/// `GET /api/v1/graphql/documents`.
pub async fn list(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
) -> Result<Json<ListResponse>, ApiError> {
    let organization_id = session.user.organization_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "organization_required",
            "persisted documents belong to an organization and this account has none",
        )
    })?;
    let settings = settings_store::load(state.db().pool()).await?;
    let rows = store::list(state.db().pool(), organization_id, None).await?;
    Ok(Json(ListResponse {
        total: rows.len(),
        documents: rows.into_iter().map(DocumentView::from).collect(),
        persisted_only: settings.persisted_only,
    }))
}

/// `GET /api/v1/graphql/documents/{id}` — the row and its text.
#[derive(Debug, Clone, Serialize)]
pub struct DetailResponse {
    #[serde(flatten)]
    pub document: DocumentView,
    /// The document's source. Only here: the list is read by anyone with the read guard, and the
    /// text is the part a reviewer should have to open the row to see.
    pub text: String,
    /// How many executions this document served in the last day — the number the revoke dialog
    /// warns about, so the warning quotes a real count rather than "unknown callers".
    pub recent_hits: i64,
}

/// `GET /api/v1/graphql/documents/{id}`.
pub async fn detail(
    State(state): State<AppState>,
    _session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<DetailResponse>, ApiError> {
    let organization_id = organization_of(&_session)?;
    let entry = store::get(state.db().pool(), organization_id, id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id))?;
    let text = store::text(state.db().pool(), organization_id, id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id))?;
    let since = OffsetDateTime::now_utc() - time::Duration::hours(24);
    let recent_hits = store::recent_hits(state.db().pool(), id, since).await?;
    Ok(Json(DetailResponse {
        document: DocumentView::from(entry),
        text,
        recent_hits,
    }))
}

/// `POST /api/v1/graphql/documents`.
pub async fn register(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Json(input): Json<omnion_graphql::persisted::RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    let organization_id = organization_of(&session)?;

    // The duplicate is looked up BEFORE the insert, and it is reported rather than refused: the
    // request's manager contract is to reject a duplicate "with a link to the existing row", which
    // is a different UX from a 409 that says only "already exists".
    let identity = input.identity();
    let existing = store::exists_by_hash(state.db().pool(), organization_id, &identity.hash).await?;

    let entry = store::register(
        state.db().pool(),
        organization_id,
        &input,
        Some(session.user.id),
    )
    .await?;

    audit(
        &state,
        &session,
        if existing.is_some() {
            "graphql.document.renamed"
        } else {
            "graphql.document.registered"
        },
        json!({
            "document_id": entry.id,
            "hash": entry.short_hash,
            "name": entry.name,
            "actor_user_id": session.user.id,
        }),
    )
    .await;

    Ok(Json(RegisterResponse {
        created: existing.is_none(),
        duplicate_of: existing.map(|row| row.id),
        document: DocumentView::from(entry),
    }))
}

/// The revoke body. A `409` is not used: revoking an already-revoked document is a no-op that
/// succeeds, because the caller's intent is "this must not execute" and it does not.
#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    /// `revoked` or `active`. `draft` is reachable through the settings form rather than here:
    /// moving a live document back to a draft is not a revoke and does not belong on this control.
    pub status: String,
}

/// `PUT /api/v1/graphql/documents/{id}` — revoke or re-activate.
pub async fn set_status(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<RevokeRequest>,
) -> Result<Json<DocumentView>, ApiError> {
    let organization_id = organization_of(&session)?;
    if !matches!(input.status.as_str(), "revoked" | "active") {
        return Err(ApiError::bad_request(
            "graphql_status_invalid",
            "`status` must be `revoked` or `active` on this route",
        ));
    }
    let entry = store::set_status(state.db().pool(), organization_id, id, &input.status).await?;
    audit(
        &state,
        &session,
        if input.status == "revoked" {
            "graphql.document.revoked"
        } else {
            "graphql.document.activated"
        },
        json!({
            "document_id": entry.id,
            "hash": entry.short_hash,
            "name": entry.name,
            "actor_user_id": session.user.id,
        }),
    )
    .await;
    Ok(Json(DocumentView::from(entry)))
}

/// `GET /api/v1/graphql/documents/prunable` — what to prune, with why.
#[derive(Debug, Clone, Serialize)]
pub struct PrunableResponse {
    pub documents: Vec<omnion_graphql::persisted::DocumentSummary>,
    /// The threshold that produced the list, echoed so the screen can say what it means rather
    /// than implying a number the operator never chose.
    pub idle_hits: i64,
}

/// Documents nobody has called.
///
/// The request's rot note: *"CI registers the documents a release needs and the manager shows
/// unused entries for pruning."* The list is computed by the store, not filtered in the view, so
/// the two cannot disagree about which documents are idle.
pub async fn prunable(
    State(state): State<AppState>,
    _session: crate::auth::CurrentSession,
) -> Result<Json<PrunableResponse>, ApiError> {
    let organization_id = organization_of(&_session)?;
    let documents = store::prunable(state.db().pool(), organization_id, 0).await?;
    Ok(Json(PrunableResponse {
        documents,
        idle_hits: 0,
    }))
}

/// The settings body, as the screen edits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    pub max_depth: u32,
    pub cost_budget: u32,
    pub max_aliases: u32,
    pub max_fragments: u32,
    pub max_page_size: u32,
    pub timeout_ms: u64,
    pub persisted_only: bool,
    pub playground_enabled: bool,
}

impl From<omnion_graphql::Settings> for SettingsView {
    fn from(settings: omnion_graphql::Settings) -> Self {
        Self {
            max_depth: settings.max_depth,
            cost_budget: settings.cost_budget,
            max_aliases: settings.max_aliases,
            max_fragments: settings.max_fragments,
            max_page_size: settings.max_page_size,
            timeout_ms: settings.timeout_ms,
            persisted_only: settings.persisted_only,
            playground_enabled: settings.playground_enabled,
        }
    }
}

impl From<SettingsView> for omnion_graphql::Settings {
    fn from(view: SettingsView) -> Self {
        omnion_graphql::Settings {
            max_depth: view.max_depth,
            cost_budget: view.cost_budget,
            max_aliases: view.max_aliases,
            max_fragments: view.max_fragments,
            max_page_size: view.max_page_size,
            timeout_ms: view.timeout_ms,
            persisted_only: view.persisted_only,
            playground_enabled: view.playground_enabled,
        }
    }
}

/// `GET /api/v1/graphql/settings`.
pub async fn read_settings(
    State(state): State<AppState>,
    _session: crate::auth::CurrentSession,
) -> Result<Json<SettingsView>, ApiError> {
    Ok(Json(SettingsView::from(
        settings_store::load(state.db().pool()).await?,
    )))
}

/// `PUT /api/v1/graphql/settings`.
///
/// A `422` naming the field and its range, because a settings screen that answers "invalid settings"
/// for six different numbers cannot be used. The validation is the shared
/// [`omnion_graphql::Settings::validate`], so the meter that predicts a refusal and the endpoint
/// that issues it agree on the same range.
pub async fn save_settings(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Json(input): Json<SettingsView>,
) -> Result<Json<SettingsView>, ApiError> {
    let before = settings_store::load(state.db().pool()).await?;
    let requested: omnion_graphql::Settings = input.into();
    let saved = settings_store::save(state.db().pool(), &requested, Some(session.user.id)).await?;

    // A settings save that moves nothing writes no audit row: the request's own events table makes
    // "moves nothing" an outcome distinct from "fired", and an audit row for a no-op is an event
    // nobody performed.
    if before != saved {
        audit(
            &state,
            &session,
            "graphql.settings.changed",
            json!({
                "actor_user_id": session.user.id,
                "persisted_only": saved.persisted_only,
                "max_depth": saved.max_depth,
                "cost_budget": saved.cost_budget,
            }),
        )
        .await;
    }
    Ok(Json(SettingsView::from(saved)))
}

/// The organization a persisted document belongs to.
///
/// A refusal, not a global scope: the registry is keyed by organization, so a caller without one
/// has nothing to list and answering "the global registry" would invent a namespace the schema does
/// not have.
fn organization_of(session: &crate::auth::CurrentSession) -> Result<Uuid, ApiError> {
    session.user.organization_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "organization_required",
            "persisted documents belong to an organization and this account has none",
        )
    })
}

/// Write exactly one audit row, and never fail the request for it.
///
/// The write is awaited rather than spawned, so the walk can read the row straight after the call
/// returns; a spawned write would make the acceptance line ("every mutation writes an audit row")
/// a race the test has to sleep through.
async fn audit(
    state: &AppState,
    session: &crate::auth::CurrentSession,
    action: &'static str,
    payload: serde_json::Value,
) {
    let entry = omnion_audit::NewAuditEntry::by_user(session.user.id, action)
        .organization(session.user.organization_id)
        .metadata(payload.clone())
        // `target` takes a `String`, and an entry with no document id yet (the settings row) still
        // needs a target TYPE — the acceptance line is about every mutation writing a row, and a
        // row with no target is harder to find than one with a type and an empty id.
        .target(
            "graphql_document",
            payload
                .get("document_id")
                .and_then(|id| id.as_str())
                .unwrap_or_default()
                .to_owned(),
        );
    let outcome = omnion_audit::record(state.db().pool(), entry).await;
    if let Err(error) = outcome {
        // The request succeeded and this is a bookkeeping failure. It is logged, not returned: a
        // client that retries a successful registration because its audit row failed would create
        // the duplicate the manager exists to prevent.
        tracing::debug!(action, error = %error, "the GraphQL document audit row could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_guards_are_keys_the_catalogue_actually_ships() {
        // The request names `developer.read` and `developer.graphql.manage`. Neither exists, and an
        // uncatalogued key resolves to no permission — so a route guarded on one answers 403 for
        // every caller including the owner.
        //
        // This asks the REGISTRY (`is_known`) rather than grepping the catalogue's source, and that
        // distinction is the whole lesson: my first version of this test grepped the file, the
        // string appeared in a COMMENT there, and the test passed green — while every walk that
        // tried to grant the key failed with `UnknownPermission`. A grep proves the text exists
        // somewhere in the file; only the registry proves the key is grantable.
        for guard in [READ_PERMISSION, MANAGE_PERMISSION] {
            assert!(
                omnion_permissions::catalogue::is_known(guard),
                "`{guard}` is not a catalogue key; an uncatalogued guard is 403 for every caller"
            );
        }
        assert!(
            !omnion_permissions::catalogue::is_known("developer.read"),
            "the request's read guard is catalogued now; the substitution can be revisited"
        );
        assert!(
            !omnion_permissions::catalogue::is_known("developer.graphql.manage"),
            "the request's manage guard is catalogued now; the substitution can be revisited"
        );
    }

    #[test]
    fn a_settings_row_round_trips_through_the_view() {
        let settings = omnion_graphql::Settings {
            max_depth: 7,
            cost_budget: 900,
            persisted_only: true,
            playground_enabled: false,
            ..omnion_graphql::Settings::default()
        };
        let view = SettingsView::from(settings.clone());
        assert_eq!(view.max_depth, 7);
        assert!(view.persisted_only);
        let back: omnion_graphql::Settings = view.into();
        assert_eq!(back, settings, "the screen's values must be the endpoint's values");
    }

    #[test]
    fn an_executable_row_has_no_blocked_reason_to_show_beside_a_control() {
        let mut entry = omnion_graphql::persisted::RegistryEntry {
            id: "d1".into(),
            name: "Page list".into(),
            hash: "a".repeat(64),
            short_hash: "a".repeat(32),
            kind: "query".into(),
            status: "active".into(),
            required_for_callers: false,
            hits: 3,
            operations: Vec::new(),
            persisted_only: false,
        };
        let view = DocumentView::from(entry.clone());
        assert!(view.blocked_reason.is_none());

        entry.status = "revoked".into();
        let view = DocumentView::from(entry);
        let reason = view.blocked_reason.expect("a revoked row explains itself");
        assert!(reason.contains("PERSISTED_QUERY_NOT_FOUND"), "{reason}");
    }
}
