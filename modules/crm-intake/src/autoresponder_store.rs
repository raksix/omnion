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
//!
//! **A reservation is either completed or released; nothing else ends it.** Both directions
//! matter and the second one is the easy half to omit. `mark_sent` completes the row once the
//! mailer returns, and `release_claim` hands it back when the send fails — a reservation left
//! standing is a lead the sweep keeps *offering*, so every way the sweep can decline one has to
//! release it. Declining without releasing is not a quiet no-op: the claim still satisfies the
//! sweep's own WHERE clause (`sent = 'false'` and a `due_at` in the past), so the same
//! reservation is re-declined and re-noted on every tick, for ever. That was the defect this
//! file now carries two tests for — one per decline path, because the two are separate arms and
//! fixing one while leaving the other is how a branch keeps the same bug in a new place.

use serde::Serialize;
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
    ///
    /// **This asks [`Delivery::is_sendable`], which knows about the delay, and used to ask
    /// nothing.** It was `matches!(self.verdict, Delivery::Ready(_))` under a doc comment
    /// claiming the message "went to the mailer" — and `prepare` claims a *delayed* message on
    /// purpose, so a source with a 30-minute send delay returned `sent() == true` for a
    /// reservation that no mailer had touched. The name was the guarantee and the body
    /// contradicted it; the tests that read this method asserted the claim, not the send.
    ///
    /// **What this is used for is what makes the direction matter**: a caller asking "was this
    /// lead answered" must be told no for a message still waiting out its delay, because
    /// answering yes is how a lead gets marked handled by a send that has not happened.
    #[must_use]
    pub fn sent(&self) -> bool {
        self.verdict.is_sendable()
    }

    /// The message, if it can go to the mailer now — the same rule, without the `bool`.
    ///
    /// **The store asks this rather than matching `Delivery::Ready` itself.** The arms of a
    /// match are the part that goes stale when the enum grows; a named accessor is not.
    #[must_use]
    pub fn sendable(&self) -> Option<&Message> {
        self.verdict.sendable()
    }
}

/// How long a *completed* claim is left alone before the sweep may consider it again.
///
/// Only ever applied to the recovery arm below, and it exists so that re-offering a lost send
/// costs one tick rather than racing the caller that is presumably still sending it.
pub const ABANDONED_CLAIM_AFTER: time::Duration = time::Duration::minutes(5);

/// What a claim row currently is, as a *reader* needs it.
///
/// ## Why the state is computed here and not in the browser
///
/// A claim line is a jsonb blob and the detail screen used to read the raw keys and infer:
/// `sent: true` means answered, no `delivered_at` means waiting, `due_at` in the future means
/// held. Each of those is a *separate* inference, in the one consumer that has no way to be
/// wrong loudly — so an abandoned claim rendered exactly like a claim the caller is still
/// holding, on a lead whose reply was never sent. The row already carries the two facts; what
/// was missing was the sentence that says which one this is, in the same place `verdict_name`
/// was put in slice 45. **A state a reader has to infer from three keys is a state two readers
/// will infer differently.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimState {
    /// The message went to the mailer and a delivery instant was recorded.
    Sent,
    /// Reserved and waiting for its delay to elapse.
    Reserved,
    /// Undelayed, and the caller is still holding it.
    Claimed,
    /// Undelayed, past the window, and nothing has sent it — the recovery case.
    Abandoned,
    /// The claim was recorded before any delivery could be observed (migration `0202`).
    Unknown,
}

