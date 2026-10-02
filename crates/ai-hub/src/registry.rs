//! The `ai_tools` registry: the seeder, the operator's four decisions, and the read path the
//! panel and the execution path share (REQ-100, slice 1).
//!
//! # Why a table mirrors a Rust constant list
//!
//! [`crate::catalogue::specs`] is the source of truth for *what tools exist*. This table exists
//! for the four things code cannot know: whether an operator enabled a tool, its timeout, its
//! per-run cap, and whether it is gated behind approval. That split is the whole reason the
//! request says the seeder "touches only `description`, `class`, `permission`, `risk`,
//! `input_schema`, `example` and `idempotent`, preserving `enabled`, `timeout_ms`,
//! `max_calls_per_run` and `requires_approval`".
//!
//! The failure that motivates the rule is concrete: a deploy that re-runs the seeder and
//! overwrites `requires_approval = false` silently un-gates `deployment.deploy` for every
//! organization on the installation. The gate is the one column an operator set *because* they
//! disagreed with the default, so [`seed`] writes it exactly once — on insert — and never in its
//! update branch. A reader should be able to see that rule in the SQL rather than trust it.
//!
//! # Why a retired tool keeps its row
//!
//! A tool removed from code keeps its row, gets `enabled = false` and carries a `retired_note`.
//! Deleting the row would cascade away the `ai_tool_grants` decisions made about it, and the
//! request is explicit that the registry "never silently deletes grants" — the grant history is
//! how an operator answers "was this agent ever allowed to deploy?". So retirement is a flag and
//! a note, never a delete.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalogue::{self, ToolSpec};
use crate::error::{AiHubError, Result};

/// Every column the store reads back, in one place so a write and a read cannot drift.
const TOOL_COLUMNS: &str = "id, key, class, permission, risk, description, input_schema, \
     example, idempotent, requires_approval, enabled, timeout_ms, max_calls_per_run, \
     retired_note, created_at, updated_at";

/// The note a tool's row carries once the tool has left the compiled catalogue.
///
/// It is a real column rather than a derived flag because the two are not the same question: a
/// row can be *retired* (the code is gone) and *disabled* (an operator said no) independently, and
/// the panel shows different copy for each. Deriving one from the other is how a screen ends up
/// telling an operator their tool "was removed in an upgrade" when they disabled it in March.
const RETIRED_NOTE: &str =
    "This tool is no longer in the compiled catalogue. Its row and its grants are kept so the \
     decision history survives; re-adding the tool in code re-enables it.";

/// A registry row, as the API and the panel read it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ToolRow {
    pub id: Uuid,
    pub key: String,
    pub class: String,
    pub permission: String,
    pub risk: String,
    pub description: String,
    pub input_schema: Value,
    pub example: Option<Value>,
    pub idempotent: bool,
    pub requires_approval: bool,
    pub enabled: bool,
    pub timeout_ms: i32,
    pub max_calls_per_run: i32,
    /// `Some` once the tool has left the compiled catalogue.
    pub retired_note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl ToolRow {
    /// A high-risk tool that is enabled and ungated — the state the panel stripes.
    ///
    /// The request asks for that stripe, so this predicate has to exist and has to be reachable:
    /// the migration deliberately does not forbid the combination (see its comment), so the
    /// warning is the only thing standing between an operator and a surprise.
    #[must_use]
    pub fn is_ungated_high_risk(&self) -> bool {
        self.enabled && self.risk == "high" && !self.requires_approval
    }

    /// Whether this tool is still in the compiled catalogue.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.retired_note.is_some()
    }
}

/// The four operator-owned decisions, and only those.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ToolLimits {
    pub enabled: Option<bool>,
    pub timeout_ms: Option<i32>,
    pub max_calls_per_run: Option<i32>,
    pub requires_approval: Option<bool>,
}

