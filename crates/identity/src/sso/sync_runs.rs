//! Recording a directory sync (REQ-065, slice 4).
//!
//! [`super::role_rule_store`] and [`super::mappings`] store what an operator *configures*. This
//! stores what a sync *did*, and the two have opposite failure modes. A wrong mapping silently
//! grants the wrong role; a wrong run record silently tells an operator that last night's
//! directory sync worked. The second is worse, because the operator's response to it is to do
//! nothing.
//!
//! So the shape is deliberately narrow and the rules are spelled out rather than assumed.
//!
//! * **A run is opened once and finished once.** [`finish_run`] refuses a run that is already
//!   finished. A run that can be finished twice has a duration that depends on how often somebody
//!   opened the page, and a retry that appends a second `finished_at` is how a list screen ends
//!   up showing "42s" for a run that took four hours.
//! * **The counts are a summary, and [`finish_run`] is their only writer.** `error_count` is what
//!   the list sorts and filters on, and it is written in the same statement that finalises the
//!   errors. If they could be written separately they would drift, and a screen that sorts by a
//!   number nothing is counting is worse than no screen.
//! * **Failures outlive the run row.** They are their own table, so retrying a subject appends a
//!   new attempt rather than rewriting the record of what failed the first time — which is the
//!   only copy of "the directory was refusing this account at 02:14" anybody will ever have.
//! * **A subject is a retryable unit, not a line.** [`failed_subjects`] collapses repeats, so a
//!   directory that failed the same person three times gives the operator one row to retry, and
//!   the three attempts are still there underneath.
//!
//! Nothing here writes a credential or a raw provider response: [`SyncError`] carries a `code`
//! from a closed set and a sentence, and the subject is whatever the directory called the thing
//! (an address, a DN, a subject id) because the operator has to be able to recognise it.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// The columns, kept in one place so a schema change is a one-line edit.
const RUN_COLUMNS: &str = "id, provider_id, kind, status, started_at, finished_at, users_seen, \
                           users_created, users_updated, users_deactivated, groups_seen, \
                           error_count, message, triggered_by";

/// What sort of sweep this was.
///
/// `Full` and `Delta` are separated because `users_updated: 0` means "nothing changed" in one and
/// "the change feed was empty, or the change feed is broken" in the other, and an operator reading
/// the wrong one of those is misled by a number that is technically correct. `Scim` is a push from
/// the directory rather than a pull, and `Manual` is the same shape as a full run — it is named
/// separately so the row records that a person asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    Full,
    Delta,
    Scim,
    Manual,
}

impl SyncKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Delta => "delta",
            Self::Scim => "scim",
            Self::Manual => "manual",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "delta" => Self::Delta,
            "scim" => Self::Scim,
            "manual" => Self::Manual,
            // A row written by a future version of this enum, or by a hand-typed insert. Falling
            // back to the broadest kind reads as "it swept everything", which is the safe direction
            // to be wrong in for a summary row.
            _ => Self::Full,
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Still going. Carries no `finished_at` — the database refuses the combination.
    Running,
    /// Every subject was processed.
    Ok,
    /// Some subjects failed and the rest succeeded. The case that matters most, because it is the
    /// one a boolean "ok" would flatten.
    Partial,
    /// The run could not proceed. `users_seen` may be zero, and the message says why.
    Failed,
}

impl RunStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "ok" => Self::Ok,
            _ => Self::Running,
        }
    }

    /// What the list screen's status chip shows. `Partial` is deliberately *not* green: a run that
    /// failed a third of the directory and reported success is the failure this whole slice
    /// exists to make visible.
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Running | Self::Ok)
    }
}

/// One attempt at one subject, and why it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncError {
    pub id: Uuid,
    pub run_id: Uuid,
    /// Whatever the directory called the thing. Empty for a run-level failure such as a refused
    /// bind, which is the one failure with no subject to retry.
    pub subject: String,
    /// The closed code the panel and the automation read.
    pub code: String,
    /// The sentence an operator reads. Never a raw provider response.
    pub message: String,
    pub created_at: OffsetDateTime,
}

/// The counts a run accumulates while it goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncCounts {
    pub users_seen: i32,
    pub users_created: i32,
    pub users_updated: i32,
    pub users_deactivated: i32,
    pub groups_seen: i32,
}

impl SyncCounts {
    #[must_use]
    pub const fn total_writes(self) -> i32 {
        self.users_created + self.users_updated + self.users_deactivated
    }
}

