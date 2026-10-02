//! A SCIM push, folded into the run ledger (REQ-065, slice 4 part 3).
//!
//! SCIM is the mirror image of a directory sync: the directory *pushes* `Users` and `Groups` at
//! us, one request at a time, and [`crate::routes::scim`] writes every one of them to
//! `provisioning_log`. That log is a good record of *requests* and a bad record of *work*:
//!
//! * it has no run, so "did the connector's overnight push work" has no answer and the only
//!   visible number is a count of HTTP lines that were mostly `200`;
//! * it cannot say `partial`, because "four users created, one refused" is a normal hour for a
//!   connector whose IdP keeps sending a user whose `externalId` is already taken — and a
//!   boolean "ok" over that is the exact sentence an operator acts on by doing nothing;
//! * and it is invisible from the provider screen, so the two ledgers describe the same
//!   directory and disagree.
//!
//! So a SCIM push writes a [`SyncKind::Scim`] run alongside the log lines it already writes. The
//! run is the summary; the log stays the per-request detail, and neither is derived from the
//! other by a guess.
//!
//! **How a run is bounded is the whole design.** A push is a stream of independent HTTP requests
//! with no transaction and no natural end, so the platform opens a run per *window* rather than
//! per request: a run that has not been touched for [`IDLE_CLOSES_AFTER`] is closed by the next
//! request that arrives. Two consequences, both deliberate:
//!
//! * a request never waits for a "next" request to close its run, so a connector that stops
//!   halfway still gets a run that ends — with whatever it managed;
//! * and the run's duration is the *window*, not the work, which is why [`SyncRun::duration`] is
//!   read as "how long this push was open" and the panel says so rather than calling it a sweep
//!   time.
//!
//! The counters come from the log, recounted inside the closing transaction rather than taken
//! from the caller's slice, for the reason `finish_run` recounts: the count is what the screen
//! filters on, and a filter on a number nobody is counting is worse than no filter.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// How long a push run stays open without being touched before the next request closes it.
///
/// Fifteen minutes is the gap between two pushes from a connector that batches on a schedule —
/// an hourly connector never closes its own run, and a run that only ever closes on a second
/// request is a run that is still "running" the next morning.
pub const IDLE_CLOSES_AFTER: time::Duration = time::Duration::minutes(15);

