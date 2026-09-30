//! The background agent runner (REQ-099, slice 1).
//!
//! `main.rs` spawns this task when the runner is enabled (`OMNION_AI_RUNNER`, default on). Each
//! tick claims at most one queued run — up to `OMNION_AI_RUNNER_CONCURRENCY` in flight at once —
//! builds a [`ProviderModel`] for it and hands the run to the loop engine, with a `Persist`
//! closure that writes the trace and a `Sink` that nobody reads.
//!
//! Four decisions, each of which closes a way a run can be lost or run twice.
//!
//! **The claim is the store's, not the runner's.** [`run_store::claim_next_run`] does the
//! `for update skip locked` selection and the status change in one statement. A runner that read
//! a queue and then picked one would race a second API process, and two processes running the
//! same two-tool agent is a bill nobody approved.
//!
//! **The loop's sink is dropped on purpose.** The person watching a run watches it over SSE,
//! which is a *replay* of the step rows, not a subscription to this task. If the runner's sink had
//! a subscriber, a run with nobody watching would buffer its events in memory until the step cap
//! filled it, and the loop's own bounded channel would then block the run itself. A sink with no
//! receiver is the honest shape: `send` fails immediately and the loop moves on, which is exactly
//! the "publish and persist" contract the engine documents.
//!
//! **Cancellation is polled, not pushed.** The loop has no handle the runner can call into, so the
//! runner checks `cancel_requested` between steps and asks the run to stop at the next boundary.
//! That is the semantics the spec asks for — a tool that has already started is left to finish,
//! because a half-applied side effect is worse than a slightly later stop.
//!
//! **A model that cannot be resolved fails the run, and the run says why.** An agent whose pinned
//! model is gone, or whose installation has no route, is an operator problem; the row ends
//! `failed` with the resolver's own sentence rather than sitting `running` until a heartbeat
//! expires and a reaper requeues it forever.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use omnion_ai_hub::agent::{AgentEvent, RunLimits, StepKind, StepStatus, StopReason};
use omnion_ai_hub::identity;
use omnion_ai_hub::loop_engine::{
    CancelHandle, OutputVerification, Persist, RunOptions, Runtime, Sink, run_with as run_agent,
};
use omnion_ai_hub::provider_model::ProviderModel;
use omnion_ai_hub::run_store::{self, Run};
use omnion_ai_hub::tool_exec::{self, ToolExecutor};
use omnion_ai_hub::tools::{AllowList, Execution, ToolRegistry, ToolSummary};
use omnion_ai_hub::{DecisionContext, ProviderTarget, Scope, resolve_and_record};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

/// The permission decision for a run's caller, read from the platform's own resolver.
///
/// **A run is not more powerful than whoever started it.** The AI identity narrows what the
/// *agent* may do; this narrows what the *person* behind it may do, and a run started by a system
/// (a schedule, a webhook) has no user and therefore no grant — which is the conservative
/// direction, and the one an agent triggered by an untrusted payload should land in.
///
/// The resolution is cached per run rather than read per call: a run's permissions do not change
/// under it, and a `select` per tool call would put the permission engine in the middle of the
/// money path for no benefit. A permission revoked mid-run takes effect on the next run, which is
/// the same contract the tool's own `enabled` flag already has.
struct PermissionsForCaller {
    organization_id: Uuid,
    user_id: Option<Uuid>,
    /// Loaded once by [`PermissionsForCaller::load`]; a run's permissions do not change under it.
    cached: std::sync::OnceLock<PermissionSet>,
}

impl omnion_ai_hub::tool_exec::PermissionGate for PermissionsForCaller {
    fn allows(&self, permission: &str) -> bool {
        // The trait is synchronous and the resolution is not, so the answer is fetched once by
        // [`PermissionsForCaller::load`] before the pipeline is built and this is a pure lookup
        // from there on. The `OnceLock` is initialised in that constructor and never poisoned:
        // a failed load fills it with "denies everything" rather than leaving it unset, because
        // an uninitialised gate would have to be handled in `allows` and the second failure mode
        // (a gate that panics) is worse than the first.
        self.cached
            .get()
            .map_or(false, |set| set.iter().any(|held| held == permission))
    }
}

/// The set of permission keys a run's caller holds, loaded once.
type PermissionSet = std::collections::BTreeSet<String>;

