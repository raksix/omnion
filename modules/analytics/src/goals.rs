//! Goals, funnels and hit recording (REQ-007, slice 3).
//!
//! A goal is the platform's definition of a conversion: a `pageview`, `event`, `download` or
//! `form_submit` match — and, when the visitor should do more than one thing, an ordered list of
//! steps. The visitor's own progress is what makes a funnel readable:
//!
//! * **Ordered.** A step is only reached when every earlier step already was: a visitor who lands
//!   on the pricing page before the landing page has not progressed through a funnel that starts
//!   at the landing page. One beacon may carry a visitor more than one step forward, because a
//!   batch that holds the download *and* the form submit holds both facts.
//! * **Deduplicated.** `analytics_goal_hits` carries a unique `(goal, visitor, step)` key and
//!   every write is an `on conflict do nothing`, so re-sending the same beacon cannot
//!   double-count — the number a funnel shows is a count of visitors, not of requests.
//! * **Opaque.** A hit stores the daily-salted visitor hash and nothing else about the person:
//!   no address, no user agent, no form field. The event the platform emits on a completed goal
//!   carries the same opaque handle.
//!
//! The read side answers two shapes: a list of goals with their conversions and rate for a range,
//! and one goal's funnel. Both count a step as *reached* by the visitor whose furthest position
//! inside the range is that step or beyond — which is what keeps the counts monotonically
//! non-increasing, and therefore readable, even when a visitor's earlier step happened before the
//! range began.
//!
//! A note on the visitor hash: it rotates daily (that is the cookieless promise). A funnel is
//! therefore a **same-day** funnel — a returning visitor is a new visitor tomorrow, in a funnel
//! as everywhere else in this module.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AnalyticsError, Result};
use crate::reports::DateRange;

/// The kinds a goal — or one of its steps — can match.
pub const KINDS: [&str; 4] = ["pageview", "event", "download", "form_submit"];

/// Longest funnel a goal may describe.
pub const MAX_STEPS: usize = 5;

/// Longest goal name, matching the schema check.
pub const MAX_NAME_LENGTH: usize = 120;

/// Longest pattern a match may carry.
pub const MAX_PATTERN_LENGTH: usize = 200;

/// Columns of a goal, in [`GoalRow`] order.
const GOAL_COLUMNS: &str = "id, site_id, name, kind, match, enabled, created_by, created_at";

/// Columns of a goal step, in [`StepRow`] order.
const STEP_COLUMNS: &str = "goal_id, position, kind, match";

// ---------------------------------------------------------------------------------------------
// The match
// ---------------------------------------------------------------------------------------------

/// What a goal (or a step) matches: a path, an event name, a downloaded file — or a combination.
///
/// Every pattern is a plain string that is read as a glob when it carries `*` or `?`, and as the
/// page itself otherwise (a page named `/pricing` also matches `/pricing?utm=x`, because the
/// stored path carries its query string).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalMatch {
    /// Path pattern (`pageview`, and an optional narrowing for the other kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Event name (for `event`) or form name (for `form_submit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Downloaded file pattern (for `download`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl GoalMatch {
    /// A trimmed copy: an all-whitespace pattern is no pattern at all, and a pattern with a
    /// trailing space would only ever match by accident.
    #[must_use]
    pub fn normalised(self) -> Self {
        Self {
            path: clean(self.path),
            name: clean(self.name),
            file: clean(self.file),
        }
    }

    /// The `jsonb` value stored in the row.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Read a stored `jsonb` value. A row written by hand and unreadable becomes an empty match:
    /// validation refuses one for a new goal, and matching treats it as matching nothing.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// Trim a pattern, mapping blank to absent.
fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Input: changes and validation
// ---------------------------------------------------------------------------------------------

/// One step of a funnel, as the editor sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepChanges {
    /// `pageview`, `event`, `download` or `form_submit`.
    pub kind: String,
    /// What the step matches.
    #[serde(default, rename = "match")]
    pub matches: GoalMatch,
}

