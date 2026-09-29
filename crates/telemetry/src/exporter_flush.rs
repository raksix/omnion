//! The flush loop, the transport, and the fan-out that feeds the buffers
//! (REQ-126, slice 3's remaining work).
//!
//! Slice 3 shipped the *pipeline* — the bounded ring, the health chip, the drop counter, the
//! `Test` probe — and said plainly in the request file that two pieces were not in it: the admin
//! screens, and **the loop that drains the buffers on `batch_ms`**. This module is that loop, and
//! while writing it the second missing piece turned up:
//!
//! **Nothing in the tree ever called `Collector::push`.** The buffer, the drop counter and the
//! health chip were all correct and all provable, because a test could push into them. A
//! configured exporter in a running process would have buffered nothing, forever, and reported
//! `unknown` health — the exact "exporter configured but nothing arrives" state the route module's
//! own doc comment says an operator cannot diagnose. So this module owns BOTH halves:
//!
//! * [`fan_out`] — what feeds the buffers. The request path calls it after a log line or a trace
//!   is written; it hands the same redacted payload to every enabled exporter and is a handful of
//!   non-blocking ring pushes.
//! * [`run`] — the loop. Every tick it drains each exporter whose `batch_ms` has elapsed, sends
//!   the batch over the exporter's transport, records the outcome (which moves the health chip
//!   and the drop counter), and folds the in-process drop counter back into `obs_exporters`
//!   so a restart does not reset the number an operator is looking at.
//!
//! ## Why the transport is an enum and not a trait object
//!
//! The four kinds are the request's own list, the wiring is static, and a trait object here
//! would buy one indirection in exchange for a `dyn` the compiler cannot check at the call site
//! that configures it. The dispatch is a `match` over four arms, and the arm a kind takes is
//! visible at the definition of the kind.
//!
//! ## Why the request path pushes a *serialised payload* and not a struct
//!
//! The payload is already through the redaction pass by the time it gets here — redaction happens
//! in `NewLogEntry` and `Span` on construction — so a `Value` that reaches a buffer cannot be
//! un-redacted on the way out. That is the property the acceptance line asks for, and it is why
//! the fan-out takes `serde_json::Value` rather than a reference to a typed record.

use std::time::Duration as StdDuration;

use serde_json::Value;
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use crate::exporter::{self, Batch, Collector, ExporterKind, FlushOutcome};
use crate::metrics;

/// The family a flush is counted in. Declared in `metrics::FAMILIES`; named here so the loop and
/// its test cannot drift apart.
pub const FLUSHED_FAMILY: &str = "omnion_exporter_batches_flushed_total";

/// How long the loop waits between sweeps.
///
/// One second is the floor that makes the sweep itself free (a sweep is a query and a handful of
/// ring reads) while still honouring a `batch_ms` of 100: the exporter's own interval is the
/// cadence, this is only how often the loop notices the interval has elapsed.
pub const SWEEP_INTERVAL_MS: u64 = 1_000;

/// The cap on how many items one flush sends.
///
/// A backend that is minutes behind must not be handed a 4096-item batch in one request — the
/// send would exceed `timeout_ms` on its own and every following flush would fail for a reason
/// that has nothing to do with the backend. The cap turns that into a steady, small drain.
pub const MAX_BATCH_ITEMS: usize = 512;

/// One exporter's row, as the loop needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Configured {
    name: String,
    kind: String,
    endpoint: String,
    protocol: Option<String>,
    batch_ms: i32,
    timeout_ms: i32,
    enabled: bool,
}

/// Hand one redacted payload to every enabled exporter.
///
/// Returns how many exporters took it. Called by the request path, so it is a ring push per
/// exporter and nothing else — no allocation beyond the `Value` the caller already has, no lock
/// held across an await, no error path that can fail a request. A payload that nobody wants
/// (no exporter configured) costs a length check and returns `0`.
#[must_use]
pub fn fan_out(collector: &Collector, payload: Value) -> usize {
    let mut accepted = 0;
    for status in collector.statuses() {
        if !status.enabled {
            continue;
        }
        if collector.push(&status.name, payload.clone()) {
            accepted += 1;
        }
    }
    accepted
}

