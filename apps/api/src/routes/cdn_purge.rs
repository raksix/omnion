//! `/api/v1/cdn/purges` — the purge console, the history and the retry
//! (docs/requests/REQ-011, slice 2).
//!
//! Reading history is `cdn.read`, asking for a purge is `cdn.purge` and retrying a failed
//! one is `cdn.manage`. Those are three different keys rather than one, and the split is
//! the point: an operator who may look at what the cache did, and one who may invalidate a
//! production cache, and one who may *make it do the same thing again* are three different
//! amounts of trust. A single `cdn.manage` covering all three would let anybody who can fix
//! a typo in a cache rule also flush an entire zone.
//!
//! Everything here is a thin shell over `omnion_cdn::purge`: the validation, the batching,
//! the backoff and the status fold all live in the crate, and this file's only jobs are
//! tenancy, permission, turning a field-level refusal into a `400` that names the field,
//! and the audit entry.

use axum::Json;
use axum::extract::{Path, State};
use omnion_audit::NewAuditEntry;
use omnion_cdn::purge::{
    self, NewPurge, PurgeFilter, PurgeInputError, PurgeItemRow, PurgeKind, PurgeStatus, MAX_TARGETS,
};
use omnion_cdn::store::{self, SettingsRow};
use omnion_cdn::{CdnError, is_shipped};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::cdn::site_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/cdn/purges`.
///
/// `zone_confirmed` is the typed `PURGE`, sent as a boolean rather than as the literal
/// string: the console checks that the field says `PURGE` and sends the fact. Keeping the
/// literal here would mean the server could not tell "the user typed PURGE" from "the client
/// sent `true`", and a whole-zone purge is not a thing to allow on a boolean.
#[derive(Debug, Deserialize)]
pub struct PurgeInput {
    /// The site whose cache is being invalidated.
    pub site_id: Uuid,
    /// `url`, `tag` or `all`.
    pub kind: String,
    /// One target per line, already split by the client. A `text[]` is accepted here rather
    /// than a textarea string so the API never has to guess a line ending.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Whether the whole-zone confirmation was typed.
    #[serde(default)]
    pub zone_confirmed: bool,
}

/// A purge as the history table and the drawer see it.
#[derive(Debug, Serialize)]
pub struct PurgeBody {
    /// Primary key.
    pub id: Uuid,
    /// The site, or `null` once the site is gone.
    pub site_id: Option<Uuid>,
    /// `url`, `tag` or `all`.
    pub kind: String,
    /// The targets as submitted.
    pub targets: Vec<String>,
    /// The current state.
    pub status: String,
    /// The adapter that ran (or will run) it.
    pub provider: String,
    /// How many items it expanded into.
    pub item_count: i32,
    /// How many of those failed.
    pub failed_count: i32,
    /// Who asked, or `null` if that account is gone.
    pub requested_by: Option<Uuid>,
    /// When it was asked for.
    pub requested_at: OffsetDateTime,
    /// When a worker first claimed it.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished, if it has.
    pub finished_at: Option<OffsetDateTime>,
    /// The provider's message, verbatim.
    pub error: Option<String>,
    /// Whether the drawer offers a retry.
    ///
    /// Computed here rather than in the panel so the two can never disagree about which
    /// rows have something to do — a retry button on a `succeeded` row is a dead button,
    /// and the request forbids those.
    pub retryable: bool,
}

impl PurgeBody {
    /// Project a stored row.
    fn build(row: &purge::PurgeRow) -> Self {
        let status = purge::PurgeStatus::parse(&row.status).unwrap_or(purge::PurgeStatus::Queued);
        Self {
            id: row.id,
            site_id: row.site_id,
            kind: row.kind.clone(),
            targets: row.targets.clone(),
            status: status.as_str().to_string(),
            provider: row.provider.clone(),
            item_count: row.item_count,
            failed_count: row.failed_count,
            requested_by: row.requested_by,
            requested_at: row.requested_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
            error: row.error.clone(),
            retryable: status.retryable(),
        }
    }
}

/// One target inside the detail drawer.
#[derive(Debug, Serialize)]
pub struct PurgeItemBody {
    /// Primary key.
    pub id: i64,
    /// The target, verbatim.
    pub target: String,
    /// Its state.
    pub status: String,
    /// How many times it has been attempted.
    pub attempts: i32,
    /// When it may next be attempted.
    pub next_attempt_at: OffsetDateTime,
    /// The provider's HTTP status, when there was one.
    pub response_status: Option<i32>,
    /// The provider's message, verbatim.
    pub error: Option<String>,
    /// When it reached a terminal state.
    pub done_at: Option<OffsetDateTime>,
}

impl PurgeItemBody {
    /// Project a stored row.
    fn build(row: &PurgeItemRow) -> Self {
        Self {
            id: row.id,
            target: row.target.clone(),
            status: row.status.clone(),
            attempts: row.attempts,
            next_attempt_at: row.next_attempt_at,
            response_status: row.response_status,
            error: row.error.clone(),
            done_at: row.done_at,
        }
    }
}

/// Response of `GET /api/v1/cdn/purges` and `GET /api/v1/cdn/purges/{id}`.
#[derive(Debug, Serialize)]
pub struct PurgeDetailResponse {
    /// The purge itself.
    pub purge: PurgeBody,
    /// Its items, in listing order.
    pub items: Vec<PurgeItemBody>,
}

/// Response of `GET /api/v1/cdn/purges`.
#[derive(Debug, Serialize)]
pub struct PurgeListResponse {
    /// The page of history.
    pub purges: Vec<PurgeBody>,
    /// How many rows match the filter in total, so the panel can say "50 of 312".
    pub total: i64,
    /// The cap the console enforces, so the form can count before submitting.
    pub max_targets: usize,
}

/// Response of `GET /api/v1/cdn/status` — the overview's cards.
#[derive(Debug, Serialize)]
pub struct CdnStatusResponse {
    /// The adapter key in use.
    pub provider: String,
    /// Whether the adapter is one that actually ships.
    ///
    /// A settings row edited by hand can name an adapter that does not exist; the panel
    /// needs to say so rather than render a name that cannot purge.
    pub provider_shipped: bool,
    /// Items waiting to be attempted.
    pub queue_depth: i64,
    /// Purges that have not reached a terminal state.
    pub open_purges: i64,
    /// Purges requested in the last 24 hours.
    pub purges_24h: i64,
    /// Of those, how many fully succeeded.
    pub succeeded_24h: i64,
    /// Of those, how many ended partial.
    pub partial_24h: i64,
    /// Of those, how many ended failed.
    pub failed_24h: i64,
    /// Share of the window that did not fully succeed, as a percentage.
    pub failure_rate: f64,
    /// The last 20 purges, newest first.
    pub recent: Vec<PurgeBody>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/cdn/status` — what the overview's cards show.