/// A goal to create — or the full description a re-write carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalChanges {
    /// Display name, unique inside the site.
    pub name: String,
    /// Kind of the conversion (the last step when the goal has steps).
    pub kind: String,
    /// What the conversion matches.
    #[serde(default, rename = "match")]
    pub matches: GoalMatch,
    /// Whether the goal records hits at all.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Ordered steps; empty means a single-step goal built from `kind` + `match`.
    #[serde(default)]
    pub steps: Vec<StepChanges>,
}

/// A goal's enable switch defaults to on: a goal the operator just described is one they want.
fn default_enabled() -> bool {
    true
}

/// A partial update: `null` keeps what the row carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalPatch {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New kind of the last step.
    #[serde(default)]
    pub kind: Option<String>,
    /// New match of the last step.
    #[serde(default, rename = "match")]
    pub matches: Option<GoalMatch>,
    /// Switch the goal on or off.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Replace the funnel wholesale.
    #[serde(default)]
    pub steps: Option<Vec<StepChanges>>,
}

/// One validated step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Kind of the step.
    pub kind: String,
    /// What the step matches.
    pub matches: GoalMatch,
}

/// A validated goal: the name, one to [`MAX_STEPS`] steps and the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalised {
    /// Trimmed name.
    pub name: String,
    /// The funnel, position 1 first.
    pub steps: Vec<Step>,
    /// The enable switch.
    pub enabled: bool,
}

/// Validate a goal description, naming the field that failed.
pub fn validate(changes: &GoalChanges) -> Result<Normalised> {
    let name = changes.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(AnalyticsError::InvalidGoal(format!(
            "a goal name is 1–{MAX_NAME_LENGTH} characters"
        )));
    }

    let steps = if changes.steps.is_empty() {
        vec![validated_step(&changes.kind, changes.matches.clone())?]
    } else {
        if changes.steps.len() > MAX_STEPS {
            return Err(AnalyticsError::InvalidGoal(format!(
                "a funnel is 1–{MAX_STEPS} steps"
            )));
        }

        changes
            .steps
            .iter()
            .map(|entry| validated_step(&entry.kind, entry.matches.clone()))
            .collect::<Result<Vec<_>>>()?
    };

    Ok(Normalised {
        name,
        steps,
        enabled: changes.enabled,
    })
}

/// Validate one step: a known kind, and the match that kind needs to mean anything.
fn validated_step(kind: &str, matches: GoalMatch) -> Result<Step> {
    let kind = kind.trim().to_owned();
    if !KINDS.contains(&kind.as_str()) {
        return Err(AnalyticsError::InvalidGoal(format!(
            "\"{kind}\" is not one of {}",
            KINDS.join(", ")
        )));
    }

    let matches = matches.normalised();
    for pattern in [
        matches.path.as_deref(),
        matches.name.as_deref(),
        matches.file.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if pattern.chars().count() > MAX_PATTERN_LENGTH {
            return Err(AnalyticsError::InvalidGoal(format!(
                "a pattern is at most {MAX_PATTERN_LENGTH} characters"
            )));
        }
    }

    let describes = match kind.as_str() {
        "pageview" => matches.path.is_some(),
        "event" => matches.name.is_some(),
        "download" => matches.file.is_some() || matches.path.is_some(),
        _ => matches.name.is_some() || matches.path.is_some(),
    };
    if !describes {
        return Err(AnalyticsError::InvalidGoal(
            match kind.as_str() {
                "pageview" => "a pageview goal needs a path",
                "event" => "an event goal needs the event name",
                "download" => "a download goal needs a file or a path",
                _ => "a form goal needs the form name or a path",
            }
            .to_owned(),
        ));
    }

    Ok(Step { kind, matches })
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// One thing that happened in one beacon, in the shape a goal can match.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fact<'a> {
    /// `pageview`, `event`, `download` or `form_submit`.
    pub kind: &'a str,
    /// Page the fact happened on, when it happened on one.
    pub path: Option<&'a str>,
    /// Event name, or form name for a submission.
    pub name: Option<&'a str>,
    /// Downloaded file, for a download.
    pub file: Option<&'a str>,
    /// Monetary value the caller attached, when there was one.
    pub value: Option<f64>,
}

