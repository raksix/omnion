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

/// The shortest window a delivery log may be kept for.
///
/// **The same number `0236`'s check constraint enforces**, spelled here as a constant so the
/// API's refusal and the column's backstop cannot drift apart. It is deliberately not `0`: a
/// zero window deletes the log the moment a row is written, which leaves an administrator
/// staring at an empty delivery screen and a sweep that claims it ran.
pub const MIN_RETENTION_DAYS: i32 = 1;

/// The longest window — ten years, past which the log is a database and not a log.
///
/// The same argument `crates/events::MAX_RETENTION_DAYS` makes for the event bus.
pub const MAX_RETENTION_DAYS: i32 = 3_650;

/// Validate a delivery-log window.
///
/// **The refusal names the field and the range**, for the reason `crates/events::validation::
/// validate_retention_window` gives: a bare "invalid" leaves the caller guessing which bound it
/// crossed, and the two bounds mean opposite things to an operator — the floor is "you cannot
/// keep nothing", the ceiling is "that is a decade, and nothing will read it".
///
/// The bounds are read from the constants rather than written here, so a column check
/// constraint, an API refusal and this function cannot disagree — which is the shape of the
/// defect this slice exists to close.
pub fn validate_retention_window(days: i32) -> Result<i32> {
    if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&days) {
        return Err(crate::error::NotificationError::invalid(format!(
            "notification_retention_days must be between {MIN_RETENTION_DAYS} and \
             {MAX_RETENTION_DAYS}, and {days} is not"
        )));
    }

    Ok(days)
}

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

/// Read one organization's window, or the platform default when it has no opinion.
///
/// **`coalesce` on a subquery rather than a plain read**, for the reason `crates/events::store::
/// retention_window` gives: the column is `not null default 60`, so a `None` here can only mean
/// the organization row does not exist — and answering with the published default is what lets
/// the outbox render its sentence before the first organization exists.
///
/// This is the read the sweep does *not* make. The sweep reads its window inside `work_list`,
/// joined to the organizations that actually have notifications; this one is the screen's read,
/// and it is what finally makes `organizations.notification_retention_days` a value a person can
/// look at rather than a column only a worker consults.
pub async fn retention_window(pool: &PgPool, organization_id: Option<Uuid>) -> Result<i32> {
    let days: i32 = sqlx::query_scalar(
        "select coalesce((select notification_retention_days from organizations where id = $1), $2)",
    )
    .bind(organization_id)
    .bind(crate::push::OUTBOX_RETENTION_DAYS)
    .fetch_one(pool)
    .await?;

    Ok(days)
}

/// Set one organization's window and read back what the column now holds.
///
/// **The write is validated by the store, not only by the handler**, for the reason the event
/// bus gives for its own setter: the store is also reached by an operator's SQL and by a future
/// import, and a rule true in one handler is a rule the next writer re-implements. The column's
/// check constraint is the backstop; the read-back is what the caller returns to the screen, so
/// a caller can never report a number the database did not accept.
///
/// **An organization that does not exist is an error, not a silent zero-row update.** The
/// events setter refuses the same way, and the reason matters here: a `fetch_optional` that
/// falls back to the default would answer `200` and change nothing, which is exactly the
/// "a platform account has no window to set" trap the handler refuses by name.
pub async fn set_retention_window(pool: &PgPool, organization_id: Uuid, days: i32) -> Result<i32> {
    let days = validate_retention_window(days)?;
    let stored: i32 = sqlx::query_scalar(
        "update organizations set notification_retention_days = $2, updated_at = now() \
         where id = $1 returning notification_retention_days",
    )
    .bind(organization_id)
    .bind(days)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        crate::error::NotificationError::invalid(format!(
            "no organization {organization_id} to keep a delivery log for"
        ))
    })?;

    Ok(stored)
}

