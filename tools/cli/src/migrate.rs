//! `omnion migrate` — apply, inspect, plan and rehearse database migrations
//! (docs/requests/REQ-129, slice 1).
//!
//! ## Why this command grew beyond "apply pending"
//!
//! `omnion migrate` was one thing: run SQLx's migrator and print what happened. REQ-129 asks
//! for a ledger, a policy and a reversibility gate, and each of those needs an answer an operator
//! can get *without* applying anything. So the command now has four actions, and the split is the
//! point:
//!
//! * `up` — the original behaviour, plus the ledger row and the policy gate. Still idempotent.
//! * `status` — the ledger, checksum drift and the lock holder. Read-only, so it is safe on a
//!   production installation and is what a deploy pipeline asks first.
//! * `plan` — what a run would do, executing nothing.
//! * `verify-down` — rehearse a reversal **on a scratch database**. The scratch URL is a required
//!   flag rather than something derived from the connection, because a derivation is exactly the
//!   kind of convenience that ends up rehearsing a reversal on production. The command has no
//!   default for it and no way to express "the one I am connected to".
//!
//! ## The exit code is the machine-readable half
//!
//! `0` success, `1` the environment or the migration refused, `2` the command line was wrong —
//! the same contract `doctor` and `setup` already use, so a pipeline needs no special case per
//! action. A gate that exits `0` while reporting a drift warning is a gate nobody reads, so the
//! drift refusal is exit `1` and the message names the file.

use std::process::ExitCode;

use omnion_core::{Db, PgPool};
use omnion_core::config::Config;
use omnion_migrations::{ledger, lock, policy, runner};

use crate::args::MigrateOptions;

/// Run the requested action.
pub async fn run(options: &MigrateOptions) -> ExitCode {
    match execute(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("omnion migrate: {message}");
            ExitCode::FAILURE
        }
    }
}

/// What the action was, after defaults are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Up,
    Status,
    Plan,
    VerifyDown,
}

impl Action {
    /// Resolve the sub-action, defaulting to `up`.
    ///
    /// An unknown action is a usage error rather than a default: `omnion migrate verify-donw`
    /// silently applying migrations is the failure mode this default direction creates, and the
    /// whole command is about not doing the dangerous thing by accident.
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("up" | "apply") => Ok(Self::Up),
            Some("status" | "ledger") => Ok(Self::Status),
            Some("plan" | "dry-run") => Ok(Self::Plan),
            Some("verify-down" | "verify_down") => Ok(Self::VerifyDown),
            Some(other) => Err(format!(
                "unknown sub-action {other:?}: expected up, status, plan or verify-down"
            )),
        }
    }
}

async fn execute(options: &MigrateOptions) -> Result<(), String> {
    let action = Action::parse(options.action.as_deref())?;
    let config = Config::from_env().map_err(|err| err.to_string())?;
    let db = Db::connect(&config.database)
        .await
        .map_err(|err| format!("could not connect to the database: {err}"))?;
    let pool = db.pool();

    match action {
        Action::Up => apply(pool, options).await,
        Action::Status => status(pool).await,
        Action::Plan => plan(pool).await,
        Action::VerifyDown => verify_down(pool, options, &config.database).await,
    }
}

/// The actor the journal records, defaulting to the OS user.
///
/// Defaulting rather than requiring the flag is deliberate: the deploy job runs this command with
/// no human present, and a required flag would make the original `omnion migrate` invocation —
/// which every existing pipeline already uses — fail. The *source* is what distinguishes a deploy
/// from an operator, and it defaults to `cli` for the same reason.
fn default_actor() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "omnion-cli".to_owned())
}

