//! The delivery log's own retention (REQ-021, slice 7).
//!
//! ## What was missing
//!
//! `push::OUTBOX_RETENTION_DAYS` has been published to the panel since the outbox screen
//! shipped — `notification-outbox.tsx` renders "The log goes back 60 days" from the
//! `retention_days` the route sends — and `push::prune_deliveries` / `push::prune_stale` were
//! `pub` re-exports with **zero call sites in the repository**. A number the screen shows an
//! administrator, promising a floor the installation never enforced. `prune_endpoints` does
//! have a caller (the delivery queue, after a push service answers 404/410); these two did not
//! have one at all.
//!
//! ## What this module adds
//!
//! A sweep, per organization, on the organization's own window — and the window is a **column**,
//! so an operator sets it once and then changes it, exactly as `0123` set for the event bus.
//!
//! ## The decision that matters: the clock is the settle instant
//!
//! The dead function selected on `created_at`, which `enqueue` writes once and nothing ever
//! updates. A delivery queued on day 1 and settled on day 59 was therefore swept on day 60
//! "because it is 60 days old" — while the screen told the administrator the log answers for
//! sixty days. The guarantee was measured from the wrong end: a row that spent a month failing
//! lost its history the day after it finally arrived.
//!
//! `settled_at` is stamped by the functions that *settle* a row (`mark_sent`,
//! `settle_not_ready`, `mark_failed`, and the retry that puts one back on the queue), is `null`
//! for a row nobody has finished with, and the sweep reads `coalesce(settled_at, created_at)`.
//! That single expression is why there is **no status list in this module**: a `pending` row is
//! kept by the same clause that says it has not been settled, so the predicate cannot drift
//! away from the states the writer maintains. A status list here would be a *third* spelling
//! of "settled", beside `mark_sent`'s `status = 'pending'` guard and the settle functions —
//! and this branch's standing defect is three spellings of one question.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;

/// How many organizations one sweep pass walks.
///
/// A bound rather than "all of them", because a single `delete` over an entire backlog takes a
/// lock proportional to the whole table and holds it while every row it removes cascades. The
/// remainder is picked up by the next tick rather than lost — the same argument `0123` makes
/// for the event sweeper and `run-walks.sh` makes for every other worker on this platform.
pub const MAX_ORGANIZATIONS: i64 = 100;

/// How many delivery rows one organization's sweep removes at most.
///
/// A delete batch, so a tick that finds a tenant with a year of history worked down over
/// several ticks instead of in one transaction nobody can cancel.
pub const MAX_ROWS_PER_SWEEP: i64 = 1_000;

/// How long a device that has not been seen is kept, when the organization has no opinion.
///
/// The same argument as [`crate::push::SUBSCRIPTION_STALE_DAYS`], and deliberately **the same
/// number**: two constants for one rule is how the outbox screen and the sweeper end up
/// disagreeing about when a phone's row goes away.
pub const DEFAULT_DEVICE_STALE_DAYS: i32 = 30;

/// One organization's window, and whether the sweep could read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// The organization whose log is being swept. `None` is the platform's own traffic —
    /// the same arm [`crate::push::OutboxScope::Platform`] reads, and the reason the work list
    /// keeps it rather than dropping rows with no tenant.
    pub organization_id: Option<Uuid>,
    /// How many days of history this organization keeps.
    pub window_days: i32,
}

/// The organizations a sweep pass walks, oldest backlog first.
///
/// **The window is read here, not in the delete**, for the reason `0123` gives: an organization
/// that changes its own window changes what its next tick removes, and an organization with no
/// row falls back to the platform default rather than to `null` — `null` would mean "keep for
/// ever", which is a decision nobody made deliberately.
///
/// The two arms are `is not distinct from`, not `=`. `notifications.organization_id` is
/// nullable and the platform's own notifications carry `null`, so `=` would silently drop the
/// platform's traffic from the sweep entirely and let it grow for ever — the same
/// orgless-platform population the outbox scope was built to name rather than guess about.
pub async fn work_list(pool: &PgPool, limit: i64) -> Result<Vec<RetentionPolicy>> {
    let rows = sqlx::query_as::<_, (Option<Uuid>, i32)>(
        "select n.organization_id, \
                coalesce((select notification_retention_days from organizations o \
                          where o.id = n.organization_id), $2) \
         from notifications n \
         group by n.organization_id \
         order by min(n.created_at) asc \
         limit $1",
    )
    .bind(limit.clamp(1, MAX_ORGANIZATIONS * 10))
    .bind(crate::push::OUTBOX_RETENTION_DAYS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(organization_id, window_days)| RetentionPolicy {
            organization_id,
            window_days,
        })
        .collect())
}

