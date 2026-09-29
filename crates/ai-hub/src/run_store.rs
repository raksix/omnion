//! The store for agents, their runs and their steps (REQ-099, slice 1).
//!
//! [`crate::agent`] is deliberately pure: it decides *when a run stops* and nothing else, so
//! every stop condition has a test that fires it with values the test wrote itself. This module
//! is the other half — everything about *where the run is kept*, and it exists because the
//! durability story cannot be tested on a struct.
//!
//! # The three rules this store exists to make true
//!
//! 1. **A run row is written before the first step, and a step row is written `running` before
//!    the tool runs.** That ordering is the whole resume story. A step left `running` after a
//!    crash means the tool may or may not have executed, which is *not* the same as "did not
//!    run" — and the difference is a doubled side effect. [`begin_step`] therefore refuses to
//!    overwrite a step that is already `completed`, and [`resume_point`] reports the ambiguity
//!    instead of hiding it.
//!
//! 2. **A run's totals are the sum of its steps, and the store writes both in one transaction.**
//!    `prompt_tokens`, `completion_tokens` and `cost_micros` on `ai_runs` are denormalised so
//!    the history list can render a hundred rows without aggregating a hundred step rows each.
//!    A denormalised figure that can drift is worse than none, so [`finish_run`] recomputes from
//!    the steps rather than adding to the running total, and `run_totals_match_steps` is the test
//!    that keeps the promise.
//!
//! 3. **A cross-organization id answers "not found", never "forbidden".** Every read is scoped by
//!    `organization_id` in the `where` clause rather than filtered after the fetch, so a run in
//!    another tenant is indistinguishable from a run that does not exist — which is the only
//!    answer that does not confirm that it exists.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::agent::{
    AgentEvent, RunLimits, StepKind, StepStatus, StopReason, ToolCall, MAX_GOAL_CHARS,
};
use crate::error::{AiHubError, Result};

// `tools` and `approvals` are jsonb columns and the Rust side wants `Vec<String>`. sqlx decodes a
// `Vec<String>` as `text[]`, and PostgreSQL has no jsonb-to-text[] cast — so the column is read
// through a scalar subquery that expands the array. Writing the column as `tools` instead fails
// with "Rust type Vec<String> is not compatible with SQL type JSONB", and casting it fails with
// "cannot cast type jsonb to text[]"; the expansion is the only spelling that decodes.
//
// The `coalesce` is not decoration: `array_agg` over zero rows returns NULL, and a `Vec<String>`
// field is not optional, so an agent with no tools would come back as a decode error rather than
// an empty list. Every agent without tools would fail to load.
//
// Public because the API's agent update writes the same projection back: a second copy of this
// column list is one that can drift, and a drifted `returning` clause decodes into a struct that
// no longer has the columns the struct has.
#[doc(hidden)]
pub const AGENT_COLUMNS: &str = "id, organization_id, site_id, key, name, description, system_prompt, \
     model_id, temperature::float8, max_steps, deadline_seconds, token_budget, \
     coalesce((select array_agg(value) from jsonb_array_elements_text(tools)), '{}') as tools, \
     coalesce((select array_agg(value) from jsonb_array_elements_text(approvals)), '{}') as approvals, \
     memory_scope, enabled, created_by, created_at, updated_at";

// `output_repairs` is in the column list rather than read separately, because the resume path
// needs it at the same moment it needs the limits: a run that is re-claimed has to learn what
// the run is *and* what the output rule already spent, and two reads of one row are two reads
// that can straddle a write.
const RUN_COLUMNS: &str = "id, organization_id, site_id, agent_id, user_id, trigger, goal, status, \
     stop_reason, model_id, current_step, resume_count, cancel_requested_at, deadline_at, \
     token_budget, prompt_tokens, completion_tokens, cost_micros, output_repairs, heartbeat_at, \
     started_at, finished_at, error";

const STEP_COLUMNS: &str = "id, run_id, step_no, kind, tool, arguments, result, status, \
     prompt_tokens, completion_tokens, cost_micros, duration_ms, error, started_at, finished_at";

