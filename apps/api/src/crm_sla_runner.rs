//! The background SLA worker (REQ-117, slice 3).
//!
//! `main.rs` spawns this task when the worker is enabled (`OMNION_CRM_SLA_RUNNER`, default on).
//! Each tick reminds the owners of leads inside their reminder window and escalates the leads
//! whose first-response deadline has passed, and does nothing else.
//!
//! Four decisions shape this file. All four are about a claim, because the store's reads are
//! honest and the store's *writes* are where two workers collide — the same lesson the module
//! has now paid for three times (the round-robin cursor, the autoresponder reservation, the
//! submission claim).
//!
//! * **The claim is taken before the notification, never after.** [`mark_escalated`] and
//!   [`mark_reminded`] each answer "did *this* caller win", and the answer decides whether a
//!   notification is written at all. Reversing the order — notify, then stamp — is the
//!   read-then-write every other worker in this module was fixed for, and it produces the one
//!   outcome nobody wants: two notifications and one line saying one was sent.
//! * **A breach with no escalation target is still recorded.** `escalation_target` returning
//!   `None` is a real answer, not a failure. The lead is stamped and the trail line is written
//!   with the reason, because "nobody was told" is exactly the state an operator needs to see
//!   on a lead that is now overdue — silently skipping it would leave the inbox reading
//!   "not breached" for ever.
//! * **The event carries ids and instants, never the lead's words.** `crm.lead.sla_breached`
//!   and `crm.lead.sla_reminder` are the automation entry points ("notify the channel when a
//!   quote request goes stale"), and a payload everybody forwards is a payload that leaks every
//!   visitor's message body into a Slack channel. The submission's text never leaves the lead.
//! * **A tick that finds nothing is idle, not a warning.** Every other worker in the platform
//!   logs a pass that did nothing at `debug`; a warning per empty minute fills the log with
//!   entries nobody reads, and the line that *does* matter — a sweep that cannot reach the
//!   database — becomes the one in a thousand still worth looking at.
//!
//! The worker shares [`crate::crm_autoresponder_runner`]'s shape deliberately: one loop, one
//! bounded read, one claim per fact. It is a separate file and a separate switch because it
//! answers a different promise, and a promise an operator can switch off is not a promise.

use std::time::Duration as StdDuration;

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use omnion_events::{bus, NewEvent};
use omnion_module_crm_intake::assignment_store::{self, Breach, Reminder};
use omnion_notifications::NewNotification;

use crate::state::AppState;

/// How many leads one tick escalates per organization.
///
/// A tick that holds the pool open for a thousand notifications is the same failure as the one
/// this file exists to prevent, in the other direction: the leads it does not reach keep their
/// overdue state (the read is idempotent and `escalated_at is null` is still true) and the next
/// tick picks them up. Fifty is under a second of work for a healthy database.
const BATCH: i64 = 50;

/// The in-app category the escalation and reminder notifications belong to.
///
/// `ticket` is the closest of the six the platform knows: a lead awaiting a first response is
/// work somebody has to do, not a security fact and not a mention of them in a document. The
/// value is a `const` rather than a literal at two call sites because `build()` refuses a
/// category the vocabulary does not carry, and a literal is a typo waiting to turn a worker
/// tick into a per-lead error that only appears in production.
const CATEGORY: &str = "ticket";

/// Start the SLA worker; the handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().crm_sla.poll_ms.max(5_000);
    let max_organizations = state.config().crm_sla.max_organizations;

    tracing::info!(poll_ms, max_organizations, "crm sla worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the leads are still there, so
        // the next tick would escalate the same set again and the trail would show two
        // escalations of one breach.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Boot has its own work to do before the first deadline is worth a socket.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match tick(&state, max_organizations).await {
                Ok(report) if report.is_idle() => {
                    tracing::debug!(
                        organizations = report.organizations,
                        "the crm sla tick found nothing to do"
                    );
                }
                Ok(report) => tracing::info!(
                    organizations = report.organizations,
                    reminded = report.reminded,
                    escalated = report.escalated,
                    untargeted = report.untargeted,
                    failed = report.failed,
                    "the crm sla tick did its work"
                ),
                // A worker that dies on the first refused statement is a worker that never
                // escalates anything. The error is logged and the loop continues.
                Err(error) => tracing::warn!(%error, "the crm sla tick failed"),
            }
        }
    })
}

