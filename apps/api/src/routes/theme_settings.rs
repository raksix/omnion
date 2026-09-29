//! `/api/v1/sites/{site_id}/theme-settings` — REQ-062 slice 2, the customize screen.
//!
//! Five routes, and the split is the design again (see `themes.rs` for slice 1's reasoning):
//!
//! * **`GET` / `PUT /theme-settings`** is one resource, not two. A save writes a draft; it
//!   does not publish, and the response says which draft and which revision is live so the
//!   panel's "unsaved"/"published" line is the server's fact rather than a local flag.
//! * **`POST /publish`** is separate, because publishing is the only write here that a
//!   signed-out visitor can observe — and because it takes an `acknowledgeContrast` flag that
//!   a save has no reason to carry.
//! * **`GET /revisions` and `GET /revisions/{no}`** are two because the list is a summary and
//!   the detail is a payload, and the history screen loads the list on every visit but the
//!   detail only when a row is opened.
//! * **`POST /revisions/{no}/restore`** writes a new revision rather than moving a pointer,
//!   which the store explains in detail. Here it matters for one more reason: the route's
//!   response is the *whole view* afterwards, so the panel cannot show a restored revision and
//!   a history that disagrees about it.
//!
//! **The contrast guard is on publish, not on save.** A save that refused low-contrast tokens
//! would make the warning badge a wall: the operator could not even stage a work-in-progress
//! palette to compare it against the theme. Publishing is the moment the site changes, so
//! that is the moment the acknowledgement is required — and it is required, not advisory,
//! because a screen that merely nags is a screen people learn to dismiss.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_content::theme_settings::{
    self, SettingsChange, SettingsInput, SettingsView, contrast_report, merge_over,
};
use omnion_content::themes;
use omnion_events::NewEvent;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

use super::themes::site_in_scope;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// `PUT /theme-settings` — the body is the settings themselves.
///
/// `SettingsInput` is deserialized here rather than re-declared, because a second body struct
/// with the same fields and a slightly different optionality is how the store's validation
/// ends up validating a shape the route never sends.
pub type SaveBody = SettingsInput;