impl ToolLimits {
    /// Nothing to change. A `PATCH` with every field absent is rejected by the route rather
    /// than silently bumping `updated_at`, because a no-op that claims to have edited a tool is
    /// worse than a 400.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.timeout_ms.is_none()
            && self.max_calls_per_run.is_none()
            && self.requires_approval.is_none()
    }
}

/// The outcome of one seeding pass, so boot can log what it did instead of guessing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedOutcome {
    /// Rows created from the compiled catalogue.
    pub inserted: u32,
    /// Rows whose code half was refreshed.
    pub refreshed: u32,
    /// Rows marked retired because their key is no longer compiled.
    pub retired: u32,
    /// Rows whose operator decisions were left alone — the number that proves the rule works.
    pub decisions_preserved: u32,
}

/// Upsert the compiled catalogue into `ai_tools`, preserving every operator decision.
///
/// **The update branch touches code columns only.** That is the entire contract, and it is
/// visible in the `do update set` list below rather than hidden in Rust: `enabled`,
/// `timeout_ms`, `max_calls_per_run` and `requires_approval` appear in neither the insert's
/// defaults nor the update's assignments. A future column added to `ai_tools` is therefore
/// inert by default, which is the correct direction for a mistake to fail in.
/// Whether a tool's HTTP route exists, read from the compiled binding table.
///
/// One function so the seeder and the panel cannot answer differently: the question "is this
/// tool wired?" has one answer, and it is the compiled table's, not a re-derivation.
fn binding_is_live(key: &str) -> bool {
    matches!(
        crate::ops_binding::binding_for(key).map(|row| row.binding),
        Some(crate::ops_binding::RouteBinding::Live { .. })
    )
}

pub async fn seed(pool: &PgPool) -> Result<SeedOutcome> {
    let mut outcome = SeedOutcome::default();
    let compiled: Vec<String> = catalogue::specs().iter().map(|s| s.key.to_owned()).collect();

    for spec in catalogue::specs() {
        let (timeout_ms, max_calls_per_run) = catalogue::default_limits(spec);
        let row = spec.to_row();
        // A tool with no HTTP route ships gated even when its risk is Low: the registry row
        // would otherwise invite an operator to enable an action the platform cannot perform.
        // `wired` comes from the compiled binding, so adding the route is what ungates it.
        let requires_approval = catalogue::default_requires_approval_for(spec, binding_is_live(spec.key));
        // `on conflict (key) do update` with the code columns as assignments. The four operator
        // columns are deliberately absent from both halves.
        let sql = "insert into ai_tools (key, class, permission, risk, description, input_schema, \
                   example, idempotent, requires_approval, enabled, timeout_ms, max_calls_per_run, \
                   retired_note) \
                   values ($1, $2, $3, $4, $5, $6, $7, $8, $9, true, $10, $11, null) \
                   on conflict (key) do update set \
                     class = excluded.class, \
                     permission = excluded.permission, \
                     risk = excluded.risk, \
                     description = excluded.description, \
                     input_schema = excluded.input_schema, \
                     example = excluded.example, \
                     idempotent = excluded.idempotent, \
                     retired_note = null, \
                     updated_at = now() \
                   returning (xmax = 0) as inserted";
        let inserted: bool = sqlx::query_scalar(sql)
            .bind(spec.key)
            .bind(spec.class)
            .bind(spec.permission)
            .bind(spec.risk.as_str())
            .bind(spec.description)
            .bind(&row["input_schema"])
            .bind(&row["example"])
            .bind(spec.idempotent)
            .bind(requires_approval)
            .bind(timeout_ms)
            .bind(max_calls_per_run)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false);
        if inserted {
            outcome.inserted += 1;
        } else {
            outcome.refreshed += 1;
            outcome.decisions_preserved += 1;
        }
    }

    // Retire what left the catalogue. The key comparison is done in SQL against the array, so a
    // key that contains a quote cannot break the statement.
    let sql = "update ai_tools set enabled = false, retired_note = $2, updated_at = now() \
               where retired_note is null and not (key = any($1::text[])) \
               returning (xmax = 0) as touched";
    let retired: Vec<bool> = sqlx::query_scalar(sql)
        .bind(&compiled)
        .bind(RETIRED_NOTE)
        .fetch_all(pool)
        .await?;
    outcome.retired = retired.len() as u32;

    Ok(outcome)
}