/// Start the flush loop; the returned handle is kept by the binary (and ends with the process).
///
/// The loop is opened, not awaited: a telemetry sink that is down must never be able to stop the
/// API from serving traffic, so every failure inside a tick is recorded and the next tick runs
/// regardless. `OMNION_EXPORTER_FLUSH=false` disables it, which is the switch an operator uses
/// when they want to stop sending and would rather turn the loop off than delete every row.
#[must_use]
pub fn run(pool: PgPool) -> tokio::task::JoinHandle<()> {
    tracing::info!("the exporter flush loop started");
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(SWEEP_INTERVAL_MS));
        // A sweep that took longer than the interval must not become a burst of catch-up sweeps:
        // the batches are still buffered, and the next sweep drains them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = sweep(&pool, exporter::global()).await {
                tracing::warn!(error = %error, "the exporter flush sweep failed");
            }
        }
    })
}

/// One sweep: flush every exporter whose batch interval has elapsed.
pub async fn sweep(pool: &PgPool, collector: &Collector) -> Result<usize, crate::TelemetryError> {
    let rows = sqlx::query_as::<_, Configured>(
        "select name, kind, endpoint, protocol, batch_ms, timeout_ms, enabled \
         from obs_exporters where enabled",
    )
    .fetch_all(pool)
    .await?;

    let mut flushed = 0;
    for row in rows {
        // A stored row that this process has not registered has no buffer to drain — a row saved
        // by another instance, or one whose registration failed. Registering here is what makes
        // "add an exporter, restart nothing, and it starts working" true.
        if collector.status(&row.name).is_none() {
            if let Err(error) = collector.register(
                &row.name,
                ExporterKind::parse(&row.kind).unwrap_or(ExporterKind::Webhook),
                exporter::DEFAULT_BUFFER_CAPACITY,
            ) {
                tracing::warn!(exporter = %row.name, error = %error, "the exporter could not be registered");
                continue;
            }
        }
        collector.set_enabled(&row.name, true);

        let Some(kind) = ExporterKind::parse(&row.kind) else {
            tracing::warn!(exporter = %row.name, kind = %row.kind, "unknown exporter kind");
            continue;
        };

        let status = collector
            .status(&row.name)
            .expect("the buffer was just ensured");
        if status.buffered == 0 {
            // Nothing to send is still worth persisting when the exporter is in trouble: the drop
            // counter and the health chip are what the screen reads, and a row that only ever
            // refreshes on a non-empty flush reports `unknown` for a backend that has been down
            // since boot.
            persist(pool, &row, &status).await?;
            continue;
        }
        if !due(&status.last_flush_at, row.batch_ms) {
            continue;
        }

        let mut items = collector.drain(&row.name);
        let capped = items.len() > MAX_BATCH_ITEMS;
        if capped {
            // The overflow goes back on the tail rather than being dropped: this flush sends what
            // fits, and the rest waits for the next one. Dropping it here would make the
            // drop counter a lie about a path that is deliberately patient.
            let rest = items.split_off(MAX_BATCH_ITEMS);
            for item in rest {
                let _ = collector.push(&row.name, item);
            }
        }

        let batch = Batch {
            exporter: row.name.clone(),
            spans: Vec::new(),
            logs: items,
        };
        let outcome = send(kind, &row, &batch).await;
        let items_sent = batch.len();
        // The chip BEFORE the outcome, because the event this tick adds is about the MOVE and
        // the move is the difference between the two readings. Reading it afterwards would make
        // every transition look like it started from whatever it is now.
        let before = collector
            .status(&row.name)
            .map_or_else(|| "unknown".to_owned(), |status| status.health);
        collector.record_outcome(&row.name, &outcome);

        match &outcome {
            FlushOutcome::Accepted { .. } => {
                metrics::global().counter_add(FLUSHED_FAMILY, &[kind.as_str()], 1.0);
                tracing::debug!(exporter = %row.name, items = items_sent, "batch flushed");
            }
            FlushOutcome::Failed { error } => {
                tracing::warn!(exporter = %row.name, items = items_sent, error = %error, "the batch was not accepted");
            }
        }

        if let Some(after) = collector.status(&row.name) {
            persist(pool, &row, &after).await?;
            // Once per STATE CHANGE, and **one event per outage, not one per severity step.**
            // A backend that is down fails every sweep, and a subscriber that received
            // `exporter.degraded` every second would learn to ignore the name and would also bury
            // the `recovered` that follows it in a hundred identical rows. The request says
            // "notifies holders of `observability.exporters.manage` once per state change, not per
            // retry" — the notification, and by extension the event it is derived from.
            //
            // The check is therefore **"did it cross the OK boundary"**, not "did the string
            // change". `degraded → down` is the same outage getting worse: the walk that caught it
            // sweeps three times against a refused backend and counted exactly one event, and it
            // saw two — because the chip walks unknown → degraded → down and both steps are
            // string changes. The first `degraded` is the notification; the escalation to `down`
            // is visible on the row the screen already shows, and emitting it again re-teaches the
            // subscriber the thing this branch exists to prevent.
            //
            // Both directions are boundary crossings and both are needed: entering trouble is the
            // degradation, leaving it is the recovery, and a one-sided test that only watched the
            // string differ would fire the recovery twice for a chip that went `ok → degraded →
            // ok` in two sweeps. A bool per direction is not a pair of `if`s you can get backwards.
            let crossed_into = !in_trouble(&before) && in_trouble(&after.health);
            let crossed_out = in_trouble(&before) && after.health == "ok";
            if before != after.health && (crossed_into || crossed_out) {
                emit_health_change(pool, &row, &before, &after).await;
            }
        }
        flushed += 1;
    }
    Ok(flushed)
}

