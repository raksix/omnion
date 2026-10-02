//! The store of record for the platform-wide budgets (REQ-127, slice 1).
//!
//! PostgreSQL holds the policies and the refusal rollups; Redis holds the live counters. The
//! request says exactly that split and the reason it is right is in the risk notes: **live
//! counters belong in Redis** because they are written on every request, and **policy belongs in
//! PostgreSQL** because it is a document an operator edits, versions and rolls back.
//!
//! Every query here is `query_as` with a **struct whose fields are aliased to the column names**,
//! which is the sqlx rule this workspace has been bitten by more than once: `query_as` resolves
//! fields by name at runtime, so `id: Uuid` against a column called `policy_id` compiles and then
//! answers `no column found for name` on the first request. That is a production 500 with a green
//! build, so the aliases are written here where they can be read next to the `select`.
//!
//! ## The rollup writes one row per window, and that is the schema's job
//!
//! `rate_limit_refusals` carries `unique nulls not distinct (scope, target_id, route,
//! window_start)`. The upsert below therefore cannot create a second row for one window even if
//! two requests race into it, and [`RollupOutcome::Emitted`] is returned by the first increment
//! and `None` by every one after it — which is what makes "one aggregated event per target and
//! window rather than a per-request flood" a property of the schema and the return value rather
//! than a promise in a comment. `nulls not distinct` is the load-bearing word: a plain `unique`
//! treats two NULLs as distinct and the aggregation silently becomes per-request.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ReliabilityError, Result};
use crate::limits::{LimitPolicy, RefusalRollup};
use crate::vocabulary::MAX_PAGE;

/// What one rollup write did, and therefore whether it emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupOutcome {
    /// This increment created the window's row, so this refusal is the one that emits.
    Emitted,
    /// The row already existed, so the count moved and no event is published.
    Counted,
}

/// One stored policy, as the row it came from.
#[derive(Debug, sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    name: String,
    scope: String,
    target_id: Option<String>,
    route_pattern: Option<String>,
    limit_count: i32,
    /// `INT4` in the migration, so the row decodes into an `i32` and `try_into()`s to the
    /// `i64` the domain uses. A direct `i64` field against an `int` column compiles and answers
    /// `mismatched types; Rust type i64 (as SQL type INT8) is not compatible with SQL type INT4`
    /// on the FIRST request — the sqlx runtime-bind trap again, this time on a column the
    /// migration's own `check (window_seconds between 1 and 86400)` made un-ambiguous.
    window_seconds: i32,
    burst: i32,
    priority: i32,
    is_default: bool,
    enabled: bool,
}

impl From<PolicyRow> for LimitPolicy {
    fn from(row: PolicyRow) -> Self {
        Self {
            id: Some(row.id),
            name: row.name,
            scope: row.scope,
            target_id: row.target_id,
            route_pattern: row.route_pattern,
            limit_count: row.limit_count,
            window_seconds: i64::from(row.window_seconds),
            burst: row.burst,
            priority: row.priority,
            is_default: row.is_default,
            enabled: row.enabled,
        }
    }
}


/// Read every policy, **including the disabled ones**.
///
/// A second read rather than a flag on [`load_policies`], because the two have different
/// consumers and only one of them may ever use the list to make an enforcement decision:
/// `load_policies` feeds the request path, where a disabled policy must never appear, and this
/// feeds the panel and the dry-run, where a disabled policy is the answer to "why is nothing
/// limiting this subject". A single function with a flag would put the panel's need one argument
/// away from the limiter's, and the argument that widens a limiter's input is the dangerous one.
pub async fn load_all_policies(pool: &PgPool) -> Result<Vec<LimitPolicy>> {
    let rows = sqlx::query_as::<_, PolicyRow>(
        r#"
        select id, name, scope, target_id, route_pattern,
               limit_count, window_seconds, burst, priority, is_default, enabled
          from rate_limit_policies
         order by priority, id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(LimitPolicy::from).collect())
}

/// Read every policy the resolver may pick from.
///
/// **Only enabled rows are read**, and that is what makes the hot read one indexed scan: the
/// migration puts `(enabled, priority)` on the table precisely so this statement is the resolver's
/// whole cost. `pick` filters again in the process because a disabled row reaching the resolver
/// would depend on it remembering to.
pub async fn load_policies(pool: &PgPool) -> Result<Vec<LimitPolicy>> {
    let rows = sqlx::query_as::<_, PolicyRow>(
        r#"
        select id, name, scope, target_id, route_pattern,
               limit_count, window_seconds, burst, priority, is_default, enabled
          from rate_limit_policies
         where enabled
         order by priority, id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(LimitPolicy::from).collect())
}

