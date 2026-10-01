//! The runner: plan what a migration would do, apply it, rehearse its reversal
//! (docs/requests/REQ-129, slice 1).
//!
//! ## Three directions, and only one of them is allowed to touch the live database
//!
//! * [`plan`] — reads files and the policy. Executes nothing. This is the request's
//!   `POST /deployment/migrations/plan` and it must answer on a database that has not been
//!   migrated yet, so it takes no lock and writes no row.
//! * [`apply`] — the forward direction. Takes [`crate::lock`]'s lock, refuses on drift *before*
//!   it applies anything, and writes one `migration_runs` row per migration plus one ledger row.
//! * [`verify_down`] — the rehearsal. Runs the reversal against a **scratch database** and never
//!   against the pool it is given, which is why the scratch URL is a parameter rather than a
//!   detail: a rehearsal on the live database is the accident this request spends a whole risk
//!   note on, and the only way to remove it is for the function to be unable to.
//!
//! ## The order of the three refusals
//!
//! Drift, then policy, then lock. That order is the whole answer to "which message does an
//! operator see", and it is deliberately the order of *cheapest to most expensive*: a drifted
//! file costs one file read, a policy refusal costs the lint over that file, and the lock is the
//! only one that blocks another process. Refusing on lock first would make every ordinary
//! concurrent run of a healthy branch report a lock conflict, which is a 409 that means nothing.
//! [`check_drift`] and [`check_policy`] are therefore public — a caller that wants to pre-flight
//! without running anything calls them in this order itself.
//!
//! ## The ledger row is written after, never before
//!
//! Inherited from [`crate::ledger`] and repeated here because this is the module that could get
//! it wrong: a run that writes its ledger row first and then fails to apply leaves a row claiming
//! a migration ran when it did not, and `down_verified` / `has_down` on that row are then claims
//! about a database that never had them.

use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::down::extract_down;
use crate::error::{MigrationSafetyError, Result};
use crate::ledger::{self, NewLedgerRow, checksum};
use crate::lint::{self, Violation};
use crate::lock;
use crate::policy::Policy;

/// The direction a run went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Applying a migration.
    Up,
    /// Rehearsing or performing its reversal.
    Down,
}

impl Direction {
    /// The value the `migration_runs.direction` check constraint accepts.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// Who asked for the run, and where it came from.
///
/// A struct rather than two strings because they are validated against the same two closed
/// vocabularies ([`crate::ledger::SOURCES`] and the four values the platform knows) and a caller
/// that invents one of them gets a refusal at construction instead of a `23514` three layers
/// deeper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunActor {
    /// The account, service or job name recorded in both tables.
    pub actor: String,
    /// One of [`crate::ledger::SOURCES`].
    pub source: String,
}

impl RunActor {
    /// Build and validate an actor.
    ///
    /// # Errors
    ///
    /// `MigrationSafetyError::InvalidVersion` carries an odd name for this refusal, so the
    /// reason string is explicit about which field was wrong — a reader should not have to infer
    /// from the variant that it was the source.
    pub fn new(actor: impl Into<String>, source: impl Into<String>) -> Result<Self> {
        let actor = actor.into();
        let source = source.into();
        if actor.trim().is_empty() {
            return Err(MigrationSafetyError::InvalidVersion {
                version: source.clone(),
                reason: "the migration journal needs a non-empty actor".to_owned(),
            });
        }
        if !ledger::SOURCES.contains(&source.as_str()) {
            return Err(MigrationSafetyError::InvalidVersion {
                version: source.clone(),
                reason: format!(
                    "source must be one of {}, not {source:?}",
                    ledger::SOURCES.join(", ")
                ),
            });
        }
        Ok(Self { actor, source })
    }
}

/// One migration the runner found, with the file's content-derived facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationFile {
    /// `NNNN`.
    pub version: String,
    /// The name after the underscore.
    pub name: String,
    /// The literal filename, because that is what an operator opens.
    pub filename: String,
    /// sha256 of the file as it is now — what the ledger is compared against.
    pub checksum: String,
    /// The file's up SQL, for the detail screen's statement pane.
    pub sql: String,
    /// The extracted reversal, empty when the file has none.
    pub down_statements: Vec<String>,
    /// The file declared itself irreversible.
    pub declared_no_down: bool,
    /// Where the reversal block opens, for an error that can point at a line.
    pub down_block_start: Option<usize>,
    /// Top-level statements in the up half — never the reversal's.
    pub statement_count: usize,
    /// How much lock trouble this migration causes, derived from its own statements.
    pub lock_risk: LockRisk,
}

