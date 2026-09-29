//! Limits, usage and ownership transfer for an automation project (REQ-133, slice 4).
//!
//! Slice 1 shipped the entity and slice 2 the scoping; this file is the part where a project
//! acquires consequences. Three things live here, and they are connected by one idea: **a limit is
//! only a limit if something reads it at the moment it would be exceeded.**
//!
//! * [`transfer_ownership`] — the REQ asks for two confirmations and an audit row, because
//!   project ownership and workflow ownership are different things and the panel says so. Both
//!   writes (the project's `owner_user_id` and the membership role) happen in one transaction:
//!   a project whose owner column names one person and whose owner member is another is a project
//!   whose next editor has to guess which question to ask.
//! * [`ensure_run_allowed`] here — the limits half of the run guard. The archive half lives in
//!   [`crate::projects`], because an archived project refuses runs for a reason that is not about
//!   counting; the runs-per-day half lives here because it is.
//! * [`record_usage`] / [`usage_series`] — the counters behind the limits screen.
//!
//! ## The rule that shapes the counters
//!
//! **A `0` limit means "unlimited", not "none".** Every project is born with a row of zeros by
//! migration 0170, so the alternative reading makes a fresh installation unable to start a single
//! run — a dead platform with a working UI. It is a real decision rather than a convenience, and
//! it is asserted in [`Limit`]'s tests because the sentence lives in a comment until somebody
//! writes the opposite behaviour.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::projects::{self, ProjectRole};

// ---------------------------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------------------------

/// A project's limit overrides.
///
/// Every field is a count, and `0` is "no limit" throughout — see the module header. The struct is
/// what the limits screen renders and what `PUT /projects/{id}/limits` writes, so there is exactly
/// one definition of the shape and the API never builds one field at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Limits {
    /// The project these limits belong to.
    pub project_id: Uuid,
    /// Maximum workflows; `0` is unlimited.
    pub max_workflows: i32,
    /// Maximum credentials; `0` is unlimited.
    pub max_credentials: i32,
    /// Maximum runs per day; `0` is unlimited.
    pub max_runs_per_day: i32,
    /// Maximum runs in flight at once; `0` is unlimited.
    pub max_concurrent_runs: i32,
    /// Percent at which the screen warns.
    pub warn_at_percent: i32,
    /// Who last changed them.
    pub updated_by: Option<Uuid>,
    /// When they last changed.
    pub updated_at: OffsetDateTime,
}

impl Limits {
    /// A limit of zero imposes no cap.
    #[must_use]
    pub const fn is_unlimited(limit: i32) -> bool {
        limit == 0
    }

    /// Whether `current` has reached `limit`, naming the two numbers a refusal message needs.
    ///
    /// `None` means "no refusal" — either unlimited, or under the cap. The caller never has to
    /// remember which of the two, which is the whole point: a check written twice is a check that
    /// eventually disagrees with itself.
    #[must_use]
    pub fn exceeded(limit: i32, current: i64) -> Option<(i64, i32)> {
        if Self::is_unlimited(limit) || current < i64::from(limit) {
            None
        } else {
            Some((current, limit))
        }
    }

    /// Whether usage has crossed the warning threshold — the REQ's "soft warning at 80 percent".
    ///
    /// Strictly `>=`, so a limit of 1 warns on the first unit rather than on the second, and a
    /// project that reaches exactly its cap warned first. Unlimited never warns: a bar against an
    /// absent limit is a bar nobody can read.
    #[must_use]
    pub fn warns(limit: i32, current: i64, warn_at_percent: i32) -> bool {
        if Self::is_unlimited(limit) {
            return false;
        }
        let threshold = i64::from(limit) * i64::from(warn_at_percent) / 100;
        current >= threshold.max(1)
    }