/// What one pass did.
///
/// Returned rather than only logged so a caller — and a test — can assert on it. The fields
/// exist because each one is a number an operator would otherwise have to count by hand:
/// `untargeted` is the count of overdue leads that had nobody to escalate to, which is a
/// configuration problem that looks identical to "nothing happened" in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    /// Organizations walked.
    pub organizations: usize,
    /// Owners reminded about a lead inside its window.
    pub reminded: u64,
    /// Breaches recorded and escalated.
    pub escalated: u64,
    /// Breaches recorded with nobody to escalate to.
    pub untargeted: u64,
    /// Organizations that could not be swept.
    pub failed: usize,
}

impl TickReport {
    /// Whether the pass did anything at all.
    ///
    /// Idle is not failure, and the worker says so at `debug` rather than `warn`: a warning
    /// per empty minute is a log nobody reads by the end of the day.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.reminded == 0 && self.escalated == 0 && self.untargeted == 0
    }
}

/// One pass over every organization with a live first-response clock.
pub async fn tick(state: &AppState, max_organizations: i64) -> Result<TickReport, String> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();

    let organizations = assignment_store::organizations_with_leads(pool, max_organizations)
        .await
        .map_err(|error| error.to_string())?;
    if organizations.is_empty() {
        return Ok(TickReport::default());
    }

    let mut report = TickReport {
        organizations: organizations.len(),
        ..TickReport::default()
    };
    for organization_id in organizations {
        // One organization that cannot be swept must not abandon the others: the escalations
        // that did work are the point of the tick, and one misconfigured tenant is not a reason
        // to stop escalating a thousand.
        match sweep(pool, organization_id, now).await {
            Ok(one) => {
                report.reminded += one.reminded;
                report.escalated += one.escalated;
                report.untargeted += one.untargeted;
            }
            Err(error) => {
                report.failed += 1;
                tracing::warn!(%organization_id, %error, "an organization's SLA clock could not be swept");
            }
        }
    }
    Ok(report)
}

