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
            -- The failure-reason histogram. `coalesce(error_code, 'unknown')` because a failed
            -- call with no code is still a failure the operator has to see — dropping it would
            -- make the ranked list quietly under-report, which is the failure mode of every
            -- "only count what we can name" aggregation.
            coalesce(
                jsonb_object_agg(
                    coalesce(c.error_code, 'unknown'),
                    count(*) filter (
                        where c.status not in ('ok', 'denied')
                    )::bigint
                ) filter (
                    where c.status not in ('ok', 'denied')
                ),
                '{}'::jsonb
            )                                             as error_codes
        from ai_tool_calls c
        left join ai_run_steps s on s.id = c.step_id
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
        error_codes: serde_json::Value,
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
            case when sum(calls) = 0 then null else
                (weighted_avg(p50_ms, calls))::int
            end                       as p50_ms,
            case when sum(calls) = 0 then null else
                (weighted_avg(p95_ms, calls))::int
            end                       as p95_ms,
            case when sum(calls) = 0 then null else
                (weighted_avg(p99_ms, calls))::int
            end                       as p99_ms,
            sum(cost_micros)::bigint as cost_micros,
            -- Merging the daily histograms by summing each code's count, then dropping the zeros
            -- a re-run can leave behind. An `error_code` present with count 0 is not a failure
            -- and must not render as one.
            coalesce((
                select jsonb_object_agg(code, hits)
                from (
                    select key as code, sum(value::bigint) as hits
                    from jsonb_each_text(error_codes)
                    group by key
                    having sum(value::bigint) > 0
                ) merged
            ), '{}'::jsonb)          as error_codes,
            count(*)::int             as days_seen
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

    Ok(rows
        .into_iter()
        .map(|row| ToolAggregate {
            tool: row.tool,
            calls: row.calls,
            successes: row.successes,
            failures: row.failures,
            denials: row.denials,
            p50_ms: row.p50_ms,
            p95_ms: row.p95_ms,
            p99_ms: row.p99_ms,
            cost_micros: row.cost_micros,
            error_codes: row
                .error_codes
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(code, count)| {
                            count
                                .as_str()
                                .and_then(|raw| raw.parse::<i64>().ok())
                                .map(|hits| (code.clone(), hits))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            days_seen: row.days_seen,
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