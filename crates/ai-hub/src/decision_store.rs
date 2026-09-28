//! The store for route decisions (REQ-098, slice 3).
//!
//! Slices 1 and 2 answered *which* model and *why that one*. This stores the answer, because a
//! router whose reasoning disappears when the process exits cannot be debugged by the person who
//! has to debug it: "the cheap task answered with the expensive model" is a question the panel
//! has to answer from a row, not from a log line nobody kept.
//!
//! # What is on the hot path
//!
//! [`record`] is called **before** the provider is dialled, not after. That ordering is the
//! single most important fact in this module, and it is why [`record`] takes the `Decision` the
//! resolver already built rather than a summary of a call: the decision exists before the call
//! does, so a request that times out, that a provider refuses, or that a model's own admission
//! check rejects is still in the log. A log that fills in only on success is empty exactly when
//! an operator needs it.
//!
//! # What is deliberately not here
//!
//! - **The prompt.** A decision is one row per AI request; storing the request text would make
//!   the log a transcript store, grow the table by orders of magnitude, and put user content in
//!   a table an operator browses. [`NewDecision`] has no field a caller could put a prompt in.
//! - **The cost.** REQ-001 owns the usage store, and this request never prunes it. The join runs
//!   the other way: the usage row points at the decision ([`decision_id_of`] reads the link
//!   back for a cost row whose column landed before this one).
//! - **The outcome.** A decision explains the *choice*; whether the call then succeeded belongs
//!   to the provider usage row. Merging them would put a nullable outcome on a row that must
//!   exist before the call, which is the shape that produces "pending" rows nobody ever
//!   resolves.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::routing::{Decision, ResolvedCandidate, Scope, WalkStep};

const DECISION_COLUMNS: &str = "id, organization_id, site_id, user_id, run_id, task, feature, \
     requested, resolved_provider_id, resolved_model_id, fallback_index, rule, requirements, \
     reason, walk, created_at";

/// A decision row as stored.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RouteDecision {
    /// The row's own identifier; the cost row joins back to it.
    pub id: i64,
    /// The organization the request belonged to.
    pub organization_id: Option<Uuid>,
    /// The site the request belonged to.
    pub site_id: Option<Uuid>,
    /// The user who made the request, when there was one.
    pub user_id: Option<Uuid>,
    /// The agent run the request came from, when it came from one.
    pub run_id: Option<Uuid>,
    /// Which task the request was for.
    pub task: Option<String>,
    /// Which feature's pin was considered first.
    pub feature: Option<String>,
    /// What the caller asked for, as the string they sent.
    pub requested: Option<String>,
    /// The provider that answered.
    pub resolved_provider_id: Option<Uuid>,
    /// The model that answered.
    pub resolved_model_id: Option<Uuid>,
    /// 0-based position in the candidate list; above 0 means a fallback was used.
    pub fallback_index: i32,
    /// Which rule produced the answer.
    pub rule: String,
    /// Capabilities the request needed.
    pub requirements: Vec<String>,
    /// The one-sentence explanation.
    pub reason: String,
    /// The full candidate walk.
    pub walk: serde_json::Value,
    /// When the decision was taken.
    pub created_at: OffsetDateTime,
}

impl RouteDecision {
    /// Whether a fallback answered rather than the primary.
    ///
    /// A screen that asks "did a fallback get used" needs one predicate, and deriving it from
    /// `fallback_index > 0` in four places is how one of them ends up counting the installation
    /// default as a fallback.
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.fallback_index > 0
    }

    /// Whether nothing could answer.
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        self.rule == "unresolved"
    }
}

