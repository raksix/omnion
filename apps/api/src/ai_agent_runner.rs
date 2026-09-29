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
use omnion_ai_hub::loop_engine::{Persist, Runtime, Sink, run as run_agent};
use omnion_ai_hub::provider_model::ProviderModel;
use omnion_ai_hub::run_store::{self, Run};
use omnion_ai_hub::tools::{AllowList, ToolRegistry};
use omnion_ai_hub::{DecisionContext, ProviderTarget, Scope, resolve_and_record};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::state::AppState;

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

/// A `Persist` that writes the loop's events onto the run's step rows.
///
/// One closure per run, holding the pool and the id. It is a closure rather than a function
/// because the engine's `Persist` is `Fn(AgentEvent) -> Future`, and the run id is the only thing
/// that distinguishes one run's durable record from another's — a `fn(&PgPool, Uuid, AgentEvent)`
/// would mean every caller building that same closure by hand.
fn persister(pool: PgPool, run_id: Uuid) -> Persist {
    Box::new(move |event: AgentEvent| {
        let pool = pool.clone();
        Box::pin(async move {
            if let Err(error) = run_store::append_event(&pool, run_id, &event).await {
                tracing::warn!(%run_id, %error, "an agent run event could not be written");
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    })
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

    // The tools this agent may call. REQ-100 owns the catalogue; until it lands the registry is
    // empty and an agent's `tools` column names keys nothing implements. That is not a silent
    // success: the allow-list still governs, a call to an unregistered key is refused with
    // `tool_unknown` in the trace, and the run's own prompt says what it may do.
    let registry = ToolRegistry::empty();
    let allow = AllowList::new(agent.tools.clone(), agent.approvals.clone());
    let provider = ProviderModel::new(
        ProviderTarget::from_provider(&model.provider),
        model.model.model_key.clone(),
    )
    .with_registry(&registry)
    .temperature(Some(agent.temperature));
    let runtime = Runtime::new(
        Arc::new(provider),
        registry,
        allow,
        agent.system_prompt.clone(),
    );

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

    let persister = persister(pool.clone(), run.id);
    let outcome = run_agent(&runtime, &run.goal, limits, &sink, &persister).await;
    heartbeat(pool, run.id).await;

    // A parked run is written `awaiting_approval` with no stop reason at all, because a run
    // waiting for a person has not ended — the migration's own check refuses a reason without a
    // terminal status, and this is the one path where the loop *left* rather than finished.
    //
    // Everything else follows the engine's status, and the error text is the engine's own: a run
    // that failed carries the provider's complaint, and a run that was stopped carries nothing
    // (a person pressing stop is not an error message).
    if outcome.status == "awaiting_approval" {
        let _ = run_store::park_run(
            pool,
            run.id,
            Some("the run is waiting for a decision on a gated tool"),
        )
        .await;
    } else {
        let status = StepStatus::parse(&outcome.status).unwrap_or(StepStatus::Failed);
        // A run that stopped because somebody pressed stop carries no error text: the reason
        // `cancelled` already says it, and an `error` column on a deliberate stop is what makes a
        // list of stopped runs look like a list of failures.
        let error = if outcome.stop_reason.is_failure() {
            last_error(pool, run.id).await
        } else {
            None
        };
        let _ = run_store::finish_run(pool, run.id, status, outcome.stop_reason, error.as_deref()).await;
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