async fn apply(pool: &PgPool, options: &MigrateOptions) -> Result<(), String> {
    let policy = policy::read(pool).await.map_err(|err| err.to_string())?;
    let actor = runner::RunActor::new(
        options.actor.clone().unwrap_or_else(default_actor),
        options.source.clone().unwrap_or_else(|| "cli".to_owned()),
    )
    .map_err(|err| err.to_string())?;

    // One version means one migration, which the runner does not currently support directly — so
    // the flag is refused here rather than silently ignored. A flag that is accepted and ignored
    // is the worst kind, and this is the kind an operator types during an incident.
    if options.version.is_some() {
        return Err(
            "`--version` selects one migration for `verify-down`; an apply is always the whole set"
                .to_owned(),
        );
    }

    let migrator = omnion_core::migrator();
    // `runner::apply` takes the pool, the policy and the actor BY VALUE. `PgPool` is an Arc'd
    // handle, so cloning it shares one pool rather than opening a second — that is what the
    // `&PgPool` in the signature above is for. The policy and actor are cheap owned structs, so
    // they move. This compiled as `&pool, &policy, &actor` until a `cargo build -p omnion-cli`
    // ran: the runner's signatures are `Policy` and `RunActor` by value, so the CLI — and with it
    // every operator-facing `omnion migrate` command — had not compiled since the surface landed.
    let report = runner::apply(pool.clone(), &migrator, policy, actor)
        .await
        .map_err(|err| err.to_string())?;
    println!("{}", report.summary);
    Ok(())
}

async fn status(pool: &PgPool) -> Result<(), String> {
    let migrator = omnion_core::migrator();
    let files = runner::embedded_files(&migrator);
    let applied = runner::applied_versions(pool)
        .await
        .map_err(|err| err.to_string())?;

    let pending: Vec<&runner::MigrationFile> = files
        .iter()
        .filter(|file| {
            file.version
                .parse::<i64>()
                .map(|version| !applied.contains(&version))
                .unwrap_or(false)
        })
        .collect();

    // The ledger table is absent on an installation whose binary predates 0207. That is "no
    // ledger yet", printed as such rather than as an empty ledger, because the two mean different
    // things to somebody deciding whether to trust the schema.
    let ledger_rows = ledger::list(pool).await.unwrap_or_default();
    if ledger_rows.is_empty() {
        println!("ledger:    none yet (this installation predates the migration ledger)");
    }
    for row in &ledger_rows {
        let verified = match row.down_verified_at {
            Some(_) => "reversal rehearsed",
            None if row.has_down => "reversal never rehearsed",
            None => "no reversal",
        };
        println!(
            "applied   {} {} ({} ms, {} statements, by {} from {}, {verified})",
            row.version,
            row.name,
            row.duration_ms,
            row.statement_count,
            row.actor,
            row.source
        );
    }

    if pending.is_empty() {
        println!("pending:   none ({} migration(s) applied)", ledger_rows.len());
    } else {
        println!("pending:   {}", pending.len());
        for file in &pending {
            println!(
                "  would run  {} ({} statement(s), reversal: {})",
                file.filename,
                file.statement_count,
                if file.down_statements.is_empty() {
                    "none"
                } else {
                    "present"
                }
            );
        }
    }

    // Drift is checked last and REFUSED, because it is the one finding on this screen that means
    // the schema on disk and the schema in the database have parted company.
    let drifts = ledger::detect_drift(
        &ledger::drift_input(pool)
            .await
            .map_err(|err| err.to_string())?,
        &files
            .iter()
            .map(|file| (file.version.clone(), file.name.clone(), file.checksum.clone()))
            .collect::<Vec<_>>(),
    );
    for drift in &drifts {
        eprintln!("drift:     {}", drift.message());
    }

    let lock = lock::view(pool).await;
    if lock.held {
        println!(
            "lock:      held by {} {} ({}s, by {} from {})",
            lock.direction.as_deref().unwrap_or("up"),
            lock.version.as_deref().unwrap_or("?"),
            lock.age_seconds.unwrap_or(0),
            lock.actor.as_deref().unwrap_or("?"),
            lock.source.as_deref().unwrap_or("?")
        );
    } else {
        println!("lock:      free ({})", lock.lock_key);
    }

    if drifts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} migration(s) drift from the ledger; the runner refuses to apply until this is \
             resolved",
            drifts.len()
        ))
    }
}

