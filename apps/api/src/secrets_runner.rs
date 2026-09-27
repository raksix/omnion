//! The re-wrap runner: the background writer that walks a rotation's versions.
//!
//! `main.rs` spawns this task when the runner is enabled (`OMNION_SECRETS_RUNNER`, default on).
//! Each tick looks for a live re-wrap job and re-seals one small batch of its versions, then
//! sleeps. Everything it needs to resume after a restart is a row: the job's `status`, its
//! `cursor` and its `rewrapped_count`.
//!
//! The knobs are `OMNION_SECRETS_POLL_MS` and `OMNION_SECRETS_REWRAP` (the batch size, which
//! defaults to the crate's [`omnion_secrets::REWRAP_BATCH`]).
//!
//! Two properties this runner is written to keep:
//!
//! * **It never holds a lock a normal read needs.** A batch is a handful of single-row updates,
//!   and between them the pool is free — so a lease redemption during a rotation succeeds on the
//!   old key for every version the walk has not reached. That is the invariant the request makes
//!   about a rotation being an online ceremony.
//! * **A rotation that cannot finish says so instead of skipping.** A version that will not
//!   unseal pauses the job with the reason, and the operator sees it on the screen. The walk
//!   never advances past a version it could not re-seal.

use std::time::Duration as StdDuration;

use omnion_secrets::store::{self, RewrapJob};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    // The floor keeps a misconfigured poll from spinning the runner.
    let poll_ms = state.config().secrets.rewrap_poll_ms.max(250);
    tracing::info!(poll_ms, "secret re-wrap runner started");

    tokio::spawn(async move {
        // A fresh installation may have no key ring at all; the first write creates one. Doing
        // it here means an operator who opens the key screen before saving any secret still
        // sees a real ring rather than an empty state that is really a missing key.
        if let Err(error) = store::ensure_active_key(state.db().pool()).await {
            tracing::info!(error = %error, "no root key was created at boot");
        }

        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not become a burst of catch-up ticks: the versions are still on the
        // old key, and the next tick walks them.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match tick(&state).await {
                Ok(true) => {}
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "the re-wrap tick failed");
                }
            }
            // The lease revocation runs on the same tick, and independently: a broken re-wrap
            // must not stop a deploy from revoking the leases it invalidated, which is the
            // whole point of the consumer rather than a call from the deploy handler.
            if let Err(error) = revoke_leases_for_deploys(&state).await {
                tracing::warn!(error = %error, "the deployment lease revocation failed");
            }
        }
    })
}

/// One tick: advance the live job by one batch. Returns whether a batch was applied.
async fn tick(state: &AppState) -> Result<bool, omnion_secrets::SecretsError> {
    let pool = state.db().pool();
    let Some(job) = store::live_rewrap_job(pool).await? else {
        return Ok(false);
    };
    if job.status == "paused" {
        // A paused job is an operator's decision; the runner leaves it alone.
        return Ok(false);
    }
    let report = store::rewrap_batch(pool, job.id).await?;
    if !report.is_idle() {
        tracing::info!(
            job = %job.id,
            rewrapped = report.rewrapped,
            complete = report.complete,
            "re-wrap batch applied"
        );
    }
    Ok(true)
}

/// The consumer's own name in `event_consumer_cursors`. It is the primary key of the cursor
/// row, so it is a contract with the migration that seeded it, not a free-form label.
const LEASE_REVOCATION_CONSUMER: &str = "secrets.lease_revocation";

/// The event whose arrival revokes an environment's leases.
const DEPLOYMENT_STARTED: &str = "deployment.started";

/// How many events one tick reads. A deploy-heavy installation can emit a burst, and reading a
/// bounded batch keeps the runner's tick time predictable — the next tick picks up the rest.
const CONSUMER_BATCH: i64 = 100;