/// `true` when a page matches a pattern.
///
/// A pattern with `*` or `?` is a glob over the whole path; anything else is the page itself,
/// and the stored path's query string (or fragment) does not make it a different page.
#[must_use]
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        return glob_matches(pattern, path);
    }
    if path == pattern {
        return true;
    }

    path.starts_with(pattern)
        && path
            .as_bytes()
            .get(pattern.len())
            .is_some_and(|byte| *byte == b'?' || *byte == b'#')
}

/// A small glob matcher: `*` matches any run (including none), `?` matches one character.
#[must_use]
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }

    pattern[p..].iter().all(|character| *character == '*')
}

/// `true` when one fact is what a step's kind and match describe.
#[must_use]
pub fn fact_matches(kind: &str, matches: &GoalMatch, fact: &Fact<'_>) -> bool {
    if kind != fact.kind {
        return false;
    }

    match kind {
        "pageview" => match (matches.path.as_deref(), fact.path) {
            (Some(pattern), Some(path)) => path_matches(pattern, path),
            _ => false,
        },
        "event" => {
            let named = match matches.name.as_deref() {
                Some(name) => fact.name == Some(name),
                None => false,
            };
            named && path_holds(matches.path.as_deref(), fact.path)
        }
        "download" => {
            let file = match matches.file.as_deref() {
                Some(pattern) => fact.file.is_some_and(|file| path_matches(pattern, file)),
                None => true,
            };
            file && path_holds(matches.path.as_deref(), fact.path)
                && (matches.file.is_some() || matches.path.is_some())
        }
        "form_submit" => {
            let form = match matches.name.as_deref() {
                Some(name) => fact.name == Some(name),
                None => true,
            };
            form && path_holds(matches.path.as_deref(), fact.path)
                && (matches.name.is_some() || matches.path.is_some())
        }
        _ => false,
    }
}

/// An optional path condition: absent means the fact may be anywhere.
fn path_holds(pattern: Option<&str>, path: Option<&str>) -> bool {
    match (pattern, path) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(pattern), Some(path)) => path_matches(pattern, path),
    }
}

/// The first step this visitor has not reached yet, when the funnel still has one.
///
/// This is the whole ordering rule: progress only ever moves to the earliest missing step, so a
/// later step cannot be reached before an earlier one.
#[must_use]
pub fn eligible_step(hits: &BTreeSet<i32>, steps: usize) -> Option<i32> {
    let steps = i32::try_from(steps).ok()?;
    (1..=steps).find(|position| !hits.contains(position))
}

/// Fold a visitor-count-by-furthest-position vector into reached counts.
///
/// `furthest[i]` is how many visitors stopped exactly at step `i + 1`; the answer is how many
/// reached at least each step, which is monotonically non-increasing by construction.
#[must_use]
pub fn reached_from_furthest(furthest: &[i64]) -> Vec<i64> {
    let mut reached = vec![0i64; furthest.len()];
    let mut running = 0i64;
    for index in (0..furthest.len()).rev() {
        running += furthest[index];
        reached[index] = running;
    }
    reached
}

/// A ratio that refuses to divide by nothing: no visitors is "not measured", not zero.
#[must_use]
pub fn ratio(part: i64, total: i64) -> Option<f64> {
    if total <= 0 {
        return None;
    }
    Some(part.max(0) as f64 / total as f64)
}

// ---------------------------------------------------------------------------------------------
// The read shapes
// ---------------------------------------------------------------------------------------------

/// One step of a goal, as the screens read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalStep {
    /// 1-based position inside the funnel.
    pub position: i32,
    /// Step kind.
    pub kind: String,
    /// What the step matches.
    #[serde(rename = "match")]
    pub matches: GoalMatch,
}

/// A goal with its steps, as the editor reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Goal {
    /// Identifier.
    pub id: Uuid,
    /// Site the goal belongs to.
    pub site_id: Uuid,
    /// Display name.
    pub name: String,
    /// Kind of the conversion (the last step).
    pub kind: String,
    /// What the conversion matches (the last step).
    #[serde(rename = "match")]
    pub matches: GoalMatch,
    /// Whether the goal records hits.
    pub enabled: bool,
    /// Account that created it.
    pub created_by: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// The funnel, position 1 first.
    pub steps: Vec<GoalStep>,
}

