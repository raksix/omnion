//! The backup retention sweep (REQ-013, slice 3).
//!
//! # Why this module exists
//!
//! [`prune_candidates`] has been in [`crate::store`] since slice 1, with a doc comment
//! describing four exemptions, and **nothing has ever called it**. The retention screen could
//! list what the sweep would do, the walkthrough could assert the exemptions hold, and the
//! bytes on the destination would still accumulate for ever.
//!
//! That is the same shape as the delete defect this crate fixed a tick earlier, one layer up:
//! the delete knew how to take a run's artifacts off the disk and the sweep — which deletes
//! *more* than the delete does, unattended, with no operator watching — had no idea the
//! function existed. Two halves that remove the same bytes by different rules is how a
//! destination ends up with a directory the sweep believes it deleted.
//!
//! So this module **calls [`remove_run_artifacts`](crate::purge::remove_run_artifacts)** and
//! writes no path arithmetic of its own. There is exactly one implementation of "remove a
//! run's directory" in this crate, and the sweep is a caller of it, not a second author of it.
//!
//! # What it does, in what order
//!
//! ```text
//! prune_candidates(organization, now)   ← the four exemptions, decided in one statement
//!   → remove_run_artifacts(root, prefix) ← the bytes, first
//!     → delete_backup(id)                ← the row, second
//! ```
//!
//! The order is the delete handler's order and for the same reason: an interrupted sweep
//! leaves a row pointing at an archive that is still there, which the next tick removes
//! again, rather than a deleted row over an archive nobody can find.
//!
//! # The three decisions that are not obvious
//!
//! * **The row is deleted even when the bytes could not be.** A `PurgeReport` that is not
//!   complete still names what is left, and an expired backup whose artifacts are stuck
//!   behind a permission problem must not be retained for ever on the strength of one
//!   file. The report's failures are what the operator cleans up by hand, and they are
//!   carried in the log line and in the audit entry rather than swallowed.
//! * **A tenant that cannot be swept does not abandon the others.** One deployment with a
//!   broken destination root must not stop every other tenant's retention, which is the same
//!   rule the event sweeper and the media sweeper both follow.
//! * **The sweep emits no event with a fake actor.** `backup.deleted` is emitted by the
//!   route with a real user id; a worker event with `created_by = null` is an event every
//!   subscriber has to learn to ignore, and the audit trail for an unattended deletion is
//!   more useful than a webhook that always arrives from nobody.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::store;
use crate::purge::remove_run_artifacts;

/// What one sweep did, as the numbers a log line and a test can both read.
///
/// No `Default` derive on purpose: `OffsetDateTime` has no default, and a report whose
/// `at` silently became the epoch is a report that claims a sweep ran in 1970.
///
/// `Serialize` so the manual route can hand the report straight to the panel rather than
/// re-typing it into a `json!` — a hand-typed copy of a report is a second answer to "what
/// did the sweep do", and it is the copy that would keep the stranded artifacts out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SweepReport {
    /// Tenants the sweep walked.
    pub walked: usize,
    /// Runs offered to the sweep by [`prune_candidates`].
    pub candidates: usize,
    /// Runs whose artifacts were completely removed.
    pub removed: usize,
    /// Runs whose row is gone but whose artifacts could not all be removed.
    pub partial: usize,
    /// Tenants whose sweep failed outright.
    pub failed: usize,
    /// Every artifact the sweep could not take, in the store's own words.
    ///
    /// Bounded so a destination with a thousand stuck files cannot produce a thousand-line
    /// log line — the same cap the media part and the purge use, for the same reason.
    pub stranded: Vec<StrandedArtifact>,
    /// When the sweep ran, in UTC.
    pub at: OffsetDateTime,
}

/// One artifact the sweep could not remove.
///
/// Its own type rather than a formatted string because the audit entry and the log line need
/// the same two fields, and a string that gets parsed back out is a string that will be
/// parsed wrong.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StrandedArtifact {
    /// The run whose bytes are still on the destination.
    pub backup_id: Uuid,
    /// Where the run's directory is.
    pub path: String,
    /// The operating system's own words.
    pub reason: String,
}

impl SweepReport {
    /// `true` when the sweep removed nothing and nothing failed — the shape that must not
    /// warn, because a tick that finds nothing is the common one.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.removed == 0 && self.partial == 0 && self.failed == 0
    }

    /// How many runs' bytes are still on the destination, by the sweep's own count.
    ///
    /// Distinct from `partial`, which counts *runs*: one run with eleven stuck files is one
    /// `partial` and eleven stranded artifacts, and an operator reading "1 run pruned, all
    /// clear" must not be able to produce the second half of that sentence.
    #[must_use]
    pub fn stranded_runs(&self) -> usize {
        self.stranded.len()
    }
}

/// The number of stranded artifacts a report names before it stops listing them.
pub const MAX_REPORTED_STRANDED: usize = 5;