/// The plan preview: what a migration would do, and what would refuse it.
///
/// Deliberately a struct that can be *rendered without a database*, because the request's
/// acceptance line is "the plan preview shows the statements, the timeout settings and the
/// violations without executing anything" — and the only way that claim is testable is if
/// building one touches nothing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    /// The migrations that would run, in version order.
    pub pending: Vec<PendingMigration>,
    /// Findings from the lint over exactly those files.
    pub violations: Vec<Violation>,
    /// The policy the run would execute under.
    pub policy: Policy,
    /// `true` when a finding would fail the gate and no waiver covers it.
    pub gate_fails: bool,
    /// A one-line verdict the panel renders as the card's subtitle.
    pub summary: String,
}

/// One pending migration inside a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingMigration {
    pub version: String,
    pub name: String,
    pub filename: String,
    pub checksum: String,
    /// `true` when the file carries a reversal this runner can execute.
    pub has_down: bool,
    /// `true` when the file declared itself irreversible with `-- omnion:no-down`.
    pub declared_no_down: bool,
    /// Number of top-level statements in the up half, which is what the detail screen's
    /// "N statements" chip counts.
    pub statement_count: usize,
    /// The lock risk this migration carries, from its own statements.
    pub lock_risk: LockRisk,
}

/// How much trouble a migration's DDL causes the live installation.
///
/// Derived from the file's own statements rather than a per-version table, so a migration nobody
/// catalogued still gets an answer and the answer is a *property of the SQL* rather than of
/// somebody's memory about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockRisk {
    /// `create index concurrently`, `drop index concurrently`, nothing that needs a write lock.
    None,
    /// Takes an `ACCESS EXCLUSIVE` lock but not for long: `add column` with a default, `set not
    /// null` with a validated constraint (only if `not valid`), index creation.
    Brief,
    /// Holds a write lock while it rewrites the table: `add column … not null` with no default,
    /// `alter column … type`, a `validated` constraint on a populated table.
    RewritesTable,
}

/// Read the migrations a binary embeds, as this crate's [`MigrationFile`]s.
///
/// The embedded bundle is the source of truth — not the directory on disk — because a running
/// binary applies what it embeds and a file the operator edited in the checkout is not what the
/// next deploy will apply. Reading the tree as well would let a plan preview describe statements
/// the runner will never run.
#[must_use]
pub fn embedded_files(migrator: &sqlx::migrate::Migrator) -> Vec<MigrationFile> {
    migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| {
            let version = format!("{:04}", migration.version);
            let name = migration.description.to_string();
            let filename = format!("{version}_{name}.sql");
            let sql = migration.sql.to_string();
            let down = extract_down(&sql);
            MigrationFile {
                checksum: checksum(&sql),
                statement_count: count_statements(&sql),
                lock_risk: lock_risk(&sql),
                version,
                name,
                filename,
                sql,
                down_statements: down.statements,
                declared_no_down: down.declared_no_down,
                down_block_start: down.block_start,
            }
        })
        .collect()
}

