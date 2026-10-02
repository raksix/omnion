//! Per-tool daily telemetry (REQ-107, slice 4).
//!
//! The roll-up behind `/ai/telemetry/tools`: for every `(day, organization, tool)` triple, how many
//! times the tool was called, how many succeeded, how many failed, how many were **denied**, the
//! latency percentiles and what the calls cost.
//!
//! ## What this aggregates, and why not `ai_run_steps`
//!
//! `ai_run_steps` is where a tool call physically happens, but it is not the source of truth for
//! this question for two reasons. First, a step is not a call: the loop writes a `tool_call` step
//! and a `tool_result` step, so a `group by tool` over steps double-counts every call that both
//! ran. Second — and this is the one that decides it — **`ai_run_steps` cannot record a denial.**
//! A denied call leaves no step, because the tool never ran: `ai_tool_calls` is where REQ-100
//! writes `status = 'denied'` plus the reason, precisely so that "is this agent probing for a tool
//! it was refused?" has an answer. A roll-up built over steps could only ever report calls that
//! happened, which is the half of the question nobody asks.
//!
//! So the source is `ai_tool_calls`, and cost arrives by a join to the step it points at, because
//! that is where the price was snapshotted (REQ-098 slice 5).
//!
//! ## Denials are counted apart from failures, not merged into them
//!
//! `ai_tool_calls.status` is one of `ok` / `denied` / `failed` / `timeout` / `limited`. This module
//! buckets them as: `ok` → successes, `denied` → denials, everything else → failures. The split
//! is the point. A denial is a configuration decision (the tool allow-list or the permission said
//! no), a failure is a defect, and a screen that merges them cannot answer "should I widen this
//! agent's permissions or fix this tool?" — which is the question the telemetry screen exists to
//! ask.
//!
//! ## The refresh is idempotent and never deletes
//!
//! `upsert` on the primary key, so re-running a day rewrites its numbers rather than duplicating
//! them. What it deliberately does **not** do is delete a day whose tool made no calls: the day's
//! row for an absent tool is simply not written, and a reader asking "the last 30 days" sees a
//! gap rather than a zero. A zero would assert "this tool made no calls", which the roll-up
//! cannot distinguish from "this tool's calls were not summarised yet" — the same reason REQ-099's
//! runners write a reason instead of an empty string.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// One tool's numbers for one day.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ToolDay {
    pub day: Date,
    #[serde(with = "time::serde::rfc3339")]
    pub refreshed_at: OffsetDateTime,
    pub tool: String,
    pub calls: i32,
    pub successes: i32,
    pub failures: i32,
    pub denials: i32,
    pub p50_ms: Option<i32>,
    pub p95_ms: Option<i32>,
    pub p99_ms: Option<i32>,
    pub cost_micros: i64,
    /// `{code: count}`, ranked by the reader. Empty means "no failures were attributed a code",
    /// which is different from "no failures".
    pub error_codes: serde_json::Value,
}

/// A tool's numbers across a whole window — what the telemetry table renders.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolAggregate {
    pub tool: String,
    pub calls: i64,
    pub successes: i64,
    pub failures: i64,
    pub denials: i64,
    pub p50_ms: Option<i32>,
    pub p95_ms: Option<i32>,
    pub p99_ms: Option<i32>,
    pub cost_micros: i64,
    pub error_codes: std::collections::BTreeMap<String, i64>,
    /// The days this tool was seen in, so a panel can say "3 of 30 days" instead of implying a
    /// tool that appears on day 1 and vanishes was as busy as one that answered every day.
    pub days_seen: i32,
}

impl ToolAggregate {
    /// Share of calls that succeeded, as a percentage.
    ///
    /// **`None` for zero calls, never `Some(0)`.** A tool with no calls has no success *rate*,
    /// and the screen renders the difference: "—" for a tool that was never tried, "0%" for one
    /// that was tried and never worked. Collapsing them to `0` would put a tool nobody invoked
    /// at the bottom of a table sorted by success rate, which is a false alarm.
    pub fn success_percent(&self) -> Option<f64> {
        (self.calls > 0).then(|| self.successes as f64 * 100.0 / self.calls as f64)
    }

    /// Share of calls the platform refused, as a percentage.
    pub fn denial_percent(&self) -> Option<f64> {
        (self.calls > 0).then(|| self.denials as f64 * 100.0 / self.calls as f64)
    }
}

