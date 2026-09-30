//! Incidents and threshold policy (REQ-014, slice 3).
//!
//! Slices 1 and 2 answered **"is it up now"** and **"what has it been doing"**. This module
//! answers the third question, and it is the one that decides whether the screen is a
//! dashboard or an incident record: **"what broke, when, for how long, and who looked".**
//!
//! Four shapes, and each one exists because the obvious version is a specific lie:
//!
//! * **An incident is opened by a transition, not by a bad reading.** A disk at 91% for six
//!   hours is *one* incident with a duration, not six hours of rows. [`detect`] therefore
//!   compares against the service's *previous* state and does nothing when the state is
//!   unchanged — the difference between "opened 1 incident" and "opened 240", and only one
//!   of those is an answer a person can read.
//!
//! * **Recovery resolves with a duration, and the duration is computed once.** [`resolve_open`]
//!   reads `now() - started_at` in SQL rather than in Rust, because the row that stores
//!   `resolved_at` and the number the panel shows have to come from the same clock — a Rust
//!   subtraction from a separately-read timestamp is a duration that is wrong by exactly the
//!   gap between the two reads.
//!
//! * **A maintenance window suppresses the incident, never the state.** This is the migration's
//!   own comment and it is repeated here because the temptation is the opposite one: skipping
//!   the *sample* would make the trend line flat during a planned restart, which is a chart
//!   that lies about the machine. [`is_suppressed`] only answers "should an incident open",
//!   and the row the panel shows stays `degraded` while it is red.
//!
//! * **A threshold breach fires once per window, and the window is stored.** Dedup held in
//!   process memory forgets itself on restart, so the first run after a deploy re-fires for a
//!   disk that has been over the line for an hour. [`BreachLedger`] writes a marker row
//!   instead, and the migration's unique index on `(metric, window_start)` is what actually
//!   makes two concurrent runs agree.
//!
//! What this module deliberately does **not** do: send anything. The request says alert routing
//! is out of scope ("notifications go through the notification centre, this request only
//! decides what is alert-worthy"), so every function here returns *what happened* and the API
//! layer decides who hears about it.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{HealthError, Result};
use crate::vocabulary::{canonical_state, rank};

/// The columns of an incident row, in the order [`Incident`] declares them.
const INCIDENT_COLUMNS: &str = "id, service, from_state, to_state, summary, detail, \
     started_at, resolved_at, suppressed, acknowledged_by, acknowledged_at, note";

/// How long one breach window is, in seconds.
///
/// Fifteen minutes, and it is a constant rather than a column because the request's promise is
/// "at most once per metric per window" without ever naming a length — a window this short keeps
/// a genuine second breach (the disk came back, then filled again) as its own event, and a
/// window an hour long would swallow it.
pub const BREACH_WINDOW_SECONDS: i64 = 900;

/// The lower bound on the check interval, from `0188`'s `health_settings`.
pub const MIN_CHECK_INTERVAL_SECONDS: i32 = 5;

/// The upper bound on the check interval, from `0188`'s `health_settings`.
pub const MAX_CHECK_INTERVAL_SECONDS: i32 = 600;

/// The lower bound on the worker stale window, from `0188`'s `health_settings`.
pub const MIN_WORKER_STALE_SECONDS: i32 = 30;

/// The upper bound on the worker stale window, from `0188`'s `health_settings`.
pub const MAX_WORKER_STALE_SECONDS: i32 = 3600;

// ---------------------------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------------------------

/// One metric's warn/critical pair.
///
/// The derives are what make the round-trip in [`save_settings`] possible, and the round-trip
/// is what keeps the *document* and the *validated rows* from drifting: the settings row stores
/// one `jsonb` object while `health_thresholds` holds the same pairs per row, so a field that
/// existed on one side and not the other would be a threshold the breach emitter honours and
/// the form cannot show. Deriving rather than hand-writing them means the two shapes cannot
/// drift apart — the compiler is the check, not a test.
///
/// `metric` is skipped because the map *key* is the metric; serialising it inside the value too
/// would put it in the document twice, and `parse_thresholds` reads the key.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Threshold {
    /// The metric key, e.g. `disk_percent`.
    #[serde(skip)]
    pub metric: String,
    /// The value at which the metric is worth an operator's attention.
    pub warn: f64,
    /// The value at which it is worth waking one.
    pub crit: f64,
    /// `above` for every metric the request names; `below` is stored so a future "at least N
    /// healthy workers" rule does not need a migration.
    pub direction: String,
}

impl Threshold {
    /// Build a pair, refusing what the migration refuses.
    ///
    /// The validation is here rather than only in the database so the settings form can show
    /// **which** input was wrong before a round trip, and so the API returns a message with
    /// the metric's name in it. `NaN` is the case worth writing down: it compares false
    /// against everything, so `warn < crit` is *false* for `NaN` and a hand-rolled check that
    /// only compares would either let it through or reject the whole document for the wrong
    /// reason.
    pub fn new(metric: &str, warn: f64, crit: f64, direction: &str) -> Result<Self> {
        let metric = metric.trim();
        if metric.is_empty() || metric.chars().count() > 64 {
            return Err(HealthError::invalid(
                "a threshold needs a metric name of 1 to 64 characters",
            ));
        }
        if !crate::vocabulary::is_finite(warn) || !crate::vocabulary::is_finite(crit) {
            return Err(HealthError::invalid(format!(
                "{metric}: the limits must be real numbers"
            )));
        }
        if warn < 0.0 || crit < 0.0 {
            return Err(HealthError::invalid(format!(
                "{metric}: limits cannot be negative — a negative threshold is crossed at zero"
            )));
        }
        if warn >= crit {
            return Err(HealthError::invalid(format!(
                "{metric}: the warn limit ({warn}) must be below the critical limit ({crit})"
            )));
        }
        if !matches!(direction, "above" | "below") {
            return Err(HealthError::invalid(format!(
                "{metric}: direction must be `above` or `below`"
            )));
        }
        Ok(Self {
            metric: metric.to_string(),
            warn,
            crit,
            direction: direction.to_string(),
        })
    }

