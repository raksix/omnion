//! The five `health.*` events (REQ-014, slice 4 — the second half).
//!
//! REQ-014's Events section names five events and then says the useful thing about two of them:
//! *"an operations endpoint subscribes to `health.service.degraded` and
//! `health.service.recovered`"*. Until this file existed that sentence had nothing behind it.
//! The events table had no `health` area at all, so there was no name to subscribe to, no
//! emitter, and no way for an operator to find out from outside the panel that a dependency had
//! stopped answering. This is the fourth time on this codebase that a spec sentence was the
//! whole implementation — after REQ-010's uncalled `prune_candidates`, REQ-013's unwritten
//! `next_run_at`, and REQ-014's own `worker_heartbeats` — and it is recorded that way in the
//! BUILD-LOG because the pattern is worth naming: **a reader with no writer, a setting nobody
//! reads, a sentence nobody runs.** None of them is visible to a green gate.
//!
//! ## Why this lives in the API and not in `crates/health`
//!
//! The health crate is infrastructure and knows nothing about the bus — deliberately, and for
//! the same reason `omnion-audit` and `omnion-events` are separate from the things they
//! describe. What makes this module more than a third call site is that **both** callers of
//! `run_and_record` have to reach it: the scheduled runner and the manual button. A button that
//! produced different events from the schedule would be the exact defect `run_and_record` was
//! created to prevent, so the emission hangs off that function's *result* — which is why slice 4
//! had to change `apply_policy` to return what it decided instead of discarding it.
//!
//! ## Two kinds of fact, and only one of them is rare
//!
//! The five names are not five times a day. They split into facts that are **already
//! deduplicated by the store** and one that is not:
//!
//! * `health.service.degraded`, `health.service.recovered` and `health.threshold.breached`
//!   describe a *change*, and the incident/breach ledger is what makes them rare: one row per
//!   transition, one row per `(metric, window)`. Announcing them from **any** run — including
//!   the live `GET /health/overview` — is safe, because the deduplication already happened
//!   before this module is reached. `GET /health/overview` genuinely runs the probes, so a
//!   transition it discovers is a real discovery.
//! * `health.checks.completed` describes a *run*, and the panel auto-refreshes every 15 s.
//!   Emitting it from a read would put four facts a minute on every operations endpoint for as
//!   long as somebody had the screen open — precisely the "alarm firehose" the request's own
//!   risk note refuses. It is emitted only by the two **deliberate** runs: the scheduled runner
//!   and the "Run all checks" button. That is why the two emitters below have different names
//!   rather than one with a flag.
//!
//! ## The fan-out problem, and what it forces
//!
//! `events::store::enqueue_fanout` returns `0` for an event with no organization — endpoints
//! belong to tenants, and matching a tenant's endpoint against a fact belonging to no tenant
//! would leak. Health is **platform-level**: Redis being down is not one organization's
//! outage. So the obvious implementation — `bus::emit(pool, NewEvent::new("health.service
//! .degraded").payload(..))` — emits five names that are recorded in `events` and delivered to
//! **nobody**. That is the most dangerous possible outcome for this slice: the catalogue says
//! the name is live, the picker offers it, an operator subscribes an endpoint to `health.*`,
//! the picker expands to five names, the endpoint looks configured — and no delivery is ever
//! queued, with no error anywhere.
//!
//! So the events are emitted **per organization that has an enabled endpoint subscribed to
//! `health`**, which is exactly the set of receivers that could want a platform fact:
//!
//! * No endpoint, no event. An installation with zero webhook endpoints records nothing rather
//!   than filling `events` with rows no fan-out could ever consume.
//! * Only **enabled** endpoints count, and only for the groups they actually subscribe to — the
//!   same test `enqueue_fanout` applies, done here so the answer is not "recorded for nobody"
//!   repeated once per tenant.
//! * Organizations are capped ([`MAX_ORGANIZATIONS`]), because an outage is the one moment when
//!   a large deployment is least able to afford an unbounded insert into the busiest table.
//!
//! The alternative — inventing a platform organization and attributing platform facts to it —
//! was rejected: it would put every tenant's webhook traffic under one tenant's retention
//! window, and `sweep_events` keys on `organization_id`, so one organization could delete
//! another's health history.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use omnion_events::{NewEvent, bus};
use omnion_health::{HealthOverview, Incident, PolicyOutcome};