/// What one organization's window is, and what is due for removal because of it.
///
/// **One statement for both numbers**, for the reason `crates/events::store::retention_counts`
/// gives: the screen shows the kept count and the due count together, and two round trips could
/// be answered by two different instants — a panel drawing "3,412 rows, 12 due" where the 12
/// came from a moment after the 3,412.
///
/// **The `due` predicate is the sweep's own**, restricted to rows nobody is delivering right
/// now. A screen that said "12 due" while the sweeper removes 0 would be lying, and the two
/// only stay equal if one of them is written once.
///
/// **The window comes out of the same subquery the work list reads it through** (`o.window`),
/// rather than being bound as a parameter. That is the whole point of this statement: a count
/// that took the window from a *second* place would answer for a different window than the sweep
/// deletes on, and the difference only appears on a tenant that has set its own — which is the
/// only tenant for whom the number matters.
pub async fn retention_counts(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    lease_seconds: f64,
) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "select count(*) as total, \
                count(*) filter (where coalesce(d.settled_at, d.created_at) \
                                   < now() - make_interval(days => o.window) \
                                  and (d.claimed_at is null \
                                       or d.claimed_at <= now() - \
                                          make_interval(secs => $2::double precision))) as due \
         from notification_deliveries d \
         join notifications n on n.id = d.notification_id \
         cross join (select coalesce((select notification_retention_days from organizations o \
                                     where o.id = $1), $3) as window) o \
         where n.organization_id is not distinct from $1",
    )
    .bind(organization_id)
    .bind(lease_seconds)
    .bind(i32::from(crate::push::OUTBOX_RETENTION_DAYS))
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// What one organization's window is, and the last sweep that ran against it.
///
/// The read side of the pair the sweep writes. Without it the run log has exactly one reader —
/// the worker that wrote it — which is the "capability with no caller" shape this branch has
/// been paying for in six figures.
pub async fn retention_status(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    lease_seconds: f64,
) -> Result<RetentionStatus> {
    let window_days = retention_window(pool, organization_id).await?;
    let (rows, due) = retention_counts(pool, organization_id, lease_seconds).await?;
    let last_run = sqlx::query_as::<_, RetentionRun>(
        "select id, organization_id, started_at, finished_at, window_days, cutoff, \
                deliveries_deleted, devices_deleted, failed, error \
         from notification_retention_runs \
         where organization_id is not distinct from $1 and finished_at is not null \
         order by started_at desc limit 1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(RetentionStatus {
        organization_id,
        window_days,
        rows,
        due,
        last_run,
    })
}

/// One finished sweep, as the screen renders it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RetentionRun {
    pub id: Uuid,
    pub organization_id: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub finished_at: OffsetDateTime,
    pub window_days: i32,
    pub cutoff: OffsetDateTime,
    pub deliveries_deleted: i32,
    pub devices_deleted: i32,
    pub failed: i32,
    pub error: Option<String>,
}