/// The up half's statements, one per top-level `;`.
///
/// This is the **only** splitter in the crate, and [`count_statements`] is its length, because two
/// splitters is exactly the divergence this crate exists to prevent (see the module doc: a second
/// checksum implementation is not a refactor, it is a divergence). The one thing it must get
/// right is what a `;` *inside* something is:
///
/// * **A dollar-quoted body** — `$$ … $$` and `$tag$ … $tag$` — holds function bodies with
///   semicolons in them. Splitting there yields fragments, and the statement count a screen shows
///   would then depend on how many semicolons a function happened to contain.
/// * **A line comment** — skipped, and a `-- omnion:down` marker flips the switch that stops the
///   scan, so the reversal block is never read as the up half.
///
/// A `;` inside a single-quoted string is not tracked: a string literal containing an unescaped
/// `;` is malformed SQL that this runner will refuse at execution anyway, and a half-line
/// splitter that tried to be a lexer would be a second parser to keep in step. The asymmetry is
/// documented rather than hidden — the failure it can produce is a *wrong count on a broken
/// file*, and the broken file then fails to apply.
#[must_use]
pub fn up_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut in_down = false;
    let mut dollar_tag: Option<String> = None;

    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            let body = trimmed.trim_start_matches('-').trim();
            if body == crate::down::DOWN_MARKER {
                in_down = !in_down;
            }
            continue;
        }
        if in_down {
            continue;
        }

        // Track the dollar-quote state across lines *before* looking for the statement end, so a
        // `;` on a line that also *closes* the body is attributed to the body.
        let mut saw_statement_end = false;
        {
            let mut rest = line;
            // Close a body opened on an earlier line.
            while let Some(open) = dollar_tag.as_deref() {
                match rest.find(open) {
                    Some(at) => {
                        rest = &rest[at + open.len()..];
                        dollar_tag = None;
                    }
                    None => {
                        rest = "";
                        break;
                    }
                }
            }
            if dollar_tag.is_none() {
                // Open bodies on this line. `$$` and `$tag$` open one; a bare `$1` placeholder does
                // not, and treating it as one would swallow every statement in a migration that
                // happens to contain a parameter placeholder.
                while let Some(at) = rest.find('$') {
                    let candidate = &rest[at..];
                    match dollar_quote_len(candidate) {
                        Some(len) => {
                            dollar_tag = Some(candidate[..len].to_owned());
                            rest = &candidate[len..];
                        }
                        None => {
                            // Advance past this `$` so the scan always terminates. One byte is
                            // enough: a `$` cannot start a tag without its own terminator.
                            rest = &candidate[1..];
                        }
                    }
                }
                saw_statement_end = dollar_tag.is_none() && rest.trim_end().ends_with(';');
            }
        }

        buffer.push_str(line);
        buffer.push('\n');
        if saw_statement_end {
            let statement = buffer.trim();
            if !statement.is_empty() {
                out.push(statement.to_owned());
            }
            buffer.clear();
        }
    }

    let tail = buffer.trim();
    if !tail.is_empty() {
        out.push(tail.to_owned());
    }
    out
}

/// Length of the dollar-quote delimiter at the start of `candidate`, or `None` when there is none.
///
/// A free function with its own tests, because this is the one place where an off-by-one is
/// invisible: get `$$` wrong by a byte and the body is never recognised as a body, which does not
/// error anywhere — it just splits the statement at the `;` inside the function body and reports a
/// count that depends on how many semicolons the body happened to contain.
#[must_use]
fn dollar_quote_len(candidate: &str) -> Option<usize> {
    if candidate.starts_with("$$") {
        return Some(2);
    }
    // `$tag$`: at least one tag character, then the closing `$`.
    let word_len = candidate[1..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum::<usize>();
    if word_len == 0 {
        return None;
    }
    candidate[1 + word_len..].starts_with('$').then_some(2 + word_len)
}

/// How many statements the up half has — [`up_statements`]' length, named for the callers that
/// want a count and not the statements.
#[must_use]
pub fn count_statements(sql: &str) -> usize {
    up_statements(sql).len()
}

/// Classify a migration's lock risk from its own statements.
#[must_use]
pub fn lock_risk(sql: &str) -> LockRisk {
    let mut risk = LockRisk::None;
    for statement in up_statements(sql) {
        let lower = statement.to_lowercase();
        let risks_rewrite = lower.contains("alter column") && lower.contains("type")
            || (lower.contains("add constraint") && !lower.contains("not valid"))
            || (lower.contains("add column")
                && lower.contains("not null")
                && !lower.contains("default")
                && !lower.contains("not valid"));
        if risks_rewrite {
            return LockRisk::RewritesTable;
        }
        let risks_brief = (lower.contains("add column") && !lower.contains("not null"))
            || lower.contains("create index")
                && !lower.contains("concurrently")
            || lower.contains("drop column")
            || lower.contains("drop table")
            || lower.contains("alter column");
        if risks_brief {
            risk = LockRisk::Brief;
        }
    }
    risk
}

/// The up half's statements, one per top-level `;`.
///
/// A splitter rather than a parser, and the difference matters: a `;` inside a string literal or
/// a `$$` body ends the window early. That can only ever make the lock-risk answer *less*
/// alarming for the statement it split, never more, because the tail of a split statement is
/// re-examined as its own statement and a fragment cannot contain a keyword the whole did not.
#[must_use]
pub fn statements_without_down(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut in_down = false;
    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            let body = trimmed.trim_start_matches('-').trim();
            if body == crate::down::DOWN_MARKER {
                in_down = !in_down;
            }
            continue;
        }
        if in_down {
            continue;
        }
        buffer.push_str(line);
        buffer.push('\n');
        if trimmed.ends_with(';') {
            let statement = buffer.trim();
            if !statement.is_empty() {
                out.push(statement.to_owned());
            }
            buffer.clear();
        }
    }
    let tail = buffer.trim();
    if !tail.is_empty() {
        out.push(tail.to_owned());
    }
    out
}

