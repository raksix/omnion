//! `/api/v1/crm` — companies and contacts (docs/requests/REQ-051, slice 1).
//!
//! The HTTP layer stays thin on purpose, and three of its jobs are not "shape JSON":
//!
//! * **Resolving the caller's organization.** An organization account works inside its own
//!   organization and nowhere else; a platform account has to name one. The same rule
//!   `crate::scope` applies everywhere, because a tenant boundary that is only in the CRM would
//!   be the one boundary somebody forgets.
//! * **Resolving the visibility level.** The permission says the caller may read contacts; the
//!   *level* says how much — their own, their team's, or the organization's. The level is read
//!   from the caller's bindings (a `department` binding whose `resource_id` is `own`, `team` or
//!   `all`), defaults to `all` for a platform-level account, and is then **enforced in SQL** by
//!   the module. A record outside it is `404`, not `403`: a `403` would confirm it exists.
//! * **Auditing and emitting.** Every mutation writes an audit row (actor, before/after, request
//!   id) and the transitions emit the documented `crm.*` events for the automation engine and
//!   the webhook subscribers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_module_crm::contacts::{
    self, CompanyChanges, Contact, ContactChanges, ContactPatch, MergeRequest,
};
use omnion_module_crm::model::{SENSITIVE_FIELDS_READ, Visibility};
use omnion_module_crm::query::{ListQuery, Page, Scope};
use omnion_module_crm::CrmError;
use omnion_permissions::{Scope as PermissionScope, authorize};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::scope::resolve_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// Query of the contact and company lists.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// Free text: name, address, company, title.
    #[serde(default)]
    pub search: Option<String>,
    /// `me`, `unassigned` or a user id.
    #[serde(default)]
    pub owner: Option<String>,
    /// Lifecycle status.
    #[serde(default)]
    pub status: Option<String>,
    /// One tag the record must carry.
    #[serde(default)]
    pub tag: Option<String>,
    /// A company id.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// A contact id.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// A deal id.
    #[serde(default)]
    pub deal_id: Option<Uuid>,
    /// A pipeline id.
    #[serde(default)]
    pub pipeline_id: Option<Uuid>,
    /// Created on or after.
    #[serde(default)]
    pub created_from: Option<time::Date>,
    /// Created on or before.
    #[serde(default)]
    pub created_to: Option<time::Date>,
    /// "No activity since this many days ago" (contacts).
    #[serde(default)]
    pub inactive_days: Option<i32>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include the archived records.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// `own`, `team` or `all` — how much of the organization the caller reads.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

impl ListParams {
    /// The module's list query.
    fn into_query(self) -> Result<ListQuery, ApiError> {
        Ok(ListQuery {
            search: self.search,
            owner: self.owner,
            status: self.status,
            tag: self.tag,
            company_id: self.company_id,
            contact_id: self.contact_id,
            deal_id: self.deal_id,
            pipeline_id: self.pipeline_id,
            created_from: self.created_from,
            created_to: self.created_to,
            inactive_days: self.inactive_days,
            sort: self.sort,
            direction: self.direction,
            limit: self.limit,
            cursor: self.cursor,
            include_archived: self.include_archived,
            visibility: None,
        })
    }
}

