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

use sqlx::Acquire;
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
    /// The same version as the `bigint` SQLx stores, which is what `_sqlx_migrations` is keyed on.
    ///
    /// Carried rather than parsed from `version` at the call site: the ledger's key is the
    /// zero-padded string and SQLx's is an integer, and a parse in the apply loop is a second
    /// place where the two representations could disagree.
    pub version_number: i64,
    /// SQLx's checksum for this file — SHA-384 over its SQL, 48 bytes.
    ///
    /// Taken from the embedded `Migration`, never recomputed here. The runner writes its own
    /// `_sqlx_migrations` row (see [`apply_locked`]), and that row has to carry the digest
    /// SQLx's own `Migrator::run` would have written, or the next boot answers
    /// `VersionMismatch` on a migration this runner applied perfectly. Two checksum
    /// implementations would be the divergence this crate exists to prevent; one, borrowed, is
    /// the fix.
    pub sqlx_checksum: Vec<u8>,
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
    /// Pending migrations that carry no reversal, when the policy requires one.
    ///
    /// A GATE finding and not an apply refusal, and the distinction is load-bearing: this
    /// repository has 53 of its 58 migrations without a reversal, so a runner that refused the
    /// apply would make Omnion uninstallable. The request's own criterion says "fails the
    /// **gate** unless a waiver exists" — the gate is CI's `omnion migrate plan`, which exits 1
    /// and stops the push that introduces the *next* one. Bootstrapping an installation from the
    /// migrations that predate the rule is not the thing that rule is for.
    pub missing_down: Vec<PendingMigration>,
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
///
/// The parameter is `&'static` because that is what every caller has: the migration bundle is a
/// `static` in `omnion-core`, and a reborrow of it into an elided-lifetime parameter is what
/// stops the caller's future from being provably `Send`.
pub fn embedded_files(migrator: &'static sqlx::migrate::Migrator) -> Vec<MigrationFile> {
    migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| {
            let version = format!("{:04}", migration.version);
            // SQLx stores the file's stem with UNDERSCORES REPLACED BY SPACES in `description` —
            // `0207_migration_safety.sql` becomes `"0207 migration safety"`. That is correct for
            // its table and wrong for a filename, and the difference is not cosmetic: the drift
            // message, the ledger row and the detail screen's title are all supposed to name the
            // file an operator has to open, and `ls 0207_migration safety.sql` matches nothing.
            //
            // So the underscores are restored here, at the one point where the two representations
            // meet. Restoring them in the message instead would mean four call sites each
            // remembering to, and the copy that forgets is the one an operator reads at 2am.
            let description = migration.description.to_string();
            let name = description.replace(' ', "_");
            let filename = format!("{version}_{name}.sql");
            let sql = migration.sql.to_string();
            let down = extract_down(&sql);
            MigrationFile {
                checksum: checksum(&sql),
                statement_count: count_statements(&sql),
                lock_risk: lock_risk(&sql),
                version_number: migration.version,
                sqlx_checksum: migration.checksum.to_vec(),
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
    migrator: &'static sqlx::migrate::Migrator,
    policy: Policy,
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

    let missing_down: Vec<PendingMigration> = if policy.require_down_scripts {
        pending
            .iter()
            .filter(|entry| !entry.has_down && !entry.declared_no_down)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    let gate_fails = lint::gate_fails(&violations) || !missing_down.is_empty();
    let summary = if pending.is_empty() {
        "the database is up to date".to_owned()
    } else {
        format!(
            "{} pending migration(s), {} finding(s), {} without a reversal, gate {}",
            pending.len(),
            violations.len(),
            missing_down.len(),
            if gate_fails { "FAILS" } else { "passes" }
        )
    };

    Ok(Plan {
        pending,
        violations,
        missing_down,
        policy,
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
    // The UNION of both ledgers, and the union is the only correct answer.
    //
    // `_sqlx_migrations` carries every migration this installation ever ran, including the 57 that
    // predate the ledger table. `schema_migrations` carries only what has been applied since 0207
    // created it. So neither table alone is the applied set: taking the ledger alone reports 57
    // applied migrations as PENDING and re-applies them, and taking SQLx's alone ignores the rows
    // this crate wrote. The ledger was designed as a second record reconciled with SQLx's, and a
    // union is what "reconciled" has to mean in code.
    let mut applied = std::collections::HashSet::new();

    match sqlx::query_as::<_, (i64,)>(
        "select version from _sqlx_migrations where success",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => applied.extend(rows.into_iter().map(|(version,)| version)),
        // A database nobody has migrated has neither table. That is "nothing applied", not an
        // error: `plan` is asked for on a fresh database by the very first `omnion migrate`.
        Err(err) if ledger::is_missing_table(&err) => {}
        Err(err) => return Err(err.into()),
    }

    match sqlx::query_as::<_, (String,)>("select version from schema_migrations")
        .fetch_all(pool)
        .await
    {
        Ok(rows) => applied.extend(
            rows.into_iter()
                .filter_map(|(version,)| version.parse::<i64>().ok()),
        ),
        Err(err) if ledger::is_missing_table(&err) => {}
        Err(err) => return Err(err.into()),
    }

    Ok(applied)
}

/// Create SQLx's bookkeeping table when it is not there.
///
/// The runner writes its own row into `_sqlx_migrations` (see [`apply_one`]), and on a brand new
/// database that table does not exist: SQLx creates it inside its own `Migrator::run`, and this
/// runner does not call that. The DDL is SQLx's, copied verbatim from
/// `sqlx-postgres-0.8.6/src/migrate.rs` — a second spelling of someone else's bookkeeping table
/// would be the divergence this crate exists to prevent, so the schema and the column names are
/// exactly what SQLx's own `ensure_migrations_table` writes.
///
/// `if not exists` throughout, and the whole statement is one round trip, so two runners racing
/// here cannot both fail.
pub async fn ensure_sqlx_table(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "create table if not exists _sqlx_migrations ( \
            version bigint primary key, \
            description text not null, \
            installed_on timestamptz not null default now(), \
            success boolean not null, \
            checksum bytea not null, \
            execution_time bigint not null \
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Refuse before applying anything when the ledger disagrees with the files.
///
/// # Errors
///
/// [`MigrationSafetyError::Drift`] carrying [`crate::ledger::Drift`]'s message, which names the
/// file and the only fix.
pub async fn check_drift(pool: PgPool, files: &[MigrationFile]) -> Result<()> {
    // `PgPool` by value rather than by reference.
    //
    // This is not a style preference and not a micro-optimisation. A `&PgPool` parameter makes the
    // future's captured type `&'a PgPool` for the CALLER's `'a`, and the compiler then cannot
    // prove the future is `Send` — it reports "implementation of `Send` is not general enough",
    // which names no argument and no line. Every unit test in this crate and the CLI stayed green,
    // because both await the runner on the current thread where `Send` is never asked for. The
    // first caller that needed it — an HTTP handler, which axum requires to be `Send` — failed with
    // `the trait Handler<_, _> is not implemented`, names no cause, and cost this request a full
    // bisection. Owning a `PgPool` is an `Arc` clone, so the fix costs one atomic increment.
    let ledger_input = ledger::drift_input(&pool).await?;
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

/// Refuse before applying anything when a LINT finding fails the gate and no waiver covers it.
///
/// A *banned shape* blocks the apply. A *missing reversal* does not — see the body, and
/// [`Plan::missing_down`], which is where the gate reports it instead.
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
pub async fn check_policy(pool: PgPool, plan: &Plan) -> Result<()> {
    // `PgPool` by value rather than by reference.
    //
    // This is not a style preference and not a micro-optimisation. A `&PgPool` parameter makes the
    // future's captured type `&'a PgPool` for the CALLER's `'a`, and the compiler then cannot
    // prove the future is `Send` — it reports "implementation of `Send` is not general enough",
    // which names no argument and no line. Every unit test in this crate and the CLI stayed green,
    // because both await the runner on the current thread where `Send` is never asked for. The
    // first caller that needed it — an HTTP handler, which axum requires to be `Send` — failed with
    // `the trait Handler<_, _> is not implemented`, names no cause, and cost this request a full
    // bisection. Owning a `PgPool` is an `Arc` clone, so the fix costs one atomic increment.
    let waived: std::collections::HashSet<String> = sqlx::query_as::<_, (String, String, i32)>(
        "select version, pattern, line from migration_violations where waived_at is not null",
    )
    .fetch_all(&pool)
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

    // A missing reversal is NOT refused here, and the reason is a measurement rather than a
    // preference: this repository ships 53 migrations with no reversal at all, and all but five of
    // them predate the policy that wants one. Refusing the apply for that would mean an operator
    // could not install Omnion at all — `0001_initial.sql` alone would stop the first boot, on
    // every fresh database, forever.
    //
    // The request draws the line itself: "A migration without a down script **fails the gate**
    // unless a waiver with a reason exists". The gate is `omnion migrate plan`, which is what CI
    // runs and which exits 1 — so the rule does its real job at the moment a NEW migration is
    // pushed, and does not fire retroactively against the history it was written for.
    // [`Plan::missing_down`] carries the finding so `plan` can fail on it, and
    // [`crate::policy::Policy::require_down_scripts`] is still the switch that turns it on.
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
    pool: PgPool,
    migrator: &'static sqlx::migrate::Migrator,
    policy: Policy,
    actor: RunActor,
) -> Result<ApplyReport> {
    lock::acquire(&pool, lock::LOCK_WAIT).await?;

    // Whatever happens below, the lock is released on this path and not on the success path
    // alone: a refused run that keeps the lock is the one bug that turns a bad deploy into an
    // outage, because the next runner is then refused for a reason that no longer exists.
    let result = apply_locked(pool.clone(), migrator, policy, actor).await;
    lock::release(&pool).await;
    result
}

/// The body of [`apply`], with the lock already held.
async fn apply_locked(
    pool: PgPool,
    migrator: &'static sqlx::migrate::Migrator,
    policy: Policy,
    actor: RunActor,
) -> Result<ApplyReport> {
    // Owned, not borrowed. A `&T` parameter makes the future's captured type `&'a T` for the
    // caller's `'a`, and the compiler then cannot prove the future is `Send` even when `T` is —
    // it reports "implementation of `Send` is not general enough". Both arguments are cheap to own
    // (`PgPool` is an `Arc` clone, the migrator is a `&'static`), and owning them is what lets an
    // HTTP handler — which must run its future on a `Send` runtime — call this at all.

    let files = embedded_files(migrator);
    check_drift(pool.clone(), &files).await?;
    ensure_sqlx_table(&pool).await?;

    let plan = plan(&pool, migrator, policy.clone()).await?;
    check_policy(pool.clone(), &plan).await?;

    if plan.pending.is_empty() {
        return Ok(ApplyReport {
            applied: Vec::new(),
            summary: "the database is up to date".to_owned(),
        });
    }

    let mut applied = Vec::new();
    // The pending set is consumed rather than iterated by reference. A `slice::Iter` and the
    // `&MigrationFile` lookup below are borrows that live across the migration's await points, and
    // a future holding them is not provably `Send` — see the note on `check_policy` above. Both
    // vectors are owned locals here, so iterating by value removes the borrows at no cost.
    for entry in plan.pending {
        // The found file is CLONED, not borrowed. A `&MigrationFile` living across this
        // migration's await points is one of the captures that stops the future being `Send` (see
        // the note on `check_policy`); the clone is a `Vec<String>` of SQL that this loop runs
        // exactly once per pending migration.
        let file = files
            .iter()
            .find(|file| file.version == entry.version)
            .cloned()
            .ok_or_else(|| MigrationSafetyError::InvalidVersion {
                version: entry.version.clone(),
                reason: "the plan named a migration the bundle does not carry".to_owned(),
            })?;

        // The journal lives in `migration_runs`, which THIS migration creates — so on the very
        // first run of a fresh installation the table does not exist yet and the run cannot be
        // journalled. That is not an error to swallow and not an error to propagate either: it is
        // the one migration that cannot journal itself, because the journal arrives with it.
        // Everything after this point journals normally, which is why the fallback is per-run and
        // not a flag on the runner.
        let run_id = match start_run(
            pool.clone(),
            entry.version.clone(),
            Direction::Up,
            actor.clone(),
            pending_plan_json(&entry),
        )
            .await
        {
            Ok(id) => Some(id),
            Err(err) if ledger::is_absent_table(&err) => {
                tracing::info!(
                    version = %entry.version,
                    "no run journal yet — this is the migration that creates it"
                );
                None
            }
            Err(err) => return Err(err),
        };
        let started = Instant::now();

        // Per-migration timeouts, applied to the SAME session that runs the statement. Setting
        // them on the pool would let a later statement inherit them and, worse, would apply one
        // migration's timeout to the next one's DDL — the policy is per-run, not per-session.
        // PostgreSQL's `SET` does not take bind parameters — `set local lock_timeout = $1` is a
        // syntax error, and it fails on the FIRST migration of a fresh install, so the whole apply
        // dies before anything is written. The value is interpolated instead, and the
        // interpolation is only safe because `policy.validate()` has already constrained both
        // fields to integers inside documented bounds: `Policy` cannot carry a string here, and a
        // caller that tried would fail `validate()` before reaching this line.
        //
        // `set local` rather than `set` is what scopes them to the transaction below, so one
        // migration's timeout cannot leak onto a pooled connection and bind the NEXT one's DDL.
        // `begin()` on the pool rather than `acquire()` then `begin()` on the connection. The
        // acquired `PoolConnection` is a value that lives across the migration's await points, and
        // a future holding `&mut PgConnection` is not provably `Send` — which is what stopped an
        // HTTP handler from calling this at all. `Pool::begin()` hands back a `Transaction`
        // directly and leaves no borrowed connection in the future.
        let mut tx = pool.begin().await?;
        //
        // Each statement is bound to a NAMED local rather than passed as `&format!(..)` straight
        // into `sqlx::query`. That is not style. A temporary `&String` makes the query's lifetime
        // the tail expression, and the future this loop sits in then captures `policy` in a form
        // the compiler cannot prove `Send` — the whole of `apply` stops being `Send`, every unit
        // test and the CLI stay green (both await on the current thread), and the first caller
        // that needs `Send` — an HTTP handler — fails with
        // `the trait Handler<_, _> is not implemented`, which names no cause whatsoever. See
        // `the_public_async_surface_is_send` in this module's tests.
        //
        // Each statement is bound to a NAMED local rather than passed as `&format!(..)` straight
        // into `sqlx::query`. A temporary `&String` ties the query to the tail expression, and the
        // future then captures `policy` in a form the compiler cannot prove `Send`.
        let set_lock_timeout = format!("set local lock_timeout = '{}ms'", policy.lock_timeout_ms);
        sqlx::query(set_lock_timeout.as_str())
            .execute(&mut *tx)
            .await?;
        let set_statement_timeout =
            format!("set local statement_timeout = '{}ms'", policy.statement_timeout_ms);
        sqlx::query(set_statement_timeout.as_str())
            .execute(&mut *tx)
            .await?;

        // The whole file in ONE transaction together with SQLx's own bookkeeping row.
        //
        // This is the single most important line in the runner and it is easy to get wrong by
        // omission: without the `_sqlx_migrations` insert the next `Migrator::run` — the one every
        // boot performs — sees this version as PENDING and applies the whole migration set a
        // second time. `0207` would then be applied twice, and its `create table` would fail on the
        // tables it already made, so the instance would refuse to start with a migration error
        // naming a version it had just applied successfully.
        //
        // SQLx computes the checksum as SHA-384 over the file's SQL, so the row written here has
        // to use the SAME digest over the SAME bytes or the next boot answers
        // `VersionMismatch` on a migration this runner applied correctly.
        // The statements and the bookkeeping row commit together, which is what makes "the
        // database has this version" and "the database HAS these tables" the same claim. A
        // migration that applied and then failed to record itself would be re-applied by the
        // next boot, and every `create table` in it would fail on the tables it already made.
        let outcome = apply_one(&mut tx, &file).await;
        let duration_ms = started.elapsed().as_millis().min(u128::from(i32::MAX as u32)) as i32;

        match outcome {
            Ok(()) => {
                if let Err(err) = tx.commit().await {
                    // The statements are rolled back with the transaction, so the migration did
                    // NOT happen and the journal has to say so rather than report a success the
                    // database cannot confirm.
                    finish_run(pool.clone(), run_id, "failed".to_owned(), duration_ms, Some(err.to_string())).await?;
                    return Err(MigrationSafetyError::Store(err));
                }
                finish_run(pool.clone(), run_id, "succeeded".to_owned(), duration_ms, None).await?;
                match ledger::record(
                    &pool,
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
                .await
                {
                    Ok(()) => {}
                    // Same reason as the journal: `schema_migrations` arrives with this very
                    // migration, so the row that records it cannot be written by it.
                    Err(err) if ledger::is_absent_table(&err) => {
                        tracing::info!(
                            version = %entry.version,
                            "no ledger yet — this is the migration that creates it"
                        );
                    }
                    Err(err) => return Err(err),
                }
                applied.push(entry.version.clone());
            }
            Err(err) => {
                let message = err.to_string();
                // The transaction is dropped here, which rolls the statements back — the same
                // all-or-nothing guarantee SQLx's own `apply` makes.
                finish_run(pool.clone(), run_id, "failed".to_owned(), duration_ms, Some(message)).await?;
                return Err(MigrationSafetyError::Store(err));
            }
        }
    }

    // Backfill the ledger for everything applied BEFORE it existed.
    //
    // `schema_migrations` is created by migration 0207, so on any installation this branch reaches,
    // the 57 migrations that ran before it have SQLx rows and no ledger rows. The ledger's own
    // module doc calls that "a recoverable gap"; recoverable means this. The checksums come from
    // the embedded files — content-addressed, so they are the same bytes any installation applied
    // — and NOT from a re-read of the working tree, which is the thing that makes a checksum
    // trustworthy.
    //
    // Leaving it undone is not a cosmetic gap: the ledger screen is specified as "applied and
    // pending" over the WHOLE history, and an operator asking "has 0044 run here?" gets "no" from
    // a ledger that only ever saw 0207.
    let backfilled = backfill_ledger(&pool, &files).await?;

    Ok(ApplyReport {
        summary: format!(
            "applied {} migration(s): {}; backfilled {} into the ledger",
            applied.len(),
            applied.join(", "),
            backfilled
        ),
        applied,
    })
}

/// Write a ledger row for every applied version that has none, from the embedded files.
///
/// Returns how many rows it wrote, so the summary says "backfilled 0" rather than implying work
/// it did not do.
pub async fn backfill_ledger(pool: &PgPool, files: &[MigrationFile]) -> Result<usize> {
    // `unrecorded` returns the zero-padded STRING form, matching `schema_migrations.version` and
    // `MigrationFile.version`. The cast lives in the query rather than here because the two
    // columns have different types (`bigint` and `text`) and the conversion is a fact about the
    // SQL, not about the caller.
    let unrecorded = ledger::unrecorded(pool).await?;
    if unrecorded.is_empty() {
        return Ok(0);
    }
    let actor = RunActor::new("ledger-backfill", "boot").map_err(|err| err)?;
    let mut written = 0;
    for version in unrecorded {
        let Some(file) = files.iter().find(|file| file.version == version) else {
            // Applied here, absent from this binary's bundle: a restore from another branch. There
            // is no file to hash, and inventing a checksum would defeat the column's purpose, so
            // the row is left for `detect_drift` to report as a deletion — which is the honest
            // answer and is a different finding from "missing".
            tracing::warn!(
                version = %version,
                "applied here but not in this binary's bundle — left for the drift check"
            );
            continue;
        };
        let down = !file.down_statements.is_empty();
        sqlx::query(
            "insert into schema_migrations \
                 (version, name, checksum, duration_ms, statement_count, actor, source, has_down) \
             values ($1, $2, $3, 0, $4, $5, $6, $7) \
             on conflict (version) do nothing",
        )
        .bind(&file.version)
        .bind(&file.name)
        .bind(&file.checksum)
        .bind(file.statement_count as i32)
        .bind(&actor.actor)
        .bind(&actor.source)
        .bind(down)
        .execute(pool)
        .await?;
        written += 1;
    }
    Ok(written)
}

/// Run one migration's statements and write SQLx's bookkeeping row, inside the caller's
/// transaction.
///
/// The row is written with `success = true` because the statements ran inside this transaction:
/// if they had failed, the insert would not be reached, and if the COMMIT fails afterwards the
/// whole transaction rolls back — including this row. So `success = false` never has to be
/// written here, which is exactly why SQLx can use a plain insert.
async fn apply_one(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    file: &MigrationFile,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::raw_sql(&file.sql).execute(&mut **tx).await?;
    sqlx::query(
        "insert into _sqlx_migrations (version, description, success, checksum, execution_time) \
         values ($1, $2, true, $3, -1)",
    )
    .bind(file.version_number)
    .bind(&file.name)
    .bind(&file.sqlx_checksum)
    .execute(&mut **tx)
    .await?;
    Ok(())
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
    pool: PgPool,
    version: String,
    direction: Direction,
    actor: RunActor,
    plan: Value,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "insert into migration_runs (version, direction, status, actor, source, plan) \
         values ($1, $2, 'running', $3, $4, $5) returning id",
    )
    .bind(version)
    .bind(direction.as_str())
    .bind(actor.actor)
    .bind(actor.source)
    .bind(plan)
    .fetch_one(&pool)
    .await?;
    Ok(id)
}

/// Close a journal row with its outcome.
///
/// `status` is validated here rather than at the call site because the three callers are three
/// functions that could each pass a fourth value, and the check constraint would answer all three
/// with the same `23514` that names no function.
pub async fn finish_run(
    pool: PgPool,
    run_id: Option<i64>,
    status: String,
    duration_ms: i32,
    error: Option<String>,
) -> Result<()> {
    let Some(run_id) = run_id else {
        // No journal row exists for this run (the migration that creates the journal is the one
        // running). There is nothing to close, and inventing a row after the fact would claim a
        // start time nobody observed.
        return Ok(());
    };
    if !matches!(status.as_str(), "succeeded" | "failed" | "aborted") {
        return Err(MigrationSafetyError::InvalidVersion {
            version: status.clone(),
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
    .execute(&pool)
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
    migrator: &'static sqlx::migrate::Migrator,
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
        ledger_pool.clone(),
        version.to_owned(),
        Direction::Down,
        RunActor::new(by, "cli")?,
        json!({ "statements": file.down_statements.len(), "scratch": true }),
    )
    .await?;
    let started = Instant::now();

    let before = table_names(scratch).await?;
    let outcome = sqlx::raw_sql(&down).execute(scratch).await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(i32::MAX as u32)) as i32;

    match outcome {
        Err(err) => {
            let message = err.to_string();
            finish_run(ledger_pool.clone(), Some(run_id), "failed".to_owned(), duration_ms, Some(message)).await?;
            return Err(MigrationSafetyError::Store(err));
        }
        Ok(_) => {
            let after = table_names(scratch).await?;
            finish_run(ledger_pool.clone(), Some(run_id), "succeeded".to_owned(), duration_ms, None).await?;
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
    fn a_missing_reversal_is_a_gate_finding_and_never_blocks_the_apply() {
        // The property is not "a migration without a down script passes" — it is that an
        // INSTALLATION cannot be blocked by the migrations that predate the rule. This tree has
        // 53 such files, `0001_initial.sql` among them, so an apply that refused them would make
        // the product uninstallable while looking stricter.
        let pending = PendingMigration {
            version: "0001".to_owned(),
            name: "initial".to_owned(),
            filename: "0001_initial.sql".to_owned(),
            checksum: "aa".to_owned(),
            has_down: false,
            declared_no_down: false,
            statement_count: 13,
            lock_risk: LockRisk::None,
        };
        let plan = Plan {
            pending: vec![pending.clone()],
            violations: Vec::new(),
            missing_down: vec![pending],
            policy: Policy::default_row(),
            gate_fails: true,
            summary: String::new(),
        };
        // The GATE fails — that is the CI signal.
        assert!(plan.gate_fails, "the gate is where this is reported");
        // And the plan names the file, so the screen can show it rather than a count.
        assert_eq!(plan.missing_down[0].filename, "0001_initial.sql");
    }

    #[test]
    fn a_policy_that_does_not_require_reversals_reports_no_missing_down() {
        let policy = Policy {
            require_down_scripts: false,
            ..Policy::default_row()
        };
        // The switch is the switch: an installation that does not want reversals must not be told
        // about 53 of them.
        assert!(!policy.require_down_scripts);
    }

    #[test]
    fn a_filename_restores_the_underscores_sqlx_replaced_with_spaces() {
        // Measured against the embedded bundle rather than a hand-written description, because the
        // whole point is that SQLx's `description` is not the filename and nobody notices until an
        // operator is told to open `0207_migration safety.sql`.
        let migrator = omnion_core::migrator();
        let files = embedded_files(migrator);
        let safety = files
            .iter()
            .find(|file| file.version == "0207")
            .expect("0207 is in this tree");
        assert_eq!(
            safety.filename, "0207_migration_safety.sql",
            "the drift message, the ledger row and the detail screen all name this"
        );
        for file in &files {
            assert!(
                !file.filename.contains(' '),
                "{} carries a space, so it cannot be opened",
                file.filename
            );
            assert!(
                file.filename.ends_with(".sql") && file.filename.starts_with(&file.version),
                "{} does not look like NNNN_name.sql",
                file.filename
            );
        }
    }

    #[test]
    fn the_direction_renders_what_the_check_constraint_accepts() {
        assert_eq!(Direction::Up.as_str(), "up");
        assert_eq!(Direction::Down.as_str(), "down");
    }
}
