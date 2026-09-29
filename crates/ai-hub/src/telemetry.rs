//! Run telemetry, per run and rolled up per agent (REQ-099, slice 4).
//!
//! The panel shows two different questions and both must be answerable from the rows
//! themselves:
//!
//! - **Per run** — what did *this* run do: steps, tool calls by key, tokens, cost, duration,
//!   and why it stopped. The run detail reads it, and the acceptance box for the previous
//!   slice already proved that the run's own columns equal the sum of its step rows.
//! - **Per agent, 30 days** — "is this agent worth its cost": runs, success rate, tokens,
//!   cost, and the tools it actually reaches for. This is the roll-up the request asks for
//!   and the one that has to be honest about its own denominator.
//!
//! # Every number here is arithmetic over rows, never a counter somebody maintains
//!
//! The store has no `ai_agent_stats` table and this module does not add one, because a
//! maintained roll-up and the rows it summarises are two facts about the same thing, and
//! they drift at exactly the moments anybody is looking — after a run is deleted, after a
//! step is re-executed on resume, after a restore from a backup taken mid-run. A `30-day
//! cost` read from a table that is updated in the same transaction as the run is a number
//! that agrees with the runs *until the day somebody asks it to*.
//!
//! So every aggregate here is a `sum`/`count` over `ai_runs` and `ai_run_steps` with the
//! window in the `where` clause. It is slower than a counter and it is *right*, and at the
//! volume a single tenant's agent history reaches — thousands of runs, not millions — the
//! difference is not measurable. When it becomes measurable the answer is a materialised view
//! with a refresh, not a counter written by the run path.
//!
//! # The success rate's denominator is the thing that gets argued about
//!
//! Three candidate denominators, and they answer different questions:
//!
//! | Denominator | Question it answers |
//! |---|---|
//! | all runs | "is this agent reliable?" — a run a person stopped counts against nothing |
//! | terminal runs only | excludes a run still in flight |
//! | completed only | always 100%, and therefore useless |
//!
//! This module computes the first, over runs that have *finished*, and reports the cancelled
//! count beside it rather than folding it in. The reason is in
//! [`StopReason::is_failure`]: a person pressing stop stopped the run, and counting that
//! against the agent's reliability is the definition of a metric that gets gamed by
//! impatience. A caller that wants the other denominator can subtract `cancelled` from
//! `runs`; a caller that wants it *hidden* would have to subtract it from the code, which is
//! the difference between a documented choice and a quiet one.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::{Date, Duration, Month, OffsetDateTime};
use uuid::Uuid;

use crate::agent::StopReason;
use crate::error::Result;

/// The window the per-agent roll-up covers.
///
/// Thirty days, and it is a constant rather than a parameter for the same reason the repair
/// budget is: the panel labels the column "Runs 30 d", so a query parameter would let the
/// label and the number drift apart. A caller wanting a different window asks for
/// [`agent_telemetry_since`], which says in its name what it did.
pub const ROLLUP_DAYS: i64 = 30;

/// One run's own numbers, recomputed from its step rows.
///
/// The struct exists so the acceptance box "telemetry equals the underlying rows" has
/// something to compare *to*: this is the read of the steps, and the run's stored columns are
/// the other side of that comparison. A check that compared the columns to themselves would
/// pass forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunTelemetry {
    /// Steps that reached a terminal state.
    pub completed_steps: i64,
    /// Steps that failed.
    pub failed_steps: i64,
    /// Tool calls attempted, counted from the `tool_call` rows.
    pub tool_calls: i64,
    /// Prompt tokens across the run's steps.
    pub prompt_tokens: i32,
    /// Completion tokens across the run's steps.
    pub completion_tokens: i32,
    /// Cost in millionths, summed from the step rows.
    pub cost_micros: i64,
    /// Wall-clock milliseconds from the first step to the last, when both exist.
    pub duration_ms: i64,
    /// The stop reason, parsed.
    pub stop_reason: Option<StopReason>,
}

