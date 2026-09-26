//! `/api/v1/pages` — the content surface.
//!
//! v0 of the CMS content model (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7): a page
//! carries an append-only revision history, edits append a draft revision, publishing freezes
//! it and retires the one before, and any earlier revision can be restored forward. Content
//! changes that are not translations — the block editor, scheduling, review states — arrive in
//! later phases; the model here is the base they extend.
//!
//! Every route carries a permission guard and every state change writes an audit row. Pages
//! belong to sites, so the tenancy scope rule applies on top of the guard
//! (`crate::scope`): an account with a primary organization touches only its own tenants.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::model::{
    NewPage, NewRevisionTranslation, Page, PageChanges, PageRevision, Translation,
};
use omnion_content::{pages, translations};
use omnion_events::{NewEvent, bus};
use omnion_identity::sites::{self, Site};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// Public representation of one revision.
#[derive(Debug, Serialize)]
pub struct RevisionBody {
    /// Revision id.
    pub id: Uuid,
    /// Page the revision belongs to.
    pub page_id: Uuid,
    /// Monotonic revision number inside the page.
    pub revision_no: i32,
    /// `draft`, `published` or `archived`.
    pub state: String,
    /// Revision title.
    pub title: String,
    /// Revision body.
    pub body: String,
    /// Short summary, when the author wrote one.
    pub summary: Option<String>,
    /// Revision this one was restored from, when it was.
    pub restored_from_id: Option<Uuid>,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When the revision was published, when it ever was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub published_at: Option<OffsetDateTime>,
}

impl From<&PageRevision> for RevisionBody {
    fn from(revision: &PageRevision) -> Self {
        Self {
            id: revision.id,
            page_id: revision.page_id,
            revision_no: revision.revision_no,
            state: revision.state.clone(),
            title: revision.title.clone(),
            body: revision.body.clone(),
            summary: revision.summary.clone(),
            restored_from_id: revision.restored_from_id,
            created_at: revision.created_at,
            published_at: revision.published_at,
        }
    }
}