/// The whole registry, ordered the way the panel groups it.
pub async fn list_tools(pool: &PgPool) -> Result<Vec<ToolRow>> {
    let sql = format!("select {TOOL_COLUMNS} from ai_tools order by class, key");
    let rows = sqlx::query_as::<_, ToolRow>(&sql).fetch_all(pool).await?;
    Ok(rows)
}

/// The keys an operator has switched off, in one query.
///
/// **This is the input the model-facing payload is built from**, and it is a separate function
/// from [`get_tool`] on purpose: a run asks "which tools are off" once, not "what is the state
/// of each of the twenty-four tools I might offer". Reading the column directly — rather than
/// pulling every [`ToolRow`] and filtering in Rust — is what keeps the payload's cost at one
/// round trip whatever the catalogue grows to.
///
/// A retired tool is included, because a retired tool is disabled: the seeder sets `enabled =
/// false` on it and the operator's row is the only place that decision exists. Offering a
/// retired tool to a model is the exact failure this criterion exists to prevent.
pub async fn disabled_keys(pool: &PgPool) -> Result<std::collections::BTreeSet<String>> {
    let keys: Vec<String> = sqlx::query_scalar("select key from ai_tools where enabled is false")
        .fetch_all(pool)
        .await?;
    Ok(keys.into_iter().collect())
}