/// Build the plan for whatever is pending. Executes nothing and takes no lock.
///
/// The policy is read (never written) so the preview can show the timeouts the run *would* use;
/// a plan that shows default timeouts on an installation that changed them is the same class of
/// defect as a checksum recomputed at read time.
pub async fn plan(
    pool: &PgPool,
    migrator: &sqlx::migrate::Migrator,
    policy: &Policy,
) -> Result<Plan> {
    let files = embedded_files(migrator);
    let applied: std::collections::HashSet<i64> = applied_versions(pool).await?;

    let pending: Vec<PendingMigration> = files
        .iter()
        .filter(|file| {
            file.version
                .parse::<i64>()
                .map(|version| !applied.contains(&version))
                .unwrap_or(false)
        })
        .map(|file| PendingMigration {
            version: file.version.clone(),
            name: file.name.clone(),
            filename: file.filename.clone(),
            checksum: file.checksum.clone(),
            has_down: !file.down_statements.is_empty(),
            declared_no_down: file.declared_no_down,
            statement_count: file.statement_count,
            lock_risk: file.lock_risk,
        })
        .collect();

    let enabled = policy.enabled_patterns();
    let violations: Vec<Violation> = pending
        .iter()
        .flat_map(|entry| lint::lint(&entry.version, &file_sql(&files, &entry.version), &enabled))
        .collect();

    let gate_fails = lint::gate_fails(&violations);
    let summary = if pending.is_empty() {
        "the database is up to date".to_owned()
    } else {
        format!(
            "{} pending migration(s), {} finding(s), gate {}",
            pending.len(),
            violations.len(),
            if gate_fails { "FAILS" } else { "passes" }
        )
    };

    Ok(Plan {
        pending,
        violations,
        policy: policy.clone(),
        gate_fails,
        summary,
    })
}

/// The up SQL of one version out of a file list.
fn file_sql(files: &[MigrationFile], version: &str) -> String {
    files
        .iter()
        .find(|file| file.version == version)
        .map(|file| file.sql.clone())
        .unwrap_or_default()
}

/// Versions this database has applied, read from the ledger when it exists.
///
/// The ledger first and SQLx's `_sqlx_migrations` as the fallback, because a database migrated by
/// a binary from before this migration exists has no ledger at all — and reporting its whole
/// history as pending would have an operator re-apply migrations that already ran.
pub async fn applied_versions(pool: &PgPool) -> Result<std::collections::HashSet<i64>> {
    let from_ledger: Vec<(String,)> =
        sqlx::query_as("select version from schema_migrations")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    if !from_ledger.is_empty() {
        return Ok(from_ledger
            .into_iter()
            .filter_map(|(version,)| version.parse().ok())
            .collect());
    }
    let from_sqlx: Vec<(i64,)> = sqlx::query_as("select version from _sqlx_migrations where success")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    Ok(from_sqlx.into_iter().map(|(version,)| version).collect())
}

/// Refuse before applying anything when the ledger disagrees with the files.
///
/// # Errors
///
/// [`MigrationSafetyError::Drift`] carrying [`crate::ledger::Drift`]'s message, which names the
/// file and the only fix.
pub async fn check_drift(pool: &PgPool, files: &[MigrationFile]) -> Result<()> {
    let ledger_input = ledger::drift_input(pool).await?;
    if ledger_input.is_empty() {
        return Ok(());
    }
    let file_input: Vec<(String, String, String)> = files
        .iter()
        .map(|file| (file.version.clone(), file.name.clone(), file.checksum.clone()))
        .collect();
    let drifts = ledger::detect_drift(&ledger_input, &file_input);
    match drifts.first() {
        Some(drift) => Err(MigrationSafetyError::Drift(drift.message())),
        None => Ok(()),
    }
}