/// A stored agent, as the runtime needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Agent {
    /// Row identity.
    pub id: Uuid,
    /// The tenant this agent belongs to. Every read is scoped by it.
    pub organization_id: Uuid,
    /// The site this agent is bound to, when it is bound to one.
    pub site_id: Option<Uuid>,
    /// API-visible identifier, unique per organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the agent is for.
    pub description: String,
    /// The prompt that frames every run.
    pub system_prompt: String,
    /// The pinned model, when the agent does not route per request.
    pub model_id: Option<Uuid>,
    /// Sampling temperature.
    pub temperature: f64,
    /// Step ceiling. The column *is* the ceiling — see the migration's header.
    pub max_steps: i32,
    /// Wall-clock ceiling in seconds.
    pub deadline_seconds: i32,
    /// Token ceiling for the whole run.
    pub token_budget: i64,
    /// Tools the agent may call, in allow-list order.
    pub tools: Vec<String>,
    /// Tools that park the run for a person instead of running.
    pub approvals: Vec<String>,
    /// Which memory scope the run may read.
    pub memory_scope: String,
    /// Whether the agent can be started at all.
    pub enabled: bool,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    pub updated_at: OffsetDateTime,
}

impl Agent {
    /// The limits this agent runs under, clamped the same way for every caller.
    ///
    /// The column is already constrained by the migration, so clamping is belt-and-braces — but
    /// the runtime must not be the thing that trusts a row, because a row can also arrive from a
    /// seed script or a future import that skipped the constraint.
    #[must_use]
    pub fn limits(&self) -> RunLimits {
        RunLimits::clamped(RunLimits {
            max_steps: self.max_steps.max(0) as u32,
            deadline_seconds: self.deadline_seconds.max(0) as u32,
            token_budget: self.token_budget,
        })
    }
}

/// What a caller supplies to create an agent.
#[derive(Debug, Clone)]
pub struct NewAgent {
    /// The tenant.
    pub organization_id: Uuid,
    /// Optional site binding.
    pub site_id: Option<Uuid>,
    /// API-visible key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the agent is for.
    pub description: String,
    /// The prompt that frames every run.
    pub system_prompt: String,
    /// Optional model pin.
    pub model_id: Option<Uuid>,
    /// Sampling temperature.
    pub temperature: f64,
    /// Step ceiling.
    pub max_steps: i32,
    /// Wall-clock ceiling.
    pub deadline_seconds: i32,
    /// Token ceiling.
    pub token_budget: i64,
    /// Tool allow-list.
    pub tools: Vec<String>,
    /// Approval list.
    pub approvals: Vec<String>,
    /// Memory scope.
    pub memory_scope: String,
    /// Enabled at creation.
    pub enabled: bool,
    /// The author.
    pub created_by: Option<Uuid>,
}

impl NewAgent {
    /// The shape the form's defaults produce, so a caller that only sets a name still gets the
    /// documented ceilings rather than zeros the runtime would clamp to the floor.
    #[must_use]
    pub fn with_defaults(organization_id: Uuid, key: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            organization_id,
            site_id: None,
            key: key.into(),
            name: name.into(),
            description: String::new(),
            system_prompt: String::new(),
            model_id: None,
            temperature: 0.20,
            max_steps: crate::agent::DEFAULT_MAX_STEPS as i32,
            deadline_seconds: crate::agent::DEFAULT_DEADLINE_SECONDS as i32,
            token_budget: crate::agent::DEFAULT_TOKEN_BUDGET,
            tools: Vec::new(),
            approvals: Vec::new(),
            memory_scope: "none".to_owned(),
            enabled: true,
            created_by: None,
        }
    }
}