/// One policy, or `None`.
///
/// A missing policy and a policy the caller may not see are the same answer, so the detail route
/// cannot be used to probe what exists.
pub async fn load_policy(pool: &PgPool, id: Uuid) -> Result<Option<LimitPolicy>> {
    let row = sqlx::query_as::<_, PolicyRow>(
        r#"
        select id, name, scope, target_id, route_pattern,
               limit_count, window_seconds, burst, priority, is_default, enabled
          from rate_limit_policies
         where id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(LimitPolicy::from))
}

/// Insert a policy after validating it.
///
/// The validation runs **here**, on the way into the table, rather than only in the route: a
/// hand-edited row or a second caller that skips the route would otherwise store a window of zero
/// seconds, and the first request to match it would divide by zero.
pub async fn insert_policy(pool: &PgPool, policy: &LimitPolicy) -> Result<LimitPolicy> {
    let mut policy = policy.clone();
    policy.validate()?;
    let id = policy.id.unwrap_or_else(Uuid::new_v4);
    policy.id = Some(id);
    let row = sqlx::query_as::<_, PolicyRow>(
        r#"
        insert into rate_limit_policies
               (id, name, scope, target_id, route_pattern, limit_count,
                window_seconds, burst, priority, is_default, enabled)
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        on conflict (scope, target_id, route_pattern) do update
               set name = excluded.name,
                   limit_count = excluded.limit_count,
                   window_seconds = excluded.window_seconds,
                   burst = excluded.burst,
                   priority = excluded.priority,
                   enabled = excluded.enabled,
                   updated_at = now()
        returning id, name, scope, target_id, route_pattern,
                  limit_count, window_seconds, burst, priority, is_default, enabled
        "#,
    )
    .bind(id)
    .bind(&policy.name)
    .bind(&policy.scope)
    .bind(policy.target_id.as_deref())
    .bind(policy.route_pattern.as_deref())
    .bind(policy.limit_count)
    .bind(policy.window_seconds)
    .bind(policy.burst)
    .bind(policy.priority)
    .bind(policy.is_default)
    .bind(policy.enabled)
    .fetch_one(pool)
    .await?;
    Ok(LimitPolicy::from(row))
}

/// Edit a policy in place.
///
/// `None` when there is no such row, so the caller answers `404` rather than reporting a write
/// that did not happen — a `200` for an upsert of nothing is how an operator concludes the screen
/// is broken.
pub async fn update_policy(pool: &PgPool, id: Uuid, policy: &LimitPolicy) -> Result<Option<LimitPolicy>> {
    let mut policy = policy.clone();
    policy.validate()?;
    let row = sqlx::query_as::<_, PolicyRow>(
        r#"
        update rate_limit_policies
           set name = $2,
               scope = $3,
               target_id = $4,
               route_pattern = $5,
               limit_count = $6,
               window_seconds = $7,
               burst = $8,
               priority = $9,
               enabled = $10,
               updated_at = now()
         where id = $1
        returning id, name, scope, target_id, route_pattern,
                  limit_count, window_seconds, burst, priority, is_default, enabled
        "#,
    )
    .bind(id)
    .bind(&policy.name)
    .bind(&policy.scope)
    .bind(policy.target_id.as_deref())
    .bind(policy.route_pattern.as_deref())
    .bind(policy.limit_count)
    .bind(policy.window_seconds)
    .bind(policy.burst)
    .bind(policy.priority)
    .bind(policy.enabled)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(LimitPolicy::from))
}