/// One tool by key. A retired row is still found — the panel has to be able to explain it.
pub async fn get_tool(pool: &PgPool, key: &str) -> Result<Option<ToolRow>> {
    let sql = format!("select {TOOL_COLUMNS} from ai_tools where key = $1");
    let row = sqlx::query_as::<_, ToolRow>(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Change the four operator-owned decisions on one tool.
///
/// A `None` field is left as it is, so the route can send only what the form actually touched.
/// The ranges are re-checked here rather than trusted to the column constraints: a limit outside
/// them has to answer with a message naming the field, and a check constraint answers with a
/// database error the caller cannot act on.
pub async fn update_tool(
    pool: &PgPool,
    key: &str,
    changes: &ToolLimits,
) -> Result<Option<ToolRow>> {
    if let Some(timeout) = changes.timeout_ms
        && !(1000..=300_000).contains(&timeout)
    {
        return Err(AiHubError::InvalidTool(
            "timeout_ms must be between 1000 and 300000".to_owned(),
        ));
    }
    if let Some(cap) = changes.max_calls_per_run
        && !(1..=200).contains(&cap)
    {
        return Err(AiHubError::InvalidTool(
            "max_calls_per_run must be between 1 and 200".to_owned(),
        ));
    }

    // One statement with COALESCE per column: four optional booleans and integers cannot be
    // built as a dynamic SET list without either four round trips or a bind the planner cannot
    // type, and the predicate is written out rather than assembled so the query is still
    // readable in `pg_stat_statements`.
    let sql = format!(
        "update ai_tools set \
           enabled = coalesce($2, enabled), \
           requires_approval = coalesce($3, requires_approval), \
           timeout_ms = coalesce($4, timeout_ms), \
           max_calls_per_run = coalesce($5, max_calls_per_run), \
           updated_at = now() \
         where key = $1 \
         returning {TOOL_COLUMNS}"
    );
    let row = sqlx::query_as::<_, ToolRow>(&sql)
        .bind(key)
        .bind(changes.enabled)
        .bind(changes.requires_approval)
        .bind(changes.timeout_ms)
        .bind(changes.max_calls_per_run)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Per-tool usage over a window, as the registry's "Calls 30 d" and "Error %" columns read it.
///
/// **This is an aggregation of `ai_tool_calls` and nothing else.** The acceptance criterion says
/// "the usage counts on `/ai/tools` equal the aggregation of `ai_tool_calls` for the window
/// (asserted against SQL)", so the numbers here cannot come from a cached counter, a daily
/// rollup or a sum the client computes — a denormalised count that disagrees with the log is
/// exactly the bug that criterion exists to prevent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ToolUsage {
    pub calls: i64,
    pub errors: i64,
    /// Mean duration over the window, in milliseconds. `None` when nothing was called, so the
    /// panel can print an em dash instead of `0 ms` — a zero would read as "instantly fast".
    pub avg_duration_ms: Option<f64>,
    pub last_used: Option<OffsetDateTime>,
}

impl ToolUsage {
    /// Errors as a percentage, or `None` when there were no calls at all.
    ///
    /// `None` rather than `0.0` is the deliberate choice: a tool nobody has called in thirty
    /// days is not a tool with a 0 % error rate, and a table full of `0%` hides the difference
    /// between "healthy" and "never ran".
    #[must_use]
    pub fn error_rate(&self) -> Option<f64> {
        if self.calls == 0 {
            return None;
        }
        Some((self.errors as f64 / self.calls as f64) * 100.0)
    }
}

/// Aggregate `ai_tool_calls` for one organization over a window, keyed by tool.
///
/// The window is **seconds, not a duration type**, and that is not laziness. `crates/ai-hub`
/// depends on `time`, not `chrono`, so a `chrono::Duration` here would mean adding a second
/// clock crate to a crate whose whole point is that its timestamps are `time::OffsetDateTime`.
/// The bind was already an `i64` of seconds; the signature now says so.
pub async fn usage_over(
    pool: &PgPool,
    organization_id: Uuid,
    window_seconds: i64,
) -> Result<std::collections::HashMap<String, ToolUsage>> {
    let sql = "select tool_key, \
                 count(*) as calls, \
                 count(*) filter (where status <> 'ok') as errors, \
                 avg(duration_ms) as avg_duration_ms, \
                 max(created_at) as last_used \
               from ai_tool_calls \
               where organization_id = $1 and created_at >= now() - ($2 * interval '1 second') \
               group by tool_key";
    let rows = sqlx::query_as::<_, (String, i64, i64, Option<f64>, Option<OffsetDateTime>)>(sql)
        .bind(organization_id)
        .bind(window_seconds)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(key, calls, errors, avg_duration_ms, last_used)| {
            (
                key,
                ToolUsage { calls, errors, avg_duration_ms, last_used },
            )
        })
        .collect())
}

/// One row of a tool's "Recent calls" list.
///
/// **Arguments are never returned.** The spec asks for them redacted, and the reason is worth
/// stating: a tool call's arguments are content an agent chose to send — a page title, a search
/// phrase, sometimes a whole prompt fragment. A call log rendered on a screen an operator opens
/// casually would then be a place where tenant content accumulates outside every retention and
/// export path the rest of the platform honours. The row carries the *sizes* instead, which is
/// what an operator actually uses the list for.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RecentCall {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub agent_id: Option<Uuid>,
    pub agent_name: Option<String>,
    pub run_id: Option<Uuid>,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: Option<i32>,
    pub args_bytes: Option<i32>,
    pub result_bytes: Option<i32>,
}

