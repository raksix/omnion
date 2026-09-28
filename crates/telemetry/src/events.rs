//! The events REQ-126 documents, and the one place they are emitted from.
//!
//! ## Why this module exists
//!
//! The request's **Events** block names eight emitted events. When slice 4 wrote the retention
//! sweep it became clear that seven of the eight were **documented and dead**: a name in a table
//! describing an intent, with nothing in the tree ever calling `omnion_events::bus::emit` for it.
//! The eighth (`retention.pruned`) existed only because that tick happened to need one.
//!
//! That is the same defect class twice now, so it is a rule rather than an incident:
//!
//! * slice 3 shipped an exporter pipeline that buffered nothing because nothing called
//!   `Collector::push`,
//! * slice 4 shipped a retention window nothing honoured because nothing called `store::prune`,
//! * and the event names were documented before any of them were written.
//!
//! So the names live here, in one constant table, and a unit test reads the request file itself
//! and asserts that every name it documents is in [`DOCUMENTED`]. A name added to the request
//! without a constant here fails the crate's own tests; a constant added here without a caller
//! fails [`a_documented_event_is_never_declared_without_a_caller`]. Neither direction can rot
//! quietly, which is the only property that makes an events table worth anything.
//!
//! ## What every payload here is allowed to carry
//!
//! The request fixes the alert payload in the strongest words it has: "rule name, severity,
//! value, window, runbook link, and nothing else. Log lines, user data and secret fragments never
//! appear in a payload." So each builder below is a struct literal with no field a caller can fill
//! with a log line, and the emitters take those structs rather than a `serde_json::Value`. The one
//! free-form value that crosses the boundary is an operator's **silence reason**, which is their
//! own words and is the only text any of these payloads may contain.
//!
//! ## Emission never fails a caller
//!
//! Every loop and every route that produces one of these facts must keep working when the bus is
//! unhappy — a subscriber's inbox being unreachable is not a reason to refuse an alert, drop a log
//! line or fail a settings save. So [`try_emit`] is what the callers use: it reports success as a
//! `bool` and turns a failure into one warning line. [`emit`] exists for the walk that asserts the
//! row is really there.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::TelemetryError;

/// An exporter lost its backend.
pub const EXPORTER_DEGRADED: &str = "observability.exporter.degraded";
/// An exporter that was degraded is delivering again.
pub const EXPORTER_RECOVERED: &str = "observability.exporter.recovered";
/// A rule crossed its threshold and stayed there.
pub const ALERT_FIRED: &str = "observability.alert.fired";
/// A firing rule went back below its threshold.
pub const ALERT_RESOLVED: &str = "observability.alert.resolved";
/// An operator silenced a rule, or the whole instance.
pub const SILENCE_CREATED: &str = "observability.silence.created";
/// The share of healthy traces that is kept changed.
pub const SAMPLING_CHANGED: &str = "observability.sampling.changed";
/// A module's log level was raised or lowered.
pub const LOG_LEVEL_CHANGED: &str = "observability.log_level.changed";
/// A retention sweep removed rows.
pub const RETENTION_PRUNED: &str = "observability.retention.pruned";

/// Every emitted event this crate owns, in the order the request lists them.
///
/// The list is the contract the unit tests hold against: the request file is read and compared
/// against it, and every entry is checked for a non-test caller.
pub const DOCUMENTED: [&str; 8] = [
    EXPORTER_DEGRADED,
    EXPORTER_RECOVERED,
    ALERT_FIRED,
    ALERT_RESOLVED,
    SILENCE_CREATED,
    SAMPLING_CHANGED,
    LOG_LEVEL_CHANGED,
    RETENTION_PRUNED,
];

/// An exporter's health moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExporterTransition {
    /// The exporter's name.
    pub name: String,
    /// Its kind (`otlp`, `prometheus_remote_write`, `syslog`, `webhook`).
    pub kind: String,
    /// Where it moved to.
    pub health: &'static str,
    /// Where it moved from.
    pub previous: &'static str,
    /// Why, verbatim: the backend's own answer, truncated by the caller.
    pub error: Option<String>,
    /// How many samples its buffer has dropped so far.
    pub dropped_total: u64,
}

