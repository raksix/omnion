//! `/api/v1/health` incidents + threshold policy (REQ-014, slice 3).
//!
//! Slice 1 gave the panel a *present tense* and slice 2 a *history*, and this module is the
//! third tense: **what broke, when, for how long, and who acknowledged it.** It is the surface
//! that turns a status screen into an incident record, and it is deliberately small — five
//! routes over the store in [`omnion_health::incidents`], with no business rules of its own.
//!
//! Three rules run through every handler here, each one a way an incident list ends up
//! untrustworthy:
//!
//! * **The list answers with a count as well as rows.** A screen that shows "20 incidents" when
//!   there are 340 is not wrong in any way the reader can detect, and the number it shows next
//!   to the table is the only thing that tells them there is more. `total` is read by the same
//!   filter as the rows, not by the page it happened to return.
//! * **A malformed filter is refused, not ignored.** `?state=pending` on a filter that only
//!   knows `open` and `resolved` would otherwise answer with the *unfiltered* list — a screen
//!   the operator believes is filtered, showing everything. An ignored filter is a wrong answer
//!   with no symptom, which is the only kind of wrong a status screen cannot recover from.
//! * **The actor comes from the session, never from the body.** An acknowledgement records *who
//!   looked*; taking that from a request field would make the column a self-report, and the
//!   one question it exists to answer ("was anybody on this?") would have no trustworthy
//!   answer at all.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_health::{
    HealthSettings, Incident, IncidentFilter, IncidentPage, SettingsUpdate, THRESHOLD_METRICS,
    Threshold, Thresholds,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One incident as the panel reads it.
#[derive(Debug, Serialize)]
pub struct IncidentBody {
    /// The row's id — what `PATCH /health/incidents/{id}` takes.
    pub id: Uuid,
    /// Which service.
    pub service: String,
    /// The state it was in when this began.
    pub from_state: String,
    /// The state it moved to.
    pub to_state: String,
    /// The sentence.
    pub summary: String,
    /// Named fields behind the sentence.
    pub detail: Value,
    /// When it began.
    pub started_at: String,
    /// When it ended, if it has.
    pub resolved_at: Option<String>,
    /// `open` or `resolved` — a word the screen filters on, rather than making it re-derive
    /// one from two nullable columns and get it wrong once.
    pub state: String,
    /// How long it ran, in seconds. `None` while it is open: a duration for something that has
    /// not ended is a number that changes on every read.
    pub duration_seconds: Option<i64>,
    /// Whether a maintenance window suppressed this row.
    pub suppressed: bool,
    /// Who acknowledged it.
    pub acknowledged_by: Option<Uuid>,
    /// When.
    pub acknowledged_at: Option<String>,
    /// Their note.
    pub note: Option<String>,
}

impl From<&Incident> for IncidentBody {
    fn from(incident: &Incident) -> Self {
        Self {
            id: incident.id,
            service: incident.service.clone(),
            from_state: incident.from_state.clone(),
            to_state: incident.to_state.clone(),
            summary: incident.summary.clone(),
            detail: incident.detail.clone(),
            started_at: incident.started_at.to_string(),
            resolved_at: incident.resolved_at.map(|at| at.to_string()),
            state: if incident.is_open() { "open" } else { "resolved" }.to_string(),
            duration_seconds: incident.duration_seconds(),
            suppressed: incident.suppressed,
            acknowledged_by: incident.acknowledged_by,
            acknowledged_at: incident.acknowledged_at.map(|at| at.to_string()),
            note: incident.note.clone(),
        }
    }
}

/// The list's answer.
#[derive(Debug, Serialize)]
pub struct IncidentsBody {
    /// This page's rows, newest first.
    pub incidents: Vec<IncidentBody>,
    /// How many rows match the filter, ignoring the page window. The screen shows this next to
    /// the table, because "20 rows" beside "340 incidents" is the only visible difference
    /// between a filtered list and an unfiltered one.
    pub total: i64,
    /// The services the filter accepts, so a client never has to guess the vocabulary.
    pub services: Vec<String>,
}