async fn plan(pool: &PgPool) -> Result<(), String> {
    let migrator = omnion_core::migrator();
    let policy = policy::read(pool).await.map_err(|err| err.to_string())?;
    let plan = runner::plan(pool.clone(), &migrator, policy)
        .await
        .map_err(|err| err.to_string())?;

    println!("{}", plan.summary);
    println!(
        "policy:    down scripts {}, lock_timeout {}ms, statement_timeout {}ms, backfill batch {} \
         at {}/s",
        if plan.policy.require_down_scripts {
            "required"
        } else {
            "optional"
        },
        plan.policy.lock_timeout_ms,
        plan.policy.statement_timeout_ms,
        plan.policy.backfill_batch_size,
        plan.policy.backfill_rate_per_second
    );
    for entry in &plan.pending {
        println!(
            "  {} — {} statement(s), lock risk {:?}, reversal {}",
            entry.filename,
            entry.statement_count,
            entry.lock_risk,
            if entry.has_down { "present" } else { "absent" }
        );
    }
    for finding in &plan.violations {
        println!(
            "  finding  {}:{} {} ({}{})",
            finding.version,
            finding.line,
            finding.pattern,
            if finding.commented { "commented, " } else { "" },
            if finding.fails_gate() { "blocking" } else { "advisory" }
        );
    }

    // A gate that fails is exit 1 even though nothing ran: `plan` is what CI calls, and a CI step
    // that exits 0 on a finding nobody reads is a step that was switched off.
    if plan.gate_fails {
        return Err("the plan contains a finding that fails the migration gate".to_owned());
    }
    Ok(())
}

async fn verify_down(
    pool: &PgPool,
    options: &MigrateOptions,
    database: &omnion_core::config::DatabaseConfig,
) -> Result<(), String> {
    let version = options.version.as_deref().ok_or_else(|| {
        "`verify-down` needs --version: rehearsing \"a migration\" is not an action".to_owned()
    })?;
    let scratch_url = options.scratch.as_deref().ok_or_else(|| {
        "`verify-down` needs --scratch. It is required and has no default on purpose: a \
         rehearsal that ran against the database you are connected to would be a rollback on \
         production, and this command must not be able to express that."
            .to_owned()
    })?;

    // A bare name is a database on the same server, so the operator's credentials and host carry
    // over. A full URL is used verbatim, which is the escape hatch for a scratch instance on a
    // different host — the request's rule is that the rehearsal must be on a scratch database,
    // not that it must be on the same server.
    let scratch_config = omnion_core::config::DatabaseConfig {
        url: omnion_core::same_server_url(&database.url, scratch_url)
            .unwrap_or_else(|| scratch_url.to_owned()),
        max_connections: 2,
    };
    let scratch = Db::connect(&scratch_config)
        .await
        .map_err(|err| format!("could not connect to the scratch database: {err}"))?;
    let scratch_pool = scratch.pool().clone();

    let migrator = omnion_core::migrator();
    let actor = options.actor.clone().unwrap_or_else(default_actor);
    let report = runner::verify_down(pool, &scratch_pool, &migrator, version, &actor)
        .await
        .map_err(|err| err.to_string())?;

    println!(
        "rehearsed  {} ({} statement(s), {} ms)",
        report.filename, report.statements, report.duration_ms
    );
    if !report.restored {
        let removed: Vec<&String> = report
            .tables_before
            .iter()
            .filter(|table| !report.tables_after.contains(table))
            .collect();
        let added: Vec<&String> = report
            .tables_after
            .iter()
            .filter(|table| !report.tables_before.contains(table))
            .collect();
        // A rehearsal that ran and left a different structure is the finding the CI gate exists
        // for, so the message names WHAT is different rather than only that something is.
        return Err(format!(
            "the reversal did not restore the structure: {} table(s) left behind ({}), {} table(s) \
             created ({}). The ledger records this migration as NOT rehearsed.",
            removed.len(),
            removed.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", "),
            added.len(),
            added.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    println!("restored:  the scratch structure matches what the apply left");
    Ok(())
}
