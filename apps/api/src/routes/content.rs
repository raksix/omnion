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
use omnion_content::{comments, pages, translations};
use omnion_events::{NewEvent, bus};
use omnion_identity::sites::{self, Site};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    /// The page's block tree as stored JSON (REQ-063). `[]` for a revision that renders from
    /// its body, which is every revision written before the block system.
    pub blocks: Value,
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
            blocks: revision.blocks.clone(),
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
    pub fn from_store(
        page: &Page,
        draft: Option<&PageRevision>,
        published: Option<&PageRevision>,
    ) -> Self {
        Self::build(page, draft, published)
    }

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
    /// New block tree as stored JSON (REQ-063, appends a revision after validation).
    #[serde(default)]
    pub blocks: Option<Value>,
}

impl UpdatePageRequest {
    /// Fold the request into the store's change set.
    fn changes(self) -> PageChanges {
        PageChanges {
            slug: self.slug,
            title: self.title,
            body: self.body,
            summary: self.summary,
            blocks: self.blocks,
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

    emit(
        &state,
        page_event(
            NewEvent::new("page.created").payload(json!({
                "page_id": page.id,
                "site_id": site.id,
                "slug": page.slug,
                "status": page.status,
            })),
            &site,
            current.user.id,
        ),
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
    let site = site_of(&state, page.site_id).await?;
    let changes = body.changes();
    let appends = changes.touches_content();

    let updated =
        pages::update_page(state.db().pool(), page.id, &changes, Some(current.user.id)).await?;
    let body = load_page_body(&state, &updated).await?;

    // A block save is a structural change, and the platform's own bus is where downstream
    // listeners learn about those. The payload carries the count, never the tree: the events
    // surface is read by integrations that must not be handed a page's whole body.
    //
    // The event is gated on the request actually carrying a block tree, not merely on a draft
    // existing. A rename PATCH appends a draft revision like any other content change, so gating
    // on the draft made every title edit announce "blocks updated" with a count of zero — and an
    // integration subscribed to it would rebuild a page's media, re-run a diff and re-publish a
    // CDN cache for a page whose blocks never moved. The audit entry below still records the
    // rename, so nothing is lost; it simply stops being reported as a structural change.
    if changes.blocks.is_some()
        && let Some(draft) = &body.draft
    {
        let block_count = omnion_content::validate(&draft.blocks).block_count;
        let _report = bus::emit(
            state.db().pool(),
            NewEvent::new("content.blocks.updated")
                .organization(site_of(&state, updated.site_id).await?.organization_id)
                .site(updated.site_id)
                .actor(current.user.id)
                .payload(json!({
                    "page_id": updated.id,
                    "revision_no": draft.revision_no,
                    "block_count": block_count,
                })),
        )
        .await?;
    }
    // An endpoint subscribed to `page.*` hears edits too. Only an edit that actually appended
    // a revision is a content change; a pure rename is still worth telling, so it is reported
    // with `status` omitted rather than filtered out — a receiver that rebuilds a sitemap needs
    // the rename, and the payload's `status` is the optional field for exactly that.
    emit(
        &state,
        page_event(
            NewEvent::new("page.updated").payload(json!({
                "page_id": updated.id,
                "site_id": updated.site_id,
                "slug": updated.slug,
                "status": updated.status,
                "content_changed": appends,
            })),
            &site,
            current.user.id,
        ),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.updated")
            .target("page", updated.id.to_string())
            .metadata(json!({
                "slug": updated.slug,
                "revision_no": body.draft.as_ref().map(|draft| draft.revision_no),
                "content_changed": appends,
                "blocks_changed": changes.blocks.is_some(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
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

    emit(
        &state,
        page_event(
            NewEvent::new("page.deleted").payload(json!({
                "page_id": page.id,
                "site_id": page.site_id,
                "slug": page.slug,
            })),
            &site,
            current.user.id,
        ),
    )
    .await?;

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

    // A draft whose block tree still has an error is refused here, not rendered: the editor's
    // Save is deliberately allowed to keep an incomplete tree (an author is mid-sentence), so
    // publication is the moment the page has to be whole. The message names the first block and
    // what it needs, which is the sentence the author has to act on.
    if let Some(draft) = pages::current_draft(state.db().pool(), page.id).await? {
        let report = omnion_content::validate(&draft.blocks);
        if let Some(issue) = report.first_error() {
            return Err(ApiError::bad_request(
                "blocks_not_publishable",
                format!(
                    "this page cannot be published yet: {} (block {}, {})",
                    issue.message, issue.block_id, issue.path
                ),
            ));
        }
    }

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
                "block_count": omnion_content::validate(&published.blocks).block_count,
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

/// `GET /api/v1/pages/{id}/preview?viewport={mobile|desktop}`.
///
/// The renderer-frame payload for the page's *working draft* (REQ-063, slice 2).
///
/// The frame is a real render, not a second representation of the page: the tree that leaves
/// here is the same one `GET /pages/{id}` hands the editor and the same one the public renderer
/// draws, passed through the *server's* viewport filter. A preview that filtered with CSS would
/// show the author a phone page with a desktop block still in the DOM, which is exactly the
/// "preview lies" bug the REQ names.
///
/// The payload is scoped to the draft on purpose. Inline editing writes draft revisions, so a
/// frame that read the published revision would be reviewing the page visitors are not seeing,
/// and a save in the frame would appear to do nothing to it.
pub async fn preview_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    Query(query): Query<PreviewQuery>,
) -> Result<Json<PagePreviewBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let pool = state.db().pool();
    let draft = pages::current_draft(pool, page.id).await?;
    let published = pages::published_revision(pool, page.id).await?;

    // A page with no draft at all is a page that has never been edited since creation, which the
    // model makes impossible — but the frame says so rather than rendering an empty page, since
    // "nothing here" and "nothing to preview" are different answers.
    //
    // The one page that *does* land here is a page that was published and never touched again,
    // which is most of them. `publish_page` promotes the draft row to `published` in place, so
    // after a publish the page has no `draft` row at all and this frame answered `404
    // no_draft_revision` — the preview of a published, working page was a dead screen, and the
    // author who opened it after publishing saw a 404 where their page should be. A preview
    // falls back to the revision visitors are actually seeing, which is the honest answer when
    // there is no newer work than the published copy.
    let draft = match draft {
        Some(revision) => revision,
        None => published.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "no_draft_revision",
                "this page has no working draft to preview",
            )
        })?,
    };

    let viewport = preview_viewport(query.viewport.as_deref());
    // The filter is the same call the public renderer makes, so "what the phone sees" has one
    // implementation. A preview that filtered differently from the site would be a third answer
    // to the same question.
    let read_on = if viewport == "mobile" {
        omnion_content::ReadOn::Mobile
    } else {
        omnion_content::ReadOn::Desktop
    };
    let parsed = omnion_content::parse_blocks(&draft.blocks).unwrap_or_default();
    let visible = omnion_content::filter_for_viewport(&parsed, read_on);
    let report = omnion_content::validate(&draft.blocks);

    Ok(Json(PagePreviewBody {
        page_id: page.id,
        slug: page.slug.clone(),
        title: draft.title.clone(),
        viewport,
        // The frame carries the whole stored tree *and* the filtered one, and the two numbers are
        // not equal whenever the author hid something. Rendering only the filtered tree would
        // make a "hidden on phones" block indistinguishable from a deleted one.
        blocks: omnion_content::blocks_to_value(&parsed),
        visible_blocks: omnion_content::blocks_to_value(&visible),
        block_count: parsed.len() as i32,
        visible_count: visible.len() as i32,
        body: draft.body.clone(),
        revision_id: draft.id,
        revision_no: draft.revision_no,
        published_revision_no: published.as_ref().map(|entry| entry.revision_no),
        can_publish: report.can_publish,
        // The issues travel as the validator's own struct list, serialised here rather than typed
        // into the response: they are a read-only report, and a struct field would freeze the
        // validator's shape into the wire format the first time a code appeared on an issue.
        issues: serde_json::to_value(&report.issues).unwrap_or_else(|_| json!([])),
    }))
}

/// `?viewport=` — which screen the frame is drawing for.
#[derive(Debug, Default, Deserialize)]
pub struct PreviewQuery {
    /// `mobile` asks for the phone render; anything else (including an unknown value) is the
    /// wide one, so a typo in a link cannot produce a frame that draws nothing.
    #[serde(default)]
    pub viewport: Option<String>,
}

fn preview_viewport(value: Option<&str>) -> &'static str {
    match value.map(str::trim) {
        Some("mobile") => "mobile",
        _ => "desktop",
    }
}

/// Response body of the preview frame.
#[derive(Debug, Serialize)]
pub struct PagePreviewBody {
    /// Page the frame draws.
    pub page_id: Uuid,
    /// Address of the page inside its site.
    pub slug: String,
    /// Draft title, as the frame's document title.
    pub title: String,
    /// `desktop` or `mobile` — the screen this payload was filtered for.
    pub viewport: &'static str,
    /// The stored block tree, unfiltered.
    pub blocks: Value,
    /// The tree this viewport actually renders.
    pub visible_blocks: Value,
    /// Blocks in the stored tree.
    pub block_count: i32,
    /// Blocks this viewport renders.
    pub visible_count: i32,
    /// Plain body text, for a page that still renders from its body.
    pub body: String,
    /// Draft revision the frame reads.
    pub revision_id: Uuid,
    /// Its revision number.
    pub revision_no: i32,
    /// The revision visitors see, when the page has one. The frame names it so the author can
    /// see that their inline edits have not reached the public page.
    pub published_revision_no: Option<i32>,
    /// Whether the draft is whole enough to publish.
    pub can_publish: bool,
    /// Validation issues of the stored tree, so the frame can show the same badges the editor
    /// does rather than a second opinion.
    pub issues: Value,
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

/// `GET /api/v1/pages/{id}/revisions/{revision_id}/diff?against={revision_id}`.
///
/// The block-level compare behind the revisions screen (REQ-063 acceptance 13): "shows
/// added/removed/changed blocks with prop-level detail, not a raw JSON diff".
///
/// `against` is optional and defaults to the previous revision, so opening a revision with no
/// query string answers the question an author actually has — "what changed in this one" —
/// rather than refusing for a missing parameter.
///
/// The response carries BOTH revisions' `body` text as a plain string compare alongside the
/// block rows. A page that still renders from its body has no blocks at all, and a compare that
/// reported "nothing changed" for a page whose paragraphs were rewritten would be a lie; the
/// block rows answer the same question for a block-built page and this answers it for the rest.
pub async fn diff_revision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((page_id, revision_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<RevisionDiffBody>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let revision = revision_of(&state, page.id, revision_id).await?;

    // The base is the nearest older revision unless the caller named one. Comparing a revision
    // with itself is refused rather than answered as "no changes": it means the query was built
    // wrong, and returning an empty diff would hide that.
    let base = match query.against {
        Some(other) => revision_of(&state, page.id, other).await?,
        None => pages::list_revisions(state.db().pool(), page.id)
            .await?
            .into_iter()
            .filter(|candidate| candidate.revision_no < revision.revision_no)
            .max_by_key(|candidate| candidate.revision_no)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "no_earlier_revision",
                    format!(
                        "revision {} is the first one on this page, so there is nothing to compare it with",
                        revision.revision_no
                    ),
                )
            })?,
    };

    if base.id == revision.id {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "diff_same_revision",
            "a revision cannot be compared with itself",
        ));
    }