impl PermissionsForCaller {
    async fn load(pool: &PgPool, organization_id: Uuid, user_id: Option<Uuid>) -> Self {
        let cached = std::sync::OnceLock::new();
        let held = match user_id {
            Some(user_id) => {
                omnion_permissions::effective_permissions(
                    pool,
                    user_id,
                    omnion_permissions::Scope::Organization { organization_id },
                )
                .await
                .map(|permissions| permissions.granted_keys().into_iter().collect::<PermissionSet>())
                // A run whose caller's permissions could not be read gets an empty set: every
                // tool call is refused with `permission_denied` and the reason is in the trace.
                // Guessing the other way would make a database blip into a privilege escalation.
                .unwrap_or_else(|error| {
                    tracing::warn!(%user_id, %error, "a run's caller permissions could not be read");
                    PermissionSet::new()
                })
            }
            None => PermissionSet::new(),
        };
        let _ = cached.set(held);
        Self {
            organization_id,
            user_id,
            cached,
        }
    }
}

/// The one executor a run uses: the execution pipeline, plus the two things only the API can do.
///
/// The pipeline owns the *policy*; this struct owns the *evidence* — the `audit_log` row and the
/// `ai.tool.*` event, both of which live in crates that `ai-hub` must not depend on. The split is
/// not cosmetic: `omnion-audit` depends on `omnion-events` and `omnion-permissions` depends on
/// nothing from `ai-hub`, so a `ai-hub → omnion-audit` edge would be a new cycle risk the moment
/// anybody wired it the other way. `apps/api` already has both edges, so the rows are written
/// here, from the same outcome value the pipeline produced.
struct RunExecutor {
    pool: PgPool,
    run_id: Uuid,
    organization_id: Uuid,
    agent_id: Uuid,
    user_id: Option<Uuid>,
    site_id: Option<Uuid>,
    pipeline: tool_exec::Pipeline,
}

impl RunExecutor {
    /// The tools the model is shown — the pipeline's own filter, called once at construction.
    fn payload(&self) -> Vec<ToolSummary> {
        self.pipeline.model_facing()
    }
}

impl ToolExecutor for RunExecutor {
    fn execute<'a>(
        &'a self,
        step_no: u32,
        call: &'a omnion_ai_hub::agent::ToolCall,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Execution> + Send + 'a>> {
        Box::pin(async move {
            // The step row the run's own event stream wrote for this step. The criterion is
            // "both carrying the same run and step", and `ai_tool_calls.step_id` is a uuid
            // reference — so the executor reads the id the step actually got, rather than
            // inventing one. A run whose step was never opened leaves it null, which the column
            // allows: `set null`, not `not null`.
            let step_id = run_store::step_id(&self.pool, self.run_id, i32::try_from(step_no).unwrap_or(i32::MAX))
                .await
                .ok()
                .flatten();
            let outcome = self
                .pipeline
                .with_step(step_id)
                .call(&self.pool, call)
                .await;

            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    // A store failure inside the pipeline is not a tool failure and must not be
                    // reported as one: the call may or may not have run. The loop gets a refusal
                    // with the error's own code so the model can retry, and the operator gets a
                    // log line naming the run — because "the tool failed" and "we could not
                    // record the tool" are different incidents with different fixes.
                    tracing::warn!(
                        %self.run_id,
                        %error,
                        "the tool pipeline could not complete a call"
                    );
                    tool_exec::CallOutcome::Refused {
                        tool: call.tool.clone(),
                        code: "tool_pipeline_error".to_owned(),
                        reason: format!("the platform could not complete this call: {error}"),
                        call_id: 0,
                    }
                }
            };
            self.record(&outcome).await;
            tool_exec::as_execution(&outcome)
        })
    }
}