/// A goal plus how it did in a range — the goal list's row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoalSummary {
    /// Identifier.
    pub id: Uuid,
    /// Site the goal belongs to.
    pub site_id: Uuid,
    /// Display name.
    pub name: String,
    /// Kind of the conversion (the last step).
    pub kind: String,
    /// What the conversion matches (the last step).
    #[serde(rename = "match")]
    pub matches: GoalMatch,
    /// Whether the goal records hits.
    pub enabled: bool,
    /// Account that created it.
    pub created_by: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// The funnel, position 1 first.
    pub steps: Vec<GoalStep>,
    /// Visitors that reached the last step inside the range.
    pub conversions: i64,
    /// Visitors the range counts at all.
    pub visitors: i64,
    /// `conversions / visitors`, absent when the range met nobody.
    pub rate: Option<f64>,
    /// Last hit inside the range.
    pub last_hit: Option<OffsetDateTime>,
}

/// One step of a funnel, with the count of visitors that reached it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunnelStep {
    /// 1-based position.
    pub position: i32,
    /// Step kind.
    pub kind: String,
    /// What the step matches.
    #[serde(rename = "match")]
    pub matches: GoalMatch,
    /// Visitors that reached this step or beyond inside the range.
    pub visitors: i64,
    /// Visitors lost between the previous step and this one.
    pub drop_off: i64,
    /// `visitors / all visitors`, absent when the range met nobody.
    pub rate: Option<f64>,
}

/// One goal's funnel for a range.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Funnel {
    /// Goal the funnel belongs to.
    pub goal_id: Uuid,
    /// Goal name.
    pub name: String,
    /// Whether the goal records hits.
    pub enabled: bool,
    /// First day of the range.
    pub from: Date,
    /// Last day of the range.
    pub to: Date,
    /// Visitors the range counts at all.
    pub visitors: i64,
    /// Visitors that reached the last step.
    pub conversions: i64,
    /// `conversions / visitors`, absent when the range met nobody.
    pub rate: Option<f64>,
    /// The steps, position 1 first.
    pub steps: Vec<FunnelStep>,
}

/// A hit that was just recorded, for the caller that emits the platform event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReachedGoal {
    /// Goal the hit belongs to.
    pub goal_id: Uuid,
    /// Goal name, so a listener does not have to look it up.
    pub goal: String,
    /// Step the visitor reached.
    pub step_position: i32,
    /// `true` when this was the last step — the conversion itself.
    pub is_final: bool,
    /// The opaque, daily-rotated visitor handle (a hash; never an address).
    pub visitor: String,
    /// Page the fact happened on, when it happened on one.
    pub path: Option<String>,
    /// Value the caller attached, when there was one.
    pub value: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

/// A row of `analytics_goals`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct GoalRow {
    id: Uuid,
    site_id: Uuid,
    name: String,
    kind: String,
    #[sqlx(rename = "match")]
    matches: serde_json::Value,
    enabled: bool,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
}

/// A row of `analytics_goal_steps`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct StepRow {
    goal_id: Uuid,
    position: i32,
    kind: String,
    #[sqlx(rename = "match")]
    matches: serde_json::Value,
}

/// A goal's steps as the recorder needs them.
struct GoalSteps {
    name: String,
    steps: Vec<Step>,
}

// ---------------------------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------------------------

/// Every goal of a site with how it did in a range, ordered by name.
pub async fn list(pool: &PgPool, site_id: Uuid, range: DateRange) -> Result<Vec<GoalSummary>> {
    let sql =
        format!("select {GOAL_COLUMNS} from analytics_goals where site_id = $1 order by name");
    let rows: Vec<GoalRow> = sqlx::query_as(&sql).bind(site_id).fetch_all(pool).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let mut steps = load_steps(pool, &ids).await?;
    let (from, to) = range.bounds();
    let progress = progress_counts(pool, &ids, from, to).await?;
    let last = last_hits(pool, &ids, from, to).await?;
    let visitors = visitors_in_range(pool, site_id, from, to).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let goal_steps = steps.remove(&row.id).unwrap_or_default();
            let by_position = progress.get(&row.id).cloned().unwrap_or_default();
            let reached = reached_from_furthest(&furthest_vector(&by_position, goal_steps.len()));
            let conversions = reached.last().copied().unwrap_or(0);

            GoalSummary {
                id: row.id,
                site_id: row.site_id,
                name: row.name,
                kind: row.kind,
                matches: GoalMatch::from_value(&row.matches),
                enabled: row.enabled,
                created_by: row.created_by,
                created_at: row.created_at,
                steps: goal_steps,
                conversions,
                visitors,
                rate: ratio(conversions, visitors),
                last_hit: last.get(&row.id).copied(),
            }
        })
        .collect())
}