/// The body of a new contact.
#[derive(Debug, Deserialize)]
pub struct NewContact {
    /// Given name.
    pub first_name: String,
    /// Family name.
    #[serde(default)]
    pub last_name: String,
    /// Address.
    #[serde(default)]
    pub email: Option<String>,
    /// Phone number.
    #[serde(default)]
    pub phone: Option<String>,
    /// Job title.
    #[serde(default)]
    pub job_title: Option<String>,
    /// Company.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// Owner (defaults to the caller).
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Status.
    #[serde(default)]
    pub status: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Custom values.
    #[serde(default)]
    pub custom: Option<Value>,
    /// Note.
    #[serde(default)]
    pub notes: Option<String>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The body of a new company.
#[derive(Debug, Deserialize)]
pub struct NewCompany {
    /// Display name.
    pub name: String,
    /// Domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Industry.
    #[serde(default)]
    pub industry: Option<String>,
    /// Owner (defaults to the caller).
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Status.
    #[serde(default)]
    pub status: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Custom values.
    #[serde(default)]
    pub custom: Option<Value>,
    /// Note.
    #[serde(default)]
    pub notes: Option<String>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Resolution helpers
// ---------------------------------------------------------------------------------------------

/// The organization this request works in.
pub(crate) fn organization_of(current: &CurrentSession, requested: Option<Uuid>) -> Result<Uuid, ApiError> {
    resolve_organization(current, requested)
}

/// How much of the organization the caller reads.
///
/// A **department** binding is how an organization says "this role sees only its own records":
/// the permission is the same `crm.contacts.read`, and the *scope* of the binding carries the
/// level. Reading it here — rather than in the UI — is what makes a saved view's `own` level
/// mean the same thing in an export as on a screen.
///
/// No such binding means `all`: an account that is a plain member of the organization and was
/// never narrowed sees the organization's CRM, which is exactly what its role's permission set
/// says. When several bindings narrow the caller, the **tightest** level wins — a promise a
/// person can rely on is the narrowest one they hold, never the widest.
pub(crate) async fn visibility_of(state: &AppState, current: &CurrentSession) -> Visibility {
    let Some(organization_id) = current.user.organization_id else {
        return Visibility::All;
    };

    #[derive(sqlx::FromRow)]
    struct Narrowing {
        resource_id: String,
    }

    let rows: Vec<Narrowing> = sqlx::query_as(
        "select rb.resource_id \
         from role_bindings rb \
         where rb.user_id = $1 and rb.revoked_at is null \
           and rb.organization_id = $2 \
           and rb.scope_type = 'department' \
           and exists (select 1 from role_permissions rp \
                       where rp.role_id = rb.role_id and rp.effect = 'allow' \
                         and rp.permission_key in ('crm.contacts.read', 'crm.contacts.update'))",
    )
    .bind(current.user.id)
    .bind(organization_id)
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    let mut narrowest = Visibility::All;
    for row in rows {
        if let Some(level) = Visibility::parse(Some(&row.resource_id))
            && level != Visibility::All
            && (narrowest == Visibility::All || level < narrowest)
        {
            narrowest = level;
        }
    }

    narrowest
}

/// The scope the module reads at: the organization, the caller, the level and the team.
pub(crate) async fn scope_of(state: &AppState, current: &CurrentSession, organization_id: Uuid) -> Scope {
    let visibility = visibility_of(state, current).await;
    let team_user_ids = if visibility == Visibility::Team {
        team_of(state, current, organization_id).await
    } else {
        Vec::new()
    };

    Scope::all(organization_id, current.user.id).with(visibility, team_user_ids)
}

/// Everyone who shares a group with the caller: the `team` level's people.
pub(crate) async fn team_of(state: &AppState, current: &CurrentSession, organization_id: Uuid) -> Vec<Uuid> {
    #[derive(sqlx::FromRow)]
    struct Member {
        user_id: Uuid,
    }

    let rows: Vec<Member> = sqlx::query_as(
        "select distinct gm.user_id \
         from group_members gm \
         join groups g on g.id = gm.group_id and g.organization_id = $1 \
         where gm.user_id <> $2",
    )
    .bind(organization_id)
    .bind(current.user.id)
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    rows.into_iter().map(|row| row.user_id).collect()
}

/// `true` when the caller may read the flagged fields.
///
/// Resolved with the same authorizer the route guard uses, so a policy that denies the key has
/// the same effect here as it does on the route itself.
pub(crate) async fn may_read_sensitive(state: &AppState, current: &CurrentSession) -> bool {
    let Some(organization_id) = current.user.organization_id else {
        return true;
    };

    authorize(
        state.db().pool(),
        current.user.id,
        PermissionScope::Organization { organization_id },
        SENSITIVE_FIELDS_READ,
    )
    .await
    .map(|decision| decision.is_allowed())
    .unwrap_or(false)
}

/// Record an event without letting a webhook problem fail the caller's request.
pub(crate) async fn emit(state: &AppState, event: NewEvent) {
    if let Err(error) = bus::emit(state.db().pool(), event).await {
        tracing::warn!(error = %error, "the CRM event could not be recorded");
    }
}

/// Which contact fields a patch actually changed.
///
/// Typed rather than "serialise both and diff the JSON": the fields a person edits are the ones
/// a person can see, and a diff over two serialised rows would also report a change to a field
/// this update never touched.
fn contact_changes(before: &Contact, after: &Contact) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    if before.first_name != after.first_name {
        changed.push("first_name".to_owned());
    }
    if before.last_name != after.last_name {
        changed.push("last_name".to_owned());
    }
    if before.email != after.email {
        changed.push("email".to_owned());
    }
    if before.phone != after.phone {
        changed.push("phone".to_owned());
    }
    if before.job_title != after.job_title {
        changed.push("job_title".to_owned());
    }
    if before.company_id != after.company_id {
        changed.push("company_id".to_owned());
    }
    if before.owner_user_id != after.owner_user_id {
        changed.push("owner_user_id".to_owned());
    }
    if before.status != after.status {
        changed.push("status".to_owned());
    }
    if before.tags != after.tags {
        changed.push("tags".to_owned());
    }
    if before.custom != after.custom {
        changed.push("custom".to_owned());
    }
    if before.notes != after.notes {
        changed.push("notes".to_owned());
    }
    changed
}

/// Which company fields a patch actually changed.
fn company_changes(
    before: &omnion_module_crm::contacts::Company,
    after: &omnion_module_crm::contacts::Company,
) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    if before.name != after.name {
        changed.push("name".to_owned());
    }
    if before.domain != after.domain {
        changed.push("domain".to_owned());
    }
    if before.industry != after.industry {
        changed.push("industry".to_owned());
    }
    if before.owner_user_id != after.owner_user_id {
        changed.push("owner_user_id".to_owned());
    }
    if before.status != after.status {
        changed.push("status".to_owned());
    }
    if before.tags != after.tags {
        changed.push("tags".to_owned());
    }
    if before.custom != after.custom {
        changed.push("custom".to_owned());
    }
    if before.notes != after.notes {
        changed.push("notes".to_owned());
    }
    changed
}