/// A stored run, as the history list and the trace render it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Run {
    /// Row identity.
    pub id: Uuid,
    /// The tenant.
    pub organization_id: Uuid,
    /// Optional site binding.
    pub site_id: Option<Uuid>,
    /// The agent, or `None` once the agent is deleted.
    pub agent_id: Option<Uuid>,
    /// Who started it.
    pub user_id: Option<Uuid>,
    /// What started it.
    pub trigger: String,
    /// What the run was asked to do.
    pub goal: String,
    /// Where it is.
    pub status: String,
    /// Why it ended, once it has.
    pub stop_reason: Option<String>,
    /// The model the loop settled on.
    pub model_id: Option<Uuid>,
    /// How many steps have started.
    pub current_step: i32,
    /// How many times it has been resumed.
    pub resume_count: i32,
    /// When cancellation was asked for.
    pub cancel_requested_at: Option<OffsetDateTime>,
    /// The wall-clock instant the deadline falls at.
    pub deadline_at: Option<OffsetDateTime>,
    /// The token ceiling this run was started with.
    pub token_budget: Option<i64>,
    /// Prompt tokens charged so far.
    pub prompt_tokens: i32,
    /// Completion tokens charged so far.
    pub completion_tokens: i32,
    /// Cost in millionths, the same unit `ai_usage` uses.
    pub cost_micros: i64,
    /// Repair turns the output verification has already spent (REQ-099 slice 4).
    ///
    /// Persisted because the budget has to survive the process. The loop counts what it spent
    /// and reports it on the `Outcome`, but a count that lives only in the loop's stack is a
    /// zero after a restart — so an interrupted run resumes with a fresh allowance and
    /// repairs forever, which is the exact failure the one-turn constant exists to prevent.
    pub output_repairs: i32,
    /// The runner's last sign of life.
    pub heartbeat_at: Option<OffsetDateTime>,
    /// When the first step began.
    pub started_at: Option<OffsetDateTime>,
    /// When it reached a terminal state.
    pub finished_at: Option<OffsetDateTime>,
    /// The failure text, when it failed.
    pub error: Option<String>,
}

impl Run {
    /// Whether the run is over — the one predicate the history list and the runner share.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }

    /// The stop reason, parsed. A row whose reason the vocabulary does not know parses as `None`
    /// rather than panicking: an unknown reason is a newer writer's value, not a crash.
    #[must_use]
    pub fn reason(&self) -> Option<StopReason> {
        self.stop_reason.as_deref().and_then(StopReason::parse)
    }
}

/// A stored step, as the trace accordion renders it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Step {
    /// Row identity.
    pub id: Uuid,
    /// The run it belongs to.
    pub run_id: Uuid,
    /// Its position, 1-based and unique per run.
    pub step_no: i32,
    /// What kind of step it is.
    pub kind: String,
    /// The tool, for the tool kinds.
    pub tool: Option<String>,
    /// The call arguments, redacted at write time.
    pub arguments: Option<serde_json::Value>,
    /// The result summary.
    pub result: Option<serde_json::Value>,
    /// Where it got to.
    pub status: String,
    /// Prompt tokens this step charged.
    pub prompt_tokens: i32,
    /// Completion tokens this step charged.
    pub completion_tokens: i32,
    /// The cost this step was billed at, snapshotted when it was written.
    pub cost_micros: i64,
    /// How long it took.
    pub duration_ms: Option<i32>,
    /// The failure text, when it failed.
    pub error: Option<String>,
    /// When it began.
    pub started_at: OffsetDateTime,
    /// When it ended.
    pub finished_at: Option<OffsetDateTime>,
}

impl Step {
    /// The step's kind, parsed.
    #[must_use]
    pub fn kind(&self) -> Option<StepKind> {
        StepKind::parse(&self.kind)
    }

    /// The step's status, parsed.
    #[must_use]
    pub fn status(&self) -> Option<StepStatus> {
        StepStatus::parse(&self.status)
    }
}

/// A row about to be written, with the fields a caller can set.
#[derive(Debug, Clone, Default)]
pub struct NewRun {
    /// The tenant.
    pub organization_id: Uuid,
    /// Optional site binding.
    pub site_id: Option<Uuid>,
    /// The agent to run.
    pub agent_id: Uuid,
    /// Who is asking.
    pub user_id: Option<Uuid>,
    /// What started it.
    pub trigger: String,
    /// What the run must do.
    pub goal: String,
    /// The model pin carried from the agent.
    pub model_id: Option<Uuid>,
    /// The token ceiling carried from the agent.
    pub token_budget: i64,
    /// The wall-clock instant the deadline falls at.
    pub deadline_at: Option<OffsetDateTime>,
}

impl NewRun {
    /// The shape a caller gets from the three fields it must supply and nothing else.
    ///
    /// The alternative is nine `..Default::default()` spreads at every call site, each of which
    /// silently fills `trigger` with the empty string and trips a check constraint — a default
    /// that produces a row nobody can insert is not a convenience.
    #[must_use]
    pub fn default_for(organization_id: Uuid, agent_id: Uuid) -> Self {
        Self {
            organization_id,
            site_id: None,
            agent_id,
            user_id: None,
            trigger: "agent".to_owned(),
            goal: String::new(),
            model_id: None,
            token_budget: crate::agent::DEFAULT_TOKEN_BUDGET,
            deadline_at: None,
        }
    }
}