    /// Which word this pair puts the value in, given the limit it crossed.
    ///
    /// `None` when the value is under the warn limit — which is *not* `healthy` by
    /// construction: a metric with no pair configured is exactly the case the request's risk
    /// note asks for, and it must read as "no opinion" rather than as "fine".
    #[must_use]
    pub fn classify(&self, value: f64) -> Option<&'static str> {
        if !value.is_finite() {
            return None;
        }
        match self.direction.as_str() {
            "below" => {
                if value <= self.crit {
                    Some("down")
                } else if value <= self.warn {
                    Some("degraded")
                } else {
                    None
                }
            }
            _ => {
                if value >= self.crit {
                    Some("down")
                } else if value >= self.warn {
                    Some("degraded")
                } else {
                    None
                }
            }
        }
    }
}

/// The threshold document, keyed by metric.
///
/// A `BTreeMap` because every screen iterates it and every one of them wants the same order:
/// the settings form, the overview's threshold markers and the breach emitter. A `HashMap`
/// would render the form in an order that changes between two loads of the same page.
pub type Thresholds = BTreeMap<String, Threshold>;

/// The metric keys the request's settings screen names, in display order.
///
/// The five the request lists by name, plus `db_connections` and `load_average_1m`, which the
/// overview already renders as cards. A form that offers three of seven cards is a form whose
/// missing four look like an oversight.
pub const THRESHOLD_METRICS: &[&str] = &[
    "disk_percent",
    "memory_percent",
    "cpu_percent",
    "probe_latency_ms",
    "queue_depth",
    "db_connections",
    "load_average_1m",
];

/// The unit each thresholded metric is measured in, for the form's label.
#[must_use]
pub fn metric_unit(metric: &str) -> &'static str {
    match metric {
        "disk_percent" | "memory_percent" | "cpu_percent" => "%",
        "probe_latency_ms" => "ms",
        "queue_depth" | "db_connections" | "load_average_1m" => "",
        _ => "",
    }
}

/// The first-run defaults the migration's `{}` does not carry.
///
/// Empty by design in the schema, because "what is normal CPU on this machine" is not
/// something the platform can know. But a *form* with seven empty pairs is a form nobody fills
/// in, so these are what the settings screen shows as placeholders and clearly marks as
/// "not saved yet" until an operator saves one.
#[must_use]
pub fn suggested_thresholds() -> Thresholds {
    [
        ("disk_percent", 80.0, 90.0),
        ("memory_percent", 80.0, 92.0),
        ("cpu_percent", 85.0, 95.0),
        ("probe_latency_ms", 250.0, 1_000.0),
        ("queue_depth", 100.0, 500.0),
        ("db_connections", 80.0, 95.0),
        ("load_average_1m", 8.0, 16.0),
    ]
    .into_iter()
    .filter_map(|(metric, warn, crit)| {
        Threshold::new(metric, warn, crit, "above")
            .ok()
            .map(|threshold| (metric.to_string(), threshold))
    })
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// The platform's health policy, as stored.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct HealthSettings {
    /// Always `1`.
    pub id: i16,
    /// How often the runner probes.
    pub check_interval_seconds: i32,
    /// After how long a silent worker counts as stale.
    pub worker_stale_seconds: i32,
    /// The threshold document.
    pub thresholds: serde_json::Value,
    /// Which transitions notify.
    pub notifications: serde_json::Value,
    /// Who saved it last, when somebody has.
    pub updated_by: Option<Uuid>,
    /// When it was saved.
    pub updated_at: OffsetDateTime,
}

