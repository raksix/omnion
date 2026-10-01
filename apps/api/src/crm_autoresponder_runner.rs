//! The background autoresponder worker (REQ-117, slice 3).
//!
//! `main.rs` spawns this task when the worker is enabled
//! (`OMNION_CRM_AUTORESPONDER_RUNNER`, default on). Each tick sends the autoresponders whose
//! *send delay* has elapsed, and does nothing else.
//!
//! Four decisions shape this file, and three of them are about a race that the obvious
//! implementation loses silently.
//!
//! * **The tick is a minute, not a day.** A delay is a promise about minutes — a source that
//!   answers after ten minutes has to answer after ten minutes, not after the next nightly
//!   pass. The idle cost is one indexed query against a table holding only *pending*
//!   reservations, so a broken worker shows up within a minute instead of the next morning.
//! * **The claim was already taken; the race is over the *send*.** `capture` reserved the
//!   slot when the lead arrived and this worker completes that reservation. Two app
//!   instances both see the same due row, so the completion is arbitrated the same way the
//!   claim was: [`mark_sent`] returns whether *this* caller was the one that updated the
//!   row. Reading the row and then sending is the read-then-write that this module has now
//!   had to learn about twice (the round-robin cursor, then the claim) — a claim is taken
//!   *before* the send, and a completion is recorded *after* it, or two workers both mail.
//! * **A refused mailer releases the reservation and the next tick retries.** The
//!   alternative — a reservation that stays pending on a refused SMTP connection — is a
//!   lead whose acknowledgement is silently lost while the trail insists it is coming.
//! * **A source switched off mid-delay stops answering.** The message is re-rendered from
//!   the template at send time rather than stored on the claim, so an operator who turned
//!   the autoresponder off does not keep paying for a delay they cancelled. That is also why
//!   the rendered *body* is never written to `crm_lead_events.detail`: that column is read
//!   by the lead's detail screen and by every audit export, and a rendered body is the
//!   lead's own words back to them.
//!
//! Nothing here emits an event. A send writes the trail line the detail screen already
//! renders, and a worker event with no actor is an event every subscriber must learn to
//! ignore.

use std::time::Duration as StdDuration;

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use omnion_automation::mail::MailSettings;
use omnion_automation::Email;
use omnion_module_crm_intake::autoresponder_store::{self, DueReservation};

use crate::state::AppState;
use crate::workflow_runner;

/// How many reservations one tick will take.
///
/// A tick that holds the pool for ten thousand mailer handshakes is the same failure as the
/// one this file exists to prevent, in the other direction. Fifty is under a second of work
/// for a healthy mailer and the next tick picks up the rest.
const BATCH: i64 = 50;

/// Start the autoresponder worker; the handle is kept by the binary (and ends with the
/// process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let poll_ms = state.config().crm_autoresponder.poll_ms.max(5_000);

    tracing::info!(poll_ms, "crm autoresponder worker started");

    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the reservations are still
        // there, so the next tick sends the same set again and the trail shows two sends of
        // one message rather than one enormous batch.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Boot has its own work to do before the first reservation is worth a socket.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            if let Err(error) = tick(&state).await {
                // A worker that dies on the first refused SMTP connection is a worker that
                // never answers anybody. The error is logged and the loop continues.
                tracing::warn!(%error, "the crm autoresponder tick failed");
            }
        }
    })
}

/// One pass: find what is due, send it, record what happened.
async fn tick(state: &AppState) -> Result<(), String> {
    let pool = state.db().pool();
    let due = autoresponder_store::due_reservations(pool, OffsetDateTime::now_utc(), BATCH)
        .await
        .map_err(|error| error.to_string())?;

    if due.is_empty() {
        return Ok(());
    }
    tracing::info!(count = due.len(), "sending due autoresponders");

    let settings = workflow_runner::mail_settings(state.config());
    for reservation in due {
        send_one(pool, &settings, reservation).await;
    }
    Ok(())
}

/// Send one reservation and record the outcome on its own claim line.
///
/// **The claim comes before the mailer, and that ordering is the whole fix.** This function
/// used to call the mailer first and `mark_sent` second, which is the exact order its own
/// header forbids: *"a claim is taken **before** the send, and a completion is recorded
/// **after** it, or two workers both mail."* `mark_sent` is the completion, so the send path's
/// only arbiter was consulted after the irreversible act, and the `Ok(false)` branch below —
/// commented "another worker had already completed this reservation … sending again would be
/// the duplicate this whole design exists to prevent" — stopped a call that had already sent.
///
/// Two app instances on one database both receive the same due row, because
/// [`autoresponder_store::due_reservations`] is a plain read. Both then reached this function,
/// and before the fix both mailed. The trail showed one send while the visitor received two.
async fn send_one(pool: &PgPool, settings: &MailSettings, reservation: DueReservation) {
    let DueReservation { lead, message, .. } = reservation;

    // The right to send, taken before the socket. A worker that loses this does not send and
    // does not release — the reservation belongs to the winner, and `release_claim` deleting a
    // row another worker is mid-send on is how a lead loses its answer entirely.
    match autoresponder_store::claim_delivery(pool, lead.id, &message.to, OffsetDateTime::now_utc())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                lead_id = %lead.id,
                "another worker holds the send claim for this autoresponder"
            );
            return;
        }
        Err(error) => {
            // Without a claim there is no safe send: sending first and recording afterwards is
            // the defect, so an unreadable claim means this tick skips the lead rather than
            // risking the duplicate. The reservation stays due and the next tick retries.
            tracing::warn!(lead_id = %lead.id, %error, "the autoresponder's send claim could not be read");
            return;
        }
    }

    let email = Email::new(message.to.clone(), message.subject.clone(), message.body.clone());

    match omnion_automation::mail::send(settings, &email).await {
        Ok(()) => match autoresponder_store::mark_sent(pool, lead.id, OffsetDateTime::now_utc()).await
        {
            // `false` here means another worker completed the same reservation. The mail has
            // gone out once, which is the promise, and the trail is already written by
            // whoever won — sending again would be the duplicate this whole design exists to
            // prevent, so the loser stops here.
            Ok(true) => tracing::info!(lead_id = %lead.id, to = %message.to, "autoresponder sent"),
            Ok(false) => tracing::warn!(
                lead_id = %lead.id,
                "another worker had already completed this autoresponder"
            ),
            Err(error) => {
                tracing::warn!(lead_id = %lead.id, %error, "the autoresponder went out but its claim was not updated");
                // Release so the next tick does not send a second copy of a message the
                // visitor already has. A second copy is worse than a trail line that reads
                // "not sent" for a message that was.
                let _ = autoresponder_store::release_claim(pool, lead.id, &message.to).await;
            }
        },
        Err(error) => {
            tracing::warn!(lead_id = %lead.id, %error, "a due autoresponder could not be sent");
            // The recoverable direction: the next tick re-claims it and tries again rather
            // than leaving a reservation that never resolves.
            if let Err(release) = autoresponder_store::release_claim(pool, lead.id, &message.to).await
            {
                tracing::warn!(lead_id = %lead.id, %release, "the autoresponder's claim could not be released");
            }
        }
    }
}