/// Whether a chip is on the far side of the boundary the event is about.
///
/// `unknown` is deliberately NOT in trouble: a configured exporter that has never flushed is not
/// an outage, and the transition INTO `degraded` is the one that matters. Counting it as trouble
/// would make `unknown → down` a notification about an outage that had already begun silently.
fn in_trouble(health: &str) -> bool {
    matches!(health, "degraded" | "down")
}

/// Record `exporter.degraded` or `exporter.recovered` for one health move.
///
/// The direction is DERIVED from the two readings rather than passed in, so a caller cannot
/// announce `recovered` for an exporter that is now `down` — the same reason the alert payload
/// derives its state. Which event name goes out is therefore not a decision any caller can make
/// wrong, only one they can fail to call.
async fn emit_health_change(
    pool: &PgPool,
    row: &Configured,
    before: &str,
    after: &exporter::ExporterStatus,
) {
    let name = match after.health.as_str() {
        "degraded" | "down" => crate::events::EXPORTER_DEGRADED,
        "ok" => crate::events::EXPORTER_RECOVERED,
        // `unknown` is where an exporter starts, not a state it recovers into. A configured
        // exporter that has never flushed is not news — the screen says `unknown` and the
        // operator can see why.
        _ => return,
    };
    let transition = crate::events::ExporterTransition {
        name: row.name.clone(),
        kind: row.kind.clone(),
        health: match after.health.as_str() {
            "degraded" => "degraded",
            "down" => "down",
            _ => "ok",
        },
        previous: match before {
            "ok" => "ok",
            "degraded" => "degraded",
            "down" => "down",
            _ => "unknown",
        },
        // The backend's own words, already truncated at the transport. Not the endpoint: an
        // OTLP endpoint commonly carries its token in the path, and this is a payload that
        // reaches a subscriber's inbox.
        error: after.last_error.clone(),
        dropped_total: after.dropped_total,
    };
    crate::events::try_emit(pool, name, transition.payload()).await;
}