/// Public representation of a page, with the working draft and the visible revision.
#[derive(Debug, Serialize)]
pub struct PageBody {
    /// Page id.
    pub id: Uuid,
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Address of the page inside its site.
    pub slug: String,
    /// Content type key.
    pub page_type: String,
    /// `draft`, `published` or `archived`.
    pub status: String,
    /// Revision visitors currently see.
    pub published_revision_id: Option<Uuid>,
    /// Working draft, when the page has one.
    pub draft: Option<RevisionBody>,
    /// Published revision, when the page has one.
    pub published: Option<RevisionBody>,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl PageBody {
    /// Assemble a page with the states the store reports.
    fn build(page: &Page, draft: Option<&PageRevision>, published: Option<&PageRevision>) -> Self {
        Self {
            id: page.id,
            site_id: page.site_id,
            slug: page.slug.clone(),
            page_type: page.page_type.clone(),
            status: page.status.clone(),
            published_revision_id: page.published_revision_id,
            draft: draft.map(RevisionBody::from),
            published: published.map(RevisionBody::from),
            created_at: page.created_at,
            updated_at: page.updated_at,
        }
    }
}

/// Response body of the page list.
#[derive(Debug, Serialize)]
pub struct PagesResponse {
    /// Site the listed pages belong to.
    pub site_id: Uuid,
    /// Pages of the site.
    pub pages: Vec<PageBody>,
}

/// Response body of the revision history.
#[derive(Debug, Serialize)]
pub struct RevisionsResponse {
    /// Page the history belongs to.
    pub page_id: Uuid,
    /// Revisions, newest first.
    pub revisions: Vec<RevisionBody>,
}

/// Public representation of one translation row.
#[derive(Debug, Serialize)]
pub struct TranslationBody {
    /// Language tag.
    pub language: String,
    /// Field name.
    pub field: String,
    /// Translated value.
    pub value: String,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&Translation> for TranslationBody {
    fn from(translation: &Translation) -> Self {
        Self {
            language: translation.language.clone(),
            field: translation.field.clone(),
            value: translation.value.clone(),
            updated_at: translation.updated_at,
        }
    }
}

/// Response body of the translation endpoints.
#[derive(Debug, Serialize)]
pub struct TranslationsResponse {
    /// Revision the rows belong to.
    pub revision_id: Uuid,
    /// Rows, language and field order.
    pub translations: Vec<TranslationBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/pages`.
#[derive(Debug, Deserialize)]
pub struct PagesQuery {
    /// Site whose pages are listed.
    pub site_id: Uuid,
    /// Optional lifecycle filter: `draft`, `published` or `archived`.
    #[serde(default)]
    pub status: Option<String>,
}

/// `POST /api/v1/pages`.
#[derive(Debug, Deserialize)]
pub struct CreatePageRequest {
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Address of the page inside its site.
    pub slug: String,
    /// Title of the first revision.
    pub title: String,
    /// Content type key; `page` when omitted.
    #[serde(default)]
    pub page_type: Option<String>,
    /// Body of the first revision.
    #[serde(default)]
    pub body: Option<String>,
    /// Summary of the first revision.
    #[serde(default)]
    pub summary: Option<String>,
}

/// `PATCH /api/v1/pages/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdatePageRequest {
    /// New slug (a rename; no revision is written).
    #[serde(default)]
    pub slug: Option<String>,
    /// New title (appends a revision).
    #[serde(default)]
    pub title: Option<String>,
    /// New body (appends a revision).
    #[serde(default)]
    pub body: Option<String>,
    /// New summary; an empty string clears it (appends a revision).
    #[serde(default)]
    pub summary: Option<String>,
}

impl UpdatePageRequest {
    /// Fold the request into the store's change set.
    fn changes(self) -> PageChanges {
        PageChanges {
            slug: self.slug,
            title: self.title,
            body: self.body,
            summary: self.summary,
        }
    }
}

/// `POST /api/v1/pages/{id}/restore`.
#[derive(Debug, Deserialize)]
pub struct RestoreRevisionRequest {
    /// Revision whose content should be brought forward.
    pub revision_id: Uuid,
}

/// `PUT /api/v1/pages/{id}/revisions/{revision_id}/translations/{language}`.
#[derive(Debug, Deserialize)]
pub struct SetTranslationsRequest {
    /// Translated title.
    #[serde(default)]
    pub title: Option<String>,
    /// Translated body.
    #[serde(default)]
    pub body: Option<String>,
    /// Translated summary.
    #[serde(default)]
    pub summary: Option<String>,
}

impl SetTranslationsRequest {
    /// The fields the request carries, in a stable order.
    fn fields(self) -> Vec<(&'static str, String)> {
        let mut fields = Vec::new();
        if let Some(title) = self.title {
            fields.push(("title", title));
        }
        if let Some(body) = self.body {
            fields.push(("body", body));
        }
        if let Some(summary) = self.summary {
            fields.push(("summary", summary));
        }
        fields
    }
}

// ---------------------------------------------------------------------------------------------
// Page handlers
// ---------------------------------------------------------------------------------------------

/// List the pages of a site.
pub async fn list_pages(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<PagesQuery>,
) -> Result<Json<PagesResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let listed = pages::list_pages(state.db().pool(), site.id, query.status.as_deref()).await?;

    let mut bodies = Vec::with_capacity(listed.len());
    for page in &listed {
        bodies.push(load_page_body(&state, page).await?);
    }

    Ok(Json(PagesResponse {
        site_id: site.id,
        pages: bodies,
    }))
}

/// Create a page and its first, draft revision.
pub async fn create_page(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreatePageRequest>,
) -> Result<(StatusCode, Json<PageBody>), ApiError> {
    let site = site_in_scope(&state, &current, body.site_id).await?;

    let (page, revision) = pages::create_page(
        state.db().pool(),
        NewPage {
            site_id: site.id,
            slug: body.slug,
            page_type: body.page_type,
            title: body.title,
            body: body.body,
            summary: body.summary,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.created")
            .target("page", page.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "slug": page.slug,
                "revision_no": revision.revision_no,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(PageBody::build(&page, Some(&revision), None)),
    ))
}

/// Read one page with its working draft and published revision.
pub async fn get_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
) -> Result<Json<PageBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    Ok(Json(load_page_body(&state, &page).await?))
}

/// Edit a page: rename it and/or append a revision.
pub async fn update_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<UpdatePageRequest>,
) -> Result<Json<PageBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let changes = body.changes();
    let appends = changes.touches_content();

    let updated =
        pages::update_page(state.db().pool(), page.id, &changes, Some(current.user.id)).await?;
    let body = load_page_body(&state, &updated).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.updated")
            .target("page", updated.id.to_string())
            .metadata(json!({
                "slug": updated.slug,
                "revision_no": body.draft.as_ref().map(|draft| draft.revision_no),
                "content_changed": appends,
            }))
            .ip_address(address.as_text())
            .organization(site_of(&state, updated.site_id).await?.organization_id),
    )
    .await?;

    Ok(Json(body))
}

