//! The autoresponder's write half: claim the send, hand it to the mailer, record the delivery.
//!
//! The claim is the whole point of this file, so it is worth saying plainly how it works and
//! why the obvious version is wrong.
//!
//! The obvious version reads the trail, sees no `autoresponder_sent` line, and sends. It is
//! wrong because the keyed endpoint is called by servers that retry, the browser resends form
//! posts, and `capture` is idempotent on the submission id — so two requests can both be past
//! the read and both send. A visitor who receives the acknowledgement twice has been told the
//! platform is untrustworthy in its first thirty seconds.
//!
//! So the claim is a write that *loses* the race: a single conditional insert that succeeds
//! for exactly one caller, and every other caller reads a row it did not write and sends
//! nothing. The claim is taken **before** the send rather than after, because the alternative
//! — send, then record — has no way to recover a process that dies between the two, and the
//! failure mode of that is a visitor who is never answered because the platform crashed while
//! answering them. A claim that is later released on a send failure is the recoverable
//! direction: the retry re-claims it and answers.
//!
//! A claim for a *delayed* message is not a send at all — it reserves the slot and names the
//! instant the message becomes due, so a lead answered after the delay is not answered twice
//! and a lead answered before it is not answered at all.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::autoresponder::{Autoresponder, Delivery, Message, Recipient};
use crate::error::Result;
use crate::model::{IntakeSource, Lead};

/// The trail line the delivery writes.
pub const SENT_KIND: &str = "autoresponder_sent";

/// The outcome of trying to answer one lead, as the caller and the trail both see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What the decision layer said.
    pub verdict: Delivery,
    /// The lead the decision was about.
    pub lead_id: Uuid,
}

impl Outcome {
    /// `true` when a message actually went to the mailer.
    #[must_use]
    pub fn sent(&self) -> bool {
        matches!(self.verdict, Delivery::Ready(_))
    }
}

/// Read a lead's autoresponder claim, if it already has one.
///
/// A *sent* line blocks a second send. A *claimed* line for a message that has not gone out
/// yet is released by [`release_claim`] if the mailer refused it, so a failed send does not
/// silence the lead forever.
pub async fn existing_claim(pool: &PgPool, lead_id: Uuid) -> Result<Option<Value>> {
    let row: Option<Value> = sqlx::query_scalar(
        "select detail from crm_lead_events \
         where lead_id = $1 and kind = $2 order by id desc limit 1",
    )
    .bind(lead_id)
    .bind(SENT_KIND)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::CrmIntakeError::from)?;
    Ok(row)
}

/// `true` when the last claim for this lead is a completed send rather than a pending one.
///
/// Reading "is there a claim?" instead of "was it sent?" is the bug this exists to prevent: a
/// lead whose first attempt claimed the slot and then failed would be recorded as answered
/// forever, and the visitor would never hear back.
#[must_use]
pub fn was_sent(detail: &Value) -> bool {
    detail.get("sent").and_then(Value::as_bool).unwrap_or(false)
}

/// Take the claim for one lead, or report that somebody else already has it.
///
/// Returns `true` when this caller owns the send. The insert is conditional on there being no
/// prior `autoresponder_sent` line for the lead, which is what makes it a race that exactly
/// one caller can win — a read-then-write pair cannot, because the read is not part of the
/// same statement.
pub async fn claim(pool: &PgPool, lead: &Lead, message: &Message) -> Result<bool> {
    let detail = serde_json::json!({
        "to": message.to,
        "subject": message.subject,
        "template": message.template,
        "delayed": message.delayed,
        "due_at": message.due_at,
        "sent": !message.delayed,
    });
    // The claim is a plain insert whose *uniqueness* arbitrates the race, not a `where not
    // exists` guard. The guard version reads like a lock and is not one: under READ COMMITTED
    // each statement sees the rows committed when it began, so ten concurrent callers all
    // evaluate the subquery before any of them commits, and all ten insert. The gate measured
    // 8 winners out of 10 that way. Migration 0057's partial unique index makes the second
    // insert fail with `unique_violation`, and losing that race is the *answer* rather than an
    // error to report.
    //
    // `on conflict do nothing` is deliberately not used: it would also swallow the
    // `foreign_key_violation` of a lead that was deleted mid-flight, and a caller that cannot
    // tell "somebody else answered" from "this lead is gone" will answer a lead that no longer
    // exists.
    let claimed: Result<Option<i64>, sqlx::Error> = sqlx::query_scalar(
        "insert into crm_lead_events (lead_id, kind, actor_user_id, detail) \
         values ($1, $2, null, $3) returning id",
    )
    .bind(lead.id)
    .bind(SENT_KIND)
    .bind(&detail)
    .fetch_optional(pool)
    .await;

    let claimed = match claimed {
        Ok(row) => row,
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => None,
        Err(error) => return Err(crate::error::CrmIntakeError::from(error)),
    };

    if claimed.is_none() {
        return Ok(false);
    }
    // A delayed message is *reserved*, not sent: the line is written now with `sent: false`
    // and a due instant, so the worker that sends it later updates the same claim rather than
    // adding a second line that reads as a second message.
    if message.delayed {
        tracing::debug!(lead_id = %lead.id, due_at = ?message.due_at, "autoresponder reserved");
    } else {
        tracing::info!(lead_id = %lead.id, to = %message.to, "autoresponder claimed for send");
    }
    Ok(true)
}