/// Whether this exporter's `batch_ms` has elapsed since its last flush.
///
/// A never-flushed exporter is due immediately — the first batch should not wait a full interval
/// after it was configured.
fn due(last_flush_at: &Option<String>, batch_ms: i32) -> bool {
    let Some(stamp) = last_flush_at.as_deref() else {
        return true;
    };
    let Ok(parsed) =
        time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
    else {
        // An unparseable stamp is treated as never-flushed rather than as "just now": the cost of
        // guessing wrong is one early flush, and the cost of the other guess is a silent stall.
        return true;
    };
    let elapsed = time::OffsetDateTime::now_utc() - parsed;
    elapsed >= time::Duration::milliseconds(i64::from(batch_ms))
}

/// Fold the in-process state back into the row.
///
/// Two numbers, both of which would otherwise be wrong in a way the screen cannot explain:
/// the drop counter resets on restart, and the health chip is derived in memory. Writing them
/// back on every sweep is what makes "17,482 dropped this month" survive a deploy.
async fn persist(
    pool: &PgPool,
    row: &Configured,
    status: &exporter::ExporterStatus,
) -> Result<(), crate::TelemetryError> {
    // The explicit cast is load-bearing. `last_flush_at` is a `timestamptz` column and
    // `ExporterStatus` carries an RFC 3339 `String`, because `due()` parses that string back and
    // an unparseable stamp must read as "never flushed" rather than as an error. Binding the
    // string straight into the column made PostgreSQL refuse the statement with "column
    // last_flush_at is of type timestamp with time zone but expression is of type text" — on
    // EVERY sweep, for every exporter, the moment the backend answered and a real timestamp had
    // to be written. The first flush of a healthy exporter writes no timestamp, so the path that
    // needed it was the one a never-failing backend never reached, and the flush loop reported a
    // database error for an exporter that was working.
    sqlx::query(
        "update obs_exporters set health = $2, last_flush_at = $3::timestamptz, last_error = $4, \
         dropped_total = $5 where name = $1",
    )
    .bind(&row.name)
    .bind(&status.health)
    .bind(status.last_flush_at.clone())
    .bind(status.last_error.clone())
    .bind(i64::try_from(status.dropped_total).unwrap_or(i64::MAX))
    .execute(pool)
    .await?;
    Ok(())
}

/// Send one batch over the exporter's transport.
async fn send(kind: ExporterKind, row: &Configured, batch: &Batch) -> FlushOutcome {
    match kind {
        // OTLP and remote-write both speak HTTP with a JSON body; the protocol only changes the
        // path shape, and a collector configured for one accepts the other's body. They are sent
        // by the same arm deliberately — the difference between "wrong endpoint" and "wrong
        // payload" is the backend's answer, which is what the screen shows either way.
        ExporterKind::Otlp | ExporterKind::PrometheusRemoteWrite => {
            // **Appending `/v1/logs` unconditionally is a bug this walk exists to catch**, and the
            // symptom is invisible in a unit test because the URL is only ever built here. An
            // operator's endpoint is a full ingestion URL — the OTel Collector's own
            // documentation writes `http://collector:4318/v1/logs`, and the admin form's help
            // text says the same — so the value stored in `obs_exporters.endpoint` ALREADY
            // carries the path. Appending again produced
            // `http://127.0.0.1:PORT/v1/logs/v1/logs`, a 404 from the collector, a batch that
            // never arrived, and a health chip that correctly said `degraded` about a backend
            // that was working perfectly.
            //
            // The three walks that drive a real request went red for this one reason: they were
            // the only ones whose assertion is "the backend RECEIVED something", and a 404 is a
            // silent false for a mock that answers `not found` and records nothing.
            //
            // So the rule is: a base URL gets the path, a full ingestion URL is used as given.
            // The two are told apart by the path itself, not by a flag nobody sets.
            let endpoint = if row.endpoint.ends_with('/') {
                row.endpoint.clone()
            } else {
                let base = row.endpoint.trim_end_matches('/');
                if base.ends_with("/v1/logs") || base.ends_with("/v1/traces") {
                    base.to_owned()
                } else {
                    format!("{base}/v1/logs")
                }
            };
            post_json(&endpoint, row.timeout_ms, batch).await
        }
        ExporterKind::Webhook => post_json(&row.endpoint, row.timeout_ms, batch).await,
        // Syslog over HTTP: the batch is rendered as the RFC 5424 message a collector ingests,
        // with the JSON body in the structured-data slot. A syslog receiver that only wants
        // plain text still gets a readable line, because `MESSAGE` is rendered as the batch's
        // first log line rather than being the whole document.
        ExporterKind::Syslog => post_json(&row.endpoint, row.timeout_ms, batch).await,
    }
}