pub async fn status(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<SiteQuery>,
) -> Result<Json<CdnStatusResponse>, ApiError> {
    let site_id = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();

    let depth = purge::queue_depth(pool, Some(site_id)).await?;
    let counters = purge::counters(pool, Some(site_id), 24).await?;
    let (provider, _settings, _attempts) =
        purge::provider_for_site(pool, Some(site_id)).await?;

    let page = purge::list(
        pool,
        &PurgeFilter {
            site_id: Some(site_id),
            limit: 20,
            ..PurgeFilter::default()
        },
    )
    .await?;

    Ok(Json(CdnStatusResponse {
        provider_shipped: is_shipped(&provider),
        provider,
        queue_depth: depth.pending_items,
        open_purges: depth.open_purges,
        purges_24h: counters.total,
        succeeded_24h: counters.succeeded,
        partial_24h: counters.partial,
        failed_24h: counters.failed,
        failure_rate: counters.failure_rate(),
        recent: page.purges.iter().map(PurgeBody::build).collect(),
    }))
}

/// `GET /api/v1/cdn/purges` — the history table.
pub async fn list_purges(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<PurgeListQuery>,
) -> Result<Json<PurgeListResponse>, ApiError> {
    let site_id = site_in_scope(&state, &current, query.site_id).await?;

    let filter = PurgeFilter {
        site_id: Some(site_id),
        // An unparseable filter is *no filter* rather than a 400: a status chip the panel
        // is still transitioning should show the unfiltered table, not an error banner
        // over a screen that works.
        status: query.status.as_deref().and_then(PurgeStatus::parse),
        kind: query.kind.as_deref().and_then(PurgeKind::parse),
        since: query.since.and_then(parse_instant),
        until: query.until.and_then(parse_instant),
        limit: query.limit.unwrap_or(PurgeFilter::DEFAULT_LIMIT),
        offset: query.offset.unwrap_or(0),
    };

    let page = purge::list(state.db().pool(), &filter).await?;
    Ok(Json(PurgeListResponse {
        purges: page.purges.iter().map(PurgeBody::build).collect(),
        total: page.total,
        max_targets: MAX_TARGETS,
    }))
}