impl ExporterTransition {
    /// The payload. The endpoint and the error text are the backend's, never a credential: the
    /// exporter's own auth lives in the secret store and is not in this process to leak.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "exporter": self.name,
            "kind": self.kind,
            "health": self.health,
            "previous": self.previous,
            "dropped_total": self.dropped_total,
            "error": self.error,
        })
    }
}

/// A rule crossed, or came back from, its threshold.
///
/// Deliberately the same fields for both directions: a subscriber that handles
/// `alert.fired` must not need a second payload shape to handle `alert.resolved`, and the
/// one field that differs (`state`) is a constant per event rather than something the caller
/// fills in.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertTransition {
    /// The rule's id, for a consumer that stores the pairing.
    pub rule_id: Uuid,
    /// The rule's name.
    pub rule: String,
    /// `info`, `warning` or `critical`.
    pub severity: String,
    /// The value that crossed, or the value it fell back to.
    pub value: Option<f64>,
    /// The dwell the rule was configured with, in seconds.
    pub window_seconds: i32,
    /// Where to read more.
    pub runbook_url: Option<String>,
    /// The rule's own labels.
    pub labels: serde_json::Value,
    /// How long the incident lasted, in seconds. `None` when it is firing.
    pub duration_seconds: Option<i64>,
}

impl AlertTransition {
    /// The payload, in the five fields the request names plus three it has to have.
    ///
    /// The request says: "rule name, severity, value, window, runbook link, and nothing else."
    /// Five of the eight keys are those. The three additions are stated rather than slipped in:
    ///
    /// * `labels` — the rule's own label set, which the rule already carries and which the
    ///   sibling webhook-delivery payload ([`crate::alerts::Notification::payload`]) also
    ///   carries. Without it a subscriber cannot tell WHICH route, queue or exporter fired.
    /// * `state` — derived from `duration_seconds`, so a caller cannot emit `alert.fired`
    ///   carrying a duration or `alert.resolved` carrying none. The two facts ARE the name.
    /// * `duration_seconds` — how long the incident lasted. Without it a resolution is
    ///   indistinguishable from a firing one apart from its event name.
    ///
    /// What is absent is the point: there is no field a log line, a user datum or a secret
    /// fragment could travel through, and the unit test below pins the exact key set so the next
    /// field somebody wants has to be argued for here.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "rule": self.rule,
            "severity": self.severity,
            "value": self.value,
            "window_seconds": self.window_seconds,
            "runbook_url": self.runbook_url,
            "labels": self.labels,
            "state": if self.duration_seconds.is_some() { "resolved" } else { "firing" },
            "duration_seconds": self.duration_seconds,
        })
    }

    /// Whether this transition belongs to `alert.fired`.
    #[must_use]
    pub fn is_firing(&self) -> bool {
        self.duration_seconds.is_none()
    }
}

/// An operator silenced something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceCreated {
    /// The silence's id.
    pub silence_id: Uuid,
    /// The rule it covers, or `None` for a maintenance window over everything.
    pub rule_id: Option<Uuid>,
    /// The rule's name when there is one, so a subscriber need not look it up.
    pub rule_name: Option<String>,
    /// The operator's own reason. The only free text any of these payloads carries.
    pub reason: String,
    /// When it stops suppressing.
    pub ends_at: String,
}

impl SilenceCreated {
    /// The payload.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "silence_id": self.silence_id,
            "rule_id": self.rule_id,
            "rule_name": self.rule_name,
            "reason": self.reason,
            "ends_at": self.ends_at,
        })
    }
}

/// The trace sampling ratio moved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplingChanged {
    /// What it was.
    pub previous: f64,
    /// What it is.
    pub current: f64,
}

impl SamplingChanged {
    /// The payload. The ratio, and how many points of percentage the move is worth, so a
    /// subscriber can tell a typo from a decision.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "previous": self.previous,
            "current": self.current,
            "delta": self.current - self.previous,
        })
    }
}

