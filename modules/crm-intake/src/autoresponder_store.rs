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
    // `detail ? 'sent'` is what makes this the CLAIM rather than merely the newest line of
    // this kind. `record_skip` writes the same `kind` — the trail shows "autoresponder_sent:
    // no address" as a fact about the autoresponder — and ordering by `id desc` alone would
    // return that note, which carries no `sent` key, so `was_sent` reads `false` and the next
    // attempt believes the lead was never answered.
    let row: Option<Value> = sqlx::query_scalar(
        "select detail from crm_lead_events \
         where lead_id = $1 and kind = $2 and detail ? 'sent' \
         order by id desc limit 1",
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
    // `due_at` is written as the *formatted string*, never as the `OffsetDateTime` itself.
    // `serde_json::json!` has no special case for `time::OffsetDateTime`, so the value
    // serialises as serde's component array — `[2026, 272, 3, 11, 51, 855202933, 0, 0, 0]` —
    // and `detail->>'due_at'` then yields NULL for a JSON *array*, not the text. The sweep's
    // `->>` predicate would match nothing, forever, and the row would look like a reservation
    // with no due instant: a lead waiting for a message no query can ever find.
    //
    // The gate caught this one only because it reads the stored JSON rather than the struct
    // that produced it. `message.due_at` in Rust is a perfect `Option<OffsetDateTime>`, the
    // unit tests are green, and the column is the only place the two disagree.
    //
    // The format is `date_header` (RFC 2822) rather than RFC 3339 for one reason: it is what
    // the reader already expects, it is fixed-width and zero-padded, and therefore sorts
    // chronologically as text — which is what lets the due index be a plain text expression
    // index (a `::timestamptz` cast is STABLE and Postgres refuses it there).
    let detail = serde_json::json!({
        "to": message.to,
        "subject": message.subject,
        "template": message.template,
        "delayed": message.delayed,
        "due_at": message.due_at.map(crate::autoresponder::date_header),
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
           where lead_id = $1 and kind = $2 and detail->>'to' = $3 \
             and detail ? 'sent' and detail->>'sent' <> 'true' \
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
           where lead_id = $1 and kind = $3 \
             and detail ? 'sent' and detail->>'sent' <> 'true' \
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

    // A claim is taken for *every* ready message, delayed or not, because the claim is what
    // arbitrates the send. The earlier version skipped the reservation for a delayed message
    // on the reasoning that "the worker will claim it" — and the worker does not: it only
    // *completes* reservations. The result was a dead control: a source with any send delay at
    // all reserved nothing, the worker found nothing, and the visitor was never answered. The
    // delay is the feature; the reservation is what makes it happen later instead of never.
    if let Delivery::Ready(message) = &verdict {
        if !claim(pool, lead, message).await? {
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

/// A reservation that has come due and is waiting to be sent.
///
/// `PartialEq` without `Eq`: both members carry a `serde_json::Value`, which is not `Eq`.
/// Deriving `Eq` on `Lead` and `IntakeSource` to satisfy a struct that only ever gets
/// compared in a test would put a promise about two shared model types into a third file.
#[derive(Debug, Clone, PartialEq)]
pub struct DueReservation {
    /// The lead the message answers.
    pub lead: Lead,
    /// The source that reserved it.
    pub source: IntakeSource,
    /// The message, re-rendered from the source's template at the moment it is sent.
    pub message: Message,
}

/// The reservations whose delay has elapsed, oldest first.
///
/// A source can configure a send delay so the acknowledgement lands *after* the
/// salesperson's own reply instead of racing it. That is a real feature, and it has a
/// consequence that only shows up later: `capture` reserves the slot and something has to
/// come back and send it. Without this function the delay is a control that makes the reply
/// silently never go out — worse than not having it, because the operator watches the
/// autoresponder working on every source they left at zero and has no reason to suspect the
/// ones they did not.
///
/// Ordered by the instant the message came due, so a lead that waited a week is answered
/// before one that waited a minute, and bounded by `limit` so one tick cannot pick up ten
/// thousand reservations and hold the pool for the length of ten thousand mailers.
pub async fn due_reservations(
    pool: &PgPool,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<DueReservation>> {
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(
        // The comparison is on the *string*, not on a cast, and that is what lets the partial
        // expression index serve this query. It is also only correct because one function
        // writes this key: `claim` formats `due_at` with `date_header` (RFC 2822), which is
        // fixed-width and zero-padded, so lexical order is chronological order within a zone.
        // A cast here would be `STABLE` and would make Postgres fall back to a full scan of
        // the trail once a minute — or, worse, an index that sorts differently per machine.
        // `nullif` covers the two shapes a hand-edited row can hold: no key at all (NULL) and
        // an empty string, which a cast would reject with `invalid input syntax for type
        // timestamp` and turn the whole worker into a 500.
        "select e.lead_id, e.detail \
         from crm_lead_events e \
         where e.kind = $1 \
           and e.detail->>'sent' = 'false' \
           and nullif(e.detail->>'due_at', '') is not null \
           and e.detail->>'due_at' <= $2 \
         order by e.detail->>'due_at' asc, e.id asc \
         limit $3",
    )
    .bind(SENT_KIND)
    .bind(crate::autoresponder::date_header(now))
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;

    let mut due = Vec::with_capacity(rows.len());
    for (lead_id, detail) in rows {
        // A reservation whose lead or source is gone is not an error. The trail outlives
        // both — a deleted lead keeps its lines for the audit export — and a sweep that
        // refused to move on would retry the same dead row on every tick for ever.
        let Some(lead) = find_lead_any_org(pool, lead_id).await? else {
            tracing::debug!(lead_id = %lead_id, "a due autoresponder has no lead left");
            continue;
        };
        let Some(source) = source_of(pool, lead.source_id).await? else {
            tracing::debug!(lead_id = %lead_id, "a due autoresponder has no source left");
            continue;
        };

        // The message is *re-rendered* from the source's template, not read back out of the
        // claim. The claim stores the recipient, the subject and the template name, and
        // deliberately not the body: `crm_lead_events.detail` is read by the lead's detail
        // screen and by every audit export, and a rendered body is the lead's own words back
        // to them. Re-rendering costs one thing — a template edited inside the delay sends
        // the new wording — and buys the other: a source that was *switched off* inside the
        // delay stops answering, which is what an operator who turned it off asked for.
        let autoresponder = Autoresponder::from_json(&source.autoresponder);
        if !autoresponder.is_configured() {
            tracing::info!(
                lead_id = %lead_id,
                "a reserved autoresponder's source is no longer configured — not sending"
            );
            record_skip(
                pool,
                &lead,
                &source,
                "source_disabled",
                serde_json::json!({ "reserved_at": detail.get("due_at").cloned().unwrap_or(Value::Null) }),
            )
            .await?;
            continue;
        }
        let context = Recipient {
            address: lead.email.as_deref(),
            first_name: lead.first_name.as_deref().unwrap_or_default(),
            source_name: source.name.as_str(),
            product_interest: lead.product_interest.as_deref().unwrap_or_default(),
            accepted: matches!(
                lead.status.as_str(),
                "new" | "assigned" | "contacted" | "qualified"
            ),
            reason: "",
        };
        let message = match autoresponder.deliver(&context, now, false) {
            Delivery::Ready(message) => message,
            other => {
                // The lead was rejected or turned to spam inside the delay. The reservation
                // is released rather than left pending, so the trail stops promising a mail
                // that will never be justified to answer.
                if matches!(other, Delivery::AlreadySent) {
                    release_claim(pool, lead_id, detail.get("to").and_then(Value::as_str).unwrap_or_default())
                        .await?;
                } else {
                    record_skip(
                        pool,
                        &lead,
                        &source,
                        other.reason(),
                        serde_json::json!({ "reserved": true }),
                    )
                    .await?;
                }
                continue;
            }
        };

        due.push(DueReservation {
            lead,
            source,
            message,
        });
    }
    Ok(due)
}

/// Read a lead without an organization filter, for the sweeper.
///
/// The sweep is already scoped — it starts from a reservation, and the reservation is on the
/// lead — and the organization is a property of the lead rather than of the caller. A sweep
/// that filtered by the caller's organization would need one, and a reservation whose source
/// was moved between organizations would then be stranded.
async fn find_lead_any_org(pool: &PgPool, lead_id: Uuid) -> Result<Option<Lead>> {
    Ok(sqlx::query_as::<_, Lead>(&format!(
        "select {} from crm_leads where id = $1",
        crate::store::LEAD_COLUMNS
    ))
        .bind(lead_id)
        .fetch_optional(pool)
        .await?)
}

/// The source a lead's autoresponder belongs to.
async fn source_of(pool: &PgPool, source_id: Option<Uuid>) -> Result<Option<IntakeSource>> {
    let Some(source_id) = source_id else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, IntakeSource>(&format!(
        "select {} from crm_intake_sources where id = $1",
        crate::store::SOURCE_COLUMNS
    ))
    .bind(source_id)
    .fetch_optional(pool)
    .await?)
}