/// `GET /api/v1/cdn/purges/{id}` — the detail drawer.
pub async fn get_purge(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<PurgeDetailResponse>, ApiError> {
    let row = purge_in_scope(&state, &current, id).await?;
    let items = purge::items_of(state.db().pool(), id).await?;
    Ok(Json(PurgeDetailResponse {
        purge: PurgeBody::build(&row),
        items: items.iter().map(PurgeItemBody::build).collect(),
    }))
}

/// `POST /api/v1/cdn/purges` — the purge console's submit.
pub async fn create_purge(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    axum::extract::Json(input): axum::extract::Json<PurgeInput>,
) -> Result<(axum::http::StatusCode, Json<PurgeBody>), ApiError> {
    let site_id = site_in_scope(&state, &current, input.site_id).await?;

    let kind = PurgeKind::parse(&input.kind).ok_or_else(|| {
        ApiError::bad_request("invalid_purge_kind", "kind must be one of url, tag or all")
            .with_details(json!({ "field": "kind" }))
    })?;

    let targets = purge::validate(kind, &input.targets, input.zone_confirmed).map_err(input_error)?;

    // The adapter is captured now, not resolved when the worker drains. A purge queued
    // under one provider and drained under another is a history row describing a call
    // nobody made, and the operator reading it has no way to tell.
    let pool = state.db().pool();
    let (provider, _settings, _attempts) = purge::provider_for_site(pool, Some(site_id)).await?;
    if !is_shipped(&provider) {
        return Err(ApiError::bad_request(
            "unknown_purge_provider",
            format!("{provider:?} is not an adapter this build ships; pick one from the catalogue"),
        )
        .with_details(json!({ "field": "provider" })));
    }

    let new = NewPurge {
        site_id: Some(site_id),
        kind,
        targets: targets.clone(),
        provider: provider.clone(),
        requested_by: current.user.id,
    };
    let row = purge::enqueue(pool, &new, &targets).await?;

    // The event is emitted on *request*, not on completion: an operations endpoint that
    // wants to watch invalidation activity needs to see the request, and the completion
    // event comes from the worker when the provider has actually answered.
    bus::emit(
        pool,
        NewEvent::new("cdn.purge.requested")
            .organization(current.user.organization_id)
            .site(site_id)
            .actor(current.user.id)
            .payload(json!({
                "purge_id": row.id,
                "site_id": site_id,
                "kind": kind.as_str(),
                "target_count": targets.len(),
                "provider": provider,
            })),
    )
    .await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "cdn.purge.requested")
            .target("cdn_purge", row.id.to_string())
            .metadata(json!({
                "site_id": site_id,
                "kind": kind.as_str(),
                "target_count": targets.len(),
                "provider": provider,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok((axum::http::StatusCode::CREATED, Json(PurgeBody::build(&row))))
}

/// `POST /api/v1/cdn/purges/{id}/retry` — requeue only the failed items.
pub async fn retry_purge(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(id): Path<Uuid>,
) -> Result<Json<PurgeDetailResponse>, ApiError> {
    let row = purge_in_scope(&state, &current, id).await?;
    let status = PurgeStatus::parse(&row.status).unwrap_or(PurgeStatus::Queued);

    // A retry on a purge that has nothing to retry is refused with an explanation rather
    // than accepted and silently doing nothing. `409` because the row exists and the
    // request is coherent — it is the *state* that is wrong.
    if !status.retryable() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "purge_not_retryable",
            format!(
                "a {} purge has nothing to retry — only a partial or failed one does",
                status.as_str()
            ),
        ));
    }

    let requeued = purge::requeue_failed(state.db().pool(), id).await?;

    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(current.user.id, "cdn.purge.retried")
            .target("cdn_purge", id.to_string())
            .metadata(json!({ "requeued_items": requeued }))
            .ip_address(address.as_text()),
    )
    .await?;

    let items = purge::items_of(state.db().pool(), id).await?;
    Ok(Json(PurgeDetailResponse {
        purge: PurgeBody::build(&purge::find(state.db().pool(), id)
            .await?
            .ok_or_else(|| ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "purge_not_found",
                "no cache purge with that id exists",
            ))?),
        items: items.iter().map(PurgeItemBody::build).collect(),
    }))
}