/// Revoke the live leases of every environment a `deployment.started` event names.
///
/// This is the request's "consumed" event, and it is a consumer rather than a call from the
/// deployment handler on purpose. The deployment centre belongs to REQ-024 and does not know
/// that this request exists; if the revocation lived in the deploy handler, every future deploy
/// path would have to remember to call it, and the one that forgot would be exactly the case
/// the rule exists for — a redeploy running on the credential the operator just replaced.
///
/// Reading from a cursor instead of the whole stream means:
///
/// * a restart resumes where it left off rather than re-revoking everything;
/// * a deploy recorded by another writer is honoured, because it is in the same `events`
///   table;
/// * the cursor moves only after the revocation succeeded, so a failed tick is retried rather
///   than skipped.
pub async fn revoke_leases_for_deploys(state: &AppState) -> Result<usize, omnion_secrets::SecretsError> {
    let pool = state.db().pool();
    // A database that predates this migration has no cursor table at all. That is not an
    // error worth failing a tick over: the install simply has no deployment-driven revocation
    // yet, and the next migration run creates the table.
    let Ok(cursor) = sqlx::query_scalar::<_, i64>(
        "select last_event_id from event_consumer_cursors where consumer = $1",
    )
    .bind(LEASE_REVOCATION_CONSUMER)
    .fetch_optional(pool)
    .await
    else {
        return Ok(0);
    };
    let Some(cursor) = cursor else {
        return Ok(0);
    };

    let events: Vec<(i64, serde_json::Value)> = sqlx::query_as(
        "select id, payload from events where id > $1 and name = $2 order by id limit $3",
    )
    .bind(cursor)
    .bind(DEPLOYMENT_STARTED)
    .bind(CONSUMER_BATCH)
    .fetch_all(pool)
    .await?;
    if events.is_empty() {
        return Ok(0);
    }

    let mut revoked_total = 0;
    for (event_id, payload) in events {
        let Some(environment) = payload
            .get("environment")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            // An event with no environment names nothing to revoke. The cursor still moves:
            // a malformed event is not going to become a well-formed one by being read again.
            advance(pool, event_id).await?;
            continue;
        };

        let reason = format!("revoked automatically: a deployment started in {environment}");
        match omnion_secrets::leases::revoke_environment_leases(pool, environment, &reason).await {
            Ok(ids) => {
                revoked_total += ids.len();
                for lease_id in &ids {
                    tracing::info!(
                        %lease_id,
                        environment,
                        "lease revoked by a deployment"
                    );
                    // A denial would be wrong here: the leases were valid and the operator
                    // replaced the credential underneath them. `secret.lease.revoked` says so
                    // with the reason attached.
                    let _ = omnion_audit::entries::record(
                        pool,
                        omnion_audit::NewAuditEntry::system("secret.lease.revoked")
                            .target("lease", lease_id.to_string())
                            .metadata(serde_json::json!({
                                "reason": reason,
                                "environment": environment,
                                "event_id": event_id,
                            })),
                    )
                    .await;
                }
                advance(pool, event_id).await?;
            }
            Err(error) => {
                // The cursor stays where it is, so the next tick retries this event rather than
                // stepping over a deployment whose leases were not revoked.
                tracing::warn!(%error, event_id, environment, "a deployment did not revoke its leases");
                break;
            }
        }
    }
    Ok(revoked_total)
}

/// Move the consumer's cursor past one event.
async fn advance(
    pool: &sqlx::PgPool,
    event_id: i64,
) -> Result<(), omnion_secrets::SecretsError> {
    sqlx::query(
        "update event_consumer_cursors set last_event_id = $2, processed = processed + 1,                 updated_at = now()          where consumer = $1 and last_event_id < $2",
    )
    .bind(LEASE_REVOCATION_CONSUMER)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The finished jobs the screen's history strip reads, newest first.
pub async fn recent_jobs(
    state: &AppState,
    limit: i64,
) -> Result<Vec<RewrapJob>, omnion_secrets::SecretsError> {
    let rows = sqlx::query_as::<_, RewrapJob>(
        "select id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                pause_reason, last_error, started_at, completed_at \
         from secret_rewrap_jobs where status in ('completed', 'failed') \
         order by started_at desc limit $1",
    )
    .bind(limit)
    .fetch_all(state.db().pool())
    .await?;
    Ok(rows)
}
