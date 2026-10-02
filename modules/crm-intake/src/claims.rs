//! Delivery claims: the half of "one submission, one lead" that is not [`store::capture`]'s
//! own read.
//!
//! ## The defect this exists to close
//!
//! `capture` opened with a read — "has this source already seen this submission id?" — and then
//! inserted. Between the two statements there was no lock and nothing to collide with, so two
//! deliveries of the same `content.form.submitted` event arriving together both read *not
//! found* and both wrote a lead. Nothing raised, nothing logged, and the second lead is a
//! perfectly well-formed row with its own dedupe verdict: a duplicate, not an error.
//!
//! The shape is worth naming because it is the third time this branch has shipped it in a
//! different column. `run-crm-assignment.sh`'s header says it about the round-robin cursor: *"a
//! read-then-write cursor passes every sequential test and fails here, intermittently"*. The
//! idempotency read is the same mistake in a place no gate covered, and the REQ's own risk list
//! names the exposure: the platform's events are at-least-once by contract, and
//! `mark_quote_accepted` is idempotent on `converted_at` for exactly that reason.
//!
//! ## The shape of the fix
//!
//! A claim row per delivery identity, keyed `(source_id, submission_id)`. The insert is
//! `on conflict do nothing` and its *result* is the decision: a row inserted means this
//! delivery owns the submission, a row not inserted means the loser, and the loser then reads
//! the lead the winner is writing (or is about to). No advisory lock, no serializable retry
//! loop, no `select … for update` — a primary key does the work the application logic used to
//! promise it was doing.
//!
//! ## A claim is taken before the work and completed after
//!
//! Not in one transaction with the lead, and the reason is the failure it prevents. A claim held
//! in a transaction that the capture's own errors roll back is *releasable* — which sounds like
//! a feature and is the bug: a mapping error or a rate limit would free the key, and a burst of
//! retries would collide again on exactly the submissions that are already failing. Instead the
//! claim is written on its own and the lead is written with it, and a claim that is still open
//! says [`CLAIM_STALE_AFTER`]-old which of two things it is: a capture in flight, or a capture
//! that died. The first is left alone; the second is taken over by a compare-and-swap on
//! `claimed_at`, so two recoverers cannot both take over.
//!
//! This is the difference between a duplicate lead and a form that is wedged for ever. A bare
//! unique index would have given the first and paid for it with the second: the only remedy for
//! a crashed capture would have been a manual database write.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{CrmIntakeError, Result};

/// How old an **open** claim has to be before a second delivery may take it over.
///
/// Two seconds, and the number is a judgement with a stated bias: too short and a live capture
/// is stolen from under itself — the legitimate writer finishes, finds its claim gone, and the
/// submission is recorded twice, which is the exact defect this module exists to prevent. Too
/// long and a crashed capture blocks that one submission for that long, which is bounded,
/// silent, and self-healing. The bias is deliberate: **a duplicate is a permanent, invisible
/// data defect; a delayed submission is a temporary, visible one.** Twice the shortest plausible
/// full capture — a mapping, a contact lookup, an insert and an event — is the lower bound
/// worth taking.
pub const CLAIM_STALE_AFTER: time::Duration = time::Duration::seconds(2);

/// The id a caller may claim under, matching the header's cap in
/// `apps/api/src/routes/crm_intake.rs`.
pub const MAX_SUBMISSION_ID: usize = 128;

/// What a capture found when it asked for the submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claimed {
    /// This delivery owns the submission. Hold it, write the lead, then [`complete`].
    ///
    /// [`complete`]: Self::complete
    Owned {
        /// The instant the claim was taken, for the compare-and-swap in [`complete`].
        claimed_at: OffsetDateTime,
    },
    /// Another delivery owns it. `None` while that delivery is still writing the lead.
    Taken {
        /// The lead it produced, if it finished.
        lead_id: Option<Uuid>,
    },
}

/// Normalize a caller's submission id into the form the claim key stores.
///
/// Trimmed and capped, and both for the same reason the header is capped: this value reaches a
/// primary key, so an unbounded id is a key wide enough to be expensive. Returns `None` for
/// anything that is not a usable identity — the caller then treats the submission as having no
/// id at all, which is the honest reading of a blank one.
#[must_use]
pub fn normalize(id: &str) -> Option<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_SUBMISSION_ID).collect())
}