/// Delete a page together with its history and translations.
pub async fn delete_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let site = site_of(&state, page.site_id).await?;

    if !pages::delete_page(state.db().pool(), page.id).await? {
        return Err(page_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.deleted")
            .target("page", page.id.to_string())
            .metadata(json!({ "slug": page.slug }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Publish the page's working draft.
pub async fn publish_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<Json<PageBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let site = site_of(&state, page.site_id).await?;

    let (page, published) = pages::publish_page(state.db().pool(), page.id).await?;

    // Fan-out (docs/BUILD-BACKLOG.md P12): the platform's own bus records the publication, and
    // the bus queues one signed delivery per enabled endpoint of the organization subscribed to
    // `page.published`, in the same transaction as the event row. The site renderer already
    // serves the page either way — a bus that cannot record the fact reports the failure
    // instead of hiding it.
    let report = bus::emit(
        state.db().pool(),
        NewEvent::new("page.published")
            .organization(site.organization_id)
            .site(page.site_id)
            .actor(current.user.id)
            .payload(json!({
                "page_id": page.id,
                "site_id": page.site_id,
                "slug": page.slug,
                "status": page.status,
                "revision_id": published.id,
                "revision_no": published.revision_no,
                "title": published.title,
            })),
    )
    .await?;

    tracing::debug!(
        event_id = report.event.id,
        deliveries = report.deliveries,
        slug = %page.slug,
        "page.published recorded"
    );

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.published")
            .target("page", page.id.to_string())
            .metadata(json!({
                "slug": page.slug,
                "revision_no": published.revision_no,
                "revision_id": published.id,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(PageBody::build(&page, None, Some(&published))))
}

// ---------------------------------------------------------------------------------------------
// Revision handlers
// ---------------------------------------------------------------------------------------------

/// The revision history of a page, newest first.
pub async fn list_revisions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
) -> Result<Json<RevisionsResponse>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let revisions = pages::list_revisions(state.db().pool(), page.id).await?;

    Ok(Json(RevisionsResponse {
        page_id: page.id,
        revisions: revisions.iter().map(RevisionBody::from).collect(),
    }))
}

/// Read one revision of a page.
pub async fn get_revision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((page_id, revision_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<RevisionBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let revision = revision_of(&state, page.id, revision_id).await?;
    Ok(Json(RevisionBody::from(&revision)))
}

/// Restore an earlier revision: copy it forward as a new draft.
pub async fn restore_revision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<RestoreRevisionRequest>,
) -> Result<(StatusCode, Json<RevisionBody>), ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let site = site_of(&state, page.site_id).await?;

    let restored = pages::restore_revision(
        state.db().pool(),
        page.id,
        body.revision_id,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.revision.restored")
            .target("page", page.id.to_string())
            .metadata(json!({
                "slug": page.slug,
                "restored_from_id": body.revision_id,
                "revision_no": restored.revision_no,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(RevisionBody::from(&restored))))
}

// ---------------------------------------------------------------------------------------------
// Translation handlers
// ---------------------------------------------------------------------------------------------

/// The translation rows of one revision.
pub async fn list_translations(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((page_id, revision_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TranslationsResponse>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    revision_of(&state, page.id, revision_id).await?;
    let rows = translations::revision_translations(state.db().pool(), revision_id).await?;

    Ok(Json(TranslationsResponse {
        revision_id,
        translations: rows.iter().map(TranslationBody::from).collect(),
    }))
}

/// Write (or overwrite) translated fields of one revision in one language.
pub async fn set_translations(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((page_id, revision_id, language)): Path<(Uuid, Uuid, String)>,
    address: ClientAddress,
    Json(body): Json<SetTranslationsRequest>,
) -> Result<Json<TranslationsResponse>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let revision = revision_of(&state, page.id, revision_id).await?;
    let site = site_of(&state, page.site_id).await?;

    let fields = body.fields();
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "empty_translation",
            "name at least one translated field (title, body or summary)",
        ));
    }

    let mut written = Vec::with_capacity(fields.len());
    for (field, value) in fields {
        translations::set_revision_translation(
            state.db().pool(),
            NewRevisionTranslation {
                revision_id: revision.id,
                language: language.clone(),
                field: field.to_owned(),
                value,
                created_by: Some(current.user.id),
            },
        )
        .await?;
        written.push(field);
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.translation.updated")
            .target("page", page.id.to_string())
            .metadata(json!({
                "slug": page.slug,
                "revision_no": revision.revision_no,
                "language": language.to_lowercase(),
                "fields": written,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    let rows = translations::revision_translations(state.db().pool(), revision.id).await?;

    Ok(Json(TranslationsResponse {
        revision_id: revision.id,
        translations: rows.iter().map(TranslationBody::from).collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load a site or answer `404 site_not_found`.
async fn site_of(state: &AppState, site_id: Uuid) -> Result<Site, ApiError> {
    sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

/// Load a site and refuse it when it lives outside the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<Site, ApiError> {
    let site = site_of(state, site_id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// Load a page and refuse the request when its site is out of the caller's scope.
async fn page_in_scope(
    state: &AppState,
    current: &CurrentSession,
    page_id: Uuid,
) -> Result<Page, ApiError> {
    let page = pages::find_page(state.db().pool(), page_id)
        .await?
        .ok_or_else(page_not_found)?;
    site_in_scope(state, current, page.site_id).await?;
    Ok(page)
}

/// Load one revision of a page or answer `404 revision_not_found`.
async fn revision_of(
    state: &AppState,
    page_id: Uuid,
    revision_id: Uuid,
) -> Result<PageRevision, ApiError> {
    pages::find_revision(state.db().pool(), page_id, revision_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "revision_not_found",
                "no such revision on this page",
            )
        })
}

/// Assemble the page response with the working draft and the visible revision.
async fn load_page_body(state: &AppState, page: &Page) -> Result<PageBody, ApiError> {
    let pool = state.db().pool();
    let draft = pages::current_draft(pool, page.id).await?;
    let published = pages::published_revision(pool, page.id).await?;
    Ok(PageBody::build(page, draft.as_ref(), published.as_ref()))
}

fn page_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_content::validation::TRANSLATION_FIELDS;

    #[test]
    fn update_requests_fold_into_change_sets() {
        let rename = UpdatePageRequest {
            slug: Some(" About ".to_owned()),
            title: None,
            body: None,
            summary: None,
        }
        .changes();
        assert_eq!(rename.slug.as_deref(), Some(" About "));
        assert!(!rename.touches_content(), "a rename writes no revision");

        let empty = UpdatePageRequest {
            slug: None,
            title: None,
            body: None,
            summary: None,
        }
        .changes();
        assert!(empty.is_empty(), "an empty patch changes nothing");
    }

    #[test]
    fn translation_requests_cover_the_catalogued_fields() {
        let fields = SetTranslationsRequest {
            title: Some("Merhaba".to_owned()),
            body: None,
            summary: Some("Kısa".to_owned()),
        }
        .fields();

        let named: Vec<&str> = fields.iter().map(|(field, _)| *field).collect();
        assert_eq!(named, vec!["title", "summary"]);
        for (field, _) in &fields {
            assert!(
                TRANSLATION_FIELDS.contains(field),
                "{field} must be a translatable field"
            );
        }

        let none = SetTranslationsRequest {
            title: None,
            body: None,
            summary: None,
        }
        .fields();
        assert!(none.is_empty(), "an empty request names no field");
    }

    #[test]
    fn restore_requests_require_a_revision() {
        let body: RestoreRevisionRequest =
            serde_json::from_str(r#"{"revision_id":"11111111-1111-1111-1111-111111111111"}"#)
                .expect("valid body");
        assert_eq!(body.revision_id.to_string().len(), 36);

        let missing = serde_json::from_str::<RestoreRevisionRequest>("{}");
        assert!(missing.is_err(), "revision_id is required");
    }
}