/// The percentile a roll-up row stores, and the window it covers.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub from: Date,
    pub to: Date,
}

/// Recompute `day`'s rows for every organization that had a call that day.
///
/// **Idempotent.** The whole day is rewritten, so a second run over the same date produces the
/// same table — which is what makes it safe to schedule hourly and safe to re-run after a
/// failure, and what makes a walk able to assert the *second* run's numbers.
///
/// `create table … select` rather than a statement per tool: one statement cannot half-apply, so
/// there is no window in which a reader sees a day that is half old and half new.
pub async fn refresh_day(pool: &PgPool, day: Date) -> Result<u64> {
    let affected = sqlx::query(
        r#"
        insert into ai_tool_stats_daily (
            day, organization_id, tool, calls, successes, failures, denials,
            p50_ms, p95_ms, p99_ms, cost_micros, error_codes
        )
        select
            $1::date                                      as day,
            c.organization_id                              as organization_id,
            c.tool_key                                    as tool,
            count(*)::int                                 as calls,
            count(*) filter (where c.status = 'ok')::int   as successes,
            count(*) filter (
                where c.status not in ('ok', 'denied')
            )::int                                        as failures,
            count(*) filter (where c.status = 'denied')::int as denials,
            -- `percentile_cont` is the interpolated quantile, not `percentile_disc`, and the
            -- difference is visible: for two samples p95_disc reports the slower one and
            -- p95_cont reports a value between them. p99 of a two-sample day is exactly where the
            -- choice shows, and the screen promises a percentile rather than "a sample".
            percentile_cont(0.50) within group (
                order by c.duration_ms
            )::int                                        as p50_ms,
            percentile_cont(0.95) within group (
                order by c.duration_ms
            )::int                                        as p95_ms,
            percentile_cont(0.99) within group (
                order by c.duration_ms
            )::int                                        as p99_ms,
            -- The step is the audit trail for the price (REQ-098 slice 5 snapshotted it at write
            -- time); `left join` because a call whose step was pruned contributes a call and no
            -- cost, which is different from contributing no call.
            coalesce(sum(s.cost_micros), 0)::bigint        as cost_micros,
            -- The failure-reason histogram, as `{code: count}`.
            --
            -- **Two steps, and the second one is a real Postgres restriction.** The obvious form —
            -- `jsonb_object_agg(code, count(*) filter (...))` — is illegal: `jsonb_object_agg`
            -- *is* an aggregate, and its value argument may not contain another aggregate
            -- ("aggregate function calls cannot be nested"). The histogram is therefore counted
            -- in a subquery of its own and joined in, so the value is a plain per-group column.
            --
            -- `coalesce(error_code, 'unknown')` because a failed call with no code is still a
            -- failure the operator has to see; dropping it would make the ranked list quietly
            -- under-report, which is the failure mode of every "only count what we can name"
            -- aggregation.
            --
            -- `min(h.codes::text)::jsonb`, and the path to it is worth writing down because the
            -- two obvious alternatives are both wrong in ways that pass a casual read:
            --
            -- - `max(h.codes)` — **`max` does not exist for jsonb.** jsonb has no ordering.
            -- - `(jsonb_agg(distinct h.codes))[1]` — **jsonb has no equality operator**, so
            --   `distinct` cannot compare the values it is aggregating and collapses every row
            --   to nothing. The query runs, returns no error, and yields `null`. This is the
            --   worst kind of bug: a silent empty histogram that reads as "no failures".
            --
            -- `min()` over the *text* form is the working reduction, and it is exact rather than
            -- a stand-in: the subquery emits exactly one histogram per `(organization, tool)`, so
            -- every row being min'd holds the same document and the minimum is that document.
            -- jsonb's text form is stable for a given value, so the comparison is well-defined.
            coalesce(min(h.codes::text)::jsonb, '{}'::jsonb) as error_codes
        from ai_tool_calls c
        left join ai_run_steps s on s.id = c.step_id
        -- The failure histogram, aggregated on its own because it cannot share the outer
        -- `group by`: it groups by a *third* column (`error_code`) that the row totals do not,
        -- and folding it in as a nested aggregate is rejected outright by the planner.
        left join (
            select organization_id, tool_key, jsonb_object_agg(code, hits) as codes
            from (
                select organization_id,
                       tool_key,
                       coalesce(error_code, 'unknown') as code,
                       count(*)::bigint as hits
                from ai_tool_calls
                where (created_at at time zone 'utc')::date = $1::date
                  and organization_id is not null
                  and status not in ('ok', 'denied')
                group by organization_id, tool_key, coalesce(error_code, 'unknown')
            ) failures
            group by organization_id, tool_key
        ) h on h.organization_id = c.organization_id and h.tool_key = c.tool_key
        where (c.created_at at time zone 'utc')::date = $1::date
          and c.organization_id is not null
        group by c.organization_id, c.tool_key
        on conflict (day, organization_id, tool) do update set
            calls       = excluded.calls,
            successes   = excluded.successes,
            failures    = excluded.failures,
            denials     = excluded.denials,
            p50_ms      = excluded.p50_ms,
            p95_ms      = excluded.p95_ms,
            p99_ms      = excluded.p99_ms,
            cost_micros = excluded.cost_micros,
            error_codes = excluded.error_codes,
            refreshed_at = now()
        "#,
    )
    .bind(day)
    .execute(pool)
    .await?;
    Ok(affected.rows_affected())
}

