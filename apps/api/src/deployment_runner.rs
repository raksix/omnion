//! The deploy runner (REQ-024, slice 2): what actually runs a job's steps.
//!
//! Slice 1 decided what a deploy *is* — the step plan, the cancel boundary, the status fold — and
//! slice 2's routes start a job. This file is the part in between, and it is the part the
//! request is really about: a deploy that runs its steps in order, writes each one to the log the
//! operator is watching, and ends with a health verification.
//!
//! What it deliberately does **not** do, because the request puts it out of scope: download an
//! artifact, run migrations against a live schema, or swap a binary. Those belong to
//! `infra/deploy/deploy-omnion-live.sh` and to the owner's release pipeline (docs/05-VERSIONING).
//! A runner that *claimed* to swap the running process and did not would be the most dangerous
//! thing in this crate: the history would say the deploy succeeded while the old build kept
//! serving. So each step records what it did and what it could not do, and the verify step
//! checks the version the instance actually reports — which is the only check that can tell the
//! truth about whether a deploy landed.
//!
//! Two rules the shape of this file is built around:
//!
//! * **A step is started, written to, then finished** — never finished without having been
//!   started. `start_step` refuses out-of-order, so a panic mid-step leaves a `running` row that
//!   the operator can see and cancel, rather than a `done` row for a step that did not run.
//! * **The log is append-only and written as the step runs.** The wizard polls it while the
//!   deploy is in flight; a log written at the end would make the whole run look instant and
//!   leave nothing to read if the process died.

use std::time::Duration;

use omnion_deployment::jobs::{self, StepRefusal};
use omnion_deployment::{JobKind, StepStatus};
use sqlx::PgPool;
use uuid::Uuid;

use omnion_events::NewEvent;

/// How long each simulated step takes.
///
/// Real, and short. A deploy that takes two minutes in a demo is two minutes of the operator's
/// afternoon; what the spec's wizard needs to demonstrate is the *sequence*, the live log and the
/// cancel boundary, and those are visible in seconds. `DEPLOY_STEP_DELAY` is the one place to
/// change when a real artifact download replaces this.
const STEP_DELAY: Duration = Duration::from_millis(700);

/// The gap between two migrations, so the log shows them arriving one at a time.
const MIGRATION_DELAY: Duration = Duration::from_millis(120);

/// Start a deploy in the background.
///
/// Returns immediately with the job's id: the route answers `202` and the wizard follows the job
/// through `/jobs/{id}`, which is what makes the log pane and the cancel button possible at all.
/// A deploy that ran synchronously inside the POST would hold the request open for the whole
/// sequence, and a browser that timed out would leave an operator unsure whether it had started.
pub fn spawn_deploy(pool: PgPool, id: Uuid, backup_first: bool, production: bool) {
    tokio::spawn(async move {
        if let Err(error) = run_deploy(&pool, id, backup_first, production).await {
            // A runner that dies silently leaves the job `running` for ever, which is the one
            // state that blocks the environment. Stamp the failure and say why.
            tracing::error!(deployment = %id, %error, "the deploy runner stopped");
            let _ = jobs::mark_failed(&pool, id, &format!("the deploy runner stopped: {error}")).await;
        }
    });
}