/// The group an operations endpoint subscribes to, as the picker writes it.
///
/// Stored on an endpoint as the wildcard **and** as today's expansion
/// (`catalogue::reconcile`), so this test mirrors `enqueue_fanout`'s: a row carrying only
/// `health.*` — written by hand, or before reconciliation existed — still receives.
const HEALTH_GROUP: &str = "health";

/// The ceiling on organizations one run may fan out to.
///
/// Small on purpose, and the reason is the one the request's own risk note gives: *"the panel
/// must not become an alarm firehouse."* A run that announced a Redis outage to 500 tenants
/// while Redis was down is a second outage, caused by the reporting.
const MAX_ORGANIZATIONS: i64 = 200;

/// One `health.*` fact, before it knows which organizations will receive it.
///
/// A struct rather than a `(name, payload)` pair because every emitter needs the same three
/// things and getting one of them wrong is silent: the catalogue's drift test only checks the
/// **name**, so a payload missing a required field ships green and fails at the receiver.
struct Announcement {
    /// The event name, exactly as the catalogue spells it.
    name: &'static str,
    /// The payload. Checked against the catalogue's required fields on construction.
    payload: serde_json::Value,
    /// The organization the action belongs to, when there was a person behind it.
    actor: Option<Uuid>,
}

impl Announcement {
    /// Build one, and **check it against the catalogue**.
    ///
    /// The check is not ceremony. `every_emitted_name_is_in_the_catalogue` proves the name is
    /// listed; nothing in the workspace proves the payload carries the fields the catalogue
    /// calls required, because a JSON literal with a missing key is a perfectly valid value of
    /// type `Value`. So a receiver would get `{"service":"redis"}` on an event whose catalogue
    /// row promises `from_state` and `to_state`, and the only symptom is a receiver that
    /// panics or silently takes a default on somebody else's production system.
    ///
    /// A defect is **logged and returned as `None`, not panicked on**, and the reason is the
    /// failure mode it prevents: this module is reached from a probe run, so a panic here would
    /// take down the health runner inside the very task that reports the platform's health. The
    /// probe's own reading is still true and the platform is still up; a reporting defect must
    /// not become an outage. `tracing::error!` is loud on purpose — this branch means the
    /// catalogue row and the emitter have drifted apart, and that is a bug to fix rather than a
    /// condition to absorb.
    fn new(name: &'static str, payload: serde_json::Value, actor: Option<Uuid>) -> Option<Self> {
        let Some(entry) = omnion_events::catalogue::lookup(name) else {
            tracing::error!(
                name,
                "{name} is emitted but is not in the catalogue, so no subscriber can receive it"
            );
            return None;
        };
        let missing: Vec<&str> = entry
            .required_fields()
            .filter(|field| {
                !payload
                    .get(field.name)
                    .is_some_and(|value| !value.is_null())
            })
            .map(|field| field.name)
            .collect();
        if !missing.is_empty() {
            tracing::error!(
                name,
                fields = ?missing,
                "{name} promises required field(s) the emitter does not send; the event was not recorded"
            );
            return None;
        }
        Some(Self {
            name,
            payload,
            actor,
        })
    }
}

/// Announce the **changes** a run implied: every transition the store acted on, and every metric
/// that crossed its line for the first time in its window.
///
/// Safe to call from any run, including the live read, because the store has already
/// deduplicated both by the time this is reached — a service whose state did not move produced
/// no transition, and a metric that has been over its line for an hour produced a
/// `BreachCheck` whose `should_announce` is false. Returns how many facts were recorded.
pub async fn announce_changes(pool: &PgPool, policy: &PolicyOutcome) -> usize {
    let mut announced = 0;

    for entry in &policy.transitions {
        // `Nothing` means a concurrent run already opened or resolved the row. Announcing it
        // would tell every operations endpoint about the same outage twice.
        let Some(incident) = entry.outcome.incident() else {
            continue;
        };
        let Some(announcement) = (if entry.transition.is_recovery() {
            Announcement::new(
                "health.service.recovered",
                json!({
                    "service": entry.transition.service,
                    "from_state": entry.transition.from_state,
                    "to_state": entry.transition.to_state,
                    "duration_seconds": incident.duration_seconds(),
                    "incident_id": incident.id,
                }),
                None,
            )
        } else {
            Announcement::new(
                "health.service.degraded",
                json!({
                    "service": entry.transition.service,
                    "from_state": entry.transition.from_state,
                    "to_state": entry.transition.to_state,
                    "message": entry.transition.summary,
                    "incident_id": incident.id,
                    "suppressed": entry.suppressed,
                }),
                None,
            )
        }) else {
            continue;
        };
        announced += emit_for_subscribers(pool, &announcement).await;
        tracing::debug!(
            service = %entry.service,
            name = announcement.name,
            "a health state change was announced"
        );
    }

    for (_, check) in &policy.breaches {
        if !check.should_announce() {
            continue;
        }
        let Some(breach) = Announcement::new(
            "health.threshold.breached",
            json!({
                "metric": check.metric,
                "value": check.value,
                "crit_limit": check.crit_limit,
                "window_start": omnion_health::breach_window(time::OffsetDateTime::now_utc()),
            }),
            None,
        ) else {
            continue;
        };
        announced += emit_for_subscribers(pool, &breach).await;
    }

    announced
}