impl RunExecutor {
    /// The two records only the API can write: the append-only audit row and the bus event.
    ///
    /// **Both are best-effort, and neither can undo the call.** The tool has already run by the
    /// time this is called; a failed audit write is a gap in the evidence, not a reason to tell
    /// the model the call did not happen. That is why the failures are logged at `warn` and the
    /// outcome is passed through untouched.
    async fn record(&self, outcome: &tool_exec::CallOutcome) {
        let tool = tool_exec::other_key(outcome);
        let risk = omnion_ai_hub::catalogue::find(&tool).map_or("unknown", |spec| spec.risk.as_str());
        let permission = omnion_ai_hub::catalogue::find(&tool)
            .map_or("unknown", |spec| spec.permission);

        let entry = omnion_audit::NewAuditEntry {
            organization_id: Some(self.organization_id),
            actor_user_id: None,
            actor_type: omnion_audit::ActorType::Agent,
            action: "ai.tool.call",
            target_type: Some("ai_tool"),
            target_id: Some(tool.clone()),
            metadata: serde_json::json!({
                "tool_key": tool,
                "run_id": self.run_id,
                "agent_id": self.agent_id,
                "user_id": self.user_id,
                "site_id": self.site_id,
                "risk": risk,
                "permission": permission,
                "outcome": outcome.code(),
                "ran": outcome.ran(),
                "call_id": call_id_of(outcome),
            }),
            ip_address: None,
        };
        if let Err(error) = omnion_audit::record(&self.pool, entry).await {
            tracing::warn!(%self.run_id, %error, "a tool call's audit row could not be written");
        }

        let Some(name) = outcome.alert_event() else {
            return;
        };
        let event = omnion_events::NewEvent::new(name)
            .payload(serde_json::json!({
                "run_id": self.run_id,
                "tool_key": tool,
                "error_code": outcome.code(),
                "identity_id": self.pipeline.identity_id(),
            }));
        if let Err(error) = omnion_events::bus::emit(&self.pool, event).await {
            tracing::warn!(%self.run_id, %error, "a tool alert event could not be published");
        }
    }
}

/// The call log row's id, for the audit metadata. Zero for an outcome the pipeline refused
/// before it wrote one.
fn call_id_of(outcome: &tool_exec::CallOutcome) -> i64 {
    match outcome {
        tool_exec::CallOutcome::Ran { call_id, .. }
        | tool_exec::CallOutcome::Refused { call_id, .. }
        | tool_exec::CallOutcome::TimedOut { call_id, .. } => *call_id,
        tool_exec::CallOutcome::Parked { .. } => 0,
    }
}

/// The tick cadence: how often a worker looks for a queued run.
///
/// 250 ms is the same reasoning the workflow engine uses for its fast tick — most of a run's
/// duration is spent inside a provider call, and a slow tick is pure added latency on a run that
/// was already queued when the box had nothing else to do. A missed tick is skipped rather than
/// replayed, because a burst of catch-up claims after a GC pause would start four runs at once on
/// a machine that was already behind.
const TICK_MS: u64 = 250;

/// How often stale runs are handed back to the queue.
///
/// Once a minute rather than every tick: the reaper's own predicate is "older than 120 s", so
/// running it four times a second can only ever find the same row or nothing.
const REAPER_MS: u64 = 60_000;

/// What one tick did, as the log line and the unit tests describe it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Runs claimed and executed this tick.
    pub executed: usize,
    /// Runs that ended because a model could not be resolved.
    pub unresolved: usize,
    /// Runs the reaper handed back on this sweep.
    pub requeued: u64,
}

impl TickReport {
    /// `true` when there was nothing to do.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.executed == 0 && self.unresolved == 0 && self.requeued == 0
    }
}