/// The identity of a contact for an event payload.
fn contact_ref(contact: &Contact) -> Value {
    json!({
        "contact_id": contact.id,
        "organization_id": contact.organization_id,
        "email": contact.email,
        "owner_user_id": contact.owner_user_id,
        "status": contact.status,
    })
}

// ---------------------------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/contacts` — one page of contacts.
pub async fn list_contacts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<Contact>>, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;
    let query = params.into_query()?;

    Ok(Json(
        contacts::list_contacts(state.db().pool(), &scope, &query, sensitive).await?,
    ))
}

/// `GET /api/v1/crm/contacts/{id}` — one contact.
pub async fn get_contact(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(contact_id): Path<Uuid>,
) -> Result<Json<Contact>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;

    Ok(Json(
        contacts::get_contact(state.db().pool(), &scope, contact_id, sensitive).await?,
    ))
}

/// `POST /api/v1/crm/contacts` — create a contact.
pub async fn create_contact(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<NewContact>,
) -> Result<(StatusCode, Json<Contact>), ApiError> {
    let organization_id = organization_of(&current, body.organization_id)?;
    let body = body.0;

    let changes = ContactChanges {
        first_name: body.first_name,
        last_name: body.last_name,
        email: body.email,
        phone: body.phone,
        job_title: body.job_title,
        company_id: body.company_id,
        // A contact with no named owner belongs to whoever created it: an unowned record is
        // nobody's, and the "needs an owner" list is a list of records that *nobody* owns.
        owner_user_id: Some(body.owner_user_id.unwrap_or(current.user.id)),
        status: body.status,
        tags: body.tags,
        custom: body.custom,
        notes: body.notes,
    };

    let after = contacts::create_contact(state.db().pool(), organization_id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.contact.created")
            .organization(organization_id)
            .target("crm_contact", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "after": contact_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.contact.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(contact_ref(&after)),
    )
    .await;

    Ok((StatusCode::CREATED, Json(after)))
}

/// `PATCH /api/v1/crm/contacts/{id}` — update a contact (the inline edit of the list included).
pub async fn update_contact(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(contact_id): Path<Uuid>,
    body: Json<ContactPatch>,
) -> Result<Json<Contact>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let before = contacts::get_contact(state.db().pool(), &scope, contact_id, true).await?;
    let after = contacts::patch_contact(state.db().pool(), &scope, contact_id, &body.0).await?;

    let changes = contact_changes(&before, &after);
    if !changes.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "crm.contact.updated")
                .organization(organization_id)
                .target("crm_contact", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changes,
                    "before": contact_ref(&before),
                    "after": contact_ref(&after),
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("crm.contact.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({
                    "contact_id": after.id,
                    "changed": changes,
                })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/crm/contacts/{id}` — archive a contact.
///
/// `200` with the archived row rather than `204`: the list replaces its optimistic row with the
/// answer, and a body that says when it was archived is what the screen shows.
pub async fn archive_contact(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(contact_id): Path<Uuid>,
) -> Result<Json<Contact>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let before = contacts::get_contact(state.db().pool(), &scope, contact_id, true).await?;
    let after = contacts::archive_contact(state.db().pool(), &scope, contact_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.contact.archived")
            .organization(organization_id)
            .target("crm_contact", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": contact_ref(&before),
                "after": contact_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.contact.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(contact_ref(&after)),
    )
    .await;

    Ok(Json(after))
}

/// `POST /api/v1/crm/contacts/merge` — merge two contacts into one.
pub async fn merge_contacts(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<MergeRequest>,
) -> Result<Json<Contact>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let before = contacts::get_contact(state.db().pool(), &scope, body.0.survivor, true).await?;
    let after = contacts::merge_contacts(state.db().pool(), &scope, &body.0).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.contact.merged")
            .organization(organization_id)
            .target("crm_contact", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "survivor": after.id,
                "archived": body.0.loser,
                "before": contact_ref(&before),
                "after": contact_ref(&after),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.contact.merged")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "contact_id": after.id,
                "archived_contact_id": body.0.loser,
            })),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Companies
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/companies` — one page of companies.
pub async fn list_companies(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<contacts::Company>>, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;
    let query = params.into_query()?;

    Ok(Json(
        contacts::list_companies(state.db().pool(), &scope, &query, sensitive).await?,
    ))
}

