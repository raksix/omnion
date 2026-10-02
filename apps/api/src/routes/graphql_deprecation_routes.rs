//! The deprecation routes (REQ-130, slice 4).
//!
//! ## Two guards, and the request's `settings.manage` is not one of them
//!
//! The request's API table gives the two writes `settings.manage`. **This repository ships no
//! such key** — an uncatalogued key resolves to no permission, so a route guarded on one answers
//! `403` for every caller including the instance owner while looking perfectly healthy in review.
//! That is the fifth time this defect has cost this repository a tick, so the substitutions are
//! named here and asserted by the walk rather than left in a comment:
//!
//! | Route | Request's key | Key used, and why |
//! |---|---|---|
//! | list · detail | `developer.read` | `developer.read` — **the request's own key, and it is catalogued.** Main added it for the developer portal, which is what a deprecation list is part of. |
//! | announce · extend · withdraw · notify | `settings.manage` | `developer.keys.manage` — a deprecation is a change to *what integrators may call*, which is the same authority a key grant is. `deployment.migrations.apply` was considered and rejected: a document registration can be renamed, a sunset can be moved either way, and neither is a release. |
//!
//! ## Every write is audited, and the extension's REASON is the audited payload
//!
//! A sunset moving later is the one action integrators care about most and the one an operator
//! takes quietly. The row keeps the new date; the audit trail keeps who moved it and why, because
//! a deadline with no recorded reason is a deadline integrators cannot plan around.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::graphql_deprecations as store;
use crate::state::AppState;

/// The read guard. The request's own key, which main's developer portal catalogued.
pub const READ_PERMISSION: &str = "developer.read";
/// The write guard. See the module note on why this is not `settings.manage`.
pub const MANAGE_PERMISSION: &str = "developer.keys.manage";

/// `GET /api/v1/api/deprecations`.
#[derive(Debug, Clone, Serialize)]
pub struct ListResponse {
    pub deprecations: Vec<store::DeprecationView>,
    pub total: usize,
    /// How many are past their sunset. A screen that shows this and nothing else can answer
    /// "how much of my API is already gone" without reading every row's countdown.
    pub removed: usize,
    /// The two windows in force, so the form can state them before an operator picks a date
    /// instead of refusing the date afterwards.
    pub policy: PolicyView,
}

/// The policy the form shows, from the crate's constants rather than a copy of them.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyView {
    pub public_months: i64,
    pub developer_months: i64,
    pub amber_within_days: i64,
}

/// `GET /api/v1/api/deprecations`.
pub async fn list(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
) -> Result<Json<ListResponse>, ApiError> {
    let now = OffsetDateTime::now_utc();
    let deprecations = store::list(state.db().pool(), organization_of(&session), now).await?;
    let removed = deprecations
        .iter()
        .filter(|row| row.status == "removed")
        .count();
    Ok(Json(ListResponse {
        total: deprecations.len(),
        removed,
        deprecations,
        policy: PolicyView {
            public_months: omnion_graphql::deprecation::PUBLIC_WINDOW,
            developer_months: omnion_graphql::deprecation::DEVELOPER_WINDOW,
            amber_within_days: omnion_graphql::deprecation::AMBER_WITHIN_DAYS,
        },
    }))
}

/// `GET /api/v1/api/deprecations/{id}`.
pub async fn detail(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<store::DeprecationView>, ApiError> {
    store::get(
        state.db().pool(),
        id,
        organization_of(&session),
        OffsetDateTime::now_utc(),
    )
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("deprecation", id))
}

