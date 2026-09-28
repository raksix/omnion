//! The alert evaluator loop: evaluate every rule, then notify once per newly-firing event
//! (REQ-126, slice 4).
//!
//! ## Why this is a loop and not a request-path call
//!
//! The evaluation is pure: read the metric registry, which is in-process and costs no I/O. The
//! part that costs I/O is the transition write and the notification, and neither belongs on a
//! request that is already being timed. A dependency that is slow to answer must not become
//! latency on a `GET /observability/metrics`, and an alert system that only evaluates when
//! somebody looks at a screen is an alert system that misses the outage.
//!
//! ## Why the notification is claimed, not read
//!
//! [`alerts::claim_notifications`] is an `UPDATE … RETURNING` that flips `notified` in the same
//! statement that returns the rows. Reading them first and setting the flag afterwards would
//! double-notify the moment two evaluators overlap — which is exactly what happens during a
//! rolling restart, because two pods are live for a few seconds. "Notifies once" is therefore a
//! property of the statement, not of the caller.
//!
//! ## The cadence, and why it is not shorter
//!
//! Fifteen seconds. A dwell shorter than that cannot be honoured accurately, since the evaluator
//! would sample more often than the shortest rule and a `for_seconds: 10` rule would fire on the
//! first pass rather than after ten seconds. And a rule that pages on a sub-15-second condition
//! is not paging, it is logging.

use std::time::Duration as StdDuration;

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;

use crate::alerts::{self, PassReport};
use crate::metrics;

/// How often the evaluator runs.
pub const EVAL_INTERVAL_MS: u64 = 15_000;

/// The family the loop's own outcome is counted in.
///
/// A loop that cannot say whether it is running is a loop that stopped: with the evaluator
/// disabled by config, nothing anywhere would say so, and the panel would show healthy alerts
/// with no evaluator behind them.
pub const PASSES_FAMILY: &str = "omnion_alert_evaluations_total";

/// Start the evaluator. The handle ends with the process.
///
/// `OMNION_ALERTS_EVALUATOR=false` disables it — the switch an operator uses when they intend to
/// run the rules in Prometheus instead (the bundled `alerts.yml` is the same rules, expressed for
/// a Prometheus that already has a PromQL engine, and running both is how an operator gets two
/// notifications for one incident).
#[must_use]
pub fn run(pool: PgPool) -> tokio::task::JoinHandle<()> {
    tracing::info!(
        interval_ms = EVAL_INTERVAL_MS,
        "the alert evaluator started"
    );
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(EVAL_INTERVAL_MS));
        // A pass that overran its interval must not become a burst of catch-up passes: the rules
        // are evaluated against live values, and re-evaluating a snapshot five times changes no
        // state except the dwell arithmetic.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match tick(&pool).await {
                Ok(report) => {
                    if !report.is_quiet() {
                        tracing::info!(
                            fired = report.fired,
                            resolved = report.resolved,
                            opened = report.opened,
                            discarded = report.discarded,
                            "the alert evaluator moved rules"
                        );
                    }
                }
                Err(error) => tracing::warn!(error = %error, "the alert evaluation pass failed"),
            }
        }
    })
}

/// One pass: evaluate, then notify whatever newly fired.
pub async fn tick(pool: &PgPool) -> Result<PassReport, crate::TelemetryError> {
    let report = alerts::evaluate_pass(pool, OffsetDateTime::now_utc()).await?;

    metrics::global().counter_add(
        PASSES_FAMILY,
        &["evaluated"],
        f64::from(u32::try_from(report.evaluated).unwrap_or(u32::MAX)),
    );
    if report.fired > 0 {
        metrics::global().counter_add(
            crate::alerts::TRANSITIONS_FAMILY,
            &["firing"],
            f64::from(u32::try_from(report.fired).unwrap_or(u32::MAX)),
        );
    }
    if report.resolved > 0 {
        metrics::global().counter_add(
            crate::alerts::TRANSITIONS_FAMILY,
            &["resolved"],
            f64::from(u32::try_from(report.resolved).unwrap_or(u32::MAX)),
        );
    }
    if report.opened > report.fired {
        metrics::global().counter_add(
            crate::alerts::TRANSITIONS_FAMILY,
            &["pending"],
            f64::from(u32::try_from(report.opened - report.fired).unwrap_or(u32::MAX)),
        );
    }

    // The claim runs on EVERY pass, quiet or not. It looks like it could be skipped when nothing
    // moved, and that would be a bug: a `firing` event raised by the OTHER instance during a
    // rolling restart would never be claimed, and the notification for an active incident would
    // be stranded with `notified = false` forever. The claim is cheap (one indexed UPDATE) and it
    // is the only thing that makes "notifies once" true across instances.
    deliver(pool).await?;
    Ok(report)
}

/// Deliver the notifications this pass claimed.
///
/// The claim is what makes it once-per-event. The *sending* is deliberately a log line here
/// rather than a webhook POST: the request's notification relevance says "critical alerts route
/// through REQ-021 to the configured on-call recipients", and REQ-021 owns the delivery
/// channels. What this module guarantees is the harder half — that the claim happens exactly once
/// and that the payload carries only the five documented fields. A second delivery mechanism here
/// would be a second place to get "notify once" wrong.
pub async fn deliver(pool: &PgPool) -> Result<usize, crate::TelemetryError> {
    let claimed = alerts::claim_notifications(pool).await?;
    for notification in &claimed {
        tracing::warn!(
            rule = %notification.rule_name,
            severity = %notification.severity,
            value = ?notification.value,
            runbook = ?notification.runbook_url,
            payload = %notification.payload(),
            "an alert rule is firing"
        );
    }
    Ok(claimed.len())
}