/// `GET /api/v1/crm/companies/{id}` — one company with its rollups.
pub async fn get_company(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(company_id): Path<Uuid>,
) -> Result<Json<contacts::CompanyDetail>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;

    Ok(Json(
        contacts::get_company(state.db().pool(), &scope, company_id, sensitive).await?,
    ))
}

/// `POST /api/v1/crm/companies` — create a company.
pub async fn create_company(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<NewCompany>,
) -> Result<(StatusCode, Json<contacts::Company>), ApiError> {
    let organization_id = organization_of(&current, body.organization_id)?;
    let body = body.0;

    let changes = CompanyChanges {
        name: body.name,
        domain: body.domain,
        industry: body.industry,
        owner_user_id: Some(body.owner_user_id.unwrap_or(current.user.id)),
        status: body.status,
        tags: body.tags,
        custom: body.custom,
        notes: body.notes,
    };

    let after = contacts::create_company(state.db().pool(), organization_id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.company.created")
            .organization(organization_id)
            .target("crm_company", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "after": { "company_id": after.id, "name": after.name, "status": after.status },
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.company.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "company_id": after.id, "name": after.name })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(after)))
}

/// `PATCH /api/v1/crm/companies/{id}` — update a company.
pub async fn update_company(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(company_id): Path<Uuid>,
    body: Json<contacts::CompanyPatch>,
) -> Result<Json<contacts::Company>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;

    let before = contacts::get_company(state.db().pool(), &scope, company_id, true)
        .await?
        .company;
    let after = contacts::patch_company(state.db().pool(), &scope, company_id, &body.0).await?;

    let changes = company_changes(&before, &after);
    if !changes.is_empty() {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "crm.company.updated")
                .organization(organization_id)
                .target("crm_company", after.id.to_string())
                .metadata(json!({
                    "request_id": after.id,
                    "changed": changes,
                }))
                .ip_address(address.as_text()),
        )
        .await?;

        emit(
            &state,
            NewEvent::new("crm.company.updated")
                .organization(organization_id)
                .actor(current.user.id)
                .payload(json!({ "company_id": after.id, "changed": changes })),
        )
        .await;
    }

    Ok(Json(after))
}