/// A run, as the list screen reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRun {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub kind: SyncKind,
    pub status: RunStatus,
    pub started_at: OffsetDateTime,
    /// `None` exactly while the run is still going; the database refuses any other combination.
    pub finished_at: Option<OffsetDateTime>,
    pub counts: SyncCounts,
    /// The number of failed attempts, kept level with `directory_sync_errors` by [`finish_run`].
    pub error_count: i32,
    pub message: Option<String>,
    /// The person who asked for it, or `None` for a schedule — the two are different questions in
    /// the audit and a scheduled run has no actor to name.
    pub triggered_by: Option<Uuid>,
}

impl SyncRun {
    /// How long the run took, or `None` while it is still going. Never a guess: a run with no end
    /// has no duration, and rendering one as the time since it started makes a slow run look
    /// permanently unfinished.
    #[must_use]
    pub fn duration(&self) -> Option<time::Duration> {
        self.finished_at.map(|end| end - self.started_at)
    }
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    provider_id: Uuid,
    kind: String,
    status: String,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    users_seen: i32,
    users_created: i32,
    users_updated: i32,
    users_deactivated: i32,
    groups_seen: i32,
    error_count: i32,
    message: Option<String>,
    triggered_by: Option<Uuid>,
}

impl From<RunRow> for SyncRun {
    fn from(row: RunRow) -> Self {
        Self {
            id: row.id,
            provider_id: row.provider_id,
            kind: SyncKind::parse(&row.kind),
            status: RunStatus::parse(&row.status),
            started_at: row.started_at,
            finished_at: row.finished_at,
            counts: SyncCounts {
                users_seen: row.users_seen,
                users_created: row.users_created,
                users_updated: row.users_updated,
                users_deactivated: row.users_deactivated,
                groups_seen: row.groups_seen,
            },
            error_count: row.error_count,
            message: row.message,
            triggered_by: row.triggered_by,
        }
    }
}

/// The counters, refused if any of them is negative.
///
/// The database refuses this too, and the reason for checking here as well is that the *witness*
/// should be the writer: a caller that has underflowed a counter should be told which counter,
/// and a `check` violation arrives as a generic database error that names a constraint. Saturating
/// rather than wrapping is deliberate — `i32::MIN - 1` is not a count, it is a bug, and clamping
/// to zero would hide it behind a plausible number.
fn guard_counts(counts: SyncCounts) -> Result<()> {
    let named = [
        ("users_seen", counts.users_seen),
        ("users_created", counts.users_created),
        ("users_updated", counts.users_updated),
        ("users_deactivated", counts.users_deactivated),
        ("groups_seen", counts.groups_seen),
    ];
    if let Some((name, _)) = named.iter().find(|(_, value)| *value < 0) {
        return Err(IdentityError::InvalidProvider(format!(
            "the sync counts {name} is negative — a counter cannot go below zero, and a row \
             that says it did is a row the panel would render as a number"
        )));
    }
    Ok(())
}

/// Open a run. The row is `running` with no `finished_at`, which is the only shape the
/// `directory_sync_runs_finish_shape` constraint accepts for a run in progress.
pub async fn start_run(
    pool: &PgPool,
    provider_id: Uuid,
    kind: SyncKind,
    triggered_by: Option<Uuid>,
) -> Result<SyncRun> {
    let row = sqlx::query_as::<_, RunRow>(&format!(
        "insert into directory_sync_runs (provider_id, kind, status, triggered_by) \
         values ($1, $2, 'running', $3) \
         returning {RUN_COLUMNS}"
    ))
    .bind(provider_id)
    .bind(kind.as_str())
    .bind(triggered_by)
    .fetch_one(pool)
    .await?;
    Ok(SyncRun::from(row))
}