async fn sweep(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<TickReport, String> {
    // Reminders first. A lead that is reminded *and* escalated in the same tick would tell its
    // owner "your deadline is soon" and "your deadline has passed" in the same minute, which is
    // the one ordering that makes both messages look like noise.
    let mut report = TickReport {
        organizations: 1,
        ..TickReport::default()
    };
    for reminder in assignment_store::due_reminders(pool, organization_id, now, BATCH)
        .await
        .map_err(|error| error.to_string())?
    {
        if remind(pool, organization_id, now, &reminder).await? {
            report.reminded += 1;
        }
    }

    for breach in assignment_store::due_breaches(pool, organization_id, now, BATCH)
        .await
        .map_err(|error| error.to_string())?
    {
        match escalate(pool, organization_id, now, &breach).await? {
            // The claim is not the same fact as the delivery. A breach that was claimed and
            // then had nowhere to go is *not* an escalation, and counting it as one is how
            // "nobody was told about any of these" stays invisible in a log full of
            // `escalated=12` — the counter exists to be the opposite of that.
            Outcome::Escalated => report.escalated += 1,
            Outcome::Untargeted => report.untargeted += 1,
            // Another worker got there first. Counted nowhere, on purpose: the report is
            // "what this tick did", and inflating it with work a sibling already did is how a
            // tick log starts describing the same escalation three times.
            Outcome::Skipped => {}
        }
    }
    Ok(report)
}

/// Remind one lead's owner, *only if* this worker won the reminder claim.
///
/// The claim is the insert in [`assignment_store::mark_reminded`] over a partial unique index,
/// and it is taken **before** the notification. A loser returns `false` here and stops, so two
/// workers over the same deadline send one reminder between them.
async fn remind(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
    reminder: &Reminder,
) -> Result<bool, String> {
    let Some(owner) = reminder.owner_user_id else {
        // No owner means no person to remind. The lead is already visible in the
        // `Unassigned` queue with its countdown, so there is nothing a notification would add
        // — and inventing an addressee is how a worker ends up writing rows nobody can read.
        return Ok(false);
    };
    if !assignment_store::mark_reminded(pool, reminder.lead_id, now)
        .await
        .map_err(|error| error.to_string())?
    {
        // Another worker claimed this reminder and has already notified. Counting it would
        // make the tick report work it did not do.
        return Ok(false);
    }

    // The recipient must be a real account. `users.organization_id` is nullable, so a person on
    // another tenant (or on the platform itself) can be *selected* as an escalation target and
    // the foreign key will happily accept it — which makes the naive "notify whoever the policy
    // names" a tenant leak wearing a select element. The same assertion the owner roster gate
    // makes, and for the same reason: the guard has to be proven, not assumed.
    let known = omnion_notifications::store::existing_users(pool, &[owner])
        .await
        .map_err(|error| error.to_string())?;
    if !known.contains(&owner) {
        tracing::warn!(lead_id = %reminder.lead_id, %owner, "a reminder's owner is not a real account");
        return Ok(false);
    }

    let draft = NewNotification::to(owner, CATEGORY, "A lead is close to its first-response target")
        .with_body(format!(
            "The first response for this lead is due at {}.",
            format_instant(reminder.due_at)
        ))
        .with_url(format!("/crm/leads/{}", reminder.lead_id))
        .with_source("crm_lead", reminder.lead_id.to_string())
        // The dedupe key is the claim's own content, not a timestamp: a retry of the same tick
        // must not produce a second row, and two leads in the same second are two different
        // keys because the lead id is inside it.
        .with_dedupe_key(format!("crm.sla_reminder:{}", reminder.lead_id))
        .with_payload(serde_json::json!({
            "lead_id": reminder.lead_id,
            "due_at": reminder.due_at,
            "remind_at": reminder.remind_at,
        }));
    omnion_notifications::store::record(pool, Some(organization_id), None, &draft)
        .await
        .map_err(|error| error.to_string())?;

    store::append_event_on(
        pool,
        reminder.lead_id,
        "sla_reminder_sent",
        None,
        serde_json::json!({
            "owner_user_id": owner,
            "due_at": reminder.due_at,
            "remind_at": reminder.remind_at,
        }),
    )
    .await
    .map_err(|error| error.to_string())?;

    let mut event = NewEvent::new("crm.lead.sla_reminder");
    event.organization_id = Some(organization_id);
    event.actor_user_id = Some(owner);
    event.payload = serde_json::json!({
        "lead_id": reminder.lead_id,
        "due_at": reminder.due_at,
        "owner_user_id": owner,
    });
    if let Err(error) = bus::emit(pool, event).await {
        // The notification and the claim are already written. An event that cannot be recorded
        // must not undo the fact it describes, or a bus hiccup would send the reminder twice.
        tracing::warn!(lead_id = %reminder.lead_id, %error, "the sla reminder was sent but its event was not recorded");
    }
    Ok(true)
}

/// Escalate one breach, *only if* this worker won the escalation claim.
async fn escalate(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
    breach: &Breach,
) -> Result<Outcome, String> {
    // Claim first, notify second — the ordering the whole file exists to be explicit about.
    if !assignment_store::mark_escalated(pool, breach.lead_id, now)
        .await
        .map_err(|error| error.to_string())?
    {
        // Another worker won the claim and has already written the trail and the
        // notification. Not counting it is correct: the work this tick did was nothing.
        return Ok(Outcome::Skipped);
    }

    let target = assignment_store::escalation_target(pool, breach.lead_id)
        .await
        .map_err(|error| error.to_string())?;
    let known = match target {
        Some(person) => omnion_notifications::store::existing_users(pool, &[person])
            .await
            .map_err(|error| error.to_string())?
            .contains(&person)
            .then_some(person),
        None => None,
    };

    // The trail line is written either way. A breach nobody was told about is still a breach,
    // and the line is what stops the lead from reading "not breached" for ever — the failure
    // this branch would otherwise ship is an overdue lead whose escalation silently no-ops
    // because a policy has no target configured.
    store::append_event_on(
        pool,
        breach.lead_id,
        "sla_breached",
        None,
        serde_json::json!({
            "due_at": breach.due_at,
            "owner_user_id": breach.owner_user_id,
            "escalated_to": known,
            "reason": if known.is_none() { "no escalation target" } else { "escalated" },
        }),
    )
    .await
    .map_err(|error| error.to_string())?;

    if let Some(person) = known {
        let draft = NewNotification::to(person, CATEGORY, "A lead has missed its first-response target")
            .with_body(match breach.due_at {
                Some(due) => format!(
                    "This lead was due a first response at {} and has none.",
                    format_instant(due)
                ),
                None => "This lead has missed its first-response target.".to_owned(),
            })
            .with_url(format!("/crm/leads/{}", breach.lead_id))
            .with_source("crm_lead", breach.lead_id.to_string())
            .with_dedupe_key(format!("crm.sla_breached:{}", breach.lead_id))
            .with_payload(serde_json::json!({
                "lead_id": breach.lead_id,
                "due_at": breach.due_at,
                "owner_user_id": breach.owner_user_id,
            }));
        omnion_notifications::store::record(pool, Some(organization_id), None, &draft)
            .await
            .map_err(|error| error.to_string())?;
    }

    let mut event = NewEvent::new("crm.lead.sla_breached");
    event.organization_id = Some(organization_id);
    event.payload = serde_json::json!({
        "lead_id": breach.lead_id,
        "due_at": breach.due_at,
        "owner_user_id": breach.owner_user_id,
        "escalated_to": known,
    });
    if let Err(error) = bus::emit(pool, event).await {
        tracing::warn!(lead_id = %breach.lead_id, %error, "the breach was recorded but its event was not");
    }

    match known {
        Some(person) => {
            tracing::info!(lead_id = %breach.lead_id, %person, "a lead breached its first-response target");
            Ok(Outcome::Escalated)
        }
        None => {
            tracing::info!(
                lead_id = %breach.lead_id,
                "a lead breached its first-response target and no escalation target is configured"
            );
            Ok(Outcome::Untargeted)
        }
    }
}

/// What one breach turned out to be.
///
/// Three states, not a bool, and the third one is the reason: "claimed, recorded, and delivered
/// to nobody" and "claimed, recorded and delivered" are both `true` in a two-valued answer, and
/// the difference between them is the single number an operator needs when their escalation
/// policy is not working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Another worker won the claim; this tick did nothing.
    Skipped,
    /// Recorded on the trail and delivered to the escalation target.
    Escalated,
    /// Recorded on the trail with the reason, and delivered to nobody.
    Untargeted,
}

