//! The trace search, the exporter list and their mutations (REQ-126, slice 3).
//!
//! Split out from `observability.rs` rather than appended to it, for the same reason that file is
//! already long: this is a different subject with a different set of invariants. The log explorer
//! reads rows someone else wrote; this module *configures* what gets written and where.
//!
//! ## Three decisions worth stating
//!
//! 1. **The trace detail answers a backend link even when there is none.** The request says that
//!    "when no tracing backend is configured the screen says so and offers the collector example
//!    instead of an empty waterfall". `backend_trace_url` is therefore `Option` in the response and
//!    the screen's empty state keys off it — never off a zero span count, because a trace with
//!    zero spans is a different problem.
//! 2. **The exporter list is metadata only.** An exporter row carries a `secret_id`, never a
//!    credential, and the response projects that to `auth_configured: bool`. A response that echoed
//!    the reference would still be harmless, but a response that echoed the *value* would be a
//!    leak in a screen every operator opens — so the DTO has no field a value could go in.
//! 3. **`Test` is a real request to the configured endpoint.** The request asks the form to "send
//!    a synthetic batch and report the backend's response", and a `Test` that only validates the
//!    form is a dead button. It goes to whatever the operator configured — including a wrong one,
//!    which is exactly the case the walkthrough drives.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use omnion_audit::NewAuditEntry;
use omnion_telemetry::exporter::{self, ExporterKind, FlushOutcome};
use omnion_telemetry::trace_store;
use omnion_telemetry::tracing_spine;
use serde::Serialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

fn internal(message: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "telemetry_error",
        message.to_string(),
    )
}

fn not_found(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, code, message)
}

/// The trace search's query string.
#[derive(Debug, Default, serde::Deserialize)]
pub struct TraceQuery {
    /// Only traces of this request — the jump from a log line or an error banner.
    pub request_id: Option<Uuid>,
    /// Only traces of this route template.
    pub route: Option<String>,
    /// Only `ok` or only `error`.
    pub status: Option<String>,
    /// Only traces at least this slow, in milliseconds.
    pub min_duration_ms: Option<i64>,
    /// How far back, in minutes.
    #[serde(default)]
    pub window_minutes: Option<i64>,
    /// How many rows.
    pub limit: Option<i64>,
}

impl TraceQuery {
    fn status_value(&self) -> Result<Option<&str>, ApiError> {
        match self.status.as_deref() {
            None => Ok(None),
            Some("ok") | Some("error") => Ok(self.status.as_deref()),
            Some(other) => Err(ApiError::bad_request(
                "invalid_status",
                format!("status must be `ok` or `error`, not `{other}`"),
            )),
        }
    }
}

/// Search the trace index.
pub async fn read_traces(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<TraceQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let status = query.status_value()?;
    if let Some(minutes) = query.window_minutes
        && !(1..=(60 * 24 * 30)).contains(&minutes)
    {
        return Err(ApiError::bad_request(
            "invalid_window",
            "window_minutes must be between 1 and 43200 (30 days)",
        ));
    }

    let traces = tracing_spine::search(
        state.db().pool(),
        query.request_id,
        query.route.as_deref(),
        status,
        query.min_duration_ms,
        query.window_minutes.unwrap_or(60),
        query.limit,
    )
    .await
    .map_err(internal)?;

    Ok(Json(json!({
        "traces": traces,
        "total": trace_store::count(state.db().pool()).await.map_err(internal)?,
        "window_minutes": query.window_minutes.unwrap_or(60),
    })))
}

/// One trace with its waterfall.
pub async fn read_trace(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(trace_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let record = trace_store::load(state.db().pool(), &trace_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            // A trace that is not in the index is a normal answer, not an error: an unsampled
            // trace and an expired one are both "not here", and the screen's message says which
            // the operator should expect rather than showing a 404 with no explanation.
            not_found(
                "trace_not_found",
                format!(
                    "no trace `{trace_id}` in the index; it may be unsampled, or older than the \
                     retention window — the full trace lives in your tracing backend"
                ),
            )
        })?;

    // The waterfall is ordered by the spans' own offsets, and the `truncated` flag is passed
    // through so the screen can say the drawing is incomplete rather than implying it is complete.
    let waterfall: Vec<&omnion_telemetry::Span> = record.waterfall();
    Ok(Json(json!({
        "trace_id": record.trace_id,
        "root_name": record.root_name,
        "service": record.service,
        "route": record.route,
        "request_id": record.request_id,
        "started_at": record.started_at.format(&Rfc3339).unwrap_or_default(),
        "duration_ms": record.duration_ms,
        "span_count": record.span_count,
        "spans_kept": record.spans_kept,
        "spans_truncated": record.spans_truncated,
        "status": record.status,
        "sampled": record.sampled,
        "sampling": record.sampling,
        "backend_trace_url": record.backend_trace_url,
        "spans": waterfall,
    })))
}