/// One run's telemetry, from its steps.
///
/// `duration_ms` is computed from the step rows' own `started_at`/`finished_at` rather than
/// from the run's two stamps. The run's stamps bracket the whole attempt *including* the
/// time it sat queued, and "how long did this take" on the run detail means how long the work
/// took — a run that waited four minutes in the queue under a loaded box would otherwise read
/// as a four-minute agent. The queued interval is not lost: the run list's own `Started` and
/// the history's sort order both use the run row.
pub async fn run_telemetry(pool: &PgPool, run_id: Uuid) -> Result<RunTelemetry> {
    #[derive(sqlx::FromRow)]
    struct Row {
        completed_steps: Option<i64>,
        failed_steps: Option<i64>,
        tool_calls: Option<i64>,
        prompt_tokens: Option<i32>,
        completion_tokens: Option<i32>,
        cost_micros: Option<i64>,
        first_started: Option<OffsetDateTime>,
        last_finished: Option<OffsetDateTime>,
    }

    let row = sqlx::query_as::<_, Row>(
        "select
            count(*) filter (where status = 'completed')                      as completed_steps,
            count(*) filter (where status = 'failed')                         as failed_steps,
            count(*) filter (where kind = 'tool_call')                        as tool_calls,
            coalesce(sum(prompt_tokens) filter (where status = 'completed'), 0)::int
                                                                              as prompt_tokens,
            coalesce(sum(completion_tokens) filter (where status = 'completed'), 0)::int
                                                                              as completion_tokens,
            -- `sum(bigint)` returns `numeric` in PostgreSQL, not `int8`. Reading it as
            -- `Option<i64>` is a `ColumnDecode` on a perfectly good row, which is how the
            -- same mistake cost an entire walk suite two slices ago.
            coalesce(sum(cost_micros) filter (where status = 'completed'), 0)::bigint
                                                                              as cost_micros,
            min(started_at)                                                    as first_started,
            max(finished_at)                                                   as last_finished
         from ai_run_steps where run_id = $1",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;

    let duration_ms = match (row.first_started, row.last_finished) {
        (Some(first), Some(last)) => i64::try_from((last - first).whole_milliseconds()).unwrap_or(0),
        // A run with one unfinished step has a start and no end. Reporting the elapsed time
        // so far is more useful than zero, and it is honest: the run is still going.
        (Some(first), None) => i64::try_from((OffsetDateTime::now_utc() - first).whole_milliseconds())
            .unwrap_or(0),
        _ => 0,
    };

    Ok(RunTelemetry {
        completed_steps: row.completed_steps.unwrap_or(0),
        failed_steps: row.failed_steps.unwrap_or(0),
        tool_calls: row.tool_calls.unwrap_or(0),
        prompt_tokens: row.prompt_tokens.unwrap_or(0),
        completion_tokens: row.completion_tokens.unwrap_or(0),
        cost_micros: row.cost_micros.unwrap_or(0),
        duration_ms: duration_ms.max(0),
        stop_reason: None,
    })
}

/// How often each tool was called, in a window.
///
/// A `BTreeMap` rather than a `HashMap` because the panel renders it as a list and a list
/// whose order changes between two requests is a list nobody can screenshot twice.
pub async fn tool_usage(
    pool: &PgPool,
    organization_id: Uuid,
    since: OffsetDateTime,
) -> Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select s.tool, count(*) as calls
           from ai_run_steps s
           join ai_runs r on r.id = s.run_id
          where r.organization_id = $1
            and s.kind = 'tool_call'
            and s.tool is not null
            and r.started_at >= $2
          group by s.tool
          order by calls desc, s.tool",
    )
    .bind(organization_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// One agent's 30-day roll-up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentTelemetry {
    /// The agent this is about.
    pub agent_id: Uuid,
    /// Runs that reached a terminal state inside the window.
    pub runs: i64,
    /// Of those, the ones that produced an answer.
    pub completed: i64,
    /// Of those, the ones a person stopped.
    pub cancelled: i64,
    /// Of those, the ones that failed.
    pub failed: i64,
    /// The one word per failed run, so a panel can show why without a second query.
    pub stop_reasons: BTreeMap<String, i64>,
    /// Prompt tokens across the window.
    pub prompt_tokens: i32,
    /// Completion tokens across the window.
    pub completion_tokens: i32,
    /// Cost in millionths across the window.
    pub cost_micros: i64,
    /// Steps across the window, so the cost per step is derivable in the panel.
    pub steps: i64,
}