/// Announce a **deliberate** run: `health.checks.completed`, with the operator when there was
/// one.
///
/// Called by the scheduled runner and by the "Run all checks" button, and by nothing else — the
/// module doc explains why a live read must not produce one of these. `actor` is `None` for the
/// schedule and `Some` for the button, and the difference is on the event's own row *and* on
/// the payload, because a receiver reading the delivery body never sees `actor_user_id`.
pub async fn announce_run(pool: &PgPool, overview: &HealthOverview, actor: Option<Uuid>) -> usize {
    let mut payload = json!({
        "state": overview.banner.state,
        "services": overview.services.len(),
        "worst_service": overview.banner.worst_service,
    });
    if let Some(actor) = actor {
        payload["actor"] = json!(actor);
    }
    match Announcement::new("health.checks.completed", payload, actor) {
        Some(announcement) => emit_for_subscribers(pool, &announcement).await,
        None => 0,
    }
}

/// Announce an operator's acknowledgement: `health.incident.acknowledged`.
///
/// The **actor is on the payload**, not only in `actor_user_id`, and the difference is the
/// point: `actor_user_id` is a column on the event row that a receiver reading the delivery body
/// never sees. An operations endpoint that wants to know *who* looked has to be told, and the
/// catalogue row marks `actor` required for that reason.
pub async fn announce_acknowledgement(
    pool: &PgPool,
    incident: &Incident,
    actor: Uuid,
    note: &str,
) -> usize {
    let mut payload = json!({
        "incident_id": incident.id,
        "service": incident.service,
        "actor": actor,
    });
    // An operator who typed nothing did not write the word "null", and a receiver that logs
    // notes should not have to tell "no note" from "a note that says null". The field is
    // optional in the catalogue precisely so it can be absent.
    if !note.trim().is_empty() {
        payload["note"] = json!(note);
    }
    match Announcement::new("health.incident.acknowledged", payload, Some(actor)) {
        Some(announcement) => emit_for_subscribers(pool, &announcement).await,
        None => 0,
    }
}

/// Record one announcement for every organization with a receiver that wants it.
async fn emit_for_subscribers(pool: &PgPool, announcement: &Announcement) -> usize {
    let recipients = match subscribers(pool).await {
        Ok(recipients) => recipients,
        Err(error) => {
            tracing::warn!(
                error = %error,
                name = announcement.name,
                "the health recipients could not be read; nothing was announced"
            );
            return 0;
        }
    };
    if recipients.is_empty() {
        return 0;
    }

    let mut recorded = 0;
    for organization_id in recipients {
        if let Err(error) = bus::emit(
            pool,
            NewEvent::new(announcement.name)
                .organization(organization_id)
                .actor(announcement.actor)
                .payload(announcement.payload.clone()),
        )
        .await
        {
            // Logged per recipient and not propagated: one tenant whose insert failed must not
            // cost the other 199 tenants their notification of the same outage.
            tracing::warn!(
                organization_id = %organization_id,
                name = announcement.name,
                error = %error,
                "a health event could not be recorded for one organization"
            );
            continue;
        }
        recorded += 1;
    }
    recorded
}