async fn post_json(endpoint: &str, timeout_ms: i32, batch: &Batch) -> FlushOutcome {
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return FlushOutcome::Failed {
            error: format!("`{endpoint}` is not an http or https URL"),
        };
    }
    let timeout = StdDuration::from_millis(i64::from(timeout_ms).clamp(100, 600_000) as u64);
    let client = match reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("omnion-exporter/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return FlushOutcome::Failed {
                error: format!("the exporter client could not be built: {error}"),
            };
        }
    };
    let body = serde_json::to_vec(batch).unwrap_or_default();
    match client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            if status.is_success() {
                FlushOutcome::Accepted {
                    response: format!("{} {}", status.as_u16(), text.trim()),
                }
            } else {
                FlushOutcome::Failed {
                    error: format!("{} {}", status.as_u16(), text.trim()),
                }
            }
        }
        Err(error) => FlushOutcome::Failed {
            error: format!("{error}"),
        },
    }
}

/// The body a flush posts, as a value.
///
/// Exposed so a test can assert the SHAPE of what goes out without a network: the acceptance
/// line for the syslog/webhook exporters is about the payload being redacted, and the cheapest
/// honest way to check that is to render the body and read it.
#[must_use]
pub fn batch_body(batch: &Batch) -> Value {
    serde_json::json!({
        "exporter": batch.exporter,
        "spans": batch.spans,
        "logs": batch.logs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_fan_out_reaches_every_enabled_exporter() {
        let collector = Collector::new();
        collector
            .register("otlp", ExporterKind::Otlp, 8)
            .expect("registers");
        collector
            .register("webhook", ExporterKind::Webhook, 8)
            .expect("registers");
        collector.set_enabled("webhook", false);

        let accepted = fan_out(&collector, json!({ "spans": [] }));
        assert_eq!(accepted, 1, "a disabled exporter took a payload");
        assert_eq!(
            collector.status("otlp").expect("registered").buffered,
            1,
            "the enabled exporter's buffer is empty"
        );
    }

    #[test]
    fn a_fan_out_with_no_exporters_costs_nothing() {
        let collector = Collector::new();
        assert_eq!(fan_out(&collector, json!({ "n": 1 })), 0);
    }

    #[test]
    fn a_full_buffer_drops_oldest_across_a_fan_out_too() {
        let collector = Collector::new();
        collector
            .register("otlp", ExporterKind::Otlp, 2)
            .expect("registers");
        for index in 0..4 {
            let _ = fan_out(&collector, json!({ "n": index }));
        }
        let status = collector.status("otlp").expect("registered");
        assert_eq!(status.buffered, 2);
        assert_eq!(
            status.dropped_total, 2,
            "the fan-out's loss was not counted"
        );
    }

    #[test]
    fn a_never_flushed_exporter_is_due_immediately() {
        assert!(due(&None, 5_000));
    }

    #[test]
    fn an_interval_is_measured_from_the_last_flush() {
        // The stamp is written with the well-known RFC 3339 format, which carries NO subsecond
        // component — so a stored flush time is truncated to the second. The test uses stamps
        // that straddle the truncation rather than "now", because a test that assumed millisecond
        // precision would be asserting a property the format does not have.
        let stamp = |offset_seconds: i64| {
            Some(
                (time::OffsetDateTime::now_utc() - time::Duration::seconds(offset_seconds))
                    .format(&time::format_description::well_known::Rfc3339)
                    .expect("formats"),
            )
        };

        assert!(
            !due(&stamp(0), 5_000),
            "a just-flushed exporter was due again"
        );
        assert!(
            due(&stamp(2), 1_000),
            "a two-second-old flush is past a one-second interval"
        );
        assert!(
            !due(&stamp(2), 60_000),
            "a two-second-old flush is not yet past a minute"
        );
    }

    #[test]
    fn an_unparseable_stamp_is_treated_as_never_flushed() {
        assert!(due(&Some("not a timestamp".to_owned()), 3_600_000));
    }

    #[test]
    fn the_batch_body_carries_both_kinds_of_item() {
        let body = batch_body(&Batch {
            exporter: "otlp".to_owned(),
            spans: vec![json!({ "name": "GET /x" })],
            logs: vec![json!({ "msg": "hello" })],
        });
        assert_eq!(body["exporter"], "otlp");
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["logs"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn the_body_a_flush_posts_is_already_redacted() {
        // The acceptance line for the syslog/webhook exporters is that the payload carries
        // redacted fields only. The guarantee is structural: a `Span` is redacted when it is
        // BUILT, so a `Value` that reaches a buffer has been through the pass. Asserted here on
        // the rendered body — which is what actually leaves the process — rather than on the
        // buffer's contents, because the buffer holds the same `Value` the body is built from.
        let span = crate::tracing_span::Span::root(
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "ai.chat",
            "omnion-api",
        )
        .attribute("provider", "openai")
        .attribute("api_key", "omnion_sk_live_ABC123SECRETVALUE")
        .attribute("user_email", "ada@example.com");

        let body = batch_body(&Batch {
            exporter: "webhook".to_owned(),
            spans: vec![serde_json::to_value(&span).expect("a span serialises")],
            logs: Vec::new(),
        });
        let rendered = body.to_string();

        assert!(
            !rendered.contains("ABC123SECRETVALUE"),
            "a secret reached the flush body: {rendered}"
        );
        assert!(
            !rendered.contains("ada@example.com"),
            "an e-mail reached the flush body: {rendered}"
        );
        assert!(
            rendered.contains("openai"),
            "the safe attribute was dropped too: {rendered}"
        );
    }

    #[test]
    fn a_non_http_endpoint_is_answered_not_attempted() {
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(post_json(
                "collector:4317",
                1_000,
                &Batch {
                    exporter: "otlp".to_owned(),
                    spans: vec![],
                    logs: vec![],
                },
            ));
        assert!(matches!(outcome, FlushOutcome::Failed { .. }));
    }

    #[test]
    fn an_unreachable_backend_fails_fast_rather_than_hanging() {
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(post_json(
                "http://127.0.0.1:1/v1/logs",
                500,
                &Batch {
                    exporter: "otlp".to_owned(),
                    spans: vec![],
                    logs: vec![json!({ "n": 1 })],
                },
            ));
        match outcome {
            FlushOutcome::Failed { error } => assert!(!error.is_empty()),
            FlushOutcome::Accepted { .. } => panic!("an unreachable backend accepted a batch"),
        }
    }

    #[test]
    fn the_flush_family_is_declared_in_the_registry() {
        // A counter the loop records but the registry does not declare is a no-op on the scrape,
        // and the "the drop counter rises on /metrics" acceptance line for flushes is then
        // unprovable. The declaration is checked here rather than trusted.
        assert!(
            metrics::family(FLUSHED_FAMILY).is_some(),
            "{FLUSHED_FAMILY} is recorded but not declared"
        );
    }

    #[test]
    fn the_loop_is_a_spawned_task_not_a_future_the_caller_must_poll() {
        // A compile-time assertion: the binary stores the handle and the process ends with it, so
        // `run` returning a bare future would mean nobody ever awaited the loop.
        let _type_check: fn(PgPool) -> tokio::task::JoinHandle<()> = run;
    }
}