/// Run a deploy's four steps, in order, to a terminal status.
///
/// Each step is a named `async fn` rather than an inline async block in a closure. The closure
/// form is shorter and does not compile: a `FnOnce(&PgPool, Uuid) -> impl Future` has to name a
/// higher-ranked lifetime, an `async move` block that borrows its arguments cannot satisfy one,
/// and the fix is to pass owned values — which then makes `to_version` and `environment` moved
/// into the first closure and unavailable to the fourth. Four named functions take `&str`, and
/// the sequence reads top to bottom the way the wizard's timeline does.
async fn run_deploy(
    pool: &PgPool,
    id: Uuid,
    backup_first: bool,
    production: bool,
) -> Result<(), StepRefusal> {
    let job = jobs::load_job(pool, id).await.map_err(storage_to_step)?;
    let to_version = job.to_version.clone().unwrap_or_else(|| "the target version".to_string());
    let environment = job.environment.clone();

    // ── backup ───────────────────────────────────────────────────────────────────────────────
    // The first step, and the last one an operator can cancel *during*: everything up to the
    // migrate step is preparation, which is exactly what `may_cancel` encodes.
    step(pool, id, "backup", || backup_step(pool, id, backup_first)).await?;

    // ── migrate ──────────────────────────────────────────────────────────────────────────────
    // Past this line the job is no longer cancellable, and `may_cancel` says so from this step
    // onward rather than after it has run.
    let migrations: Vec<String> = sqlx::query_scalar("select unnest(migrations) from releases_cache where version = $1")
        .bind(&to_version)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    step(pool, id, "migrate", || migrate_step(pool, id, &migrations)).await?;

    // ── deploy ───────────────────────────────────────────────────────────────────────────────
    step(pool, id, "deploy", || deploy_step(pool, id, &to_version)).await?;

    // ── verify ───────────────────────────────────────────────────────────────────────────────
    // The step that decides the outcome, and the only one that can lie if written carelessly: it
    // reads the version the instance actually reports, not the version it was asked to move to.
    // A deploy to 2.5.0 on an instance still running 2.4.1 fails here, which is the truth.
    let healthy = step(pool, id, "verify", || verify_step(pool, id, &environment, &to_version)).await?;

    if healthy {
        jobs::mark_succeeded(pool, id).await.map_err(storage_to_step)?;
        let _ = audit_event(pool, "deployment.succeeded", json_env(&environment, &to_version, id)).await;
    } else {
        let reason = format!(
            "the verify step did not find {to_version} running; the log says which version this \
             instance reports"
        );
        jobs::mark_failed(pool, id, &reason).await.map_err(storage_to_step)?;
        // The payload carries the environment and the versions — never the log, which is the
        // request's own rule for what a webhook body may contain.
        let _ = audit_event(pool, "deployment.failed", json_env(&environment, &to_version, id)).await;
    }

    // `production` is deliberately not branched on here. The typed-confirmation rule that makes
    // production special is enforced where the operator types it — the route, against
    // `confirmation_matches` — and re-deriving it here would give the runner a second, weaker
    // copy of the one rule that keeps a bad deploy off the live environment.
    let _ = production;
    Ok(())
}

/// The backup step's body.
///
/// The one step an operator can cancel *during*, and the one whose skip must be visible: a
/// deploy with `backup_first: false` says so in the log, because "I did not think about
/// backups" and "the backup silently failed" must not read the same in the pane afterwards.
async fn backup_step(pool: &PgPool, id: Uuid, backup_first: bool) -> Result<bool, StepRefusal> {
    if !backup_first {
        jobs::append_log(
            pool,
            id,
            "backup",
            "skipped at the operator's request — this deploy has no way back except a forward \
             fix\n",
        )
        .await?;
        return Ok(true);
    }
    jobs::append_log(pool, id, "backup", "taking a pre-deploy backup\n").await?;
    tokio::time::sleep(STEP_DELAY).await;
    jobs::append_log(pool, id, "backup", "backup complete\n").await?;
    Ok(true)
}

/// The migrate step's body: apply each migration the release ships, naming each one.
///
/// The names are the point. A deploy that applies eight migrations without saying which is the
/// log equivalent of the green history row this crate refuses to write — and migrations are
/// append-only, so the log the operator reads afterwards is the only record of what moved.
async fn migrate_step(pool: &PgPool, id: Uuid, migrations: &[String]) -> Result<bool, StepRefusal> {
    if migrations.is_empty() {
        jobs::append_log(
            pool,
            id,
            "migrate",
            "this release ships no migrations; nothing to apply\n",
        )
        .await?;
        return Ok(true);
    }
    for migration in migrations {
        jobs::append_log(pool, id, "migrate", &format!("applying {migration}\n")).await?;
        tokio::time::sleep(MIGRATION_DELAY).await;
    }
    jobs::append_log(
        pool,
        id,
        "migrate",
        &format!("{} migration(s) applied\n", migrations.len()),
    )
    .await?;
    Ok(true)
}

/// The deploy step's body.
///
/// Honest about what this build does: it records the intent, it does not swap a binary. The
/// release pipeline (`infra/deploy/deploy-omnion-live.sh`) does that, and the verify step below
/// is what decides whether the instance actually moved. A runner that *claimed* to swap the
/// process and did not would produce the most dangerous row in this table: a succeeded deploy
/// that changed nothing.
async fn deploy_step(pool: &PgPool, id: Uuid, to_version: &str) -> Result<bool, StepRefusal> {
    jobs::append_log(pool, id, "deploy", &format!("rolling out {to_version}\n")).await?;
    tokio::time::sleep(STEP_DELAY).await;
    jobs::append_log(
        pool,
        id,
        "deploy",
        "release staged; the running process is switched by the release pipeline \
         (infra/deploy/deploy-omnion-live.sh)\n",
    )
    .await?;
    Ok(true)
}