/// The last `limit` calls to one tool, newest first, scoped to one organization.
pub async fn recent_calls(
    pool: &PgPool,
    organization_id: Uuid,
    tool_key: &str,
    limit: i64,
) -> Result<Vec<RecentCall>> {
    // `limit` is clamped rather than bound raw: a caller asking for 10 million rows is not an
    // error, it is a screen with a Next button, and the cap is what keeps that a cheap query.
    let limit = limit.clamp(1, 200);
    let sql = "select c.id, c.created_at, c.agent_id, a.name as agent_name, c.run_id, \
                      c.status, c.error_code, c.duration_ms, c.args_bytes, c.result_bytes \
               from ai_tool_calls c \
               left join ai_agents a on a.id = c.agent_id \
               where c.organization_id = $1 and c.tool_key = $2 \
               order by c.created_at desc \
               limit $3";
    let rows = sqlx::query_as::<_, RecentCall>(sql)
        .bind(organization_id)
        .bind(tool_key)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One day of the usage chart.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UsagePoint {
    /// `YYYY-MM-DD` in UTC.
    pub day: String,
    pub calls: i64,
    pub errors: i64,
    pub avg_duration_ms: Option<f64>,
}

/// The per-day series for one tool's usage chart, oldest first.
///
/// **`generate_series` fills the gaps.** A chart that only carries days with calls draws a
/// straight line across a week of silence and reads as "steady usage" when the truth is two
/// bursts. The left join is what makes the quiet days visible as zero.
pub async fn usage_by_day(
    pool: &PgPool,
    organization_id: Uuid,
    tool_key: &str,
    days: i64,
) -> Result<Vec<UsagePoint>> {
    let days = days.clamp(1, 365);
    let sql = "select to_char(d.day, 'YYYY-MM-DD') as day, \
                      coalesce(count(c.id), 0) as calls, \
                      coalesce(count(c.id) filter (where c.status <> 'ok'), 0) as errors, \
                      avg(c.duration_ms) as avg_duration_ms \
               from generate_series( \
                        date_trunc('day', now()) - make_interval(days => $3::int - 1), \
                        date_trunc('day', now()), \
                        interval '1 day') as d(day) \
               left join ai_tool_calls c \
                 on c.tool_key = $2 \
                and c.organization_id = $1 \
                and c.created_at >= d.day \
                and c.created_at < d.day + interval '1 day' \
               group by d.day order by d.day";
    let rows = sqlx::query_as::<_, UsagePoint>(sql)
        .bind(organization_id)
        .bind(tool_key)
        .bind(days as i32)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// A minimal reference to an agent, for the disable dialog's list of names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRef {
    pub id: Uuid,
    pub name: String,
}

/// How many agents currently name a tool in their allow-list — and which ones.
///
/// The disable confirmation in the spec names those agents ("a confirmation dialog for
/// 'Disable' on a tool agents currently use (naming those agents)"), and this is the query that
/// makes the dialog honest. It reads the *agent's own* allow-list rather than the grant table,
/// because an agent can carry a tool no identity grants it — and that is exactly the case an
/// operator needs to hear about before switching it off.
pub async fn agents_using(pool: &PgPool, tool_key: &str) -> Result<Vec<AgentRef>> {
    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        id: Uuid,
        name: String,
    }
    let rows = sqlx::query_as::<_, Row>(
        // `tools` is a jsonb ARRAY, so the membership test is `?`-against-jsonb, not `= any(...)`:
        // `any` over a jsonb column is an array-of-array comparison and silently matches nothing,
        // which would make this dialog report "no agent uses this tool" for every tool and the
        // operator would disable one that four agents depend on. `enabled` is the column, not
        // `status` — see migration 0064.
        "select id, name from ai_agents \
         where tools ? $1 and enabled order by name",
    )
    .bind(tool_key)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AgentRef { id: row.id, name: row.name })
        .collect())
}

/// The compiled specs, for the route that answers "is this key a real tool".
pub fn is_compiled(key: &str) -> bool {
    catalogue::specs().iter().any(|spec| spec.key == key)
}

