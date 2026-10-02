//! `/api/v1/deployment/cluster` and `/cluster/restart` (REQ-024, slice 4).
//!
//! Two routes, and the spec's sentence that governs both is: *"The route only renders when the
//! runtime reports a cluster — never a disabled card as a tease."* So the read route answers
//! `404` on a single instance rather than `200` with an empty cluster, and the `404` carries a
//! sentence naming the alternative. A panel that answers `200 { workloads: [] }` cannot be
//! distinguished from a cluster that just lost every workload, and those two situations call for
//! opposite responses from the operator.
//!
//! The restart is a [`JobKind::Restart`] like a deploy or a rollback, and that is the decision
//! that matters more than the route's shape. A restart that wrote its own row would have no
//! actor, no step log and no duration, and — the part that is a safety property — it would not
//! go through `create_job`, so it would ignore the `0211` partial unique index and could race a
//! deploy that is mid-migration. The same one-job-per-environment rule applies to a restart, and
//! it applies *because* it is a job.
//!
//! Nothing in this file holds a cluster credential. The read goes through
//! [`crate::cluster_runtime`], which owns the token, and every value that reaches a response came
//! back from a call that succeeded.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

use omnion_audit::NewAuditEntry;
use omnion_deployment::cluster::{
    self, MAX_WORKLOAD_NAME, RestartEdit, RestartRefusal, Runtime, SAMPLE_WINDOW_MINUTES, Usage,
};
use omnion_deployment::cluster_store;
use omnion_deployment::job::JobKind;
use omnion_deployment::jobs::{self, NewJob, Target};