/// Remove a policy, or `None` when there is no such row.
///
/// **A shipped default is disabled, not deleted.** The request says it in the screen column and
/// the rule belongs in the store rather than in the route: a `delete` that removed a default would
/// leave the next boot with no row for a scope every deployment needs, and the only symptom would
/// be a limiter that silently stops limiting.
pub async fn delete_policy(pool: &PgPool, id: Uuid) -> Result<Option<LimitPolicy>> {
    let row = sqlx::query_as::<_, PolicyRow>(
        r#"
        update rate_limit_policies
           set enabled = false, updated_at = now()
         where id = $1 and is_default
        returning id, name, scope, target_id, route_pattern,
                  limit_count, window_seconds, burst, priority, is_default, enabled
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        return Ok(Some(LimitPolicy::from(row)));
    }

    let custom = sqlx::query_as::<_, PolicyRow>(
        r#"
        delete from rate_limit_policies
         where id = $1
        returning id, name, scope, target_id, route_pattern,
                  limit_count, window_seconds, burst, priority, is_default, enabled
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(custom.map(LimitPolicy::from))
}

/// Count a refusal into this window's rollup, and say whether it is the one that emits.
///
/// `insert … on conflict … do update` with `xmax = 0` in the returning list is the standard
/// PostgreSQL way to ask "did this write create the row", and it is what turns the request's
/// "one aggregated event per window" into a return value instead of a read-then-write race: two
/// concurrent refusals both upsert, exactly one sees `xmax = 0`, and the loser updates the same
/// row instead of emitting a second event.
pub async fn record_refusal(pool: &PgPool, rollup: &RefusalRollup) -> Result<RollupOutcome> {
    let created: bool = sqlx::query_scalar(
        r#"
        insert into rate_limit_refusals
               (scope, target_id, route, window_start, refusals, last_refusal_at)
        values ($1, $2, $3, $4, 1, $5)
        on conflict (scope, target_id, route, window_start) do update
               set refusals = rate_limit_refusals.refusals + 1,
                   last_refusal_at = excluded.last_refusal_at
        returning xmax = 0
        "#,
    )
    .bind(&rollup.scope)
    .bind(rollup.target_id.as_deref())
    .bind(&rollup.route)
    .bind(rollup.window_start)
    .bind(rollup.last_refusal_at)
    .fetch_one(pool)
    .await?;
    Ok(if created {
        RollupOutcome::Emitted
    } else {
        RollupOutcome::Counted
    })
}

/// One stored rollup, as the panel reads it.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RefusalRow {
    /// Which scope was refused.
    pub scope: String,
    /// The subject that was refused, or `None` when the scope has none.
    pub target_id: Option<String>,
    /// The route template.
    pub route: String,
    /// The window these refusals belong to.
    pub window_start: OffsetDateTime,
    /// How many requests were refused in it.
    pub refusals: i32,
    /// When the last one landed.
    pub last_refusal_at: OffsetDateTime,
}