/// `POST /theme-settings/publish` — the body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishBody {
    /// The caller has seen the contrast findings and is publishing anyway.
    ///
    /// `#[serde(default)]` is deliberate: a client that has never heard of the flag still
    /// gets a clean 422 naming the reason, rather than "missing field". The `alias` is the
    /// other half — the wire name is camelCase like the rest of this API, and a body field
    /// that silently reads `false` is a guard the caller can never satisfy.
    #[serde(default)]
    pub acknowledge_contrast: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/sites/{site_id}/theme-settings` — everything the customize screen loads.
pub async fn read_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SettingsView>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    Ok(Json(settings_view(&state, site.id).await?))
}

/// `PUT /api/v1/sites/{site_id}/theme-settings` — save a draft.
pub async fn save_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    Json(body): Json<SaveBody>,
) -> Result<Json<SettingsView>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;

    let revision = theme_settings::save_draft(
        state.db().pool(),
        site.id,
        &body,
        Some(current.user.id),
    )
    .await?;

    // A save is an audit event but NOT a bus event. The bus is what a CDN and a static-site
    // integration subscribe to in order to invalidate a cache — and a draft nobody can see
    // must not invalidate anything, or every keystroke-save becomes a cold cache for real
    // visitors. The publish below is the one that announces.
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.settings.draft_saved")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(theme_settings::describe_settings(&revision)),
    )
    .await;

    Ok(Json(
        settings_view(&state, site.id).await?,
    ))
}

/// `POST /api/v1/sites/{site_id}/theme-settings/publish` — make the draft live.
pub async fn publish_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    body: Option<Json<PublishBody>>,
) -> Result<Json<SettingsView>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let acknowledge = body.map_or(false, |Json(body)| body.acknowledge_contrast);

    // The findings are computed from the *draft that is about to go live*, read back through
    // the store rather than taken from the request. A caller that sent no body at all is the
    // normal case for this route, and a guard that only ran when a body was present would
    // mean the client decides whether the check happens.
    let draft = match theme_settings::list_revisions(state.db().pool(), site.id)
        .await?
        .into_iter()
        .find(|row| row.is_draft)
    {
        Some(row) => theme_settings::revision_by_id(state.db().pool(), row.id).await?,
        None => None,
    };

    let Some(draft) = draft else {
        return Err(ApiError::from(omnion_content::error::ContentError::ThemeSettingsNothingToPublish));
    };

    let defaults = default_tokens_for(&state, &draft.theme_key).await;
    let findings = contrast_report(&merge_over(defaults, draft.tokens.clone()));
    if !findings.is_empty() && !acknowledge {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "theme_settings_contrast_required",
            findings
                .iter()
                .map(|finding| finding.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let change = theme_settings::publish(state.db().pool(), site.id, Some(current.user.id)).await?;
    let SettingsChange::Published {
        revision_no,
        previous_revision_no,
    } = change
    else {
        // Unreachable while this is the only caller, and a `let … else` beats a panic in a
        // request path: if a future caller gets a different variant, the answer is a 500 the
        // log explains rather than a process that dies mid-response.
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "publishing theme settings returned an unexpected change",
        ));
    };

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.settings.published")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(json!({
                "site_id": site.id,
                "site_key": site.key,
                "revision_no": revision_no,
                "previous_revision_no": previous_revision_no,
                "contrast_findings": findings.len(),
                "contrast_acknowledged": acknowledge,
            })),
    )
    .await;

    // The one bus event this surface emits. `Live` in the catalogue because it is written
    // inside the request, so a subscriber hears about the publish from the publish.
    if let Err(error) = omnion_events::bus::emit(
        state.db().pool(),
        NewEvent::new("themes.settings.published")
            .organization(site.organization_id)
            .site(site.id)
            .payload(json!({
                "site_id": site.id,
                "theme_key": draft.theme_key,
                "revision_no": revision_no,
            })),
    )
    .await
    {
        // A bus that cannot record the fact is a warning, not a failed publish: the settings
        // are already live and the site is already rendering with them.
        tracing::warn!(site_id = %site.id, %error, "theme settings publish was not recorded on the bus");
    }

    Ok(Json(
        settings_view(&state, site.id).await?,
    ))
}

/// `GET /api/v1/sites/{site_id}/theme-settings/revisions` — the history.
pub async fn list_revisions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    Ok(Json(json!({
        "site_id": site.id,
        "revisions": theme_settings::list_revisions(state.db().pool(), site.id).await?,
    })))
}

/// `GET /api/v1/sites/{site_id}/theme-settings/revisions/{no}` — one revision.
pub async fn read_revision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, revision_no)): Path<(Uuid, i32)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let revision = theme_settings::revision(state.db().pool(), site.id, revision_no).await?;

    // The diff is against the revision that was live *before* this one, resolved by number
    // rather than by "the next lower row" — a restore writes a new revision whose content
    // comes from far away, so the row next to it in the table is not what it was derived from.
    let previous = if revision.revision_no > 1 {
        theme_settings::revision(state.db().pool(), site.id, revision.revision_no - 1).await.ok()
    } else {
        None
    };

    Ok(Json(json!({
        "revision": revision,
        // An empty list, never `null`. The history screen maps over this to render per-field
        // rows, and `null` there is a panel that has to special-case the FIRST revision — the
        // one case where "nothing changed because nothing came before" is the whole answer.
        "diff": previous.map(|before| diff(&before, &revision)).unwrap_or_default(),
    })))
}

/// `POST /api/v1/sites/{site_id}/theme-settings/revisions/{no}/restore`.
pub async fn restore_revision(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, revision_no)): Path<(Uuid, i32)>,
) -> Result<Json<SettingsView>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;

    let change =
        theme_settings::restore_revision(state.db().pool(), site.id, revision_no, Some(current.user.id))
            .await?;
    let SettingsChange::Restored { revision } = change else {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "restoring theme settings returned an unexpected change",
        ));
    };

    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "themes.settings.restored")
            .organization(site.organization_id)
            .target("site", site.id)
            .metadata(theme_settings::describe_settings(&revision)),
    )
    .await;

    Ok(Json(
        settings_view(&state, site.id).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The whole payload for one site, with the theme's own defaults folded in.
async fn settings_view(state: &AppState, site_id: Uuid) -> Result<SettingsView, ApiError> {
    let active = themes::active_theme_key(state.db().pool(), site_id).await?;
    let defaults = default_tokens_for(state, &active).await;
    Ok(theme_settings::settings_view(state.db().pool(), site_id, &active, defaults).await?)
}

/// A theme's declared default tokens, or `{}` for a key that is not installed.
///
/// An unknown key is an empty map rather than an error, because the whole slice-1 contract is
/// that a site pointing at a theme the gallery cannot show still renders. The customize screen
/// then shows empty token inputs with the "no theme installed" notice instead of a 404 on a
/// page the operator needs in order to fix the site.
async fn default_tokens_for(state: &AppState, theme_key: &str) -> serde_json::Value {
    match themes::find_theme(state.db().pool(), theme_key).await {
        Ok(Some(theme)) => theme
            .manifest
            .get("tokens")
            .cloned()
            .unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}

/// The per-field diff the history screen renders.
///
/// A list of `{field, from, to}` rather than a rendered string: the panel needs the raw values
/// to show "was #1a1a1a, now #bbbbbb" with a colour swatch on each side, and a stringified
/// diff would make it re-parse what the server already had as structured values.
fn diff(
    before: &theme_settings::SettingsRevision,
    after: &theme_settings::SettingsRevision,
) -> Vec<serde_json::Value> {
    // Every field is compared as a `Value` rather than mixing `&String` and `&Value` in one
    // array: the two string fields are the only scalars, and a heterogeneous tuple array would
    // need a cast at every comparison and still not type-check the mixed entries.
    let mut changes = Vec::new();
    for (name, was, now) in [
        // The diff's field NAMES are the wire names, not the column names: the panel renders
        // them straight into labels, and `default_mode` next to `typography` in a history
        // screen is a column name leaking into the product.
        ("themeKey", json!(before.theme_key), json!(after.theme_key)),
        ("defaultMode", json!(before.default_mode), json!(after.default_mode)),
        ("tokens", before.tokens.clone(), after.tokens.clone()),
        ("typography", before.typography.clone(), after.typography.clone()),
        ("layout", before.layout.clone(), after.layout.clone()),
        ("branding", before.branding.clone(), after.branding.clone()),
        (
            "headerFooter",
            before.header_footer.clone(),
            after.header_footer.clone(),
        ),
    ] {
        if was != now {
            changes.push(json!({ "field": name, "from": was, "to": now }));
        }
    }
    changes
}