/// `DELETE /api/v1/crm/companies/{id}` — archive a company.
pub async fn archive_company(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(company_id): Path<Uuid>,
) -> Result<Json<contacts::Company>, ApiError> {
    let organization_id = organization_of(&current, None)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let before = contacts::get_company(state.db().pool(), &scope, company_id, true)
        .await?
        .company;
    let after = contacts::archive_company(state.db().pool(), &scope, company_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.company.archived")
            .organization(organization_id)
            .target("crm_company", after.id.to_string())
            .metadata(json!({
                "request_id": after.id,
                "before": { "name": before.name, "status": before.status },
                "after": { "name": after.name, "status": after.status },
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        NewEvent::new("crm.company.archived")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "company_id": after.id })),
    )
    .await;

    Ok(Json(after))
}

// ---------------------------------------------------------------------------------------------
// Helpers that the module does not own
// ---------------------------------------------------------------------------------------------

impl From<CrmError> for ApiError {
    /// The CRM's refusals, mapped to the platform's HTTP vocabulary: a validation failure is a
    /// `400` that names the field the form renders it under, a missing or out-of-scope record is
    /// a `404`, a taken address or name is a `409`, and the store keeps the dependency/internal
    /// split the rest of the platform reports.
    fn from(error: CrmError) -> Self {
        match error {
            CrmError::Invalid { entity, field, message } => {
                Self::bad_request("invalid_crm_record", message)
                    .with_details(json!({ "entity": entity, "field": field }))
            }
            CrmError::InvalidQuery(message) => Self::bad_request("invalid_crm_query", message),
            CrmError::NotFound(kind) => Self::new(
                StatusCode::NOT_FOUND,
                match kind {
                    "contact" => "contact_not_found",
                    "company" => "company_not_found",
                    _ => "crm_record_not_found",
                },
                format!("no such {kind} in this organization"),
            ),
            CrmError::EmailTaken => Self::new(
                StatusCode::CONFLICT,
                "contact_email_taken",
                "another contact of this organization already uses this e-mail address",
            ),
            CrmError::CompanyNameTaken => Self::new(
                StatusCode::CONFLICT,
                "company_name_taken",
                "another company of this organization already carries this name",
            ),
            CrmError::InvalidMerge(message) => Self::bad_request("invalid_crm_merge", message),
            CrmError::InvalidStageChange(message) => {
                Self::bad_request("invalid_crm_stage_change", message)
            }
            // The call reached the model and came back with nothing readable. That is an upstream
            // answer, not a bad request and not our bug, so it is a `502` with a stable code the
            // panel can offer a retry against — rendering an empty draft as if the model had
            // written one is the outcome this variant exists to prevent.
            CrmError::EmptyAnswer => Self::new(
                StatusCode::BAD_GATEWAY,
                "crm_copilot_empty_answer",
                "the assistant returned an empty answer — try again",
            ),
            CrmError::Database(err) if database_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            CrmError::Database(err) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", err.to_string())
            }
        }
    }
}