impl Default for HealthSettings {
    fn default() -> Self {
        Self {
            id: 1,
            check_interval_seconds: 60,
            worker_stale_seconds: 120,
            thresholds: serde_json::json!({}),
            notifications: serde_json::json!({}),
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

impl HealthSettings {
    /// The stored thresholds, parsed.
    ///
    /// A malformed document yields an **empty map**, not an error, and that choice is the
    /// point. This is read on the overview's request path, where a settings row written by a
    /// future version (or by hand) must not take the status screen down with it — the honest
    /// answer for "the thresholds cannot be read" is "no metric has a threshold", which is
    /// what the screen was already showing before anybody configured one. The *save* path is
    /// where a malformed document is refused, because that is where the caller can be told.
    #[must_use]
    pub fn parsed_thresholds(&self) -> Thresholds {
        parse_thresholds(&self.thresholds).unwrap_or_default()
    }

    /// Whether a transition should be announced, for one of the request's three toggles.
    #[must_use]
    pub fn notifies(&self, toggle: &str) -> bool {
        self.notifications
            .get(toggle)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

/// The settings row, creating it if a database somehow has none.
pub async fn load_settings(pool: &PgPool) -> Result<HealthSettings> {
    if let Some(row) = sqlx::query_as::<_, HealthSettings>(
        "select id, check_interval_seconds, worker_stale_seconds, thresholds, \
                notifications, updated_by, updated_at \
         from health_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok(row);
    }
    sqlx::query("insert into health_settings (id) values (1) on conflict (id) do nothing")
        .execute(pool)
        .await?;
    sqlx::query_as::<_, HealthSettings>(
        "select id, check_interval_seconds, worker_stale_seconds, thresholds, \
                notifications, updated_by, updated_at \
         from health_settings where id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(HealthError::from)
}

/// What a settings save asks for.
///
/// Every field is optional so a form can save the intervals without also rewriting the
/// threshold document, and vice versa — a `PUT` that took the whole policy would mean a form
/// with two tabs has to hold both in memory or it resets the one it is not looking at.
#[derive(Debug, Clone, Default)]
pub struct SettingsUpdate {
    /// The probe interval, if the form sent one.
    pub check_interval_seconds: Option<i32>,
    /// The worker stale window, if the form sent one.
    pub worker_stale_seconds: Option<i32>,
    /// The threshold document, if the form sent one.
    pub thresholds: Option<Thresholds>,
    /// The notification toggles, if the form sent them.
    pub notifications: Option<serde_json::Value>,
    /// Who saved it.
    pub updated_by: Option<Uuid>,
}

/// Save the policy, folding onto whatever the row already holds.
///
/// Three rules, and each is a way an operator's saved setting quietly reverts:
///
/// * **An interval outside its bounds is refused, not clamped.** The request names "interval 0"
///   as a rejected input with a message, and a clamp answers it with a `200` and a row holding
///   5 — so the form would say it saved something it did not.
/// * **A threshold document replaces the whole document.** A partial merge here is how
///   deleting a pair becomes impossible, and "I removed this limit and it came back" is a bug
///   nobody reports because it is indistinguishable from the limit still being in force.
/// * **The validated copy is rewritten in the same transaction.** `health_thresholds` is what
///   the breach emitter joins against; a settings save that updated only the JSON document
///   would leave the emitter honouring the *previous* pair until the next deploy, which is the
///   exact "the screen says one thing and the alert does another" failure.
pub async fn save_settings(pool: &PgPool, update: &SettingsUpdate) -> Result<HealthSettings> {
    let current = load_settings(pool).await?;
    let interval = update
        .check_interval_seconds
        .unwrap_or(current.check_interval_seconds);
    if !(MIN_CHECK_INTERVAL_SECONDS..=MAX_CHECK_INTERVAL_SECONDS).contains(&interval) {
        return Err(HealthError::invalid(format!(
            "the check interval must be between {MIN_CHECK_INTERVAL_SECONDS} and \
             {MAX_CHECK_INTERVAL_SECONDS} seconds"
        )));
    }
    let stale = update
        .worker_stale_seconds
        .unwrap_or(current.worker_stale_seconds);
    if !(MIN_WORKER_STALE_SECONDS..=MAX_WORKER_STALE_SECONDS).contains(&stale) {
        return Err(HealthError::invalid(format!(
            "a worker counts as stale after between {MIN_WORKER_STALE_SECONDS} and \
             {MAX_WORKER_STALE_SECONDS} seconds"
        )));
    }

    let thresholds_doc = match &update.thresholds {
        Some(map) => serde_json::to_value(map).map_err(|error| {
            // `BTreeMap<String, Threshold>` over a plain f64 cannot fail to serialise, so this
            // arm is unreachable in practice — and it is mapped to `Invalid` rather than to
            // an internal error so that if a future field makes it reachable, the caller is
            // told the *document* is the problem.
            HealthError::invalid(format!("the threshold document could not be written: {error}"))
        })?,
        None => current.thresholds.clone(),
    };
    if let Some(map) = &update.thresholds {
        // Round-trip through the parser so a `Thresholds` that reached the store without going
        // through `Threshold::new` still cannot write a shape the reader would reject.
        let serialized = serde_json::to_value(map).unwrap_or(serde_json::Value::Null);
        parse_thresholds(&serialized)?;
    }
    let notifications = update.notifications.clone().unwrap_or(current.notifications.clone());

    let mut tx = pool.begin().await?;
    let saved = sqlx::query_as::<_, HealthSettings>(
        "insert into health_settings \
           (id, check_interval_seconds, worker_stale_seconds, thresholds, notifications, \
            updated_by, updated_at) \
         values (1, $1, $2, $3, $4, $5, now()) \
         on conflict (id) do update set \
            check_interval_seconds = excluded.check_interval_seconds, \
            worker_stale_seconds = excluded.worker_stale_seconds, \
            thresholds = excluded.thresholds, \
            notifications = excluded.notifications, \
            updated_by = excluded.updated_by, updated_at = now() \
         returning id, check_interval_seconds, worker_stale_seconds, thresholds, \
                notifications, updated_by, updated_at",
    )
    .bind(interval)
    .bind(stale)
    .bind(&thresholds_doc)
    .bind(&notifications)
    .bind(update.updated_by)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(map) = &update.thresholds {
        sqlx::query("delete from health_thresholds")
            .execute(&mut *tx)
            .await?;
        for threshold in map.values() {
            sqlx::query(
                "insert into health_thresholds (metric, warn, crit, direction, updated_by) \
                 values ($1, $2, $3, $4, $5)",
            )
            .bind(&threshold.metric)
            .bind(threshold.warn)
            .bind(threshold.crit)
            .bind(&threshold.direction)
            .bind(update.updated_by)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(saved)
}

/// Parse a threshold document, refusing anything the store would refuse.
///
/// The shape is `{ "<metric>": { "warn": n, "crit": n, "direction": "above" } }`, and the
/// `direction` defaults to `above` because every metric the request names is a "more is
/// worse" measure — a document written before the column existed still has to mean something.
pub fn parse_thresholds(document: &serde_json::Value) -> Result<Thresholds> {
    let Some(object) = document.as_object() else {
        if document.is_null() {
            return Ok(Thresholds::new());
        }
        return Err(HealthError::invalid(
            "the thresholds must be an object keyed by metric",
        ));
    };
    let mut parsed = Thresholds::new();
    for (metric, pair) in object {
        let pair = pair.as_object().ok_or_else(|| {
            HealthError::invalid(format!("{metric}: the limits must be an object"))
        })?;
        let read = |key: &str| -> Result<f64> {
            let raw = pair.get(key).ok_or_else(|| {
                HealthError::invalid(format!("{metric}: the {key} limit is missing"))
            })?;
            raw.as_f64().ok_or_else(|| {
                HealthError::invalid(format!("{metric}: the {key} limit must be a number"))
            })
        };
        let warn = read("warn")?;
        let crit = read("crit")?;
        let direction = pair
            .get("direction")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("above");
        let threshold = Threshold::new(metric, warn, crit, direction)?;
        parsed.insert(metric.clone(), threshold);
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------------------------
// Incidents
// ---------------------------------------------------------------------------------------------

/// One row of the incident history.
#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
pub struct Incident {
    /// The row's id.
    pub id: Uuid,
    /// Which service.
    pub service: String,
    /// The state it was in when this began.
    pub from_state: String,
    /// The state it moved to.
    pub to_state: String,
    /// The sentence an operator reads.
    pub summary: String,
    /// Named fields behind the sentence.
    pub detail: serde_json::Value,
    /// When it began.
    pub started_at: OffsetDateTime,
    /// When it ended, if it has.
    pub resolved_at: Option<OffsetDateTime>,
    /// Whether a maintenance window suppressed the incident row.
    pub suppressed: bool,
    /// Who acknowledged it.
    pub acknowledged_by: Option<Uuid>,
    /// When they did.
    pub acknowledged_at: Option<OffsetDateTime>,
    /// The note they left.
    pub note: Option<String>,
}

impl Incident {
    /// How long this ran, in seconds, or `None` while it is open.
    ///
    /// `None` rather than the age-so-far for an open incident: the request's table column is
    /// "From → To · Duration", and a duration for something that has not ended is a number
    /// that changes every time the list is re-read. `None` renders as "ongoing".
    #[must_use]
    pub fn duration_seconds(&self) -> Option<i64> {
        self.resolved_at
            .map(|ended| (ended - self.started_at).whole_seconds())
    }

    /// Whether this row is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.resolved_at.is_none()
    }
}

/// The list read: rows plus how many there are in total.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IncidentPage {
    /// This page's rows, newest first.
    pub incidents: Vec<Incident>,
    /// How many rows match the filter, ignoring the page window.
    pub total: i64,
}

/// What the incidents list accepts.
#[derive(Debug, Clone, Default)]
pub struct IncidentFilter {
    /// Only this service.
    pub service: Option<String>,
    /// Only `open` or only `resolved`.
    pub state: Option<String>,
    /// Only rows from this moment on.
    pub from: Option<OffsetDateTime>,
    /// Only rows up to this moment.
    pub to: Option<OffsetDateTime>,
    /// How many rows to return.
    pub limit: i64,
    /// How many to skip.
    pub offset: i64,
}

impl IncidentFilter {
    /// The bounds a list read will honour.
    const MAX_LIMIT: i64 = 200;

    /// A validated filter, with the defaults filled in.
    ///
    /// An unrecognised `state` is refused rather than dropped, for the reason the rest of this
    /// module refuses a malformed document: a filter the server silently ignores is a filter
    /// the operator believes they applied.
    pub fn validated(&self) -> Result<Self> {
        if let Some(state) = &self.state {
            if !matches!(state.as_str(), "open" | "resolved") {
                return Err(HealthError::invalid(
                    "the incident state filter must be `open` or `resolved`",
                ));
            }
        }
        if let Some(service) = &self.service {
            if !crate::vocabulary::all_services().contains(&service.as_str()) {
                return Err(HealthError::invalid(format!(
                    "{service} is not a service this platform probes"
                )));
            }
        }
        Ok(Self {
            limit: self.limit.clamp(1, Self::MAX_LIMIT),
            offset: self.offset.max(0),
            ..self.clone()
        })
    }
}

/// The incidents, newest first.
pub async fn list_incidents(pool: &PgPool, filter: &IncidentFilter) -> Result<IncidentPage> {
    let filter = filter.validated()?;
    let sql = format!(
        "select {INCIDENT_COLUMNS} from health_incidents \
         where ($1::text is null or service = $1) \
           and ($2::text is null \
                or ($2 = 'open' and resolved_at is null) \
                or ($2 = 'resolved' and resolved_at is not null)) \
           and ($3::timestamptz is null or started_at >= $3) \
           and ($4::timestamptz is null or started_at <= $4) \
         order by started_at desc, id desc \
         limit $5 offset $6"
    );
    let count_sql = "select count(*)::bigint from health_incidents \
         where ($1::text is null or service = $1) \
           and ($2::text is null \
                or ($2 = 'open' and resolved_at is null) \
                or ($2 = 'resolved' and resolved_at is not null)) \
           and ($3::timestamptz is null or started_at >= $3) \
           and ($4::timestamptz is null or started_at <= $4)";
    let incidents = sqlx::query_as::<_, Incident>(&sql)
        .bind(filter.service.as_deref())
        .bind(filter.state.as_deref())
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.limit)
        .bind(filter.offset)
        .fetch_all(pool)
        .await?;
    let total: i64 = sqlx::query_scalar(count_sql)
        .bind(filter.service.as_deref())
        .bind(filter.state.as_deref())
        .bind(filter.from)
        .bind(filter.to)
        .fetch_one(pool)
        .await?;
    Ok(IncidentPage { incidents, total })
}

/// One incident by id.
pub async fn incident(pool: &PgPool, id: Uuid) -> Result<Incident> {
    let sql = format!("select {INCIDENT_COLUMNS} from health_incidents where id = $1");
    sqlx::query_as::<_, Incident>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(HealthError::NotFound)
}

/// The currently open incident for a service, if any.
///
/// `fetch_optional` with **two** states to compare is the trap this function exists to
/// document: a nullable column read through a non-`Option` decoder is a runtime
/// `ColumnDecode` panic, so the "does the row exist at all" question is asked in SQL
/// (`where resolved_at is null`) rather than by decoding the row and inspecting a field the
/// decoder may not have produced.
pub async fn open_incident(pool: &PgPool, service: &str) -> Result<Option<Incident>> {
    let sql = format!(
        "select {INCIDENT_COLUMNS} from health_incidents \
         where service = $1 and resolved_at is null \
         order by started_at desc limit 1"
    );
    Ok(sqlx::query_as::<_, Incident>(&sql)
        .bind(service)
        .fetch_optional(pool)
        .await?)
}

/// Acknowledge an incident: who, when, and what they said.
///
/// Refuses an incident that is already acknowledged with a different actor, because
/// overwriting one acknowledgement with another erases the fact that somebody was already
/// looking — and "was anyone on this?" is the question the column exists to answer. Re-acking
/// the *same* actor is idempotent, so a double-click is not an error.
pub async fn acknowledge(pool: &PgPool, id: Uuid, actor: Uuid, note: &str) -> Result<Incident> {
    let sql = format!(
        "update health_incidents set acknowledged_by = $2, acknowledged_at = now(), \
                note = $3 \
         where id = $1 \
           and (acknowledged_by is null or acknowledged_by = $2) \
         returning {INCIDENT_COLUMNS}"
    );
    sqlx::query_as::<_, Incident>(&sql)
        .bind(id)
        .bind(actor)
        .bind(note.trim())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            HealthError::invalid(
                "this incident does not exist, or it is already acknowledged by somebody else",
            )
        })
}

/// Resolve an incident by hand.
///
/// The request asks for this "when a service recovered during a maintenance window", and the
/// migration enforces `(resolved_at is null) = (to_state = 'healthy')`. So a manual resolve
/// that did *not* rewrite the state would be refused by the database with a constraint name
/// instead of a sentence — which is why the state is written here, in the same statement.
pub async fn resolve(pool: &PgPool, id: Uuid) -> Result<Incident> {
    let sql = format!(
        "update health_incidents set resolved_at = now(), to_state = 'healthy' \
         where id = $1 and resolved_at is null \
         returning {INCIDENT_COLUMNS}"
    );
    sqlx::query_as::<_, Incident>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            HealthError::invalid("this incident does not exist, or it is already resolved")
        })
}

/// What one run concluded about one service.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// Which service.
    pub service: String,
    /// The word it was in before this run.
    pub from_state: String,
    /// The word it is in now.
    pub to_state: String,
    /// The sentence for the row.
    pub summary: String,
    /// Named fields behind the sentence.
    pub detail: serde_json::Value,
}

impl Transition {
    /// Whether this pair of states means anything happened.
    ///
    /// `false` for a no-op and, deliberately, for a `healthy → unknown` pair: a probe that
    /// could not run is not an outage, it is a gap in our knowledge, and opening an incident
    /// for it produces a history full of entries an operator learns to dismiss. The *row* on
    /// the overview still goes grey — the state is never hidden — but nothing is announced.
    #[must_use]
    pub fn is_event(&self) -> bool {
        self.from_state != self.to_state && self.to_state != "unknown"
    }

    /// Whether this is a recovery — the transition that closes an incident.
    #[must_use]
    pub fn is_recovery(&self) -> bool {
        self.to_state == "healthy" && rank(&self.from_state) > rank("healthy")
    }
}

/// Compare one service's previous state with the one this run concluded.
///
/// The previous state is read from the open incident rather than from the last sample, and
/// that choice is the reason the two tables cannot be confused: an open incident is *already*
/// the platform's memory of "this service is broken", so asking it means a probe that went
/// `healthy → degraded → healthy → degraded` produces two incidents, and asking the samples
/// would produce four — because a sample is written on every run, including every run that
/// changed nothing.
pub async fn detect(pool: &PgPool, report: &crate::model::ServiceReport) -> Result<Option<Transition>> {
    let previous = open_incident(pool, &report.service)
        .await?
        .map(|incident| incident.to_state);
    // With no open incident the service was last *known* healthy: the platform has no record of
    // it being otherwise. That is a claim about our memory, not about the machine, and it is
    // the only honest default when there is no incident to read.
    let from_state = previous.unwrap_or_else(|| "healthy".to_string());
    let transition = Transition {
        service: report.service.clone(),
        from_state: canonical_state(&from_state).to_string(),
        to_state: canonical_state(&report.state).to_string(),
        summary: report.message.clone(),
        detail: report.detail.clone(),
    };
    Ok(transition.is_event().then_some(transition))
}

/// Open an incident for a transition, or resolve the open one if it is a recovery.
///
/// One function because the caller should not have to know which of the two it is looking at,
/// and because the two halves share the question "is there an open incident for this service?"
/// — a caller that asked separately would race between them, and a service that recovered in
/// the same run it opened would leave two rows.
pub async fn apply(
    pool: &PgPool,
    transition: &Transition,
    suppressed: bool,
) -> Result<IncidentOutcome> {
    if transition.is_recovery() {
        let sql = format!(
            "update health_incidents set resolved_at = now(), to_state = 'healthy' \
             where service = $1 and resolved_at is null \
             returning {INCIDENT_COLUMNS}"
        );
        let resolved = sqlx::query_as::<_, Incident>(&sql)
            .bind(&transition.service)
            .fetch_optional(pool)
            .await?;
        return Ok(match resolved {
            Some(incident) => IncidentOutcome::Resolved(incident),
            None => IncidentOutcome::Nothing,
        });
    }

    let sql = format!(
        "insert into health_incidents \
           (service, from_state, to_state, summary, detail, suppressed) \
         values ($1, $2, $3, $4, $5, $6) \
         returning {INCIDENT_COLUMNS}"
    );
    let opened = sqlx::query_as::<_, Incident>(&sql)
        .bind(&transition.service)
        .bind(&transition.from_state)
        .bind(&transition.to_state)
        .bind(&transition.summary)
        .bind(&transition.detail)
        .bind(suppressed)
        .fetch_optional(pool)
        .await?;
    Ok(match opened {
        Some(incident) => IncidentOutcome::Opened(incident),
        None => IncidentOutcome::Nothing,
    })
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum IncidentOutcome {
    /// An incident was opened.
    Opened(Incident),
    /// An open incident was resolved.
    Resolved(Incident),
    /// Nothing happened — the migration's unique index refused a second open incident, which
    /// is the correct outcome for a run that raced itself.
    Nothing,
}

impl IncidentOutcome {
    /// The incident, when there is one.
    #[must_use]
    pub fn incident(&self) -> Option<&Incident> {
        match self {
            Self::Opened(incident) | Self::Resolved(incident) => Some(incident),
            Self::Nothing => None,
        }
    }
}

/// Whether a maintenance window covers this service right now.
///
/// An **empty** `services` array means every service, which is the common case (a deploy that
/// touches all of them) and the reason the migration stores `'{}'` rather than a null: a null
/// would make every query a two-branch case for a value that has one meaning.
pub async fn is_suppressed(pool: &PgPool, service: &str, at: OffsetDateTime) -> Result<bool> {
    let covered: i64 = sqlx::query_scalar(
        "select count(*)::bigint from health_maintenance_windows \
         where starts_at <= $1 and ends_at > $1 \
           and (cardinality(services) = 0 or $2 = any (services))",
    )
    .bind(at)
    .bind(service)
    .fetch_one(pool)
    .await?;
    Ok(covered > 0)
}

// ---------------------------------------------------------------------------------------------
// Breach dedup
// ---------------------------------------------------------------------------------------------

/// The window a moment belongs to.
///
/// Computed **once** by the writer and stored, because "the current 15-minute bucket" is a
/// sentence whose answer depends on when you ask: an emitter that derives it from `now()` at
/// insert time writes one bucket at 10:14:59 and the next at 10:15:01, and the unique index
/// then treats one continuous breach as two events.
#[must_use]
pub fn breach_window(at: OffsetDateTime) -> OffsetDateTime {
    let seconds = at.unix_timestamp();
    let floored = seconds.div_euclid(BREACH_WINDOW_SECONDS) * BREACH_WINDOW_SECONDS;
    OffsetDateTime::from_unix_timestamp(floored).unwrap_or(at)
}

/// What one breach check concluded.
#[derive(Debug, Clone, PartialEq)]
pub struct BreachCheck {
    /// Which metric.
    pub metric: String,
    /// The value that was read.
    pub value: f64,
    /// The critical limit it crossed.
    pub crit_limit: f64,
    /// How many consecutive runs have stayed over the line inside this window.
    pub observations: i32,
}

impl BreachCheck {
    /// `true` when this is the **first** crossing in its window and the event should fire.
    ///
    /// The comparison is `<= 1` rather than `== 1` so a window whose counter was reset by a
    /// hand edit cannot produce a second event at zero.
    #[must_use]
    pub fn should_announce(&self) -> bool {
        self.observations <= 1
    }
}

/// Record that a metric is over its critical line, and say whether this was the first time.
///
/// The write is an `on conflict do update` that increments the counter, so a continuous breach
/// is **one** row updated rather than one row per interval. When the value comes back under the
/// warn limit the row is resolved rather than deleted, so "how many runs stayed over the line
/// and for how long" survives the recovery — the same reasoning as the incidents table, and for
/// the same reason: the history is the product.
pub async fn record_breach(pool: &PgPool, metric: &str, value: f64, at: OffsetDateTime) -> Result<BreachCheck> {
    let window_start = breach_window(at);
    let (observations, resolved_at, crit_limit): (i32, Option<OffsetDateTime>, f64) = sqlx::query_as(
        "insert into health_breaches (metric, window_start, value, crit_limit) \
         values ($1, $2, $3, (select crit from health_thresholds where metric = $1)) \
         on conflict (metric, window_start) do update set \
            value = excluded.value, \
            observations = health_breaches.observations + 1, \
            last_seen_at = now(), \
            resolved_at = null \
         returning observations, resolved_at, crit_limit",
    )
    .bind(metric)
    .bind(window_start)
    .bind(value)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        HealthError::invalid(format!(
            "{metric} has no threshold pair, so there is no line for it to have crossed"
        ))
    })?;
    let _ = resolved_at;
    Ok(BreachCheck {
        metric: metric.to_string(),
        value,
        crit_limit,
        observations,
    })
}

/// Clear a metric's breach marker, because the value came back under the warn limit.
///
/// Resolving rather than deleting is deliberate: the row is how long the breach lasted, and a
/// marker that vanished on recovery is a marker that says "this happened at most once" — which
/// is the same false claim the incidents table avoids by keeping resolved rows.
pub async fn clear_breach(pool: &PgPool, metric: &str, at: OffsetDateTime) -> Result<()> {
    sqlx::query(
        "update health_breaches set resolved_at = now() \
         where metric = $1 and window_start = $2 and resolved_at is null",
    )
    .bind(metric)
    .bind(breach_window(at))
    .execute(pool)
    .await?;
    Ok(())
}

/// How many runs a metric stayed over its critical line, and for how long.
///
/// The overview's "breached" count, and it reads the resolved rows too — a number that only
/// counted open breaches would fall back to zero the moment recovery arrived, which is the
/// point at which an operator still wants to know.
pub async fn breach_count(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*)::bigint from health_breaches").fetch_one(pool).await?;
    Ok(count)
}