/// The exporter list, with health and drop counters.
pub async fn read_exporters(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    // The in-process collector is reconciled with the stored rows on read, which is what makes the
    // screen honest after a restart: a row saved but not yet loaded shows `unknown` rather than a
    // health chip from a previous process.
    let configured = load_exporters(state.db().pool()).await?;
    let live = exporter::global().statuses();

    let rows: Vec<ExporterRow> = configured
        .into_iter()
        .map(|row| {
            let status = live.iter().find(|s| s.name == row.name);
            ExporterRow {
                id: row.id,
                name: row.name.clone(),
                kind: row.kind.clone(),
                endpoint: row.endpoint.clone(),
                protocol: row.protocol.clone(),
                auth_configured: row.auth_secret_id.is_some(),
                batch_ms: row.batch_ms,
                timeout_ms: row.timeout_ms,
                enabled: row.enabled,
                health: status.map_or("unknown", |s| s.health.as_str()).to_owned(),
                buffered: status.map_or(0, |s| s.buffered),
                capacity: status.map_or(0, |s| s.capacity),
                dropped_total: status.map_or(0, |s| s.dropped_total),
                last_flush_at: status.and_then(|s| s.last_flush_at.clone()),
                last_error: status.and_then(|s| s.last_error.clone()),
            }
        })
        .collect();

    Ok(Json(json!({
        "exporters": rows,
        "kinds": ExporterKind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        // Stated on the payload rather than only in the UI copy: "what leaves this instance" is
        // the sentence the request requires, and a sentence that lives only in a component is a
        // sentence nobody can audit.
        "egress_notice": "Configuring an exporter sends log lines and span attributes to the \
                          endpoint below. Prompt and completion text, user data and secret values \
                          never leave the instance; the shared redaction pass runs on every payload.",
    })))
}

