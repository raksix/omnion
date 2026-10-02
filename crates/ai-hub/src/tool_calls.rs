//! The call log and the two queries the execution path owes it (REQ-100, slice 3).
//!
//! `ai_tool_calls` was migrated in `0173` and this file is its first writer. It exists apart from
//! `registry.rs` because the two answer different questions: the registry is the operator's copy
//! of compiled code, this is the record of what actually ran. Keeping them apart is what makes
//! "the usage counts equal the aggregation of `ai_tool_calls`" a checkable claim — the aggregate
//! (`registry::usage_over`) and the rows it aggregates cannot drift, because they live in
//! different files and one of them is a pure `sum` over the other.
//!
//! # The two rules this file exists to hold
//!
//! 1. **One row per call, always.** A refusal is a row as much as a success is, and the request
//!    says so: "the denial is recorded in `ai_tool_calls` with `status = denied` plus an
//!    `ai.tool.denied` event". A log that only records what *worked* cannot answer "is this agent
//!    probing for a tool it was refused?" — which is the whole point of the `denied` status and
//!    the reason the table is append-only rather than a counter.
//!
//! 2. **A cap is counted by rows, not by an in-memory counter.** [`count_in_run`] is
//!    `count(*) … where run_id = $1 and tool_key = $2`, and it is read *before* the call rather
//!    than after. A counter lives in the loop's stack and a resumed run arrives with a fresh one,
//!    so a run that is parked, resumed and parked again gets a second allowance each time — the
//!    cap is a money limit, and a money limit that resets on resume is not a limit. The row count
//!    survives a process restart because the rows do.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;

/// The five outcomes a call can have. Closed on purpose — the migration's check constraint
/// refuses a sixth, and a sixth would break the `status` index the error-rate column relies on.
///
/// `Denied` and `Limited` are *refusals*, not failures: nothing ran, so nothing could be broken.
/// Keeping them apart from `Failed` is what lets "error %" mean "the tool broke" instead of
/// "the agent asked for something it may not have".
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum CallStatus {
    /// The tool ran.
    Ok,
    /// Nothing ran: the registry, the identity, the agent's list or the caller's permissions
    /// refused it.
    Denied,
    /// The tool ran and reported a failure.
    Failed,
    /// The tool ran and did not finish inside `ai_tools.timeout_ms`.
    Timeout,
    /// Nothing ran: the per-run call cap was already reached.
    Limited,
}

impl CallStatus {
    /// The value stored in `ai_tool_calls.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Limited => "limited",
        }
    }

    /// Whether nothing ran. A refused call leaves no side effect, and the two questions — "did it
    /// work" and "did it run" — are different.
    #[must_use]
    pub fn ran(self) -> bool {
        matches!(self, Self::Ok | Self::Failed | Self::Timeout)
    }

    /// Whether this outcome is a refusal the operator should be alerted about.
    ///
    /// `Limited` is deliberately *not* here: a model that keeps calling one tool until the cap
    /// bites is normal loop behaviour the `loop_detected` guard already reports, and an
    /// operator paged for it learns nothing.
    #[must_use]
    pub fn is_alerting(self) -> bool {
        matches!(self, Self::Denied)
    }
}

impl std::fmt::Display for CallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a call log row needs, minus what the store can derive.
///
/// `duration_ms` is `None` for a refusal that never started a clock, and `args_bytes` /
/// `result_bytes` are sizes rather than contents: the table records that a 40 kB page came back,
/// not the page. The request says arguments are "redacted in the trace for tools that can carry
/// user text" and the log is read by every operator on the tenant, so a log row is a fact about
/// a call rather than a copy of it.
#[derive(Debug, Clone)]
pub struct NewCall {
    pub organization_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub step_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub identity_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    /// The key the model named, even when no such tool exists — `tool_unknown` is the one
    /// refusal where the tool is not in the registry at all, and dropping the key would make
    /// that row unqueryable.
    pub tool_key: String,
    pub status: CallStatus,
    /// A stable machine code (`tool_denied`, `permission_denied`, `tool_timeout`, …), or `None`
    /// for a success. The request's events carry it, and the tool detail's "Error code" column
    /// is this column read back.
    pub error_code: Option<String>,
    pub duration_ms: Option<i32>,
    pub args_bytes: Option<i32>,
    pub result_bytes: Option<i32>,
}