/// The delivery log's retention, as one organization sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionStatus {
    pub organization_id: Option<Uuid>,
    /// How many days of delivery history this organization keeps.
    pub window_days: i32,
    /// How many delivery rows the log holds now.
    pub rows: i64,
    /// How many of them the next sweep would remove — the sweep's own predicate.
    pub due: i64,
    /// The last finished sweep, or none.
    pub last_run: Option<RetentionRun>,
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
        let (report, failure) = match sweep_deliveries(pool, *policy, max_rows, lease_seconds).await
        {
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
        tracing::debug!(
            walked,
            "the notification retention sweep found nothing to remove"
        );
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

    /// A window outside the range is refused by name, and the bounds are the column's.
    ///
    /// Both ends, not just the obvious one: a caller asking for `0` is asking to delete the log
    /// the moment a row is written, and a caller asking for `3651` is asking for a decade of a
    /// table nobody reads. The assertion is on the *message*, because the message is what the
    /// screen shows an operator who typed the wrong thing.
    #[test]
    fn a_window_outside_the_range_is_refused_by_name() {
        for days in [0, -1, MAX_RETENTION_DAYS + 1] {
            let refused = validate_retention_window(days).expect_err("the window must be refused");
            assert!(
                refused.to_string().contains("notification_retention_days"),
                "the refusal names the field: {refused}"
            );
            assert!(
                refused
                    .to_string()
                    .contains(&MIN_RETENTION_DAYS.to_string())
                    && refused
                        .to_string()
                        .contains(&MAX_RETENTION_DAYS.to_string()),
                "the refusal names both bounds, so a caller can tell which one it crossed: \
                     {refused}"
            );
        }

        // Both ends are legal, and the *returned* value is the input — the store returns what
        // was asked for so the handler can hand the same number back to the screen.
        assert_eq!(
            validate_retention_window(MIN_RETENTION_DAYS).expect("the floor is valid"),
            MIN_RETENTION_DAYS
        );
        assert_eq!(
            validate_retention_window(MAX_RETENTION_DAYS).expect("the ceiling is valid"),
            MAX_RETENTION_DAYS
        );
        assert_eq!(
            validate_retention_window(90).expect("a quarter is valid"),
            90
        );
    }

    /// **The floor and the ceiling are the ones `0236`'s check constraint enforces.**
    ///
    /// A constant asserted against itself proves nothing; what matters is that the migration's
    /// SQL and this module agree, because the constraint is the backstop for the operator's own
    /// SQL and for a future import. So the number is read *out of the migration file* rather
    /// than typed here — the same trick the module's `code()` reader plays on this file.
    #[test]
    fn the_bounds_are_the_migrations_own_check_constraint() {
        let migration = crate::testing::code_of(include_str!(
            "../../../database/migrations/0236_notification_retention.sql"
        ));
        let flat = migration.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(&format!(
                "check (notification_retention_days between {MIN_RETENTION_DAYS} and \
                     {MAX_RETENTION_DAYS})"
            )),
            "the migration's constraint and this module's bounds must be one number pair, or a \
                 write refused here and a write allowed there are two different rules"
        );
    }

    /// **The published constant sits inside the range, and is not the floor.**
    ///
    /// The default window is what an installation with no row gets. If it were ever set to the
    /// floor (`1`), every fresh installation would silently keep one day of delivery history —
    /// and the screen would say "The log goes back 1 day" beside a log that has already lost
    /// yesterday's failures. So the relationship is asserted, not the literal.
    #[test]
    fn the_default_window_is_inside_the_range_and_is_not_the_floor() {
        let default = crate::push::OUTBOX_RETENTION_DAYS;
        assert!(
            (MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&default),
            "the fallback the work list binds must itself be a legal window"
        );
        assert!(
            default > MIN_RETENTION_DAYS,
            "a default of {MIN_RETENTION_DAYS} day would keep no history at all, and the screen \
                 would publish that number"
        );
    }

    /// **The count reads its window from the same subquery the work list does.**
    ///
    /// `retention_counts` is the screen's "12 due" and `sweep_deliveries` is what actually
    /// removes rows. Two statements that spell the window differently agree on every tenant
    /// using the default and disagree on exactly one kind of tenant — the one that set its own
    /// window — which is the only kind for which the sentence is worth anything. So this
    /// asserts the *shared spelling*: the `due` filter references `o.window`, the cross join that
    /// produces `o.window` is the one that reads the column, and neither hardcodes a literal.
    #[test]
    fn the_due_count_reads_the_window_from_the_column_rather_than_a_literal() {
        let flat = flat();
        let counts = flat
            .split("pub async fn retention_counts")
            .nth(1)
            .expect("the counts statement")
            .split("pub async fn retention_status")
            .next()
            .expect("the counts statement's end");
        assert!(
            counts.contains("< now() - make_interval(days => o.window)"),
            "the due count must be judged on the organization's own window"
        );
        assert!(
            counts.contains("notification_retention_days"),
            "that window is read from the column, not from the published constant"
        );
        assert!(
            !counts.contains("days => $2::int"),
            "a window bound as a parameter is a SECOND place the window comes from, and the two \
                 are only equal until a tenant sets its own"
        );
    }

    /// **The run log finally has a reader, and the reader filters finished runs.**
    ///
    /// `notification_retention_runs` shipped with exactly one reader — the worker that wrote it —
    /// which is this branch's standing defect in its purest form: a table an operator can find
    /// nothing about on the day they ask why a March delivery is still on the screen. The
    /// screen's read must also exclude an unfinished run, because an unfinished one is a run in
    /// flight or a process that died mid-sweep, and rendering "last sweep at 03:00, removed
    /// nothing" from a row that never finished is the log lying in the one direction it exists
    /// to prevent.
    #[test]
    fn the_run_log_is_readable_and_only_finished_runs_are() {
        let flat = flat();
        let status = flat
            .split("pub async fn retention_status")
            .nth(1)
            .expect("the status read");
        assert!(
            status.contains("from notification_retention_runs"),
            "the last sweep must be read out of the run log the sweep writes"
        );
        assert!(
            status.contains("finished_at is not null"),
            "an unfinished run is a run in flight, not the last sweep"
        );
    }

    /// The bounds are constants, so the assertion about them is a constant too — clippy
    /// says so, and it is right. `assert!(MAX_ORGANIZATIONS > 0)` in a test body is evaluated
    /// at run time and reads like a measurement; a `const` block is evaluated at compile time
    /// and **fails the build** when somebody sets the bound to zero, which is the actual defect
    /// (a `limit 0` sweep that silently sweeps nothing, or an unbounded `clamp(1, 0)` that
    /// sweeps everything in one transaction).
    const _: () = assert!(MAX_ORGANIZATIONS > 0);
    const _: () = assert!(MAX_ROWS_PER_SWEEP > 0);
    /// The same for the window's bounds, which are compile-time facts about the column too.
    const _: () = assert!(MIN_RETENTION_DAYS >= 1);
    const _: () = assert!(MAX_RETENTION_DAYS >= MIN_RETENTION_DAYS);

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