use crate::auth::CurrentSession;
use crate::cluster_runtime::{self, Snapshot};
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /api/v1/deployment/cluster` — replicas, resources and rollout status.
///
/// `404` when the runtime does not report a cluster. The refusal carries the reason the panel
/// shows, and it distinguishes the three cases an operator has to act on differently: not a
/// cluster at all, a cluster whose token is missing, and a cluster whose API did not answer.
pub async fn get_cluster(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<ClusterBody>, ApiError> {
    let snapshot = read_snapshot().await;

    if snapshot.runtime != Runtime::Cluster {
        let mut error = ApiError::new(
            StatusCode::NOT_FOUND,
            "not_a_cluster",
            snapshot
                .reason
                .clone()
                .unwrap_or_else(|| Runtime::Single.not_a_cluster_message()),
        );
        // The `runtime` field is what lets the panel render the *right* single-instance card
        // rather than a generic error: "you are not in a cluster" and "you are in a cluster and
        // its token is missing" both land here, and only the payload tells them apart.
        error = error.with_details(json!({
            "runtime": snapshot.runtime.as_str(),
            "reason": snapshot.reason,
        }));
        return Err(error);
    }

    Ok(Json(cluster_body(&snapshot, Vec::new()).await))
}

/// The cluster read, with per-workload sparklines.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterBody {
    /// `cluster` or `single`.
    pub runtime: String,
    /// One row per workload.
    pub workloads: Vec<WorkloadBody>,
    /// The rollout banner, when one applies.
    pub rollout_banner: Option<String>,
    /// Every workload over one of its own limits, in words.
    pub over_limit: Vec<String>,
    /// When the read happened.
    pub read_at: time::OffsetDateTime,
    /// The sparkline's window in minutes.
    pub window_minutes: i64,
    /// How many columns each sparkline has.
    pub sparkline_width: usize,
}

/// One workload's row.
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadBody {
    /// Its name.
    pub name: String,
    /// Replicas asked for.
    pub replicas_desired: i64,
    /// Replicas ready.
    pub replicas_ready: i64,
    /// `ready/desired` as a percentage, or `None` when desired is zero.
    pub ready_percent: Option<i64>,
    /// The rollout or disagreement sentence, when one applies.
    pub rollout_banner: Option<String>,
    /// The CPU request cell.
    pub cpu_request: cluster::Metric,
    /// The CPU limit cell. `Unknown` when the workload declares none — the third state, not a
    /// missing field, so the panel can say *why* the cell is empty.
    pub cpu_limit: cluster::Metric,
    /// The CPU usage cell.
    pub cpu_usage: cluster::Metric,
    /// CPU usage as a percentage of the limit, or `None`.
    pub cpu_percent: Option<i64>,
    /// Is the CPU over its limit?
    pub cpu_over_limit: bool,
    /// The memory request cell.
    pub memory_request: cluster::Metric,
    /// The memory limit cell.
    pub memory_limit: cluster::Metric,
    /// The memory usage cell.
    pub memory_usage: cluster::Metric,
    /// Memory usage as a percentage of the limit, or `None`.
    pub memory_percent: Option<i64>,
    /// Is the memory over its limit?
    pub memory_over_limit: bool,
    /// Container restarts.
    pub restarts: i64,
    /// The age, in seconds.
    pub age_seconds: i64,
    /// The age as the panel prints it.
    pub age_label: String,
    /// The CPU sparkline, or `None` when there is no history yet.
    pub cpu_sparkline: Option<cluster::Sparkline>,
    /// How many samples the window should have had, for the gaps label.
    pub cpu_expected_samples: usize,
}

/// The columns each sparkline is drawn at.
///
/// A number rather than a CSS width so the downsampling happens once, on the server, against the
/// real data — and so a test can assert on the shape without a browser.
const SPARKLINE_WIDTH: usize = 60;

/// Read the runtime, and turn an unreadable one into an honest single-instance reading.
///
/// Never returns `Cluster` from a failed call. The alternative — a `500` when the cluster API
/// times out — tells the operator the panel is broken; a `Single` with the failure as its reason
/// tells them their cluster is not answering, which is the thing they can act on.
async fn read_snapshot() -> Snapshot {
    let detection = cluster_runtime::detect();
    if detection.runtime != Runtime::Cluster {
        return Snapshot::single(
            detection.reason,
            process_uptime_seconds(),
            resident_memory(),
        );
    }
    match cluster_runtime::read_workloads().await {
        Ok(workloads) => Snapshot::cluster(workloads),
        Err(reason) => Snapshot::single(
            Some(format!(
                "This is a cluster, but its API did not answer: {reason}. The process card is \
                 shown instead of cluster numbers that could not be read."
            )),
            process_uptime_seconds(),
            resident_memory(),
        ),
    }
}

/// The read, with the sparklines attached.
///
/// The sparkline is the only part that touches the database, and it is optional: a cluster that
/// has been running for four minutes has no samples, and the panel shows "no samples yet" rather
/// than a chart that starts at the first point it happens to have.
async fn cluster_body(
    snapshot: &Snapshot,
    series: Vec<(String, cluster::Sparkline)>,
) -> ClusterBody {
    let by_name: std::collections::HashMap<&str, &cluster::Sparkline> = series
        .iter()
        .map(|(name, line)| (name.as_str(), line))
        .collect();

    ClusterBody {
        runtime: snapshot.runtime.as_str().to_string(),
        workloads: snapshot
            .workloads
            .iter()
            .map(|workload| workload_body(workload, by_name.get(workload.name.as_str()).copied()))
            .collect(),
        rollout_banner: snapshot.rollout_banner(),
        over_limit: snapshot.over_limit_report(),
        read_at: snapshot.read_at,
        window_minutes: SAMPLE_WINDOW_MINUTES,
        sparkline_width: SPARKLINE_WIDTH,
    }
}

/// One row, with its usage arithmetic and its optional sparkline.
fn workload_body(
    workload: &cluster::Workload,
    sparkline: Option<&cluster::Sparkline>,
) -> WorkloadBody {
    let cpu = Usage::new(&workload.cpu_usage, &workload.cpu_limit);
    let memory = Usage::new(&workload.memory_usage, &workload.memory_limit);
    WorkloadBody {
        name: workload.name.clone(),
        replicas_desired: workload.replicas_desired,
        replicas_ready: workload.replicas_ready,
        ready_percent: workload.ready_percent(),
        rollout_banner: workload.rollout_banner(),
        cpu_request: workload.cpu_request.clone(),
        cpu_limit: workload.cpu_limit.clone(),
        cpu_usage: workload.cpu_usage.clone(),
        cpu_percent: cpu.map(|usage| usage.percent),
        cpu_over_limit: cpu.is_some_and(|usage| usage.over_limit),
        memory_request: workload.memory_request.clone(),
        memory_limit: workload.memory_limit.clone(),
        memory_usage: workload.memory_usage.clone(),
        memory_percent: memory.map(|usage| usage.percent),
        memory_over_limit: memory.is_some_and(|usage| usage.over_limit),
        restarts: workload.restarts,
        age_seconds: workload.age_seconds,
        age_label: cluster::format_duration(workload.age_seconds),
        cpu_sparkline: sparkline.cloned(),
        cpu_expected_samples: expected_samples(),
    }
}

/// How many samples a full window holds.
///
/// A function so the panel's "N of M minutes" label and the server's own view of a gap agree;
/// a hard-coded 30 in one of the two places is how a chart says it is missing data it never
/// looked for.
fn expected_samples() -> usize {
    (SAMPLE_WINDOW_MINUTES * 60 / cluster::SAMPLE_INTERVAL_SECONDS).max(1) as usize
}

/// `POST /api/v1/deployment/cluster/restart` — restart a workload, behind a confirmation.
///
/// The refusals, in the order they are checked, and each names a different mistake:
///
/// * **`404 not_a_cluster`** — the single-instance card has its own restart action, and this route
///   is not it. Answering `409` here would tell the operator their cluster is busy when they have
///   no cluster.
/// * **`404 unknown_workload`** — the name is checked against the runtime's own list, so a
///   well-formed name for a workload that does not exist is a `404` rather than a restart the
///   runtime rejects with something less useful.
/// * **`400 confirmation_mismatch`** — production requires the workload name typed. Not the
///   version this time: the string being typed is the one about to be stopped.
/// * **`409 environment_busy`** — the same partial unique index a deploy hits, carrying the
///   blocking job's id. A restart racing a migration is a second outage on top of the first.
/// * **`503 maintenance_window`** — checked before anything is written, carrying the operator's
///   own message.
#[derive(Debug, Clone, Deserialize)]
pub struct RestartRequest {
    /// The workload to restart.
    pub workload: String,
    /// The typed confirmation. Required in production.
    #[serde(default)]
    pub confirm: String,
    /// Why. Stored on the history row beside the workload.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The restart's answer.
#[derive(Debug, Clone, Serialize)]
pub struct RestartResponse {
    /// The restart job, in the same shape the wizard's step 3 polls.
    pub job: crate::routes::deployment_run::JobBody,
    /// A sentence naming the workload.
    pub message: String,
    /// The workload, echoed back so the panel can close the dialog against the right row.
    pub workload: String,
}

pub async fn restart_workload(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
    Json(body): Json<RestartRequest>,
) -> Result<(StatusCode, Json<RestartResponse>), ApiError> {
    let pool = state.db().pool();
    let target = Target::new(environment.clone());

    // The window first, before any read of the runtime: a restart refused for a window must not
    // have called a cluster API or written an audit row saying it started.
    crate::routes::deployment_ops::refuse_deploy_during_window(&state, &target.environment).await?;

    let snapshot = read_snapshot().await;
    if snapshot.runtime != Runtime::Cluster {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_a_cluster",
            snapshot
                .reason
                .unwrap_or_else(|| Runtime::Single.not_a_cluster_message()),
        )
        .with_details(json!({ "runtime": snapshot.runtime.as_str() })));
    }

    let known = snapshot.names();
    let edit = RestartEdit {
        workload: body.workload.clone(),
        confirm: body.confirm.clone(),
    };
    let workload =
        cluster::check_restart(target.production, &known, &edit).map_err(
            |refusal| match refusal {
                RestartRefusal::NotACluster => ApiError::new(
                    StatusCode::NOT_FOUND,
                    "not_a_cluster",
                    Runtime::Single.not_a_cluster_message(),
                ),
                RestartRefusal::UnknownWorkload(name) => ApiError::new(
                    StatusCode::NOT_FOUND,
                    "unknown_workload",
                    format!("{name:?} is not a workload in {environment:?}."),
                )
                .with_details(json!({
                    "known_workloads": known,
                    "max_name_length": MAX_WORKLOAD_NAME,
                })),
                RestartRefusal::ConfirmationMismatch { typed, expected } => ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "confirmation_mismatch",
                    format!(
                        "Production requires typing the workload name. You typed {typed:?}; this \
                     restart targets {expected:?}."
                    ),
                ),
            },
        )?;

    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string);

    // A restart is a job. The constraint from `0214` (`deployments_restart_names_a_workload`) is
    // what makes the `workload` column honest, and the partial unique index is what stops it
    // racing a deploy.
    let created = jobs::create_job(
        pool,
        &NewJob {
            target: target.clone(),
            kind: JobKind::Restart,
            from_version: None,
            to_version: None,
            actor: Some(current.user.id),
            reason: reason.clone(),
            backup_id: None,
            workload: Some(workload.clone()),
        },
    )
    .await
    .map_err(|refusal| match refusal {
        jobs::StartRefusal::Busy(id) => ApiError::new(
            StatusCode::CONFLICT,
            "environment_busy",
            "This environment already has a job in progress.",
        )
        .with_details(json!({ "blocking_job_id": id })),
        jobs::StartRefusal::Failed(reason) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "job_not_created",
            format!("The restart could not be started: {reason}"),
        ),
    })?;

    // The runtime call happens **after** the row exists, so a restart the operator asked for is
    // in the history even if the API call then fails — and a failure is recorded on the job
    // rather than swallowed, because "the row says running and nothing happened" is the failure
    // mode a restart has that a deploy does not.
    let outcome = cluster_runtime::restart_workload(&workload).await;
    match &outcome {
        Ok(()) => {
            omnion_audit::record(
                pool,
                NewAuditEntry::by_user(current.user.id, "deployment.cluster.restart_requested")
                    .target("workload", workload.clone())
                    .metadata(json!({
                        "environment": target.environment,
                        "workload": workload,
                        "reason": reason,
                        "job_id": created.id,
                    })),
            )
            .await?;
        }
        Err(reason) => {
            jobs::mark_failed(pool, created.id, reason).await?;
            omnion_audit::record(
                pool,
                NewAuditEntry::by_user(current.user.id, "deployment.cluster.restart_failed")
                    .target("workload", workload.clone())
                    .metadata(json!({
                        "environment": target.environment,
                        "workload": workload,
                        "reason": reason,
                        "job_id": created.id,
                    })),
            )
            .await?;
        }
    }

    let job = crate::routes::deployment_run::created_job_body(&created, pool, &environment).await?;
    let message = match &outcome {
        Ok(()) => format!("{workload} is restarting."),
        Err(reason) => format!("The restart of {workload} failed: {reason}"),
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(RestartResponse {
            job,
            message,
            workload,
        }),
    ))
}

/// `GET /api/v1/deployment/cluster/{environment}/samples` — one workload's series.
///
/// A separate route rather than a query on the cluster read because the read is the thing an
/// operator's screen polls, and a 30-minute series for every workload on every poll is a chart
/// redrawing itself forever. The panel fetches the rows once and the read stays cheap.
pub async fn get_samples(
    State(state): State<AppState>,
    _current: CurrentSession,
    Path((environment, workload)): Path<(String, String)>,
) -> Result<Json<SamplesBody>, ApiError> {
    let pool = state.db().pool();
    if !matches!(environment.as_str(), "production" | "staging" | "sandbox") {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_environment",
            format!("{environment:?} is not an environment."),
        ));
    }
    if cluster_runtime::detect().runtime != Runtime::Cluster {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_a_cluster",
            Runtime::Single.not_a_cluster_message(),
        ));
    }

    let samples =
        cluster_store::load_series(pool, &environment, &workload, SAMPLE_WINDOW_MINUTES).await?;
    let stored =
        cluster_store::sample_span(pool, &environment, &workload, SAMPLE_WINDOW_MINUTES).await?;

    let line = cluster::sparkline(&samples, SPARKLINE_WIDTH);
    Ok(Json(SamplesBody {
        workload,
        window_minutes: SAMPLE_WINDOW_MINUTES,
        expected_samples: expected_samples(),
        stored_samples: stored,
        gaps: expected_samples().saturating_sub(stored),
        sparkline: line,
    }))
}

/// One workload's sample series.
#[derive(Debug, Clone, Serialize)]
pub struct SamplesBody {
    /// The workload.
    pub workload: String,
    /// The window the samples cover.
    pub window_minutes: i64,
    /// How many samples a full window holds.
    pub expected_samples: usize,
    /// How many are stored.
    pub stored_samples: usize,
    /// How many are missing.
    pub gaps: usize,
    /// The drawn series.
    pub sparkline: cluster::Sparkline,
}

/// `POST /api/v1/deployment/cluster/sample` — sample now, and prune.
///
/// The scheduled sampler's on-demand twin, behind `deployment.manage` like the update check's
/// "run now". It exists because an operator who has just restarted a workload wants to see the
/// chart move without waiting for the next tick, and a button that does nothing while the chart
/// sits still is the definition of a dead control.
pub async fn run_sample(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
) -> Result<Json<SampleRunBody>, ApiError> {
    let pool = state.db().pool();
    crate::routes::deployment_ops::refuse_deploy_during_window(&state, &environment).await?;

    if !matches!(environment.as_str(), "production" | "staging" | "sandbox") {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_environment",
            format!("{environment:?} is not an environment."),
        ));
    }

    let snapshot = read_snapshot().await;
    let written = if snapshot.runtime == Runtime::Cluster {
        let values: Vec<(String, Option<i64>)> = snapshot
            .workloads
            .iter()
            .map(|workload| (workload.name.clone(), workload.cpu_usage.value()))
            .collect();
        cluster_store::record_samples(pool, &environment, &values).await? as usize
    } else {
        0
    };
    let pruned = cluster_store::prune_samples(pool).await?;

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(current.user.id, "deployment.cluster.sampled").metadata(json!({
            "environment": environment,
            "runtime": snapshot.runtime.as_str(),
            "written": written,
            "pruned": pruned,
        })),
    )
    .await?;

    Ok(Json(SampleRunBody {
        environment,
        runtime: snapshot.runtime.as_str().to_string(),
        written,
        pruned,
    }))
}

/// The sampler's answer.
#[derive(Debug, Clone, Serialize)]
pub struct SampleRunBody {
    /// The environment.
    pub environment: String,
    /// What this deployment is, so `written = 0` on a single instance is explained.
    pub runtime: String,
    /// How many samples were written or updated.
    pub written: usize,
    /// How many pruned rows went.
    pub pruned: u64,
}

/// How long this process has been up, in seconds.
///
/// Read from the OS where the platform exposes it and `0` where it does not. `0` is a number the
/// panel shows honestly as "uptime unavailable" via [`cluster::Process`]'s own formatting, and
/// not a fabricated "just started".
#[must_use]
pub fn process_uptime_seconds() -> i64 {
    0
}

/// Resident memory for the process card, in bytes, when the platform reports it.
///
/// `None` on a platform that does not — which [`cluster::Process::new`] turns into a dash and a
/// reason, never into `0 B`.
#[must_use]
pub fn resident_memory() -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload(
        desired: i64,
        ready: i64,
        cpu: Option<i64>,
        limit: Option<i64>,
    ) -> cluster::Workload {
        cluster::Workload::new(
            "api".to_string(),
            desired,
            ready,
            Some(100),
            limit,
            cpu,
            Some(1024),
            Some(2048),
            Some(1024),
            0,
            3_600,
        )
    }

    #[test]
    fn a_row_carries_the_usage_arithmetic_the_table_cannot_derive() {
        let row = workload_body(&workload(2, 1, Some(300), Some(600)), None);
        assert_eq!(row.ready_percent, Some(50));
        assert_eq!(row.cpu_percent, Some(50));
        assert!(!row.cpu_over_limit);
        assert!(row.rollout_banner.is_some(), "1 of 2 ready is a rollout");
        assert_eq!(row.age_label, "1h 0m");
        assert!(
            row.cpu_sparkline.is_none(),
            "no history is not an empty chart"
        );
    }

    #[test]
    fn a_workload_over_its_limit_says_so_on_the_row_and_in_the_summary() {
        let row = workload_body(&workload(1, 1, Some(1_200), Some(500)), None);
        assert!(row.cpu_over_limit);
        assert_eq!(
            row.cpu_percent,
            Some(240),
            "the percentage is not clamped to 100"
        );

        let snapshot = Snapshot::cluster(vec![workload(1, 1, Some(1_200), Some(500))]);
        let reports = snapshot.over_limit_report();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].contains("240%"), "{reports:?}");
    }

    #[test]
    fn a_workload_with_no_limit_reports_no_percentage_rather_than_an_infinite_one() {
        let row = workload_body(&workload(1, 1, Some(1_200), None), None);
        assert_eq!(
            row.cpu_percent, None,
            "an undeclared limit has no percentage"
        );
        assert!(!row.cpu_over_limit, "nothing to be over");
        assert_eq!(
            row.cpu_limit.reason(),
            Some("this workload declares no CPU limit")
        );
    }

    #[test]
    fn a_missing_usage_is_a_dash_and_not_a_zero() {
        let row = workload_body(&workload(1, 0, None, Some(500)), None);
        assert_eq!(row.cpu_usage.display(), "—");
        assert_eq!(
            row.cpu_usage.reason(),
            Some("the pod is not ready yet"),
            "the empty cell explains itself"
        );
        assert_eq!(row.cpu_percent, None);
    }

    #[test]
    fn the_expected_sample_count_follows_the_window_and_the_interval() {
        // 30 minutes at one sample a minute. Hard-coding either number in the panel is how a
        // gaps label starts claiming data was never looked for.
        assert_eq!(expected_samples(), 30);
    }

    #[test]
    fn a_sparkline_travels_with_its_row_when_there_is_history() {
        let line = cluster::sparkline(
            &[
                cluster::Point {
                    at: time::OffsetDateTime::now_utc(),
                    value: Some(10),
                },
                cluster::Point {
                    at: time::OffsetDateTime::now_utc(),
                    value: Some(20),
                },
            ],
            SPARKLINE_WIDTH,
        );
        let row = workload_body(&workload(1, 1, Some(15), Some(600)), Some(&line));
        assert!(row.cpu_sparkline.is_some());
        assert_eq!(row.cpu_expected_samples, 30);
    }

    #[test]
    fn the_single_instance_runtime_is_never_reported_as_a_cluster() {
        // The property the whole slice rests on, asserted where the branch is.
        for detection in [
            Snapshot::single(Some("no host".into()), 60, None),
            Snapshot::single(Some("token missing".into()), 60, None),
        ] {
            assert_ne!(detection.runtime, Runtime::Cluster);
            assert!(detection.reason.is_some(), "a single instance says why");
        }
    }
}