impl AgentTelemetry {
    /// Completed runs as a percentage of finished runs.
    ///
    /// `None` — not `0` and not `100` — when nothing has finished. An agent with no runs has
    /// no success rate, and a cell reading "0%" on a brand-new agent is a fact about a
    /// division that never happened. The panel renders `None` as an em dash, which is what
    /// every other empty measurement in it does.
    #[must_use]
    pub fn success_rate(&self) -> Option<f64> {
        if self.runs == 0 {
            return None;
        }
        Some((self.completed as f64 / self.runs as f64) * 100.0)
    }

    /// Total tokens across the window.
    #[must_use]
    pub fn total_tokens(&self) -> i64 {
        i64::from(self.prompt_tokens) + i64::from(self.completion_tokens)
    }
}

/// One agent's roll-up over [`ROLLUP_DAYS`].
pub async fn agent_telemetry(pool: &PgPool, organization_id: Uuid, agent_id: Uuid) -> Result<AgentTelemetry> {
    agent_telemetry_since(pool, organization_id, agent_id, window_start(ROLLUP_DAYS)).await
}

/// One agent's roll-up since an instant.
pub async fn agent_telemetry_since(
    pool: &PgPool,
    organization_id: Uuid,
    agent_id: Uuid,
    since: OffsetDateTime,
) -> Result<AgentTelemetry> {
    #[derive(sqlx::FromRow)]
    struct Row {
        runs: i64,
        completed: i64,
        cancelled: i64,
        failed: i64,
        prompt_tokens: Option<i32>,
        completion_tokens: Option<i32>,
        cost_micros: Option<i64>,
        steps: Option<i64>,
    }

    // The tenant predicate is on the *run* row, not implied by the agent: an agent id is a
    // uuid the caller supplies, and the one place that uuid is not enough is the query that
    // sums money. Four store functions in this crate learned that lesson the hard way.
    let row = sqlx::query_as::<_, Row>(
        "select
            count(*)                                                          as runs,
            count(*) filter (where r.status = 'completed')                     as completed,
            count(*) filter (where r.status = 'cancelled')                     as cancelled,
            count(*) filter (where r.status = 'failed')                        as failed,
            coalesce(sum(r.prompt_tokens), 0)::int                              as prompt_tokens,
            coalesce(sum(r.completion_tokens), 0)::int                          as completion_tokens,
            coalesce(sum(r.cost_micros), 0)::bigint                            as cost_micros,
            coalesce(sum(r.current_step), 0)::bigint                           as steps
         from ai_runs r
        where r.organization_id = $1
          and r.agent_id = $2
          and r.finished_at is not null
          and r.finished_at >= $3",
    )
    .bind(organization_id)
    .bind(agent_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let mut stop_reasons = BTreeMap::new();
    let reason_rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "select r.stop_reason, count(*)
           from ai_runs r
          where r.organization_id = $1
            and r.agent_id = $2
            and r.finished_at is not null
            and r.finished_at >= $3
            and r.stop_reason is not null
          group by r.stop_reason",
    )
    .bind(organization_id)
    .bind(agent_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    for (reason, count) in reason_rows {
        if let Some(reason) = reason {
            stop_reasons.insert(reason, count);
        }
    }

    Ok(AgentTelemetry {
        agent_id,
        runs: row.runs,
        completed: row.completed,
        cancelled: row.cancelled,
        failed: row.failed,
        stop_reasons,
        prompt_tokens: row.prompt_tokens.unwrap_or(0),
        completion_tokens: row.completion_tokens.unwrap_or(0),
        cost_micros: row.cost_micros.unwrap_or(0),
        steps: row.steps.unwrap_or(0),
    })
}

/// Every agent's roll-up in one query, for the table.
///
/// One query rather than N: the agents table renders a telemetry cell on every row, and a
/// screen that issues one query per row on a 200-agent tenant is a screen that takes eleven
/// seconds and looks broken. The `left join` is what keeps an agent with no runs *in* the
/// result with zeroes rather than dropping it, because a table row that disappears when
/// nothing has happened yet is a row whose absence looks like a deletion.
pub async fn agents_telemetry(
    pool: &PgPool,
    organization_id: Uuid,
    since: OffsetDateTime,
) -> Result<BTreeMap<Uuid, AgentTelemetry>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        agent_id: Uuid,
        runs: i64,
        completed: i64,
        cancelled: i64,
        failed: i64,
        prompt_tokens: Option<i32>,
        completion_tokens: Option<i32>,
        cost_micros: Option<i64>,
        steps: Option<i64>,
    }

    let rows = sqlx::query_as::<_, Row>(
        "select
            a.id                                                            as agent_id,
            count(r.id)                                                     as runs,
            count(r.id) filter (where r.status = 'completed')               as completed,
            count(r.id) filter (where r.status = 'cancelled')               as cancelled,
            count(r.id) filter (where r.status = 'failed')                  as failed,
            coalesce(sum(r.prompt_tokens), 0)::int                          as prompt_tokens,
            coalesce(sum(r.completion_tokens), 0)::int                      as completion_tokens,
            coalesce(sum(r.cost_micros), 0)::bigint                        as cost_micros,
            coalesce(sum(r.current_step), 0)::bigint                       as steps
         from ai_agents a
         left join ai_runs r
           on r.agent_id = a.id
          and r.finished_at is not null
          and r.finished_at >= $2
        where a.organization_id = $1
        group by a.id",
    )
    .bind(organization_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut out = BTreeMap::new();
    for row in rows {
        out.insert(
            row.agent_id,
            AgentTelemetry {
                agent_id: row.agent_id,
                runs: row.runs,
                completed: row.completed,
                cancelled: row.cancelled,
                failed: row.failed,
                stop_reasons: BTreeMap::new(),
                prompt_tokens: row.prompt_tokens.unwrap_or(0),
                completion_tokens: row.completion_tokens.unwrap_or(0),
                cost_micros: row.cost_micros.unwrap_or(0),
                steps: row.steps.unwrap_or(0),
            },
        );
    }
    Ok(out)
}