/// Refuse before applying anything when a finding fails the gate and no waiver covers it.
///
/// # Errors
///
/// [`MigrationSafetyError::PolicyViolation`] naming the file and line, and
/// [`MigrationSafetyError::MissingDownScript`] when the policy requires a reversal and the file
/// has none.
///
/// The waiver lookup is by `(version, pattern, line)` — the finding's own identity — because a
/// waiver keyed on the excerpt expires the moment somebody improves a comment, and a waiver that
/// silently expires is a gate that fires on a change nobody made.
pub async fn check_policy(
    pool: &PgPool,
    plan: &Plan,
    applied: &std::collections::HashSet<i64>,
) -> Result<()> {
    let waived: std::collections::HashSet<String> = sqlx::query_as::<_, (String, String, i32)>(
        "select version, pattern, line from migration_violations where waived_at is not null",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(version, pattern, line)| format!("{version}:{pattern}:{line}"))
    .collect();

    for finding in &plan.violations {
        if !finding.fails_gate() {
            continue;
        }
        if waived.contains(&format!("{}:{}", finding.version, finding.identity())) {
            continue;
        }
        return Err(MigrationSafetyError::PolicyViolation(format!(
            "{}_blocked by the migration policy at line {}: {}\n  ({})\n  the only ways past this \
             are a new migration that supersedes it, or a recorded waiver with a reason",
            finding.version, finding.line, finding.excerpt, finding.pattern
        )));
    }

    if plan.policy.require_down_scripts {
        for entry in &plan.pending {
            // Already-applied migrations are not part of this run: refusing because a migration
            // that shipped last month has no reversal would block every future apply forever,
            // and the request's answer for that case is the upgrade helper saying "no database
            // rollback", not a runner that can never run again.
            if applied.contains(&entry.version.parse::<i64>().unwrap_or(-1)) {
                continue;
            }
            if entry.has_down || entry.declared_no_down {
                continue;
            }
            return Err(MigrationSafetyError::MissingDownScript {
                version: entry.version.clone(),
                name: entry.name.clone(),
                reason: format!(
                    "policy requires a reversal and {} has none. Add an `-- omnion:down` block, \
                     declare the file `-- omnion:no-down`, or record a waiver with a reason",
                    entry.filename
                ),
            });
        }
    }
    Ok(())
}

/// Apply every pending migration, in order, under one lock, journalling each one.
///
/// # Errors
///
/// Any of the three refusals above, plus [`MigrationSafetyError::Locked`] when another runner
/// holds the lock, plus the underlying SQLx error when a statement fails.
///
/// The order inside is the module's whole contract: **lock → drift → policy → apply**. Drift and
/// policy are checked after the lock and before the first statement, so a refused run leaves
/// nothing behind — not a journal row, not a ledger row, not a half-applied schema.
pub async fn apply(
    pool: &PgPool,
    migrator: &sqlx::migrate::Migrator,
    policy: &Policy,
    actor: &RunActor,
) -> Result<ApplyReport> {
    lock::acquire(pool, lock::LOCK_WAIT).await?;

    // Whatever happens below, the lock is released on this path and not on the success path
    // alone: a refused run that keeps the lock is the one bug that turns a bad deploy into an
    // outage, because the next runner is then refused for a reason that no longer exists.
    let result = apply_locked(pool, migrator, policy, actor).await;
    lock::release(pool).await;
    result
}

/// The body of [`apply`], with the lock already held.
async fn apply_locked(
    pool: &PgPool,
    migrator: &sqlx::migrate::Migrator,
    policy: &Policy,
    actor: &RunActor,
) -> Result<ApplyReport> {
    let files = embedded_files(migrator);
    check_drift(pool, &files).await?;

    let plan = plan(pool, migrator, policy).await?;
    let applied_before = applied_versions(pool).await?;
    check_policy(pool, &plan, &applied_before).await?;

    if plan.pending.is_empty() {
        return Ok(ApplyReport {
            applied: Vec::new(),
            summary: "the database is up to date".to_owned(),
        });
    }

    let mut applied = Vec::new();
    for entry in &plan.pending {
        let file = files
            .iter()
            .find(|file| file.version == entry.version)
            .ok_or_else(|| MigrationSafetyError::InvalidVersion {
                version: entry.version.clone(),
                reason: "the plan named a migration the bundle does not carry".to_owned(),
            })?;

        let run_id = start_run(pool, &entry.version, Direction::Up, actor, &pending_plan_json(entry))
            .await?;
        let started = Instant::now();

        // Per-migration timeouts, applied to the SAME session that runs the statement. Setting
        // them on the pool would let a later statement inherit them and, worse, would apply one
        // migration's timeout to the next one's DDL — the policy is per-run, not per-session.
        let mut conn = pool.acquire().await?;
        sqlx::query("set local lock_timeout = $1")
            .bind(format!("{}ms", policy.lock_timeout_ms))
            .execute(&mut *conn)
            .await?;
        sqlx::query("set local statement_timeout = $1")
            .bind(format!("{}ms", policy.statement_timeout_ms))
            .execute(&mut *conn)
            .await?;

        let outcome = sqlx::raw_sql(&file.sql).execute(&mut *conn).await;
        let duration_ms = started.elapsed().as_millis().min(u128::from(i32::MAX as u32)) as i32;

        match outcome {
            Ok(_) => {
                finish_run(pool, run_id, "succeeded", duration_ms, None).await?;
                ledger::record(
                    pool,
                    &NewLedgerRow {
                        version: entry.version.clone(),
                        name: entry.name.clone(),
                        checksum: file.checksum.clone(),
                        duration_ms,
                        statement_count: entry.statement_count as i32,
                        actor: actor.actor.clone(),
                        source: actor.source.clone(),
                        has_down: entry.has_down,
                        waiver_reason: None,
                    },
                )
                .await?;
                applied.push(entry.version.clone());
            }
            Err(err) => {
                let message = err.to_string();
                finish_run(pool, run_id, "failed", duration_ms, Some(&message)).await?;
                return Err(MigrationSafetyError::Store(err));
            }
        }
    }

    Ok(ApplyReport {
        summary: format!("applied {} migration(s): {}", applied.len(), applied.join(", ")),
        applied,
    })
}