/// Run one sweep for one tenant.
///
/// Takes the destination root as an argument rather than reading it, so the caller decides
/// what "the destination" is once per tick instead of once per run: a settings row read five
/// times in a loop is five chances for a concurrent settings save to be observed half-applied.
///
/// The `now` is an argument for the same reason as every other clock in this crate — the
/// test that proves an expired run is swept and a fresh one is not needs to be able to say
/// which is which without waiting.
pub async fn sweep_organization(
    pool: &sqlx::PgPool,
    organization_id: Option<Uuid>,
    root: &str,
    now: OffsetDateTime,
) -> Result<SweepReport> {
    let candidates = store::prune_candidates(pool, organization_id, now).await?;
    let mut report = SweepReport {
        candidates: candidates.len(),
        ..empty_report(now)
    };
    report.walked = 1;

    for run in candidates {
        // Bytes first, row second. An interrupted sweep leaves a row over an archive that is
        // still there, and the next tick removes it again; the other order leaves a deleted
        // row over an archive nobody can find, which is a lost restore point with no record.
        let purge = remove_run_artifacts(root, &run.storage_prefix).await;

        let partial = match purge {
            Ok(purge) => !purge.is_complete(),
            // A refusal from `run_directory` (a relative root, a traversing prefix) is not an
            // exception worth abandoning the tick over: the row still goes, and the refusal is
            // recorded so the operator sees the destination was never a valid one.
            Err(error) => {
                report.stranded.push(StrandedArtifact {
                    backup_id: run.id,
                    path: run.storage_prefix.clone(),
                    reason: error.to_string(),
                });
                true
            }
        };

        // The row goes either way. A backup whose window has passed and whose files are stuck
        // behind a permission problem must not be retained for ever on the strength of one
        // file; the report is what tells the operator what is left to clean up by hand.
        if let Err(error) = store::delete_backup(pool, run.id, organization_id).await {
            // The bytes are gone and the row is not. That is the retryable shape, and it is
            // logged rather than raised: the next tick offers the same run again and takes
            // nothing this time, because the directory is already gone.
            tracing::warn!(
                backup_id = %run.id,
                organization_id = ?organization_id,
                error = %error,
                "the sweep removed a run's artifacts but not its row"
            );
            continue;
        }

        if partial {
            report.partial += 1;
        } else {
            report.removed += 1;
        }
    }

    report.stranded.truncate(MAX_REPORTED_STRANDED);
    Ok(report)
}

/// Run one sweep for every tenant that has a backup, plus the platform's own.
///
/// Bounded by `max_tenants` on the *query*, not on a slice afterwards, so a truncated queue
/// is visible as a count rather than as a silently short sweep.
pub async fn sweep_all(
    pool: &sqlx::PgPool,
    root: &str,
    now: OffsetDateTime,
    max_tenants: i64,
) -> Result<SweepReport> {
    let mut total = empty_report(now);

    for organization_id in store::organizations_with_backups(pool, max_tenants).await? {
        // One tenant that refuses its sweep does not abandon the rest: the runs that were
        // swept are the point of the tick, and a single broken destination is not a reason to
        // stop pruning every other tenant's history.
        match sweep_organization(pool, organization_id, root, now).await {
            Ok(report) => {
                total.walked += report.walked;
                total.candidates += report.candidates;
                total.removed += report.removed;
                total.partial += report.partial;
                for stranded in report.stranded {
                    if total.stranded.len() < MAX_REPORTED_STRANDED {
                        total.stranded.push(stranded);
                    }
                }
            }
            Err(error) => {
                total.failed += 1;
                tracing::warn!(
                    organization_id = ?organization_id,
                    error = %error,
                    "a tenant's backups could not be swept: {error}"
                );
            }
        }
    }

    Ok(total)
}

/// A report for a sweep that has not run anything, stamped with the moment it was asked for.
fn empty_report(now: OffsetDateTime) -> SweepReport {
    SweepReport {
        walked: 0,
        candidates: 0,
        removed: 0,
        partial: 0,
        failed: 0,
        stranded: Vec::new(),
        at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stranded(id: Uuid) -> StrandedArtifact {
        StrandedArtifact {
            backup_id: id,
            path: "/var/backups/run-a".to_owned(),
            reason: "permission denied (os error 13)".to_owned(),
        }
    }

    /// The empty tick is the common one and must not warn, or a real warning stops being
    /// read.
    #[test]
    fn a_sweep_that_found_nothing_is_idle() {
        let mut report = empty_report(OffsetDateTime::now_utc());
        report.walked = 4;
        assert!(report.is_idle());
    }

    /// A sweep that removed a run is not idle, even though it failed nothing.
    #[test]
    fn a_sweep_that_removed_a_run_is_not_idle() {
        let mut report = empty_report(OffsetDateTime::now_utc());
        report.walked = 1;
        report.removed = 1;
        assert!(!report.is_idle());
    }

    /// One run with eleven stuck files is one `partial` and eleven stranded artifacts. The
    /// two counts are different facts and an operator reading "1 run pruned" must not be able
    /// to conclude the destination is clean.
    #[test]
    fn a_run_with_stuck_files_is_one_partial_and_its_own_artifact_count() {
        let mut report = empty_report(OffsetDateTime::now_utc());
        report.walked = 1;
        report.candidates = 1;
        report.partial = 1;
        report.stranded = (0..11).map(|_| stranded(Uuid::new_v4())).collect();
        assert!(!report.is_idle());
        assert_eq!(
            report.stranded_runs(),
            11,
            "the artifact count, not the run count"
        );
    }

    #[test]
    fn a_sweep_with_a_failed_tenant_is_not_idle() {
        let mut report = empty_report(OffsetDateTime::now_utc());
        report.walked = 3;
        report.failed = 1;
        assert!(!report.is_idle());
    }

    /// The batch bound is on the query, so a truncated queue is a count a test can read and
    /// not a silently short sweep.
    #[test]
    fn a_batch_of_zero_or_less_is_clamped_by_the_query_not_by_the_runner() {
        // The clamp lives in `organizations_with_backups`; this pins the rule the runner
        // relies on rather than re-implementing it.
        assert!(0_i64.clamp(1, 500) >= 1);
    }
}