/// Take the claim for a submission, or report that somebody else holds it.
///
/// Three outcomes, and the order is the design:
///
/// 1. **A fresh insert wins the claim.** `on conflict do nothing` and the *row count* is the
///    answer — no advisory lock, no serializable retry, no `select … for update`. A primary key
///    does the work the application logic used to promise it was doing.
/// 2. **An open, stale claim is taken over.** This is the step a bare unique index cannot have,
///    and without it a crashed capture wedges that one submission for ever with only a manual
///    database write as the remedy. It runs on the *delivery* path, not on a sweeper's timer,
///    because the only moment recovery matters is the moment somebody retries — and a delivery
///    that heals its own predecessor needs no worker, no schedule and no cleanup to forget.
/// 3. **A completed claim answers with the lead its winner wrote.** Not a retry loop: a winner
///    that is still writing answers "taken, nothing yet", which is the correct answer rather
///    than a failure. The lead exists or is about to, and spinning until it appears has
///    re-introduced the load the claim was taken to shed.
pub async fn take(
    pool: &PgPool,
    source_id: Uuid,
    submission_id: &str,
) -> Result<Claimed> {
    let key = normalize(submission_id).ok_or_else(|| {
        CrmIntakeError::invalid("a submission id must be a non-empty value")
    })?;

    let inserted = sqlx::query(
        "insert into crm_lead_submissions (source_id, submission_id) \
         values ($1, $2) on conflict (source_id, submission_id) do nothing",
    )
    .bind(source_id)
    .bind(&key)
    .execute(pool)
    .await?
    .rows_affected();

    let now = sqlx::query_scalar::<_, OffsetDateTime>("select now()")
        .fetch_one(pool)
        .await?;

    if inserted == 1 {
        // Read the column back rather than reuse `now()`: the compare-and-swap in `complete`
        // compares against it, and an instant from a different clock is one the swap never
        // matches — which would read as a permanently incomplete claim.
        let claimed_at: OffsetDateTime = sqlx::query_scalar(
            "select claimed_at from crm_lead_submissions \
             where source_id = $1 and submission_id = $2",
        )
        .bind(source_id)
        .bind(&key)
        .fetch_one(pool)
        .await?;
        return Ok(Claimed::Owned { claimed_at });
    }

    // The claim exists. Before conceding, try to inherit a dead one — same statement, same
    // compare-and-swap a sweeper would use, so the two paths cannot disagree about what
    // "stale" means.
    if let Some(claimed_at) = take_over_stale(pool, source_id, &key, now).await? {
        return Ok(Claimed::Owned { claimed_at });
    }

    let lead_id: Option<Option<Uuid>> = sqlx::query_scalar(
        "select lead_id from crm_lead_submissions \
         where source_id = $1 and submission_id = $2",
    )
    .bind(source_id)
    .bind(&key)
    .fetch_one(pool)
    .await?;

    Ok(Claimed::Taken { lead_id: lead_id.flatten() })
}