impl NewCall {
    /// A call with only what every row carries, for the paths that have nothing else yet.
    #[must_use]
    pub fn for_tool(tool_key: impl Into<String>, status: CallStatus) -> Self {
        Self {
            organization_id: None,
            site_id: None,
            run_id: None,
            step_id: None,
            agent_id: None,
            identity_id: None,
            user_id: None,
            tool_key: tool_key.into(),
            status,
            error_code: None,
            duration_ms: None,
            args_bytes: None,
            result_bytes: None,
        }
    }

    /// Attach the run the call belonged to.
    #[must_use]
    pub fn with_run(mut self, organization_id: Uuid, run_id: Uuid, agent_id: Uuid) -> Self {
        self.organization_id = Some(organization_id);
        self.run_id = Some(run_id);
        self.agent_id = Some(agent_id);
        self
    }

    /// Attach the step row the loop wrote before the tool ran.
    #[must_use]
    pub fn with_step(mut self, step_id: Uuid) -> Self {
        self.step_id = Some(step_id);
        self
    }

    /// Attach the identity whose grants were consulted.
    #[must_use]
    pub fn with_identity(mut self, identity_id: Uuid) -> Self {
        self.identity_id = Some(identity_id);
        self
    }

    /// Attach the human behind the run, for a call started from the panel.
    #[must_use]
    pub fn with_user(mut self, user_id: Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Attach the site the run was about.
    #[must_use]
    pub fn with_site(mut self, site_id: Uuid) -> Self {
        self.site_id = Some(site_id);
        self
    }

    /// Attach a stable refusal code.
    #[must_use]
    pub fn with_error(mut self, code: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self
    }

    /// Attach the wall-clock cost of the call.
    #[must_use]
    pub fn with_duration(mut self, duration_ms: i64) -> Self {
        self.duration_ms = Some(i32::try_from(duration_ms).unwrap_or(i32::MAX));
        self
    }

    /// Attach the *size* of the arguments, never the arguments.
    #[must_use]
    pub fn with_args_size(mut self, bytes: usize) -> Self {
        self.args_bytes = Some(i32::try_from(bytes).unwrap_or(i32::MAX));
        self
    }

    /// Attach the *size* of the result, never the result.
    #[must_use]
    pub fn with_result_size(mut self, bytes: usize) -> Self {
        self.result_bytes = Some(i32::try_from(bytes).unwrap_or(i32::MAX));
        self
    }

    /// Attach the step only when the loop wrote one.
    ///
    /// A `None` step is a real state — a call from a workflow node or the internal SDK has no run
    /// step — and the migration's `on delete set null` is what lets such a row survive the run it
    /// belonged to being pruned. Writing the option straight through would have been the same
    /// thing; the named method exists so the three optional columns on this row are attached the
    /// same way in every call site rather than by whichever builder the author found first.
    #[must_use]
    pub fn with_step_opt(mut self, step_id: Option<Uuid>) -> Self {
        self.step_id = step_id;
        self
    }

    /// Attach the user only when the run has one.
    #[must_use]
    pub fn with_user_opt(mut self, user_id: Option<Uuid>) -> Self {
        self.user_id = user_id;
        self
    }

    /// Attach the site only when the run names one.
    #[must_use]
    pub fn with_site_opt(mut self, site_id: Option<Uuid>) -> Self {
        self.site_id = site_id;
        self
    }
}

/// The byte count of a JSON value, for the two size columns.
///
/// `serde_json::to_vec` is the honest measure — it is the number of bytes that went over the wire
/// to the provider or into the transcript — and a `len()` on a `Value` counts structure rather
/// than content, so a payload of 200 empty strings would read as 200 bytes.
#[must_use]
pub fn json_bytes(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// Append one row to the call log and return its id.
///
/// **Never fails soft.** The request treats the log as part of the call rather than a report on
/// it: "a tool call writes exactly one `ai_tool_calls` row and one `audit_log` row", and a
/// writer that swallowed a database error would make a call that ran with no evidence of it.
#[must_use]
pub async fn record(pool: &PgPool, call: NewCall) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "insert into ai_tool_calls (organization_id, site_id, run_id, step_id, agent_id, \
         identity_id, user_id, tool_key, status, error_code, duration_ms, args_bytes, \
         result_bytes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) returning id",
    )
    .bind(call.organization_id)
    .bind(call.site_id)
    .bind(call.run_id)
    .bind(call.step_id)
    .bind(call.agent_id)
    .bind(call.identity_id)
    .bind(call.user_id)
    .bind(&call.tool_key)
    .bind(call.status.as_str())
    .bind(call.error_code.as_deref())
    .bind(call.duration_ms)
    .bind(call.args_bytes)
    .bind(call.result_bytes)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// How many times this run has already called this tool, counted from the rows.
///
/// Read *before* the call, which is what makes the cap mean what it says across a resume: a run
/// that parked on an approval and was re-driven arrives with its previous rows still here, so it
/// does not get a second allowance. A counter in the loop's stack would.
///
/// `tool_key` rather than "any tool" because `ai_tools.max_calls_per_run` is a per-tool column:
/// a cap of 20 on each of three tools is 60 calls, and a per-run total would make the third
/// tool's limit depend on which two the model happened to use first.
pub async fn count_in_run(pool: &PgPool, run_id: Uuid, tool_key: &str) -> Result<i64> {
    let used: i64 = sqlx::query_scalar(
        "select count(*) from ai_tool_calls where run_id = $1 and tool_key = $2",
    )
    .bind(run_id)
    .bind(tool_key)
    .fetch_one(pool)
    .await?;
    Ok(used)
}

/// The calls for one run, newest first, with their outcomes — the run detail's tool section.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RunCall {
    pub id: i64,
    pub tool_key: String,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: Option<i32>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// One run's calls. Scoped by `run_id` *and* organization, because the run detail is reachable
/// from a panel that has `ai.agents.read` and an unscoped read is a cross-tenant history.
pub async fn calls_for_run(
    pool: &PgPool,
    organization_id: Uuid,
    run_id: Uuid,
    limit: i64,
) -> Result<Vec<RunCall>> {
    let sql = "select id, tool_key, status, error_code, duration_ms, created_at \
               from ai_tool_calls \
               where run_id = $1 and organization_id = $2 \
               order by created_at desc, id desc \
               limit $3";
    Ok(sqlx::query_as::<_, RunCall>(sql)
        .bind(run_id)
        .bind(organization_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(pool)
        .await?)
}

/// The refusal counts for one run, by reason — the trace's "why did it keep trying" answer.
pub async fn denials_for_run(
    pool: &PgPool,
    run_id: Uuid,
) -> Result<BTreeMap<String, i64>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        tool_key: String,
        error_code: Option<String>,
        hits: i64,
    }
    // `count(*)` returns `bigint`, and so does `sum` — the walk in REQ-100's first slice already
    // paid for learning that the quota query decodes `numeric` while a plain `count` decodes
    // `int8`, so the two are separated here deliberately.
    let rows: Vec<Row> = sqlx::query_as(
        "select tool_key, error_code, count(*) as hits from ai_tool_calls \
         where run_id = $1 and status = 'denied' \
         group by tool_key, error_code",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let label = match row.error_code {
                Some(code) => format!("{}: {code}", row.tool_key),
                None => row.tool_key,
            };
            (label, row.hits)
        })
        .collect())
}