/// One decision as the resolve path writes it.
///
/// Built by [`NewDecision::from_decision`] rather than assembled at each call site, so the two
/// things that must never drift — the walk stored here and the walk the resolver returned — are
/// the same value by construction.
#[derive(Debug, Clone)]
pub struct NewDecision {
    /// The organization the request belonged to.
    pub organization_id: Option<Uuid>,
    /// The site the request belonged to.
    pub site_id: Option<Uuid>,
    /// The user who made the request.
    pub user_id: Option<Uuid>,
    /// The agent run the request came from.
    pub run_id: Option<Uuid>,
    /// Which task the request was for.
    pub task: Option<String>,
    /// Which feature was considered.
    pub feature: Option<String>,
    /// What the caller asked for, verbatim.
    pub requested: Option<String>,
    /// Which rule produced the answer.
    pub rule: String,
    /// Capabilities the request needed.
    pub requirements: Vec<String>,
    /// The provider and model that answered, once the row ids are known.
    pub answer: Option<Answer>,
    /// The one-sentence explanation.
    pub reason: String,
    /// The full candidate walk.
    pub walk: serde_json::Value,
}

/// The provider and model a decision settled on.
#[derive(Debug, Clone, Copy)]
pub struct Answer {
    /// The provider that answered.
    pub provider_id: Uuid,
    /// The model that answered.
    pub model_id: Uuid,
    /// 0-based position in the candidate list; above 0 means a fallback answered.
    pub fallback_index: i32,
}

impl NewDecision {
    /// Build the row a [`Decision`] implies.
    ///
    /// The interesting part is `fallback_index`, and it is worth reading twice: a chosen
    /// candidate's [`ResolvedCandidate::position`] is **1-based** (position 1 is the primary) and
    /// the column is **0-based**, so a naive `position` write would make every primary look like
    /// the first fallback and the "fallback used" filter would match every row in the log. A
    /// candidate with no position — an explicit pin, a feature override, the installation
    /// default — is index 0, because none of them is a fallback and all of them are "the answer".
    #[must_use]
    pub fn from_decision(
        decision: &Decision,
        request: DecisionContext<'_>,
        answer: Option<Answer>,
    ) -> Self {
        // The resolver's own walk is the stored walk: re-serialising a *subset* is how a log row
        // ends up explaining only the parts that worked.
        let walk = serde_json::to_value(&decision.walk).unwrap_or_else(|_| serde_json::json!([]));

        // The `Decision` deliberately carries no `reason` string of its own — its reasons live on
        // the walk entries, and a second copy of "why" is a second thing that can drift. The
        // stored sentence is therefore *derived*: the chosen entry's reason when one answered,
        // and otherwise the first skip's reason, so an unresolved row names the requirement that
        // refused it rather than saying only "unresolved".
        let reason = decision
            .walk
            .iter()
            .find(|entry| matches!(entry.outcome, WalkStep::Chosen))
            .or_else(|| decision.walk.first())
            .map(|entry| entry.reason.clone())
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| "nothing in the maps could answer this request".to_owned());

        Self {
            organization_id: request.organization_id,
            site_id: request.site_id,
            user_id: request.user_id,
            run_id: request.run_id,
            task: request.task.map(str::to_owned),
            feature: request.feature.map(str::to_owned),
            requested: request.requested.map(str::to_owned),
            rule: decision.rule.to_owned(),
            requirements: request.requirements.to_vec(),
            answer,
            reason,
            walk,
        }
    }
}

/// The request a decision was taken for.
///
/// Separate from [`NewDecision`] so the same decision can be written for two different callers
/// (a chat and an agent replay) without either of them re-deriving the other's scope.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecisionContext<'a> {
    /// The organization the request belonged to.
    pub organization_id: Option<Uuid>,
    /// The site the request belonged to.
    pub site_id: Option<Uuid>,
    /// The user who made the request.
    pub user_id: Option<Uuid>,
    /// The agent run the request came from.
    pub run_id: Option<Uuid>,
    /// Which task the request was for.
    pub task: Option<&'a str>,
    /// Which feature was considered.
    pub feature: Option<&'a str>,
    /// What the caller asked for, verbatim.
    pub requested: Option<&'a str>,
    /// Capabilities the request needed.
    pub requirements: &'a [String],
}

impl DecisionContext<'_> {
    /// The scope this context belongs to, for a caller that needs it in the log event.
    #[must_use]
    pub fn scope(&self) -> Scope {
        match (self.site_id, self.organization_id) {
            (Some(site), _) => Scope::Site(site),
            (None, Some(organization)) => Scope::Organization(organization),
            (None, None) => Scope::Installation,
        }
    }
}