/// Point a claim at the lead it produced.
///
/// The `claimed_at` comparison is not a lock and does not need to be: the only writer of a
/// claim is the delivery that took it, and the only other writer is a recoverer, which only
/// touches a claim older than [`CLAIM_STALE_AFTER`]. A capture that overruns that window and
/// finds its claim gone has **not** lost — the takeover winner holds it, and the lead it writes
/// is the same lead, so the honest answer is still a duplicate rather than a silent overwrite.
pub async fn complete(
    pool: &PgPool,
    source_id: Uuid,
    submission_id: &str,
    claimed_at: OffsetDateTime,
    lead_id: Uuid,
) -> Result<()> {
    let key = normalize(submission_id).ok_or_else(|| {
        CrmIntakeError::invalid("a submission id must be a non-empty value")
    })?;
    sqlx::query(
        "update crm_lead_submissions set lead_id = $3, completed_at = now() \
         where source_id = $1 and submission_id = $2 and claimed_at = $4",
    )
    .bind(source_id)
    .bind(&key)
    .bind(lead_id)
    .bind(claimed_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Take over a claim that was left open, and report whether this caller is the one that took it.
///
/// The compare-and-swap is on `claimed_at` **and** on the age, both in one statement, so the
/// answer is yes-or-no and never "yes to both of us": two recoverers running the same sweep
/// concurrently get one `true` between them.
///
/// Returns `None` when there is nothing to take over, so a caller that sweeps every
/// organization's claims does not have to distinguish "no stale claims" from "stale but young".
pub async fn take_over_stale(
    pool: &PgPool,
    source_id: Uuid,
    submission_id: &str,
    now: OffsetDateTime,
) -> Result<Option<OffsetDateTime>> {
    let key = normalize(submission_id).ok_or_else(|| {
        CrmIntakeError::invalid("a submission id must be a non-empty value")
    })?;
    let cutoff = now - CLAIM_STALE_AFTER;
    let taken: Option<OffsetDateTime> = sqlx::query_scalar(
        "update crm_lead_submissions set claimed_at = $3 \
         where source_id = $1 and submission_id = $2 \
           and lead_id is null and claimed_at < $4 \
         returning claimed_at",
    )
    .bind(source_id)
    .bind(&key)
    .bind(now)
    .bind(cutoff)
    .fetch_optional(pool)
    .await?;
    Ok(taken)
}

/// The open claims of one source, oldest first — the shape a recovery sweep walks.
///
/// Ordered by age on purpose: a sweep that took the newest first would keep the one claim that
/// has been wedged longest as the one it never reaches.
pub async fn open_claims(
    pool: &PgPool,
    source_id: Uuid,
    limit: i64,
) -> Result<Vec<(String, OffsetDateTime)>> {
    let rows: Vec<(String, OffsetDateTime)> = sqlx::query_as(
        "select submission_id, claimed_at from crm_lead_submissions \
         where source_id = $1 and lead_id is null order by claimed_at asc limit $2",
    )
    .bind(source_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The lead a completed claim names, or `None` when the claim is open or absent.
pub async fn lead_of(pool: &PgPool, source_id: Uuid, submission_id: &str) -> Result<Option<Uuid>> {
    let key = normalize(submission_id).ok_or_else(|| {
        CrmIntakeError::invalid("a submission id must be a non-empty value")
    })?;
    let lead_id: Option<Uuid> =
        sqlx::query_scalar("select lead_id from crm_lead_submissions \
             where source_id = $1 and submission_id = $2")
            .bind(source_id)
            .bind(&key)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(lead_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_or_whitespace_id_is_no_identity() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("\t\n"), None);
    }

    #[test]
    fn an_id_is_trimmed_because_a_header_carries_whitespace() {
        // `x-idempotency-key: "  abc  "` is one submission, not two, and the two spellings
        // must not produce two claims: the winner of the first would not be found by the
        // second and the submission would be captured twice.
        assert_eq!(normalize("  abc  ").as_deref(), Some("abc"));
    }

    #[test]
    fn an_oversized_id_is_capped_rather_than_refused() {
        // Capping, not refusing: the caller supplied an id, the submission is real, and a
        // 200-character id is not a reason to lose the lead. The cap is a key-width bound.
        let long = "x".repeat(400);
        let normalized = normalize(&long).expect("a usable identity");
        assert_eq!(normalized.chars().count(), MAX_SUBMISSION_ID);
    }

    #[test]
    fn the_cap_counts_characters_and_not_bytes() {
        // A multi-byte id truncated by *bytes* can end mid-character and produce a key the
        // database refuses with 22021, which is a capture failure caused by a defensive
        // measure. `chars()` is the right unit for a text column.
        let wide = "ş".repeat(200);
        let normalized = normalize(&wide).expect("a usable identity");
        assert_eq!(normalized.chars().count(), MAX_SUBMISSION_ID);
        assert!(normalized.is_char_boundary(normalized.len()), "must be whole characters");
    }

    #[test]
    fn the_stale_window_is_long_enough_to_not_steal_a_live_capture() {
        // The bias, stated as a test: a window of zero or one second would hand a live
        // capture's submission to the next delivery, and the live writer would then finish and
        // find its claim gone — the exact duplicate this module prevents. Two seconds is the
        // smallest value that is defensible against a capture that does one mapping, one
        // contact lookup, one insert and one event.
        assert!(CLAIM_STALE_AFTER >= time::Duration::seconds(2));
        assert!(CLAIM_STALE_AFTER < time::Duration::seconds(30));
    }

    #[test]
    fn a_distinct_id_keeps_its_own_whitespace_and_case() {
        // Only the *edges* are noise. Trimming inside would make two different submissions
        // claim one key, and a case fold would do the same for two ids that differ only in
        // case — which is a real pair for any id an operator's own script generated.
        assert_eq!(normalize(" a b ").as_deref(), Some("a b"));
        assert_ne!(normalize("abc"), normalize("ABC"));
    }
}