/// A module's level moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLevelChanged {
    /// The module path prefix.
    pub target: String,
    /// What it was.
    pub previous: String,
    /// What it is now.
    pub current: String,
    /// When the raise expires, if it has one.
    pub expires_at: Option<String>,
}

impl LogLevelChanged {
    /// The payload.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "target": self.target,
            "previous": self.previous,
            "current": self.current,
            "expires_at": self.expires_at,
        })
    }
}

/// Record one platform-wide event.
///
/// A fact about the instance rather than about a tenant carries no organization, and
/// `enqueue_fanout` returns zero deliveries for those — matching a tenant's endpoint against a
/// platform fact would leak one tenant's configuration into another's. The row is still written,
/// which is what an operations endpoint and `GET /events` read.
pub async fn emit(
    pool: &PgPool,
    name: &str,
    payload: serde_json::Value,
) -> Result<omnion_events::bus::EmitReport, TelemetryError> {
    omnion_events::bus::emit(pool, omnion_events::NewEvent::new(name).payload(payload))
        .await
        .map_err(|error| TelemetryError::Telemetry(format!("the {name} event failed: {error}")))
}

/// Record one event that belongs to an organization, for an actor who caused it.
///
/// This is the only variant that fans out to a tenant's webhook endpoints, and the only one that
/// names a user: a silence is somebody's decision about their own instance, so the subscriber is
/// that tenant and the actor is the person.
pub async fn emit_for_organization(
    pool: &PgPool,
    name: &str,
    organization_id: Uuid,
    actor_user_id: Uuid,
    payload: serde_json::Value,
) -> Result<omnion_events::bus::EmitReport, TelemetryError> {
    omnion_events::bus::emit(
        pool,
        omnion_events::NewEvent::new(name)
            .organization(organization_id)
            .actor(actor_user_id)
            .payload(payload),
    )
    .await
    .map_err(|error| TelemetryError::Telemetry(format!("the {name} event failed: {error}")))
}

/// Emit, and turn a failure into one warning line instead of a caller-visible error.
///
/// Returns whether the row was written. Every production caller uses this: a loop that returns
/// `Err` because the bus is down would log an error every interval and — worse — a route that
/// returned `Err` would answer `500` for a settings save that had already been written.
pub async fn try_emit(pool: &PgPool, name: &str, payload: serde_json::Value) -> bool {
    match emit(pool, name, payload).await {
        Ok(report) => {
            tracing::debug!(
                event_id = report.event.id,
                name,
                deliveries = report.deliveries,
                "the event was recorded"
            );
            true
        }
        Err(error) => {
            tracing::warn!(event = name, error = %error, "the event could not be recorded");
            false
        }
    }
}