    // A payload either side cannot parse is compared as "no blocks" rather than refused: the
    // page still renders (the renderer's own fallback takes over) and an author asking what
    // changed should get the body compare even if one side's block tree is corrupt.
    let blocks_of =
        |value: &Value| omnion_content::parse_blocks(value).unwrap_or_else(|_| Vec::new());
    let diff = omnion_content::diff_blocks(&blocks_of(&base.blocks), &blocks_of(&revision.blocks));

    Ok(Json(RevisionDiffBody {
        page_id: page.id,
        base: DiffRevisionRef::from(&base),
        compared: DiffRevisionRef::from(&revision),
        blocks: serde_json::to_value(&diff).unwrap_or(Value::Null),
        body: BodyDiff {
            changed: base.body != revision.body,
            before: base.body.clone(),
            after: revision.body.clone(),
        },
    }))
}

/// `?against=` — which revision to compare against.
#[derive(Debug, Default, Deserialize)]
pub struct DiffQuery {
    /// Revision to compare against; the nearest earlier one when omitted.
    #[serde(default)]
    pub against: Option<Uuid>,
}

/// One side of a compare, as a pointer rather than a full revision.
#[derive(Debug, Serialize)]
pub struct DiffRevisionRef {
    /// Revision id.
    pub id: Uuid,
    /// Monotonic revision number.
    pub revision_no: i32,
    /// `draft`, `published` or `archived`.
    pub state: String,
    /// Revision title, for the compare header.
    pub title: String,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&PageRevision> for DiffRevisionRef {
    fn from(revision: &PageRevision) -> Self {
        Self {
            id: revision.id,
            revision_no: revision.revision_no,
            state: revision.state.clone(),
            title: revision.title.clone(),
            created_at: revision.created_at,
        }
    }
}

/// How a revision's plain body text compared.
#[derive(Debug, Serialize)]
pub struct BodyDiff {
    /// `false` when the two bodies are byte-identical.
    pub changed: bool,
    /// Body before.
    pub before: String,
    /// Body after.
    pub after: String,
}

/// The compare two revisions, as the revisions screen reads it.
#[derive(Debug, Serialize)]
pub struct RevisionDiffBody {
    /// Page the compare belongs to.
    pub page_id: Uuid,
    /// The revision the change is measured from.
    pub base: DiffRevisionRef,
    /// The revision being read.
    pub compared: DiffRevisionRef,
    /// The block compare: `entries`, `added`, `removed`, `changed`, `moved`, `has_removals`.
    pub blocks: Value,
    /// The body compare, for a page that still renders from plain text.
    pub body: BodyDiff,
}

/// One comment on a revision, as the panel reads it.
#[derive(Debug, Serialize)]
pub struct RevisionCommentBody {
    /// Comment id.
    pub id: Uuid,
    /// Revision the comment is about.
    pub revision_id: Uuid,
    /// Account that wrote it; `None` for an automation.
    pub author_user_id: Option<Uuid>,
    /// `user` or `automation` — where the note came from.
    pub source: String,
    /// The note itself.
    pub body: String,
    /// When it was written.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl RevisionCommentBody {
    /// Describe one stored comment.
    fn build(comment: &comments::RevisionComment) -> Self {
        Self {
            id: comment.id,
            revision_id: comment.revision_id,
            author_user_id: comment.author_user_id,
            source: comment.source.clone(),
            body: comment.body.clone(),
            created_at: comment.created_at,
        }
    }
}

/// The comments of one revision.
#[derive(Debug, Serialize)]
pub struct RevisionCommentsResponse {
    /// Page the revision belongs to.
    pub page_id: Uuid,
    /// Revision the comments are about.
    pub revision_id: Uuid,
    /// Comments, oldest first.
    pub comments: Vec<RevisionCommentBody>,
}

/// Read the comments of one revision (oldest first).
///
/// Comments are written by people (arriving with the review flow) and by automations — the
/// `comment_revision` action of the automation layer leaves one when a rule fires on an event.
pub async fn list_revision_comments(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((page_id, revision_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<RevisionCommentsResponse>, ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let revision = revision_of(&state, page.id, revision_id).await?;

    let notes =
        comments::list_for_revision(state.db().pool(), revision.id, COMMENTS_PAGE_MAX).await?;

    Ok(Json(RevisionCommentsResponse {
        page_id: page.id,
        revision_id: revision.id,
        comments: notes.iter().map(RevisionCommentBody::build).collect(),
    }))
}

/// Most comments one revision's list returns.
const COMMENTS_PAGE_MAX: i64 = 200;

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

    // Restoring a revision copies it forward as a new draft. The page was never removed, so the
    // fact a receiver wants is "its content is back the way it was" — `page.restored` with the
    // revision it came from, which is the only identifier that makes the restore reproducible.
    emit(
        &state,
        page_event(
            NewEvent::new("page.restored").payload(json!({
                "page_id": page.id,
                "site_id": page.site_id,
                "slug": page.slug,
                "restored_from_revision_id": body.revision_id,
                "revision_no": restored.revision_no,
            })),
            &site,
            current.user.id,
        ),
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

    emit(
        &state,
        page_event(
            NewEvent::new("translation.updated").payload(json!({
                "page_id": page.id,
                "site_id": page.site_id,
                "locale": language.to_lowercase(),
                "revision_id": revision.id,
                "fields": written,
            })),
            &site,
            current.user.id,
        ),
    )
    .await?;

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

/// Record a content fact on the platform's bus (REQ-016 slice 2).
///
/// The page lifecycle is what the outside world most wants to hear about, and until this call
/// existed the catalogue *promised* `page.created`, `page.updated`, `page.deleted` and
/// `page.restored` as live names while nothing ever recorded them. A `bus::emit` is a `bus::emit`
/// here for the same reason it is beside `page.published`: an endpoint subscribed to
/// `page.*` is entitled to hear the whole lifecycle, not only the publish.
async fn emit(state: &AppState, event: NewEvent) -> Result<(), ApiError> {
    let report = bus::emit(state.db().pool(), event).await?;

    tracing::debug!(
        event_id = report.event.id,
        deliveries = report.deliveries,
        name = %report.event.name,
        "content event recorded"
    );

    Ok(())
}

/// Attach a page's site and its actor to an event that has already been named.
///
/// The name arrives as a built `NewEvent` rather than a `&str` on purpose. The first version
/// took the name as a string, and that has a cost worth writing down: the drift gate
/// (`apps/api/tests/events.rs`) finds emitters by looking for the constructor call in the
/// source, so a name that lives in a helper's argument is invisible to it. It fired at once,
/// reporting `page.created` unbacked from a file that emitted it three lines above. A gate
/// that a convenience wrapper can blind is a gate to design against, so the literal stays at
/// the call site and this helper only fills in what every content event shares.
fn page_event(event: NewEvent, site: &Site, actor: Uuid) -> NewEvent {
    event
        .organization(site.organization_id)
        .site(site.id)
        .actor(actor)
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
            blocks: None,
        }
        .changes();
        assert_eq!(rename.slug.as_deref(), Some(" About "));
        assert!(!rename.touches_content(), "a rename writes no revision");

        let empty = UpdatePageRequest {
            slug: None,
            title: None,
            body: None,
            summary: None,
            blocks: None,
        }
        .changes();
        assert!(empty.is_empty(), "an empty patch changes nothing");
    }

    #[test]
    fn a_block_only_patch_is_a_content_change() {
        // REQ-063: blocks live on the revision, so saving a block tree writes a draft revision
        // exactly like saving the body does. A panel that only sends `blocks` must therefore
        // never look like a no-op.
        let blocks_only = UpdatePageRequest {
            slug: None,
            title: None,
            body: None,
            summary: None,
            blocks: Some(serde_json::json!([])),
        }
        .changes();
        assert!(blocks_only.touches_content());
        assert!(!blocks_only.is_empty());
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