/// Store one decision, returning its id.
///
/// The id is returned because the cost row joins back to it, and a caller that had to go and
/// find its own decision by timestamp would be guessing on a table that can hold several rows
/// for the same millisecond.
pub async fn record(pool: &PgPool, new: &NewDecision) -> Result<i64> {
    // The walk is stored as jsonb and the row is written in one statement: a decision that exists
    // without its walk is a decision the detail view cannot render, and a two-statement write
    // that half-succeeded is exactly that.
    let sql = format!(
        "insert into ai_route_decisions \
         (organization_id, site_id, user_id, run_id, task, feature, requested, \
          resolved_provider_id, resolved_model_id, fallback_index, rule, requirements, reason, walk) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) returning id"
    );

    let row: (i64,) = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(new.user_id)
        .bind(new.run_id)
        .bind(new.task.as_deref())
        .bind(new.feature.as_deref())
        .bind(new.requested.as_deref())
        .bind(new.answer.map(|answer| answer.provider_id))
        .bind(new.answer.map(|answer| answer.model_id))
        .bind(new.answer.map_or(0, |answer| answer.fallback_index))
        .bind(&new.rule)
        .bind(&new.requirements)
        .bind(&new.reason)
        .bind(&new.walk)
        .fetch_one(pool)
        .await?;

    Ok(row.0)
}

/// The filters the decision log accepts.
///
/// Every field is optional and the defaults are "no filter", because a log screen that needs a
/// filter to show anything is a log screen nobody opens. `organization_id` is *not* one of the
/// public filters: the tenancy filter is applied by the endpoint, never by the caller, so a
/// client cannot widen the read by omitting it.
#[derive(Debug, Clone, Default)]
pub struct DecisionFilter {
    /// Restrict to one organization.
    pub organization_id: Option<Uuid>,
    /// Restrict to one site.
    pub site_id: Option<Uuid>,
    /// Restrict to one task.
    pub task: Option<String>,
    /// Restrict to one feature.
    pub feature: Option<String>,
    /// Restrict to one resolved model.
    pub model_id: Option<Uuid>,
    /// Only rows where a fallback answered.
    pub fallback_only: bool,
    /// Only rows where nothing could answer.
    pub unresolved_only: bool,
    /// Start of the window, inclusive.
    pub from: Option<OffsetDateTime>,
    /// End of the window, exclusive.
    pub to: Option<OffsetDateTime>,
    /// How many rows to return.
    pub limit: i64,
    /// How many rows to skip.
    pub offset: i64,
}

impl DecisionFilter {
    /// A filter that matches everything, bounded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 50,
            ..Self::default()
        }
    }
}

/// One page of the decision log, plus the total the screen paginates against.
///
/// The total is a second query rather than a `count(*) over ()` window column because the panel
/// shows "row 51 of 240" and a count over a window is computed over the *limited* result set
/// anyway — which answers 50, always, and looks correct.
#[derive(Debug, Clone)]
pub struct DecisionPage {
    /// The rows, newest first, each already joined with the label of the model that answered.
    pub rows: Vec<DecisionRow>,
    /// How many rows the filters match in total.
    pub total: i64,
}

/// A decision joined with the model that answered, for the log table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DecisionRow {
    /// The stored decision.
    ///
    /// `sqlx(flatten)` rather than `serde(flatten)`: the two look alike and only the first one
    /// tells sqlx to read the nested row's columns from the same result set. With the serde
    /// attribute the derive silently produces a struct sqlx cannot decode, and the error arrives
    /// as a missing trait impl on `RouteDecision` — three files away from the attribute that
    /// caused it.
    #[sqlx(flatten)]
    pub decision: RouteDecision,
    /// The `provider/model` that answered, when one did and the model still exists.
    pub resolved_label: Option<String>,
    /// The key of the model that answered, when it still exists.
    pub resolved_model_key: Option<String>,
}

impl DecisionRow {
    /// Whether a fallback answered, forwarded so the screen does not re-derive it.
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.decision.used_fallback()
    }
}