    /// The refusal message: the limit, the number reached, and who to ask.
    ///
    /// It names all three because the REQ asks for a message "naming the limit and the project
    /// owner", and a message that names only the limit sends the reader to a screen to find out
    /// who to ask.
    #[must_use]
    pub fn refusal_message(
        limit_name: &str,
        current: i64,
        limit: i32,
        project_key: &str,
        owner_display: &str,
    ) -> String {
        format!(
            "{project_key} has reached its {limit_name} limit ({current} of {limit}) — ask {owner_display}, the project owner, to raise it"
        )
    }
}

/// Read a project's limits, creating the zero row if the project somehow has none.
///
/// The `on conflict do nothing` shape answers a missing row without a read first, and then reads it
/// back. Two statements, but the first cannot race: the insert is the arbiter.
///
/// Takes a **connection**, not a pool, because the caller of this function is the run guard inside
/// `store::create_execution_in` — which holds a transaction and must ask its question inside it. A
/// pool-based read there would be a second connection reading counters the open transaction is about
/// to change, which is the check-then-write shape the archive guard was moved away from.
pub async fn read_limits_in(
    connection: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<Limits> {
    sqlx::query(
        "insert into automation_project_limits (project_id) values ($1) \
         on conflict (project_id) do nothing",
    )
    .bind(project_id)
    .execute(&mut *connection)
    .await?;

    let limits = sqlx::query_as::<_, Limits>(
        "select project_id, max_workflows, max_credentials, max_runs_per_day, max_concurrent_runs, \
         warn_at_percent, updated_by, updated_at from automation_project_limits where project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| WorkflowError::invalid("project_not_found", "no such project"))?;
    Ok(limits)
}

/// [`read_limits_in`] for a caller with only a pool — the API's `GET /projects/{id}/limits`.
pub async fn read_limits(pool: &PgPool, project_id: Uuid) -> Result<Limits> {
    let mut connection = pool.acquire().await?;
    read_limits_in(&mut connection, project_id).await
}

/// Replace a project's limit overrides.
///
/// Validated in Rust with the same rule the migration's checks state, so a caller that posts `-1`
/// gets a message naming the field instead of a `23514` that reads in the panel as a broken form.
pub async fn set_limits(pool: &PgPool, project_id: Uuid, limits: LimitOverrides) -> Result<Limits> {
    for (name, value) in [
        ("max_workflows", limits.max_workflows),
        ("max_credentials", limits.max_credentials),
        ("max_runs_per_day", limits.max_runs_per_day),
        ("max_concurrent_runs", limits.max_concurrent_runs),
    ] {
        if value < 0 {
            return Err(WorkflowError::invalid(
                "invalid_project_limit",
                format!("{name} cannot be negative (it was {value}); use 0 for no limit"),
            ));
        }
    }
    if !(1..=100).contains(&limits.warn_at_percent) {
        return Err(WorkflowError::invalid(
            "invalid_warn_threshold",
            format!(
                "warn_at_percent is {}; it is between 1 and 100",
                limits.warn_at_percent
            ),
        ));
    }

    let limits = sqlx::query_as::<_, Limits>(
        "insert into automation_project_limits (project_id, max_workflows, max_credentials, \
         max_runs_per_day, max_concurrent_runs, warn_at_percent, updated_by) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (project_id) do update set max_workflows = excluded.max_workflows, \
         max_credentials = excluded.max_credentials, max_runs_per_day = excluded.max_runs_per_day, \
         max_concurrent_runs = excluded.max_concurrent_runs, \
         warn_at_percent = excluded.warn_at_percent, updated_by = excluded.updated_by, \
         updated_at = now() \
         returning project_id, max_workflows, max_credentials, max_runs_per_day, \
         max_concurrent_runs, warn_at_percent, updated_by, updated_at",
    )
    .bind(project_id)
    .bind(limits.max_workflows)
    .bind(limits.max_credentials)
    .bind(limits.max_runs_per_day)
    .bind(limits.max_concurrent_runs)
    .bind(limits.warn_at_percent)
    .bind(limits.updated_by)
    .fetch_one(pool)
    .await?;
    Ok(limits)
}

/// The four caps and the warning threshold, as a caller submits them.
///
/// **A struct rather than eight positional arguments.** The function grew one parameter per limit
/// the REQ listed, and clippy noticed at eight what was already true at four: two adjacent `i32`s
/// that mean different things are exactly the pair a caller swaps by accident, and the compiler
/// cannot catch it because they have the same type. Naming them once is the fix; the API body
/// deserialises straight into this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitOverrides {
    /// Maximum workflows; `0` is unlimited.
    pub max_workflows: i32,
    /// Maximum credentials; `0` is unlimited.
    pub max_credentials: i32,
    /// Maximum runs per day; `0` is unlimited.
    pub max_runs_per_day: i32,
    /// Maximum runs in flight at once; `0` is unlimited.
    pub max_concurrent_runs: i32,
    /// Percent at which the screen warns.
    pub warn_at_percent: i32,
    /// Who is making the change, for the row's `updated_by`.
    pub updated_by: Option<Uuid>,
}

impl LimitOverrides {
    /// The same overrides with a different warning threshold, for the one test that checks it.
    #[must_use]
    pub const fn warn(mut self, percent: i32) -> Self {
        self.warn_at_percent = percent;
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// One day's counters for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct UsageDay {
    /// The day, as a date.
    pub usage_date: time::Date,
    /// Runs started.
    pub runs: i32,
    /// Runs that failed.
    pub failures: i32,
    /// Total compute time in milliseconds.
    pub compute_ms: i64,
}

/// Today's counters for a project, at zero rather than absent.
///
/// **A missing row is a zero day, not a missing day.** The limits screen reads this for every
/// project it shows, and a project created this morning has run nothing; returning `None` would
/// make the caller handle "no usage yet" as a state, and the first thing it would do is pick a
/// default — which is the zero row this function returns.
pub async fn usage_today_in(
    connection: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<UsageDay> {
    let day: Option<UsageDay> = sqlx::query_as(
        "select usage_date, runs, failures, compute_ms from automation_project_usage \
         where project_id = $1 and usage_date = current_date",
    )
    .bind(project_id)
    .fetch_optional(&mut *connection)
    .await?;
    Ok(day.unwrap_or(UsageDay {
        usage_date: time::Date::from_calendar_date(1970, time::Month::January, 1)
            .map_err(|e| WorkflowError::invalid("usage_date_invalid", e.to_string()))?,
        runs: 0,
        failures: 0,
        compute_ms: 0,
    }))
}

/// [`usage_today_in`] for a caller with only a pool — the limits screen.
pub async fn usage_today(pool: &PgPool, project_id: Uuid) -> Result<UsageDay> {
    let mut connection = pool.acquire().await?;
    usage_today_in(&mut connection, project_id).await
}

/// Count one run, atomically.
///
/// The increment is inside the upsert, so two engines counting the same run cannot read-modify-write
/// over each other. This is the "counters update atomically" the REQ asks for, and it is why the
/// limit check that reads the counter may overshoot by in-flight work and no more.
pub async fn record_usage(
    pool: &PgPool,
    project_id: Uuid,
    failed: bool,
    compute_ms: i64,
) -> Result<UsageDay> {
    let day = sqlx::query_as::<_, UsageDay>(
        "insert into automation_project_usage (project_id, usage_date, runs, failures, compute_ms) \
         values ($1, current_date, 1, $2, $3) \
         on conflict (project_id, usage_date) do update set \
         runs = automation_project_usage.runs + 1, \
         failures = automation_project_usage.failures + excluded.failures, \
         compute_ms = automation_project_usage.compute_ms + excluded.compute_ms, \
         updated_at = now() \
         returning usage_date, runs, failures, compute_ms",
    )
    .bind(project_id)
    .bind(if failed { 1 } else { 0 })
    .bind(compute_ms.max(0))
    .fetch_one(pool)
    .await?;
    Ok(day)
}

/// How many runs are in flight right now.
///
/// Counted from the executions table rather than from a counter, because "in flight" is a question
/// about rows that exist and a counter would drift the moment a process died holding one.
pub async fn concurrent_runs_in(
    connection: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from workflow_executions e \
         join workflows w on w.id = e.workflow_id \
         where w.project_id = $1 and e.status = 'running'",
    )
    .bind(project_id)
    .fetch_one(&mut *connection)
    .await?;
    Ok(count)
}

/// [`concurrent_runs_in`] for the limits screen.
pub async fn concurrent_runs(pool: &PgPool, project_id: Uuid) -> Result<i64> {
    let mut connection = pool.acquire().await?;
    concurrent_runs_in(&mut connection, project_id).await
}

/// The daily series for the limits screen and its CSV, oldest first.
///
/// `days` is bounded in Rust: an unbounded series is what turns a limits page into a table nobody
/// scrolls. 90 is the longest window the REQ's CSV mentions.
pub async fn usage_series(pool: &PgPool, project_id: Uuid, days: i32) -> Result<Vec<UsageDay>> {
    let days = days.clamp(1, 90);
    let rows = sqlx::query_as::<_, UsageDay>(
        "select usage_date, runs, failures, compute_ms from automation_project_usage \
         where project_id = $1 and usage_date > current_date - make_interval(days => $2) \
         order by usage_date",
    )
    .bind(project_id)
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// The limit half of the run guard
// ---------------------------------------------------------------------------------------------

/// Whether this project may start one more run today and right now.
///
/// The archive half of the same question lives in [`crate::projects::ensure_run_allowed`], and this
/// one is called from the same place — `store::create_execution_in` — so an archived project and an
/// over-quota project are refused by one code path rather than by two that can disagree about what
/// "allowed" means.
///
/// Returns the project key and the owner's display name with the refusal, because the REQ asks for a
/// message naming both and a guard that cannot name them would have to query them at the call site.
pub async fn ensure_run_within_limits(
    connection: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<()> {
    let limits = read_limits_in(connection, project_id).await?;

    if !Limits::is_unlimited(limits.max_runs_per_day) {
        let today = usage_today_in(connection, project_id).await?;
        if let Some((current, limit)) =
            Limits::exceeded(limits.max_runs_per_day, i64::from(today.runs))
        {
            let (key, owner) = owner_display(connection, project_id).await?;
            return Err(WorkflowError::invalid(
                "project_runs_per_day_exceeded",
                Limits::refusal_message(
                    "daily run",
                    current,
                    limit,
                    &key,
                    &owner,
                ),
            ));
        }
    }

    if !Limits::is_unlimited(limits.max_concurrent_runs) {
        let running = concurrent_runs_in(connection, project_id).await?;
        if let Some((current, limit)) = Limits::exceeded(limits.max_concurrent_runs, running) {
            let (key, owner) = owner_display(connection, project_id).await?;
            return Err(WorkflowError::invalid(
                "project_concurrent_runs_exceeded",
                Limits::refusal_message(
                    "concurrent run",
                    current,
                    limit,
                    &key,
                    &owner,
                ),
            ));
        }
    }

    Ok(())
}

/// The project's key and its owner's display name, for a refusal message.
///
/// Falls back to a phrase rather than to an id: "ask 6f2c…" is not an instruction anybody can
/// follow, and the REQ's message requirement is about naming a *person*.
async fn owner_display(
    connection: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<(String, String)> {
    // `coalesce` already made the second column non-null, so the tuple is (String, String) and the
    // only question left is whether the name is empty — an owner who was deleted leaves one.
    let row: Option<(String, String)> = sqlx::query_as(
        "select p.key, coalesce(u.display_name, '') from automation_projects p \
         left join users u on u.id = p.owner_user_id where p.id = $1",
    )
    .bind(project_id)
    .fetch_optional(&mut *connection)
    .await?;
    Ok(match row {
        Some((key, name)) if !name.is_empty() => (key, name),
        Some((key, _)) => (key, "the project owner".into()),
        None => ("this project".into(), "the project owner".into()),
    })
}

// ---------------------------------------------------------------------------------------------
// Ownership transfer
// ---------------------------------------------------------------------------------------------

/// Hand a project to a new owner, in one transaction, with an audit row.
///
/// **Two writes, one transaction.** `automation_projects.owner_user_id` and the `owner` membership
/// row are two facts about the same thing; a transfer that wrote one and not the other leaves a
/// project whose owner column names a person who cannot administer it (or can, with no record of
/// how). The transfer also **demotes the previous owner's membership to `editor`** rather than
/// removing it: the person who owned the project yesterday usually still works in it, and dropping
/// their row would silently remove their access as a side effect of handing the project over.
///
/// The audit row is `project_ownership.transferred` — its own action name, because REQ-133 asks for
/// two confirmations precisely because this is a different act from a rename, and an audit stream
/// that cannot tell them apart cannot answer "who owned this project in March".
///
/// **Returns the previous owner, or `None` when there was none.** `owner_user_id` is nullable
/// (`on delete set null`), so "a project nobody owns" is a real state a real installation reaches
/// by deleting an account — and a return type of `Uuid` would have to invent an owner for it or
/// fail on it. The caller's two confirmations are about the *new* owner, which is why `None` here
/// is a fact to render rather than an error.
pub async fn transfer_ownership(
    pool: &PgPool,
    project_id: Uuid,
    to_user_id: Uuid,
    actor_user_id: Option<Uuid>,
    organization_id: Uuid,
) -> Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;

    // Two columns in one statement rather than two queries: the row must exist AND its owner read
    // under the same lock, and a first query that only checked presence would let the row be
    // deleted between the two. `owner_user_id` is `on delete set null`, so a deleted owner leaves a
    // project with NO owner — which is a real state (`owner_user_id` is nullable) and the reason the
    // absence branch below transfers rather than answering "not found".
    //
    // Decoding matters here and bit the first run: `owner_user_id` is NULL for an ownerless project,
    // so it is fetched as `(bool, Option<Uuid>)` — `exists, owner`. Reading a nullable column into
    // `Option<Uuid>` and wrapping it in `fetch_optional` asks for `Option<Option<Uuid>>`, and sqlx
    // answers `ColumnDecode: UnexpectedNullError` rather than the `None` the code meant.
    let (exists, previous): (bool, Option<Uuid>) = sqlx::query_as(
        "select true, owner_user_id from automation_projects where id = $1 for update",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| WorkflowError::invalid("project_not_found", "no such project"))?;
    let _ = exists;

    if previous == Some(to_user_id) {
        return Err(WorkflowError::invalid(
            "already_project_owner",
            "that account already owns this project",
        ));
    }
    // `None` means the project has no owner (a deleted one, since the column is `on delete set
    // null`). There is nobody to demote, and that is the only difference — the transfer itself is
    // the same two writes.
    let previous_for_demote = previous;

    // The new owner must be able to hold the role: a `viewer` promoted by a side effect would be an
    // administrator who cannot edit anything, which is the one state worse than no owner at all.
    let existing_role = projects::role_of(pool, project_id, to_user_id).await?;
    let new_role = match existing_role {
        Some(ProjectRole::Viewer) => ProjectRole::Editor,
        _ => ProjectRole::Owner,
    };

    // `$3` is the role, not a literal — the first version wrote `'owner'` here and computed
    // `new_role` for nothing, so promoting a `viewer` made them an owner instead of an editor:
    // an administrator who could not edit anything. The compiler caught it by flagging the unused
    // binding, which is the only reason a variable that exists precisely to be used can be dead.
    sqlx::query(
        "insert into automation_project_members (project_id, user_id, role, added_by) \
         values ($1, $2, $3, $4) \
         on conflict (project_id, user_id) do update set role = excluded.role",
    )
    .bind(project_id)
    .bind(to_user_id)
    .bind(new_role.as_str())
    .bind(actor_user_id)
    .execute(&mut *tx)
    .await?;

    // Demote rather than remove. The `and user_id <> $2` guard means the case where the previous
    // owner IS the new owner — already refused above, but the statement must be safe on its own.
    sqlx::query(
        "update automation_project_members set role = 'editor' \
         where project_id = $1 and role = 'owner' and user_id <> $2 and user_id = $3",
    )
    .bind(project_id)
    .bind(to_user_id)
    .bind(previous_for_demote)
    .execute(&mut *tx)
    .await?;

    sqlx::query("update automation_projects set owner_user_id = $2, updated_at = now() where id = $1")
        .bind(project_id)
        .bind(to_user_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "insert into audit_log (organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata, project_id) \
         values ($1, $2, 'user', 'project_ownership.transferred', 'automation_project', $3::text, $4, $5)",
    )
    .bind(organization_id)
    .bind(actor_user_id)
    .bind(project_id)
    .bind(serde_json::json!({
        "from_user_id": previous,
        "to_user_id": to_user_id,
        "previous_owner_role": "editor",
    }))
    .bind(project_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_unlimited_and_not_zero() {
        // The single decision every fresh install depends on, asserted rather than commented:
        // migration 0170 gives every project a row of zeros, so reading a zero as "no automation"
        // would lock a new installation out of its own engine.
        assert!(Limits::is_unlimited(0));
        assert!(!Limits::is_unlimited(1));
        assert_eq!(Limits::exceeded(0, 9_999), None);
        assert_eq!(Limits::exceeded(1, 0), None);
        assert_eq!(Limits::exceeded(1, 1), Some((1, 1)));
        assert_eq!(Limits::exceeded(3, 4), Some((4, 3)));
    }

    #[test]
    fn the_warning_crosses_at_the_threshold_and_not_one_unit_earlier() {
        assert!(Limits::warns(100, 80, 80), "80 of 100 warns at 80 percent");
        // The first version of this line read `assert!(Limits::warns(100, 79, 80), "79 of 100 does
        // not")` — the assertion said "warns" and the message said "does not", so the test passed
        // only while the function was wrong and then failed the moment it was fixed. **A negated
        // claim needs `assert!(!…)`; the message is not the negation.** Worth stating because the
        // failure reads as "the code broke" when in fact the code was right and the test had the
        // sign backwards.
        assert!(!Limits::warns(100, 79, 80), "79 of 100 is one unit short of the threshold");
        assert!(Limits::warns(100, 150, 80), "an over-quota project still warns");
        // A limit of one: the threshold is 0.8, which floors to 0, so it must be floored to 1 or the
        // warning can only ever fire after the cap.
        assert!(Limits::warns(1, 1, 80), "a limit of one warns on the first unit");
        assert!(!Limits::warns(0, 5_000, 80), "an unlimited project never warns");
    }

    #[test]
    fn a_refusal_names_the_limit_the_numbers_and_the_owner() {
        let message = Limits::refusal_message("daily run", 12, 10, "OPS", "Furkan ERMAĞ");
        assert!(message.contains("OPS"), "{message}");
        assert!(message.contains("12 of 10"), "{message}");
        assert!(message.contains("Furkan ERMAĞ"), "{message}");
        assert!(message.contains("daily run"), "{message}");
    }

    #[test]
    fn a_day_with_no_rows_reads_as_zero() {
        // `usage_today` returns a zero row rather than `None`; the assertion is on the shape of
        // that promise — a limits screen cannot render a bar against a missing number.
        let zero = UsageDay {
            usage_date: time::Date::from_calendar_date(1970, time::Month::January, 1).expect("a valid date"),
            runs: 0,
            failures: 0,
            compute_ms: 0,
        };
        assert_eq!(zero.runs, 0);
        assert_eq!(Limits::exceeded(10, i64::from(zero.runs)), None);
    }
}