/// One goal of a site, with its steps.
pub async fn get(pool: &PgPool, site_id: Uuid, goal_id: Uuid) -> Result<Goal> {
    let sql = format!("select {GOAL_COLUMNS} from analytics_goals where id = $1 and site_id = $2");
    let row: Option<GoalRow> = sqlx::query_as(&sql)
        .bind(goal_id)
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    let row = row.ok_or(AnalyticsError::GoalNotFound)?;
    let steps = load_steps(pool, &[goal_id])
        .await?
        .remove(&goal_id)
        .unwrap_or_default();

    Ok(Goal {
        id: row.id,
        site_id: row.site_id,
        name: row.name,
        kind: row.kind,
        matches: GoalMatch::from_value(&row.matches),
        enabled: row.enabled,
        created_by: row.created_by,
        created_at: row.created_at,
        steps,
    })
}

/// Create a goal and its steps in one transaction.
pub async fn create(
    pool: &PgPool,
    site_id: Uuid,
    created_by: Option<Uuid>,
    changes: &GoalChanges,
) -> Result<Goal> {
    let normalised = validate(changes)?;
    let id = Uuid::new_v4();
    let last = last_step(&normalised);

    let mut transaction = pool.begin().await?;
    let insert = sqlx::query(
        "insert into analytics_goals (id, site_id, name, kind, match, enabled, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(site_id)
    .bind(&normalised.name)
    .bind(&last.kind)
    .bind(last.matches.to_value())
    .bind(normalised.enabled)
    .bind(created_by)
    .execute(&mut *transaction)
    .await;

    match insert {
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => {
            return Err(AnalyticsError::GoalNameTaken);
        }
        Err(error) => return Err(error.into()),
    }

    write_steps(&mut transaction, id, &normalised.steps).await?;
    transaction.commit().await?;

    get(pool, site_id, id).await
}

/// Apply a partial update: what the body carries is replaced, what it leaves out is kept.
///
/// `kind` and `match` name the **last step** (the conversion), which is what the goal row
/// mirrors; a body that carries `steps` replaces the whole funnel and its last step becomes the
/// mirror. A body that only carries `enabled` is a switch, and carries nothing else.
pub async fn patch(
    pool: &PgPool,
    site_id: Uuid,
    goal_id: Uuid,
    changes: &GoalPatch,
) -> Result<Goal> {
    let current = get(pool, site_id, goal_id).await?;

    let replace_steps = changes
        .steps
        .as_ref()
        .is_some_and(|steps| !steps.is_empty());
    let mut steps: Vec<StepChanges> = if replace_steps {
        changes.steps.clone().unwrap_or_default()
    } else {
        current
            .steps
            .iter()
            .map(|step| StepChanges {
                kind: step.kind.clone(),
                matches: step.matches.clone(),
            })
            .collect()
    };

    if steps.is_empty() {
        steps.push(StepChanges {
            kind: current.kind.clone(),
            matches: current.matches.clone(),
        });
    }

    if !replace_steps {
        if let Some(last) = steps.last_mut() {
            if let Some(kind) = &changes.kind {
                last.kind = kind.clone();
            }
            if let Some(matches) = &changes.matches {
                last.matches = matches.clone();
            }
        }
    }

    let kind = steps
        .last()
        .map(|step| step.kind.clone())
        .unwrap_or_else(|| current.kind.clone());
    let matches = steps
        .last()
        .map(|step| step.matches.clone())
        .unwrap_or_else(|| current.matches.clone());
    let merged = GoalChanges {
        name: changes.name.clone().unwrap_or_else(|| current.name.clone()),
        kind,
        matches,
        enabled: changes.enabled.unwrap_or(current.enabled),
        steps,
    };
    let normalised = validate(&merged)?;
    let last = last_step(&normalised);

    let mut transaction = pool.begin().await?;
    let update = sqlx::query(
        "update analytics_goals set name = $3, kind = $4, match = $5, enabled = $6 \
         where id = $1 and site_id = $2",
    )
    .bind(goal_id)
    .bind(site_id)
    .bind(&normalised.name)
    .bind(&last.kind)
    .bind(last.matches.to_value())
    .bind(normalised.enabled)
    .execute(&mut *transaction)
    .await;

    match update {
        Ok(result) if result.rows_affected() == 0 => {
            return Err(AnalyticsError::GoalNotFound);
        }
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => {
            return Err(AnalyticsError::GoalNameTaken);
        }
        Err(error) => return Err(error.into()),
    }

    sqlx::query("delete from analytics_goal_steps where goal_id = $1")
        .bind(goal_id)
        .execute(&mut *transaction)
        .await?;
    write_steps(&mut transaction, goal_id, &normalised.steps).await?;
    transaction.commit().await?;

    get(pool, site_id, goal_id).await
}

/// Delete a goal; its steps and hits go with it.
pub async fn delete(pool: &PgPool, site_id: Uuid, goal_id: Uuid) -> Result<()> {
    let result = sqlx::query("delete from analytics_goals where id = $1 and site_id = $2")
        .bind(goal_id)
        .bind(site_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AnalyticsError::GoalNotFound);
    }

    Ok(())
}