/// [`try_emit`] for a tenant's event.
pub async fn try_emit_for_organization(
    pool: &PgPool,
    name: &str,
    organization_id: Uuid,
    actor_user_id: Uuid,
    payload: serde_json::Value,
) -> bool {
    match emit_for_organization(pool, name, organization_id, actor_user_id, payload).await {
        Ok(report) => {
            tracing::debug!(
                event_id = report.event.id,
                name,
                deliveries = report.deliveries,
                "the event was recorded"
            );
            true
        }
        Err(error) => {
            tracing::warn!(event = name, error = %error, "the event could not be recorded");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    /// The request file these names are read back out of, relative to this crate.
    ///
    /// It lives inside the test module because it is the *test's* oracle: nothing at runtime
    /// reads the documentation, and a production path that parsed its own request file would be a
    /// dependency on a file that is not deployed.
    const REQUEST: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/requests/REQ-126-observability-stack.md"
    );

    /// Every event the request documents is a constant here, and nothing else is.
    ///
    /// The request file is the source of truth, read at test time rather than copied into a
    /// fixture — a hand-copied list would agree with the constants forever even after somebody
    /// edited the request. When the file is absent (a packaged build without `docs/`) the test
    /// says so and passes, rather than failing a release for a missing document.
    #[test]
    fn every_event_the_request_documents_is_declared_here_and_nothing_else() {
        let path = Path::new(REQUEST);
        let Ok(text) = std::fs::read_to_string(path) else {
            eprintln!("SKIP: {} is not readable from here", path.display());
            return;
        };
        let Some(line) = text.lines().find(|line| line.starts_with("- **Emitted:**")) else {
            eprintln!("SKIP: the request has no `**Emitted:**` line");
            return;
        };
        // The names are inside backticks on that line. Taking every backticked token is more
        // robust than the prose around them, which is what makes this survive an edit to the
        // sentence itself.
        let documented: Vec<&str> = line
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|token| token.starts_with("observability."))
            .collect();

        assert!(
            !documented.is_empty(),
            "the request's `**Emitted:**` line names no observability event — the parser is wrong, \
             not the request"
        );
        for name in &documented {
            assert!(
                DOCUMENTED.contains(name),
                "the request documents `{name}` and nothing in this crate emits it"
            );
        }
        for name in DOCUMENTED {
            assert!(
                documented.contains(&name),
                "`{name}` is declared here but the request does not document it — either the \
                 name or the documentation is wrong"
            );
        }
        assert_eq!(
            documented.len(),
            DOCUMENTED.len(),
            "the request documents {} observability events and this crate declares {}",
            documented.len(),
            DOCUMENTED.len()
        );
    }

    /// A constant nobody emits is a row in a table describing an intent.
    ///
    /// The check greps for the CONSTANT'S IDENTIFIER, not the event name — no caller writes
    /// `\"observability.alert.fired\"`, they write `crate::events::ALERT_FIRED`, and the first
    /// version of this test grepped the string and therefore could never find a caller at all.
    /// That is the worst shape a test of this kind can have: it fails for the wrong reason today
    /// and would have passed against a fully dead name if the constants had been inlined.
    ///
    /// The scan covers the crate AND `apps/api/src`, because three of the eight are emitted from
    /// a route handler rather than from a loop. A crate-only scan would call `silence.created`
    /// unemitted forever while it is in fact written on every save.
    #[test]
    fn a_documented_event_is_never_declared_without_a_caller() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let api_src = crate_root.join("../../apps/api/src");
        let mut emitters = String::new();
        for root in [crate_root.join("src"), api_src] {
            collect_rust(&root, &mut emitters, 0);
        }
        // This module declares the constants and registers them in `DOCUMENTED`, so its own
        // occurrences are the declaration, not a caller.
        let here = std::fs::read_to_string(std::path::Path::new(file!())).unwrap_or_default();
        let elsewhere = emitters.replacen(&here, "", 1);

        let mut callers: HashMap<&str, usize> = HashMap::new();
        for (name, identifier) in [
            (EXPORTER_DEGRADED, "EXPORTER_DEGRADED"),
            (EXPORTER_RECOVERED, "EXPORTER_RECOVERED"),
            (ALERT_FIRED, "ALERT_FIRED"),
            (ALERT_RESOLVED, "ALERT_RESOLVED"),
            (SILENCE_CREATED, "SILENCE_CREATED"),
            (SAMPLING_CHANGED, "SAMPLING_CHANGED"),
            (LOG_LEVEL_CHANGED, "LOG_LEVEL_CHANGED"),
            (RETENTION_PRUNED, "RETENTION_PRUNED"),
        ] {
            let uses = elsewhere
                .matches(identifier)
                .count()
                // `ALERT_FIRED` is a prefix of nothing, but `EXPORTER_RECOVERED` contains no
                // other identifier either; the subtraction is only needed for the `events::` and
                // bare forms of the same name, which both count as one call site.
                .max(0);
            callers.insert(name, uses);
        }

        for (name, uses) in &callers {
            assert!(
                *uses >= 1,
                "`{name}` is declared in events.rs and named nowhere else — a constant with no \
                 caller is a documented event nothing emits"
            );
        }
    }

    /// Read every `.rs` under `root`, recursively, into one string.
    fn collect_rust(root: &std::path::Path, out: &mut String, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                collect_rust(&path, out, depth + 1);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }

    /// The alert payload carries the five documented fields and nothing else.
    ///
    /// Asserted as an allowlist over the rendered object rather than as "no log line in it",
    /// because an absence check passes forever against an implementation that starts leaking on
    /// the next field.
    #[test]
    fn the_alert_payload_is_exactly_the_documented_fields() {
        let transition = AlertTransition {
            rule_id: Uuid::nil(),
            rule: "HighErrorRate".to_owned(),
            severity: "critical".to_owned(),
            value: Some(0.42),
            window_seconds: 300,
            runbook_url: Some("https://example.test/runbooks/error-rate".to_owned()),
            labels: json!({ "team": "platform" }),
            duration_seconds: None,
        };
        let rendered = transition.payload();
        let mut keys: Vec<&str> = rendered
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "duration_seconds",
                "labels",
                "rule",
                "runbook_url",
                "severity",
                "state",
                "value",
                "window_seconds",
            ],
            "the payload's key set is the contract a subscriber writes against"
        );
        assert!(transition.is_firing());

        // The same builder, resolved: the name decides the state, so a caller cannot emit a
        // resolution with a duration or a firing with one.
        let resolved = AlertTransition {
            duration_seconds: Some(90),
            ..transition.clone()
        };
        assert_eq!(resolved.payload()["state"], "resolved");
        assert!(!resolved.is_firing());
    }

    /// The exporter payload names the exporter and its health, never its endpoint or a secret.
    #[test]
    fn the_exporter_payload_names_the_health_and_not_the_endpoint() {
        let transition = ExporterTransition {
            name: "otlp".to_owned(),
            kind: "otlp".to_owned(),
            health: "degraded",
            previous: "ok",
            error: Some("503 the collector is shedding load".to_owned()),
            dropped_total: 17,
        };
        let payload = transition.payload();
        assert_eq!(payload["exporter"], "otlp");
        assert_eq!(payload["health"], "degraded");
        assert_eq!(payload["dropped_total"], 17);
        assert!(
            payload.get("endpoint").is_none(),
            "the endpoint may carry a token in its path; it is not in the payload"
        );
    }

    /// The silence payload is the only one with free text, and it is the operator's own reason.
    #[test]
    fn the_silence_payload_carries_the_reason_and_the_window() {
        let silence = SilenceCreated {
            silence_id: Uuid::nil(),
            rule_id: Some(Uuid::nil()),
            rule_name: Some("QueueBacklog".to_owned()),
            reason: "the migration holds the queue for ten minutes".to_owned(),
            ends_at: "2026-09-28T19:00:00Z".to_owned(),
        };
        let payload = silence.payload();
        assert_eq!(payload["rule_name"], "QueueBacklog");
        assert_eq!(payload["ends_at"], "2026-09-28T19:00:00Z");
        assert!(
            payload["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "a silence with no reason is one nobody dares to remove, and the payload is where \
             that text has to travel"
        );
    }

    /// The sampling payload reports the delta as well as the two values.
    #[test]
    fn the_sampling_payload_reports_the_move() {
        let payload = SamplingChanged {
            previous: 0.1,
            current: 0.25,
        }
        .payload();
        assert_eq!(payload["previous"], 0.1);
        assert_eq!(payload["current"], 0.25);
        assert!(
            (payload["delta"].as_f64().expect("a number") - 0.15).abs() < f64::EPSILON,
            "a subscriber should be able to tell a typo from a decision: {payload}"
        );
    }

    /// `try_emit` cannot fail a caller — the walk asserts the row, the loops only get a `bool`.
    #[tokio::test]
    async fn try_emit_reports_failure_instead_of_raising_it() {
        // A pool that cannot be reached: `PgPoolOptions::connect_lazy` gives a handle whose
        // every query fails without a timeout wait, which is what makes this a unit test.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://omnion@127.0.0.1:1/omnion_w6_missing")
            .expect("a lazy pool");
        let written = try_emit(&pool, ALERT_FIRED, json!({ "rule": "x" })).await;
        assert!(!written, "an unreachable bus must be reported, not raised");
    }
}