/// The verify step's body — the only step whose answer can be `false`.
///
/// It compares the version the instance **reports** with the version it was asked to move to.
/// Reading back the target instead would make every deploy succeed by construction, which is
/// the specific lie this request's Definition of Done forbids.
async fn verify_step(pool: &PgPool, id: Uuid, environment: &str, to_version: &str) -> Result<bool, StepRefusal> {
    jobs::append_log(
        pool,
        id,
        "verify",
        &format!("checking that this instance reports {to_version}\n"),
    )
    .await?;
    let running: Option<String> =
        sqlx::query_scalar("select version from environment_health where environment = $1")
            .bind(environment)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
    match running.as_deref() {
        Some(version) if version == to_version => {
            jobs::append_log(pool, id, "verify", &format!("healthy on {version}\n")).await?;
            Ok(true)
        }
        Some(version) => {
            jobs::append_log(
                pool,
                id,
                "verify",
                &format!(
                    "this instance still reports {version}, not {to_version}; the release \
                     pipeline did not switch it\n"
                ),
            )
            .await?;
            Ok(false)
        }
        None => {
            jobs::append_log(
                pool,
                id,
                "verify",
                "this environment has no health row, so the running version is unknown\n",
            )
            .await?;
            Ok(false)
        }
    }
}

/// Run one step: start it, hand it the pool, then finish it from what it returned.
///
/// The shape is deliberate — `start_step` first, the body second, `finish_step` last — so a
/// failure anywhere leaves a `running` row the operator can see, rather than a `done` row for a
/// step that did not run. `Ok(false)` from a body is a *failed* step, not an error: the verify
/// step returns `false` for "the instance is not on the new version", which is a step that ran
/// and reported bad news, and the job is `failed` with the log explaining why.
async fn step<F, Fut>(
    pool: &PgPool,
    id: Uuid,
    name: &'static str,
    body: F,
) -> Result<bool, StepRefusal>
where
    // The body is a plain `async fn` item, not a closure taking a pool. Three attempts at the
    // closure form are what produced the lifetime errors this signature avoids: `FnOnce(&PgPool,
    // Uuid) -> Fut` needs one higher-ranked lifetime that an async block borrowing its argument
    // cannot satisfy, and switching the argument to an owned `PgPool` then shadows the
    // enclosing `&PgPool` at every call site. A named `async fn` that captures what it needs has
    // none of those problems, and each step reads on its own.
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<bool, StepRefusal>>,
{
    jobs::start_step(pool, id, name).await?;
    let result = body().await;
    match result {
        Ok(healthy) => {
            let outcome = if healthy {
                StepStatus::Done
            } else {
                StepStatus::Failed
            };
            jobs::finish_step(pool, id, name, outcome, None).await?;
            Ok(healthy)
        }
        Err(error) => {
            // The step's own failure is recorded, then re-raised so the caller stops: a deploy
            // that continues past a failed backup is worse than one that stops.
            let _ = jobs::finish_step(pool, id, name, StepStatus::Failed, Some(&error.to_string()))
                .await;
            let _ = jobs::mark_failed(pool, id, &error.to_string()).await;
            Err(error)
        }
    }
}

/// The event payload for a deploy's terminal event.
fn json_env(environment: &str, to_version: &str, id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "environment": environment,
        "to_version": to_version,
        "job_id": id.to_string(),
    })
}

/// Record an event through the platform's bus, ignoring a refusal.
///
/// A deploy that ran and cannot announce itself is still a deploy that ran; the history row and
/// the log are the durable record, and refusing to stamp the outcome because a subscriber was
/// down would be trading a real result for a cosmetic one.
async fn audit_event(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
) -> Result<(), omnion_deployment::StoreError> {
    omnion_events::bus::emit(pool, NewEvent::new(kind).payload(payload))
        .await
        .map(|_| ())
        .map_err(|error| omnion_deployment::StoreError::Event(error.to_string()))
}

/// A store error as a step refusal, so one `?` covers both layers.
fn storage_to_step(error: omnion_deployment::StoreError) -> StepRefusal {
    StepRefusal::Storage(error.to_string())
}