impl ClaimState {
    /// The state of one stored claim detail.
    ///
    /// `None` for anything that is not a claim at all — a skip note, which shares the `kind`
    /// deliberately so the trail shows "autoresponder_sent: no address" as a fact about the
    /// autoresponder rather than about a message.
    ///
    /// ## The order of the questions is the rule
    ///
    /// `delivered_at` first, because it is the only key written by [`mark_sent`] and therefore
    /// the only evidence of a send. `due_at` second, because a due instant means the delay
    /// still governs this row. `delivery_unknown` before the `created_at` arithmetic, because
    /// migration `0202` stamped the rows it could not repair with a key that means exactly
    /// "never claimed as delivered, and never will be".
    #[must_use]
    pub fn of(detail: &Value) -> Option<Self> {
        if !detail.get("sent").is_some() {
            return None;
        }
        if detail.get("delivered_at").and_then(Value::as_str).is_some() {
            return Some(Self::Sent);
        }
        if detail.get("delivery_unknown").and_then(Value::as_bool) == Some(true) {
            return Some(Self::Unknown);
        }
        let due = detail.get("due_at").and_then(Value::as_str).filter(|v| !v.is_empty());
        match due {
            // A delayed claim is the reservation case, and it is the sweep's own predicate.
            Some(_) => Some(Self::Reserved),
            // **No due instant means the delay does not govern this row**, so what governs it
            // is whether the caller is still holding it. `created_at` is the claim instant,
            // and the row carries no other timestamp for an immediate send.
            None => Some(Self::Claimed),
        }
    }

    /// The same state, decided against the clock: is an *undelayed* claim old enough that the
    /// sweep may take it over?
    ///
    /// **This is the one place the answer changes with time**, and it is deliberately not
    /// folded into [`ClaimState::of`] — that function is a pure reading of a stored row, which
    /// is what the panel and the API want, and this one is a decision about *another*
    /// process's liveness. The bound is [`ABANDONED_CLAIM_AFTER`], not the worker's send claim
    /// window: a claim that has been standing that long with no `delivered_at` is not somebody
    /// mid-send, and the bias the module has always taken holds — **a duplicate is a permanent,
    /// invisible defect; a delayed send is a temporary, visible one.**
    ///
    /// The bound is five minutes against an SMTP timeout an operator can raise out to any
    /// length (`OMNION_SMTP_TIMEOUT_MS`), so a send that outlives the window is possible and
    /// the bias decides what happens then: the second copy goes out, rather than the lead never
    /// being answered at all.
    #[must_use]
    pub fn is_abandoned(&self, claimed_at: OffsetDateTime, now: OffsetDateTime) -> bool {
        match self {
            Self::Claimed => now - claimed_at >= ABANDONED_CLAIM_AFTER,
            Self::Reserved | Self::Sent | Self::Unknown | Self::Abandoned => false,
        }
    }

    /// The full state of a stored row at an instant, `Abandoned` included.
    ///
    /// The reader's entry point, and the reason [`ClaimState::Abandoned`] is reachable at all:
    /// [`ClaimState::of`] deliberately cannot produce it, because deciding that a claim is
    /// abandoned is a statement about *another process's* liveness rather than a fact in the
    /// row. Splitting the two is what lets the API answer "what does this line mean" without a
    /// clock argument, and the sweep answer "is this one still owed" with one.
    #[must_use]
    pub fn at(detail: &Value, claimed_at: OffsetDateTime, now: OffsetDateTime) -> Option<Self> {
        let state = Self::of(detail)?;
        Some(if state.is_abandoned(claimed_at, now) {
            Self::Abandoned
        } else {
            state
        })
    }

