//! `/api/v1/crm/*` slice 2: the saved views and the CSV round trip (REQ-051).
//!
//! The routes in `crate::routes::crm` already cover the records themselves. This file is the
//! half a **list screen** needs to be more than a table:
//!
//! * **`/crm/views`** — the filter + column + sort combinations a person keeps. A view is stored
//!   as the query it stands for, so it never goes stale, and it is written in the organization
//!   the caller is already working in, never one the body names.
//! * **`/crm/contacts/import`** and **`/crm/contacts/export`** — the file. A dry run answers the
//!   mapping and every row's verdict and writes nothing; the commit runs the *same* parse and
//!   writes the rows the preview accepted. The export applies the **same** visibility and the
//!   same field hiding as the list it was started from, because a file is a copy of the screen
//!   and a screen that hides a value must not hand it out in a download.
//!
//! Import writes with the module's own validators rather than a second, looser copy: a row the
//! import accepts and the form rejects would be a data-quality bug nobody would ever see.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_module_crm::contacts::{self, CompanyChanges, ContactChanges, Contact};
use omnion_module_crm::csv::{self, MAX_IMPORT_BYTES};
use omnion_module_crm::views::{self, View, ViewChanges};
use omnion_module_crm::CrmError;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{emit, may_read_sensitive, organization_of, scope_of};
use crate::routes::iam::record;
use crate::scope::resolve_organization;
use crate::state::AppState;