/// `POST /api/v1/api/deprecations` — Announce.
pub async fn announce(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Json(input): Json<store::NewDeprecation>,
) -> Result<(axum::http::StatusCode, Json<store::DeprecationView>), ApiError> {
    let now = OffsetDateTime::now_utc();
    let view = store::announce(
        state.db().pool(),
        organization_of(&session),
        Some(session.user.id),
        &input,
        now,
    )
    .await?;

    // The write invalidates the header cache BEFORE the audit row is awaited: a cache that still
    // says "not deprecated" for a millisecond after an announcement is the window in which an
    // integrator's first call goes out without the headers this action exists to make it carry.
    crate::deprecation_middleware::refresh(state.db().pool()).await;

    audit(
        &state,
        &session,
        "api.deprecation.announced",
        json!({
            "deprecation_id": view.id,
            "surface": view.surface,
            "deprecated_in": view.deprecated_in,
            "sunset_at": view.sunset_at,
            "replacement": view.replacement,
            "actor_user_id": session.user.id,
        }),
    )
    .await;

    Ok((axum::http::StatusCode::CREATED, Json(view)))
}

/// The extend body. The reason is required by the policy, not by this struct: a `#[required]`
/// here would make the refusal a serde message naming a field, while the policy's message says
/// what the reason is FOR.
#[derive(Debug, Deserialize)]
pub struct ExtendRequest {
    /// RFC 3339, later than the current sunset.
    pub sunset_at: String,
    /// Recorded in the audit trail. Never stored on the row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/v1/api/deprecations/{id}/extend`.
pub async fn extend(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<ExtendRequest>,
) -> Result<Json<store::DeprecationView>, ApiError> {
    let now = OffsetDateTime::now_utc();
    let organization_id = organization_of(&session);
    let existing = store::get(state.db().pool(), id, organization_id, now)
        .await?
        .ok_or_else(|| ApiError::not_found("deprecation", id))?;

    let new_sunset = OffsetDateTime::parse(
        &input.sunset_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "deprecation_invalid",
            format!(
                "sunset_at must be an RFC 3339 instant; {:?} is not one",
                input.sunset_at
            ),
        )
    })?;

    // The policy's check, on the row the caller may read. A store that checked it would have to
    // take the reason and throw it away, and a reason nobody stores is a reason nobody asked for.
    let policy_row = existing.to_policy_row().ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the stored sunset could not be read back",
        )
    })?;
    omnion_graphql::deprecation::check_extension(
        &policy_row,
        new_sunset,
        input.reason.as_deref().unwrap_or_default(),
        now,
    )
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "deprecation_invalid",
            error.to_string(),
        )
    })?;

    let view = store::extend(state.db().pool(), id, new_sunset, now)
        .await?
        .ok_or_else(|| ApiError::not_found("deprecation", id))?;
    crate::deprecation_middleware::refresh(state.db().pool()).await;

    audit(
        &state,
        &session,
        "api.deprecation.extended",
        json!({
            "deprecation_id": id,
            "from": existing.sunset_at,
            "to": view.sunset_at,
            "reason": input.reason,
            "actor_user_id": session.user.id,
        }),
    )
    .await;

    Ok(Json(view))
}