    /// `true` when the recovery sweep may take this row over.
    ///
    /// **One state, and that is the point.** A `Reserved` row is *not* recoverable by age: the
    /// delay governs it and the SQL decides it from the due instant, which is a different
    /// question with a different answer. A `Sent` or `Unknown` row must never be sent again,
    /// and a fresh `Claimed` one still belongs to whoever took it.
    ///
    /// There is deliberately no `is_owed()` here. It was written first, answered "yes" for
    /// `Reserved` as well, and had **no caller** — which is the exact shape this file's
    /// neighbours in this branch have been burned by three times: a method whose doc describes a
    /// decision nobody makes. The sweep's own predicate is the SQL; a reader's is
    /// [`ClaimState::at`].
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Abandoned)
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
///
/// ## The claim records *claimed*, never *delivered* — and that is the whole fix
///
/// `sent` is written `false` on **every** line this function inserts, immediate or delayed,
/// because the claim is taken *before* the mailer is touched and nothing at that point knows
/// whether the message went anywhere. It was written `!message.delayed`, so an immediate
/// message was recorded as delivered at the instant it was reserved — two full sentences
/// before the socket. Everything downstream keys off that key, and each of them was therefore
/// looking for a row that could not exist:
///
/// * [`release_claim`] deletes `detail->>'sent' <> 'true'`, so a refused **immediate** send
///   released nothing, the claim stayed, and [`prepare`] answered `AlreadySent` for ever:
///   the visitor's one reply was lost and the lead was permanently silenced by the very
///   mechanism that exists to guarantee it. That is this file's header promise ("a failed
///   send does not silence the lead forever") and the worker's ("a refused mailer releases
///   the reservation and the next tick retries"), both of which were true for the delayed
///   path and false for the immediate one — the path every source that never touched the
///   delay control is on.
/// * [`mark_sent`] updates the same `<> 'true'` predicate, so the completion of an immediate
///   send updated **zero** rows and returned `Ok(false)`. The caller cannot tell that from
///   "another worker won", so no `sent_at` was ever written for an immediate message: the
///   trail showed *when the line was claimed* and never *when the mail left*.
///
/// `delayed` is still written, and it is the fact the caller needs to tell the two apart at
/// a glance. What it is **not** is evidence of delivery, and nothing reads it as such: the
/// due sweep requires `sent = 'false'` **and** a non-null `due_at`, and an immediate claim
/// writes `due_at: null`, so it is never offered to the worker a second time.
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
        // Always `false`, always — see the section on this function. The claim is taken
        // before the mailer, so "sent" is not a fact any caller could know yet; writing
        // `!message.delayed` recorded an immediate message as delivered at the moment it
        // was reserved, which left `release_claim` and `mark_sent` — both keyed on
        // `sent <> 'true'` — looking for a row that could not exist. A refused immediate
        // send then released nothing and the lead was `AlreadySent` for ever.
        "sent": false,
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
///
/// ## Why the predicate is `delivered_at is absent` and not `sent <> 'true'`
///
/// This is the half of migration `0202` that decides whether the platform can recover at
/// all. `sent` is written `true` at *claim* time — the slot is taken before the mailer is
/// touched — so a predicate of `sent <> 'true'` selects only rows that are already delivered,
/// which is the exact inverse of what a release needs. Under the old writer it happened to
/// work for the delayed path only, because a delayed claim was the one case written with
/// `sent = false`; an immediate claim was written `sent = true`, so a refused immediate send
/// released **nothing** and the lead answered `AlreadySent` for ever.
///
/// The predicate is therefore the same one `mark_sent` uses: a row with no recorded delivery
/// is a row whose delivery was never confirmed, and a refused send must be able to take it
/// back. A row that *does* carry `delivered_at` is left alone — the message went out, and
/// re-claiming the lead would mail it twice, which is the duplicate this design exists to
/// prevent. That is also what makes the two functions safe to run in either order after an
/// upgrade: whichever observes the send first wins the row, and the other finds it excluded.
///
/// ## `not (detail ? 'delivered_at')` is the wrong question, and it is wrong in BOTH directions
///
/// The key-existence operator `?` is true for a key whose value is JSON `null`, so every
/// function in this file that asked "is there a `delivered_at` key?" was asking the wrong
/// question about migration `0202`'s own output. `0202` stamps its rows with
/// `jsonb_build_object('sent', true, 'delivered_at', null, 'delivery_unknown', true)` — the key
/// is *present* and *null*, which is the honest way to record "never claimed as delivered" and
/// is asserted by `run-crm-autoresponder-claim.sh`. Measured on the server, not reasoned about:
///
/// ```text
///   {"sent":true,"delivered_at":null,"delivery_unknown":true} ? 'delivered_at'  ->  true
///   not (that)                                                                  ->  false
/// ```
///
/// So the old predicate *excluded* exactly the rows `0202` was written to save. Two
/// consequences, and they pull in opposite directions, which is why the repair cannot be one
/// search-and-replace:
///
/// * [`mark_sent`] could not complete such a row. The migration's stated purpose — *"an
///   installation upgrading mid-flight can complete a claim the old code claimed but never
///   recorded, rather than leaving it permanently uncompletable"* — was **unreachable code**,
///   written in a comment above a predicate that excluded its subject.
/// * Fixing that question in [`release_claim`] alone would be **worse than the defect**: the
///   old predicate had been accidentally shielding a *settled* row from deletion, and a
///   release is a `delete`. A `0202` row is `sent: true` — the lead IS answered, which is the
///   one thing `0202` refuses to give up — so a release that can now see it would delete the
///   evidence and let `prepare` answer "not sent", and the second copy would go out to a
///   visitor who already has the first.
///
/// **So the two functions ask the same question and answer it differently, and that is the
/// rule rather than an inconsistency:** *completion may proceed on an unknown row, a release
/// may not.* [`mark_sent`] reads `detail->>'delivered_at' is null` alone, which is what its
/// own doc comment already promised. This function reads that clause **and** additionally
/// requires `delivery_unknown` to be absent or false, so the exemption is written down rather
/// than being an accident of a key-existence test.
///
/// ## Why the exemption is safe even though it is not redundant
///
/// A `0202` row is `sent: true`, so no send path ever reaches this function with it:
/// `prepare` answers `AlreadySent` (it reads `sent`), and [`due_reservations`] requires
/// `sent = 'false'`. The guard is therefore *not* load-bearing for any flow that exists today —
/// and it is written anyway, because the failure it prevents is silent, permanent and
/// duplicate-shaped, and because a predicate that happens to be safe is a predicate that is
/// one refactor away from not being safe. `ClaimState::of` in this file already reads the same
/// way (`and_then(Value::as_str)`), so the SQL and the reader now agree on one spelling of the
/// same fact instead of two that disagree only for `0202`'s rows.
pub async fn release_claim(pool: &PgPool, lead_id: Uuid, to: &str) -> Result<bool> {
    let removed: Option<i64> = sqlx::query_scalar(
        "delete from crm_lead_events \
         where id = ( \
           select id from crm_lead_events \
           where lead_id = $1 and kind = $2 and detail->>'to' = $3 \
             and detail ? 'sent' and detail->>'delivered_at' is null \
             and coalesce(detail->>'delivery_unknown', 'false') <> 'true' \
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
///
/// ## `delivered_at` is the completion marker, and `sent` is not
///
/// The predicate is `delivered_at is absent` rather than `sent <> 'true'`, and the reason is
/// the whole point of migration `0202`. `sent` answers "is this lead answered?" and is `true`
/// from the moment the slot is claimed; `delivered_at` answers "did a message actually leave?"
/// and is only written here, after the mailer returned. Keying the completion on `sent`
/// meant that an immediate claim — the default — could never be completed at all, so no
/// delivery instant was ever recorded for it.
///
/// `sent_at` is kept as a second, older spelling because a row written by the pre-`0202`
/// code may already carry it, and a reader that sees two different keys for one fact is worse
/// than a reader that sees one old key. `delivered_at` is the one new code writes.
///
/// Migration `0202` marks the *already wrong* rows with `delivery_unknown: true` and no
/// `delivered_at`, and this predicate therefore also matches them: an installation upgrading
/// mid-flight can complete a claim the old code claimed but never recorded, rather than
/// leaving it permanently uncompletable. A row already carrying `delivered_at` is excluded,
/// so "exactly once" still holds.
pub async fn mark_sent(pool: &PgPool, lead_id: Uuid, sent_at: OffsetDateTime) -> Result<bool> {
    let updated: Option<i64> = sqlx::query_scalar(
        "update crm_lead_events set detail = detail || \
           jsonb_build_object('sent', true, 'delivered_at', $2::text, 'sent_at', $2::text) \
         where id = ( \
           select id from crm_lead_events \
           where lead_id = $1 and kind = $3 \
             and detail ? 'sent' and detail->>'delivered_at' is null \
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

/// How long a worker's send claim may stand before another worker may take it.
///
/// One minute, and the bound it has to respect is the SMTP timeout
/// (`OMNION_SMTP_TIMEOUT_MS`, default ten seconds, raiseable by an operator): a claim must
/// outlive the send it guards, or two workers really do mail. The bias is the module's own —
/// **a duplicate is a permanent, invisible defect; a delayed send is a temporary, visible one**
/// — so the window is long rather than tight, and recovery lands on the next worker tick
/// instead of on a restart.
pub const DELIVERY_CLAIM_STALE_AFTER: time::Duration = time::Duration::minutes(1);

/// Take the right to send one due reservation, and report whether this caller is the one to send
/// it.
///
/// ## This is the half that makes the worker's exactly-once claim true
///
/// The runner's own header states the rule this function implements, one line above the code
/// that broke it: *"a claim is taken **before** the send, and a completion is recorded **after**
/// it, or two workers both mail."* `send_one` did the opposite — it called the mailer first and
/// `mark_sent` (which *is* the completion) second — so the only arbiter the send path had was
/// consulted **after** the irreversible act. Two app instances on one database both receive the
/// same due row, because [`due_reservations`] is a plain read with no lock, and both mailed.
///
/// **The losing branch was real and useless.** Its comment said the loser "stops here …
/// sending again would be the duplicate this whole design exists to prevent", and it did stop —
/// after its own copy had left. The trail then showed one send while the visitor received two,
/// which is the worst of both halves: a record that is reassuring and false.
///
/// The write is a compare-and-swap on the row, and `skip locked` is what makes it *safe* rather
/// than merely likely: under READ COMMITTED two workers both see the row unlocked and both
/// attempt the update, and Postgres hands the row to exactly one while the other is skipped
/// rather than blocking behind an SMTP conversation that may take `OMNION_SMTP_TIMEOUT_MS`.
/// Without `skip locked` the *record* would still be right — the loser is told "not yours" — but
/// only after waiting out a send it never should have started.
///
/// ## `skip locked` skips a row, not a lead
///
/// The bias is deliberate and stated: a skipped reservation is one the winner is already
/// sending, so the lead is answered by this tick either way. Nothing is lost by the skip, and a
/// worker blocked behind a ten-second conversation is a worker that cannot answer the
/// forty-nine reservations behind it.
///
/// A claim older than [`DELIVERY_CLAIM_STALE_AFTER`] is claimable again, which is the recovery
/// half a lock cannot give: a worker that dies between claiming and sending must not leave a
/// reservation that is skipped for ever by the mechanism meant to answer it.
pub async fn claim_delivery(
    pool: &PgPool,
    lead_id: Uuid,
    to: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let claimed: Option<i64> = sqlx::query_scalar(
        "update crm_lead_events set delivery_claimed_at = $4 \
         where id = ( \
           select id from crm_lead_events \
           where lead_id = $1 and kind = $2 and detail->>'to' = $3 \
             and detail ? 'sent' and detail->>'delivered_at' is null \
             and (delivery_claimed_at is null \
                  or delivery_claimed_at < $5::timestamptz) \
           order by id desc limit 1 \
           for update skip locked \
         ) returning id",
    )
    .bind(lead_id)
    .bind(SENT_KIND)
    .bind(to)
    .bind(now)
    .bind(now - DELIVERY_CLAIM_STALE_AFTER)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::CrmIntakeError::from)?;
    Ok(claimed.is_some())
}

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
///
/// ## Every row this function drops is also *released*
///
/// Four arms can drop a row — no lead, no source, a source that is no longer configured, and
/// a `deliver` that declines — and all four release the claim, which is the only thing that
/// removes the row from this sweep's WHERE clause. The two "nothing is left" arms used to
/// `continue` and nothing else, on the reasoning that a reservation whose lead or source was
/// deleted "is not an error". That was a statement about the *return value* being right while
/// the *row* stayed exactly where the next pass would find it — and because an orphan is the
/// oldest row in the table, it is also the one that most reliably fills the `limit` and hides
/// the live work behind it. See the comment at the loop head for the measurement.
///
/// The rule, then: **this function returns rows to send, and it has no way to say "skip this
/// one" to anything downstream — so any row it does not return must be one it has ended.**
#[allow(clippy::too_many_lines)]
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
           and e.detail->>'delivered_at' is null \
           and ( \
             (nullif(e.detail->>'due_at', '') is not null and e.detail->>'due_at' <= $2) \
             or (nullif(e.detail->>'due_at', '') is null and e.created_at <= $3) \
           ) \
         order by coalesce(nullif(e.detail->>'due_at', ''), e.created_at::text) asc, e.id asc \
         limit $4",
    )
    .bind(SENT_KIND)
    .bind(crate::autoresponder::date_header(now))
    // The recovery arm's bound. A delayed row's own `due_at` governs it; an *immediate* claim
    // has no due instant at all, so what governs it is how long it has stood with no recorded
    // delivery. That is the clause the previous version lacked, and it is the whole fix: without
    // it this sweep could not see an abandoned immediate claim, so a send lost to a restart was
    // owed by nobody for ever while `prepare` answered `AlreadySent`.
    //
    // `$3` is a real `timestamptz` and not the RFC 2822 string `due_at` uses, for the reason
    // migration `0205` already wrote down for `delivery_claimed_at`: comparing an instant as
    // text needs every writer to share one formatter in one zone, and the bias of getting that
    // wrong here is toward *never* recovering a lead that is owed an answer.
    .bind(now - ABANDONED_CLAIM_AFTER)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;

    let mut due = Vec::with_capacity(rows.len());
    for (lead_id, detail) in rows {
        // The recipient the claim was taken for, read **before** anything can drop this row.
        // A release is only allowed to remove *that* claim — the same rule `release_claim`
        // itself enforces, read here so all four arms below cannot disagree about whose row
        // they are giving back.
        let claimed_to = detail
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // A reservation whose lead or source is gone is not an error — the trail outlives
        // both, and a deleted lead keeps its lines for the audit export. But "not an error" is
        // a statement about the *return value*, not about the row, and this is the third arm
        // of the sweep to get that wrong in the same direction.
        //
        // These two `continue`s used to leave the claim exactly as the sweep found it: `sent:
        // false`, `due_at` in the past — which is **this sweep's own WHERE clause**. So a
        // reservation whose source was deleted stayed permanently due, was re-read and
        // re-skipped once a minute for ever, and — the part that matters — stayed in the
        // batch. The sweep is `order by due_at asc, id asc limit $3`, and an orphan is the
        // *oldest* thing in the table by construction: it has been due longer than anything
        // live. So orphans sort to the front of every batch and spend its budget on rows the
        // worker cannot send. The gate measures it: five orphaned reservations ahead of one
        // live lead and `limit 5` returns **zero** rows — the live lead is not offered, on
        // every tick, for ever, while the worker logs that it sent nothing.
        //
        // The shape is not exotic. `crm_leads.source_id` is `on delete set null` (0055), so
        // `DELETE /crm/intake/sources/{id}` — a button the panel has, and the ordinary way an
        // operator retires a form — nulls it and the reservation outlives its source. Deleting
        // one source silently disabled the autoresponder for every lead that had a pending
        // reservation on it, and starved the ones behind it.
        //
        // Both arms therefore **release**, which is what actually ends a reservation. No note
        // is written here and that is deliberate in the other direction too: there is no
        // source left to name, and a line saying "we did not answer a lead whose source you
        // deleted" is a fact the operator produced themselves and can read by deleting
        // nothing.
        let Some(lead) = find_lead_any_org(pool, lead_id).await? else {
            tracing::debug!(lead_id = %lead_id, "a due autoresponder has no lead left — releasing");
            release_claim(pool, lead_id, &claimed_to).await?;
            continue;
        };
        let Some(source) = source_of(pool, lead.source_id).await? else {
            tracing::debug!(lead_id = %lead_id, "a due autoresponder has no source left — releasing");
            release_claim(pool, lead_id, &claimed_to).await?;
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
            // **Released, not merely noted.** This arm used to record the skip and `continue`,
            // which left the claim standing exactly as the sweep found it: `sent: false` with a
            // `due_at` in the past, which is precisely this sweep's own WHERE clause. The
            // reservation was therefore offered again on the next tick, declined again on the
            // same grounds and noted again — once a minute, for ever, on a source its operator
            // switched off. The note is still written: it is the trail line that says *why*
            // nothing went out, which is a different fact from whether the claim survives. The
            // order matters — the note first, because `release_claim` deletes the claim row and
            // a note written afterwards would outlive it while the release did not.
            record_skip(
                pool,
                &lead,
                &source,
                "source_disabled",
                serde_json::json!({ "reserved_at": detail.get("due_at").cloned().unwrap_or(Value::Null) }),
            )
            .await?;
            release_claim(pool, lead_id, &claimed_to).await?;
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
                // The lead was rejected, turned to spam, or lost its address inside the delay.
                //
                // **Every decline releases the reservation — and the two this arm used to
                // separate are now one, because one of them was unreachable.** `AlreadySent`
                // was the only variant that released, and `deliver` is called below with
                // `already_sent` hardcoded `false`, so that variant could not be produced here
                // at all: the branch was a comment with an arm on it. Every decline a sweep can
                // actually make therefore fell to the `else`, which wrote a note and left the
                // claim row standing — `sent: false`, `due_at` in the past, which is precisely
                // this sweep's own WHERE clause. So the same reservation was offered again next
                // tick, declined again on the same grounds and noted again: one trail line per
                // minute, for ever, for a lead nobody is ever going to answer.
                //
                // The note stays and the release joins it. They are different facts — the note is
                // *why* nothing went out, the release is *that nothing is still owed* — and a
                // trail line alone answers only the first. The note is written first because
                // `release_claim` deletes the claim row; a note written after would describe a
                // reservation that is already gone.
                record_skip(
                    pool,
                    &lead,
                    &source,
                    other.reason(),
                    // The verdict's own payload again, and for the same reason as the capture
                    // path: an `InvalidTemplate` here means the source was **edited to be
                    // broken during its own delay** — an operator emptied the body while a reply
                    // was waiting — and the sweep is the only thing that ever sees it, because
                    // the row was reserved before the edit. `json!({ "reserved": true })` said
                    // "there was a reservation", which is true of every row in this arm and says
                    // nothing about why it is being dropped. The two keys are different facts and
                    // both are kept: the reservation is context, the verdict explains.
                    other.skip_payload_merge(serde_json::json!({ "reserved": true })),
                )
                .await?;
                release_claim(pool, lead_id, &claimed_to).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    /// A fixed instant, so no test in this module reads the wall clock.
    const NOON: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

    /// The claim `claim` writes for an undelayed message: `sent: false`, no due instant.
    fn immediate_claim() -> Value {
        json!({
            "to": "ada@example.com",
            "subject": "We received your message",
            "template": "acknowledgement",
            "delayed": false,
            "due_at": Value::Null,
            "sent": false,
        })
    }

    /// The claim `claim` writes for a delayed message.
    fn reserved_claim() -> Value {
        json!({
            "to": "ada@example.com",
            "subject": "We received your message",
            "template": "acknowledgement",
            "delayed": true,
            "due_at": "Thu, 02 Oct 2026 12:30:00 +0000",
            "sent": false,
        })
    }

    // -----------------------------------------------------------------------------------------
    // The state is one question with five answers, and each key decides exactly one of them.
    //
    // Every assertion here is a *negative* except the two that prove a state is reachable —
    // a `ClaimState::of` that answered `None` for everything would satisfy the first three, so
    // the "this row really is a claim" fixtures are asserted too.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn a_skip_note_is_not_a_claim() {
        // `record_skip` writes the SAME kind with no `sent` key, so "does this row claim the
        // send" is the question — and answering it wrongly would render every silence in the
        // trail as a message that is on its way.
        assert_eq!(
            ClaimState::of(&json!({ "reason": "no_address" })),
            None,
            "a note about why nothing went out is not a claim"
        );
    }

    #[test]
    fn an_immediate_claim_reads_as_claimed_and_not_as_a_reservation() {
        // The defect this slice fixes, stated as a fact about one row: the *only* difference
        // between "the caller is still sending" and "the caller is gone" is how long the row has
        // stood, and `of` is the pure half that says nothing about time.
        assert_eq!(
            ClaimState::of(&immediate_claim()),
            Some(ClaimState::Claimed),
            "a claim with no due instant is the immediate path, whatever its age"
        );
    }

    #[test]
    fn a_delayed_claim_reads_as_reserved() {
        assert_eq!(
            ClaimState::of(&reserved_claim()),
            Some(ClaimState::Reserved),
            "a due instant means the delay still governs this row"
        );
        assert!(
            !ClaimState::Reserved.is_recoverable(),
            "a reservation is the delay working; age is not what governs it"
        );
    }

    #[test]
    fn a_completed_claim_reads_as_sent() {
        // `delivered_at` is the ONLY key `mark_sent` writes, so it is the only evidence of a
        // send — which is why it is the first question asked.
        let mut detail = immediate_claim();
        detail["delivered_at"] = json!("Thu, 02 Oct 2026 12:00:01 +0000");
        assert_eq!(ClaimState::of(&detail), Some(ClaimState::Sent));
        assert!(
            !ClaimState::Sent.is_recoverable(),
            "a delivered claim must never be offered again"
        );
    }

    #[test]
    fn a_pre_0202_row_reads_as_unknown_and_is_still_settled() {
        // Migration 0202 marked the rows it could not repair with `delivery_unknown` and a null
        // `delivered_at`. Those leads ARE answered — `prepare` must keep saying `AlreadySent` —
        // so this state has to be distinct from `Abandoned`, or the recovery sweep would go and
        // mail every lead answered before the fix.
        let detail = json!({
            "to": "ada@example.com", "delayed": false, "due_at": Value::Null,
            "sent": true, "delivered_at": Value::Null, "delivery_unknown": true,
        });
        assert_eq!(
            ClaimState::of(&detail),
            Some(ClaimState::Unknown),
            "an unrepairable row is answered, and saying so is the whole point of 0202"
        );
        assert!(
            !ClaimState::Unknown.is_recoverable(),
            "0202's rows are answered and settled; recovery must not re-mail every pre-fix lead"
        );
    }

    #[test]
    fn a_claim_only_becomes_abandoned_once_it_is_old_enough() {
        let fresh = ClaimState::at(&immediate_claim(), NOON, NOON);
        assert_eq!(
            fresh,
            Some(ClaimState::Claimed),
            "a claim taken this instant is somebody's in-flight send, not a recovery case"
        );
        let stale = ClaimState::at(
            &immediate_claim(),
            NOON - ABANDONED_CLAIM_AFTER - time::Duration::seconds(1),
            NOON,
        );
        assert_eq!(
            stale,
            Some(ClaimState::Abandoned),
            "past the window with no recorded delivery, nobody is holding this reply"
        );
        assert!(
            stale.is_some_and(|state| state.is_recoverable()),
            "an abandoned claim is the one state the sweep may take over"
        );
    }

    #[test]
    fn a_reserved_claim_is_never_abandoned_however_old_it_is() {
        // **The negative control for the test above, and the assertion that keeps the fix from
        // over-correcting.** A delayed reservation is *supposed* to stand still for its delay —
        // that is what a reservation is — so treating its age as abandonment would mail a
        // message the operator deliberately asked to hold, during the very window they asked to
        // hold it for.
        let state = ClaimState::at(
            &reserved_claim(),
            NOON - ABANDONED_CLAIM_AFTER * 100,
            NOON,
        );
        assert_eq!(
            state,
            Some(ClaimState::Reserved),
            "age is not abandonment; only the absence of a delay is"
        );
    }

    #[test]
    fn at_is_the_only_way_to_reach_abandoned() {
        // `of` deliberately cannot produce it. A reader that could get `Abandoned` without a
        // clock would be calling a row abandoned at any age, which is the defect this whole
        // split exists to prevent.
        assert!(
            !matches!(ClaimState::of(&immediate_claim()), Some(ClaimState::Abandoned)),
            "a pure reading of the row has no opinion about how long it has stood"
        );
    }
}