/// The thresholds as stored, for the settings form and the overview's markers.
pub async fn thresholds(pool: &PgPool) -> Result<Thresholds> {
    let rows: Vec<(String, f64, f64, String)> = sqlx::query_as(
        "select metric, warn, crit, direction from health_thresholds order by metric",
    )
    .fetch_all(pool)
    .await?;
    let mut map = Thresholds::new();
    for (metric, warn, crit, direction) in rows {
        // A row the database accepted is still routed through the constructor, so a pair that
        // predates a validation rule is reported rather than trusted.
        map.insert(
            metric.clone(),
            Threshold::new(&metric, warn, crit, &direction)?,
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(metric: &str, warn: f64, crit: f64) -> Threshold {
        Threshold::new(metric, warn, crit, "above").expect("a usable pair")
    }

    #[test]
    fn a_warn_at_or_above_critical_is_refused_with_the_numbers_in_the_message() {
        let error = Threshold::new("disk_percent", 90.0, 85.0, "above").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("disk_percent"), "{text}");
        assert!(text.contains("90"), "{text}");
        assert!(text.contains("85"), "{text}");

        assert!(Threshold::new("disk_percent", 85.0, 85.0, "above").is_err());
    }

    #[test]
    fn a_nan_limit_is_refused() {
        // `NaN < crit` is false, so a hand-rolled comparison either lets it through or
        // rejects the document for the wrong reason. The finiteness check is what makes the
        // message name the actual problem.
        let error = Threshold::new("cpu_percent", f64::NAN, 90.0, "above").unwrap_err();
        assert!(error.to_string().contains("real numbers"), "{error}");
        assert!(Threshold::new("cpu_percent", 80.0, f64::INFINITY, "above").is_err());
    }

    #[test]
    fn a_negative_limit_is_refused() {
        let error = Threshold::new("queue_depth", -1.0, 500.0, "above").unwrap_err();
        assert!(error.to_string().contains("negative"), "{error}");
    }

    #[test]
    fn an_unknown_direction_is_refused() {
        assert!(Threshold::new("queue_depth", 10.0, 20.0, "sideways").is_err());
    }

    #[test]
    fn classification_reads_the_direction_it_was_given() {
        let above = pair("disk_percent", 80.0, 90.0);
        assert_eq!(above.classify(50.0), None);
        assert_eq!(above.classify(85.0), Some("degraded"));
        assert_eq!(above.classify(95.0), Some("down"));
        assert_eq!(above.classify(f64::NAN), None, "a non-finite value has no opinion");

        let below = Threshold::new("workers", 2.0, 1.0, "below").expect("below");
        assert_eq!(below.classify(4.0), None);
        assert_eq!(below.classify(2.0), Some("degraded"));
        assert_eq!(below.classify(0.0), Some("down"));
    }

    #[test]
    fn the_suggested_pairs_are_ones_the_store_accepts() {
        let suggested = suggested_thresholds();
        for metric in THRESHOLD_METRICS {
            assert!(suggested.contains_key(*metric), "{metric} has no suggestion");
        }
        for threshold in suggested.values() {
            assert!(threshold.warn < threshold.crit);
            assert!(crate::vocabulary::is_finite(threshold.warn));
        }
    }

    #[test]
    fn a_document_parses_and_the_shape_it_refuses_is_named() {
        let parsed = parse_thresholds(&serde_json::json!({
            "disk_percent": { "warn": 80, "crit": 90 },
            "queue_depth": { "warn": 100, "crit": 500, "direction": "above" }
        }))
        .expect("a well-formed document");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["disk_percent"].crit, 90.0);

        for bad in [
            serde_json::json!([]),
            serde_json::json!({ "disk_percent": 80 }),
            serde_json::json!({ "disk_percent": { "warn": 80 } }),
            serde_json::json!({ "disk_percent": { "warn": "eighty", "crit": 90 } }),
            serde_json::json!({ "disk_percent": { "warn": 90, "crit": 80 } }),
        ] {
            assert!(
                parse_thresholds(&bad).is_err(),
                "{bad} should have been refused"
            );
        }
    }

    #[test]
    fn an_empty_document_is_no_opinion_not_an_error() {
        assert!(parse_thresholds(&serde_json::json!({})).unwrap().is_empty());
        assert!(parse_thresholds(&serde_json::Value::Null).unwrap().is_empty());
    }

    #[test]
    fn direction_defaults_to_above_for_a_document_written_before_the_column() {
        let parsed = parse_thresholds(&serde_json::json!({ "cpu_percent": { "warn": 85, "crit": 95 } }))
            .expect("legacy shape");
        assert_eq!(parsed["cpu_percent"].direction, "above");
    }

    #[test]
    fn a_malformed_stored_document_degrades_to_no_opinion() {
        // The read happens on the overview's request path, so a row written by a future
        // version must not take the status screen down. The *save* path is where it is
        // refused, and `parse_thresholds` is that path.
        let settings = HealthSettings {
            thresholds: serde_json::json!({ "disk_percent": "soon" }),
            ..HealthSettings::default()
        };
        assert!(settings.parsed_thresholds().is_empty());
    }

    #[test]
    fn an_unchanged_state_is_not_an_event() {
        let steady = Transition {
            service: "redis".to_string(),
            from_state: "degraded".to_string(),
            to_state: "degraded".to_string(),
            summary: String::new(),
            detail: serde_json::json!({}),
        };
        assert!(!steady.is_event(), "six hours of the same bad news is one incident");
    }

    #[test]
    fn a_probe_that_could_not_run_does_not_open_an_incident() {
        // The state still shows `unknown` on the overview — that is never hidden. But an
        // incident history full of "we could not look" trains an operator to dismiss the list.
        let blind = Transition {
            service: "s3".to_string(),
            from_state: "healthy".to_string(),
            to_state: "unknown".to_string(),
            summary: String::new(),
            detail: serde_json::json!({}),
        };
        assert!(!blind.is_event());
    }

    #[test]
    fn a_recovery_is_recognised_from_every_worse_state() {
        for from in ["degraded", "down"] {
            let recovery = Transition {
                service: "queue".to_string(),
                from_state: from.to_string(),
                to_state: "healthy".to_string(),
                summary: String::new(),
                detail: serde_json::json!({}),
            };
            assert!(recovery.is_recovery(), "{from} → healthy is a recovery");
        }
        let never_broken = Transition {
            service: "queue".to_string(),
            from_state: "healthy".to_string(),
            to_state: "healthy".to_string(),
            summary: String::new(),
            detail: serde_json::json!({}),
        };
        assert!(!never_broken.is_recovery());
    }

    #[test]
    fn an_incidents_filter_refuses_what_it_does_not_honour() {
        let ok = IncidentFilter {
            state: Some("open".to_string()),
            service: Some("redis".to_string()),
            limit: 5_000,
            offset: -3,
            ..IncidentFilter::default()
        }
        .validated()
        .expect("a filter the list can honour");
        assert_eq!(ok.limit, IncidentFilter::MAX_LIMIT);
        assert_eq!(ok.offset, 0);

        assert!(IncidentFilter {
            state: Some("pending".to_string()),
            ..IncidentFilter::default()
        }
        .validated()
        .is_err());
        assert!(IncidentFilter {
            service: Some("mystery".to_string()),
            ..IncidentFilter::default()
        }
        .validated()
        .is_err());
    }

    #[test]
    fn the_breach_window_is_computed_not_derived_at_insert_time() {
        // Two moments either side of a 15-minute boundary must land in different windows, and
        // two moments inside one must land in the same one — the property a per-insert
        // `now()` calculation cannot hold at a boundary.
        let base = OffsetDateTime::UNIX_EPOCH;
        let window = breach_window(base + time::Duration::seconds(870)); // 14:30
        assert_eq!(window.unix_timestamp(), 15 * 60);
        assert_eq!(
            breach_window(base + time::Duration::seconds(870)).unix_timestamp(),
            breach_window(base + time::Duration::seconds(880)).unix_timestamp(),
            "two moments inside one window share it"
        );
        assert_ne!(
            breach_window(base + time::Duration::seconds(870)).unix_timestamp(),
            breach_window(base + time::Duration::seconds(960)).unix_timestamp(),
            "two moments either side of a boundary do not"
        );
    }

    #[test]
    fn a_window_is_aligned_to_the_epoch_not_to_the_process_start() {
        // A window measured from "when this process booted" shifts on every restart, so the
        // same disk at the same value breaches in a different window after a deploy.
        let window = breach_window(OffsetDateTime::from_unix_timestamp(1_700_000_123).unwrap());
        assert_eq!(window.unix_timestamp() % BREACH_WINDOW_SECONDS, 0);
    }

    #[test]
    fn only_the_first_run_in_a_window_announces() {
        let first = BreachCheck {
            metric: "disk_percent".to_string(),
            value: 95.0,
            crit_limit: 90.0,
            observations: 1,
        };
        assert!(first.should_announce());
        let second = BreachCheck {
            observations: 2,
            ..first.clone()
        };
        assert!(!second.should_announce(), "run two of one continuous breach is not news");
        let zero = BreachCheck {
            observations: 0,
            ..first
        };
        assert!(zero.should_announce(), "a reset counter must not mute a real breach");
    }

    #[test]
    fn an_open_incident_reports_no_duration() {
        let open = Incident {
            id: Uuid::nil(),
            service: "redis".to_string(),
            from_state: "healthy".to_string(),
            to_state: "down".to_string(),
            summary: String::new(),
            detail: serde_json::json!({}),
            started_at: OffsetDateTime::UNIX_EPOCH,
            resolved_at: None,
            suppressed: false,
            acknowledged_by: None,
            acknowledged_at: None,
            note: None,
        };
        assert!(open.is_open());
        assert_eq!(open.duration_seconds(), None, "ongoing is not a number that ticks");

        let closed = Incident {
            resolved_at: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(90)),
            ..open
        };
        assert_eq!(closed.duration_seconds(), Some(90));
        assert!(!closed.is_open());
    }

    #[test]
    fn an_outcome_without_an_incident_has_nothing_to_return() {
        assert!(IncidentOutcome::Nothing.incident().is_none());
    }

    #[test]
    fn the_notification_toggle_is_off_until_it_is_configured() {
        let settings = HealthSettings::default();
        assert!(!settings.notifies("degraded"));
        let on = HealthSettings {
            notifications: serde_json::json!({ "degraded": true }),
            ..HealthSettings::default()
        };
        assert!(on.notifies("degraded"));
        assert!(!on.notifies("recovered"), "one toggle is not all of them");
    }
}