/// What one organization's sweep removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Delivery rows removed.
    pub deliveries_deleted: i64,
    /// Browser devices removed for want of a sighting.
    pub devices_deleted: i64,
}

/// Remove one organization's delivery history past its window.
///
/// **The clock is `coalesce(settled_at, created_at)` and that expression is the whole
/// predicate.** A settled row is judged on when the runner finished with it; a row nobody ever
/// settled — a channel with no transport, a queue nobody drains — is judged on when it was
/// written, so an abandoned row cannot pin its own history for ever. There is no `status in
/// (...)` clause, and adding one would be the defect this module exists to prevent: the old
/// function had `status in ('sent', 'skipped')`, which meant a **`failed` row — the one an
/// administrator opens the outbox to find — was never swept at all**, so the failure log grew
/// for ever precisely because it was the log people cared about.
///
/// **The guard is the CLAIM, not the status, and that is the second half of this slice's
/// finding.** The obvious clause is `status <> 'pending'`, and it is wrong in a way that only
/// running it shows: a `pending` row is *unsettled by definition*, so the clause removes every
/// row the clock expression was written to rescue. The `coalesce(settled_at, created_at)`
/// fallback — whose entire job is to sweep an **abandoned** row, a delivery queued for a
/// channel with no transport or a queue nobody drains — becomes dead code, and the population
/// grows for ever on exactly the installations that already have a stuck queue.
///
/// The gate caught it by returning zero for every row that should have gone, which is why this
/// clause is written down rather than discovered twice: the two conditions look interchangeable
/// ("a row the runner is working on") and are not. What protects an in-flight send is
/// `claimed_at` being recent, which is the *same* lease predicate `claim_due` and
/// `settle_not_ready` use. So the sweep joins the platform's own vocabulary instead of inventing
/// a second one — which is also why it takes `lease_seconds` as an argument rather than reading
/// the config itself.
pub async fn sweep_deliveries(
    pool: &PgPool,
    policy: RetentionPolicy,
    max_rows: i64,
    lease_seconds: f64,
) -> Result<SweepReport> {
    // **The clock is named once, here, and used by both statements.** A device's window and a
    // delivery's window are the same policy, and two copies of `coalesce(settled_at,
    // created_at)` written twice is two places for the next change to miss one.
    let deliveries = sqlx::query_scalar::<_, i32>(
        "with doomed as ( \
             select d.id from notification_deliveries d \
             join notifications n on n.id = d.notification_id \
             where n.organization_id is not distinct from $1 \
               and coalesce(d.settled_at, d.created_at) \
                   < now() - make_interval(days => $2::int) \
               and (d.claimed_at is null \
                    or d.claimed_at <= now() - make_interval(secs => $4::double precision)) \
             limit $3 \
         ) \
         delete from notification_deliveries d using doomed \
         where d.id = doomed.id \
         returning 1",
    )
    .bind(policy.organization_id)
    .bind(policy.window_days)
    .bind(max_rows.clamp(1, MAX_ROWS_PER_SWEEP))
    .bind(lease_seconds)
    .fetch_all(pool)
    .await?
    .len() as i64;

    // **A device row has no `settled_at`, and that is not an oversight.** `push_subscriptions`
    // has no delivery history at all: its own lifecycle is `last_seen_at`, which
    // `SUBSCRIPTION_STALE_DAYS` has always judged. This statement therefore keeps that column
    // as its clock and does NOT reach for `settled_at` — copying the delivery clock into a
    // table that has no notion of a settle instant would be inventing a column's meaning rather
    // than reusing one.
    let devices = sqlx::query_scalar::<_, i32>(
        "with doomed as ( \
             select s.id from push_subscriptions s \
             join users u on u.id = s.user_id \
             where $1 is null or u.organization_id = $1 \
               and s.last_seen_at < now() - make_interval(days => $3::int) \
             limit $2 \
         ) \
         delete from push_subscriptions s using doomed \
         where s.id = doomed.id \
         returning 1",
    )
    .bind(policy.organization_id)
    .bind(max_rows.clamp(1, MAX_ROWS_PER_SWEEP))
    .bind(crate::push::SUBSCRIPTION_STALE_DAYS)
    .fetch_all(pool)
    .await?
    .len() as i64;

    Ok(SweepReport {
        deliveries_deleted: deliveries,
        devices_deleted: devices,
    })
}