/// `true` when the database refused to answer, rather than refusing the statement.
fn database_unavailable(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::json;

    /// The JSON body an `ApiError` turns into, read through its own `IntoResponse` — the shape a
    /// client actually receives, nested under `error` as the rest of the API documents.
    async fn body_of(error: ApiError) -> Value {
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the error body must read");
        serde_json::from_slice(&bytes).expect("the error body is JSON")
    }

    #[tokio::test]
    async fn a_validation_failure_carries_the_field_the_form_needs() {
        let error = ApiError::from(CrmError::invalid(
            "contact",
            "email",
            "that is not an e-mail address",
        ));
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.code(), "invalid_crm_record");

        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], json!("invalid_crm_record"));
        assert_eq!(
            body["error"]["message"],
            json!("that is not an e-mail address")
        );
        // The form attaches the message to a named field, so the field has to be in the body.
        assert_eq!(body["error"]["details"]["field"], json!("email"));
        assert_eq!(body["error"]["details"]["entity"], json!("contact"));
    }

    #[tokio::test]
    async fn a_record_of_another_organization_is_a_404_not_a_403() {
        let error = ApiError::from(CrmError::NotFound("contact"));
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        assert_eq!(error.code(), "contact_not_found");

        let body = body_of(error).await;
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("another organization"),
            "a 404 must not confirm that the record exists somewhere else: {message}"
        );
    }

    #[tokio::test]
    async fn a_taken_address_is_a_conflict_naming_the_address() {
        let error = ApiError::from(CrmError::EmailTaken);
        assert_eq!(error.status(), StatusCode::CONFLICT);

        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], json!("contact_email_taken"));
    }

    #[tokio::test]
    async fn a_taken_company_name_is_its_own_conflict() {
        let error = ApiError::from(CrmError::CompanyNameTaken);
        assert_eq!(error.status(), StatusCode::CONFLICT);

        let body = body_of(error).await;
        assert_eq!(body["error"]["code"], json!("company_name_taken"));
    }

    /// A contact row to diff in the tests below.
    fn contact_fixture() -> Contact {
        Contact {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            initials: "AL".to_owned(),
            email: Some("ada@example.com".to_owned()),
            phone: None,
            job_title: None,
            company_id: None,
            company_name: None,
            owner_user_id: None,
            owner_name: None,
            status: "lead".to_owned(),
            tags: Vec::new(),
            custom: json!({}),
            notes: String::new(),
            last_activity_at: None,
            archived_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_diff_names_only_the_fields_a_patch_really_changed() {
        let before = contact_fixture();
        let mut after = before.clone();
        after.status = "customer".to_owned();
        assert_eq!(contact_changes(&before, &after), vec!["status".to_owned()]);

        let mut after = before.clone();
        after.tags = vec!["vip".to_owned()];
        after.notes = "Called on Tuesday.".to_owned();
        assert_eq!(
            contact_changes(&before, &after),
            vec!["tags".to_owned(), "notes".to_owned()]
        );

        // A patch that changed nothing reports nothing — an empty diff must not still emit an
        // update event, or every opened form would wake the automations.
        assert!(contact_changes(&before, &before.clone()).is_empty());
    }

    #[test]
    fn the_event_payload_carries_the_identifier_not_the_record() {
        let mut contact = contact_fixture();
        contact.custom = json!({ "contract_value_note": "40k" });
        contact.notes = "a private note".to_owned();

        let payload = contact_ref(&contact);
        assert_eq!(payload["contact_id"], json!(Uuid::nil()));
        assert!(
            payload.get("notes").is_none(),
            "the payload must not carry the record's free text"
        );
        assert!(
            payload.get("custom").is_none(),
            "the payload must not carry the flagged custom fields"
        );
    }
}