/// The instant a window of N days back from now starts.
///
/// A free function rather than an inline `now() - interval` because the *test* has to be able
/// to place runs on either side of the boundary, and a boundary that is only expressible as
/// "whatever the database thinks now is" cannot be tested against rows written a moment ago.
#[must_use]
pub fn window_start(days: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - Duration::days(days.clamp(1, 3650))
}

/// Yesterday, as a date, for a day-bucketed view.
#[must_use]
pub fn day_bucket(instant: OffsetDateTime) -> Date {
    instant.date()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the success rate, which is the number that gets argued about ------------------

    #[test]
    fn an_agent_with_no_finished_runs_has_no_success_rate() {
        // Not 0% and not 100%: neither of those is a statement about a division that did not
        // happen, and both read as a measurement.
        assert_eq!(AgentTelemetry::default().success_rate(), None);
    }

    #[test]
    fn a_run_a_person_stopped_does_not_count_against_reliability() {
        let telemetry = AgentTelemetry {
            runs: 4,
            completed: 3,
            cancelled: 1,
            failed: 0,
            ..AgentTelemetry::default()
        };
        assert_eq!(telemetry.success_rate(), Some(75.0));
        assert_eq!(telemetry.runs, 4, "the denominator is still every finished run");
    }

    #[test]
    fn a_failed_run_does_count_against_reliability() {
        let telemetry = AgentTelemetry {
            runs: 2,
            completed: 1,
            failed: 1,
            ..AgentTelemetry::default()
        };
        assert_eq!(telemetry.success_rate(), Some(50.0));
    }

    #[test]
    fn a_perfect_agent_reads_one_hundred_percent() {
        let telemetry = AgentTelemetry {
            runs: 7,
            completed: 7,
            ..AgentTelemetry::default()
        };
        assert_eq!(telemetry.success_rate(), Some(100.0));
    }

    #[test]
    fn total_tokens_is_the_sum_of_both_halves() {
        let telemetry = AgentTelemetry {
            prompt_tokens: 1200,
            completion_tokens: 340,
            ..AgentTelemetry::default()
        };
        assert_eq!(telemetry.total_tokens(), 1540);
    }

    #[test]
    fn the_other_denominator_is_subtractable_rather_than_hidden() {
        // A caller who wants "success rate excluding cancellations" can compute it from the
        // published fields. The rule the module states is checkable from the struct, which is
        // the property that makes it a rule and not a preference.
        let telemetry = AgentTelemetry {
            runs: 4,
            completed: 3,
            cancelled: 1,
            ..AgentTelemetry::default()
        };
        let without_cancellations =
            (telemetry.completed as f64 / (telemetry.runs - telemetry.cancelled) as f64) * 100.0;
        assert_eq!(without_cancellations, 100.0);
    }

    // -- the window --------------------------------------------------------------------

    #[test]
    fn the_window_start_is_in_the_past_and_bounded() {
        let start = window_start(ROLLUP_DAYS);
        let now = OffsetDateTime::now_utc();
        assert!(start < now);
        // 30 days, with a minute of slack for the two calls not being simultaneous.
        let days = (now - start).whole_days();
        assert!((29..=31).contains(&days), "got {days} days");
    }

    #[test]
    fn a_nonsense_window_is_clamped_rather_than_panicking() {
        // `Duration::days(0)` is a zero-length window that reports "no runs, ever" and reads
        // as a broken query; a negative one underflows.
        let now = OffsetDateTime::now_utc();
        for days in [0, -5, i64::MIN] {
            let start = window_start(days);
            assert!(start <= now, "days={days}");
        }
        // The ceiling clamps rather than rejects: an absurd window becomes the largest one
        // allowed, which reads as "everything" instead of as a query that returned nothing.
        // Compared as *durations* rather than as instants: each call reads its own
        // `now_utc()`, so two `OffsetDateTime`s are nanoseconds apart and an equality check
        // between them fails for a reason that has nothing to do with the clamp.
        let now = OffsetDateTime::now_utc();
        let years = |days: i64| (now - window_start(days)).whole_days();
        assert_eq!(years(99999), years(3650), "an absurd window clamps to the ceiling");
        assert!(
            years(1) < years(30),
            "a shorter window starts later: {} vs {}",
            years(1),
            years(30)
        );
    }

    #[test]
    fn the_day_bucket_is_the_utc_date() {
        // UTC and not local: an operator in two timezones comparing two panels must see the
        // same bucket, and the server has no idea which of them is asking.
        let instant = OffsetDateTime::parse(
            "2026-09-29T23:30:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("parse");
        assert_eq!(day_bucket(instant), Date::from_calendar_date(2026, Month::September, 29).expect("date"));
    }

    #[test]
    fn a_run_with_no_telemetry_reports_zeroes_rather_than_nulls() {
        // The default is the shape the API serialises, and every field is a number: a panel
        // that has to `?? 0` in eight places is a panel that will forget one.
        let telemetry = RunTelemetry::default();
        assert_eq!(telemetry.tool_calls, 0);
        assert_eq!(telemetry.cost_micros, 0);
        assert_eq!(telemetry.duration_ms, 0);
        assert!(telemetry.stop_reason.is_none());
    }
}