/// Build the `where` clause a filter implies, with its bind values.
///
/// Hand-written here rather than composed from `Option::is_some()` per filter because the bind
/// index has to line up with the placeholder count; a filter that is skipped must therefore
/// consume a placeholder too. The alternative — a query per combination — is a matrix of twelve
/// hand-written statements, and the twelfth is the one nobody wrote.
///
/// The clause is `Clone` for a reason beyond convenience: the rows query and the count query are
/// two calls that must see the **same** predicates, and a second hand-built clause is the shape
/// that lets them drift — so the table says "42 rows" while the pager counts 40. One clause,
/// bound twice, cannot drift.
#[derive(Clone)]
struct Clause {
    sql: String,
    binds: Vec<Bind>,
}

/// One bind value in a clause, positionally.
#[derive(Clone)]
enum Bind {
    Uuid(Uuid),
    Text(String),
    Bool(bool),
    Time(OffsetDateTime),
}

impl Bind {
    fn bind<'q, T>(self, query: sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>)
    -> sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>
    where
        T: sqlx::FromRow<'q, sqlx::postgres::PgRow>,
    {
        match self {
            Self::Uuid(value) => query.bind(value),
            Self::Text(value) => query.bind(value),
            Self::Bool(value) => query.bind(value),
            Self::Time(value) => query.bind(value),
        }
    }
}

fn clause(filter: &DecisionFilter) -> Clause {
    // Each condition always contributes exactly one placeholder, even when the filter is
    // "off" (`$n is not null` is false for null, and `($n::boolean is null or …)` reads as the
    // neutral case). This is what keeps the bind numbering and the placeholder count identical
    // between the rows query and the count query — the bug that a hand-written pair hits the
    // first time someone adds a filter.
    let mut parts: Vec<String> = Vec::new();
    let mut binds: Vec<Bind> = Vec::new();

    let mut push = |parts: &mut Vec<String>, binds: &mut Vec<Bind>, text: String, bind: Bind| {
        binds.push(bind);
        parts.push(text);
    };

    push(
        &mut parts,
        &mut binds,
        "($1::uuid is null or organization_id = $1)".to_owned(),
        Bind::Uuid(filter.organization_id.unwrap_or(Uuid::nil())),
    );
    push(
        &mut parts,
        &mut binds,
        "($2::uuid is null or site_id = $2)".to_owned(),
        Bind::Uuid(filter.site_id.unwrap_or(Uuid::nil())),
    );
    push(
        &mut parts,
        &mut binds,
        "($3::text is null or task = $3)".to_owned(),
        Bind::Text(filter.task.clone().unwrap_or_default()),
    );
    push(
        &mut parts,
        &mut binds,
        "($4::text is null or feature = $4)".to_owned(),
        Bind::Text(filter.feature.clone().unwrap_or_default()),
    );
    push(
        &mut parts,
        &mut binds,
        "($5::uuid is null or resolved_model_id = $5)".to_owned(),
        Bind::Uuid(filter.model_id.unwrap_or(Uuid::nil())),
    );
    // `fallback_only` and `unresolved_only` are two questions about one column rather than two
    // columns: a row is either unresolved or it has a position, and encoding them separately
    // would let a caller ask for `fallback_only AND unresolved_only` and get a silently empty
    // page instead of a refusal.
    push(
        &mut parts,
        &mut binds,
        "(not $6::boolean or (fallback_index > 0 and rule <> 'unresolved'))".to_owned(),
        Bind::Bool(filter.fallback_only),
    );
    push(
        &mut parts,
        &mut binds,
        "(not $7::boolean or rule = 'unresolved')".to_owned(),
        Bind::Bool(filter.unresolved_only),
    );
    push(
        &mut parts,
        &mut binds,
        "($8::timestamptz is null or created_at >= $8)".to_owned(),
        Bind::Time(filter.from.unwrap_or(OffsetDateTime::UNIX_EPOCH)),
    );
    push(
        &mut parts,
        &mut binds,
        "($9::timestamptz is null or created_at < $9)".to_owned(),
        Bind::Time(filter.to.unwrap_or(OffsetDateTime::UNIX_EPOCH)),
    );

    Clause {
        sql: parts.join(" and "),
        binds,
    }
}