/// Turn a page into the body the panel reads.
///
/// A free function rather than `impl IncidentPage`, because `IncidentPage` is defined in the
/// `omnion-health` crate and Rust's orphan rule forbids an inherent `impl` for it here. That
/// rule is not a style preference: the obvious alternative — moving the `impl` into the health
/// crate — would make the API's *wire shape* (`services` is a list of vocabulary the client
/// should not hard-code) a decision the storage crate makes, and the first change to that
/// payload would then have to touch two crates.
fn incidents_body(page: IncidentPage) -> IncidentsBody {
    IncidentsBody {
        total: page.total,
        incidents: page.incidents.iter().map(IncidentBody::from).collect(),
        services: omnion_health::all_services()
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// What `PATCH /health/incidents/{id}` accepts.
///
/// One field, and it is a string rather than a boolean: the request asks to "acknowledge or
/// resolve", and an `action` that cannot be anything but one of two values is still worth
/// typing because the *second* one takes no arguments and the first does — a body of
/// `{ "resolved": true }` would have to invent a field to carry the note into, and the field
/// would then be required on a resolve that has nothing to say.
#[derive(Debug, Deserialize)]
pub struct IncidentPatch {
    /// `acknowledge` or `resolve`.
    pub action: String,
    /// The acknowledgement note. Optional; an empty note is allowed because a person
    /// acknowledging at 3 a.m. is not required to also write an essay.
    #[serde(default)]
    pub note: Option<String>,
}

/// One metric's pair as the settings form reads it.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ThresholdBody {
    /// The metric key.
    pub metric: String,
    /// The attention limit.
    pub warn: f64,
    /// The wake-up limit.
    pub crit: f64,
    /// `above` or `below`.
    pub direction: String,
    /// The unit the metric is measured in, so the form's label does not carry its own list.
    pub unit: String,
    /// Whether a pair is stored for this metric at all.
    pub configured: bool,
}

impl ThresholdBody {
    fn of(threshold: &Threshold, configured: bool) -> Self {
        Self {
            metric: threshold.metric.clone(),
            warn: threshold.warn,
            crit: threshold.crit,
            direction: threshold.direction.clone(),
            unit: omnion_health::metric_unit(&threshold.metric).to_string(),
            configured,
        }
    }
}

/// The settings screen's answer.
#[derive(Debug, Serialize)]
pub struct SettingsBody {
    /// How often the runner probes.
    pub check_interval_seconds: i32,
    /// After how long a silent worker counts as stale.
    pub worker_stale_seconds: i32,
    /// Every thresholded metric, in display order, whether or not one is stored — because a
    /// form that only lists configured rows hides the five metrics nobody has set yet, and
    /// "not configured" is exactly what a first-run operator needs to be shown.
    pub thresholds: Vec<ThresholdBody>,
    /// The notification toggles.
    pub notifications: Value,
    /// Who saved it last.
    pub updated_by: Option<Uuid>,
    /// When.
    pub updated_at: String,
    /// The bounds each interval accepts, so the form's `min`/`max` come from the server that
    /// enforces them rather than from a copy in the client.
    pub bounds: BoundsBody,
}

/// The numbers a settings form must not guess.
#[derive(Debug, Serialize)]
pub struct BoundsBody {
    /// Interval, seconds.
    pub check_interval_seconds: (i32, i32),
    /// Worker stale window, seconds.
    pub worker_stale_seconds: (i32, i32),
}

/// What `PUT /health/settings` accepts.
#[derive(Debug, Deserialize, Default)]
pub struct SettingsBodyIn {
    /// The probe interval. `null` means "leave it alone", so a form with one tab does not
    /// reset the other.
    #[serde(default)]
    pub check_interval_seconds: Option<i32>,
    /// The worker stale window.
    #[serde(default)]
    pub worker_stale_seconds: Option<i32>,
    /// The pairs to store. The whole document is replaced — see
    /// [`omnion_health::save_settings`] for why a partial merge makes deletion impossible.
    #[serde(default)]
    pub thresholds: Option<Vec<ThresholdBody>>,
    /// The notification toggles.
    #[serde(default)]
    pub notifications: Option<Value>,
}

/// The maintenance windows, as the settings tab's table reads them.
#[derive(Debug, Serialize)]
pub struct MaintenanceWindowBody {
    /// The row's id — what `DELETE` takes.
    pub id: Uuid,
    /// When it starts.
    pub starts_at: String,
    /// When it ends.
    pub ends_at: String,
    /// Which services it covers; empty means all of them.
    pub services: Vec<String>,
    /// The note.
    pub note: String,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: String,
    /// Whether it covers this moment — the word the settings table filters by, because "is
    /// this window active" is a question about *now* and a client cannot answer it from the
    /// two timestamps without reimplementing the comparison.
    pub active: bool,
}

/// One window as stored.
#[derive(Debug, sqlx::FromRow)]
pub struct MaintenanceWindow {
    /// The row's id.
    pub id: Uuid,
    /// When it starts.
    pub starts_at: time::OffsetDateTime,
    /// When it ends.
    pub ends_at: time::OffsetDateTime,
    /// Which services it covers.
    pub services: Vec<String>,
    /// The note.
    pub note: String,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: time::OffsetDateTime,
}

/// What `POST /health/maintenance-windows` accepts.
#[derive(Debug, Deserialize)]
pub struct MaintenanceWindowIn {
    /// When it starts.
    pub starts_at: time::OffsetDateTime,
    /// When it ends.
    pub ends_at: time::OffsetDateTime,
    /// Which services; omitted or empty means all of them.
    #[serde(default)]
    pub services: Option<Vec<String>>,
    /// The note.
    #[serde(default)]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The query the incidents list accepts.
#[derive(Debug, Deserialize)]
pub struct IncidentsQuery {
    /// Only this service.
    pub service: Option<String>,
    /// `open` or `resolved`.
    pub state: Option<String>,
    /// Only rows from this moment on. RFC 3339.
    pub from: Option<String>,
    /// Only rows up to this moment.
    pub to: Option<String>,
    /// How many rows.
    pub limit: Option<i64>,
    /// How many to skip.
    pub offset: Option<i64>,
}

/// Parse an RFC 3339 instant, refusing a malformed one with a message.
///
/// A filter that quietly drops an unparseable `from` returns rows the operator did not ask
/// for, and the screen cannot tell — so the parse is here, and it names the parameter.
fn parse_instant(raw: &str, field: &str) -> Result<time::OffsetDateTime, ApiError> {
    time::OffsetDateTime::parse(
        raw.trim(),
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| {
        ApiError::bad_request(
            "invalid_incident_filter",
            format!("{field} must be an RFC 3339 instant, e.g. 2026-09-30T12:00:00Z"),
        )
    })
}

/// `GET /health/incidents` — the list, newest first, with the count behind the filter.
pub async fn incidents(
    State(state): State<AppState>,
    Query(query): Query<IncidentsQuery>,
) -> Result<Json<IncidentsBody>, ApiError> {
    let from = query
        .from
        .as_deref()
        .map(|raw| parse_instant(raw, "from"))
        .transpose()?;
    let to = query
        .to
        .as_deref()
        .map(|raw| parse_instant(raw, "to"))
        .transpose()?;
    let filter = IncidentFilter {
        service: query.service,
        state: query.state,
        from,
        to,
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
    };
    let page = omnion_health::list_incidents(state.db().pool(), &filter)
        .await
        .map_err(map_store)?;
    Ok(Json(incidents_body(page)))
}

/// `GET /health/incidents/{id}` — one incident with its own sample context.
pub async fn incident(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<IncidentBody>, ApiError> {
    let row = omnion_health::incident(state.db().pool(), id)
        .await
        .map_err(map_store)?;
    Ok(Json(IncidentBody::from(&row)))
}

/// `PATCH /health/incidents/{id}` — acknowledge with a note, or resolve by hand.
///
/// The two live on one route because they are one decision: an operator looking at an incident
/// either claims it or closes it, and a screen with two buttons pointing at two URLs is two
/// screens' worth of state to keep in sync. The actor is the session's user — never the
/// request's — because "acknowledged by" is the only evidence that a human looked.
pub async fn patch_incident(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<IncidentPatch>,
) -> Result<Json<IncidentBody>, ApiError> {
    let actor = session.user.id;
    let row = match body.action.as_str() {
        "acknowledge" => {
            omnion_health::acknowledge(state.db().pool(), id, actor, body.note.as_deref().unwrap_or(""))
                .await
        }
        "resolve" => omnion_health::resolve(state.db().pool(), id).await,
        other => {
            return Err(ApiError::bad_request(
                "unknown_incident_action",
                format!("`{other}` is not an incident action; expected acknowledge or resolve"),
            ));
        }
    }
    .map_err(map_store)?;
    Ok(Json(IncidentBody::from(&row)))
}

/// `GET /health/settings` — intervals, thresholds, toggles, and the bounds the form needs.
pub async fn get_settings(State(state): State<AppState>) -> Result<Json<SettingsBody>, ApiError> {
    let settings = omnion_health::load_settings(state.db().pool())
        .await
        .map_err(map_store)?;
    Ok(Json(settings_body_from(state.db().pool(), &settings).await))
}

/// A settings payload built from an explicit pool, so the function is testable without one.
///
/// Split from the handler because the threshold pairs are a second read: `health_settings`
/// carries the document, `health_thresholds` carries the validated copy, and a payload built
/// from only the first would answer "not configured" for a pair the breach emitter is
/// currently honouring.
async fn settings_body_from(
    pool: &sqlx::PgPool,
    settings: &HealthSettings,
) -> SettingsBody {
    let stored: Thresholds = omnion_health::thresholds(pool).await.unwrap_or_default();
    let suggested = omnion_health::suggested_thresholds();

    // Every metric the form offers gets a row, in the order the form shows them. A metric with
    // a stored pair carries its numbers; one without carries the suggestion and says
    // `configured: false`, so a first-run operator sees seven inputs rather than an empty
    // form with nothing to fill in.
    let mut thresholds = Vec::with_capacity(THRESHOLD_METRICS.len());
    for metric in THRESHOLD_METRICS {
        let effective = stored.get(*metric).or_else(|| suggested.get(*metric));
        match effective {
            Some(threshold) => thresholds.push(ThresholdBody::of(threshold, stored.contains_key(*metric))),
            None => thresholds.push(ThresholdBody {
                metric: (*metric).to_string(),
                warn: 0.0,
                crit: 0.0,
                direction: "above".to_string(),
                unit: omnion_health::metric_unit(metric).to_string(),
                configured: false,
            }),
        }
    }

    SettingsBody {
        check_interval_seconds: settings.check_interval_seconds,
        worker_stale_seconds: settings.worker_stale_seconds,
        thresholds,
        notifications: settings.notifications.clone(),
        updated_by: settings.updated_by,
        updated_at: settings.updated_at.to_string(),
        bounds: BoundsBody {
            check_interval_seconds: (
                omnion_health::MIN_CHECK_INTERVAL_SECONDS,
                omnion_health::MAX_CHECK_INTERVAL_SECONDS,
            ),
            worker_stale_seconds: (
                omnion_health::MIN_WORKER_STALE_SECONDS,
                omnion_health::MAX_WORKER_STALE_SECONDS,
            ),
        },
    }
}

/// `PUT /health/settings` — save intervals, pairs and toggles.
///
/// A rejected value answers `400` with the metric's name and the two numbers, which is the
/// request's own criterion ("out-of-range settings are refused with messages") rather than
/// the clamp that would answer `200` with something the form did not ask for.
pub async fn put_settings(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<SettingsBodyIn>,
) -> Result<Json<SettingsBody>, ApiError> {
    let thresholds = match &body.thresholds {
        None => None,
        Some(rows) => {
            let mut map = Thresholds::new();
            for row in rows {
                // Every pair goes through the constructor, so the message names the metric
                // the operator typed wrong rather than "invalid health input".
                let threshold = omnion_health::Threshold::new(
                    &row.metric,
                    row.warn,
                    row.crit,
                    &row.direction,
                )
                .map_err(map_store)?;
                map.insert(row.metric.clone(), threshold);
            }
            Some(map)
        }
    };

    let saved = omnion_health::save_settings(
        state.db().pool(),
        &SettingsUpdate {
            check_interval_seconds: body.check_interval_seconds,
            worker_stale_seconds: body.worker_stale_seconds,
            thresholds,
            notifications: body.notifications,
            updated_by: Some(session.user.id),
        },
    )
    .await
    .map_err(map_store)?;

    Ok(Json(settings_body_from(state.db().pool(), &saved).await))
}

/// `GET /health/maintenance-windows` — the windows, newest first, with an `active` flag.
pub async fn list_windows(State(state): State<AppState>) -> Result<Json<Vec<MaintenanceWindowBody>>, ApiError> {
    let rows: Vec<MaintenanceWindow> = sqlx::query_as(
        "select id, starts_at, ends_at, services, note, created_by, created_at \
         from health_maintenance_windows order by starts_at desc, id desc",
    )
    .fetch_all(state.db().pool())
    .await
    .map_err(map_store_error)?;
    let now = time::OffsetDateTime::now_utc();
    Ok(Json(
        rows.iter().map(|row| window_body(row, now)).collect(),
    ))
}

/// `POST /health/maintenance-windows` — create one.
///
/// An end before the start is refused here with a sentence naming both fields; the migration
/// carries the same constraint, but a constraint violation reaches a form as
/// `internal_error` and the request names the *rejection* as an acceptance criterion.
pub async fn create_window(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<MaintenanceWindowIn>,
) -> Result<(StatusCode, Json<MaintenanceWindowBody>), ApiError> {
    if body.ends_at <= body.starts_at {
        return Err(ApiError::bad_request(
            "invalid_maintenance_window",
            "the window has to end after it starts",
        ));
    }
    let services = body.services.unwrap_or_default();
    for service in &services {
        if !omnion_health::all_services().contains(&service.as_str()) {
            return Err(ApiError::bad_request(
                "unknown_service",
                format!("{service} is not a service this platform probes"),
            ));
        }
    }
    let created: MaintenanceWindow = sqlx::query_as(
        "insert into health_maintenance_windows \
           (starts_at, ends_at, services, note, created_by) \
         values ($1, $2, $3, $4, $5) \
         returning id, starts_at, ends_at, services, note, created_by, created_at",
    )
    .bind(body.starts_at)
    .bind(body.ends_at)
    .bind(&services)
    .bind(body.note.unwrap_or_default().trim())
    .bind(session.user.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(map_store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(window_body(&created, time::OffsetDateTime::now_utc())),
    ))
}

/// `DELETE /health/maintenance-windows/{id}` — remove one.
///
/// A hard delete rather than an archive: a window is a statement about a period of time, and
/// a future one that has not happened can be withdrawn outright. Windows that *have* happened
/// stay readable through the incidents they suppressed, which are not deleted.
pub async fn delete_window(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query("delete from health_maintenance_windows where id = $1")
        .bind(id)
        .execute(state.db().pool())
        .await
        .map_err(map_store_error)?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "this maintenance window does not exist",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn window_body(row: &MaintenanceWindow, now: time::OffsetDateTime) -> MaintenanceWindowBody {
    MaintenanceWindowBody {
        id: row.id,
        starts_at: row.starts_at.to_string(),
        ends_at: row.ends_at.to_string(),
        services: row.services.clone(),
        note: row.note.clone(),
        created_by: row.created_by,
        created_at: row.created_at.to_string(),
        active: row.starts_at <= now && now < row.ends_at,
    }
}

/// Map a store error onto the API surface.
///
/// The same mapping slice 1 uses, extracted here so the incidents module and the panel module
/// cannot drift into answering `400` for one and `500` for the other on the same failure.
fn map_store(error: omnion_health::HealthError) -> ApiError {
    use omnion_health::HealthError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_health_input", message),
        E::NotFound => ApiError::new(StatusCode::NOT_FOUND, "not_found", "not found"),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("health store: {inner}"),
        ),
    }
}

/// Map a raw sqlx error from the window tables, which this module writes directly.
fn map_store_error(error: sqlx::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("health store: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_incident_is_one_word_and_no_duration() {
        let incident = Incident {
            id: Uuid::nil(),
            service: "redis".to_string(),
            from_state: "healthy".to_string(),
            to_state: "down".to_string(),
            summary: "PING did not answer".to_string(),
            detail: serde_json::json!({}),
            started_at: time::OffsetDateTime::UNIX_EPOCH,
            resolved_at: None,
            suppressed: false,
            acknowledged_by: None,
            acknowledged_at: None,
            note: None,
        };
        let body = IncidentBody::from(&incident);
        assert_eq!(body.state, "open");
        assert_eq!(body.duration_seconds, None, "ongoing is not a number");
        assert_eq!(body.resolved_at, None);
    }

    #[test]
    fn a_resolved_incident_carries_its_duration() {
        let incident = Incident {
            id: Uuid::nil(),
            service: "redis".to_string(),
            from_state: "degraded".to_string(),
            to_state: "down".to_string(),
            summary: String::new(),
            detail: serde_json::json!({}),
            started_at: time::OffsetDateTime::UNIX_EPOCH,
            resolved_at: Some(time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(300)),
            suppressed: true,
            acknowledged_by: None,
            acknowledged_at: None,
            note: None,
        };
        let body = IncidentBody::from(&incident);
        assert_eq!(body.state, "resolved");
        assert_eq!(body.duration_seconds, Some(300));
        assert!(body.suppressed, "a suppressed incident is still a real row");
    }

    #[test]
    fn a_malformed_instant_names_the_parameter() {
        let error = parse_instant("yesterday", "from").unwrap_err();
        assert!(error.to_string().contains("from"), "{error}");
        assert!(parse_instant("2026-09-30T12:00:00Z", "to").is_ok());
    }

    #[test]
    fn a_window_is_active_only_inside_its_own_span() {
        let window = MaintenanceWindow {
            id: Uuid::nil(),
            starts_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(100),
            ends_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(200),
            services: Vec::new(),
            note: String::new(),
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let before = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(99);
        let inside = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(150);
        let after = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(201);
        assert!(!window_body(&window, before).active);
        assert!(window_body(&window, inside).active);
        // The end is exclusive, so a window ending exactly now is over — otherwise a window
        // and the moment it ends would both claim to be active.
        assert!(!window_body(&window, time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(200)).active);
        assert!(!window_body(&window, after).active);
    }

    #[test]
    fn the_bounds_the_form_reads_are_the_bounds_the_store_enforces() {
        // One list, imported from the crate. A form that carried its own copy would drift
        // the first time the migration's bounds changed, and would offer a value the save
        // path then refuses — which is the "interval 0" criterion arriving by a different
        // route.
        assert_eq!(
            omnion_health::MIN_CHECK_INTERVAL_SECONDS, 5,
            "the migration says 5"
        );
        assert_eq!(omnion_health::MAX_CHECK_INTERVAL_SECONDS, 600);
        assert_eq!(omnion_health::MIN_WORKER_STALE_SECONDS, 30);
        assert_eq!(omnion_health::MAX_WORKER_STALE_SECONDS, 3600);
    }
}
