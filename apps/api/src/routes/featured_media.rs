//! `/api/v1/pages/{id}/featured-media` and `/api/v1/sites/{site_id}/featured-media/candidates`
//! — REQ-064 slice 4d, "media reuse".
//!
//! Four endpoints, and the split between them is the design:
//!
//! * **The page's own fields** (`GET`/`PUT /pages/{id}/featured-media`) are edited by whoever
//!   edits the page. They hang off the page for the same reason the SEO fields do: a featured
//!   image is a property of the page, and a site-wide "featured image" would be a setting one
//!   page silently shares with every other.
//!
//! * **The candidate list** (`GET /sites/{site_id}/featured-media/candidates`) is the picker. It
//!   is a *separate* read with a `media.read` guard rather than an inline expansion of the
//!   page's own read, because the two answer different questions and have different powers: what
//!   this page uses, and what it may use. One endpoint would need one guard for both, and the
//!   weaker one wins.
//!
//! * **The renderer's payload** is carried on the *public* page response rather than behind its
//!   own endpoint, and that is deliberate: a renderer that has to make a second request to learn
//!   whether a page has a picture is a renderer that can render a page without one. The
//!   degradation rule (a trashed file still renders, with a warning for the panel) is applied in
//!   [`public.rs`](super::public), reading the same store.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::featured::{
    FeaturedChanges, FeaturedImage, FeaturedMedia, FeaturedStore, PickableMedia,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

/// How many candidates one picker page asks for.
///
/// Sixty, not "all of them": a site with four thousand uploads must not make the editor wait for
/// four thousand rows to show the three it is looking at, and the picker is a filter-and-page
/// control. The panel's own search narrows before this number ever matters.
const CANDIDATE_PAGE: i32 = 60;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/pages/{id}/featured-media` — the fields plus what a renderer would do with them.
#[derive(Debug, Serialize)]
pub struct ReadFeaturedMediaBody {
    /// The fields.
    pub media: FeaturedMedia,
    /// The availability chip's label.
    pub availability_label: &'static str,
    /// The degradation sentence, when there is one.
    pub warning: Option<String>,
    /// Exactly what a renderer draws, so the panel's preview cannot disagree with the site.
    pub render: Option<FeaturedImage>,
}

/// The candidate list the picker shows.
#[derive(Debug, Serialize)]
pub struct CandidatesBody {
    /// The images this site may use, newest first.
    pub candidates: Vec<PickableMedia>,
    /// How many were asked for — the panel uses it to say "showing the first 60".
    pub limit: i32,
}

/// The query the picker sends.
#[derive(Debug, Deserialize)]
pub struct CandidatesQuery {
    /// How many rows. Clamped, because a client asking for everything is a client that wants a
    /// denial of service on somebody else's panel.
    #[serde(default)]
    pub limit: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/pages/{id}/featured-media` — the page's featured image and everything about it.
pub async fn get_featured_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
) -> Result<Json<ReadFeaturedMediaBody>, ApiError> {
    let (page, _site) = page_in_scope(&state, &current, page_id).await?;
    let store = FeaturedStore::new(state.db().pool().clone());
    let media = store.read(page.site_id, page_id).await?;
    let availability = media.availability();
    Ok(Json(ReadFeaturedMediaBody {
        warning: media.warning(),
        render: omnion_content::featured::renderable(&media),
        availability_label: availability.label(),
        media,
    }))
}

/// `PUT /api/v1/pages/{id}/featured-media` — write the page's featured image fields.
///
/// The response is the **read after the write**, from the same store, for the same reason the SEO
/// route returns the tags it just generated: the panel's preview and its availability chip are
/// read from what is stored, so a save cannot leave a stale chip next to a fresh image.
pub async fn put_featured_media(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    Json(input): Json<FeaturedChanges>,
) -> Result<Json<ReadFeaturedMediaBody>, ApiError> {
    let (page, site) = page_in_scope(&state, &current, page_id).await?;
    let store = FeaturedStore::new(state.db().pool().clone());

    let before = store.read(page.site_id, page_id).await?;
    store.write(page.site_id, page_id, &input).await?;
    let after = store.read(page.site_id, page_id).await?;

    // The audit row names the *change*, not the request. "media_id changed from a to b" is what
    // somebody reads when they ask why this page looks different than it did last month, and a
    // row holding only the new value answers half of it.
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "content.page.featured_media")
            .organization(site.organization_id)
            .target("page", page_id)
            .metadata(json!({
                "site_id": site.id,
                "before": {
                    "media_id": before.media_id,
                    "alt": before.alt,
                    "has_focal": before.focal_x.is_some(),
                },
                "after": {
                    "media_id": after.media_id,
                    "alt": after.alt,
                    "focal": omnion_content::featured::object_position(after.focal_x, after.focal_y),
                },
                "availability": after.availability(),
            })),
    )
    .await;

    let availability = after.availability();
    Ok(Json(ReadFeaturedMediaBody {
        warning: after.warning(),
        render: omnion_content::featured::renderable(&after),
        availability_label: availability.label(),
        media: after,
    }))
}

/// `GET /api/v1/sites/{site_id}/featured-media/candidates` — the picker's rows.
///
/// Only **live images**, because a trashed file cannot be served and offering it is an operator
/// picking something that answers 404 the moment they save. The `used_by_pages` count travels
/// with each row because it is the "reuse" half of this criterion made visible: a file already
/// on three pages is a file somebody is about to put on a fourth.
pub async fn list_candidates(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    Query(query): Query<CandidatesQuery>,
) -> Result<Json<CandidatesBody>, ApiError> {
    let _site = site_in_scope(&state, &current, site_id).await?;
    let limit = query.limit.unwrap_or(CANDIDATE_PAGE).clamp(1, 200);
    let store = FeaturedStore::new(state.db().pool().clone());
    Ok(Json(CandidatesBody {
        candidates: store.pickable(site_id, limit).await?,
        limit,
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Resolve a page, then the site it belongs to.
///
/// The same pair of steps the SEO route uses, and for the same reason: `pages` has no
/// organization column, so the site's organization is what a scope check has to compare against —
/// and resolving the site through the page rather than through the caller is what stops a panel
/// user reading a page of another site by guessing its id.
async fn page_in_scope(
    state: &AppState,
    current: &CurrentSession,
    page_id: Uuid,
) -> Result<(omnion_content::Page, omnion_identity::Site), ApiError> {
    let page = omnion_content::pages::find_page(state.db().pool(), page_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page"))?;
    let site = site_in_scope(state, current, page.site_id).await?;
    Ok((page, site))
}

/// Resolve a site through the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<omnion_identity::Site, ApiError> {
    let site = omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}