/// Release a claim whose send failed, so the next attempt may answer the lead.
///
/// Only a line this caller's own failed send may be removed: the condition matches the
/// recipient as well as the lead, so a claim belonging to a different address (a lead whose
/// e-mail was corrected between attempts) is left alone.
pub async fn release_claim(pool: &PgPool, lead_id: Uuid, to: &str) -> Result<bool> {
    let removed: Option<i64> = sqlx::query_scalar(
        "delete from crm_lead_events \
         where id = ( \
           select id from crm_lead_events \
           where lead_id = $1 and kind = $2 and detail->>'to' = $3 and detail->>'sent' <> 'true' \
           order by id desc limit 1 \
         ) returning id",
    )
    .bind(lead_id)
    .bind(SENT_KIND)
    .bind(to)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::CrmIntakeError::from)?;
    Ok(removed.is_some())
}

/// Mark a claimed message as actually delivered, and record when.
///
/// The claim is updated rather than re-inserted so the timeline shows one line that grew a
/// `sent_at`, rather than a reservation and a delivery that a reader has to correlate.
pub async fn mark_sent(pool: &PgPool, lead_id: Uuid, sent_at: OffsetDateTime) -> Result<bool> {
    let updated: Option<i64> = sqlx::query_scalar(
        "update crm_lead_events set detail = detail || jsonb_build_object('sent', true, 'sent_at', $2::text) \
         where id = ( \
           select id from crm_lead_events \
           where lead_id = $1 and kind = $3 and detail->>'sent' <> 'true' \
           order by id desc limit 1 \
         ) returning id",
    )
    .bind(lead_id)
    .bind(crate::autoresponder::date_header(sent_at))
    .bind(SENT_KIND)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::CrmIntakeError::from)?;
    Ok(updated.is_some())
}

/// Record that the autoresponder was *not* sent, and why.
///
/// A lead whose autoresponder found no address, a broken template or a rejected submission
/// gets a line saying so. The alternative — no line at all — makes the trail read as "we sent
/// nothing here" and "we never considered it" as the same event, which is exactly the question
/// an operator opens a lead to answer.
pub async fn record_skip(
    pool: &PgPool,
    lead: &Lead,
    source: &IntakeSource,
    reason: &str,
    detail: Value,
) -> Result<()> {
    let mut payload = detail;
    if let Some(object) = payload.as_object_mut() {
        object.insert("reason".to_string(), Value::String(reason.to_string()));
        object.insert("source".to_string(), Value::String(source.name.clone()));
    }
    crate::store::append_event(pool, lead.id, SENT_KIND, None, payload).await
}

/// Decide, claim and describe what one accepted lead gets — without sending.
///
/// Splitting the decision from the send is what lets the caller drive the mailer: this
/// function owns "what should go out and may I be the one to send it", and the caller owns
/// the socket.
pub async fn prepare(
    pool: &PgPool,
    lead: &Lead,
    source: &IntakeSource,
    now: OffsetDateTime,
) -> Result<Outcome> {
    let autoresponder = Autoresponder::from_json(&source.autoresponder);
    let accepted = matches!(
        lead.status.as_str(),
        "new" | "assigned" | "contacted" | "qualified"
    );
    let reason = match lead.status.as_str() {
        "spam" => "spam",
        "duplicate" => "duplicate",
        "rejected" => "rejected",
        _ => "rejected",
    };
    let context = Recipient {
        address: lead.email.as_deref(),
        first_name: lead.first_name.as_deref().unwrap_or_default(),
        source_name: source.name.as_str(),
        product_interest: lead.product_interest.as_deref().unwrap_or_default(),
        accepted,
        reason,
    };

    let already_sent = existing_claim(pool, lead.id)
        .await?
        .as_ref()
        .is_some_and(was_sent);
    let verdict = autoresponder.deliver(&context, now, already_sent);

    // A claim is only taken for a message that is ready *now*; a delayed one is reserved by
    // the worker that sends it, so `capture` does not write a reservation the moment nobody
    // is going to honour for an hour.
    if let Delivery::Ready(message) = &verdict {
        if !message.delayed && !claim(pool, lead, message).await? {
            return Ok(Outcome {
                verdict: Delivery::AlreadySent,
                lead_id: lead.id,
            });
        }
    }

    Ok(Outcome {
        verdict,
        lead_id: lead.id,
    })
}