/// One goal's funnel for a range.
pub async fn funnel(
    pool: &PgPool,
    site_id: Uuid,
    goal_id: Uuid,
    range: DateRange,
) -> Result<Funnel> {
    let goal = get(pool, site_id, goal_id).await?;
    let (from, to) = range.bounds();
    let progress = progress_counts(pool, &[goal_id], from, to).await?;
    let by_position = progress.get(&goal_id).cloned().unwrap_or_default();
    let reached = reached_from_furthest(&furthest_vector(&by_position, goal.steps.len()));
    let visitors = visitors_in_range(pool, site_id, from, to).await?;

    let steps = goal
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let count = reached.get(index).copied().unwrap_or(0);
            let before = if index == 0 {
                count
            } else {
                reached.get(index - 1).copied().unwrap_or(count)
            };

            FunnelStep {
                position: step.position,
                kind: step.kind.clone(),
                matches: step.matches.clone(),
                visitors: count,
                drop_off: before - count,
                rate: ratio(count, visitors),
            }
        })
        .collect();
    let conversions = reached.last().copied().unwrap_or(0);

    Ok(Funnel {
        goal_id,
        name: goal.name,
        enabled: goal.enabled,
        from: range.from,
        to: range.to,
        visitors,
        conversions,
        rate: ratio(conversions, visitors),
        steps,
    })
}

// ---------------------------------------------------------------------------------------------
// Hit recording
// ---------------------------------------------------------------------------------------------