/// The body of a create or edit.
#[derive(Debug, serde::Deserialize)]
pub struct ExporterInput {
    /// The exporter's name — its identity, and the drop counter's label.
    pub name: String,
    /// One of `otlp`, `prometheus_remote_write`, `syslog`, `webhook`.
    pub kind: String,
    /// Where the batch goes.
    pub endpoint: String,
    /// The transport, where the kind has more than one.
    #[serde(default)]
    pub protocol: Option<String>,
    /// A secret id from the secret store — never an inline credential.
    #[serde(default)]
    pub auth_secret_id: Option<Uuid>,
    /// How often the buffer is flushed, in milliseconds.
    #[serde(default)]
    pub batch_ms: Option<i32>,
    /// How long one flush may take, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<i32>,
    /// Whether the exporter is on.
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ExporterInput {
    fn validate(&self) -> Result<(String, ExporterKind), ApiError> {
        let name = self.name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(ApiError::bad_request(
                "invalid_name",
                "the exporter name must be 1-64 characters",
            ));
        }
        let kind = ExporterKind::parse(&self.kind).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_kind",
                format!(
                    "kind must be one of {}",
                    ExporterKind::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?;
        if self.endpoint.trim().is_empty() {
            return Err(ApiError::bad_request(
                "invalid_endpoint",
                "the exporter needs an endpoint",
            ));
        }
        // A negative interval or a zero timeout is not "unbounded", it is a busy loop or an
        // instant failure, and both belong in a request as a 422 rather than on a timer.
        if self
            .batch_ms
            .is_some_and(|v| !(100..=3_600_000).contains(&v))
        {
            return Err(ApiError::bad_request(
                "invalid_batch_ms",
                "batch_ms must be between 100 and 3600000",
            ));
        }
        if self
            .timeout_ms
            .is_some_and(|v| !(100..=600_000).contains(&v))
        {
            return Err(ApiError::bad_request(
                "invalid_timeout_ms",
                "timeout_ms must be between 100 and 600000",
            ));
        }
        Ok((name.to_owned(), kind))
    }
}

/// Add an exporter.
pub async fn create_exporter(
    State(state): State<AppState>,
    session: CurrentSession,
    axum::extract::Json(input): axum::extract::Json<ExporterInput>,
) -> Result<impl IntoResponse, ApiError> {
    let (name, kind) = input.validate()?;
    if load_exporters(state.db().pool())
        .await?
        .iter()
        .any(|row| row.name == name)
    {
        return Err(ApiError::bad_request(
            "exporter_exists",
            format!("an exporter named `{name}` already exists"),
        ));
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "insert into obs_exporters (id, name, kind, endpoint, protocol, auth_secret_id, \
                                batch_ms, timeout_ms, enabled, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(&name)
    .bind(kind.as_str())
    .bind(input.endpoint.trim())
    .bind(input.protocol.as_deref())
    .bind(input.auth_secret_id)
    .bind(input.batch_ms.unwrap_or(5000))
    .bind(input.timeout_ms.unwrap_or(10_000))
    .bind(input.enabled.unwrap_or(true))
    .bind(session.user.id)
    .execute(state.db().pool())
    .await
    .map_err(internal)?;

    register_in_process(&name, kind);

    audit(
        &state,
        &session,
        "observability.exporter.created",
        "exporter",
        &id,
        json!({ "name": name, "kind": kind.as_str() }),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(json!({ "id": id, "name": name }))))
}

/// Edit an exporter.
pub async fn update_exporter(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    axum::extract::Json(input): axum::extract::Json<ExporterInput>,
) -> Result<impl IntoResponse, ApiError> {
    let (name, kind) = input.validate()?;
    let result = sqlx::query(
        "update obs_exporters set name = $2, kind = $3, endpoint = $4, protocol = $5, \
                                auth_secret_id = $6, batch_ms = $7, timeout_ms = $8, enabled = $9 \
         where id = $1",
    )
    .bind(id)
    .bind(&name)
    .bind(kind.as_str())
    .bind(input.endpoint.trim())
    .bind(input.protocol.as_deref())
    .bind(input.auth_secret_id)
    .bind(input.batch_ms.unwrap_or(5000))
    .bind(input.timeout_ms.unwrap_or(10_000))
    .bind(input.enabled.unwrap_or(true))
    .execute(state.db().pool())
    .await
    .map_err(internal)?;

    if result.rows_affected() == 0 {
        return Err(not_found(
            "exporter_not_found",
            format!("no exporter with id {id}"),
        ));
    }
    register_in_process(&name, kind);
    exporter::global().set_enabled(&name, input.enabled.unwrap_or(true));

    audit(
        &state,
        &session,
        "observability.exporter.updated",
        "exporter",
        &id,
        json!({ "name": name, "kind": kind.as_str() }),
    )
    .await?;

    Ok(Json(json!({ "id": id, "name": name })))
}

/// Send a synthetic batch to an exporter's endpoint and report the backend's answer.
///
/// The request is explicit that this is a real send — "Test sends a synthetic batch and reports
/// the backend's response" — and the QA plan drives it against a deliberately wrong endpoint
/// expecting a degraded report rather than an error page. So the failure is a 200 with the
/// backend's own words, not a 502: a `Test` button that renders an error page has told the
/// operator nothing they could not have learned by waiting.
pub async fn test_exporter(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, i32)>(
        "select name, kind, endpoint, protocol, timeout_ms from obs_exporters where id = $1",
    )
    .bind(id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(internal)?
    .ok_or_else(|| not_found("exporter_not_found", format!("no exporter with id {id}")))?;
    let (name, kind, endpoint, _protocol, timeout_ms) = row;

    let probe = exporter::Probe {
        timeout_ms: i64::from(timeout_ms),
    };
    let outcome = probe.send(&endpoint).await;
    exporter::global().record_outcome(&name, &outcome);

    let (ok, detail) = match &outcome {
        FlushOutcome::Accepted { response } => (true, response.clone()),
        FlushOutcome::Failed { error } => (false, error.clone()),
    };

    audit(
        &state,
        &session,
        "observability.exporter.tested",
        "exporter",
        &id,
        json!({ "name": name, "kind": kind, "ok": ok }),
    )
    .await?;

    Ok(Json(json!({
        "name": name,
        "kind": kind,
        "ok": ok,
        "detail": detail,
        // `.to_owned()` because the status is a temporary: returning `&str` out of it would
        // borrow a value that dies at the end of the statement.
        "health": exporter::global()
            .status(&name)
            .map_or_else(|| "unknown".to_owned(), |status| status.health),
    })))
}

/// Delete an exporter.
pub async fn delete_exporter(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let name: Option<String> =
        sqlx::query_scalar("delete from obs_exporters where id = $1 returning name")
            .bind(id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(internal)?;
    let name =
        name.ok_or_else(|| not_found("exporter_not_found", format!("no exporter with id {id}")))?;
    exporter::global().remove(&name);

    audit(
        &state,
        &session,
        "observability.exporter.deleted",
        "exporter",
        &id,
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(json!({ "deleted": id })))
}

/// One exporter as the screen reads it.
#[derive(Debug, Serialize)]
pub struct ExporterRow {
    /// The row's id.
    pub id: Uuid,
    /// The exporter's name.
    pub name: String,
    /// Its kind.
    pub kind: String,
    /// Where it sends.
    pub endpoint: String,
    /// Its transport.
    pub protocol: Option<String>,
    /// Whether an auth secret is REFERENCED — never the secret itself.
    pub auth_configured: bool,
    /// The flush interval.
    pub batch_ms: i32,
    /// The flush timeout.
    pub timeout_ms: i32,
    /// Whether it is on.
    pub enabled: bool,
    /// The health chip.
    pub health: String,
    /// What is waiting to be sent.
    pub buffered: usize,
    /// The buffer's cap.
    pub capacity: usize,
    /// How many samples were dropped because the buffer was full.
    pub dropped_total: u64,
    /// When the last batch left.
    pub last_flush_at: Option<String>,
    /// The last error, verbatim.
    pub last_error: Option<String>,
}

/// A stored exporter row.
#[derive(Debug, sqlx::FromRow)]
struct StoredExporter {
    id: Uuid,
    name: String,
    kind: String,
    endpoint: String,
    protocol: Option<String>,
    auth_secret_id: Option<Uuid>,
    batch_ms: i32,
    timeout_ms: i32,
    enabled: bool,
}

async fn load_exporters(pool: &sqlx::PgPool) -> Result<Vec<StoredExporter>, ApiError> {
    let rows = sqlx::query_as::<_, StoredExporter>(
        "select id, name, kind, endpoint, protocol, auth_secret_id, batch_ms, timeout_ms, enabled \
         from obs_exporters order by name",
    )
    .fetch_all(pool)
    .await
    .map_err(internal)?;
    Ok(rows)
}

/// Load a stored row into the in-process collector.
///
/// Failure is reported rather than swallowed: a row that is saved but never buffered is exactly
/// the "exporter configured but nothing arrives" state an operator cannot diagnose.
fn register_in_process(name: &str, kind: ExporterKind) {
    if let Err(error) = exporter::global().register(name, kind, exporter::DEFAULT_BUFFER_CAPACITY) {
        eprintln!("omnion-api: the exporter `{name}` could not be registered: {error}");
    }
}

/// Every exporter mutation writes an audit row — the acceptance line names it, and an exporter
/// row is a standing instruction to send this instance's telemetry somewhere, which is exactly
/// the kind of change a security review asks about later.
async fn audit(
    state: &AppState,
    session: &CurrentSession,
    // `&'static str` because the audit store's action column is a `&'static str`: the set of
    // actions is a closed vocabulary, and a runtime-built action name would be a row nobody can
    // find by querying the catalogue.
    action: &'static str,
    target_type: &'static str,
    target_id: &Uuid,
    metadata: serde_json::Value,
) -> Result<(), ApiError> {
    omnion_audit::record(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, action)
            .organization(session.user.organization_id)
            .target(target_type, target_id.to_string())
            .metadata(metadata),
    )
    .await
    // The store answers with the written row; the caller only needs to know it landed.
    .map(|_| ())
    .map_err(internal)
}