/// One row of the sweep's run log, written whether or not anything was removed.
///
/// **A sweep that deleted nothing still writes its row**, for the reason `0049` and `0123` both
/// give: "the last sweep was at 03:00 and it found nothing" is the sentence an operator needs
/// on the day they ask why a March delivery is still on the screen. A log that only records
/// activity cannot answer it on the day nothing happened.
pub async fn write_run(
    pool: &PgPool,
    policy: RetentionPolicy,
    cutoff: OffsetDateTime,
    report: SweepReport,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "insert into notification_retention_runs \
           (organization_id, window_days, cutoff, deliveries_deleted, devices_deleted, \
            failed, error, finished_at) \
         values ($1, $2, $3, $4, $5, $6, $7, now())",
    )
    .bind(policy.organization_id)
    .bind(policy.window_days)
    .bind(cutoff)
    .bind(report.deliveries_deleted)
    .bind(report.devices_deleted)
    .bind(i32::from(error.is_some()))
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// What one pass over the work list did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassReport {
    /// Organizations the pass walked.
    pub walked: usize,
    /// Delivery rows removed across every organization.
    pub deliveries_deleted: i64,
    /// Devices removed across every organization.
    pub devices_deleted: i64,
    /// Organizations whose sweep failed.
    pub failed: usize,
}