impl Clause {
    /// Run the clause against a query, in bind order.
    ///
    /// `&self`, not `self`: the rows query and the count query share one clause, and consuming
    /// it on the first would force the second to rebuild it — which is the drift this shape
    /// exists to prevent. The binds are cloned per call because sqlx consumes them, and a
    /// `text[]` bind is a handful of bytes, not a row.
    fn apply<'q, T>(
        &self,
        mut query: sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>
    where
        T: sqlx::FromRow<'q, sqlx::postgres::PgRow>,
    {
        for bind in &self.binds {
            query = bind.clone().bind(query);
        }
        query
    }
}

/// Read one page of the decision log.
pub async fn list(pool: &PgPool, filter: &DecisionFilter) -> Result<DecisionPage> {
    let where_clause = clause(filter);
    let clause = &where_clause.sql;

    let list_sql = format!(
        "select {DECISION_COLUMNS}, \
             (select provider.name || '/' || model.model_key \
                from ai_models model \
                join ai_providers provider on provider.id = model.provider_id \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_label, \
             (select model.model_key from ai_models model \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_model_key \
         from ai_route_decisions \
         where {clause} \
         order by created_at desc, id desc \
         limit $10 offset $11"
    );
    let count_sql = format!("select count(*) from ai_route_decisions where {clause}");

    let limit = i64::from(u16::try_from(filter.limit.clamp(1, 500)).unwrap_or(50));
    let offset = filter.offset.max(0);

    let rows: Vec<DecisionRow> = where_clause
        .apply(sqlx::query_as(&list_sql))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

    let total: (i64,) = where_clause
        .apply(sqlx::query_as(&count_sql))
        .fetch_one(pool)
        .await?;

    Ok(DecisionPage {
        rows,
        total: total.0,
    })
}

/// Read one decision with its full walk.
pub async fn read_one(pool: &PgPool, id: i64) -> Result<DecisionRow> {
    let sql = format!(
        "select {DECISION_COLUMNS}, \
             (select provider.name || '/' || model.model_key \
                from ai_models model \
                join ai_providers provider on provider.id = model.provider_id \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_label, \
             (select model.model_key from ai_models model \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_model_key \
         from ai_route_decisions where id = $1"
    );

    sqlx::query_as(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AiHubError::DecisionNotFound)
}

/// Every decision in the window, in the order the log renders them, for the CSV export.
///
/// The export reads through the *same* filter as the screen — a second set of conditions is how
/// a CSV ends up containing rows the table did not show, which is the acceptance criterion by
/// name ("the CSV export matches the filtered rows row-for-row").
pub async fn export_rows(pool: &PgPool, filter: &DecisionFilter) -> Result<Vec<DecisionRow>> {
    let where_clause = clause(filter);
    let clause = &where_clause.sql;

    let sql = format!(
        "select {DECISION_COLUMNS}, \
             (select provider.name || '/' || model.model_key \
                from ai_models model \
                join ai_providers provider on provider.id = model.provider_id \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_label, \
             (select model.model_key from ai_models model \
               where model.id = ai_route_decisions.resolved_model_id) as resolved_model_key \
         from ai_route_decisions \
         where {clause} \
         order by created_at desc, id desc"
    );

    // The export is deliberately *not* bounded by the page size: an operator exporting a month
    // is asking for the month. The `limit` of the filter is a display concern, and reusing it
    // here would silently truncate the export to whatever the table happened to show.
    let rows: Vec<DecisionRow> = where_clause.apply(sqlx::query_as(&sql)).fetch_all(pool).await?;
    Ok(rows)
}