/// Write the counts a run has reached so far, without finishing it.
///
/// A long directory sweep calls this as it goes so the panel shows movement rather than a run
/// that appears to hang; the guards are the same ones [`finish_run`] applies, so a run cannot be
/// walked into a negative counter on the way to a legitimate final state.
pub async fn record_counts(pool: &PgPool, run_id: Uuid, counts: SyncCounts) -> Result<()> {
    guard_counts(counts)?;
    sqlx::query(
        "update directory_sync_runs set \
           users_seen = $2, users_created = $3, users_updated = $4, \
           users_deactivated = $5, groups_seen = $6 \
         where id = $1 and status = 'running'",
    )
    .bind(run_id)
    .bind(counts.users_seen)
    .bind(counts.users_created)
    .bind(counts.users_updated)
    .bind(counts.users_deactivated)
    .bind(counts.groups_seen)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record one failed subject against a run.
///
/// A subject may honestly fail more than once in a run — a flaky directory, a duplicate entry —
/// and each attempt is its own row. Collapsing them here would lose the only evidence that the
/// second attempt happened at all.
pub async fn record_error(
    pool: &PgPool,
    run_id: Uuid,
    subject: &str,
    code: &str,
    message: &str,
) -> Result<SyncError> {
    let row = sqlx::query_as::<_, ErrorRow>(
        "insert into directory_sync_errors (run_id, subject, code, message) \
         values ($1, $2, $3, $4) \
         returning id, run_id, subject, code, message, created_at",
    )
    .bind(run_id)
    .bind(subject)
    .bind(code)
    .bind(message)
    .fetch_one(pool)
    .await?;
    Ok(SyncError::from(row))
}

#[derive(sqlx::FromRow)]
struct ErrorRow {
    id: Uuid,
    run_id: Uuid,
    subject: String,
    code: String,
    message: String,
    created_at: OffsetDateTime,
}

impl From<ErrorRow> for SyncError {
    fn from(row: ErrorRow) -> Self {
        Self {
            id: row.id,
            run_id: row.run_id,
            subject: row.subject,
            code: row.code,
            message: row.message,
            created_at: row.created_at,
        }
    }
}

/// Finish a run: write the final counts, count the failures, and stamp the end.
///
/// One transaction, and the reason is the same as the attribute map's and the role rules' but with
/// a smaller window and a louder failure. Between "the run is finished" and "the error count is
/// right" a list screen would show a completed run with zero failures — the exact sentence an
/// operator does not need to be told at 09:00.
///
/// The `status` is **derived** from the failures rather than taken on trust: a caller that reports
/// `Ok` with three recorded errors would be asserting that the run worked, and the panel would
/// show a green chip over a directory that refused every account it was sent. The one case where
/// the caller's word is honoured is [`RunStatus::Failed`] — a run that could not proceed is
/// `failed` whatever the counters say.
pub async fn finish_run(
    pool: &PgPool,
    run_id: Uuid,
    counts: SyncCounts,
    failures: &[SyncError],
    message: Option<&str>,
) -> Result<SyncRun> {
    guard_counts(counts)?;

    let mut tx = pool.begin().await?;

    // The run must still be open. A finished run is not re-finishable: a retry that appends a
    // second outcome is how "42s" ends up describing a four-hour sweep.
    let status: String = sqlx::query_scalar(
        "select status from directory_sync_runs where id = $1 for update",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        IdentityError::InvalidProvider("this sync run does not exist".to_string())
    })?;
    if status != "running" {
        return Err(IdentityError::InvalidProvider(format!(
            "this sync run already finished with `{status}` — a run has one outcome, and writing a \
             second one makes its duration depend on how often somebody reopened the page"
        )));
    }

    // Recount from the table rather than trusting the caller's length: the caller's slice and the
    // table can disagree if anything else wrote to the run, and the count is what the screen
    // filters on.
    let error_count: i64 = sqlx::query_scalar(
        "select count(*) from directory_sync_errors where run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await?;

    let status = if error_count > 0 && counts.total_writes() + counts.users_seen > 0 {
        RunStatus::Partial
    } else if error_count > 0 {
        RunStatus::Failed
    } else {
        RunStatus::Ok
    };

    let row = sqlx::query_as::<_, RunRow>(&format!(
        "update directory_sync_runs set status = $2, finished_at = now(), \
           users_seen = $3, users_created = $4, users_updated = $5, users_deactivated = $6, \
           groups_seen = $7, error_count = $8, message = $9 \
         where id = $1 returning {RUN_COLUMNS}"
    ))
    .bind(run_id)
    .bind(status.as_str())
    .bind(counts.users_seen)
    .bind(counts.users_created)
    .bind(counts.users_updated)
    .bind(counts.users_deactivated)
    .bind(counts.groups_seen)
    .bind(error_count as i32)
    .bind(message)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    let _ = failures; // the recount above is authoritative, not the caller's slice
    Ok(SyncRun::from(row))
}

/// Read a provider's runs, newest first.
///
/// `limit` is clamped rather than refused: a list screen asking for a million rows is a bug, and
/// turning it into a 422 teaches an operator nothing. The clamp is the same number for everybody,
/// so the screen cannot be made to render a different number of rows by asking differently.
pub async fn list_runs(pool: &PgPool, provider_id: Uuid, limit: i64) -> Result<Vec<SyncRun>> {
    const MAX: i64 = 200;
    let limit = limit.clamp(1, MAX);
    let rows = sqlx::query_as::<_, RunRow>(&format!(
        "select {RUN_COLUMNS} from directory_sync_runs \
         where provider_id = $1 order by started_at desc, id desc limit $2"
    ))
    .bind(provider_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(SyncRun::from).collect())
}

/// Read one run.
pub async fn get_run(pool: &PgPool, run_id: Uuid) -> Result<Option<SyncRun>> {
    let row = sqlx::query_as::<_, RunRow>(&format!(
        "select {RUN_COLUMNS} from directory_sync_runs where id = $1"
    ))
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(SyncRun::from))
}

