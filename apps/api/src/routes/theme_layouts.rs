//! `/api/v1/sites/{site_id}/theme-layouts` and the package routes — REQ-062 slice 3.
//!
//! Five routes for the builder and three for the package, and the splits are the same two
//! questions slice 2 asked:
//!
//! * **A slot save is not a slot render.** `PUT /theme-layouts/{slot}` writes the site's own
//!   row; what a visitor draws is the *published* settings revision's theme plus whichever row
//!   is not a default. That is the same draft/published split the settings surface has, one
//!   level down, and collapsing it would make "Save draft" in the builder repaint the site.
//!
//! * **A validation report is not an install.** `POST /themes/validate` writes nothing and
//!   answers with every finding; `POST /themes/install` refuses while any of them is an error
//!   and then writes the row **inactive**. The install never touches `site_themes`, so an
//!   uploaded package is in the gallery with an `Uploaded` tag and nothing renders with it
//!   until an operator activates it — acceptance 13's first clause, enforced by the route and
//!   not by the screen that happens to call it.
//!
//! The builder's canvas is the same component as the page editor's (REQ-063), and this module
//! never re-implements a block rule: `save_slot` hands the payload to
//! `blocks::prepare_tree`, so a theme slot and a page revision cannot end up stored in
//! different shapes.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::theme_layouts::{self, LayoutsView, PackageReport};
use omnion_content::themes;
use omnion_events::NewEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

use super::themes::site_in_scope;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// `PUT /theme-layouts/{slot}` — the body is the block tree itself.
///
/// A bare `Value` and not a `{ blocks: … }` wrapper: the editor already holds a tree and posts
/// it, and a wrapper would be one more shape to keep in step with the canvas. A body that is
/// not a list is refused by `prepare_tree` with the platform's own message rather than by a
/// deserializer the client never sees.
pub type SaveSlotBody = Value;