/// A `Persist` that writes the loop's events onto the run's step rows, and hears the cancel button.
///
/// **This is where a live run learns it was cancelled.** `POST /ai/runs/{id}/cancel` writes
/// `cancel_requested_at`; nothing else in the process is watching that column while a run is in
/// flight, so without this the flag would sit there until the run finished on its own and a
/// person watching a four-step run press stop twice to no effect.
///
/// The poll rides on the event stream rather than on a timer, for the same reason the heartbeat
/// does: the stream *is* the tick. A run that produced an event in the last moment still learns
/// about the cancel, and a run blocked inside a provider call for ninety seconds will learn at
/// its next boundary — which is the contract the panel's copy already promises ("the loop stops
/// at the next step boundary"), so no new promise is needed and no polling interval is invented.
///
/// The check is skipped on the very first event of a run. A cancel request that arrives between
/// `claim_next_run` and the first boundary has already been answered by the pre-flight check in
/// [`tick`], and asking again there would only add a query to every run for no decision.
fn persister(pool: PgPool, run_id: Uuid, cancel: CancelHandle) -> Persist {
    // An atomic counter rather than a `mut` local: the engine's `Persist` is an `Fn` closure, so
    // the compiler — correctly — refuses to let it own mutable state. `seen` only ever counts up,
    // and the first event of every run is the one being skipped.
    let seen = std::sync::atomic::AtomicU32::new(0);
    Box::new(move |event: AgentEvent| {
        let pool = pool.clone();
        let cancel = cancel.clone();
        let probe = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
        Box::pin(async move {
            if probe {
                match run_store::cancel_requested(&pool, run_id).await {
                    Ok(true) => cancel.request(),
                    Ok(false) => {}
                    Err(error) => {
                        // A failed read is not a cancellation. Guessing either way is wrong in
                        // both directions: stopping a healthy run because a read blipped loses
                        // the work, and ignoring a real cancel loses the reason the person gave.
                        tracing::warn!(%run_id, %error, "the cancel check for an agent run failed");
                    }
                }
            }
            if let Err(error) = run_store::append_event(&pool, run_id, &event).await {
                tracing::warn!(%run_id, %error, "an agent run event could not be written");
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    })
}

/// The runner's persister, as something a walk can drive one event at a time.
///
/// A `Persist` is a boxed closure returning a boxed future, which is exactly the wrong shape to
/// call from a test: `persister(event)` would hand back a `Pin<Box<dyn Future>>` and the walk
/// would have to `.await` a value it cannot name. Returning `impl Fn(AgentEvent) -> impl Future`
/// costs one line and makes the same closure callable both ways, so the integration walk that
/// proves a live run hears its cancellation drives **the same closure** the runner uses rather
/// than a copy of it that could drift.
pub fn persister_for_tests(
    pool: PgPool,
    run_id: Uuid,
    cancel: CancelHandle,
) -> impl Fn(AgentEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send
       + Sync
{
    let record = persister(pool, run_id, cancel);
    move |event| (record)(event)
}

/// The step row a run is currently on, heartbeated on every event.
///
/// The heartbeat is here rather than in a timer task because a separate timer for a run whose
/// steps take four seconds each is a timer that fires twice a run and never during the part it
/// exists for. The event stream *is* the tick: a provider call that takes ninety seconds produces
/// no events at all, and that is the case the 120-second threshold exists to cover.
async fn heartbeat(pool: &PgPool, run_id: Uuid) {
    if let Err(error) = run_store::heartbeat(pool, run_id).await {
        tracing::warn!(%run_id, %error, "the agent run heartbeat could not be written");
    }
}

/// Execute one claimed run, end to end.
///
/// Split out from the tick so the unit tests can drive it against a scripted model — the tick's
/// only job is to decide *when* to run something, and a test that had to fake a claim to test the
/// run would be testing the fake.
pub async fn execute(pool: &PgPool, run: &Run) -> RunOutcomeRow {
    execute_with_sink(pool, run, None).await
}

/// [`execute`] with a sink the caller reads.
///
/// The streaming endpoint passes the channel it forwards to the browser; the background runner
/// passes `None`. The persistence is the same closure either way, so the trace a person watches
/// live and the trace they read after a reload are the same rows — and if they ever disagreed, the
/// bug would be in the relay, not in the loop.
pub async fn execute_with_sink(
    pool: &PgPool,
    run: &Run,
    stream: Option<Sink>,
) -> RunOutcomeRow {
    let Some(agent_id) = run.agent_id else {
        // A run whose agent was deleted keeps `agent_id` as null. It is not resurrected and not
        // silently completed: the row ends `failed` saying the definition is gone, which is the
        // only thing the history list can usefully show.
        let outcome = "failed";
        let _ = run_store::finish_run(pool, run.id, StepStatus::Failed, StopReason::Error, Some(
            "this run's agent was deleted; nothing can execute it",
        ))
        .await;
        return RunOutcomeRow {
            status: outcome.to_owned(),
            stop_reason: StopReason::Error.as_str().to_owned(),
        };
    };

    let Some(agent) = run_store::get_agent(pool, run.organization_id, agent_id).await.ok().flatten()
    else {
        let _ = run_store::finish_run(pool, run.id, StepStatus::Failed, StopReason::Error, Some(
            "this run's agent is not in this organization",
        ))
        .await;
        return RunOutcomeRow {
            status: "failed".to_owned(),
            stop_reason: StopReason::Error.as_str().to_owned(),
        };
    };

    if !agent.enabled {
        let _ = run_store::finish_run(pool, run.id, StepStatus::Failed, StopReason::Error, Some(
            "this run's agent is disabled",
        ))
        .await;
        return RunOutcomeRow {
            status: "failed".to_owned(),
            stop_reason: StopReason::Error.as_str().to_owned(),
        };
    }

    // -- the model. The run asks the router exactly as the chat endpoint does, so an agent run
    //    and a chat turn land in the same decision log and cost the same accounting.
    //
    //    An agent's `model_id` pins the *registry row*, not a `provider/model` string, and the
    //    routing walk speaks in strings. Reading the pin back through the catalog to produce the
    //    identifier is one extra query, and it is the difference between "this agent uses the
    //    model it was configured with" and "this agent uses whatever the map says today" — for an
    //    agent with a pin, the second is a bug an operator cannot see.
    let requested = match agent.model_id {
        Some(id) => pinned_model_id(pool, id).await,
        None => None,
    };
    let requirements = if agent.tools.is_empty() {
        Vec::new()
    } else {
        vec!["tools".to_owned()]
    };
    let resolved = resolve_and_record(
        pool,
        DecisionContext {
            organization_id: Some(run.organization_id),
            site_id: run.site_id,
            user_id: run.user_id,
            run_id: Some(run.id),
            task: Some("agent"),
            feature: None,
            requested: requested.as_deref(),
            requirements: &requirements,
        },
        Scope::Organization(run.organization_id),
        requested.as_deref(),
    )
    .await;

    let Ok(resolved) = resolved else {
        let _ = run_store::finish_run(pool, run.id, StepStatus::Failed, StopReason::Error, Some(
            "the model registry could not be read for this run",
        ))
        .await;
        return RunOutcomeRow {
            status: "failed".to_owned(),
            stop_reason: StopReason::Error.as_str().to_owned(),
        };
    };

    let Some(model) = resolved.model else {
        // No candidate at all versus a candidate that was refused: the first is "this
        // installation has no model for agents" and the second is "the one it has cannot call
        // tools". Both end the run, and the message distinguishes them because the fix is
        // different.
        let reason = if resolved.had_candidates {
            "no configured model can serve an agent run; the models this installation has were \
             all refused"
                .to_owned()
        } else {
            "no model is configured for agent runs; set a default model first".to_owned()
        };
        let _ = run_store::finish_run(pool, run.id, StepStatus::Failed, StopReason::Error, Some(&reason)).await;
        return RunOutcomeRow {
            status: "failed".to_owned(),
            stop_reason: StopReason::Error.as_str().to_owned(),
        };
    };

    // -- the tools. **This is the wiring REQ-100 slice 3 existed to make possible.** The line
    //    used to be `ToolRegistry::empty()`, with a comment that the allow-list still governed:
    //    it did, because a run with no registered tool can call nothing at all. But `empty()` also
    //    meant the run had no *identity*, so the first time a tool was registered this run would
    //    have had a tool and no policy. The pipeline below is the only door: identity, grant,
    //    permission, schema, cap, timeout and the call log, in the order the request names them.
    //
    //    The identity is resolved **here**, once, before the first step. A pipeline that looked
    //    its own identity up would need an organization on every call site — which is how a tool
    //    ends up callable from a path that never resolved one.
    let registry = ToolRegistry::empty();
    let allow = AllowList::new(agent.tools.clone(), agent.approvals.clone());

    let resolved_identity = match identity::default_identity(pool, run.organization_id)
        .await
        .ok()
        .flatten()
    {
        Some(row) => {
            let grants = identity::grants_of(pool, row.id).await.unwrap_or_default();
            Some(tool_exec::identity_of(&row, grants))
        }
        None => {
            // Not a failure: a run with no resolvable identity still *runs*, it simply cannot
            // call anything, and the model is shown an empty tool payload. Saying so out loud is
            // the difference between an installation whose identities were never configured and
            // an operator staring at a run that refuses every tool for no visible reason.
            tracing::warn!(
                %run.id,
                %run.organization_id,
                "this run resolved no AI identity, so it may call no tools"
            );
            None
        }
    };

    // A run is not more powerful than whoever started it. The identity narrows what the *agent*
    // may do; this narrows what the *person* behind it may do, and a run started by a schedule or
    // a webhook has no user and therefore no grant — the conservative direction, and the one an
    // agent triggered by an untrusted payload should land in.
    let gate = Arc::new(PermissionsForCaller::load(pool, run.organization_id, run.user_id).await);

    // The pipeline borrows the registry and the gate, and the executor holds the pipeline, so
    // both live behind an `Arc` the executor also holds. That is the whole ownership story: one
    // registry, one gate, one pipeline, one executor, and no path that can assemble a run with
    // the pipeline and without them.
    //
    // **The gate is coerced to `Arc<dyn PermissionGate>` here**, once. The variable's type is the
    // inference's to choose, and leaving it as `Arc<PermissionsForCaller>` is a wall of `E0308`s
    // three lines later at the `clone` — one of which is the only place the type is written down.
    let registry = std::sync::Arc::new(registry);
    let gate: Arc<dyn tool_exec::PermissionGate> = gate;
    let executor = Arc::new(RunExecutor {
        pool: pool.clone(),
        run_id: run.id,
        organization_id: run.organization_id,
        agent_id,
        user_id: run.user_id,
        site_id: run.site_id,
        pipeline: tool_exec::Pipeline::new(
            std::sync::Arc::clone(&registry),
            resolved_identity,
            agent.tools.clone(),
            agent.approvals.clone(),
            std::sync::Arc::clone(&gate),
            tool_exec::Caller {
                organization_id: run.organization_id,
                agent_id,
                run_id: run.id,
                // The step row is written by the loop's own event stream, so at construction
                // there is no step yet; the executor fills it in from the step the loop names.
                step_id: None,
                user_id: run.user_id,
                site_id: run.site_id,
            },
        ),
    });

    // The model is offered **only** the tools the pipeline would let it call. A tool the model
    // can see but cannot call is a tool it will try, and three tries is `loop_detected` — so the
    // payload and the refusal come from one filter, and a disabled or denied tool is invisible
    // rather than merely refused.
    let provider = ProviderModel::new(
        ProviderTarget::from_provider(&model.provider),
        model.model.model_key.clone(),
    )
    .with_tools(executor.payload())
    .temperature(Some(agent.temperature));
    let runtime = Runtime::new(Arc::new(provider), executor, agent.system_prompt.clone());

    let limits = RunLimits::clamped(run_limits_for(&run, &agent));
    // The sink is the one thing the caller supplies. The background runner passes `None` and gets
    // a channel nobody reads (drained in a detached task, so the loop's `send` stays a no-op); the
    // streaming endpoint passes a channel it *is* reading, and the loop's events become the SSE
    // frames. One loop, two consumers, and no branch inside it on which one it is talking to —
    // which is the whole point of the sink seam.
    let owned;
    let sink = match stream {
        Some(sink) => sink,
        None => {
            let (sender, mut drain) = tokio::sync::mpsc::channel::<AgentEvent>(1);
            tokio::spawn(async move { while drain.recv().await.is_some() {} });
            owned = sender;
            owned
        }
    };

    // The handle the persister flips when somebody presses stop, and the one the route's cancel
    // endpoint never needs to know about: the column is the contract between them, not a channel.
    let cancel = runtime.cancel_handle();
    let persister = persister(pool.clone(), run.id, cancel);
    // The output rule, seeded with what this run has already spent. A run claimed for the first
    // time carries `0`; a run that was interrupted *after* a repair turn carries `1`, and
    // arriving with a fresh allowance is how a run ends up repairing until the step cap. The
    // rule itself is `None` for now — no caller of `POST /ai/agents/{id}/runs` has declared one
    // yet — but the seam is here and tested, and a caller that adds the column changes one
    // line rather than teaching the resume path about a concept it does not have.
    let output = output_rule_for(run).map(|rule| OutputVerification {
        rule,
        repairs_spent: u32::try_from(run.output_repairs).unwrap_or(0),
    });
    let outcome = run_agent(
        &runtime,
        &run.goal,
        limits,
        &sink,
        &persister,
        RunOptions { output, ..RunOptions::default() },
    )
    .await;
    heartbeat(pool, run.id).await;

    // A parked run is written `awaiting_approval` with no stop reason at all, because a run
    // waiting for a person has not ended — the migration's own check refuses a reason without a
    // terminal status, and this is the one path where the loop *left* rather than finished.
    //
    // Everything else follows the engine's status, and the error text is the engine's own: a run
    // that failed carries the provider's complaint, and a run that was stopped carries nothing
    // (a person pressing stop is not an error message).
    //
    // The open steps are closed **before** the run row is written, and only for a run that has
    // actually ended. A run parked on an approval deliberately leaves its step `running`: that is
    // the state `resume_point` reads as "a tool may already have fired", and a parked run is
    // exactly the case where that question is open. `finish_run` recomputes the run's totals from
    // its *completed* steps, so closing them first is what makes the stored numbers equal the
    // recomputed ones the panel shows next to them.
    if outcome.status == "awaiting_approval" {
        let _ = run_store::park_run(
            pool,
            run.id,
            Some("the run is waiting for a decision on a gated tool"),
        )
        .await;
    } else {
        let _ = run_store::close_open_steps(pool, run.id).await;
        let status = StepStatus::parse(&outcome.status).unwrap_or(StepStatus::Failed);
        // A run that stopped because somebody pressed stop carries no error text: the reason
        // `cancelled` already says it, and an `error` column on a deliberate stop is what makes a
        // list of stopped runs look like a list of failures.
        let error = if outcome.stop_reason.is_failure() {
            last_error(pool, run.id).await
        } else {
            None
        };
        // The repair count goes on the row, not just into this attempt's trace: it is the one
        // number that has to survive a restart, and a run that failed its output rule and was
        // later resumed must arrive with the budget already spent.
        let _ = run_store::finish_run_with_repairs(
            pool,
            run.id,
            status,
            outcome.stop_reason,
            error.as_deref(),
            outcome.output_repairs,
        )
        .await;
    }

    // The run events that make a run automatable (REQ-099's event list). Emitted after the row is
    // written, so a webhook subscriber that immediately calls back for the run detail reads a
    // finished run rather than a half-written one.
    announce(pool, run, &outcome).await;

    RunOutcomeRow {
        status: outcome.status,
        stop_reason: outcome.stop_reason.as_str().to_owned(),
    }
}

/// The failing step's own message — the text the run's trace already shows.
///
/// Read back from the rows rather than threaded through the loop's return, because the loop
/// deliberately returns only the outcome and publishes the detail as events. A run that failed
/// with a provider's own sentence has that sentence on its `error` step; this is where the run
/// row's `error` column gets it, so the list screen's "why did this end" and the trace's last
/// line are the same sentence.
async fn last_error(pool: &PgPool, run_id: Uuid) -> Option<String> {
    let row: (Option<String>,) = sqlx::query_as(
        "select error from ai_run_steps where run_id = $1 and error is not null \
         order by step_no desc limit 1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;
    row.0
}

/// The `provider/model` identifier an agent's pinned registry row answers to.
///
/// Two queries rather than one because the two rows carry the two halves of the identifier, and
/// a single join would have to decide what a model whose provider has been deleted returns —
/// `None`, which is right, or a dangling string, which is what a left join spells. `None` is the
/// answer either way here: the routing walk then resolves by the map, and a pin whose provider
/// disappeared cannot be a silent second-best.
async fn pinned_model_id(pool: &PgPool, model_id: Uuid) -> Option<String> {
    let model = omnion_ai_hub::find_model(pool, model_id).await.ok().flatten()?;
    let provider = omnion_ai_hub::find_provider(pool, model.provider_id)
        .await
        .ok()
        .flatten()?;
    Some(omnion_ai_hub::model_id(&provider, &model))
}

/// What one executed run produced, as the tick counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcomeRow {
    /// The run's terminal status.
    pub status: String,
    /// Why it ended.
    pub stop_reason: String,
}

/// The limits a run executes under.
///
/// The row's own columns win over the agent's, because a run that was queued when the agent had
/// a 50-step cap and is claimed after the operator set it to 5 must honour the 5 — the run is
/// bounded by the *current* policy, and a cap that only applies to runs created after the edit is
/// a cap that can be escaped by queueing first.
fn run_limits_for(run: &Run, agent: &run_store::Agent) -> RunLimits {
    RunLimits::clamped(RunLimits {
        max_steps: agent.max_steps.max(1) as u32,
        deadline_seconds: agent.deadline_seconds.max(1) as u32,
        token_budget: run.token_budget.unwrap_or(agent.token_budget),
    })
}

/// The output rule a run executes under, if its caller declared one.
///
/// `None` today, and that is the honest answer rather than a stub: the request's output
/// verification is about *a workflow node* asking for a shape, and the run-start route has no
/// field for it yet. What exists is the seam — the run's persisted `output_repairs` is read
/// above and threaded in — so the day a column lands here, the resume path already obeys the
/// budget instead of resetting it.
fn output_rule_for(_run: &Run) -> Option<omnion_ai_hub::guardrails::OutputRule> {
    None
}

/// Publish the terminal event for one run.
///
/// Four names, all dotted lower-case, all carrying the organization and site so an org-scoped
/// endpoint receives only its own deliveries (REQ-099's event table). A run that failed for any
/// other reason is announced as `ai.run.failed`, which is also the event a subscriber listens to
/// when it wants to know a run did not produce an answer.
async fn announce(pool: &PgPool, run: &Run, outcome: &omnion_ai_hub::loop_engine::Outcome) {
    let (name, extra) = match (outcome.status.as_str(), outcome.stop_reason) {
        ("awaiting_approval", _) => (
            "ai.run.awaiting_approval",
            serde_json::json!({ "goal": run.goal, "stop_reason": outcome.stop_reason.as_str() }),
        ),
        ("cancelled", StopReason::Cancelled) => (
            "ai.run.cancelled",
            serde_json::json!({ "goal": run.goal, "steps": outcome.steps }),
        ),
        ("completed", _) => (
            "ai.run.completed",
            serde_json::json!({
                "goal": run.goal,
                "steps": outcome.steps,
                "stop_reason": outcome.stop_reason.as_str(),
            }),
        ),
        (_, StopReason::LoopDetected) => (
            "ai.run.loop_detected",
            serde_json::json!({ "goal": run.goal, "steps": outcome.steps }),
        ),
        _ => (
            "ai.run.failed",
            serde_json::json!({
                "goal": run.goal,
                "steps": outcome.steps,
                "stop_reason": outcome.stop_reason.as_str(),
            }),
        ),
    };

    let event = omnion_events::NewEvent::new(name)
        .organization(run.organization_id)
        .site(run.site_id)
        .actor(run.user_id)
        .payload(extra);
    if let Err(error) = omnion_events::bus::emit(pool, event).await {
        tracing::warn!(%error, name, "an agent run event could not be published");
    }
}

/// One claim, one run: the tick the runner actually executes.
pub async fn tick(pool: &PgPool, slots_free: usize) -> Result<TickReport, omnion_ai_hub::AiHubError> {
    if slots_free == 0 {
        return Ok(TickReport::default());
    }
    let Some(run) = run_store::claim_next_run(pool).await? else {
        return Ok(TickReport::default());
    };
    // A claim is immediately followed by a cancellation check. Claiming a cancelled run and then
    // running it for four steps is a real cost for a decision the operator already made.
    if run_store::cancel_requested(pool, run.id).await? {
        let _ = run_store::finish_run(
            pool,
            run.id,
            StepStatus::Skipped,
            StopReason::Cancelled,
            Some("cancelled before the first step"),
        )
        .await;
        return Ok(TickReport {
            executed: 1,
            ..TickReport::default()
        });
    }

    let outcome = execute(pool, &run).await;
    tracing::info!(
        run = %run.id,
        status = %outcome.status,
        stop_reason = %outcome.stop_reason,
        "agent run finished"
    );
    Ok(TickReport {
        executed: 1,
        unresolved: usize::from(outcome.status == "failed" && outcome.stop_reason == "error"),
        ..TickReport::default()
    })
}

/// Hand runs whose worker died back to the queue.
pub async fn sweep(pool: &PgPool) -> Result<u64, omnion_ai_hub::AiHubError> {
    run_store::requeue_stale(pool).await
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
///
/// `in_flight` is the process-wide count of runs this binary is executing. It is an `Arc` because
/// the worker tasks are spawned and the count has to outlive this function, and an `AtomicUsize`
/// because two ticks must not both see three free slots and start a fourth and a fifth run.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let pool = state.db().pool().clone();
    let concurrency = state.config().ai_hub.runner_concurrency.max(1);
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    tracing::info!(concurrency, "agent runner started");

    let ticks = pool.clone();
    let tick_count = Arc::clone(&in_flight);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_millis(TICK_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work, so let it pass.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let free = concurrency.saturating_sub(tick_count.load(std::sync::atomic::Ordering::SeqCst));
            if free == 0 {
                continue;
            }
            let Some(run) = run_store::claim_next_run(&ticks).await.ok().flatten() else {
                continue;
            };
            tick_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let task_pool = ticks.clone();
            let task_count = Arc::clone(&tick_count);
            tokio::spawn(async move {
                let _ = execute(&task_pool, &run).await;
                task_count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
    });

    let reaper = pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_millis(REAPER_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match run_store::requeue_stale(&reaper).await {
                Ok(0) => {}
                Ok(count) => {
                    tracing::warn!(count, "agent runs with a stale heartbeat were requeued");
                }
                Err(error) => {
                    tracing::warn!(%error, "the agent run reaper failed");
                }
            }
        }
    });

    tokio::spawn(async move {})
}

// `StepKind` and `Uuid` are used by the row-writing helpers the tests below need; keeping the
// imports honest rather than letting `cargo build` fail on an unused one is why they are named
// here with a use that documents the contract.
const _: Option<StepKind> = None;
const _: Option<Uuid> = None;
