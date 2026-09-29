//! `/api/v1/themes`, `/api/v1/sites/{site_id}/theme` — REQ-062 slice 1, the gallery.
//!
//! Four endpoints, and the split is the design:
//!
//! * **The gallery** (`GET /themes?site=<id>`) is site-scoped, not organization-scoped, because
//!   the question the screen answers is "what can THIS site render with". A bundled theme
//!   belongs to no organization and an uploaded one belongs to the organization that installed
//!   it, so the read is a union — which is also what lets a platform owner with no
//!   organization of their own still see the ten bundled themes.
//!
//! * **Activation** (`POST /sites/{id}/theme`) and **rollback** (`POST /sites/{id}/theme/rollback`)
//!   are separate routes rather than one route with a flag. They have different guards'
//!   *consequences* — a rollback can restore a key the site no longer may activate directly —
//!   and they emit different event names, so a flag would be a boolean that changes three
//!   things at once.
//!
//! * **The theme itself** (`GET /themes/{key}`) carries no site scope, because a theme is a
//!   presentation asset and both the preview frame and the installer need it. What it refuses
//!   is a key no live theme carries.
//!
//! The response of every write is the **gallery after the write**, read back through the same
//! store, for the reason the featured-media route returns its own write: a panel that shows
//! "Active" on the card it was told about, and a `Restore previous` button that is only
//! rendered when the server says there is something to restore, cannot disagree with the
//! database — and the button's existence is a fact the client is not allowed to invent.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::themes::{self, GalleryView};
use omnion_events::NewEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The gallery query. `site` is required: there is no "gallery of the whole platform", because
/// the answer would differ per site anyway and a default would pick one of them silently.
#[derive(Debug, Deserialize)]
pub struct GalleryQuery {
    /// Site the gallery is for.
    pub site: Uuid,
}

/// `POST /sites/{id}/theme` — the body.
#[derive(Debug, Deserialize)]
pub struct ActivateBody {
    /// Key of the theme to activate.
    pub theme_key: String,
}

/// The write response: the gallery after the write, plus what changed.
#[derive(Debug, Serialize)]
pub struct ActivationBody {
    /// The gallery, exactly as `GET /themes?site=<id>` would answer now.
    pub gallery: GalleryView,
    /// The key now active.
    pub theme_key: String,
    /// The key it replaced, for the confirmation strip and the toast.
    pub previous_theme_key: Option<String>,
    /// Whether this was a restore rather than a forward switch.
    pub restored: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/themes?site=<id>` — the gallery for one site.
pub async fn list_gallery(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<GalleryQuery>,
) -> Result<Json<GalleryView>, ApiError> {
    let site = site_in_scope(&state, &current, query.site).await?;
    Ok(Json(
        themes::gallery_for_site(state.db().pool(), site.id, Some(site.organization_id)).await?,
    ))
}

/// `GET /api/v1/themes/{key}` — one theme, for the preview frame and the installer.
pub async fn get_theme(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The read is guarded by `themes.read`, and the organization filter is *not* applied:
    // a theme's manifest is presentation data, and a preview frame that could not read the
    // candidate theme's tokens would render the very fallback this route exists to avoid.
    // Uploads are the exception and the store is where that is enforced — an uploaded theme
    // is reachable by key, but the gallery never lists another organization's package.
    let _ = current;
    let theme = themes::find_theme(state.db().pool(), &key)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "theme_not_found",
                "no installed theme has this key",
            )
        })?;
    Ok(Json(themes::describe(&theme)))
}

/// `POST /api/v1/sites/{id}/theme` — activate a theme for a site.
pub async fn activate_theme(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<ActivateBody>,
) -> Result<Json<ActivationBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;

    let change = themes::activate(
        state.db().pool(),
        site.id,
        Some(site.organization_id),
        &body.theme_key,
        Some(current.user.id),
    )
    .await?;

    // The audit row carries BOTH keys, not just the new one. "Who switched this site off the
    // theme it was on" is the question somebody asks when a site looks wrong, and a row with
    // only the destination cannot answer it.
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.theme.activated")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(json!({
                "site_id": site.id,
                "site_key": site.key,
                "theme_key": change.theme_key,
                "previous_theme_key": change.previous_theme_key,
            })),
    )
    .await;

    emit_theme_event(
        &state,
        site.organization_id,
        site.id,
        "themes.theme.activated",
        &change.theme_key,
        change.previous_theme_key.as_deref(),
    )
    .await;

    Ok(Json(ActivationBody {
        gallery: read_gallery(&state, &site).await?,
        theme_key: change.theme_key,
        previous_theme_key: change.previous_theme_key,
        restored: change.restored,
    }))
}

/// `POST /api/v1/sites/{id}/theme/rollback` — restore the theme the last activation displaced.
pub async fn rollback_theme(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<ActivationBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;

    let change = themes::restore_previous(
        state.db().pool(),
        site.id,
        Some(site.organization_id),
        Some(current.user.id),
    )
    .await?;

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.theme.rolled_back")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(json!({
                "site_id": site.id,
                "site_key": site.key,
                "theme_key": change.theme_key,
                "previous_theme_key": change.previous_theme_key,
            })),
    )
    .await;

    // The rollback event carries the settings revision in the REQ's payload sketch, and slice 1
    // has no revisions — so it carries the theme key it came FROM instead, which is the fact
    // an operator restoring a site needs and the one the event name promises.
    emit_theme_event(
        &state,
        site.organization_id,
        site.id,
        "themes.theme.rolled_back",
        &change.theme_key,
        change.previous_theme_key.as_deref(),
    )
    .await;

    Ok(Json(ActivationBody {
        gallery: read_gallery(&state, &site).await?,
        theme_key: change.theme_key,
        previous_theme_key: change.previous_theme_key,
        restored: true,
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Read the gallery a write just changed.
async fn read_gallery(
    state: &AppState,
    site: &omnion_identity::Site,
) -> Result<GalleryView, ApiError> {
    Ok(themes::gallery_for_site(
        state.db().pool(),
        site.id,
        Some(site.organization_id),
    )
    .await?)
}

/// Announce a theme change on the bus.
///
/// A bus that cannot record the fact is a warning, not a failed activation: the theme is
/// already active and the site renders with it, so a marketing automation that misses one
/// activation must not also cost the operator their theme switch.
async fn emit_theme_event(
    state: &AppState,
    organization_id: Uuid,
    site_id: Uuid,
    name: &'static str,
    theme_key: &str,
    previous: Option<&str>,
) {
    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new(name)
            .organization(organization_id)
            .site(site_id)
            .payload(json!({
                "site_id": site_id,
                "theme_key": theme_key,
                "previous_theme_key": previous,
            })),
    )
    .await
    {
        tracing::warn!(site_id = %site_id, event = name, %error, "theme change was not recorded on the bus");
    }
}

/// Resolve a site through the caller's organization.
///
/// `pub(crate)` rather than private because the settings routes resolve the site the same way,
/// and a second copy of this function is how the two surfaces end up disagreeing about what
/// "in scope" means — the settings screen must refuse a cross-tenant site exactly where the
/// gallery refuses it.
pub(crate) async fn site_in_scope(
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
