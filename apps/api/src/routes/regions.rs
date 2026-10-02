//! `/api/v1/regions` — the edge region registry (docs/requests/REQ-035, slice 1).
//!
//! Four reads and one write, and the split between them is the security property rather than
//! a convenience: **reading which regions exist is an operator's question and renaming one
//! is a control-plane action**, so they are different keys (`platform.regions.read` and
//! `platform.regions.manage`). A single key would hand every auditor the ability to move the
//! routing default.
//!
//! What this file deliberately does NOT do, and why each absence is a decision:
//!
//! * **No `POST /regions`.** The REQ's scope section says it outright: "the registry describes
//!   regions the deployment already provides; the admin surface never creates infrastructure."
//!   A `create` handler would be one more thing to keep correct, and its failure mode is a
//!   region code in a hostname that resolves nowhere.
//! * **No `DELETE /regions/{code}`.** A region's history is referenced by `residency_migrations`
//!   and by the health table's foreign key. Deleting one is a retention decision, and
//!   `is_active = false` is the same operation without the data loss.
//! * **The write is a `PATCH`, and an empty patch is refused.** A panel that submits an
//!   untouched form should be told nothing changed rather than answered `200` and written to
//!   the audit log as an edit.
//!
//! The health and latency reads are one round trip each rather than one per region: the
//! matrix is `regions × services`, and seven separate queries for seven regions is the shape
//! of a dashboard that is fast on a two-region deployment and times out on a seven-region one.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_regions::store::{self, RegionEdit};
use omnion_regions::{HealthMatrix, LatencyMatrix, RegionOverview, RegionStatus};
use serde::Deserialize;
use serde_json::json;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The health policy the reads answer with.
///
/// One constant rather than three, because the freshness window, the latency ceiling and the
/// de-bounce threshold are one *policy*: a panel and a checker that disagree about which
/// checks count draw different regions and send an operator to the wrong one. Slice 3's
/// routing document will move the threshold and ceiling into `routing_policy`; until then the
/// defaults the REQ's data model names are the answer, and there is exactly one copy of them.
const POLICY: omnion_regions::HealthPolicy = omnion_regions::HealthPolicy {
    fresh_window: time::Duration::seconds(300),
    latency_ceiling_ms: 2000,
    failure_threshold: 3,
};

/// Body of `PATCH /api/v1/regions/{code}`.
///
/// Every field is `Option`, and `Option<Option<_>>` for the two endpoints so "leave the
/// admin host alone" and "clear it" stay different instructions. Collapsing them would make
/// a region that legitimately has no admin host impossible to PATCH at all.
#[derive(Debug, Default, Deserialize)]
pub struct RegionPatch {
    /// New operator-facing name, 2–80 characters.
    pub display_name: Option<String>,
    /// New operator-set status. `maintenance` is the one an operator sets; the others are
    /// what the checker concludes and a human may set them deliberately during an incident.
    pub status: Option<String>,
    /// New admin host, `null` to clear.
    pub admin_endpoint: Option<Option<String>>,
    /// New web host, `null` to clear.
    pub web_endpoint: Option<Option<String>>,
    /// Share of traffic, 0–100.
    pub traffic_share: Option<f64>,
    /// Whether routing may choose this region.
    pub is_active: Option<bool>,
    /// Whether this region is the routing fallback.
    pub is_default: Option<bool>,
}

/// `GET /api/v1/regions` — the registry, the health matrix and the latency matrix.
///
/// One request, because the list screen renders all three and a panel that issues three
/// fetches on mount shows three spinners and can disagree with itself: the region table from
/// response one and the health badges from response two are two moments in time, and on a
/// deployment that is actively changing a row can be `healthy` in the table and `down` in the
/// badge row for as long as the slower request takes.
pub async fn list(
    State(state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<RegionOverview>, ApiError> {
    let overview = store::overview(state.db().pool(), &POLICY).await?;
    Ok(Json(overview))
}

/// `GET /api/v1/regions/{code}` — one region with its services and endpoints.
///
/// `404` naming the code when it is not registered, so the panel's not-found state can say
/// which code was looked for instead of rendering an empty detail screen that looks like a
/// load failure.
pub async fn get_one(
    State(state): State<AppState>,
    _current: CurrentSession,
    Path(code): Path<String>,
) -> Result<Json<RegionDetail>, ApiError> {
    let overview = store::overview(state.db().pool(), &POLICY).await?;
    let region = overview
        .regions
        .into_iter()
        .find(|r| r.region.code == code)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "region_not_found",
                format!("No region is registered with the code {code:?}."),
            )
        })?;
    Ok(Json(RegionDetail {
        region,
        multi_region_active: overview.multi_region_active,
        inactive_reason: overview.inactive_reason,
        latency: overview.latency,
    }))
}