/// How old a call row may be before the runner drops it, in days.
///
/// 180, from the request. The `ai_tool_calls` table is the aggregation source for the registry's
/// usage columns, so its retention is a *product* decision rather than housekeeping: at 180 days
/// a tool's "Calls 30 d" column is exact and its year-over-year is absent, which is what the
/// screens ask for. `audit_log` is the permanent record and this never touches it.
pub const RETENTION_DAYS: i64 = 180;

/// Drop call rows older than [`RETENTION_DAYS`], and only those.
///
/// `audit_log` is deliberately absent: the request says it "is append-only and never pruned", and
/// an agent's `audit_log` row is the permanent record of what it did. Deleting the call log's
/// older half removes a duplicate, never the original.
pub async fn prune(pool: &PgPool) -> Result<u64> {
    // The `::int` cast is load-bearing for the same reason REQ-098's pruner carries one:
    // `make_interval(days => int)` does not narrow a `bigint` parameter, and without it the
    // statement fails and the sweep silently deletes nothing.
    let deleted = sqlx::query(
        "delete from ai_tool_calls \
         where created_at < now() - make_interval(days => $1::int)",
    )
    .bind(RETENTION_DAYS)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_status_reads_back_as_the_wire_value_the_constraint_accepts() {
        // The five values the migration's check constraint knows. A sixth would fail at
        // INSERT time in production and in no test, because every test in this module builds
        // its rows in Rust — so the list is pinned here instead.
        assert_eq!(CallStatus::Ok.as_str(), "ok");
        assert_eq!(CallStatus::Denied.as_str(), "denied");
        assert_eq!(CallStatus::Failed.as_str(), "failed");
        assert_eq!(CallStatus::Timeout.as_str(), "timeout");
        assert_eq!(CallStatus::Limited.as_str(), "limited");
    }

    #[test]
    fn a_refusal_never_claims_it_ran() {
        // The distinction the whole table rests on: a denied call must leave no side effect, and
        // "ran" is how a reader checks that afterwards.
        assert!(!CallStatus::Denied.ran());
        assert!(!CallStatus::Limited.ran());
        assert!(CallStatus::Ok.ran());
        assert!(CallStatus::Failed.ran());
        assert!(CallStatus::Timeout.ran());
    }

    #[test]
    fn only_a_denial_is_an_alert() {
        // A cap that pages an operator teaches them to ignore pages.
        assert!(CallStatus::Denied.is_alerting());
        assert!(!CallStatus::Limited.is_alerting());
        assert!(!CallStatus::Failed.is_alerting());
        assert!(!CallStatus::Timeout.is_alerting());
        assert!(!CallStatus::Ok.is_alerting());
    }

    #[test]
    fn a_size_column_measures_bytes_and_not_structure() {
        // `Value::len()` counts the members of an object, so `{"a":1,"b":2}` would read as 2
        // bytes and the log's sizes would be meaningless. Asserted against the real encoding.
        let value = json!({"a": 1, "b": 2});
        assert!(json_bytes(&value) > 2);
        assert_eq!(json_bytes(&value), serde_json::to_vec(&value).unwrap().len());
    }

    #[test]
    fn a_call_carries_only_the_tool_key_until_told_more() {
        let call = NewCall::for_tool("content.search", CallStatus::Ok);
        assert_eq!(call.tool_key, "content.search");
        assert!(call.run_id.is_none());
        assert!(call.duration_ms.is_none());
        assert!(call.error_code.is_none());
    }

    #[test]
    fn a_duration_longer_than_a_column_can_hold_saturates_instead_of_wrapping() {
        // `i32::try_from` on a `u64` millisecond counter that had been running for 25 days would
        // be a negative duration, and the migration's `duration_ms >= 0` check would refuse the
        // row — losing the evidence of a call that really happened.
        let call = NewCall::for_tool("logs.read", CallStatus::Ok).with_duration(9_000_000_000);
        assert_eq!(call.duration_ms, Some(i32::MAX));
    }

    #[test]
    fn the_builders_attach_without_erasing_each_other() {
        let run = Uuid::new_v4();
        let step = Uuid::new_v4();
        let identity = Uuid::new_v4();
        let call = NewCall::for_tool("media.upload", CallStatus::Failed)
            .with_run(run, run, Uuid::new_v4())
            .with_step(step)
            .with_identity(identity)
            .with_error("tool_failed")
            .with_args_size(12)
            .with_result_size(34);
        assert_eq!(call.run_id, Some(run));
        assert_eq!(call.step_id, Some(step));
        assert_eq!(call.identity_id, Some(identity));
        assert_eq!(call.error_code.as_deref(), Some("tool_failed"));
        assert_eq!(call.args_bytes, Some(12));
        assert_eq!(call.result_bytes, Some(34));
    }
}