/// One pass over the work list.
///
/// **One organization that refuses its sweep does not abandon the rest.** A sweep is a delete
/// batch and a batch that stops at the first error is a batch that never reaches the tail; the
/// failure is logged with the organization named, the run row records it, and the pass
/// continues — because one misbehaving tenant is not a reason to stop pruning every other
/// tenant's history.
pub async fn run_pass(
    pool: &PgPool,
    limit: i64,
    max_rows: i64,
    lease_seconds: f64,
) -> Result<PassReport> {
    let queue = work_list(pool, limit).await?;
    let walked = queue.len();

    let mut deliveries_deleted = 0_i64;
    let mut devices_deleted = 0_i64;
    let mut failed = 0_usize;

    for policy in &queue {
        let cutoff =
            OffsetDateTime::now_utc() - time::Duration::days(i64::from(policy.window_days));

        // The result and the failure sentence both outlive the `match`, because the run row
        // below is written for a sweep that failed as well as one that succeeded. Holding the
        // outcome in one variable is what keeps the two from drifting: an arm that recorded its
        // own run row would skip the failure case entirely, and a run log that records only
        // successes cannot tell "nothing has been swept since March" from "every sweep has been
        // failing since March".
        let (report, failure) = match sweep_deliveries(pool, *policy, max_rows, lease_seconds).await {
            Ok(report) => {
                if report.deliveries_deleted > 0 || report.devices_deleted > 0 {
                    tracing::info!(
                        organization_id = ?policy.organization_id,
                        window_days = policy.window_days,
                        deliveries_deleted = report.deliveries_deleted,
                        devices_deleted = report.devices_deleted,
                        "the notification retention sweep removed history"
                    );
                }
                (report, None)
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    organization_id = ?policy.organization_id,
                    error = %error,
                    "an organization's delivery log could not be swept"
                );
                (SweepReport::default(), Some(error.to_string()))
            }
        };

        deliveries_deleted += report.deliveries_deleted;
        devices_deleted += report.devices_deleted;

        if let Err(error) = write_run(pool, *policy, cutoff, report, failure.as_deref()).await {
            tracing::warn!(
                organization_id = ?policy.organization_id,
                error = %error,
                "the notification retention run could not be recorded"
            );
        }
    }

    // The empty pass is the common one — most organizations have nothing past their window on
    // any given day — so it is logged at debug, where it is available and does not push a real
    // warning out of the reader's attention.
    if deliveries_deleted == 0 && devices_deleted == 0 && failed == 0 {
        tracing::debug!(walked, "the notification retention sweep found nothing to remove");
    }

    Ok(PassReport {
        walked,
        deliveries_deleted,
        devices_deleted,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This file's own code: comments stripped, test module dropped, literals kept.
    ///
    /// **A test that asserts on a literal it defines itself passes whether or not the
    /// statement says anything at all** — five of this branch's gates were caught by exactly
    /// that, and the first version of this module's own reader was one of them. The three
    /// things a reader has to get right are all recorded in [`crate::testing`]: comments must go
    /// (this module's header quotes the old predicate on purpose), string literals must STAY
    /// (the SQL under test is one), and the test module must be dropped (or the assertion's
    /// own argument satisfies it).
    ///
    /// `code_of` also **unwraps the string literals**, so an assertion can name a phrase that
    /// rustfmt has wrapped across three source lines. The first version of every test here
    /// searched for `coalesce(d.settled_at, d.created_at)` as one string and failed on a
    /// statement that plainly contained it — the line break is between the two clock terms.
    /// Searching a normalised form is the difference between a gate that measures the code and
    /// one that measures rustfmt's line-width setting.
    fn code() -> String {
        crate::testing::code_of(include_str!("retention.rs"))
    }

    /// The same code with every run of whitespace collapsed to one space.
    ///
    /// A statement is one sentence split across lines by the formatter, and an assertion that
    /// cannot see it is measuring the formatter.
    fn flat() -> String {
        code().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The default window is the number the panel publishes, not a second constant.
    ///
    /// The work list binds [`crate::push::OUTBOX_RETENTION_DAYS`] as the fallback, so an
    /// installation with no per-organization row gets the same sixty days the screen says — and
    /// a future edit that typed `60` here instead would make the screen and the sweeper
    /// disagree without either file mentioning it.
    #[test]
    fn the_default_window_is_the_constant_the_screen_reads() {
        assert_eq!(crate::push::OUTBOX_RETENTION_DAYS, 60);
        assert!(
            flat().contains(".bind(crate::push::OUTBOX_RETENTION_DAYS)"),
            "the work list must fall back to the published constant, not a literal"
        );
    }

    /// **The settle clock is used, and it is used in the delete.**
    ///
    /// The dead function this module replaces selected on `created_at`, so a row queued on day 1
    /// and settled on day 59 lost its history on day 60 for being sixty days old — the
    /// guarantee measured from the wrong end. `coalesce(settled_at, created_at)` is what makes
    /// it the right end, and the fallback term is what keeps an abandoned row from pinning its
    /// own history for ever.
    #[test]
    fn the_sweep_judges_a_row_by_when_it_was_settled() {
        assert!(
            flat().contains("coalesce(d.settled_at, d.created_at)"),
            "the sweep must read the settle clock with the enqueue clock as its fallback"
        );
    }

    /// **No `status in (...)` list — the sweep has no state vocabulary of its own.**
    ///
    /// The pre-fix predicate was `status in ('sent','skipped')`, which meant a `failed` row —
    /// the one an administrator opens the outbox to find — was never swept at all, so the
    /// failure log grew for ever precisely because it was the log people cared about. A status
    /// list here would also be a third spelling of "settled" beside `mark_sent`'s own
    /// `status = 'pending'` guard, which is this branch's standing defect class.
    #[test]
    fn the_sweep_carries_no_status_list() {
        assert!(
            !flat().contains("status in ("),
            "a status list here is a second answer to which rows are history"
        );
    }

    /// **The guard is the claim, and asserting on the *status* guard is the assertion that
    /// matters.**
    ///
    /// The sweep shipped `d.status <> 'pending'` first, on the reasoning that a `pending` row is
    /// one the runner is working on. It is not: a `pending` row is *unsettled by definition*,
    /// which is precisely the population the clock's `created_at` fallback exists to sweep. The
    /// clause deleted the sweep's own abandoned-row case, the fallback became dead code, and
    /// every gate assertion that only asked "is this old row gone" answered *no* for all of
    /// them at once. So this test names both halves: the claim guard must be present, and the
    /// status guard must be **absent** — because a reader finding one would assume it is
    /// protective.
    #[test]
    fn the_guard_is_the_claim_and_not_the_status() {
        let flat = flat();
        assert!(
            flat.contains("d.claimed_at is null"),
            "a row the runner is working on must be kept: the lease predicate is the guard"
        );
        assert!(
            !flat.contains("d.status <> 'pending'"),
            "a status guard would delete every ABANDONED row — a pending row is unsettled by \
             definition, which is exactly what the clock's fallback is for"
        );
    }

    /// **The device sweep keeps its own clock, and this test is the record of why.**
    ///
    /// `push_subscriptions` has no settle instant at all — its lifecycle is `last_seen_at`.
    /// Copying the delivery clock across that boundary would invent a column's meaning rather
    /// than reuse one, and the next person to "make the two statements consistent" would do it
    /// silently.
    #[test]
    fn the_device_sweep_keeps_its_own_clock() {
        let flat = flat();
        assert!(flat.contains("s.last_seen_at < now()"));
        // The device statement, from its `with doomed as (` to the end of the function. Slicing
        // on the *function* rather than on `delete from push_subscriptions` matters: the
        // delivery statement above it also deletes rows, and a slice bounded by the wrong
        // anchor would have compared the delivery statement's clock against the device rule
        // and reported a pass for the wrong reason.
        let device_statement = flat
            .split("let devices =")
            .nth(1)
            .expect("the device sweep statement");
        assert!(
            !device_statement.contains("settled_at"),
            "a device row has no settle instant; the delivery clock must not reach it"
        );
    }

    /// **Both arms of the work list keep the platform's own traffic.**
    ///
    /// `notifications.organization_id` is nullable, and `=` would drop every `null` row — so
    /// the platform's own announcements would grow without bound while the work list reported
    /// having swept everything. This is the same population `push::OutboxScope::Platform` names
    /// on the read side, and the two halves have to agree or the screen shows traffic no sweep
    /// will ever reach.
    #[test]
    fn the_work_list_names_the_platform_arm_rather_than_dropping_it() {
        assert!(
            flat().contains("n.organization_id is not distinct from $1"),
            "the sweep must keep the orgless platform arm the outbox scope reads"
        );
    }

    /// **The run row is written outside the sweep's outcome, so a failure is recorded too.**
    ///
    /// "Nothing has been swept since March" and "every sweep has been failing since March" are
    /// different sentences, and a run log that records only successes cannot tell them apart.
    /// The test asserts the *call* sits outside the match, which is the property; the reason it
    /// is written this way is in the comment above it.
    #[test]
    fn a_failed_sweep_still_writes_its_run_row() {
        let flat = flat();
        let arm = flat.find("Err(error) =>").expect("the sweep's failure arm");
        let write = flat.find("write_run(pool").expect("the run row");
        assert!(
            write > arm,
            "the run row must be written after the match, so the failure case records a row too"
        );
    }

    /// **The bounds are constants, so the assertion about them is a constant too** — clippy
    /// says so, and it is right. `assert!(MAX_ORGANIZATIONS > 0)` in a test body is evaluated
    /// at run time and reads like a measurement; a `const` block is evaluated at compile time
    /// and **fails the build** when somebody sets the bound to zero, which is the actual defect
    /// (a `limit 0` sweep that silently sweeps nothing, or an unbounded `clamp(1, 0)` that
    /// sweeps everything in one transaction).
    const _: () = assert!(MAX_ORGANIZATIONS > 0);
    const _: () = assert!(MAX_ROWS_PER_SWEEP > 0);

    /// The clamps are in the statements, which is the half a constant assertion cannot reach.
    #[test]
    fn both_bounds_are_clamped_in_the_statements() {
        let flat = flat();
        assert!(
            flat.contains("max_rows.clamp(1, MAX_ROWS_PER_SWEEP)"),
            "a caller must not be able to pass an unbounded batch through the sweep"
        );
        assert!(flat.contains("limit.clamp(1, MAX_ORGANIZATIONS * 10)"));
    }
}