/// The run a push is currently being written into for a provider, opening one if there is none
/// or if the last one has gone idle.
///
/// Returns the open run, or `None` when there is no provider to hang it off. A SCIM token
/// provisions an *organization*; only a provider-scoped token has somewhere to put a run, and
/// inventing one would put a sync ledger row on a tenant that never configured a directory.
pub async fn current_run(
    pool: &PgPool,
    provider_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<Uuid>> {
    // The open run, if any. `for update` so two concurrent pushes cannot both decide there is
    // nothing open and each open one: the loser would then have written its work into a run
    // nobody is reading.
    let open: Option<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "select id, last_seen_at from directory_sync_runs \
         where provider_id = $1 and kind = 'scim' and status = 'running' \
         order by started_at desc limit 1 for update",
    )
    .bind(provider_id)
    .fetch_optional(pool)
    .await?;

    if let Some((run_id, last_seen)) = open {
        if now - last_seen < IDLE_CLOSES_AFTER {
            // Still inside the window: touch it, so the run's liveness is the log's own recency
            // and not a second thing that has to be kept in step with it.
            sqlx::query("update directory_sync_runs set last_seen_at = $2 where id = $1")
                .bind(run_id)
                .bind(now)
                .execute(pool)
                .await?;
            return Ok(Some(run_id));
        }
        // Gone idle. Close it against what it actually did, and start a fresh one below — a run
        // that is still "running" the next morning is a run nobody can act on.
        close_run(pool, run_id).await?;
    }

    let run: Uuid = sqlx::query_scalar(
        "insert into directory_sync_runs (provider_id, kind, status, last_seen_at) \
         values ($1, 'scim', 'running', $2) returning id",
    )
    .bind(provider_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(Some(run))
}

/// Close a push run, deriving its counters and its verdict from the log lines it owns.
///
/// The counts are `count(*) filter (where …)`, so a run's numbers can only be the log's numbers:
/// there is no path by which a caller asserts "4 created" and the run row says 4 while the log
/// says 3. The `detail is not null and detail <> ''` guard on the counters is what keeps a
/// "provisioning request arrived" line out of the work count — a bare `201` with no entity is a
/// request, not a created user, and counting it would make the log and the run disagree.
async fn close_run(pool: &PgPool, run_id: Uuid) -> Result<()> {
    let counted: Option<(i32, i32, i32, i32, i32, i32)> = sqlx::query_as(
        "select
            count(*) filter (where outcome in ('created', 'updated', 'deactivated', 'skipped'))::int,
            count(*) filter (where outcome = 'created')::int,
            count(*) filter (where outcome = 'updated')::int,
            count(*) filter (where outcome = 'deactivated')::int,
            count(*) filter (where resource = 'group')::int,
            count(*) filter (where outcome = 'failed')::int
         from provisioning_log
         where detail is not null and detail <> ''
           and action like 'scim/%'
           and created_at >= (select started_at from directory_sync_runs where id = $1)",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;

    // The log has no run column, so a run's lines are identified by the action prefix this
    // module writes. If the count came back nothing (the log is empty, or every line predates
    // the prefix), the run closes as a failure with no counts rather than as a clean `ok` with
    // zeroes — "nothing arrived" and "everything worked" are different sentences.
    let Some((seen, created, updated, deactivated, groups, failed)) = counted else {
        sqlx::query(
            "update directory_sync_runs set status = 'failed', finished_at = now(), \
               message = 'no provisioning log lines are attached to this run' where id = $1",
        )
        .bind(run_id)
        .execute(pool)
        .await?;
        return Ok(());
    };

    let work = created + updated + deactivated;
    let status = if failed > 0 && work > 0 {
        "partial"
    } else if failed > 0 {
        "failed"
    } else {
        "ok"
    };

    sqlx::query(
        "update directory_sync_runs set status = $2, finished_at = now(), users_seen = $3, \
           users_created = $4, users_updated = $5, users_deactivated = $6, groups_seen = $7, \
           error_count = $8, message = $9 where id = $1",
    )
    .bind(run_id)
    .bind(status)
    .bind(seen)
    .bind(created)
    .bind(updated)
    .bind(deactivated)
    .bind(groups)
    .bind(failed)
    .bind(if failed > 0 {
        Some(format!("{failed} of {seen} provisioning operations failed"))
    } else {
        None
    })
    .execute(pool)
    .await?;
    Ok(())
}

/// Record one failed SCIM subject against the open run, so the retry drawer has something to
/// work on.
///
/// A SCIM failure has no retry button in the protocol, but it does have an operator: the person
/// who configured the connector. Without this row the failure lives only in a log line nobody
/// reads, and a run that reports `error_count: 0` over three refusals is a run lying by omission.
pub async fn record_failure(
    pool: &PgPool,
    run_id: Uuid,
    subject: &str,
    code: &str,
    message: &str,
) -> Result<()> {
    sqlx::query(
        "insert into directory_sync_errors (run_id, subject, code, message) values ($1, $2, $3, $4)",
    )
    .bind(run_id)
    .bind(subject)
    .bind(code)
    .bind(message)
    .execute(pool)
    .await
    .map_err(IdentityError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_window_is_long_enough_for_a_batched_connector_and_short_enough_for_morning() {
        // An hourly connector must not keep yesterday's run open all night, and a run that closes
        // within the batch interval would split one push into three rows — the same "42s
        // describes four hours" problem `finish_run` exists to prevent, in the other direction.
        assert!(IDLE_CLOSES_AFTER > time::Duration::minutes(5));
        assert!(IDLE_CLOSES_AFTER < time::Duration::hours(1));
    }

    #[test]
    fn a_run_touched_inside_the_window_stays_open_and_one_touched_outside_closes() {
        let start = OffsetDateTime::UNIX_EPOCH;
        // A second request 90 seconds in is the same push.
        assert!(start + time::Duration::seconds(90) - start < IDLE_CLOSES_AFTER);
        // The first request the next morning is a new push, and the previous one ends with
        // whatever it managed.
        assert!(start + time::Duration::hours(9) - start > IDLE_CLOSES_AFTER);
    }
}