/// The token and cost totals a finished run carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunTotals {
    /// Prompt tokens across the run's completed steps.
    pub prompt_tokens: i32,
    /// Completion tokens across the run's completed steps.
    pub completion_tokens: i32,
    /// Cost in millionths across the run's completed steps.
    pub cost_micros: i64,
}

/// A run's step totals recomputed from the step rows.
///
/// This is the function the "a run's cost equals the sum of its steps" acceptance box is proved
/// with. It is deliberately a read of the steps rather than a read of the run's own columns: a
/// check that compared the column to itself would pass forever.
pub async fn run_totals_match_steps(pool: &PgPool, run_id: Uuid) -> Result<RunTotals> {
    let row: (Option<i32>, Option<i32>, Option<i64>) = sqlx::query_as(
        "select sum(prompt_tokens)::int, sum(completion_tokens)::int, sum(cost_micros)::bigint \
         from ai_run_steps where run_id = $1 and status = 'completed'",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    Ok(RunTotals {
        prompt_tokens: row.0.unwrap_or(0),
        completion_tokens: row.1.unwrap_or(0),
        cost_micros: row.2.unwrap_or(0),
    })
}

/// Create an agent, refusing a duplicate key with a message that names the key.
///
/// The unique index is the real guarantee; this turns its error into something an operator can
/// act on instead of a 500 carrying a constraint name.
pub async fn create_agent(pool: &PgPool, new: &NewAgent) -> Result<Agent> {
    // The insert names every column except `id`, `created_at` and `updated_at`, so the database
    // supplies them. Each placeholder is cast to its column's real type: sqlx sends these as
    // untyped text over the wire and PostgreSQL has no other way to know that `$8` is a numeric
    // rather than a uuid — the two failure modes are a `text`-into-`uuid` error for the nullable
    // columns and a `double precision`-into-`uuid` one when a cast is put on the wrong slot.
    let sql = format!(
        "insert into ai_agents (organization_id, site_id, key, name, description, system_prompt, \
         model_id, temperature, max_steps, deadline_seconds, token_budget, tools, approvals, \
         memory_scope, enabled, created_by) values \
         ($1,$2::uuid,$3,$4,$5,$6,$7::uuid,$8::numeric,$9,$10,$11,$12::jsonb,$13::jsonb,$14,$15,$16::uuid) \
         returning {AGENT_COLUMNS}"
    );
    match sqlx::query_as::<_, Agent>(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(&new.key)
        .bind(&new.name)
        .bind(&new.description)
        .bind(&new.system_prompt)
        .bind(new.model_id)
        .bind(new.temperature)
        .bind(new.max_steps)
        .bind(new.deadline_seconds)
        .bind(new.token_budget)
        .bind(serde_json::to_value(&new.tools).unwrap_or(serde_json::Value::Array(Vec::new())))
        .bind(serde_json::to_value(&new.approvals).unwrap_or(serde_json::Value::Array(Vec::new())))
        .bind(&new.memory_scope)
        .bind(new.enabled)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
    {
        Ok(agent) => Ok(agent),
        Err(sqlx::Error::Database(err)) if err.code().as_deref() == Some("23505") => {
            Err(AiHubError::InvalidAgent(format!(
                "an agent keyed \"{}\" already exists in this organization",
                new.key
            )))
        }
        Err(err) => Err(err.into()),
    }
}

/// Read one agent inside one organization.
///
/// A row in another organization is `None` rather than an error: the caller cannot tell the
/// difference between "not yours" and "not there", which is the only answer that does not confirm
/// that somebody else's agent exists.
pub async fn get_agent(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<Agent>> {
    let sql = format!("select {AGENT_COLUMNS} from ai_agents where id = $1 and organization_id = $2");
    Ok(sqlx::query_as::<_, Agent>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

/// Every agent in an organization, newest first — the list screen's own ordering.
pub async fn list_agents(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Agent>> {
    let sql = format!(
        "select {AGENT_COLUMNS} from ai_agents where organization_id = $1 order by created_at desc"
    );
    Ok(sqlx::query_as::<_, Agent>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// The runner's three queries
// ---------------------------------------------------------------------------------------------

/// How long a claim is trusted before another process may take the run back.
pub const HEARTBEAT_STALE_SECONDS: i64 = 120;

/// Claim the next queued run, atomically, and mark it running.
///
/// `for update skip locked` is the whole mechanism, and it is a deliberate choice over "select
/// one, then update it": the pair without `skip locked` is the classic double-claim race, and two
/// API processes both running a two-tool agent is a bill nobody approved. `skip locked` makes the
/// loser move to the next row instead of waiting for the winner's transaction, so a claim costs
/// one statement and a contended claim costs nothing.
///
/// The `started_at` is set *here* rather than when the row was created, because the run does not
/// begin when somebody pressed Run — it begins when a worker picked it up. A run queued behind
/// three others has been waiting, and the list screen's "started" column is about the work.
///
/// Ordering is `started_at nulls first, id`, which is the queue order for rows that have never
/// been claimed and stays stable for rows that have: a run that was requeued after a dead worker
/// keeps the instant it first started, so it is retried before a run created later.
pub async fn claim_next_run(pool: &PgPool) -> Result<Option<Run>> {
    // The selection is one statement and the status change is another, inside one transaction,
    // and both are `for update skip locked`.
    //
    // It began as a single `with candidate as (...) update ai_runs ... from candidate` statement,
    // which is the tidier SQL and is a **runtime error**: with the CTE in the `from` clause, every
    // bare column in the returning list resolves against both relations and PostgreSQL answers
    // `column reference "id" is ambiguous` before the statement runs. The `select … for update
    // skip locked` half is exactly the part that must not be a separate transaction — a claim
    // that is not atomic is the double-claim this whole function exists to prevent — so the
    // selection is still `skip locked`, just issued on its own and closed in the same
    // transaction as the update.
    let mut tx = pool.begin().await?;
    // `fetch_optional`, not `fetch_one`: an empty queue is the *normal* state of a runner on an
    // installation nobody has pressed Run on, and `fetch_one` turns it into `RowNotFound` — which
    // the caller would have to read as "the claim failed" rather than "there was nothing to
    // claim". A queue poll that logs an error whenever it finds no work is a queue poll that
    // trains an operator to ignore its log.
    let picked: Option<(Uuid,)> = sqlx::query_as(
        "select id from ai_runs where status = 'queued' \
         order by started_at asc nulls first, id \
         for update skip locked limit 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((run_id,)) = picked else {
        tx.rollback().await?;
        return Ok(None);
    };

    let sql = format!(
        "update ai_runs set status = 'running', started_at = coalesce(started_at, now()), \
         heartbeat_at = now() where id = $1 returning {RUN_COLUMNS}"
    );
    let run = sqlx::query_as::<_, Run>(&sql)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(run))
}

/// Claim one *specific* run, by id, atomically.
///
/// The streaming path needs this and the background runner does not: a client that pressed Run
/// holds a run id it will show on screen, and a queue-position-based claim would start a
/// *different* run than the one it is watching. The `and status = 'queued'` predicate is what
/// makes the two racers safe — whichever gets there first moves the row to `running`, and the
/// other's update matches zero rows and returns `None` instead of running the loop twice.
pub async fn claim_run(pool: &PgPool, run_id: Uuid) -> Result<Option<Run>> {
    let sql = format!(
        "update ai_runs set status = 'running', started_at = coalesce(started_at, now()), \
         heartbeat_at = now() where id = $1 and status = 'queued' returning {RUN_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Run>(&sql)
        .bind(run_id)
        .fetch_optional(pool)
        .await?)
}

/// The runner's sign of life. Called at every step boundary.
///
/// A separate statement rather than part of `begin_step` because the two answer different
/// questions: the step row says what the run *did*, the heartbeat says a worker is still holding
/// it. A run whose provider call takes ninety seconds writes no steps at all, and a reaper that
/// only looked at steps would hand that run to a second worker while the first was still waiting
/// on the socket.
pub async fn heartbeat(pool: &PgPool, run_id: Uuid) -> Result<()> {
    sqlx::query("update ai_runs set heartbeat_at = now() where id = $1")
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Hand runs whose worker died back to the queue.
///
/// A run is stale when its heartbeat is older than [`HEARTBEAT_STALE_SECONDS`] **and** it is
/// still `running`. Both halves matter: requeueing a run that is `queued` would be a no-op that
/// still takes the row lock, and requeueing a `completed` run would undo a result somebody is
/// looking at. The threshold is deliberately several times the provider timeout, so a slow call is
/// never mistaken for a dead worker.
pub async fn requeue_stale(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "update ai_runs set status = 'queued', heartbeat_at = null, resume_count = resume_count + 1 \
         where status = 'running' \
         and (heartbeat_at is null or heartbeat_at < now() - make_interval(secs => $1))",
    )
    .bind(HEARTBEAT_STALE_SECONDS as f64)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Park a run for a decision, keeping the step that asked.
///
/// The status is the run's own, not an inference: the approval inbox reads this state, the list
/// screen's badge reads it, and a run that *looks* parked because its last step is an approval
/// row is a run whose status and its trace disagree the moment a second run starts.
pub async fn park_run(pool: &PgPool, run_id: Uuid, error: Option<&str>) -> Result<()> {
    sqlx::query(
        "update ai_runs set status = 'awaiting_approval', stop_reason = null, \
         heartbeat_at = null, error = $2 where id = $1",
    )
    .bind(run_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Put a finished or parked run back on the queue, for a resume or a decision.
///
/// `resume_count` is bumped here rather than at the moment the answer arrived, so the count is
/// the number of times the run actually *re-ran* — which is the number the trace needs and the
/// one an operator reading "resumed 4 times" is asking about.
pub async fn requeue_run(pool: &PgPool, run_id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "update ai_runs set status = 'queued', resume_count = resume_count + 1, \
         heartbeat_at = null, error = null \
         where id = $1 and status in ('awaiting_approval', 'failed')",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// How many runs are queued — the runner's own saturation read, and the 503 the start endpoint
/// answers when the queue is far deeper than the pool can ever drain.
pub async fn queued_runs(pool: &PgPool) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("select count(*) from ai_runs where status = 'queued'")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Remove an agent. Its runs survive with a null `agent_id` — the migration says why.
pub async fn delete_agent(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let done = sqlx::query("delete from ai_agents where id = $1 and organization_id = $2")
        .bind(id)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Insert a run in `queued`.
///
/// The row exists before the runner claims it, so a request that dies between "the user pressed
/// Run" and "the model was called" still leaves a row somebody can find.
pub async fn create_run(pool: &PgPool, new: &NewRun) -> Result<Run> {
    let sql = format!(
        "insert into ai_runs (id, organization_id, site_id, agent_id, user_id, trigger, goal, \
         status, model_id, token_budget, deadline_at) \
         values ($1,$2,$3::uuid,$4::uuid,$5::uuid,$6,$7,'queued',$8::uuid,$9,$10) returning {RUN_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Run>(&sql)
        .bind(Uuid::new_v4())
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(new.agent_id)
        .bind(new.user_id)
        .bind(&new.trigger)
        .bind(&new.goal)
        .bind(new.model_id)
        .bind(new.token_budget)
        .bind(new.deadline_at)
        .fetch_one(pool)
        .await?)
}

/// Read one run inside one organization.
pub async fn get_run(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<Run>> {
    let sql = format!("select {RUN_COLUMNS} from ai_runs where id = $1 and organization_id = $2");
    Ok(sqlx::query_as::<_, Run>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

/// Runs for one organization, newest first, with an optional cap.
///
/// The cap is applied in SQL rather than by truncating a `Vec`: the history list asks for 50 and
/// the database has a million, and fetching a million to drop all but fifty is how a screen stops
/// responding at exactly the moment somebody needs it.
pub async fn list_runs(
    pool: &PgPool,
    organization_id: Uuid,
    agent_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Run>> {
    let sql = format!(
        "select {RUN_COLUMNS} from ai_runs where organization_id = $1 \
         and ($2::uuid is null or agent_id = $2) order by started_at desc nulls last limit $3"
    );
    Ok(sqlx::query_as::<_, Run>(&sql)
        .bind(organization_id)
        .bind(agent_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(pool)
        .await?)
}

/// A run's steps in trace order — what the trace and the replay assertion both read.
pub async fn list_steps(pool: &PgPool, run_id: Uuid) -> Result<Vec<Step>> {
    let sql = format!("select {STEP_COLUMNS} from ai_run_steps where run_id = $1 order by step_no");
    Ok(sqlx::query_as::<_, Step>(&sql).bind(run_id).fetch_all(pool).await?)
}

/// Append a step in `running` and return its number.
///
/// This is the call that makes resume safe, and its ordering is the whole point: the row exists
/// *before* the tool runs, so a process that dies mid-tool leaves a `running` row that
/// [`resume_point`] can report as ambiguous. Writing the row after the tool returns would make a
/// crash and a tool that never ran look identical.
pub async fn begin_step(
    pool: &PgPool,
    run_id: Uuid,
    kind: StepKind,
    tool: Option<&str>,
    arguments: Option<&serde_json::Value>,
) -> Result<i32> {
    let next: (i32,) =
        sqlx::query_as("select coalesce(max(step_no), 0) + 1 from ai_run_steps where run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await?;
    let step_no = next.0;
    sqlx::query(
        "insert into ai_run_steps (run_id, step_no, kind, tool, arguments, status) \
         values ($1,$2,$3,$4,$5,'running')",
    )
    .bind(run_id)
    .bind(step_no)
    .bind(kind.as_str())
    .bind(tool)
    .bind(arguments)
    .execute(pool)
    .await?;
    sqlx::query("update ai_runs set current_step = greatest(current_step, $2) where id = $1")
        .bind(run_id)
        .bind(step_no)
        .execute(pool)
        .await?;
    Ok(step_no)
}

/// Close a step, charging the run's totals from this step's own counters.
///
/// The run's columns are written in the same statement as the step's, so a crash between the two
/// cannot leave a run whose totals disagree with its steps for longer than one statement.
pub async fn finish_step(
    pool: &PgPool,
    run_id: Uuid,
    step_no: i32,
    status: StepStatus,
    result: Option<&serde_json::Value>,
    prompt_tokens: i32,
    completion_tokens: i32,
    cost_micros: i64,
    duration_ms: Option<i32>,
    error: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "update ai_run_steps set status = $3, result = $4, prompt_tokens = $5, \
         completion_tokens = $6, cost_micros = $7, duration_ms = $8, error = $9, finished_at = now() \
         where run_id = $1 and step_no = $2",
    )
    .bind(run_id)
    .bind(step_no)
    .bind(status.as_str())
    .bind(result)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(cost_micros)
    .bind(duration_ms)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    // Only the *token* counters are incremented here. The cost is deliberately not: it is
    // recomputed from the steps by `finish_run`, and a column that is both incremented per step
    // and recomputed at the end is a column that counts the last step twice. The first version
    // did both and a three-step run reported 13 599 for 3 600 micros of work — invisible in the
    // list, obvious in the invoice.
    sqlx::query(
        "update ai_runs set prompt_tokens = prompt_tokens + $2, \
         completion_tokens = completion_tokens + $3 where id = $1",
    )
    .bind(run_id)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Move a run to a terminal state, recomputing its totals from its steps.
///
/// Recomputing rather than adding is deliberate: `finish_step` already increments, so adding
/// again would double every figure, and the failure would be invisible in the list and obvious in
/// the invoice. A sum over the steps is idempotent, so calling this twice is harmless.
pub async fn finish_run(
    pool: &PgPool,
    run_id: Uuid,
    status: StepStatus,
    reason: StopReason,
    error: Option<&str>,
) -> Result<()> {
    finish_run_with_repairs(pool, run_id, status, reason, error, 0).await
}

/// [`finish_run`], recording how much of the output-verification budget this attempt spent.
///
/// **The default is `0`, and that is a decision rather than a convenience.** The four other
/// call sites in this crate predate the output rule and are finishing runs that never had one,
/// so `0` is the correct value for all of them. The runner passes the loop's own count, and a
/// caller that forgets to will see the budget reset on the next resume — which is why the
/// column is bounded to `between 0 and 1` rather than left open: a run claiming two repairs is
/// a violation the database refuses, not a number the panel quietly renders.
pub async fn finish_run_with_repairs(
    pool: &PgPool,
    run_id: Uuid,
    status: StepStatus,
    reason: StopReason,
    error: Option<&str>,
    output_repairs: u32,
) -> Result<()> {
    let run_status = match status {
        StepStatus::Completed => "completed",
        StepStatus::Failed => "failed",
        _ => "cancelled",
    };
    let totals = run_totals_match_steps(pool, run_id).await?;
    // `least(1, …)` rather than trusting the caller: the constant is a Rust value and the
    // constraint is a SQL one, and the day they disagree the one that loses is whichever is
    // checked later. Saturating here makes the write succeed with the policy's own ceiling.
    let repairs = i32::try_from(output_repairs).unwrap_or(i32::MAX).min(1);
    sqlx::query(
        "update ai_runs set status = $2, stop_reason = $3, error = $4, finished_at = now(), \
         prompt_tokens = $5, completion_tokens = $6, cost_micros = $7, output_repairs = $8 \
         where id = $1",
    )
    .bind(run_id)
    .bind(run_status)
    .bind(reason.as_str())
    .bind(error)
    .bind(totals.prompt_tokens)
    .bind(totals.completion_tokens)
    .bind(totals.cost_micros)
    .bind(repairs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ask a run to stop. The loop notices at the next step boundary, never mid-tool.
///
/// Writing the request rather than acting on it is what makes cancellation *safe*: a tool that
/// has already started is left to finish, because a half-applied side effect is worse than a
/// slightly later stop.
pub async fn request_cancel(pool: &PgPool, run_id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "update ai_runs set cancel_requested_at = now() \
         where id = $1 and cancel_requested_at is null and status in ('queued','running')",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Whether cancellation has been asked for — the check the loop makes at a step boundary.
pub async fn cancel_requested(pool: &PgPool, run_id: Uuid) -> Result<bool> {
    let row: (Option<OffsetDateTime>,) =
        sqlx::query_as("select cancel_requested_at from ai_runs where id = $1")
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or((None,));
    Ok(row.0.is_some())
}

/// Where a resumed run should pick up: the first step that is not `completed`.
///
/// `None` means every step completed, and the caller must refuse the resume rather than start a
/// fresh loop on a finished run. `Some(step_no)` for a step whose status is `running` is the
/// ambiguous case: the tool may already have executed, so the runtime reports it for a person
/// instead of running it again.
pub async fn resume_point(pool: &PgPool, run_id: Uuid) -> Result<Option<i32>> {
    let row: (Option<i32>,) = sqlx::query_as(
        "select min(step_no) from ai_run_steps where run_id = $1 and status <> 'completed'",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Count a run's steps in one state — the acceptance boxes that talk about "how many".
pub async fn count_steps(pool: &PgPool, run_id: Uuid, status: StepStatus) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from ai_run_steps where run_id = $1 and status = $2",
    )
    .bind(run_id)
    .bind(status.as_str())
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Append a loop event to the run's trace.
///
/// The event is stored as a `note` step rather than in a separate table, so the trace the panel
/// renders and the rows a replay assertion reads are the same rows — an event stream that lives
/// somewhere else is an event stream that eventually disagrees with what the user saw.
pub async fn append_event(pool: &PgPool, run_id: Uuid, event: &AgentEvent) -> Result<i32> {
    let kind = match event {
        AgentEvent::StepStarted { .. } => StepKind::Note,
        AgentEvent::Error { .. } => StepKind::Error,
        _ => StepKind::Note,
    };
    let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    begin_step(pool, run_id, kind, None, Some(&payload)).await
}

/// The redacted form of a tool call, as the trace stores it.
///
/// A transcript is read by people who are not the agent's author, and an argument named `token`
/// or `password` in a trace is a secret in a screen that has no reason to hold one. The key set
/// is matched on the *name*, because that is the only thing available before the call runs.
#[must_use]
pub fn redact_arguments(arguments: &serde_json::Value) -> serde_json::Value {
    const SECRETISH: [&str; 6] = ["token", "secret", "password", "apikey", "api_key", "authorization"];
    match arguments {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let lower = key.to_ascii_lowercase();
                if SECRETISH.iter().any(|needle| lower.contains(needle)) {
                    out.insert(key.clone(), serde_json::Value::String("[redacted]".to_owned()));
                } else {
                    out.insert(key.clone(), value.clone());
                }
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A step's call arguments, redacted, ready to write.
#[must_use]
pub fn call_arguments(call: &ToolCall) -> serde_json::Value {
    redact_arguments(&call.arguments)
}

/// Refuse a goal the run cannot be started with.
///
/// The column already bounds it, so this exists to turn a constraint violation into a message
/// with the limit in it — the API puts that message under the goal field, and a 500 carrying a
/// constraint name is not something a person can act on.
pub fn validate_goal(goal: &str) -> Result<()> {
    if goal.trim().is_empty() {
        return Err(AiHubError::InvalidRun("the goal is empty".to_owned()));
    }
    if goal.chars().count() > MAX_GOAL_CHARS {
        return Err(AiHubError::InvalidRun(format!(
            "the goal is {} characters; the limit is {MAX_GOAL_CHARS}",
            goal.chars().count()
        )));
    }
    Ok(())
}