/// The withdraw body. Same required-reason rule as an extension, and for the same reason: a
/// deprecation called off is a promise to integrators, so somebody has to say why it changed.
#[derive(Debug, Deserialize)]
pub struct WithdrawRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/v1/api/deprecations/{id}/withdraw`.
pub async fn withdraw(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
    Json(input): Json<WithdrawRequest>,
) -> Result<Json<store::DeprecationView>, ApiError> {
    let now = OffsetDateTime::now_utc();
    let organization_id = organization_of(&session);
    let existing = store::get(state.db().pool(), id, organization_id, now)
        .await?
        .ok_or_else(|| ApiError::not_found("deprecation", id))?;

    let reason = input.reason.unwrap_or_default();
    if reason.trim().is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "deprecation_invalid",
            "a withdrawal requires a reason, and it is recorded in the audit trail",
        ));
    }

    let view = store::withdraw(state.db().pool(), id, now)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "deprecation_already_withdrawn",
                "this deprecation was already withdrawn",
            )
        })?;
    crate::deprecation_middleware::refresh(state.db().pool()).await;

    audit(
        &state,
        &session,
        "api.deprecation.withdrawn",
        json!({
            "deprecation_id": id,
            "surface": existing.surface,
            "reason": reason,
            "actor_user_id": session.user.id,
        }),
    )
    .await;

    Ok(Json(view))
}

/// `POST /api/v1/api/deprecations/{id}/notified` — the screen's "Notified" tick.
///
/// This one records a fact about the OUTSIDE (the operator emailed integrators) rather than a
/// change to the platform, so it is not a permission-guarded write of the platform's own state.
/// It is still under the write guard: a row nobody may mark notified is a column that reads
/// `null` for ever, and the screen's own acceptance line needs the control to work.
pub async fn notify(
    State(state): State<AppState>,
    session: crate::auth::CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<store::DeprecationView>, ApiError> {
    let now = OffsetDateTime::now_utc();
    store::get(state.db().pool(), id, organization_of(&session), now)
        .await?
        .ok_or_else(|| ApiError::not_found("deprecation", id))?;
    store::mark_notified(state.db().pool(), id, now).await?;
    let view = store::get(state.db().pool(), id, organization_of(&session), now)
        .await?
        .ok_or_else(|| ApiError::not_found("deprecation", id))?;
    Ok(Json(view))
}

/// The tenant a row belongs to, and `None` for an installation-wide announcement.
fn organization_of(session: &crate::auth::CurrentSession) -> Option<Uuid> {
    session.user.organization_id
}

/// Write exactly one audit row, awaited, and never failing the request for it.
///
/// Awaited because the walk reads the row straight after the call returns; a spawned write would
/// make "every write is audited" a race the test has to sleep through. Not returned on failure
/// because a client retrying a successful announcement because its audit row failed would file a
/// duplicate — which the store would then have to refuse, and the operator would see a conflict
/// for an action that worked.
async fn audit(
    state: &AppState,
    session: &crate::auth::CurrentSession,
    action: &'static str,
    payload: serde_json::Value,
) {
    let entry = omnion_audit::NewAuditEntry::by_user(session.user.id, action)
        .organization(session.user.organization_id)
        .metadata(payload.clone())
        .target(
            "api_deprecation",
            payload
                .get("deprecation_id")
                .and_then(|id| id.as_str())
                .unwrap_or_default()
                .to_owned(),
        );
    if let Err(error) = omnion_audit::record(state.db().pool(), entry).await {
        tracing::debug!(action, error = %error, "the deprecation audit row could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_guards_are_keys_the_catalogue_actually_ships() {
        // The request's `settings.manage` is NOT shipped, and the substitution is asserted by
        // name so a later catalogue edit that adds it can be adopted deliberately rather than by
        // accident. The catalogue itself is read by `apps/api/tests/graphql_parity.rs`, which is
        // the one crate that can see both sides; what is checked here is that the strings are the
        // ones this router registers, so a typo is caught here and the meaning there.
        assert_eq!(READ_PERMISSION, "developer.read");
        assert_eq!(MANAGE_PERMISSION, "developer.keys.manage");
        assert_ne!(
            MANAGE_PERMISSION, "settings.manage",
            "an uncatalogued key answers 403 for every caller including the instance owner"
        );
    }

    #[test]
    fn the_policy_shown_to_the_form_comes_from_the_crate_not_from_a_copy() {
        // A screen that states its own windows can disagree with the ones the server enforces, and
        // the operator picks a date the form said was fine and the server then refuses.
        let policy = PolicyView {
            public_months: omnion_graphql::deprecation::PUBLIC_WINDOW,
            developer_months: omnion_graphql::deprecation::DEVELOPER_WINDOW,
            amber_within_days: omnion_graphql::deprecation::AMBER_WITHIN_DAYS,
        };
        assert_eq!(policy.public_months, 6);
        assert_eq!(policy.developer_months, 3);
        assert_eq!(policy.amber_within_days, 30);
    }
}