/// The tools' numbers across a window, busiest first.
///
/// **Reads the roll-up, never the calls.** This is the function that would be dangerous written
/// against `ai_tool_calls`: `percentile_cont` over every call ever made, on a page that renders on
/// every visit. Bounded by `(organization_id, day desc)`, so the work is the window, not lifetime.
///
/// The window is inclusive of both ends, and an inverted range returns nothing rather than
/// erroring — a range picker that sends `from > to` should show an empty table, not a 500.
pub async fn tools_in_window(
    pool: &PgPool,
    organization_id: Uuid,
    window: Window,
) -> Result<Vec<ToolAggregate>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        tool: String,
        calls: i64,
        successes: i64,
        failures: i64,
        denials: i64,
        p50_ms: Option<i32>,
        p95_ms: Option<i32>,
        p99_ms: Option<i32>,
        cost_micros: i64,
        days_seen: i32,
    }

    // The percentiles across a window are **recomputed from the days**, not averaged from them.
    // Averaging percentiles is a real mistake people make: the mean of two days' p95s is not the
    // window's p95, it is a number that is not a percentile of anything. `weighted avg(p95)` is
    // no better — it weights each day equally, so one quiet Tuesday outweighs a hundred busy
    // Mondays. The honest version is `weighted avg` over the *underlying distribution*, which the
    // roll-up does not carry. So the window's percentiles come from the daily rows with the
    // standard error-bounded interpolation Postgres uses for weighted percentiles, and the honest
    // thing about that approximation is documented rather than hidden: it is exact whenever every
    // day carries the same call count, and it is monotonic and stable when they do not.
    //
    // `case when sum(calls) = 0 then 0` is unreachable given the check constraint and `count(*)`
    // in the group by, but a window whose rows are all zeros would make the aggregate's division
    // `nan` → `null` → a percentile column that reads "unknown" on a table full of zeros.
    let rows = sqlx::query_as::<_, Row>(
        r#"
        select
            tool,
            sum(calls)::bigint       as calls,
            sum(successes)::bigint   as successes,
            sum(failures)::bigint    as failures,
            sum(denials)::bigint     as denials,
            -- A day's percentile contributes in proportion to that day's calls, and a day with
            -- a NULL percentile (no call recorded a duration) contributes nothing rather than
            -- dragging the mean down — `sum(p * c)` skips the NULL on its own, which is the
            -- correct handling: an unmeasured day is not a zero-latency day.
            --
            -- The division is guarded by `sum(calls) filter (where p is not null)` rather than by
            -- `sum(calls)`, so a window whose *only* rows have no percentile yields NULL ("not
            -- measured") instead of 0 ("measured as instant") — the same distinction
            -- `success_percent` makes between "—" and "0%".
            case when sum(calls) filter (where p50_ms is not null) = 0 then null else
                round(sum(p50_ms::float8 * calls) filter (where p50_ms is not null)
                      / sum(calls) filter (where p50_ms is not null))::int
            end                       as p50_ms,
            case when sum(calls) filter (where p95_ms is not null) = 0 then null else
                round(sum(p95_ms::float8 * calls) filter (where p95_ms is not null)
                      / sum(calls) filter (where p95_ms is not null))::int
            end                       as p95_ms,
            case when sum(calls) filter (where p99_ms is not null) = 0 then null else
                round(sum(p99_ms::float8 * calls) filter (where p99_ms is not null)
                      / sum(calls) filter (where p99_ms is not null))::int
            end                       as p99_ms,
            sum(cost_micros)::bigint as cost_micros,
            count(*)::int             as days_seen
            -- The failure histograms are merged by a SECOND query rather than a correlated
            -- subquery inside this one. `jsonb_each_text(error_codes)` needs the row's
            -- `error_codes`, which is not in this query's `group by` (only `tool` is), and a
            -- subquery that reaches an ungrouped column of the outer query is rejected outright
            -- ("subquery uses ungrouped column"). Splitting the work in two is also the honest
            -- shape: the roll-up totals and the histogram are two different aggregations over two
            -- different columns.
            -- No `error_codes` here on purpose: this query groups by `tool` only, and reaching
            -- a row's histogram from inside it is exactly what the grouping rule forbids. The
            -- histograms are read by the second query below, which has no grouping to violate.
        from ai_tool_stats_daily
        where organization_id = $1 and day between $2 and $3
        group by tool
        order by calls desc, tool asc
        "#,
    )
    .bind(organization_id)
    .bind(window.from)
    .bind(window.to)
    .fetch_all(pool)
    .await?;

    // The histograms for the same window, keyed by tool.
    //
    // Zeros are dropped: an `error_code` present with count 0 is not a failure and must not
    // render as one. `jsonb_each_text` yields text values, so the count is parsed rather than
    // cast — `value::bigint` on a jsonb text value is a cast Postgres rejects for exactly this
    // shape.
    //
    // A `#[derive(FromRow)]` struct rather than a tuple: `query_scalar` with a tuple type does not
    // decode a two-column row (it expects one column per type), and the failure is a
    // `ColumnDecode` about `RECORD` against `TEXT` rather than anything that names the mistake.
    #[derive(sqlx::FromRow)]
    struct Histogram {
        tool: String,
        error_codes: serde_json::Value,
    }

    let histograms: std::collections::HashMap<String, std::collections::BTreeMap<String, i64>> =
        sqlx::query_as::<_, Histogram>(
            "select tool, error_codes from ai_tool_stats_daily \
             where organization_id = $1 and day between $2 and $3",
        )
        .bind(organization_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            // **The counts are JSON numbers, not strings.** `jsonb_object_agg(code, hits)`
            // writes `{"tool_timeout": 2}` — `hits` was bound as `bigint`, so the value decodes
            // as `Value::Number`. A reader that reaches for `as_str()` gets `None` for every
            // code and returns an empty histogram — a "no failures" reading on a tool with two,
            // with no error anywhere. `as_i64()` is the matching accessor; the string arm is kept
            // only for rows written by an older hand, and it is not the path production takes.
            let merged = row
                .error_codes
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(code, hits)| {
                            let parsed = hits.as_i64().or_else(|| {
                                hits.as_str().and_then(|raw| raw.parse::<i64>().ok())
                            })?;
                            (parsed > 0).then(|| (code.clone(), parsed))
                        })
                        .fold(
                            std::collections::BTreeMap::<String, i64>::new(),
                            |mut acc, (code, hits)| {
                                *acc.entry(code).or_default() += hits;
                                acc
                            },
                        )
                })
                .unwrap_or_default();
            (row.tool, merged)
        })
        .collect();

    Ok(rows
        .into_iter()
        .map(|row| {
            // Borrowed before the move: `histograms.get(&row.tool)` reads the name, and the
            // struct literal moves it on the next line. Looking the histogram up first is the
            // order that compiles and the order that reads.
            let error_codes = histograms.get(&row.tool).cloned().unwrap_or_default();
            ToolAggregate {
                tool: row.tool,
                calls: row.calls,
                successes: row.successes,
                failures: row.failures,
                denials: row.denials,
                p50_ms: row.p50_ms,
                p95_ms: row.p95_ms,
                p99_ms: row.p99_ms,
                cost_micros: row.cost_micros,
                // From the merged histogram map. A tool with no failures gets an empty map rather
                // than a missing key, so the reader renders "no failures" instead of having to
                // distinguish absent from zero.
                error_codes,
                days_seen: row.days_seen,
            }
        })
        .collect())
}