/// `GET /api/v1/regions/health` — the matrix, on its own.
///
/// Registered separately from the list because the health checker's own runner polls it, and
/// a route that returns the whole overview to a machine caller is a route that changes shape
/// when a card is added to the panel.
pub async fn health(
    State(state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<HealthMatrix>, ApiError> {
    let regions = store::list_regions(state.db().pool()).await?;
    let matrix = store::health_matrix(state.db().pool(), &regions, &POLICY).await?;
    Ok(Json(matrix))
}

/// `GET /api/v1/regions/latency-matrix` — region-to-region p95, on its own.
pub async fn latency(
    State(state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<LatencyMatrix>, ApiError> {
    let regions = store::list_regions(state.db().pool()).await?;
    let matrix = store::latency_matrix(state.db().pool(), &regions, &POLICY).await?;
    Ok(Json(matrix))
}

/// `PATCH /api/v1/regions/{code}` — rename, set a status, set the default.
///
/// Two rules in this handler that are not obvious from its shape:
///
/// * **The audit entry carries the before/after for every field the patch changed.** The REQ
///   asks for "region status changes and policy updates appear in the audit log with actor
///   and diff", and a diff means the *previous* value. Reading the row first is one extra
///   query and the only way the audit log can answer "what was it before".
/// * **A status change to anything other than `maintenance` is refused.** The stored status
///   is what the checker concludes; letting an operator set `healthy` by hand is how a
///   region whose database is down gets painted green with a click. `degraded` and `down`
///   are allowed (an operator marking an incident by hand is legitimate and conservative),
///   but `healthy` may only be written by the checker.
pub async fn patch(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(code): Path<String>,
    Json(body): Json<RegionPatch>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Parse the status BEFORE opening anything, so a misspelled status is a 400 that names
    // the field rather than a 404 from a code that was never looked up.
    let status = body
        .status
        .as_deref()
        .map(|raw| {
            RegionStatus::parse(raw).map_err(|_| {
                ApiError::bad_request(
                    "invalid_status",
                    format!(
                        "{raw:?} is not a region status. Use healthy, degraded, down or maintenance."
                    ),
                )
                .with_details(json!({ "field": "status" }))
            })
        })
        .transpose()?;

    if status == Some(RegionStatus::Healthy) {
        return Err(ApiError::forbidden(
            "status_not_settable",
            "A region is marked healthy by the health checker, not by hand. Mark it degraded \
             or down to record an incident, and the checker will clear it when the services \
             answer again.",
        )
        .with_details(json!({ "field": "status", "value": "healthy" })));
    }

    let edit = RegionEdit {
        display_name: body.display_name,
        status,
        admin_endpoint: body.admin_endpoint,
        web_endpoint: body.web_endpoint,
        traffic_share: body.traffic_share,
        is_active: body.is_active,
        is_default: body.is_default,
    };

    if edit.is_empty() {
        return Err(ApiError::bad_request(
            "empty_patch",
            "This request would change nothing. Send at least one field.",
        ));
    }

    let pool = state.db().pool();
    let before = store::find_region(pool, &code)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "region_not_found",
                format!("No region is registered with the code {code:?}."),
            )
        })?;

    let after = store::update_region(pool, &code, &edit).await?;

    // The diff is built by comparing the two rows field by field rather than by recording
    // the request body. The body is what the caller *asked for*; the diff is what happened,
    // and the two differ whenever a field was clamped, defaulted or ignored — which is
    // exactly the case an auditor is looking for.
    // Every triple is `(field, before, after)` with all three as `Option<String>`. The
    // uniform type is the point: an array literal takes its element type from its FIRST
    // entry, so a list that begins with `display_name: String` and then carries
    // `admin_endpoint: Option<String>` fails to compile — and the version that "works" is
    // the one that unwraps the optionals into empty strings, which makes a *cleared* host
    // and a *never-set* host look identical in the audit diff.
    let changed: Vec<serde_json::Value> = [
        ("display_name", Some(before.display_name.clone()), Some(after.display_name.clone())),
        ("status", Some(before.status.clone()), Some(after.status.clone())),
        ("admin_endpoint", before.admin_endpoint.clone(), after.admin_endpoint.clone()),
        ("web_endpoint", before.web_endpoint.clone(), after.web_endpoint.clone()),
        ("traffic_share", before.traffic_share.clone(), after.traffic_share.clone()),
        ("is_active", Some(before.is_active.to_string()), Some(after.is_active.to_string())),
        ("is_default", Some(before.is_default.to_string()), Some(after.is_default.to_string())),
    ]
    .into_iter()
    .filter(|(_, from, to)| from != to)
    .map(|(field, from, to)| json!({ "field": field, "from": from, "to": to }))
    .collect();

    let action = if changed.iter().any(|c| {
        c.get("field").and_then(|f| f.as_str()) == Some("is_default")
    }) {
        "region.default.changed"
    } else {
        "region.updated"
    };

    // A region is global: the entry carries the caller's tenant for the *feed they can see*
    // and the region code as the target, because "who moved the routing default" is a
    // platform question and a tenant-scoped entry would hide the answer from the platform
    // feed an operator actually reads.
    let mut entry = NewAuditEntry::by_user(current.user.id, action)
        .target("region", &code)
        .metadata(json!({ "changes": changed }));
    if let Some(organization_id) = current.user.organization_id {
        entry = entry.organization(organization_id);
    }
    omnion_audit::record(state.db().pool(), entry).await?;

    Ok(Json(json!({ "region": after })))
}

/// The detail route's body: the region plus the two figures only the detail screen shows.
#[derive(Debug, serde::Serialize)]
pub struct RegionDetail {
    /// The row, its derived status and its seven service checks.
    pub region: omnion_regions::RegionView,
    /// Whether multi-region routing is active on this deployment.
    pub multi_region_active: bool,
    /// Why it is not, when it is not.
    pub inactive_reason: Option<String>,
    /// The latency matrix, for the region's own row and column.
    pub latency: LatencyMatrix,
}

/// Map a crate error onto the API surface.
///
/// Client refusals are `400` with the field they belong to, because a form that only learns
/// "invalid" cannot put the message under the right input. Everything else is a `503`: a
/// region read that cannot reach the database is a dependency problem the operator retries,
/// not a bug in the request.
impl From<omnion_regions::RegionError> for ApiError {
    fn from(error: omnion_regions::RegionError) -> Self {
        use omnion_regions::RegionError as E;
        let message = error.to_string();
        match error {
            E::InvalidCode { .. } => {
                ApiError::bad_request("invalid_region_code", message)
                    .with_details(json!({ "field": "code" }))
            }
            E::InvalidDisplayName { .. } => {
                ApiError::bad_request("invalid_display_name", message)
                    .with_details(json!({ "field": "display_name" }))
            }
            E::UnknownRegion(code) => {
                ApiError::new(StatusCode::NOT_FOUND, "region_not_found", message)
                    .with_details(json!({ "code": code }))
            }
            E::TrafficShareOutOfRange => {
                ApiError::bad_request("invalid_traffic_share", message)
                    .with_details(json!({ "field": "traffic_share" }))
            }
            E::MultiRegionInactive => {
                ApiError::forbidden("multi_region_inactive", message)
            }
            // A `409` and not a `400`: the request was well formed and what refuses it is
            // the *state* of the registry — the last default cannot be demoted. A `400`
            // would send the operator to fix their request, which is not the problem.
            E::DefaultAlreadyTaken => {
                ApiError::new(StatusCode::CONFLICT, "default_already_taken", message)
                    .with_details(json!({ "field": "is_default" }))
            }
            E::InvalidCountryCode(country) => {
                ApiError::bad_request("invalid_country_code", message)
                    .with_details(json!({ "field": "country", "value": country }))
            }
            E::UnknownService(_)
            | E::UnknownServiceStatus(_)
            | E::UnknownRegionStatus(_) => {
                ApiError::bad_request("invalid_region_value", message)
            }
            // Not behind a `cfg`: `apps/api` enables the `store` feature, and this impl
            // lives in `apps/api`, so a `cfg` here would leave the arm out of the match on
            // the only build that has the variant — which is a non-exhaustive-pattern error
            // rather than anything subtler.
            E::Database(_) => ApiError::from_core(omnion_core::CoreError::Unavailable {
                dependency: "region store".into(),
                message,
            }),
        }
    }
}