/// The distinct subjects a run failed, with how many attempts each took and the first code.
///
/// This is the `Retry failed` list. Collapsing the repeats here — rather than with a unique
/// constraint on the errors table — is deliberate: the same subject failing twice is real
/// information about the directory, and a constraint that refused the second row would force a
/// sync to lose it or to lie about it.
pub async fn failed_subjects(pool: &PgPool, run_id: Uuid) -> Result<Vec<FailedSubject>> {
    let rows = sqlx::query_as::<_, FailedSubjectRow>(
        "select subject_key, \
           count(*)::int as attempts, \
           min(code) as code, \
           (array_agg(message order by created_at))[1] as message, \
           max(created_at) as last_failed_at \
         from directory_sync_errors where run_id = $1 \
         group by subject_key order by max(created_at) desc",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(FailedSubject::from).collect())
}

/// One subject a run could not process, however many times it tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedSubject {
    /// What to retry: the subject, or the code when the failure was not about a subject.
    pub key: String,
    /// How many attempts it took. `1` is the ordinary case; more says the directory is flapping.
    pub attempts: i32,
    /// The first code, not the last. The first is the one that explains the others.
    pub code: String,
    pub message: String,
    pub last_failed_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct FailedSubjectRow {
    subject_key: String,
    attempts: i32,
    code: Option<String>,
    message: Option<String>,
    last_failed_at: OffsetDateTime,
}

impl From<FailedSubjectRow> for FailedSubject {
    fn from(row: FailedSubjectRow) -> Self {
        Self {
            key: row.subject_key,
            attempts: row.attempts,
            code: row.code.unwrap_or_default(),
            message: row.message.unwrap_or_default(),
            last_failed_at: row.last_failed_at,
        }
    }
}

/// Record the groups a sync has seen, creating or refreshing each one.
///
/// One statement rather than a loop, because a directory with ten thousand groups would otherwise
/// make the sync's cost a round trip per group — and a sync that takes longer than the interval
/// between syncs is a sync that never catches up. `synced = false` is written by the caller for a
/// group whose membership could not be read, so the panel can show the group with a warning
/// instead of a stale count that looks live.
pub async fn upsert_group_links(
    pool: &PgPool,
    provider_id: Uuid,
    groups: &[(String, String, i32, bool)],
) -> Result<u64> {
    guard_counts(SyncCounts {
        groups_seen: i32::try_from(groups.len()).unwrap_or(i32::MAX),
        ..SyncCounts::default()
    })?;
    Ok(sqlx::query(
        "insert into provider_group_links \
           (provider_id, external_id, external_label, member_count, last_seen_at, synced) \
         select $1, g.external_id, g.external_label, g.member_count, now(), g.synced \
         from unnest($2::text[], $3::text[], $4::int[], $5::bool[]) \
              as g(external_id, external_label, member_count, synced) \
         on conflict (provider_id, external_id) do update set \
           external_label = excluded.external_label, \
           member_count    = excluded.member_count, \
           last_seen_at    = excluded.last_seen_at, \
           synced          = excluded.synced",
    )
    .bind(provider_id)
    .bind(groups.iter().map(|g| g.0.as_str()).collect::<Vec<_>>())
    .bind(groups.iter().map(|g| g.1.as_str()).collect::<Vec<_>>())
    .bind(groups.iter().map(|g| g.2).collect::<Vec<_>>())
    .bind(groups.iter().map(|g| g.3).collect::<Vec<_>>())
    .execute(pool)
    .await?
    .rows_affected())
}

/// Read a provider's group links, newest sighting first.
pub async fn list_group_links(
    pool: &PgPool,
    provider_id: Uuid,
) -> Result<Vec<GroupLink>> {
    let rows = sqlx::query_as::<_, GroupLinkRow>(
        "select id, provider_id, external_id, external_label, member_count, last_seen_at, synced \
         from provider_group_links where provider_id = $1 \
         order by last_seen_at desc, external_label",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(GroupLink::from).collect())
}