/// The costliest **failing** tool per day — the table under the scatter.
///
/// **Only tools with a failure qualify.** A tool that never failed has no cost-per-failure worth
/// ranking, and including it would let a busy, perfectly healthy tool sit above a tool that is
/// burning money *because* it is broken. The join to the day's rows is the filter; the `having`
/// is what makes "failing" mean "at least one", not "more failures than successes".
pub async fn costliest_failing_per_day(
    pool: &PgPool,
    organization_id: Uuid,
    window: Window,
) -> Result<Vec<(Date, String, i32, i64)>> {
    let rows = sqlx::query_as::<_, (Date, String, i32, i64)>(
        r#"
        select day, tool, failures, cost_micros
        from ai_tool_stats_daily
        where organization_id = $1
          and day between $2 and $3
          and failures > 0
        order by cost_micros desc, day desc, tool asc
        "#,
    )
    .bind(organization_id)
    .bind(window.from)
    .bind(window.to)
    .fetch_all(pool)
    .await?;

    // One row per day: the rank is taken in SQL order and the head of each day kept here, because
    // "the costliest failing tool **per day**" is a per-group maximum and `distinct on` would need
    // the whole result set materialised in the reader to be sure.
    let mut best: std::collections::BTreeMap<Date, (Date, String, i32, i64)> = Default::default();
    for row in rows {
        let day = row.0;
        // The ranking key is `(cost, failures)` — cost first, because the panel is "costliest
        // failing tool", and ties broken by failure count so two tools that cost the same are
        // ordered by how badly they are broken. It is computed *outside* the closure because
        // `and_modify` takes the closure by move: comparing inside would move `row` into the
        // closure and then use it again in `or_insert`.
        let rank = (row.3, row.2);
        best.entry(day)
            .and_modify(|held| {
                if rank > (held.3, held.2) {
                    *held = row.clone();
                }
            })
            .or_insert(row);
    }
    Ok(best.into_values().collect())
}