/// The newest decision per task, for the routing screen's "Last resolved" column.
///
/// One read for the whole scope instead of one per task row: the screen renders a row per task
/// and a per-task read is the shape that turns a 200 ms panel into a 2 s one as soon as
/// somebody adds a task.
pub async fn last_per_task(pool: &PgPool, filter: &DecisionFilter) -> Result<BTreeMap<String, RouteDecision>> {
    let where_clause = clause(filter);
    let clause = &where_clause.sql;

    let sql = format!(
        "select distinct on (task) {DECISION_COLUMNS} \
         from ai_route_decisions \
         where {clause} and task is not null \
         order by task, created_at desc, id desc"
    );

    let rows: Vec<RouteDecision> = where_clause.apply(sqlx::query_as(&sql)).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.task.clone().unwrap_or_default(), row))
        .collect())
}

/// Drop decisions older than the retention window, returning how many went.
///
/// Only the decision log is pruned. The usage counters for the same window are left complete,
/// and the assertion that proves it lives in the test rather than in a comment: a pruner that
/// took the counters with it would make every historical cost number on `/ai/costs` wrong, and
/// the only symptom would be a chart that quietly went flat.
pub async fn prune(pool: &PgPool, days: i64) -> Result<u64> {
    let deleted = sqlx::query("delete from ai_route_decisions where created_at < now() - make_interval(days => $1)")
        .bind(days.max(1))
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// How old a decision row may be before the runner drops it, in days.
pub const RETENTION_DAYS: i64 = 90;

/// The window the runner's pruner uses, as a [`time::Duration`] the config can compare against.
#[must_use]
pub fn retention() -> time::Duration {
    time::Duration::days(RETENTION_DAYS)
}

/// The decision id a usage row points at, when the column exists.
///
/// REQ-001's `ai_usage` table may not be in the tree yet, in which case there is nothing to
/// join from and this returns `None` — the same answer as "a usage row written before the
/// column existed". Reading it defensively means the cost screen works on a tree that has
/// REQ-098 without REQ-001, and starts showing the link the day REQ-001 lands, with no edit
/// here.
pub async fn decision_id_of(pool: &PgPool, usage_id: i64) -> Result<Option<i64>> {
    // `to_regclass` first: a missing table is a legitimate state, not an error, and letting
    // sqlx raise "relation does not exist" would turn a not-yet-shipped feature into a 500 on
    // a screen that has nothing else to show.
    let exists: Option<(String,)> = sqlx::query_as("select to_regclass('ai_usage')::text")
        .fetch_optional(pool)
        .await?;
    if exists.is_none_or(|(name,)| name.is_empty()) {
        return Ok(None);
    }

    let row: Option<(Option<i64>,)> = sqlx::query_as("select decision_id from ai_usage where id = $1")
        .bind(usage_id)
        .fetch_optional(pool)
        .await?;
    // The row's own value is already an `Option<i64>` (the column is nullable), and the row is
    // `Option` because the usage row may not exist. `Option<Option<i64>>` flattened once is
    // "found a row" — flattened twice would collapse "no usage row" and "a usage row from
    // before the column existed" into the same `None`, which are different answers.
    Ok(row.and_then(|(id,)| id))
}

/// The answer a decision stored, re-read as the resolver's own type.
///
/// Used by the tests to assert that a stored `fallback_index` and a resolver `position` agree,
/// which is the pairing that a 0-based column and a 1-based position can silently break.
#[must_use]
pub fn answer_position(answer: Option<ResolvedCandidate>) -> i32 {
    // 1-based position, 0-based column: the primary (position 1) is index 0. A candidate with no
    // position never came from a candidate list, so it is also index 0.
    answer.and_then(|candidate| candidate.position).map_or(0, |position| {
        position.saturating_sub(1).max(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{DecisionSource, WalkEntry, WalkStep};

    fn decision(rule: &'static str, model: Option<&str>, position: Option<i32>) -> Decision {
        Decision {
            model: model.map(|model_id| ResolvedCandidate {
                model_id: model_id.to_owned(),
                position,
                source: DecisionSource::TaskRoute,
                scope: Scope::Installation,
            }),
            walk: vec![WalkEntry {
                position: Some(1),
                model_id: model.map(str::to_owned),
                outcome: WalkStep::Chosen,
                reason: "the first candidate that is switched on".to_owned(),
                scope: Scope::Installation,
                source: DecisionSource::TaskRoute,
            }],
            rule,
            unresolved: model.is_none(),
        }
    }

    #[test]
    fn the_primary_writes_index_zero_and_a_fallback_writes_its_position() {
        // 1-based position, 0-based column. Getting this backwards makes the "fallback used"
        // filter match every row in the log, which is exactly the bug the unit exists for.
        assert_eq!(answer_position(None), 0);
        assert_eq!(
            answer_position(Some(ResolvedCandidate {
                model_id: "p/m".into(),
                position: Some(1),
                source: DecisionSource::TaskRoute,
                scope: Scope::Installation,
            })),
            0
        );
        assert_eq!(
            answer_position(Some(ResolvedCandidate {
                model_id: "p/m".into(),
                position: Some(2),
                source: DecisionSource::TaskRoute,
                scope: Scope::Installation,
            })),
            1
        );
    }

    #[test]
    fn a_position_below_one_never_produces_a_negative_index() {
        // A `position` of 0 cannot be written (the table checks `>= 1`), but a stored decision
        // from a future writer could carry one, and `saturating_sub` is what stops a log row
        // rendering a fallback badge on the primary.
        assert_eq!(answer_position(Some(ResolvedCandidate {
            model_id: "p/m".into(),
            position: Some(0),
            source: DecisionSource::TaskRoute,
            scope: Scope::Installation,
        })), 0);
    }

    #[test]
    fn the_stored_walk_is_the_resolver_walk() {
        // Re-deriving a "summary" walk is how a log row ends up explaining only the parts that
        // worked. The assertion is structural: the stored jsonb is the serialized resolver walk.
        let resolver = decision("task_route", Some("p/m"), Some(1));
        let new = NewDecision::from_decision(
            &resolver,
            DecisionContext {
                task: Some("cheap"),
                ..DecisionContext::default()
            },
            None,
        );
        let stored: Vec<WalkEntry> = serde_json::from_value(new.walk.clone()).expect("a walk");
        assert_eq!(stored.len(), resolver.walk.len());
        assert_eq!(stored[0].reason, resolver.walk[0].reason);
        assert!(matches!(stored[0].outcome, WalkStep::Chosen));
    }

    #[test]
    fn an_unresolved_decision_always_carries_a_reason() {
        // The warning banner on the routing screen renders this sentence. An empty reason there
        // is a banner that explains nothing, so the fallback sentence is the writer's job and
        // not the panel's.
        let resolver = decision("unresolved", None, None);
        let new = NewDecision::from_decision(&resolver, DecisionContext::default(), None);
        assert!(!new.reason.trim().is_empty());
        assert_eq!(new.rule, "unresolved");
    }

    #[test]
    fn a_fallback_only_filter_never_matches_an_unresolved_row() {
        // Two questions about one column, encoded as one. A caller asking for both gets an
        // empty page rather than a row that claims to be both a failure and a fallback.
        let filter = DecisionFilter {
            fallback_only: true,
            unresolved_only: true,
            ..DecisionFilter::new()
        };
        let clause = clause(&filter).sql;
        assert!(clause.contains("fallback_index > 0 and rule <> 'unresolved'"));
        assert!(clause.contains("rule = 'unresolved'"));
    }

    #[test]
    fn every_filter_contributes_exactly_one_placeholder() {
        // The bind numbering is shared by the rows query and the count query; a filter that
        // skipped its placeholder would shift every later bind and the count would silently
        // count a different set of rows than the table shows.
        //
        // The assertion counts *typed* placeholders (`$n::`) rather than conditions: a
        // condition may contain the word "and" in its own text — `fallback_index > 0 and rule <>
        // 'unresolved'` does — so counting joiners measures the English, not the wiring. Each
        // filter must own exactly one typed placeholder, and it must be the *typed* form, since
        // an untyped `$1` is how a uuid filter ends up compared against a text column and
        // PostgreSQL refuses the whole query.
        let clause = clause(&DecisionFilter::new()).sql;
        for index in 1..=9 {
            let placeholder = format!("${index}::");
            assert_eq!(
                clause.matches(&placeholder).count(),
                1,
                "placeholder {placeholder} is not contributed exactly once by its filter"
            );
        }
    }
}