/// Record one beacon's facts against the site's enabled goals.
///
/// Returns the hits this call actually wrote, so the caller can emit the platform event — and
/// only for the hits it wrote: a re-sent beacon reports nothing, because it changed nothing.
pub async fn record_facts(
    pool: &PgPool,
    site_id: Uuid,
    visitor_hash: &str,
    occurred_at: OffsetDateTime,
    facts: &[Fact<'_>],
) -> Result<Vec<ReachedGoal>> {
    if facts.is_empty() {
        return Ok(Vec::new());
    }

    let goals = load_enabled_steps(pool, site_id).await?;
    if goals.is_empty() {
        return Ok(Vec::new());
    }

    let goal_ids: Vec<Uuid> = goals.keys().copied().collect();
    let mut hits = load_hits(pool, &goal_ids, visitor_hash).await?;

    let mut reached = Vec::new();
    for (goal_id, goal) in &goals {
        let positions = hits.entry(*goal_id).or_default();

        // At most one step per position: the set only grows, so the funnel cannot loop forever.
        for _ in 0..goal.steps.len() {
            let Some(position) = eligible_step(positions, goal.steps.len()) else {
                break;
            };
            let index = usize::try_from(position - 1).unwrap_or(0);
            let Some(step) = goal.steps.get(index) else {
                break;
            };
            let Some(fact) = facts
                .iter()
                .find(|fact| fact_matches(&step.kind, &step.matches, fact))
            else {
                break;
            };

            let inserted = sqlx::query(
                "insert into analytics_goal_hits (goal_id, visitor_hash, step_position, occurred_at) \
                 values ($1, $2, $3, $4) \
                 on conflict (goal_id, visitor_hash, step_position) do nothing",
            )
            .bind(goal_id)
            .bind(visitor_hash)
            .bind(position)
            .bind(occurred_at)
            .execute(pool)
            .await?
            .rows_affected()
                == 1;

            // Whether it was written now or already there, the visitor has this step.
            positions.insert(position);

            if inserted {
                reached.push(ReachedGoal {
                    goal_id: *goal_id,
                    goal: goal.name.clone(),
                    step_position: position,
                    is_final: usize::try_from(position).unwrap_or(usize::MAX) == goal.steps.len(),
                    visitor: visitor_hash.to_owned(),
                    path: fact.path.map(str::to_owned),
                    value: fact.value,
                });
            }
        }
    }

    Ok(reached)
}

/// Record one server-side conversion (REQ-064 forms, REQ-008 orders).
///
/// The modules that know a submission or a payment call this with the visitor hash they already
/// computed; the matching, ordering and deduplication are the same as for a beacon.
pub async fn record_conversion(
    pool: &PgPool,
    site_id: Uuid,
    visitor_hash: &str,
    occurred_at: OffsetDateTime,
    fact: Fact<'_>,
) -> Result<Vec<ReachedGoal>> {
    record_facts(pool, site_id, visitor_hash, occurred_at, &[fact]).await
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// The steps of several goals, keyed by goal.
async fn load_steps(pool: &PgPool, goal_ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<GoalStep>>> {
    let sql = format!(
        "select {STEP_COLUMNS} from analytics_goal_steps where goal_id = any($1) \
         order by goal_id, position"
    );
    let rows: Vec<StepRow> = sqlx::query_as(&sql).bind(goal_ids).fetch_all(pool).await?;

    let mut steps: HashMap<Uuid, Vec<GoalStep>> = HashMap::new();
    for row in rows {
        steps.entry(row.goal_id).or_default().push(GoalStep {
            position: row.position,
            kind: row.kind,
            matches: GoalMatch::from_value(&row.matches),
        });
    }

    Ok(steps)
}

/// The enabled goals of a site with their steps, in the shape the recorder matches against.
async fn load_enabled_steps(pool: &PgPool, site_id: Uuid) -> Result<HashMap<Uuid, GoalSteps>> {
    let rows: Vec<(Uuid, String, i32, String, serde_json::Value)> = sqlx::query_as(
        "select goals.id, goals.name, steps.position, steps.kind, steps.match \
         from analytics_goals goals \
         join analytics_goal_steps steps on steps.goal_id = goals.id \
         where goals.site_id = $1 and goals.enabled = true \
         order by goals.name, steps.position",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await?;

    let mut goals: HashMap<Uuid, GoalSteps> = HashMap::new();
    let mut ordered: HashMap<Uuid, Vec<(i32, Step)>> = HashMap::new();
    for (id, name, position, kind, matches) in rows {
        goals.entry(id).or_insert_with(|| GoalSteps {
            name,
            steps: Vec::new(),
        });
        ordered.entry(id).or_default().push((
            position,
            Step {
                kind,
                matches: GoalMatch::from_value(&matches),
            },
        ));
    }

    for (id, mut steps) in ordered {
        steps.sort_by_key(|(position, _)| *position);
        if let Some(goal) = goals.get_mut(&id) {
            goal.steps = steps.into_iter().map(|(_, step)| step).collect();
        }
    }

    Ok(goals)
}

/// The steps this visitor has already reached, per goal.
async fn load_hits(
    pool: &PgPool,
    goal_ids: &[Uuid],
    visitor_hash: &str,
) -> Result<HashMap<Uuid, BTreeSet<i32>>> {
    let rows: Vec<(Uuid, i32)> = sqlx::query_as(
        "select goal_id, step_position from analytics_goal_hits \
         where visitor_hash = $1 and goal_id = any($2)",
    )
    .bind(visitor_hash)
    .bind(goal_ids)
    .fetch_all(pool)
    .await?;

    let mut hits: HashMap<Uuid, BTreeSet<i32>> = HashMap::new();
    for (goal_id, position) in rows {
        hits.entry(goal_id).or_default().insert(position);
    }

    Ok(hits)
}

/// How many visitors stopped at each furthest position, per goal, inside a range.
async fn progress_counts(
    pool: &PgPool,
    goal_ids: &[Uuid],
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<HashMap<Uuid, HashMap<i32, i64>>> {
    let rows: Vec<(Uuid, i32, i64)> = sqlx::query_as(
        "with progress as ( \
             select goal_id, visitor_hash, max(step_position) as furthest \
             from analytics_goal_hits \
             where goal_id = any($1) and occurred_at >= $2 and occurred_at < $3 \
             group by goal_id, visitor_hash \
         ) \
         select goal_id, furthest, count(*)::bigint from progress \
         group by goal_id, furthest",
    )
    .bind(goal_ids)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let mut counts: HashMap<Uuid, HashMap<i32, i64>> = HashMap::new();
    for (goal_id, position, count) in rows {
        counts.entry(goal_id).or_default().insert(position, count);
    }

    Ok(counts)
}

/// The last hit of each goal inside a range.
async fn last_hits(
    pool: &PgPool,
    goal_ids: &[Uuid],
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<HashMap<Uuid, OffsetDateTime>> {
    let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "select goal_id, max(occurred_at) as last_hit from analytics_goal_hits \
         where goal_id = any($1) and occurred_at >= $2 and occurred_at < $3 \
         group by goal_id",
    )
    .bind(goal_ids)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

/// How many distinct visitors the range counts at all — the denominator of every rate.
async fn visitors_in_range(
    pool: &PgPool,
    site_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<i64> {
    let visitors: i64 = sqlx::query_scalar(
        "select count(distinct visitor_hash)::bigint from analytics_visits \
         where site_id = $1 and started_at >= $2 and started_at < $3",
    )
    .bind(site_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    Ok(visitors)
}

/// Turn per-furthest-position counts into the vector [`reached_from_furthest`] wants.
///
/// A furthest position beyond the funnel (a goal that was shortened after the fact) counts at
/// the last step instead of being dropped: the visitor did reach the end of what the goal is now.
fn furthest_vector(by_position: &HashMap<i32, i64>, steps: usize) -> Vec<i64> {
    let steps = steps.max(1);
    let mut vector = vec![0i64; steps];
    for (position, count) in by_position {
        let index = usize::try_from(*position).unwrap_or(1).clamp(1, steps) - 1;
        vector[index] += count;
    }

    vector
}

/// The step a goal mirrors in its own row: the last one.
fn last_step(normalised: &Normalised) -> Step {
    normalised.steps.last().cloned().unwrap_or(Step {
        kind: KINDS[0].to_owned(),
        matches: GoalMatch::default(),
    })
}

/// Insert the steps of a goal.
async fn write_steps(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    goal_id: Uuid,
    steps: &[Step],
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let position = i32::try_from(index).unwrap_or(0) + 1;
        sqlx::query(
            "insert into analytics_goal_steps (goal_id, position, kind, match) \
             values ($1, $2, $3, $4)",
        )
        .bind(goal_id)
        .bind(position)
        .bind(&step.kind)
        .bind(step.matches.to_value())
        .execute(&mut **transaction)
        .await?;
    }

    Ok(())
}

/// `true` when PostgreSQL refused a write because a unique key already held the value.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(inner) if inner.code().as_deref() == Some("23505"))
}

#[cfg(test)]
mod tests;