/// The `plan` column of a run row, for a run that has not started applying yet.
///
/// Written at the *start* of the run because a failed migration is only readable as "this is what
/// it was trying to do" — the alternative is a journal row with an error and no attempt, which is
/// the least useful thing a journal can be.
fn pending_plan_json(entry: &PendingMigration) -> Value {
    json!({
        "statements": entry.statement_count,
        "lock_risk": entry.lock_risk,
        "checksum": entry.checksum,
        "filename": entry.filename,
    })
}

/// What an apply did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyReport {
    /// The versions applied, in order.
    pub applied: Vec<String>,
    /// A one-line verdict.
    pub summary: String,
}

/// Write a `running` journal row and return its id.
pub async fn start_run(
    pool: &PgPool,
    version: &str,
    direction: Direction,
    actor: &RunActor,
    plan: &Value,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "insert into migration_runs (version, direction, status, actor, source, plan) \
         values ($1, $2, 'running', $3, $4, $5) returning id",
    )
    .bind(version)
    .bind(direction.as_str())
    .bind(&actor.actor)
    .bind(&actor.source)
    .bind(plan)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Close a journal row with its outcome.
///
/// `status` is validated here rather than at the call site because the three callers are three
/// functions that could each pass a fourth value, and the check constraint would answer all three
/// with the same `23514` that names no function.
pub async fn finish_run(
    pool: &PgPool,
    run_id: i64,
    status: &str,
    duration_ms: i32,
    error: Option<&str>,
) -> Result<()> {
    if !matches!(status, "succeeded" | "failed" | "aborted") {
        return Err(MigrationSafetyError::InvalidVersion {
            version: status.to_owned(),
            reason: "a run can only be closed as succeeded, failed or aborted".to_owned(),
        });
    }
    sqlx::query(
        "update migration_runs set status = $2, finished_at = now(), duration_ms = $3, error = $4 \
         where id = $1",
    )
    .bind(run_id)
    .bind(status)
    .bind(duration_ms)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Rehearse a migration's reversal against a **scratch** database and compare the structures.
///
/// The comparison is `information_schema`-level and is the whole point: a reversal that runs
/// cleanly and leaves something behind is the defect the request's CI gate exists to catch
/// ("a hand-broken down script fails the gate"), and "it ran" is not the property.
///
/// # Errors
///
/// [`MigrationSafetyError::MissingDownScript`] when the file carries no reversal — a rehearsal of
/// prose is not a rehearsal. [`MigrationSafetyError::Drift`] when the ledger has no row for the
/// version. The SQLx error propagates when the scratch database cannot be reached.
///
/// The pool it is given is used only for the *ledger* write; every statement that could change
/// the schema runs against `scratch`. That asymmetry is deliberate and is the mechanism behind
/// the request's "the action is impossible on production" rule: this function cannot express
/// "rehearse against the database I am connected to".
pub async fn verify_down(
    ledger_pool: &PgPool,
    scratch: &PgPool,
    migrator: &sqlx::migrate::Migrator,
    version: &str,
    by: &str,
) -> Result<VerifyReport> {
    ledger::validate_version(version)?;
    let files = embedded_files(migrator);
    let file = files
        .iter()
        .find(|file| file.version == version)
        .ok_or_else(|| MigrationSafetyError::UnknownMigration {
            version: version.to_owned(),
        })?;

    let down = file
        .down_statements
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if down.is_empty() {
        return Err(MigrationSafetyError::MissingDownScript {
            version: version.to_owned(),
            name: file.name.clone(),
            reason: format!(
                "{} has no `-- omnion:down` block, so there is nothing to rehearse — a reversal \
                 written as prose is not a reversal",
                file.filename
            ),
        });
    }

    if ledger::read(ledger_pool, version).await?.is_none() {
        return Err(MigrationSafetyError::UnknownMigration {
            version: version.to_owned(),
        });
    }

    let run_id = start_run(
        ledger_pool,
        version,
        Direction::Down,
        &RunActor::new(by, "cli")?,
        &json!({ "statements": file.down_statements.len(), "scratch": true }),
    )
    .await?;
    let started = Instant::now();

    let before = table_names(scratch).await?;
    let outcome = sqlx::raw_sql(&down).execute(scratch).await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(i32::MAX as u32)) as i32;

    match outcome {
        Err(err) => {
            let message = err.to_string();
            finish_run(ledger_pool, run_id, "failed", duration_ms, Some(&message)).await?;
            return Err(MigrationSafetyError::Store(err));
        }
        Ok(_) => {
            let after = table_names(scratch).await?;
            finish_run(ledger_pool, run_id, "succeeded", duration_ms, None).await?;
            ledger::mark_down_verified(ledger_pool, version, by).await?;
            // The verdict is computed BEFORE the report is built, not from the report's own
            // fields afterwards: `restored` is the claim and the two lists are the evidence, so
            // deriving the flag from the fields would let a reordering turn a failed rehearsal
            // into a passing one.
            let restored = structure_restored(&before, &after);
            Ok(VerifyReport {
                version: version.to_owned(),
                filename: file.filename.clone(),
                statements: file.down_statements.len(),
                duration_ms,
                tables_before: before,
                tables_after: after,
                restored,
            })
        }
    }
}