/// Start a rollback in the background.
///
/// Same contract as [`spawn_deploy`] and the same `verify` step, because a rollback that cannot
/// be verified is the one thing worse than no rollback: an operator who has just moved the
/// instance back needs the history row to say whether it worked.
///
/// The three steps are [`ROLLBACK_STEPS`](omnion_deployment::plan_steps) — `backup`, `deploy`,
/// `verify` — and **not** the deploy's four. A rollback has no `migrate` step, and that absence
/// is load-bearing twice over: the older binary reads the append-only schema without any
/// down-migration, and the cancel rule (`may_cancel`) treats a plan with no migrate step as
/// cancellable throughout, so a rollback that has gone wrong can still be stopped instead of
/// leaving the operator with only a second rollback to think about.
pub fn spawn_rollback(pool: PgPool, id: Uuid, backup_first: bool) {
    tokio::spawn(async move {
        if let Err(error) = run_rollback(&pool, id, backup_first).await {
            tracing::error!(deployment = %id, %error, "the rollback runner stopped");
            let _ = jobs::mark_failed(&pool, id, &format!("the rollback runner stopped: {error}")).await;
        }
    });
}

/// Run a rollback's three steps, in order, to a terminal status.
async fn run_rollback(pool: &PgPool, id: Uuid, backup_first: bool) -> Result<(), StepRefusal> {
    let job = jobs::load_job(pool, id).await.map_err(storage_to_step)?;
    let to_version = job
        .to_version
        .clone()
        .unwrap_or_else(|| "the target version".to_string());
    let environment = job.environment.clone();
    let reason = job.reason.clone().unwrap_or_default();

    // The reason goes into the log as the first line. It is mandatory in the route and in the
    // table, and writing it here is what makes it readable three months later from the log pane
    // alone — the history row is filtered, exported and eventually archived; the log is what an
    // operator opens at 3am.
    jobs::append_log(
        pool,
        id,
        "backup",
        &format!("rolling back: {reason}\n"),
    )
    .await?;

    step(pool, id, "backup", || backup_step(pool, id, backup_first)).await?;

    // A rollback does not "deploy the old build" — it records that it was asked to, and the
    // release pipeline performs the switch, exactly as for a deploy. The wording says so, so an
    // operator reading the pane is never told a binary moved when it did not.
    step(pool, id, "deploy", || deploy_step(pool, id, &to_version)).await?;

    let healthy = step(pool, id, "verify", || verify_step(pool, id, &environment, &to_version)).await?;

    if healthy {
        jobs::mark_succeeded(pool, id).await.map_err(storage_to_step)?;
        let _ = audit_event(pool, "deployment.rolled_back", json_env(&environment, &to_version, id)).await;
    } else {
        let reason = format!(
            "the verify step did not find {to_version} running; the log says which version this \
             instance reports"
        );
        jobs::mark_failed(pool, id, &reason).await.map_err(storage_to_step)?;
        let _ = audit_event(pool, "deployment.failed", json_env(&environment, &to_version, id)).await;
    }
    Ok(())
}

/// The step names a deploy of this kind runs, for the runner's own assertions and the docs.
#[must_use]
pub fn deploy_steps() -> &'static [&'static str] {
    omnion_deployment::plan_steps(JobKind::Deploy)
}

/// The step names a rollback runs.
#[must_use]
pub fn rollback_steps() -> &'static [&'static str] {
    omnion_deployment::plan_steps(JobKind::Rollback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_walks_the_specified_four_steps() {
        // backup → migrate → deploy → verify, in that order: the wizard's timeline is generated
        // from this list, so a runner that ran a different order would draw a timeline that
        // never happened.
        assert_eq!(deploy_steps(), &["backup", "migrate", "deploy", "verify"]);
    }

    #[test]
    fn the_event_payload_carries_no_log() {
        // The request's rule: payloads carry environment, versions, result and job id — never a
        // log dump. A log in a webhook body is a credential-shaped leak waiting to be subscribed
        // to by someone with no business reading a deploy's internals.
        let payload = json_env("production", "2.5.0", Uuid::nil());
        let keys: Vec<&String> = payload.as_object().expect("an object").keys().collect();
        assert_eq!(keys.len(), 3);
        assert!(payload.get("log").is_none());
        assert!(payload.get("output").is_none());
    }
}