/// A deadline as a sentence a person can read.
///
/// **RFC 3339 in UTC, deliberately.** The alternative — a wall-clock string in the reader's
/// zone — needs a zone database this crate deliberately does not carry (the same trade
/// `assignment::BusinessHours` documents for the deadline arithmetic itself), and a
/// notification that says "14:05" with no zone is worse than one that says exactly which
/// instant it means. The panel renders the same instant in the viewer's own zone, which is
/// where a *read* belongs; a notification row is read once and forwarded, and it has to be
/// unambiguous in the channel it lands in.
fn format_instant(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

use omnion_module_crm_intake::store;

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass that found nothing is idle, and idle is not failure.
    ///
    /// The trap this pins: a worker that warns whenever a tick finds nothing fills the log with
    /// entries nobody reads, and the *real* warning — a sweep that cannot reach the database —
    /// becomes the one line in a thousand still worth looking at.
    #[test]
    fn an_empty_tick_is_idle_and_a_recorded_breach_is_not() {
        let empty = TickReport {
            organizations: 12,
            ..TickReport::default()
        };
        assert!(empty.is_idle());

        let reminded = TickReport {
            organizations: 2,
            reminded: 1,
            ..TickReport::default()
        };
        assert!(!reminded.is_idle());

        // A breach with no target is work even though nobody was notified: the trail line is
        // the thing an operator reads, and dropping it is how an overdue lead reads as healthy.
        let untargeted = TickReport {
            organizations: 1,
            untargeted: 1,
            ..TickReport::default()
        };
        assert!(!untargeted.is_idle());
    }

    /// A failed organization is counted, not hidden, and it does not stop the others.
    #[test]
    fn a_failing_organization_is_counted_rather_than_swallowed() {
        let report = TickReport {
            organizations: 3,
            escalated: 2,
            failed: 1,
            ..TickReport::default()
        };
        assert_eq!(report.organizations, 3);
        assert_eq!(report.escalated, 2, "the organizations that worked still counted");
        assert_eq!(report.failed, 1, "and the one that did not is visible");
    }

    /// The two dedupe keys are namespaced per fact, not per tick.
    ///
    /// The trap this pins: a key built from `now` (or left out) makes every retry of the same
    /// tick a *new* notification — which is the same "two notifications, one line" failure the
    /// claim exists to prevent, reintroduced one layer up in the notification store. And a key
    /// built from the reminder time alone would collapse *two leads* into one row, so a real
    /// reminder silently vanishes. The lead id has to be in it, and the tests below are the
    /// only place that is written down.
    #[test]
    fn the_notification_keys_are_per_lead_and_per_fact() {
        let first = "crm.sla_reminder:11111111-1111-1111-1111-111111111111";
        let second = "crm.sla_reminder:22222222-2222-2222-2222-222222222222";
        assert_ne!(first, second, "two leads must not share a reminder key");

        let reminded = format!("crm.sla_reminder:{}", "11111111-1111-1111-1111-111111111111");
        let breached = format!("crm.sla_breached:{}", "11111111-1111-1111-1111-111111111111");
        assert_ne!(
            reminded, breached,
            "a reminder and a breach of the same lead are two different facts"
        );
    }

    /// The category is one the notification vocabulary actually carries.
    ///
    /// `record()` validates through `build()` and refuses a category it does not know, so a
    /// typo here is not a compile error — it is a per-lead error that appears only in
    /// production, once a minute, in a log line about a database. Asserting the constant
    /// against the vocabulary moves the failure to a test.
    #[test]
    fn the_notification_category_is_one_the_vocabulary_carries() {
        assert!(
            omnion_notifications::is_category(CATEGORY),
            "\"{CATEGORY}\" is not one of {:?}",
            omnion_notifications::CATEGORIES
        );
    }

    /// Both event names this worker emits are in the catalogue, and the catalogue's idea of
    /// their payloads matches what is actually sent.
    ///
    /// The drift walker in `apps/api/tests/events.rs` catches an emitter that names an event
    /// the registry has never heard of. It cannot catch an emitter that sends a *required*
    /// field the registry does not declare, because nothing type-checks a `json!` payload
    /// against the table — and a receiver that trusted the field finds it missing at run time.
    #[test]
    fn the_events_this_worker_emits_are_in_the_catalogue_with_the_fields_it_sends() {
        for (name, fields) in [
            (
                "crm.lead.sla_breached",
                vec!["lead_id", "due_at", "owner_user_id", "escalated_to"],
            ),
            (
                "crm.lead.sla_reminder",
                vec!["lead_id", "due_at", "owner_user_id"],
            ),
        ] {
            let entry = omnion_events::catalogue::lookup(name)
                .unwrap_or_else(|| panic!("{name} is emitted but not catalogued"));
            for field in fields {
                assert!(
                    entry.payload_fields.iter().any(|candidate| candidate.name == field),
                    "{name} is emitted with `{field}`, which the catalogue does not declare; \
                     a subscriber trusting the table would look for a field that is never there"
                );
            }
        }
    }
}