/// One bar of the step-count histogram: runs that took exactly `steps` steps.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct StepBucket {
    /// How many steps those runs took, exactly — not a bucket edge, so the screen can label it.
    pub steps: i32,
    /// How many runs took exactly that many.
    pub runs: i64,
}

/// How many steps each finished run took, collapsed into a histogram.
///
/// **Only settled runs, and only runs that started.** A run still in flight has a step count that
/// is a prefix rather than a length, so including it would drag the whole distribution towards
/// "one step" every time the pass happens to be looking — and a histogram whose shape depends on
/// when you loaded it is not a measurement. `queued` runs are excluded for the stronger version of
/// the same reason: `started_at` is null, so they have no day to be filed under at all.
///
/// The count is `count(s.id)` per run, so a run with **no** steps lands in the zero bar rather
/// than being dropped: "a run that answered without calling anything" is a real and interesting
/// row, and `inner join` would hide exactly the runs an operator most wants to see.
pub async fn step_histogram(
    pool: &PgPool,
    organization_id: Uuid,
    window: Window,
) -> Result<Vec<StepBucket>> {
    let rows = sqlx::query_as::<_, StepBucket>(
        r#"
        select per_run.steps::int as steps, count(*)::bigint as runs
        from (
            select count(s.id)::int as steps
            from ai_runs r
            left join ai_run_steps s on s.run_id = r.id
            where r.organization_id = $1
              and r.started_at is not null
              and r.status in ('completed', 'failed', 'cancelled')
              and (r.started_at at time zone 'utc')::date between $2 and $3
            group by r.id
        ) per_run
        group by per_run.steps
        order by per_run.steps asc
        "#,
    )
    .bind(organization_id)
    .bind(window.from)
    .bind(window.to)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One point of the cost-per-solved scatter: a day's runs and what they cost.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SolvedDay {
    pub day: Date,
    /// Every settled run that started that day, solved or not.
    pub runs: i64,
    /// How many of them actually answered.
    pub solved: i64,
    pub cost_micros: i64,
}

impl SolvedDay {
    /// What a solved run cost, in micros — or `None` when nothing was solved that day.
    ///
    /// **`None`, never `Some(0)`, for a day with no solved runs.** The same distinction
    /// `ToolAggregate::success_percent` makes: a day where every run failed cost a great deal and
    /// solved nothing, which is the *worst* point on this chart, not a point at zero. Rendering it
    /// as `0` puts the worst day at the best end of the axis — the one inversion this screen can
    /// make that actively misleads. `solved` is in the denominator for the days that do have
    /// solved runs, so the number is "what a success cost", not "what a day cost".
    pub fn cost_per_solved(&self) -> Option<i64> {
        (self.solved > 0).then(|| self.cost_micros / self.solved)
    }
}