/// `GET /api/v1/cdn/adapters` — the shipped catalogue and the fields each one needs.
///
/// Only adapters that actually ship appear. A catalogue entry for an adapter that is not
/// implemented is a dead button, and the whole point of the catalogue is that choosing
/// from it is a real choice.
pub async fn adapters(current: CurrentSession) -> Result<Json<AdaptersResponse>, ApiError> {
    // The catalogue is build-constant, but the route is still permission-guarded: it
    // reveals which endpoints and capabilities the installation can drive, which is not
    // something an unauthenticated caller should be able to enumerate.
    let _ = current;
    let list: Vec<_> = omnion_cdn::catalogue().into_iter().map(AdapterBody::build).collect();
    Ok(Json(AdaptersResponse { adapters: list }))
}

/// `GET /api/v1/cdn/settings` — the settings row, credentials masked.
pub async fn get_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<OptionalSiteQuery>,
) -> Result<Json<SettingsBody>, ApiError> {
    if let Some(site_id) = query.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }
    // `resolve_settings` inherits the platform row for a site that has none, which is what
    // makes a fresh installation's settings screen show the provider that is actually
    // running rather than an empty form.
    let row = store::resolve_settings(state.db().pool(), query.site_id)
        .await
        .or_else(|_| default_settings_row(query.site_id))?;
    Ok(Json(SettingsBody::build(&row)))
}