/// `POST /themes/validate` and `POST /themes/install` — the body is the package.
///
/// The same shape for both, and that is deliberate: the upload screen holds the picked file's
/// parsed JSON, so "validate" is not a different request with a different body — it is the same
/// one that promises not to write.
pub type PackageBody = Value;

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sites/{site_id}/theme-layouts` — the builder's slot picker and canvas.
pub async fn read_layouts(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<LayoutsView>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let theme_key = themes::active_theme_key(state.db().pool(), site.id).await?;
    Ok(Json(
        theme_layouts::layouts_view(state.db().pool(), site.id, &theme_key).await?,
    ))
}

/// `GET /api/v1/sites/{site_id}/theme-layouts/{slot}` — one slot.
pub async fn read_slot(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, slot)): Path<(Uuid, String)>,
) -> Result<Json<Value>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    if !theme_layouts::is_slot(&slot) {
        return Err(ApiError::from(omnion_content::error::ContentError::ThemeUnknownSlot(
            slot,
        )));
    }
    let theme_key = themes::active_theme_key(state.db().pool(), site.id).await?;
    let row = theme_layouts::slot_layout(state.db().pool(), site.id, &theme_key, &slot).await?;
    Ok(Json(json!({
        "site_id": site.id,
        "theme_key": theme_key,
        "slot": slot,
        // An empty list rather than `null` for a theme that ships nothing in this slot: the
        // canvas mounts on an array and `null` is a screen that has to special-case "the theme
        // has no header" before it can show the empty state that says exactly that.
        "blocks": row.as_ref().map_or_else(|| json!([]), |row| row.blocks.clone()),
        "is_default": row.as_ref().is_none_or(|row| row.is_default),
        "updated_at": row.as_ref().map(|row| row.updated_at),
    })))
}

/// `PUT /api/v1/sites/{site_id}/theme-layouts/{slot}` — save a slot's blocks.
pub async fn save_slot(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, slot)): Path<(Uuid, String)>,
    Json(blocks): Json<SaveSlotBody>,
) -> Result<Json<Value>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let theme_key = themes::active_theme_key(state.db().pool(), site.id).await?;

    let (row, issues) = theme_layouts::save_slot(
        state.db().pool(),
        site.id,
        &theme_key,
        &slot,
        blocks,
        Some(current.user.id),
    )
    .await?;

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.layout.saved")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(json!({
                "site_id": site.id,
                "theme_key": theme_key,
                "slot": slot,
                "block_count": row.blocks.as_array().map_or(0, Vec::len),
            })),
    )
    .await;

    // The one bus event this surface emits. `Live` in the catalogue because it is written
    // inside the request, so a subscriber hears about the save from the save.
    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("themes.layout.saved")
            .organization(site.organization_id)
            .site(site.id)
            .payload(json!({ "site_id": site.id, "theme_key": theme_key, "slot": slot })),
    )
    .await
    {
        tracing::warn!(site_id = %site.id, %error, "theme slot save was not recorded on the bus");
    }

    Ok(Json(json!({
        "layout": row,
        // Warnings travel back with the save rather than being thrown away: an empty column in
        // a header is stored and rendered, and the author is the only person who can decide it
        // was deliberate.
        "issues": issues,
    })))
}

/// `POST /api/v1/sites/{site_id}/theme-layouts/{slot}/reset` — back to what the theme ships.
pub async fn reset_slot(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, slot)): Path<(Uuid, String)>,
) -> Result<Json<Value>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let theme_key = themes::active_theme_key(state.db().pool(), site.id).await?;
    let row = theme_layouts::reset_slot(state.db().pool(), site.id, &theme_key, &slot).await?;

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.layout.reset")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(json!({ "site_id": site.id, "theme_key": theme_key, "slot": slot })),
    )
    .await;

    Ok(Json(json!({ "layout": row })))
}

/// `GET /api/v1/sites/{site_id}/theme-package/export` — the site's look as a package.
pub async fn export_package(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<PackageReport>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    Ok(Json(
        theme_layouts::export_package(state.db().pool(), site.id).await?,
    ))
}

/// `POST /api/v1/themes/validate` — a dry run. Writes nothing.
pub async fn validate_package(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Json(package): Json<PackageBody>,
) -> Result<Json<PackageReport>, ApiError> {
    // `_current` still matters: the router's permission layer is what gates this, and an
    // unused binding is not a bypass. The route is registered under `themes.install`, so a
    // dry run costs the same permission as a real install — which is right, because a
    // validation that an editor cannot run is a validation they will guess at instead.
    let known = omnion_content::blocks::known_types();
    Ok(Json(theme_layouts::validate_package(&package, &known)))
}

/// `POST /api/v1/themes/install` — validate, refuse on any error, then install inactive.
pub async fn install_package(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(package): Json<PackageBody>,
) -> Result<Json<Value>, ApiError> {
    let known = omnion_content::blocks::known_types();
    let report = theme_layouts::validate_package(&package, &known);

    // The report goes back on the refusal, not just a count: the operator uploaded a file and
    // the answer they need is *which line*, and a 422 whose body says "2 errors" sends them to
    // the console.
    if !report.valid {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "theme_package_invalid",
            serde_json::to_string(&report).unwrap_or_else(|_| report.error_count.to_string()),
        ));
    }

    // The package is stored BEFORE the row is written, because the row cannot be written
    // without it: `themes_upload_storage_check` says an uploaded theme must point at a stored
    // package, and that is the right rule — a row claiming a theme whose bytes are nowhere
    // is a theme that cannot be reinstalled, exported or re-validated. The key is derived
    // from the checksum, so uploading the same package twice lands on the same object rather
    // than filling the library with copies of the same theme.
    let checksum = theme_layouts::package_checksum_of(&package);
    let storage_key = format!("themes/uploads/{}.json", checksum.replace(':', "-"));
    let bytes = serde_json::to_vec(&package).unwrap_or_default();
    state
        .storage()
        .put(&storage_key, &bytes, "application/json")
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("the theme package could not be stored: {error}"),
            )
        })?;

    let outcome = theme_layouts::install_package(
        state.db().pool(),
        &report,
        &package,
        &storage_key,
        current.user.organization_id,
        Some(current.user.id),
    )
    .await?;

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.package.installed")
            .metadata(json!({
                "theme_key": outcome.theme_key,
                "version": outcome.version,
                "source": "uploaded",
                "storage_key": storage_key,
            })),
    )
    .await;

    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("themes.package.installed").payload(json!({
            "theme_key": outcome.theme_key,
            "version": outcome.version,
            "source": "uploaded",
        })),
    )
    .await
    {
        tracing::warn!(%error, "theme package install was not recorded on the bus");
    }

    // `active: false` in the payload is the acceptance criterion, not a courtesy: the screen
    // has to be able to say "installed, not active — activate it when you are ready" from the
    // server's answer rather than from what it knows it did not click.
    Ok(Json(json!({ "install": outcome, "report": report })))
}

/// `DELETE /api/v1/themes/{key}` — remove an uploaded theme.
///
/// Three refusals live in the store, in this order: a bundled theme, a theme a site still
/// renders with, and a theme whose layouts somebody still holds. The route adds nothing, which
/// is the point — a rule enforced in a handler is a rule the next caller does not get.
pub async fn remove_theme(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // The stored package is read BEFORE the row goes away — the row is the only thing that
    // knows the key, and the storage layer has no way to list "packages for theme X".
    let stored_key: Option<String> = sqlx::query_scalar(
        "select storage_key from themes where key = $1 and removed_at is null",
    )
    .bind(&key)
    .fetch_optional(state.db().pool())
    .await
    .unwrap_or(None);

    theme_layouts::remove_theme(state.db().pool(), &key, current.user.organization_id).await?;

    // Removing the row without removing the object leaves an orphan nothing references and
    // nothing reaps. It is best-effort on purpose: the removal the operator asked for has
    // already happened, and failing the request because a file could not be unlinked would
    // report a state that is worse than the one being fixed.
    if let Some(storage_key) = stored_key {
        if let Err(error) = state.storage().delete(&storage_key).await {
            tracing::warn!(theme_key = %key, %error, "the stored theme package was orphaned");
        }
    }

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.package.removed")
            .organization(current.user.organization_id)
            .metadata(json!({ "theme_key": key })),
    )
    .await;

    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("themes.package.removed")
            .organization(current.user.organization_id)
            .payload(json!({ "theme_key": key })),
    )
    .await
    {
        tracing::warn!(%error, "theme removal was not recorded on the bus");
    }

    Ok(Json(json!({ "removed": key })))
}