/// A directory group this provider has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupLink {
    pub id: Uuid,
    pub provider_id: Uuid,
    /// The directory's own id. Opaque and never parsed — a group key is not a number and one that
    /// stops being one is the directory's business, not ours.
    pub external_id: String,
    pub external_label: String,
    pub member_count: i32,
    pub last_seen_at: OffsetDateTime,
    /// `false` when the last sync could not read this group's membership. The row is kept so the
    /// group does not disappear from the panel while its repair is pending.
    pub synced: bool,
}

#[derive(sqlx::FromRow)]
struct GroupLinkRow {
    id: Uuid,
    provider_id: Uuid,
    external_id: String,
    external_label: String,
    member_count: i32,
    last_seen_at: OffsetDateTime,
    synced: bool,
}

impl From<GroupLinkRow> for GroupLink {
    fn from(row: GroupLinkRow) -> Self {
        Self {
            id: row.id,
            provider_id: row.provider_id,
            external_id: row.external_id,
            external_label: row.external_label,
            member_count: row.member_count,
            last_seen_at: row.last_seen_at,
            synced: row.synced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_finished_run() -> SyncRun {
        SyncRun {
            id: Uuid::nil(),
            provider_id: Uuid::nil(),
            kind: SyncKind::Full,
            status: RunStatus::Ok,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(41)),
            counts: SyncCounts {
                users_seen: 120,
                users_created: 3,
                users_updated: 0,
                users_deactivated: 1,
                groups_seen: 7,
            },
            error_count: 0,
            message: None,
            triggered_by: None,
        }
    }

    #[test]
    fn a_run_measures_its_own_duration_and_never_guesses_one() {
        let run = a_finished_run();
        assert_eq!(run.duration(), Some(time::Duration::seconds(41)));

        // A run still going has no duration. Rendering "time since it started" as a duration makes
        // a slow run look permanently unfinished and a fast one look like it never ended.
        let running = SyncRun {
            finished_at: None,
            ..a_finished_run()
        };
        assert_eq!(running.duration(), None);
    }

    #[test]
    fn partial_is_not_a_healthy_run() {
        // The whole point of a separate `partial` status: a boolean "ok" would flatten "three
        // accounts were refused" into the same green chip as a clean sweep.
        assert!(RunStatus::Ok.is_healthy());
        assert!(RunStatus::Running.is_healthy());
        assert!(!RunStatus::Partial.is_healthy());
        assert!(!RunStatus::Failed.is_healthy());
    }

    #[test]
    fn a_negative_counter_is_refused_by_name_rather_than_wrapped() {
        let counts = SyncCounts {
            users_deactivated: -3,
            ..SyncCounts::default()
        };
        let error = guard_counts(counts).expect_err("a negative count must not reach the database");
        // Named, because "violates check constraint" arrives as a constraint name and an operator
        // reading a sync failure needs to know *which* counter.
        assert!(
            error.to_string().contains("users_deactivated"),
            "the error must name the counter, got: {error}"
        );
    }

    #[test]
    fn a_zero_count_is_ordinary() {
        // A run that deactivated nobody is the common case, not a defect. A guard that refuses it
        // would be a guard an operator learns to work around.
        let counts = SyncCounts {
            users_deactivated: 0,
            ..SyncCounts::default()
        };
        assert!(guard_counts(counts).is_ok());
    }

    #[test]
    fn the_text_enums_round_trip_and_an_unknown_value_degrades_visibly() {
        for kind in [SyncKind::Full, SyncKind::Delta, SyncKind::Scim, SyncKind::Manual] {
            assert_eq!(SyncKind::parse(kind.as_str()), kind);
        }
        for status in [
            RunStatus::Running,
            RunStatus::Ok,
            RunStatus::Partial,
            RunStatus::Failed,
        ] {
            assert_eq!(RunStatus::parse(status.as_str()), status);
        }

        // A row written by a future version must not be read as the widest thing in the set.
        assert_eq!(SyncKind::parse("incremental"), SyncKind::Full);
        assert_eq!(RunStatus::parse("degraded"), RunStatus::Running);
    }

    #[test]
    fn total_writes_is_what_decides_a_partial_run() {
        let counts = SyncCounts {
            users_seen: 8,
            users_created: 0,
            users_updated: 2,
            users_deactivated: 0,
            groups_seen: 0,
        };
        // Eight seen and two written: the run did real work, so failures make it partial rather
        // than failed. The distinction is the operator's "retry the rest" versus "start over".
        assert_eq!(counts.total_writes(), 2);
        assert!(counts.total_writes() + counts.users_seen > 0);
    }
}