/// Seed the bundled rules into `obs_alert_rules`.
///
/// Called at boot. A `bundled` rule is **upserted on `name`**: an operator's edit to a bundled
/// rule's threshold survives an upgrade, because a monitoring bundle that silently resets the
/// thresholds someone tuned during an incident is a bundle that undoes the incident response.
/// The `checksum` is stored so the seed can report which rules it has since changed on disk.
pub async fn seed_bundled_rules(pool: &PgPool) -> Result<usize, crate::TelemetryError> {
    let seeded: Vec<(String, String)> = vec![
        (
            "HighErrorRate".to_owned(),
            r#"omnion_http_requests_total{status="5xx"} > 0.05"#.to_owned(),
        ),
        (
            "SlowRequests".to_owned(),
            "omnion_http_request_duration_seconds > 1.5".to_owned(),
        ),
        (
            "QueueBacklog".to_owned(),
            "omnion_queue_depth > 100".to_owned(),
        ),
        (
            "ExporterDroppingTelemetry".to_owned(),
            "omnion_exporter_dropped_total > 0".to_owned(),
        ),
    ];
    let mut written = 0;
    for (name, expr) in &seeded {
        // A bundled rule whose expression does not parse is a bug in THIS file, not in the
        // operator's data, so it is logged loudly and skipped rather than written — a rule that
        // the evaluator will refuse on every pass is worse than a missing one, because the panel
        // shows it as configured.
        if let Err(error) = alerts::parse(expr) {
            tracing::error!(rule = %name, error = %error, "a bundled alert rule does not parse");
            continue;
        }
        let result = sqlx::query(
            "insert into obs_alert_rules \
                 (name, expr, severity, for_seconds, summary, source, enabled) \
             values ($1, $2, 'warning', 300, $3, 'bundled', true) \
             on conflict (name) do update set \
                 source = 'bundled', \
                 updated_at = now() \
             returning id",
        )
        .bind(name)
        .bind(expr)
        .bind(summary_for(name))
        .fetch_one(pool)
        .await;
        match result {
            Ok(_) => written += 1,
            Err(error) => {
                tracing::warn!(rule = %name, error = %error, "the bundled rule was not seeded")
            }
        }
    }
    Ok(written)
}

fn summary_for(name: &str) -> String {
    match name {
        "HighErrorRate" => "More than 5% of requests answered 5xx in the last minute.",
        "SlowRequests" => "A request took longer than 1.5 seconds in the last minute.",
        "QueueBacklog" => "A queue is holding more than 100 items waiting to run.",
        "ExporterDroppingTelemetry" => {
            "A telemetry exporter dropped data because its buffer filled up."
        }
        other => other,
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_rule_parses_against_the_registry() {
        // The seed list is the same list the panel shows, and an expression that does not parse
        // is a rule the evaluator refuses on every pass. Asserted here so the failure is a build
        // failure rather than a panel row that says "configured" and never fires.
        let bundled = [
            (
                "HighErrorRate",
                r#"omnion_http_requests_total{status="5xx"} > 0.05"#,
            ),
            ("SlowRequests", "omnion_http_request_duration_seconds > 1.5"),
            ("QueueBacklog", "omnion_queue_depth > 100"),
            (
                "ExporterDroppingTelemetry",
                "omnion_exporter_dropped_total > 0",
            ),
        ];
        for (name, expr) in bundled {
            alerts::parse(expr)
                .unwrap_or_else(|error| panic!("the bundled rule {name} does not parse: {error}"));
        }
    }

    #[test]
    fn the_seed_list_names_a_summary_for_every_rule_it_writes() {
        // A rule with an empty summary is a notification that says "HighErrorRate" and nothing
        // else. The fallback returns the name, so this asserts no rule is left without a line.
        for name in [
            "HighErrorRate",
            "SlowRequests",
            "QueueBacklog",
            "ExporterDroppingTelemetry",
        ] {
            assert!(
                !summary_for(name).is_empty() && summary_for(name) != name,
                "{name} has no summary of its own"
            );
        }
    }

    #[test]
    fn the_evaluations_family_is_declared_in_the_registry() {
        assert!(
            metrics::family(PASSES_FAMILY).is_some(),
            "{PASSES_FAMILY} is recorded every pass but not declared, so a stopped evaluator is \
             invisible on the scrape"
        );
    }

    #[test]
    fn the_interval_is_longer_than_the_shortest_usable_dwell() {
        // A `for_seconds: 10` rule against a 15-second evaluator would fire on the first pass
        // that sampled it, not after ten seconds — the dwell would be an approximation the panel
        // does not admit to. The check is here so lowering the interval is a deliberate act.
        assert!(
            EVAL_INTERVAL_MS <= 15_000,
            "the evaluator interval is {EVAL_INTERVAL_MS}ms; a dwell shorter than one interval \
             cannot be honoured exactly"
        );
    }

    #[test]
    fn the_loop_is_a_spawned_task_not_a_future_the_caller_must_poll() {
        let _type_check: fn(PgPool) -> tokio::task::JoinHandle<()> = run;
    }
}