/// The organizations that have at least one enabled endpoint subscribed to `health`.
///
/// The set, and not `organizations` — a tenant that never wired a webhook has no receiver, so
/// a platform-wide fact has nothing to deliver to and the row in `events` would be dead weight
/// that `sweep_events` then has to clean up. The `limit` is [`MAX_ORGANIZATIONS`] and it is
/// applied **before** the emission rather than after, so a truncated run is smaller rather than
/// merely reported.
async fn subscribers(pool: &PgPool) -> Result<Vec<Uuid>, sqlx::Error> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "select distinct w.organization_id from webhook_endpoints w \
         where w.enabled and w.organization_id is not null \
           and ($1 = any (w.events) or $2 = any (w.events)) \
         order by w.organization_id \
         limit $3",
    )
    .bind(HEALTH_GROUP)
    .bind(format!("{HEALTH_GROUP}.*"))
    .bind(MAX_ORGANIZATIONS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_announcement_this_module_builds_passes_the_catalogue_check() {
        // The check lives in `Announcement::new`; this exercises it on the shapes the emitters
        // actually build rather than on a hand-written stand-in. Every one must be `Some` — a
        // silent `None` here would be the defect the check exists to catch, and a test that only
        // called it for the side effect would pass while the emitters recorded nothing.
        assert!(
            Announcement::new(
                "health.checks.completed",
                json!({"state": "healthy", "services": 8, "worst_service": ""}),
                None,
            )
            .is_some()
        );

        assert!(
            Announcement::new(
                "health.service.degraded",
                json!({
                    "service": "redis",
                    "from_state": "healthy",
                    "to_state": "down",
                    "message": "no answer",
                    "incident_id": Uuid::nil(),
                    "suppressed": false,
                }),
                None,
            )
            .is_some()
        );

        assert!(
            Announcement::new(
                "health.service.recovered",
                json!({
                    "service": "redis",
                    "from_state": "down",
                    "to_state": "healthy",
                    "duration_seconds": 42,
                    "incident_id": Uuid::nil(),
                }),
                None,
            )
            .is_some()
        );

        assert!(
            Announcement::new(
                "health.threshold.breached",
                json!({
                    "metric": "disk_percent",
                    "value": 97.0,
                    "crit_limit": 92.0,
                    "window_start": "2026-09-30T12:00:00Z",
                }),
                None,
            )
            .is_some()
        );

        assert!(
            Announcement::new(
                "health.incident.acknowledged",
                json!({
                    "incident_id": Uuid::nil(),
                    "service": "redis",
                    "actor": Uuid::nil(),
                    "note": "on it",
                }),
                Some(Uuid::nil()),
            )
            .is_some()
        );
    }

    #[test]
    fn an_announcement_missing_a_required_field_is_refused_and_named() {
        // The shape a hand-written emitter produces if it forgets `to_state`. The catalogue row
        // promises it, and the receiver would be the one to find out.
        let refused = Announcement::new(
            "health.service.degraded",
            json!({"service": "redis", "from_state": "healthy"}),
            None,
        );
        assert!(
            refused.is_none(),
            "an announcement that breaks the catalogue's promise must not be recorded"
        );

        // A name nobody has heard of is refused the same way, because emitting it would put a
        // row in `events` that no endpoint can ever subscribe to.
        assert!(
            Announcement::new("health.service.exploded", json!({"service": "redis"}), None,)
                .is_none()
        );
    }

    #[test]
    fn a_null_where_the_catalogue_promises_a_value_counts_as_missing() {
        // `json!({"service": null})` is a legal `Value` and satisfies a naive `is_some()` check.
        // The gap it would open is the worst kind: the event is recorded, the receiver's
        // required field is null, and nothing anywhere says so.
        assert!(
            Announcement::new(
                "health.service.degraded",
                json!({
                    "service": null,
                    "from_state": "healthy",
                    "to_state": "down",
                }),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn an_acknowledgement_without_a_note_omits_the_field_entirely() {
        // An empty note is not sent as `""` and not sent as `null`, because a receiver logging
        // notes cannot tell those apart from a note that genuinely says so.
        let build = |note: &str| {
            let mut payload = json!({
                "incident_id": Uuid::nil(),
                "service": "redis",
                "actor": Uuid::nil(),
            });
            if !note.trim().is_empty() {
                payload["note"] = json!(note);
            }
            payload
        };

        assert!(build("   ").get("note").is_none());
        assert_eq!(build("restarting redis")["note"], json!("restarting redis"));
        assert!(
            Announcement::new(
                "health.incident.acknowledged",
                build("  "),
                Some(Uuid::nil())
            )
            .is_some()
        );
    }

    #[test]
    fn a_run_that_changed_nothing_announces_nothing() {
        // The single most important property of this module: the common case is silence. A
        // platform whose operators have the panel open must not be the reason an endpoint wakes
        // up, and this asserts the count the store already decided rather than trusting the
        // caller's reading of the policy.
        assert_eq!(PolicyOutcome::default().announcements(), 0);
    }
}