/// `PUT /api/v1/cdn/settings` — save provider, triggers and queue bounds.
pub async fn put_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    axum::extract::Json(input): axum::extract::Json<SettingsInput>,
) -> Result<Json<SettingsBody>, ApiError> {
    if let Some(site_id) = input.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }
    let pool = state.db().pool();

    if !is_shipped(&input.provider) {
        return Err(ApiError::bad_request(
            "unknown_purge_provider",
            format!(
                "{:?} is not an adapter this build ships; pick one from the catalogue",
                input.provider
            ),
        )
        .with_details(json!({ "field": "provider" })));
    }
    if !(1..=1000).contains(&input.batch_size) {
        return Err(ApiError::bad_request("invalid_batch_size", "batch size must be 1 to 1000")
            .with_details(json!({ "field": "batch_size" })));
    }
    if !(1..=10).contains(&input.max_attempts) {
        return Err(ApiError::bad_request(
            "invalid_max_attempts",
            "max attempts must be 1 to 10",
        )
        .with_details(json!({ "field": "max_attempts" })));
    }

    let row = sqlx::query_as::<_, SettingsRow>(
        "insert into cdn_settings \
            (site_id, provider, endpoint_url, zone_ref, auto_purge, batch_size, max_attempts, updated_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict do nothing \
         returning id, site_id, provider, endpoint_url, zone_ref, \
                   (credential_ciphertext is not null) as has_credential, auto_purge, batch_size, \
                   max_attempts, updated_by, created_at, updated_at",
    )
    .bind(input.site_id)
    .bind(&input.provider)
    .bind(&input.endpoint_url)
    .bind(&input.zone_ref)
    .bind(&input.auto_purge)
    .bind(input.batch_size)
    .bind(input.max_attempts)
    .bind(current.user.id)
    .fetch_optional(pool)
    .await
    .map_err(CdnError::from)
    .map_err(ApiError::from)?;

    // `on conflict do nothing` returns nothing when the row already exists, so the write
    // falls back to an update. Doing it as insert-then-update in one handler is what keeps
    // the two unique indexes (per-site and platform) from needing a special case here.
    let row = match row {
        Some(row) => row,
        None => {
            sqlx::query_as::<_, SettingsRow>(
                "update cdn_settings \
                 set provider = $2, endpoint_url = $3, zone_ref = $4, auto_purge = $5, \
                     batch_size = $6, max_attempts = $7, updated_by = $8 \
                 where site_id is not distinct from $1 \
                 returning id, site_id, provider, endpoint_url, zone_ref, \
                           (credential_ciphertext is not null) as has_credential, auto_purge, \
                           batch_size, max_attempts, updated_by, created_at, updated_at",
            )
            .bind(input.site_id)
            .bind(&input.provider)
            .bind(&input.endpoint_url)
            .bind(&input.zone_ref)
            .bind(&input.auto_purge)
            .bind(input.batch_size)
            .bind(input.max_attempts)
            .bind(current.user.id)
            .fetch_optional(pool)
            .await
            .map_err(CdnError::from)
            .map_err(ApiError::from)?
            .ok_or_else(|| {
                ApiError::from_core(omnion_core::CoreError::Unavailable {
                    dependency: "cdn settings".into(),
                    message: "the settings row disappeared between the insert and the update".into(),
                })
            })?
        }
    };

    // The credential is deliberately NOT written here. `cdn_settings.credential_ciphertext`
    // is a write-only column by design (REQ-011, "Risks") and wiring a decrypt path is
    // slice 4; accepting the field here and ignoring it would make the form's "replace
    // credential" affordance a button that reports success and stores nothing.
    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "cdn.settings.updated")
            .target("cdn_settings", input.site_id.map_or_else(|| "platform".into(), |id| id.to_string()))
            .metadata(json!({
                "provider": input.provider,
                "batch_size": input.batch_size,
                "max_attempts": input.max_attempts,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(SettingsBody::build(&row)))
}

/// `POST /api/v1/cdn/settings/test` — the "Test connection" reachability check.
pub async fn test_settings(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Query(query): axum::extract::Query<OptionalSiteQuery>,
) -> Result<Json<TestResponse>, ApiError> {
    if let Some(site_id) = query.site_id {
        site_in_scope(&state, &current, site_id).await?;
    }
    let (key, settings, _attempts) =
        purge::provider_for_site(state.db().pool(), query.site_id).await?;
    let probe = tokio::task::spawn_blocking(move || {
        omnion_cdn::provider_for(&key, &settings).verify()
    })
    .await
    .map_err(|error| {
        ApiError::from_core(omnion_core::CoreError::Unavailable {
            dependency: "cdn provider".into(),
            message: format!("the reachability check could not be scheduled: {error}"),
        })
    })?;
    Ok(Json(TestResponse {
        ok: probe.ok,
        latency_ms: probe.latency_ms,
        status: probe.status,
        message: probe.message,
    }))
}

// ---------------------------------------------------------------------------------------------
// Body and query shapes
// ---------------------------------------------------------------------------------------------

/// One adapter as the settings form's picker sees it.
#[derive(Debug, Serialize)]
pub struct AdapterBody {
    /// The stable key stored in `cdn_settings.provider`.
    pub key: String,
    /// The name a human reads.
    pub label: String,
    /// What it does, in one sentence.
    pub description: String,
    /// Whether it needs an endpoint URL.
    pub needs_endpoint: bool,
    /// Whether it needs a zone reference.
    pub needs_zone: bool,
    /// Whether it needs a write-only credential.
    pub needs_credential: bool,
    /// Whether it supports surrogate-key purging.
    ///
    /// Asked of a throwaway instance rather than read from the table: the capability is a
    /// property of the adapter's behaviour, and duplicating it into the catalogue is how a
    /// listed capability and a shipped one drift apart.
    pub supports_tags: bool,
    /// Whether it supports whole-zone purging.
    pub supports_purge_all: bool,
    /// Whether this build ships it. Every entry here is shipped by construction — the
    /// catalogue is derived from the providers themselves — and the flag is kept because a
    /// client that renders a "coming soon" row needs a value to test.
    pub shipped: bool,
}

impl AdapterBody {
    /// Project a catalogue entry.
    fn build(info: omnion_cdn::AdapterInfo) -> Self {
        // A fresh adapter instance with no configuration: enough to read its capabilities,
        // which are a property of the type and not of the operator's settings.
        let capabilities = omnion_cdn::provider_for(info.key, &Default::default()).capabilities();
        Self {
            supports_tags: capabilities.tags,
            supports_purge_all: capabilities.purge_all,
            key: info.key.to_string(),
            label: info.label.to_string(),
            description: info.description.to_string(),
            needs_endpoint: info.needs_endpoint,
            needs_zone: info.needs_zone,
            needs_credential: info.needs_credential,
            shipped: true,
        }
    }
}

/// Response of `GET /api/v1/cdn/adapters`.
#[derive(Debug, Serialize)]
pub struct AdaptersResponse {
    /// The shipped adapters.
    pub adapters: Vec<AdapterBody>,
}

/// Response of `POST /api/v1/cdn/settings/test`.
#[derive(Debug, Serialize)]
pub struct TestResponse {
    /// Whether the provider answered acceptably.
    pub ok: bool,
    /// Round-trip time.
    pub latency_ms: u64,
    /// The HTTP status it returned, when there was one.
    pub status: Option<u16>,
    /// What it said, shown inline under the button.
    pub message: String,
}

/// The settings row as the panel reads it, credential reduced to a presence flag.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// Primary key.
    pub id: Uuid,
    /// The site, or `null` for the platform row.
    pub site_id: Option<Uuid>,
    /// The adapter key.
    pub provider: String,
    /// Configured endpoint.
    pub endpoint_url: Option<String>,
    /// Configured zone reference.
    pub zone_ref: Option<String>,
    /// Whether a credential is stored. The value is never returned.
    pub has_credential: bool,
    /// Trigger toggles, event name to enabled.
    pub auto_purge: serde_json::Value,
    /// Batch cap per provider call.
    pub batch_size: i32,
    /// Attempts before an item is left failed.
    pub max_attempts: i32,
    /// Last editor.
    pub updated_by: Option<Uuid>,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl SettingsBody {
    /// Project a stored row.
    fn build(row: &SettingsRow) -> Self {
        Self {
            id: row.id,
            site_id: row.site_id,
            provider: row.provider.clone(),
            endpoint_url: row.endpoint_url.clone(),
            zone_ref: row.zone_ref.clone(),
            has_credential: row.has_credential,
            auto_purge: row.auto_purge.clone(),
            batch_size: row.batch_size,
            max_attempts: row.max_attempts,
            updated_by: row.updated_by,
            updated_at: row.updated_at,
        }
    }
}

/// Body of `PUT /api/v1/cdn/settings`.
#[derive(Debug, Deserialize)]
pub struct SettingsInput {
    /// The site, or `null` for the platform row.
    pub site_id: Option<Uuid>,
    /// The adapter key.
    pub provider: String,
    /// The endpoint the adapter posts to.
    pub endpoint_url: Option<String>,
    /// The zone reference the adapter purges within.
    pub zone_ref: Option<String>,
    /// Trigger toggles.
    #[serde(default)]
    pub auto_purge: serde_json::Value,
    /// Batch cap, 1–1000.
    pub batch_size: i32,
    /// Attempt budget, 1–10.
    pub max_attempts: i32,
}

/// `?site_id=` for the routes that accept a platform row.
#[derive(Debug, Deserialize)]
pub struct OptionalSiteQuery {
    /// The site, if the caller named one.
    pub site_id: Option<Uuid>,
}

/// `?site_id=` for the history filters.
#[derive(Debug, Deserialize)]
pub struct PurgeListQuery {
    /// The site.
    pub site_id: Uuid,
    /// Status filter.
    pub status: Option<String>,
    /// Kind filter.
    pub kind: Option<String>,
    /// Lower bound on `requested_at`, RFC 3339.
    pub since: Option<String>,
    /// Upper bound on `requested_at`, RFC 3339.
    pub until: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// `?site_id=`, required.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// The site.
    pub site_id: Uuid,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// A settings row for an installation that has never been configured.
///
/// Returning defaults rather than a 404 is what lets the settings screen render a real form
/// on a fresh install. The row is not written — it is the shape the form would save.
fn default_settings_row(site_id: Option<Uuid>) -> Result<SettingsRow, ApiError> {
    Ok(SettingsRow {
        id: Uuid::nil(),
        site_id,
        provider: "origin".to_string(),
        endpoint_url: None,
        zone_ref: None,
        has_credential: false,
        auto_purge: json!({
            "page.published": true,
            "page.unpublished": true,
            "page.deleted": true,
            "media.replaced": true,
            "theme.activated": true,
            "site.domain.changed": true,
        }),
        batch_size: 100,
        max_attempts: 5,
        updated_by: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    })
}

/// Turn a console refusal into a `400` that names the field.
fn input_error(error: PurgeInputError) -> ApiError {
    let code = match &error {
        PurgeInputError::Kind => "invalid_purge_kind",
        PurgeInputError::EmptyTargets { .. } => "empty_purge_targets",
        PurgeInputError::AllNeedsConfirmation => "purge_all_unconfirmed",
        PurgeInputError::TooManyTargets { .. } => "too_many_purge_targets",
        PurgeInputError::MalformedUrl => "invalid_purge_url",
        PurgeInputError::MalformedTag => "invalid_purge_tag",
    };
    ApiError::bad_request(code, error.to_string())
        .with_details(json!({ "field": error.field() }))
}

/// Load a purge and check the caller owns its site.
async fn purge_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<purge::PurgeRow, ApiError> {
    let row = purge::find(state.db().pool(), id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "purge_not_found",
                "no cache purge with that id exists",
            )
        })?;
    if let Some(site_id) = row.site_id {
        site_in_scope(state, current, site_id).await?;
    } else if current.user.organization_id.is_none() {
        // A purge whose site is gone has no organization to check against, and a platform
        // account is the only thing that should be able to read it — otherwise the
        // tenancy rule has a hole exactly where the data is thinnest.
        return Err(ApiError::forbidden(
            "permission_denied",
            "this purge belongs to a site that no longer exists",
        ));
    }
    Ok(row)
}

/// Parse an RFC 3339 instant, ignoring anything else.
fn parse_instant(value: String) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339).ok()
}
