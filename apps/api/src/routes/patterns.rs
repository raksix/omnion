//! `/api/v1/patterns` and `/api/v1/page-templates` — REQ-063 slice 3.
//!
//! Two libraries with one shape. A **pattern** is a block group an author drops into a page; a
//! **page template** is a whole page's worth of blocks with sample content. Both are read with
//! `content.blocks.read` — looking at what you can build needs no more power than building does —
//! and written with `content.patterns.manage` / `content.templates.manage`.
//!
//! The two permissions are separate on purpose. Reusable content outlives the page it was made
//! for: a "hero" group inserted into forty pages is worth more than any of them, and an account
//! that may edit a page should not thereby gain the right to rewrite the library every page on
//! the site draws from. Merging them would make "who can publish a page" and "who can change the
//! site's building blocks" the same question, and they are not.
//!
//! `POST /api/v1/pages/from-template` sits in this module rather than in `content.rs` because it
//! is the template gallery's action: it needs `content.pages.create` (it creates a page) *and*
//! `content.templates.manage` is deliberately **not** required — reading the gallery and
//! building from a template is authoring, not curation.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::patterns::{
    NewPattern, NewTemplate, PageFromTemplate, Pattern, PatternChanges, PageTemplate,
};
use omnion_content::{SYSTEM_TEMPLATES, instance_blocks, patterns, templates};
use omnion_events::{NewEvent, bus};
use omnion_identity::sites;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// Public representation of one pattern.
///
/// `block_count` is computed server-side from the stored tree rather than kept in a column: a
/// counter column drifts the moment a pattern is edited through any other writer, and the
/// library card's number is a claim about the payload, so it is derived from the payload.
#[derive(Debug, Serialize)]
pub struct PatternBody {
    /// Pattern id.
    pub id: Uuid,
    /// Organization the pattern belongs to.
    pub organization_id: Uuid,
    /// Stable key.
    pub key: String,
    /// Name the card shows.
    pub name: String,
    /// Grouping the library filters by.
    pub category: String,
    /// One line describing the group.
    pub description: Option<String>,
    /// Blocks in the group, nested included.
    pub block_count: usize,
    /// The block group itself, for the "edit" flow and the insert preview.
    pub blocks: Value,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

impl From<&Pattern> for PatternBody {
    fn from(pattern: &Pattern) -> Self {
        Self {
            id: pattern.id,
            organization_id: pattern.organization_id,
            key: pattern.key.clone(),
            name: pattern.name.clone(),
            category: pattern.category.clone(),
            description: pattern.description.clone(),
            block_count: pattern.block_count(),
            blocks: pattern.blocks.clone(),
            updated_at: pattern.updated_at,
        }
    }
}

/// Public representation of one page template.
#[derive(Debug, Serialize)]
pub struct TemplateBody {
    /// Template id.
    pub id: Uuid,
    /// Stable key.
    pub key: String,
    /// Name the card shows.
    pub name: String,
    /// Content type a page created from it gets.
    pub page_type: String,
    /// One line describing it.
    pub description: Option<String>,
    /// Blocks in the template, nested included.
    pub block_count: usize,
    /// `true` when the platform ships this template.
    pub is_system: bool,
    /// The page's blocks, for the gallery's preview.
    pub blocks: Value,
}

impl From<&PageTemplate> for TemplateBody {
    fn from(template: &PageTemplate) -> Self {
        Self {
            id: template.id,
            key: template.key.clone(),
            name: template.name.clone(),
            page_type: template.page_type.clone(),
            description: template.description.clone(),
            block_count: template.block_count(),
            is_system: template.is_system,
            blocks: template.blocks.clone(),
        }
    }
}

/// The pattern list.
#[derive(Debug, Serialize)]
pub struct PatternsResponse {
    /// Organization the patterns belong to.
    pub organization_id: Uuid,
    /// Patterns, newest first.
    pub patterns: Vec<PatternBody>,
}

/// The template gallery.
#[derive(Debug, Serialize)]
pub struct TemplatesResponse {
    /// Templates, gallery order.
    pub templates: Vec<TemplateBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/patterns`.
#[derive(Debug, Deserialize)]
pub struct PatternsQuery {
    /// Optional category filter.
    #[serde(default)]
    pub category: Option<String>,
    /// The organization whose library is being read.
    ///
    /// The panel's Owner account is the account that has *no* primary organization — that is what
    /// makes it an Owner — so a route that only ever falls back to `user.organization_id` answers
    /// the owner `400 organization_required` on a screen it is the primary audience of. The
    /// selector is checked against the caller's own scope by `organization_in_scope` exactly like
    /// the write routes, so naming an organization the caller does not hold is still a refusal and
    /// not a way to read another tenant's library.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `GET /api/v1/page-templates`.
///
/// The same selector as `PatternsQuery`, and for the same reason: the templates screen is one of
/// the two the Owner opens first, and an owner with no primary organization could not read it.
#[derive(Debug, Deserialize)]
pub struct TemplatesQuery {
    /// The organization whose template set is being read.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/patterns` — create, or replace the pattern that already owns the key.
#[derive(Debug, Deserialize)]
pub struct SavePatternRequest {
    /// Stable key; an existing pattern with this key is replaced.
    pub key: String,
    /// Name the card shows.
    pub name: String,
    /// Grouping; `general` when omitted.
    #[serde(default)]
    pub category: Option<String>,
    /// One line describing the group.
    #[serde(default)]
    pub description: Option<String>,
    /// The block group.
    pub blocks: Value,
    /// Organization the pattern belongs to; the caller's own when omitted.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `PUT /api/v1/patterns/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdatePatternRequest {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New category.
    #[serde(default)]
    pub category: Option<String>,
    /// New description; an empty string clears it.
    #[serde(default)]
    pub description: Option<String>,
    /// New block group.
    #[serde(default)]
    pub blocks: Option<Value>,
}

/// `POST /api/v1/page-templates`.
#[derive(Debug, Deserialize)]
pub struct SaveTemplateRequest {
    /// Stable key; an existing template with this key is replaced.
    pub key: String,
    /// Name the card shows.
    pub name: String,
    /// Content type a page created from it gets.
    #[serde(default)]
    pub page_type: Option<String>,
    /// One line describing it.
    #[serde(default)]
    pub description: Option<String>,
    /// The page's blocks.
    pub blocks: Value,
    /// Organization the template belongs to; the caller's own when omitted.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/pages/from-template`.
#[derive(Debug, Deserialize)]
pub struct PageFromTemplateRequest {
    /// Site the new page belongs to.
    pub site_id: Uuid,
    /// Template whose blocks seed the page.
    pub template_id: Uuid,
    /// Address of the new page.
    pub slug: String,
    /// Title of the new page.
    pub title: String,
}

// ---------------------------------------------------------------------------------------------
// Pattern handlers
// ---------------------------------------------------------------------------------------------

/// List the organization's patterns.
pub async fn list_patterns(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<PatternsQuery>,
) -> Result<Json<PatternsResponse>, ApiError> {
    let organization_id = organization_in_scope(&current, query.organization_id)?;
    let listed =
        patterns::list_patterns(state.db().pool(), organization_id, query.category.as_deref())
            .await?;
    Ok(Json(PatternsResponse {
        organization_id,
        patterns: listed.iter().map(PatternBody::from).collect(),
    }))
}

/// Read one pattern.
pub async fn get_pattern(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(pattern_id): Path<Uuid>,
    Query(query): Query<PatternsQuery>,
) -> Result<Json<PatternBody>, ApiError> {
    let organization_id = organization_in_scope(&current, query.organization_id)?;
    // A path-addressed read has no body to name an organization in, so the selector rides the
    // query string for the same reason `PatternsQuery` carries it: the Owner has no primary
    // organization, and the panel reaches this screen by id. `organization_in_scope` still
    // refuses a caller naming an organization it does not hold.
    let pattern = patterns::find_pattern(state.db().pool(), organization_id, pattern_id)
        .await?
        .ok_or(Content404::Pattern)?;
    Ok(Json(PatternBody::from(&pattern)))
}

/// The blocks a pattern contributes, with ids fresh for the page they are going into.
///
/// A separate read endpoint rather than "the editor re-identifies what the list gave it": the
/// ids that matter are the ones the *page* will store, and a client that renames ids itself has
/// two implementations of "copy this pattern" — one on the server, one in the browser, which
/// disagree the moment either one learns a rule the other does not have.
pub async fn get_pattern_blocks(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(pattern_id): Path<Uuid>,
    Query(query): Query<PatternsQuery>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = organization_in_scope(&current, query.organization_id)?;
    // A path-addressed read has no body to name an organization in, so the selector rides the
    // query string for the same reason `PatternsQuery` carries it: the Owner has no primary
    // organization, and the panel reaches this screen by id. `organization_in_scope` still
    // refuses a caller naming an organization it does not hold.
    let pattern = patterns::find_pattern(state.db().pool(), organization_id, pattern_id)
        .await?
        .ok_or(Content404::Pattern)?;
    let blocks = instance_blocks(&pattern.blocks)?;
    Ok(Json(json!({
        "pattern_id": pattern.id,
        "blocks": omnion_content::blocks_to_value(&blocks),
        "block_count": pattern.block_count(),
    })))
}

/// Create a pattern, or replace the one that already owns the key.
pub async fn save_pattern(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SavePatternRequest>,
) -> Result<(StatusCode, Json<PatternBody>), ApiError> {
    let organization_id = organization_in_scope(&current, body.organization_id)?;
    let pool = state.db().pool();

    let existing = patterns::find_pattern_by_key(pool, organization_id, &body.key).await?;
    let was_update = existing.is_some();

    let pattern = patterns::save_pattern(
        pool,
        NewPattern {
            organization_id,
            key: body.key,
            name: body.name,
            category: body.category,
            description: body.description,
            blocks: body.blocks,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    emit(
        &state,
        organization_id,
        current.user.id,
        if was_update { "content.pattern.updated" } else { "content.pattern.created" },
        json!({ "pattern_id": pattern.id, "key": pattern.key, "category": pattern.category }),
    )
    .await?;

    let action = if was_update { "pattern.updated" } else { "pattern.created" };
    record(
        &state,
        current.user.id,
        action,
        "pattern",
        &pattern.id.to_string(),
        json!({ "key": pattern.key, "category": pattern.category, "block_count": pattern.block_count() }),
        &address,
        organization_id,
    )
    .await?;

    Ok((
        if was_update { StatusCode::OK } else { StatusCode::CREATED },
        Json(PatternBody::from(&pattern)),
    ))
}

/// Edit a pattern.
pub async fn update_pattern(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(pattern_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<UpdatePatternRequest>,
) -> Result<Json<PatternBody>, ApiError> {
    let organization_id = organization_in_scope(&current, None)?;
    let changes = PatternChanges {
        name: body.name,
        category: body.category,
        description: body.description,
        blocks: body.blocks,
    };
    let pattern = patterns::update_pattern(state.db().pool(), organization_id, pattern_id, &changes)
        .await?;

    emit(
        &state,
        organization_id,
        current.user.id,
        "content.pattern.updated",
        json!({ "pattern_id": pattern.id, "key": pattern.key, "category": pattern.category }),
    )
    .await?;

    record(
        &state,
        current.user.id,
        "pattern.updated",
        "pattern",
        &pattern.id.to_string(),
        json!({ "key": pattern.key, "block_count": pattern.block_count() }),
        &address,
        organization_id,
    )
    .await?;

    Ok(Json(PatternBody::from(&pattern)))
}

/// Delete a pattern.
pub async fn delete_pattern(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(pattern_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_in_scope(&current, None)?;
    let deleted = patterns::delete_pattern(state.db().pool(), organization_id, pattern_id).await?;
    if !deleted {
        return Err(Content404::Pattern.into());
    }

    emit(
        &state,
        organization_id,
        current.user.id,
        "content.pattern.deleted",
        json!({ "pattern_id": pattern_id }),
    )
    .await?;

    record(
        &state,
        current.user.id,
        "pattern.deleted",
        "pattern",
        &pattern_id.to_string(),
        json!({}),
        &address,
        organization_id,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Template handlers
// ---------------------------------------------------------------------------------------------

/// The page template gallery, with the platform's own starting points seeded on first read.
pub async fn list_templates(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<TemplatesQuery>,
) -> Result<Json<TemplatesResponse>, ApiError> {
    let organization_id = organization_in_scope(&current, query.organization_id)?;
    seed_system_templates(&state, organization_id).await?;
    let listed = patterns::list_templates(state.db().pool(), organization_id).await?;
    Ok(Json(TemplatesResponse {
        templates: listed.iter().map(TemplateBody::from).collect(),
    }))
}

/// Create a template, or replace the one that already owns the key.
pub async fn save_template(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<SaveTemplateRequest>,
) -> Result<(StatusCode, Json<TemplateBody>), ApiError> {
    let organization_id = organization_in_scope(&current, body.organization_id)?;
    let pool = state.db().pool();
    let existing = patterns::find_template_by_key(pool, organization_id, &body.key).await?;
    let was_update = existing.is_some();

    let template = patterns::save_template(
        pool,
        NewTemplate {
            organization_id,
            key: body.key,
            name: body.name,
            page_type: body.page_type,
            description: body.description,
            blocks: body.blocks,
            // A custom template can never claim `is_system`: that flag is what makes a row
            // undeletable, and a body that sets it would be a way to lock the gallery.
            is_system: false,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    emit(
        &state,
        organization_id,
        current.user.id,
        "content.template.created",
        json!({ "template_id": template.id, "key": template.key, "page_type": template.page_type }),
    )
    .await?;

    record(
        &state,
        current.user.id,
        "template.saved",
        "page_template",
        &template.id.to_string(),
        json!({ "key": template.key, "page_type": template.page_type }),
        &address,
        organization_id,
    )
    .await?;

    Ok((
        if was_update { StatusCode::OK } else { StatusCode::CREATED },
        Json(TemplateBody::from(&template)),
    ))
}

/// Delete a custom template. A system template answers 409 `template_is_system`.
pub async fn delete_template(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(template_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_in_scope(&current, None)?;
    let pool = state.db().pool();

    // The refusal is raised rather than reported by a false "deleted nothing": an author who
    // clicks delete on a platform template deserves to be told *why* it is still there, and
    // "404" for a row the gallery is still showing is the answer that sends them looking for a
    // bug that is not there.
    if let Some(template) = patterns::find_template(pool, organization_id, template_id).await? {
        if template.is_system {
            return Err(omnion_content::ContentError::TemplateIsSystem.into());
        }
    }

    let deleted = patterns::delete_template(pool, organization_id, template_id).await?;
    if !deleted {
        return Err(Content404::Template.into());
    }

    emit(
        &state,
        organization_id,
        current.user.id,
        "content.template.deleted",
        json!({ "template_id": template_id }),
    )
    .await?;

    record(
        &state,
        current.user.id,
        "template.deleted",
        "page_template",
        &template_id.to_string(),
        json!({}),
        &address,
        organization_id,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Create a page from a template: a draft page whose blocks match the template's.
pub async fn create_page_from_template(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<PageFromTemplateRequest>,
) -> Result<(StatusCode, Json<crate::routes::content::PageBody>), ApiError> {
    // The site's organization is the one that owns the templates, so the scope check is the
    // site's — a template from another organization can never be read here, and `create_page_from_template`
    // re-checks it against the template row it loads.
    let site = sites::find_site(state.db().pool(), body.site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))?;
    ensure_same_organization(&current, Some(site.organization_id))?;

    let (page, revision) = patterns::create_page_from_template(
        state.db().pool(),
        &PageFromTemplate {
            site_id: site.id,
            template_id: body.template_id,
            organization_id: site.organization_id,
            slug: body.slug,
            title: body.title,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    emit(
        &state,
        site.organization_id,
        current.user.id,
        "content.template.applied",
        json!({
            "template_id": body.template_id,
            "page_id": page.id,
            "slug": page.slug,
            "revision_no": revision.revision_no,
        }),
    )
    .await?;

    record(
        &state,
        current.user.id,
        "page.created_from_template",
        "page",
        &page.id.to_string(),
        json!({
            "template_id": body.template_id,
            "site_id": site.id,
            "slug": page.slug,
            "block_count": revision.blocks.as_array().map_or(0, Vec::len),
        }),
        &address,
        site.organization_id,
    )
    .await?;

    let page = omnion_content::pages::find_page(state.db().pool(), page.id)
        .await?
        .ok_or(Content404::Page)?;
    Ok((
        StatusCode::CREATED,
        Json(crate::routes::content::PageBody::from_store(
            &page,
            Some(&revision),
            None,
        )),
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Not-found errors, kept in one place so the codes cannot drift.
enum Content404 {
    Page,
    Pattern,
    Template,
}

impl From<Content404> for ApiError {
    fn from(which: Content404) -> Self {
        match which {
            Content404::Page => {
                ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
            }
            Content404::Pattern => {
                ApiError::new(StatusCode::NOT_FOUND, "pattern_not_found", "no such pattern")
            }
            Content404::Template => ApiError::new(
                StatusCode::NOT_FOUND,
                "template_not_found",
                "no such page template",
            ),
        }
    }
}

/// The organization a request acts on, with the tenancy scope rule applied.
///
/// `requested` is the body's `organization_id` when the caller named one. A caller with a
/// primary organization may only act on its own; a platform account (no primary organization)
/// may act on any, which is the same rule the sites and pages routes already apply.
fn organization_in_scope(current: &CurrentSession, requested: Option<Uuid>) -> Result<Uuid, ApiError> {
    let organization_id = requested.or(current.user.organization_id).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "organization_required",
            "name an organization to save a reusable block group into",
        )
    })?;
    ensure_same_organization(current, Some(organization_id))?;
    Ok(organization_id)
}

/// Seed the platform's own templates into an organization, once.
///
/// Seeded on the first gallery read rather than by a migration because the blocks are code: a
/// migration would freeze one copy of the sample content in a ledger row that no later release
/// can improve, and every organization would get it at a different time. The upsert is keyed on
/// `(organization_id, key)`, so a release that improves a template's blocks updates every
/// organization's copy of that one template and leaves custom templates alone.
async fn seed_system_templates(state: &AppState, organization_id: Uuid) -> Result<(), ApiError> {
    let pool = state.db().pool();
    for template in SYSTEM_TEMPLATES {
        let existing =
            patterns::find_template_by_key(pool, organization_id, template.key).await?;
        if let Some(existing) = existing {
            if existing.is_system {
                continue;
            }
        }
        patterns::save_template(
            pool,
            NewTemplate {
                organization_id,
                key: template.key.to_owned(),
                name: template.name.to_owned(),
                page_type: Some(template.page_type.to_owned()),
                description: Some(template.description.to_owned()),
                blocks: (template.blocks)(),
                is_system: true,
                created_by: None,
            },
        )
        .await?;
    }
    Ok(())
}

async fn emit(
    state: &AppState,
    organization_id: Uuid,
    actor: Uuid,
    event: &str,
    payload: Value,
) -> Result<(), ApiError> {
    bus::emit(
        state.db().pool(),
        NewEvent::new(event)
            .organization(organization_id)
            .actor(actor)
            .payload(payload),
    )
    .await?;
    Ok(())
}

///
/// `action` and `target_type` are `&'static str` because the audit entry itself stores them
/// that way (`NewAuditEntry` carries no lifetime of its own): a computed action name would have
/// to be interned before it reached the row, and an audit trail whose action is computed at
/// runtime is a trail nobody can grep.
async fn record(
    state: &AppState,
    actor: Uuid,
    action: &'static str,
    target_type: &'static str,
    target: &str,
    metadata: Value,
    address: &ClientAddress,
    organization_id: Uuid,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(actor, action)
            .target(target_type, target)
            .metadata(metadata)
            .ip_address(address.as_text())
            .organization(organization_id),
    )
    .await?;
    Ok(())
}

/// The system templates as plain JSON, for the panel's own error hints.
#[must_use]
pub fn system_template_keys() -> Vec<&'static str> {
    templates::SYSTEM_TEMPLATES.iter().map(|entry| entry.key).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_request_keeps_what_the_author_chose() {
        let body: SavePatternRequest = serde_json::from_str(
            r#"{"key":" Hero-CTA ","name":"Hero","category":"marketing",
                "description":"A hero with a button","blocks":[]}"#,
        )
        .expect("a valid body");
        assert_eq!(body.key, " Hero-CTA ", "the store normalises, not the request");
        assert_eq!(body.category.as_deref(), Some("marketing"));
        assert!(body.organization_id.is_none(), "the caller's own organization is implied");
    }

    #[test]
    fn a_save_without_blocks_is_refused_by_the_request_shape() {
        // `blocks` is not `Option`: a pattern with no block payload is not a pattern, and
        // defaulting it to `[]` would silently save an empty group the author cannot see.
        let missing = serde_json::from_str::<SavePatternRequest>(
            r#"{"key":"hero","name":"Hero"}"#,
        );
        assert!(missing.is_err(), "blocks is required");
    }

    #[test]
    fn the_gallery_offers_the_five_named_starting_points() {
        let keys = system_template_keys();
        for wanted in ["landing", "about", "pricing", "blog-post", "contact"] {
            assert!(keys.contains(&wanted), "{wanted} must be in the gallery");
        }
        assert_eq!(keys.len(), 5);
    }
}