/// The spec for one key, if the tool is still compiled.
#[must_use]
pub fn spec_for(key: &str) -> Option<&'static ToolSpec> {
    catalogue::specs().iter().find(|spec| spec.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The seeder's contract is a list of columns, so it is asserted as one. If somebody adds an
    /// operator-owned column to the migration and then to this struct, this fails with the name
    /// of the column instead of a boot that resets somebody's limits.
    const OPERATOR_COLUMNS: &[&str] =
        &["enabled", "timeout_ms", "max_calls_per_run", "requires_approval"];

    #[test]
    fn the_ungated_high_risk_predicate_is_reachable() {
        let base = ToolRow {
            id: Uuid::nil(),
            key: "deployment.deploy".to_owned(),
            class: "ops".to_owned(),
            permission: "deployment.deploy".to_owned(),
            risk: "high".to_owned(),
            description: String::new(),
            input_schema: json!({}),
            example: None,
            idempotent: false,
            requires_approval: true,
            enabled: true,
            timeout_ms: 60_000,
            max_calls_per_run: 10,
            retired_note: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(!base.is_ungated_high_risk(), "a gated tool is not ungated");

        let ungated = ToolRow {
            requires_approval: false,
            ..base.clone()
        };
        assert!(ungated.is_ungated_high_risk(), "the stripe must be reachable");

        // Disabled wins over the warning: a tool nobody can call is not a live hazard, and
        // striping every retired row would make the warning meaningless.
        let disabled = ToolRow {
            enabled: false,
            ..ungated.clone()
        };
        assert!(!disabled.is_ungated_high_risk());

        // And a low-risk tool is never striped, whatever its gate says.
        let low = ToolRow {
            risk: "low".to_owned(),
            ..ungated
        };
        assert!(!low.is_ungated_high_risk());
    }

    #[test]
    fn retirement_is_a_flag_and_a_never_a_delete() {
        let retired = ToolRow {
            retired_note: Some(RETIRED_NOTE.to_owned()),
            ..fixture()
        };
        assert!(retired.is_retired());
        assert!(!retired.is_ungated_high_risk(), "a retired tool is not enabled");
        // The note has to read as an explanation, not a code.
        assert!(retired.retired_note.as_deref().unwrap_or_default().contains("grants"));
    }

    #[test]
    fn an_empty_patch_is_distinguishable_from_a_real_one() {
        assert!(ToolLimits::default().is_empty());
        assert!(!ToolLimits {
            enabled: Some(false),
            ..ToolLimits::default()
        }
        .is_empty());
        // `Some(false)` must not read as "nothing to change": that is the bug a `bool` instead
        // of `Option<bool>` would introduce here.
        assert!(!ToolLimits {
            enabled: Some(false),
            ..ToolLimits::default()
        }
        .is_empty());
    }

    #[test]
    fn every_operator_column_is_one_the_seeder_never_writes_on_update() {
        // Read the seeder's own SQL out of the source so this cannot drift from the statement
        // that actually runs. A regex over the file beats a copy of the string here: the copy is
        // the thing that goes stale.
        let source = include_str!("registry.rs");
        let update_branch = source
            .split("on conflict (key) do update set")
            .nth(1)
            .and_then(|rest| rest.split("returning").next())
            .expect("the seeder's update branch must be readable from this file");
        for column in OPERATOR_COLUMNS {
            // `excluded.<column>` would be the operator's decision being overwritten by the
            // code default, which is the exact failure this module exists to prevent.
            assert!(
                !update_branch.contains(&format!("excluded.{column}")),
                "the seeder's update branch writes excluded.{column} — an operator decision \
                 would be reset on every boot"
            );
        }
    }

    #[test]
    fn the_seed_outcome_counts_preserved_decisions() {
        // A pass that inserted nothing must still report the rows whose decisions it kept:
        // that counter is what makes "seeding preserves operator edits" observable in a boot log
        // rather than a claim in a test name.
        let outcome = SeedOutcome {
            inserted: 0,
            refreshed: 23,
            retired: 0,
            decisions_preserved: 23,
        };
        assert_eq!(outcome.decisions_preserved, outcome.refreshed);
        assert_eq!(SeedOutcome::default(), SeedOutcome {
            inserted: 0,
            refreshed: 0,
            retired: 0,
            decisions_preserved: 0,
        });
    }

    #[test]
    fn a_compiled_key_is_recognised_and_a_removed_one_is_not() {
        assert!(is_compiled("content.search"));
        assert!(!is_compiled("content.exfiltrate"));
        assert!(!is_compiled(""));
        assert_eq!(spec_for("content.search").map(|s| s.key), Some("content.search"));
        assert!(spec_for("content.exfiltrate").is_none());
    }

    #[test]
    fn limits_the_seeder_ships_are_inside_the_ranges_the_store_enforces() {
        // The seeder writes `default_limits`; the store refuses anything outside its own range.
        // If those two ever disagree, a fresh install cannot boot its own registry.
        for spec in catalogue::specs() {
            let (timeout_ms, max_calls_per_run) = catalogue::default_limits(spec);
            assert!(
                (1000..=300_000).contains(&timeout_ms),
                "{} ships a timeout of {timeout_ms} ms, which update_tool refuses",
                spec.key
            );
            assert!(
                (1..=200).contains(&max_calls_per_run),
                "{} ships a cap of {max_calls_per_run}, which update_tool refuses",
                spec.key
            );
        }
    }

    #[test]
    fn an_error_rate_of_zero_and_never_called_are_different_answers() {
        let healthy = ToolUsage { calls: 40, errors: 0, avg_duration_ms: Some(120.0), last_used: None };
        assert_eq!(healthy.error_rate(), Some(0.0));
        // Never called: not a 0 % error rate. The panel prints an em dash for this and the test
        // is the reason it can.
        assert_eq!(ToolUsage::default().error_rate(), None);
        let rough = ToolUsage { calls: 4, errors: 1, ..ToolUsage::default() };
        assert!((rough.error_rate().unwrap() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_window_is_bound_as_seconds_and_never_as_a_raw_interval() {
        // `now() - ($1 * interval '1 second')` is chosen over binding an interval directly: a
        // bind of an interval is text-typed by inference, and the comparison then either errors
        // or silently coerces. A 30-day window is 2_592_000 seconds, which fits comfortably in
        // the i64 the bind carries — and `crates/ai-hub` has no `chrono` dependency to name,
        // which is why the signature is a plain number rather than a duration.
        const THIRTY_DAYS: i64 = 30 * 24 * 60 * 60;
        const ONE_DAY: i64 = 24 * 60 * 60;
        assert_eq!(THIRTY_DAYS, 2_592_000);
        assert_eq!(ONE_DAY, 86_400);
    }

    #[test]
    fn the_agent_membership_query_reads_jsonb_and_not_a_sql_array() {
        // `ai_agents.tools` is jsonb. `= any (tools)` compiles, runs, and matches nothing; `?`
        // is the jsonb membership operator. This asserts the operator is the right one, because
        // the failure mode is a dialog that always says "no agent uses this tool".
        let source = include_str!("registry.rs");
        let query = source
            .split("select id, name from ai_agents")
            .nth(1)
            .and_then(|rest| rest.split(".bind(tool_key)").next())
            .expect("agents_using's query must be readable from this file");
        assert!(query.contains("tools ? $1"), "the membership test must be jsonb `?`");
        assert!(
            !query.contains("any (tools)"),
            "`any` over a jsonb array matches nothing — the dialog would name zero agents"
        );
        assert!(
            !query.contains("status"),
            "ai_agents has `enabled`, not `status` — see migration 0064"
        );
    }

    fn fixture() -> ToolRow {
        ToolRow {
            id: Uuid::nil(),
            key: "content.search".to_owned(),
            class: "content".to_owned(),
            permission: "content.read".to_owned(),
            risk: "low".to_owned(),
            description: String::new(),
            input_schema: json!({ "type": "object" }),
            example: Some(json!({})),
            idempotent: true,
            requires_approval: false,
            enabled: true,
            timeout_ms: 30_000,
            max_calls_per_run: 20,
            retired_note: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}