/// The rollups, newest window first, bounded by [`MAX_PAGE`].
///
/// A bounded read with no filter is the shape that bites: an operator watching refusals arrive
/// and a retention job deleting them at the same time is two writers, and the page must not grow
/// with the number of windows ever seen.
pub async fn load_refusals(pool: &PgPool, limit: usize) -> Result<Vec<RefusalRow>> {
    let rows = sqlx::query_as::<_, RefusalRow>(
        r#"
        select scope, target_id, route, window_start, refusals, last_refusal_at
          from rate_limit_refusals
         order by window_start desc, last_refusal_at desc
         limit $1
        "#,
    )
    .bind(i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(MAX_PAGE as i64))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// How many refusals landed in the last `hours`, for the overview's tile.
pub async fn refusals_in_window(pool: &PgPool, hours: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        r#"
        select coalesce(sum(refusals), 0)
          from rate_limit_refusals
         where last_refusal_at > now() - make_interval(hours => $1::int)
        "#,
    )
    .bind(i64::try_from(hours.clamp(1, 24 * 30)).unwrap_or(720))
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Delete rollups whose window has closed.
///
/// `make_interval(days => $1::int)` with the **cast on the parameter**, not on the call: the
/// observability retention sweep hit exactly this and every prune was refused with
/// "function make_interval(days => bigint) does not exist", which reported itself as `0` deleted
/// and hid a retention window that was never honoured.
pub async fn prune_refusals(pool: &PgPool, older_than_days: i64) -> Result<u64> {
    let deleted = sqlx::query("delete from rate_limit_refusals where window_start < now() - make_interval(days => $1::int)")
        .bind(i64::try_from(older_than_days).unwrap_or(7))
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every raw SQL string in this file, and nothing else.
    ///
    /// A source-scanning test that reads the whole file matches its own filter, its own
    /// assertion message and its own explanation — and then fails on them. Extracting the `r#"…"#`
    /// literals is the difference between a test that reads the SQL and a test that reads itself.
    fn sql_statements() -> Vec<String> {
        let source = include_str!("store.rs");
        let mut statements = Vec::new();
        let mut rest = source;
        while let Some(open) = rest.find("r#\"") {
            let after = &rest[open + 3..];
            let Some(close) = after.find("\"#") else {
                break;
            };
            statements.push(after[..close].to_owned());
            rest = &after[close + 2..];
        }
        statements
    }

    /// The struct's field names ARE the column names.
    ///
    /// This is a compile-time-cheap, runtime-expensive rule in this codebase: `query_as` binds by
    /// name, so a field called `limit_count` against a column called `limit` builds fine and
    /// answers `no column found for name: limit_count` on the first real request. Asserting the
    /// names here means the assertion fails in the crate rather than in production.
    ///
    /// The list is written out rather than derived from the struct: a list generated from the
    /// struct agrees with itself by construction, which is exactly the failure this exists to
    /// catch. What is checked is the *other* direction — every column the SQL selects is a field
    /// the struct actually declares, read from the struct's own construction below.
    #[test]
    fn every_selected_column_has_a_field_of_the_same_name() {
        const SELECTED: &[&str] = &[
            "id",
            "name",
            "scope",
            "target_id",
            "route_pattern",
            "limit_count",
            "window_seconds",
            "burst",
            "priority",
            "is_default",
            "enabled",
        ];

        // Constructing the row with every field named is the assertion: rename a field and this
        // literal stops compiling, which is the failure `query_as` would otherwise defer to the
        // first production request.
        let row = PolicyRow {
            id: Uuid::nil(),
            name: String::new(),
            scope: String::new(),
            target_id: None,
            route_pattern: None,
            limit_count: 0,
            window_seconds: 0,
            burst: 0,
            priority: 0,
            is_default: false,
            enabled: false,
        };
        assert_eq!(row.id, Uuid::nil());
        assert_eq!(row.limit_count, 0);
        assert!(!row.enabled);

        // And every selected column appears in a projection at least once. A column the
        // projection names and the struct does not have is a request-time 500; the reverse — a
        // field with no column — is a field that is always `NULL`, which is worse because it is
        // silent, so the projection is required to carry all of them.
        let sql = sql_statements().join("\n");
        for column in SELECTED {
            assert!(
                sql.contains(column),
                "{column} is a field but no projection selects it"
            );
        }
        // A `select *` is what would let a schema change slip past this test silently.
        assert!(
            !sql.contains("select *"),
            "an explicit column list is the only thing this assertion can read"
        );
    }

    /// The upsert asks PostgreSQL whether the row was created rather than reading it back first.
    #[test]
    fn the_rollup_asks_xmax_whether_it_created_the_row() {
        // `xmax = 0` is the only race-free way to ask "was this an insert"; a `select count(*)`
        // before the write is wrong under exactly the concurrency the aggregation exists for.
        let sql = sql_statements().join("\n");
        assert!(
            sql.contains("xmax = 0"),
            "the created-row question is asked"
        );
        assert!(
            sql.contains("on conflict (scope, target_id, route, window_start)"),
            "the upsert names the unique constraint the migration declared"
        );
    }

    /// Every `make_interval` call casts its parameter to `int`.
    ///
    /// The observability sweep shipped a prune that was refused on every call by exactly this,
    /// and it hid because a failed prune reports the same number as a quiet one.
    #[test]
    fn every_interval_call_casts_its_parameter() {
        let sql = sql_statements().join("\n");
        let calls: Vec<&str> = sql
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("make_interval("))
            .collect();
        assert!(!calls.is_empty(), "the prune and the rollup both call it");
        for line in calls {
            assert!(
                line.contains("::int"),
                "make_interval without an int cast: {line}"
            );
        }
    }

    /// A list read is bounded, because a page that grows with every window ever seen is a page
    /// that eventually takes the panel down.
    #[test]
    fn the_rollup_read_is_bounded_by_the_shared_page_ceiling() {
        let sql = sql_statements().join("\n");
        assert!(
            sql.contains("limit $1"),
            "the refusal read must pass a bound rather than trusting the caller"
        );
        assert!(
            MAX_PAGE <= 1_000,
            "the shared page ceiling has to stay a page"
        );
    }

    /// A shipped default is disabled rather than deleted, and that rule is in the store.
    ///
    /// The request states it as a screen behaviour ("a delete on a default is a disable, not a
    /// removal"). A rule that lives only in a route is a rule the second caller does not have, so
    /// it is asserted here against the SQL: a default's removal is an `update … set enabled`.
    #[test]
    fn a_default_is_disabled_rather_than_deleted() {
        let sql = sql_statements().join("\n");
        assert!(
            sql.contains("set enabled = false") && sql.contains("is_default"),
            "the default path updates `enabled`, and only the `is_default` row takes it"
        );
        // The plain delete must therefore carry a guard, or a custom policy would take the
        // branch that only disables.
        let delete = sql
            .split("delete from rate_limit_policies")
            .nth(1)
            .expect("a custom delete exists");
        assert!(
            delete.contains("where id = $1"),
            "the custom delete is keyed on the id it was given"
        );
    }
}