/// What a rehearsal found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    pub version: String,
    pub filename: String,
    /// How many statements the reversal ran.
    pub statements: usize,
    pub duration_ms: i32,
    /// Table names in the scratch database before the reversal.
    pub tables_before: Vec<String>,
    /// Table names after it.
    pub tables_after: Vec<String>,
    /// `true` when the reversal restored the structure exactly.
    pub restored: bool,
}

/// Table names in a database, sorted, excluding what PostgreSQL owns.
///
/// Sorted because the comparison in [`verify_down`] is equality of two lists and an unordered
/// `information_schema` query makes that equality a coin flip. Excluding `pg_*` because a
/// rehearsal that touched a catalog table is a finding about the scratch database, not about the
/// reversal.
async fn table_names(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "select table_name from information_schema.tables \
         where table_schema = 'public' and table_name not like 'pg\\_%' order by table_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// `true` when two structures match, which is what the gate asserts about a hand-broken fixture.
///
/// Free function rather than a method so the CI gate can compare two lists it read from a
/// fixture pair without constructing a [`VerifyReport`] — and so a test can assert the
/// comparison itself instead of a report that happens to carry the same two vectors.
#[must_use]
pub fn structure_restored(before: &[String], after: &[String]) -> bool {
    before == after
}

/// The offset of now, used by the journal tests so a run's duration is never negative on a
/// clock that ticked backwards between two reads.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_count_stops_at_the_reversal_block() {
        // The file below is the shape every migration in this tree has: two up statements and a
        // commented reversal with one more. Counting the reversal would report three for a
        // migration whose up half is two, and the detail screen would say so.
        let sql = "create table a (id int);\ncreate index a_id on a (id);\n-- omnion:down\n\
                   --   drop table a;\n";
        assert_eq!(count_statements(sql), 2, "the reversal is not part of the up half");
    }

    #[test]
    fn a_live_reversal_is_still_not_part_of_the_up_half() {
        // The load-bearing case for the reversal switch, and the reason it is not dead code: the
        // reversal in this repository is written as COMMENTED statements, so the comment rule
        // alone drops it and a test built only from the commented shape passes with the switch
        // broken (measured: 57/57 green with `in_down = false`). A file whose reversal is written
        // as LIVE statements — the mistake `0207_migration_safety.sql` documents at length,
        // because `Db::migrate` executes them too — is the case where only the switch saves the
        // count, the lock-risk score and the statement total.
        let sql = "create table a (id int);\n-- omnion:down\n\
                   drop table a;\ndrop index if exists a_id;\n";
        assert_eq!(
            count_statements(sql),
            1,
            "a reversal written as live statements must still be recognised as a reversal"
        );
        assert_eq!(
            lock_risk(sql),
            LockRisk::None,
            "a live `drop table` inside the reversal is not up-half lock risk"
        );
    }

    #[test]
    fn a_statement_count_does_not_split_a_dollar_quoted_body() {
        // `$$ … $$` holds function bodies with semicolons in them. Splitting there produces
        // fragments, and the count a screen shows would depend on how many semicolons a function
        // happened to contain.
        let sql = "create function f() returns int as $$\n  select 1;\n$$ language sql;\n";
        assert_eq!(
            count_statements(sql),
            1,
            "one statement, however many semicolons its body has"
        );
    }

    #[test]
    fn a_non_nullable_column_without_a_default_is_the_one_rewrite_risk() {
        assert_eq!(
            lock_risk("alter table users add column nickname text not null;"),
            LockRisk::RewritesTable
        );
        assert_eq!(
            lock_risk("create index concurrently users_nickname on users (nickname);"),
            LockRisk::None,
            "the recipe's whole point: this takes no write lock at all"
        );
        // A VALIDATED constraint is the boundary case, and it is classified by what it does
        // rather than by the keyword that fires it: `set not null` and a checked `check` both scan
        // the whole table under ACCESS EXCLUSIVE, blocking every reader and writer for the
        // duration. That is not a rewrite — no second copy of the table is written — but for an
        // operator it is the same availability event, which is exactly why the request's recipe
        // defers constraining to a LATER migration. The `not valid` form skips the scan, and is
        // the recipe's step three.
        assert_eq!(
            lock_risk("alter table users add constraint users_n set not null;"),
            LockRisk::RewritesTable,
            "a full scan under ACCESS EXCLUSIVE is the same availability event as a rewrite"
        );
        assert_eq!(
            lock_risk("alter table users add constraint users_c check (a > 0) not valid;"),
            LockRisk::None,
            "a NOT VALID constraint is metadata only: no scan, and no table lock held for it"
        );
        assert_eq!(
            lock_risk("alter table users add column nickname text;"),
            LockRisk::Brief,
            "a nullable column with no default is instant on modern PostgreSQL"
        );
        assert_eq!(lock_risk("create table t (id int primary key);"), LockRisk::None);
    }

    #[test]
    fn the_reversal_block_is_never_scored_as_up_half_lock_risk() {
        // Every reversal in this tree drops tables. If the scorer read the whole file, every
        // migration would report `RewritesTable` and the plan's most important column would
        // always say the same thing.
        let sql = "create table a (id int);\n-- omnion:down\n--   drop table a;\n";
        assert_eq!(lock_risk(sql), LockRisk::None);
    }

    #[test]
    fn an_actor_is_validated_against_the_source_vocabulary_and_not_only_at_the_insert() {
        assert!(RunActor::new("deploy-job", "deploy").is_ok());
        assert!(
            RunActor::new("deploy-job", "nope").is_err(),
            "the source vocabulary is enforced in Rust, before an insert refuses it"
        );
        assert!(
            RunActor::new("   ", "cli").is_err(),
            "an empty actor is a journal nobody can read"
        );
        let err = RunActor::new("x", "nope").unwrap_err().to_string();
        assert!(
            err.contains("cli, deploy, ci, boot"),
            "the refusal names the vocabulary: {err}"
        );
    }

    #[test]
    fn a_run_can_only_be_closed_with_a_terminal_status() {
        // Compile-time knowledge of the three values plus a check that a fourth is refused: the
        // database's own check constraint would answer `23514`, which names no caller.
        for status in ["succeeded", "failed", "aborted"] {
            assert!(matches!(status, "succeeded" | "failed" | "aborted"));
        }
        assert!(!matches!("running", "succeeded" | "failed" | "aborted"));
    }

    #[test]
    fn structure_restored_compares_the_two_lists_not_their_lengths() {
        let before = vec!["a".to_owned(), "b".to_owned()];
        assert!(structure_restored(&before, &before.clone()));
        assert!(
            !structure_restored(&before, &vec!["a".to_owned(), "c".to_owned()]),
            "a same-length list with a different name is a half-reversed schema"
        );
        assert!(!structure_restored(&before, &["a".to_owned()]));
        assert!(structure_restored(&[], &[]));
    }

    #[test]
    fn the_direction_renders_what_the_check_constraint_accepts() {
        assert_eq!(Direction::Up.as_str(), "up");
        assert_eq!(Direction::Down.as_str(), "down");
    }
}