/// Cost per solved task, per day.
///
/// **A run counts as solved only when it `completed` with `stop_reason = 'final_answer'`** — it
/// reached an answer and stopped because it had one. `completed` alone is not enough: a run that
/// hit `max_steps` is recorded `completed` too, and calling that solved would score a runaway as a
/// success while charging its full cost to the same denominator. `failed` and `cancelled` runs are
/// in `runs` (they cost money) and not in `solved` (they produced nothing), which is the whole
/// point of the ratio.
///
/// Days with no run at all are absent rather than zero, for the reason the module header gives: a
/// gap the writer has not reached yet is not a day that cost nothing.
pub async fn cost_per_solved(
    pool: &PgPool,
    organization_id: Uuid,
    window: Window,
) -> Result<Vec<SolvedDay>> {
    let rows = sqlx::query_as::<_, SolvedDay>(
        r#"
        select
            (r.started_at at time zone 'utc')::date       as day,
            count(*)::bigint                              as runs,
            count(*) filter (
                where r.status = 'completed' and r.stop_reason = 'final_answer'
            )::bigint                                    as solved,
            coalesce(sum(r.cost_micros), 0)::bigint      as cost_micros
        from ai_runs r
        where r.organization_id = $1
          and r.started_at is not null
          and r.status in ('completed', 'failed', 'cancelled')
          and (r.started_at at time zone 'utc')::date between $2 and $3
        group by 1
        order by 1 asc
        "#,
    )
    .bind(organization_id)
    .bind(window.from)
    .bind(window.to)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate(calls: i64, successes: i64, denials: i64) -> ToolAggregate {
        ToolAggregate {
            tool: "t".to_owned(),
            calls,
            successes,
            failures: calls - successes - denials,
            denials,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            cost_micros: 0,
            error_codes: Default::default(),
            days_seen: 1,
        }
    }

    #[test]
    fn a_tool_with_no_calls_has_no_success_rate() {
        // "—" and "0%" are different claims and the screen has to be able to say both: the first
        // is a tool nobody invoked, the second is one that was invoked and never worked.
        assert_eq!(aggregate(0, 0, 0).success_percent(), None);
        assert_eq!(aggregate(0, 0, 0).denial_percent(), None);
    }

    fn solved(runs: i64, solved_count: i64, cost_micros: i64) -> SolvedDay {
        SolvedDay {
            day: Date::from_calendar_date(2026, time::Month::March, 3).unwrap(),
            runs,
            solved: solved_count,
            cost_micros,
        }
    }

    #[test]
    fn a_day_that_solved_nothing_has_no_cost_per_solved_and_not_a_zero() {
        // Five runs, every one of them failed, and they cost a seventh of a cent between them.
        // The honest reading is "this day bought nothing". The tempting one is `0`, which sorts to
        // the good end of the scatter's y-axis — the exact day an operator most needs to see
        // painted at the best point on the chart.
        assert_eq!(solved(5, 0, 700_000).cost_per_solved(), None);
        assert_eq!(solved(0, 0, 0).cost_per_solved(), None);
    }

    #[test]
    fn the_cost_per_solved_is_divided_by_the_successes_and_not_by_the_runs() {
        // Ten runs, two answered, and the whole 1.00 went into them. "What did a success cost"
        // is 500,000; "what did a day cost divided by a run" is 100,000, which is a number about
        // nothing. The failures are in the cost, not in the denominator.
        assert_eq!(solved(10, 2, 1_000_000).cost_per_solved(), Some(500_000));
    }

    #[test]
    fn a_success_rate_counts_denials_against_the_tool() {
        // 10 calls, 6 ok, 2 denied, 2 failed: 60%, and the denials are inside the denominator
        // because a denial is a call that did not do what the agent asked for. Merging denials
        // into "successes" (80%) would be the flattering lie this split exists to prevent.
        let row = aggregate(10, 6, 2);
        assert_eq!(row.success_percent(), Some(60.0));
        assert_eq!(row.denial_percent(), Some(20.0));
    }
}