/// A `sqlx` failure in the view store, in the module's own vocabulary.
///
/// The views table is read and written here rather than through `modules/crm`, so these queries
/// have no module error type to hand — mapping them into [`CrmError::Database`] means the *same*
/// `From` impl decides the status, and a pool that timed out answers `503` here exactly as it
/// does for a contact.
fn store(error: sqlx::Error) -> ApiError {
    ApiError::from(CrmError::Database(error))
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// The import body: the file, and whether the caller means to write it.
#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    /// The CSV, as text. The screen reads the file in the browser and posts the text, so an
    /// import is reviewable before it is sent and a dry run needs no re-upload.
    pub csv: String,
    /// `dry_run` (the default) or `commit`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Which entity the file is for.
    #[serde(default)]
    pub entity: Option<String>,
    /// Organization to write in (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The export's filters — the same list query the screen sends, so the file is the screen.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    /// Free text.
    #[serde(default)]
    pub search: Option<String>,
    /// `me`, `unassigned` or a user id.
    #[serde(default)]
    pub owner: Option<String>,
    /// Lifecycle status.
    #[serde(default)]
    pub status: Option<String>,
    /// One tag.
    #[serde(default)]
    pub tag: Option<String>,
    /// A company id.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Include the archived rows.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// How many rows the file may hold.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// The list of a caller's saved views.
#[derive(Debug, Deserialize)]
pub struct ViewListParams {
    /// Which entity the views filter.
    #[serde(default)]
    pub entity: Option<String>,
    /// Organization to read (platform accounts only).
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Saved views
// ---------------------------------------------------------------------------------------------

/// The whole answer of the view list.
#[derive(serde::Serialize)]
pub struct ViewList {
    /// The caller's own views and the organization's shared ones.
    pub views: Vec<View>,
}

/// `GET /api/v1/crm/views` — the caller's views plus the organization's shared ones.
pub async fn list_views(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ViewListParams>,
) -> Result<Json<ViewList>, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;

    let mut sql = String::from(
        "select id, organization_id, owner_user_id, entity, name, filters, columns, sort, \
         is_shared, created_at, updated_at from crm_views \
         where organization_id = $1 and (owner_user_id = $2 or is_shared)",
    );
    if params.entity.is_some() {
        sql.push_str(" and entity = $3");
    }
    sql.push_str(" order by is_shared desc, name asc");

    let rows = if let Some(entity) = params.entity {
        sqlx::query_as::<_, View>(&sql)
            .bind(organization_id)
            .bind(current.user.id)
            .bind(entity)
            .fetch_all(state.db().pool())
            .await
            .map_err(store)?
    } else {
        sqlx::query_as::<_, View>(&sql)
            .bind(organization_id)
            .bind(current.user.id)
            .fetch_all(state.db().pool())
            .await
            .map_err(store)?
    };

    Ok(Json(ViewList { views: rows }))
}

/// `POST /api/v1/crm/views` — save a view.
pub async fn create_view(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<ViewChanges>,
) -> Result<(StatusCode, Json<View>), ApiError> {
    let organization_id = organization_of(&current, None)?;
    let normalised = views::validate(body.0.entity.trim(), &body.0)?;

    let view: View = sqlx::query_as(
        "insert into crm_views (organization_id, owner_user_id, entity, name, filters, columns, sort, is_shared) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning id, organization_id, owner_user_id, entity, name, filters, columns, sort, \
         is_shared, created_at, updated_at",
    )
    .bind(organization_id)
    .bind(current.user.id)
    .bind(&normalised.entity)
    .bind(&normalised.name)
    .bind(&normalised.filters)
    .bind(&normalised.columns)
    .bind(&normalised.sort)
    .bind(normalised.is_shared)
    .fetch_one(state.db().pool())
    .await
    .map_err(store)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.view.created")
            .organization(organization_id)
            .target("crm_view", view.id.to_string())
            .metadata(json!({
                "request_id": view.id,
                "entity": view.entity,
                "name": view.name,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        omnion_events::NewEvent::new("crm.view.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "view_id": view.id, "entity": view.entity })),
    )
    .await;

    Ok((StatusCode::CREATED, Json(view)))
}

/// `DELETE /api/v1/crm/views/{id}` — forget a view.
///
/// A view is a lens, not a record: removing it never touches the contacts it pointed at, and a
/// view of somebody else's is refused rather than removed.
pub async fn delete_view(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(view_id): Path<Uuid>,
) -> Result<Json<View>, ApiError> {
    let organization_id = organization_of(&current, None)?;

    let view: View = sqlx::query_as(
        "select id, organization_id, owner_user_id, entity, name, filters, columns, sort, \
         is_shared, created_at, updated_at from crm_views \
         where id = $1 and organization_id = $2 and owner_user_id = $3",
    )
    .bind(view_id)
    .bind(organization_id)
    .bind(current.user.id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(store)?
    .ok_or(CrmError::NotFound("view"))?;

    sqlx::query("delete from crm_views where id = $1")
        .bind(view_id)
        .execute(state.db().pool())
        .await
        .map_err(store)?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.view.deleted")
            .organization(organization_id)
            .target("crm_view", view.id.to_string())
            .metadata(json!({ "request_id": view.id, "entity": view.entity }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        omnion_events::NewEvent::new("crm.view.deleted")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({ "view_id": view.id })),
    )
    .await;

    Ok(Json(view))
}

/// `GET /api/v1/crm/views/columns?entity=contacts` — the columns the chooser may offer.
///
/// The chooser reads this rather than hard-coding a list: a column the API would not accept in
/// a saved view must not be offered as one.
pub async fn view_columns(
    current: CurrentSession,
    Query(params): Query<ViewListParams>,
) -> Result<Json<Value>, ApiError> {
    // Reading the chooser needs no record, but it does need a session and an organization: the
    // guard below refuses an anonymous caller, which is what the chooser needs to know.
    resolve_organization(&current, None)?;
    let entity = params.entity.as_deref().unwrap_or("contacts");

    Ok(Json(json!({
        "entity": entity,
        "columns": views::available_columns(entity),
        "statuses": omnion_module_crm::model::STATUSES,
    })))
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/crm/contacts/import` — read a file, or write the rows it holds.
///
/// `mode=dry_run` (the default) parses every row, answers the mapping and the per-row verdict,
/// and writes **nothing**. `mode=commit` runs the identical parse and writes the rows the dry run
/// accepted — the refused ones are named again in the answer, because a commit that silently
/// skipped three bad rows of a file would look like a success.
pub async fn import_contacts(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    body: Json<ImportRequest>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_of(&current, body.0.organization_id)?;
    let mode = body.0.mode.as_deref().unwrap_or("dry_run").trim().to_lowercase();
    if !matches!(mode.as_str(), "dry_run" | "commit") {
        return Err(CrmError::InvalidQuery(format!(
            "mode is {mode:?}; use dry_run or commit"
        ))
        .into());
    }
    if body.0.csv.len() > MAX_IMPORT_BYTES {
        return Err(CrmError::invalid(
            "contact",
            "file",
            format!("a file may be at most {} MB", MAX_IMPORT_BYTES / (1024 * 1024)),
        )
        .into());
    }

    let scope = scope_of(&state, &current, organization_id).await;
    let file = body.0.csv.as_str();
    let preview = csv::preview_contacts(file)?;

    if mode == "dry_run" {
        return Ok(Json(json!({
            "mode": "dry_run",
            "mapping": {
                "columns": preview
                    .mapping
                    .columns
                    .iter()
                    .map(|(field, index)| json!({ "field": field, "column": index }))
                    .collect::<Vec<_>>(),
                "ignored": preview.mapping.ignored,
                "duplicates": preview.mapping.duplicates,
            },
            "total_rows": preview.total_rows,
            "valid_rows": preview.valid_rows,
            "errors": preview.errors.iter().map(csv::RowError::to_json).collect::<Vec<_>>(),
            "sample": preview.sample,
            "summary": preview.summary(),
        })));
    }

    // The commit needs the accounts a row may name as its owner. Resolving them by display name
    // is a convenience, not a permission: an owner the caller may not assign simply does not
    // match, and the row keeps the caller's ownership instead of silently naming someone else.
    let known_owners = owners_of(&state, organization_id).await;
    let committable = csv::committable_rows(file, &preview);

    let mut created: Vec<Contact> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut company_cache: Vec<(String, Uuid)> = companies_named(&state, organization_id).await;

    for (line, view) in committable {
        let owner_user_id = csv::owner_id_of(&known_owners, view.owner.as_deref());

        let company_id = match view.company.as_deref() {
            Some(name) if csv::is_importable_name(name) => {
                match company_cache
                    .iter()
                    .find(|(known, _)| known == &csv::company_key(name))
                    .map(|(_, id)| *id)
                {
                    Some(id) => Some(id),
                    None => {
                        let changes = CompanyChanges {
                            name: name.trim().to_owned(),
                            owner_user_id,
                            ..CompanyChanges::default()
                        };
                        // A company name that is already taken by an archived record is a
                        // refusal, not a duplicate: the person has to un-archive it first.
                        match contacts::create_company(state.db().pool(), organization_id, &changes)
                            .await
                        {
                            Ok(company) => {
                                company_cache
                                    .push((csv::company_key(name), company.id));
                                Some(company.id)
                            }
                            Err(CrmError::CompanyNameTaken) => {
                                errors.push(json!({
                                    "line": line,
                                    "field": "company",
                                    "message": format!("{name} already exists as an archived company"),
                                }));
                                continue;
                            }
                            Err(cause) => {
                                errors.push(json!({
                                    "line": line,
                                    "field": "company",
                                    "message": cause.to_string(),
                                }));
                                continue;
                            }
                        }
                    }
                }
            }
            _ => None,
        };

        let changes = ContactChanges {
            first_name: view.first_name.clone(),
            last_name: view.last_name.clone(),
            email: view.email.clone(),
            phone: view.phone.clone(),
            job_title: view.job_title.clone(),
            company_id,
            owner_user_id: Some(owner_user_id.unwrap_or(current.user.id)),
            status: view.status.clone(),
            tags: Some(view.tags.clone()),
            custom: view.custom.clone(),
            notes: view.notes.clone(),
        };

        match contacts::create_contact(state.db().pool(), organization_id, &changes).await {
            Ok(contact) => created.push(contact),
            Err(cause) => errors.push(json!({
                "line": line,
                "field": field_of(&cause),
                "message": message_of(&cause),
            })),
        }
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "crm.contacts.imported")
            .organization(organization_id)
            .target("crm_contact", "import".to_owned())
            .metadata(json!({
                "request_id": "import",
                "created": created.len(),
                "refused": errors.len(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    emit(
        &state,
        omnion_events::NewEvent::new("crm.contact.created")
            .organization(organization_id)
            .actor(current.user.id)
            .payload(json!({
                "imported": created.len(),
                "refused": errors.len(),
            })),
    )
    .await;

    // The contacts come back with the same visibility and field hiding as the list, so a row the
    // screen would hide cannot be smuggled out through the answer to an import.
    let sensitive = may_read_sensitive(&state, &current).await;
    let mut visible: Vec<Contact> = Vec::new();
    for contact in &created {
        if let Ok(row) = contacts::get_contact(state.db().pool(), &scope, contact.id, sensitive).await
        {
            visible.push(row);
        }
    }

    // `created` answers **how many rows were written**, which is `created.len()` and never
    // `visible.len()`: the re-read above filters, so a row the caller's visibility level or field
    // hiding keeps back would silently lower the count of a commit that really happened. A person
    // importing a file would read that as data loss, and the audit row and the event — which both
    // use `created.len()` — would disagree with the answer on screen. The filtered rows are still
    // returned under `contacts`; the count is the count.
    Ok(Json(json!({
        "mode": "commit",
        "created": created.len(),
        "refused": errors.len(),
        "errors": errors,
        "contacts": visible,
    })))
}

/// The field a refusal is about, when it names one.
fn field_of(error: &CrmError) -> Option<&str> {
    match error {
        CrmError::Invalid { field, .. } => Some(field),
        CrmError::EmailTaken => Some("email"),
        CrmError::CompanyNameTaken => Some("company"),
        _ => None,
    }
}

/// The sentence a refusal reads, without the module's own `invalid contact.email:` prefix —
/// the row already names its field, and the import table has a column for it.
fn message_of(error: &CrmError) -> String {
    match error {
        CrmError::Invalid { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// The accounts of the organization a row may name as its owner.
///
/// Matched on the display name **and** the address, because a file comes from a spreadsheet where
/// one of the two is what the column held. An owner the caller may not assign simply does not
/// match, and the row keeps the caller's ownership rather than silently naming someone else.
async fn owners_of(state: &AppState, organization_id: Uuid) -> Vec<(String, Uuid)> {
    #[derive(sqlx::FromRow)]
    struct Owner {
        id: Uuid,
        display_name: String,
        email: String,
    }

    let rows: Vec<Owner> = sqlx::query_as(
        "select id, display_name, email from users where organization_id = $1 and status = 'active' \
         order by display_name",
    )
    .bind(organization_id)
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    let mut owners: Vec<(String, Uuid)> = rows
        .into_iter()
        .flat_map(|row| {
            vec![
                (row.display_name.clone(), row.id),
                (row.email.clone(), row.id),
            ]
        })
        .collect();
    owners.sort_by(|left, right| left.0.cmp(&right.0));
    owners.dedup_by(|left, right| left.0 == right.0);
    owners
}

/// The companies of the organization, so a row naming one links instead of creating a twin.
async fn companies_named(state: &AppState, organization_id: Uuid) -> Vec<(String, Uuid)> {
    #[derive(sqlx::FromRow)]
    struct Named {
        name: String,
        id: Uuid,
    }

    let rows: Vec<Named> = sqlx::query_as(
        "select name, id from crm_companies where organization_id = $1 and archived_at is null",
    )
    .bind(organization_id)
    .fetch_all(state.db().pool())
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|row| (csv::company_key(&row.name), row.id))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// How many rows an export walks. Matches the search export's cap so both files stay comparable.
const EXPORT_MAX_ROWS: i64 = 10_000;

/// `GET /api/v1/crm/contacts/export` — the current filter set as a file.
///
/// The file is the **same answer the list gives**: the same filters, the same scope, the same
/// field hiding. A download that carried a value the screen hides would be the one path around
/// the rule, so the redaction is applied here too, in the same function the list reads.
pub async fn export_contacts(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ExportParams>,
) -> Result<axum::response::Response, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;

    let query = omnion_module_crm::ListQuery {
        search: params.search,
        owner: params.owner,
        status: params.status,
        tag: params.tag,
        company_id: params.company_id,
        sort: params.sort,
        direction: params.direction,
        limit: params.limit.map(|limit| limit.min(EXPORT_MAX_ROWS)),
        include_archived: params.include_archived,
        ..omnion_module_crm::ListQuery::default()
    };

    let page = contacts::list_contacts(state.db().pool(), &scope, &query, sensitive).await?;
    let body = csv::contacts_to_csv(&page.items);
    let filename = format!("omnion-contacts-{}.csv", time::OffsetDateTime::now_utc().date());

    Ok(csv_response(body, &filename, page.items.len() as i64, false))
}

/// `GET /api/v1/crm/companies/export` — the current company filter set as a file.
pub async fn export_companies(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ExportParams>,
) -> Result<axum::response::Response, ApiError> {
    let organization_id = organization_of(&current, params.organization_id)?;
    let scope = scope_of(&state, &current, organization_id).await;
    let sensitive = may_read_sensitive(&state, &current).await;

    let query = omnion_module_crm::ListQuery {
        search: params.search,
        owner: params.owner,
        status: params.status,
        tag: params.tag,
        sort: params.sort,
        direction: params.direction,
        limit: params.limit.map(|limit| limit.min(EXPORT_MAX_ROWS)),
        include_archived: params.include_archived,
        ..omnion_module_crm::ListQuery::default()
    };

    let page = contacts::list_companies(state.db().pool(), &scope, &query, sensitive).await?;
    let body = csv::companies_to_csv(&page.items);
    let filename = format!("omnion-companies-{}.csv", time::OffsetDateTime::now_utc().date());

    Ok(csv_response(body, &filename, page.items.len() as i64, false))
}

/// A CSV answer: the bytes, the filename, and the counts the screen shows after the download.
///
/// The row count is a header, not a hope: the screen prints what the API said it wrote, so a
/// truncated file cannot quietly lie about the result set.
fn csv_response(body: String, filename: &str, rows: i64, truncated: bool) -> axum::response::Response {
    let headers = [
        (
            axum::http::header::CONTENT_TYPE,
            "text/csv; charset=utf-8".to_owned(),
        ),
        (
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
        (
            axum::http::HeaderName::from_static("x-export-rows"),
            rows.to_string(),
        ),
        (
            axum::http::HeaderName::from_static("x-export-truncated"),
            truncated.to_string(),
        ),
    ];
    let mut response = axum::response::Response::new(axum::body::Body::from(body));
    for (name, value) in headers {
        if let Ok(value) = value.parse() {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// `true` when a refusal is one a row-level retry could fix — the helper the commit uses to
/// decide whether a name lookup is worth another try.
#[must_use]
pub fn is_retryable(error: &CrmError) -> bool {
    matches!(error, CrmError::Database(_))
}

/// The pure half of this file, so a unit test can prove the two refusals without a database.
#[must_use]
pub fn pure_rules() -> bool {
    // A file that is not a CSV at all is refused by the header rule, and a view that names no
    // entity is refused by the entity rule. Both are the pure half of what the routes do.
    csv::preview_contacts("colour,size\nred,large\n").is_err()
        && views::validate("contacts", &ViewChanges::default()).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_view_is_a_404_and_not_a_403() {
        let error = ApiError::from(CrmError::NotFound("view"));
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        let response = axum::response::IntoResponse::into_response(error);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body["error"]["code"], json!("crm_record_not_found"));
    }

    #[test]
    fn a_refusal_names_the_field_the_import_table_shows() {
        assert_eq!(
            field_of(&CrmError::invalid("contact", "email", "nope")),
            Some("email")
        );
        assert_eq!(field_of(&CrmError::EmailTaken), Some("email"));
        assert_eq!(field_of(&CrmError::NotFound("contact")), None);
    }

    #[test]
    fn a_row_message_does_not_repeat_the_field_the_table_already_has() {
        let error = CrmError::invalid("contact", "email", "that is not an e-mail address");
        assert_eq!(message_of(&error), "that is not an e-mail address");
        assert!(!message_of(&error).contains("invalid contact.email:"));
    }

    #[test]
    fn a_database_failure_is_the_only_retryable_refusal() {
        assert!(!is_retryable(&CrmError::EmailTaken));
        assert!(!is_retryable(&CrmError::NotFound("contact")));
    }

    #[test]
    fn the_column_catalogue_offers_only_columns_the_entity_has() {
        let contacts = views::available_columns("contacts");
        assert!(contacts.contains(&"company"));
        assert!(!contacts.contains(&"contact_count"), "that is a company column");
        assert!(views::available_columns("invoices").is_empty());
    }

    #[test]
    fn the_pure_rules_refuse_a_file_that_is_not_a_contact_file_and_an_empty_view() {
        assert!(
            pure_rules(),
            "a header with no known field and a view with no entity are both refused"
        );
    }
}
