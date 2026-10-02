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

use omnion_core::config::Config;
use omnion_core::{Db, PgPool};
use omnion_migrations::{ledger, lock, policy, runner};

use crate::args::MigrateOptions;

/// Run the requested action.
///
/// `json` selects the documented envelope (REQ-131 acceptance 15) and `quiet` silences the human
/// report. Both change only *how the outcome is written down*: the actions, the refusals and the
/// exit codes below are identical either way. That is the property a gate depends on — if JSON
/// mode could exit `0` where the human run exits `1`, `--json` would be a way to turn a drifted
/// ledger into a green pipeline.
pub async fn run(options: &MigrateOptions, json: bool, quiet: bool) -> ExitCode {
    let sink = crate::output::Sink::new(json, quiet);

    match execute(options, sink).await {
        Ok(data) => {
            if json {
                crate::envelope::print(&crate::envelope::success("migrate", data, &[]));
            }
            ExitCode::SUCCESS
        }
        Err(outcome) => {
            // The code arrives with the refusal. `classify` is no longer consulted here: it is
            // called at the boundary where a migration-runner sentence becomes a code, which is
            // the only place that still needs it, so there is one classifier and one place it
            // runs. `migrate new` goes further and never produces a sentence at all.
            if json {
                crate::envelope::print_failure("migrate", outcome.failure(), &[]);
            }
            eprintln!("omnion migrate: {}", outcome.failure().message);
            ExitCode::FAILURE
        }
    }
}

/// Why an action failed, carrying the documented code with it.
///
/// The four older actions produce sentences — they come from the migration runner across a crate
/// boundary that has no opinion about the CLI's vocabulary, so `classify` is the adapter that
/// turns one of those sentences into a code. `migrate new` produces a typed
/// [`crate::new_migration::Refusal`] instead, and its own `code()` is the answer: a developer who
/// typed a capital letter is told `usage` by the value itself, not by a string that happens to
/// contain a recognisable phrase.
///
/// This is what keeps the two from disagreeing. Before this type, the only exit was a `String`,
/// so the typed code was consulted nowhere in production and existed to satisfy a test — and a
/// reworded sentence would have silently moved a refusal to `internal`.
enum Outcome {
    /// A sentence from the runner, classified at the boundary.
    Classified(crate::envelope::Failure),
    /// A refusal that already knows its code.
    Typed(crate::envelope::Failure),
}

/// A refusal that already knows its own code.
///
/// This is the one call site of `Refusal::code()` in the whole binary. It exists so the typed
/// answer is what a script gets, and so the string classifier has nothing left to disagree with.
fn typed(refusal: crate::new_migration::Refusal) -> Outcome {
    Outcome::Typed(crate::envelope::Failure::new(
        refusal.code(),
        refusal.message(),
    ))
}

impl Outcome {
    /// The failure to report, whichever shape it arrived in.
    fn failure(&self) -> &crate::envelope::Failure {
        match self {
            Self::Classified(failure) | Self::Typed(failure) => failure,
        }
    }
}

/// Pick the documented error code for a refusal message.
///
/// Classified from the sentence the migration runner already writes, rather than by threading a
/// code through every `Err(String)` in the chain. That is a deliberate trade: it keeps the
/// runner's messages free of contract vocabulary it has no business knowing, at the cost of a
/// mapping that goes stale if a message is reworded. Each arm therefore quotes a fragment that
/// the runner does not change lightly, and the fallback is `internal` rather than a guess — a
/// script that sees `internal` knows the CLI could not classify, which is honest; a script that
/// saw a wrong-but-plausible code would act on it.
fn classify(message: &str) -> crate::envelope::ErrorCode {
    use crate::envelope::ErrorCode;
    let lowered = message.to_lowercase();

    // The gate is checked BEFORE the generic arms on purpose. "the plan contains a finding that
    // fails the migration gate" contains none of the words below, so it used to fall through to
    // `internal` — which tells a script the CLI could not classify the failure, when in fact it
    // is the most predictable refusal in the command: the migration policy rejected a file. A
    // script branching on the code needs this one to be stable and named.
    if lowered.contains("gate") {
        ErrorCode::Refused
    } else if lowered.contains("migrations directory")
        || lowered.contains("cannot be written to")
        || lowered.contains("could not write the migration file")
    {
        // Check BEFORE the database arms, and the order is the whole point: this sentence names
        // `database/migrations`, so the `database` arm catches it and a `new` refused for a
        // missing checkout is reported to a script as a database outage. The two tests at the
        // bottom of this module exist because of exactly that — the first version of this
        // function put these arms after `database` and every assertion was green against a
        // table nobody re-read, because the table was written from the same function.
        ErrorCode::ConfigUnreadable
    } else if lowered.contains("checksum")
        || lowered.contains("drift")
        || lowered.contains("ledger")
    {
        ErrorCode::DatabaseUnreachable
    } else if lowered.contains("connect")
        || lowered.contains("database")
        || lowered.contains("pool")
    {
        ErrorCode::DatabaseUnreachable
    } else if lowered.contains("scratch") {
        ErrorCode::ConfirmationRequired
    } else if lowered.contains("lock") {
        ErrorCode::Refused
    } else if lowered.contains("already exists")
        || lowered.contains("lower_snake_case")
        || lowered.contains("needs a name")
    {
        // `migrate new` refusals about the NAME, and only those: the same three fragments
        // `new_migration::Refusal::code()` maps to `Usage`. Routing them through the generic arms
        // would report a developer who typed a capital letter as a database outage.
        //
        // Every fragment here is a piece of a message that arm of `Refusal::code()` also
        // recognises, and `the_two_classifications_agree` below asserts the two agree for all
        // six refusals. They are two views of one decision, and without that assertion a
        // reworded message would silently turn a usage error into `internal` — which tells a
        // script the CLI could not classify, and tells the operator nothing at all.
        ErrorCode::Usage
    } else if lowered.contains("unknown") || lowered.contains("takes") || lowered.contains("--") {
        ErrorCode::Usage
    } else {
        ErrorCode::Internal
    }
}

/// What the action was, after defaults are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Up,
    Status,
    Plan,
    VerifyDown,
    New,
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
            Some("new" | "create") => Ok(Self::New),
            Some(other) => Err(format!(
                "unknown sub-action {other:?}: expected up, status, plan, new or verify-down"
            )),
        }
    }
}

async fn execute(
    options: &MigrateOptions,
    sink: crate::output::Sink,
) -> Result<serde_json::Value, Outcome> {
    let action = Action::parse(options.action.as_deref()).map_err(|message| {
        Outcome::Classified(crate::envelope::Failure::new(
            crate::envelope::ErrorCode::Usage,
            message,
        ))
    })?;

    // `new` runs BEFORE the connection, deliberately. The other four actions are about a
    // database, but this one writes a file into a working tree, and an author who needs a
    // running PostgreSQL to create a migration cannot create one on a laptop, in CI, or during
    // the first five minutes of a new checkout — which is exactly when they do it.
    //
    // It is dispatched here, above the `Config`/`Db` lines, rather than inside the `match` below,
    // so the connection it never needs is never opened. `create_migration` still reads the
    // ledger on a best-effort basis, because a number the installation has already run is
    // exactly the half a tree scan cannot see — but that read failing is a warning, never a
    // refusal, so the command works with no database at all.
    if action == Action::New {
        return create_migration(options, sink).await;
    }

    let config = Config::from_env().map_err(|err| {
        Outcome::Classified(crate::envelope::Failure::new(
            crate::envelope::ErrorCode::ConfigUnreadable,
            err.to_string(),
        ))
    })?;
    let db = Db::connect(&config.database).await.map_err(|err| {
        Outcome::Classified(crate::envelope::Failure::new(
            crate::envelope::ErrorCode::DatabaseUnreachable,
            format!("could not connect to the database: {err}"),
        ))
    })?;
    let pool = db.pool();

    // The one adapter: every runner sentence becomes a code here, and nowhere else. The four
    // bodies keep returning `String` because their errors genuinely are sentences — the runner
    // has no vocabulary of its own to give up.
    let result = match action {
        Action::Up => apply(pool, options, sink).await,
        Action::Status => status(pool, sink).await,
        Action::Plan => plan(pool, sink).await,
        Action::VerifyDown => verify_down(pool, options, &config.database, sink).await,
        // Dispatched above, before the connection is made.
        Action::New => unreachable!("`new` returns before the database is connected"),
    };
    result.map_err(|message| {
        Outcome::Classified(crate::envelope::Failure::new(classify(&message), message))
    })
}

/// Allocate the next migration number and write the file.
async fn create_migration(
    options: &MigrateOptions,
    sink: crate::output::Sink,
) -> Result<serde_json::Value, Outcome> {
    let explicit = options
        .directory
        .clone()
        .or_else(crate::new_migration::directory_from_env);
    let directory = crate::new_migration::resolve_directory(explicit.as_deref())
        .map_err(typed)?;

    // Best-effort ledger read, for the numbers no tree can show. A failure here is a warning in
    // the output, never a refusal: refusing here would make the one moment a migration is
    // created — a fresh checkout, a laptop, a CI container — the one moment it cannot be.
    let mut applied = std::collections::BTreeSet::new();
    let mut ledger_seen = false;
    let mut ledger_error: Option<String> = None;
    match Config::from_env() {
        Ok(config) => match Db::connect(&config.database).await {
            Ok(db) => {
                applied = crate::new_migration::ledger_versions(db.pool()).await;
                ledger_seen = true;
            }
            Err(err) => ledger_error = Some(err.to_string()),
        },
        Err(err) => ledger_error = Some(err.to_string()),
    }

    let name = options.name.as_deref().unwrap_or_default();
    let created = crate::new_migration::create(&directory, name, &applied).map_err(typed)?;

    // Read back what the RUNNER will see, and report it, rather than only what the template
    // intended. The two are different questions and the gap between them is a class of surprise:
    // a file whose prose reversal block parses as a statement, or whose forward half already
    // contains a statement nobody wrote. `migrate plan` would catch that later, on another
    // machine, at another moment — the point of answering it here is that this is the file the
    // author is about to open.
    let (forward_statements, has_reversal, declared_irreversible) =
        crate::new_migration::inspect(&created.path).map_err(typed)?;

    sink.print(format_args!(
        "created   {}",
        created
            .path
            .strip_prefix(&created.directory)
            .unwrap_or(&created.path)
            .display()
    ));
    sink.print(format_args!(
        "in        {}",
        created.directory.display()
    ));

    // The two states a freshly created file is in, stated as lines rather than warnings. Both are
    // the EXPECTED state of a skeleton nobody has filled in, so an envelope warning would make
    // every generated migration look alarming — the warnings array is for things worth noticing,
    // and "you have not written the migration yet" is not one.
    if forward_statements == 0 {
        sink.print(format_args!(
            "status    the forward half is still the placeholder"
        ));
    }
    if !has_reversal && !declared_irreversible {
        sink.print(format_args!(
            "status    no reversal yet — the policy reports {} as not reversible until you \
             write one",
            created.version
        ));
    } else if declared_irreversible {
        sink.print(format_args!(
            "status    declared irreversible (omnion:no-down); the deployment plan will ask for \
             a backup"
        ));
    }

    let mut data = serde_json::json!({
        "action": "new",
        "version": created.version,
        "name": created.name,
        "path": created.path.display().to_string(),
        "directory": created.directory.display().to_string(),
        // The two sources, reported so an operator can see what the number was derived from
        // rather than having to trust it.
        "ledger_versions": applied.len(),
        "ledger_consulted": ledger_seen,
        // What the runner will read in the file that was just written — the same three numbers
        // `inspect` derived, so a script can assert on them without re-parsing the file.
        "forward_statements": forward_statements,
        "has_reversal": has_reversal,
        "declared_irreversible": declared_irreversible,
    });

    if let Some(err) = &ledger_error {
        sink.print(format_args!(
            "warning:  the migration ledger could not be read ({err}); the number came from the \
             tree only"
        ));
        data["ledger_error"] = serde_json::Value::String(err.clone());
    }

    Ok(data)
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

async fn apply(
    pool: &PgPool,
    options: &MigrateOptions,
    sink: crate::output::Sink,
) -> Result<serde_json::Value, String> {
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
    sink.print(format_args!("{}", report.summary));
    Ok(serde_json::json!({
        "action": "up",
        "summary": report.summary,
        "actor": options.actor.clone().unwrap_or_else(default_actor),
        "source": options.source.clone().unwrap_or_else(|| "cli".to_owned()),
    }))
}

async fn status(pool: &PgPool, sink: crate::output::Sink) -> Result<serde_json::Value, String> {
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
        sink.print(format_args!(
            "ledger:    none yet (this installation predates the migration ledger)"
        ));
    }

    // The payload is assembled from the rows this function already read, so the document and the
    // table can never disagree — a second read for the JSON would be a second answer to a
    // question about a database that may have moved between the two.
    let applied_entries: Vec<serde_json::Value> = ledger_rows
        .iter()
        .map(|row| {
            let verified = match row.down_verified_at {
                Some(_) => "rehearsed",
                None if row.has_down => "never_rehearsed",
                None => "none",
            };
            sink.print(format_args!(
                "applied   {} {} ({} ms, {} statements, by {} from {}, {})",
                row.version,
                row.name,
                row.duration_ms,
                row.statement_count,
                row.actor,
                row.source,
                verified.replace('_', " ")
            ));
            serde_json::json!({
                "version": row.version,
                "name": row.name,
                "duration_ms": row.duration_ms,
                "statement_count": row.statement_count,
                "actor": row.actor,
                "source": row.source,
                "reversal": verified,
            })
        })
        .collect();

    let pending_entries: Vec<serde_json::Value> = pending
        .iter()
        .map(|file| {
            let has_down = !file.down_statements.is_empty();
            sink.print(format_args!(
                "  would run  {} ({} statement(s), reversal: {})",
                file.filename,
                file.statement_count,
                if has_down { "present" } else { "none" }
            ));
            serde_json::json!({
                "filename": file.filename,
                "statement_count": file.statement_count,
                "has_reversal": has_down,
            })
        })
        .collect();

    if pending.is_empty() {
        sink.print(format_args!(
            "pending:   none ({} migration(s) applied)",
            ledger_rows.len()
        ));
    } else {
        sink.print(format_args!("pending:   {}", pending.len()));
    }

    // Drift is checked last and REFUSED, because it is the one finding on this screen that means
    // the schema on disk and the schema in the database have parted company.
    let drifts = ledger::detect_drift(
        &ledger::drift_input(pool)
            .await
            .map_err(|err| err.to_string())?,
        &files
            .iter()
            .map(|file| {
                (
                    file.version.clone(),
                    file.name.clone(),
                    file.checksum.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    for drift in &drifts {
        eprintln!("drift:     {}", drift.message());
    }

    let lock = lock::view(pool).await;
    if lock.held {
        sink.print(format_args!(
            "lock:      held by {} {} ({}s, by {} from {})",
            lock.direction.as_deref().unwrap_or("up"),
            lock.version.as_deref().unwrap_or("?"),
            lock.age_seconds.unwrap_or(0),
            lock.actor.as_deref().unwrap_or("?"),
            lock.source.as_deref().unwrap_or("?")
        ));
    } else {
        sink.print(format_args!("lock:      free ({})", lock.lock_key));
    }

    // The document is built even when the run is about to fail: drift is exactly the case where a
    // script most needs the per-file detail, and a failure envelope that carried only a sentence
    // would make `omnion migrate status --json` least useful at the moment it matters most.
    let payload = serde_json::json!({
        "ledger": if ledger_rows.is_empty() { "absent" } else { "present" },
        "applied": applied_entries,
        "applied_count": ledger_rows.len(),
        "pending": pending_entries,
        "pending_count": pending.len(),
        "drift": drifts.iter().map(|drift| drift.message()).collect::<Vec<_>>(),
        "lock": {
            "held": lock.held,
            "key": lock.lock_key,
            "direction": lock.direction,
            "version": lock.version,
            "actor": lock.actor,
            "source": lock.source,
        },
    });

    if drifts.is_empty() {
        Ok(payload)
    } else {
        Err(format!(
            "{} migration(s) drift from the ledger; the runner refuses to apply until this is \
             resolved",
            drifts.len()
        ))
    }
}

async fn plan(pool: &PgPool, sink: crate::output::Sink) -> Result<serde_json::Value, String> {
    let migrator = omnion_core::migrator();
    let policy = policy::read(pool).await.map_err(|err| err.to_string())?;
    let plan = runner::plan(pool.clone(), &migrator, policy)
        .await
        .map_err(|err| err.to_string())?;

    sink.print(format_args!("{}", plan.summary));
    sink.print(format_args!(
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
    ));

    let pending_entries: Vec<serde_json::Value> = plan
        .pending
        .iter()
        .map(|entry| {
            sink.print(format_args!(
                "  {} — {} statement(s), lock risk {:?}, reversal {}",
                entry.filename,
                entry.statement_count,
                entry.lock_risk,
                if entry.has_down { "present" } else { "absent" }
            ));
            serde_json::json!({
                "filename": entry.filename,
                "statement_count": entry.statement_count,
                "lock_risk": format!("{:?}", entry.lock_risk),
                "has_reversal": entry.has_down,
            })
        })
        .collect();

    let findings: Vec<serde_json::Value> = plan
        .violations
        .iter()
        .map(|finding| {
            sink.print(format_args!(
                "  finding  {}:{} {} ({}{})",
                finding.version,
                finding.line,
                finding.pattern,
                if finding.commented { "commented, " } else { "" },
                if finding.fails_gate() {
                    "blocking"
                } else {
                    "advisory"
                }
            ));
            serde_json::json!({
                "version": finding.version,
                "line": finding.line,
                "pattern": finding.pattern,
                "commented": finding.commented,
                "blocking": finding.fails_gate(),
            })
        })
        .collect();

    let payload = serde_json::json!({
        "summary": plan.summary,
        "gate_fails": plan.gate_fails,
        "pending": pending_entries,
        "findings": findings,
        "policy": {
            "require_down_scripts": plan.policy.require_down_scripts,
            "lock_timeout_ms": plan.policy.lock_timeout_ms,
            "statement_timeout_ms": plan.policy.statement_timeout_ms,
            "backfill_batch_size": plan.policy.backfill_batch_size,
            "backfill_rate_per_second": plan.policy.backfill_rate_per_second,
        },
    });

    // A gate that fails is exit 1 even though nothing ran: `plan` is what CI calls, and a CI step
    // that exits 0 on a finding nobody reads is a step that was switched off.
    if plan.gate_fails {
        return Err("the plan contains a finding that fails the migration gate".to_owned());
    }
    Ok(payload)
}

async fn verify_down(
    pool: &PgPool,
    options: &MigrateOptions,
    database: &omnion_core::config::DatabaseConfig,
    sink: crate::output::Sink,
) -> Result<serde_json::Value, String> {
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

    sink.print(format_args!(
        "rehearsed  {} ({} statement(s), {} ms)",
        report.filename, report.statements, report.duration_ms
    ));
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
            removed
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            added.len(),
            added
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    sink.print(format_args!(
        "restored:  the scratch structure matches what the apply left"
    ));
    Ok(serde_json::json!({
        "action": "verify-down",
        "version": version,
        "filename": report.filename,
        "statements": report.statements,
        "duration_ms": report.duration_ms,
        "restored": report.restored,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::ErrorCode;

    #[test]
    fn every_refusal_a_caller_can_reach_maps_to_a_documented_code() {
        // The classification is a public contract, so it is pinned with the exact sentences the
        // command writes. `internal` appears exactly once — for a message nobody has written — so
        // that a new refusal is forced to be classified rather than falling through by default.
        let cases: &[(&str, ErrorCode)] = &[
            (
                "the plan contains a finding that fails the migration gate",
                ErrorCode::Refused,
            ),
            (
                "2 migration(s) drift from the ledger; the runner refuses to apply until this is \
                 resolved",
                ErrorCode::DatabaseUnreachable,
            ),
            (
                "could not connect to the database: pool timed out",
                ErrorCode::DatabaseUnreachable,
            ),
            (
                "`verify-down` needs --scratch. It is required and has no default on purpose",
                ErrorCode::ConfirmationRequired,
            ),
            (
                "another migration run holds the lock (up 0207, 12s, by ci-bot from ci)",
                ErrorCode::Refused,
            ),
            (
                "unknown sub-action \"staus\": expected up, status, plan, new or verify-down",
                ErrorCode::Usage,
            ),
            (
                "`--version` selects one migration for `verify-down`; an apply is always the \
                 whole set",
                ErrorCode::Usage,
            ),
            // `migrate new` — acceptance 4. The codes here are asserted against
            // `Refusal::code()` in `the_two_classifications_agree`, so a reworded message shows up
            // as a disagreement rather than as a script reading a code nobody chose.
            (
                "migrate new needs a name: `omnion migrate new add_invoices`",
                ErrorCode::Usage,
            ),
            (
                "a migration name must be lower_snake_case: \"Add Invoices\" is not \
                 lower_snake_case: use a, z, 0-9 and underscores only",
                ErrorCode::Usage,
            ),
            (
                "a migration named \"add_invoices\" already exists (0004_add_invoices.sql); pick \
                 another name",
                ErrorCode::Usage,
            ),
            (
                "could not find the migrations directory: no database/migrations next to a \
                 Cargo.toml above the working directory. Run this from inside the checkout, or \
                 set OMNION_MIGRATIONS_DIR.",
                ErrorCode::ConfigUnreadable,
            ),
            ("/tmp/migrations cannot be written to (Permission denied)", ErrorCode::ConfigUnreadable),
            (
                "could not write the migration file: disk full",
                ErrorCode::ConfigUnreadable,
            ),
            ("something nobody has written yet", ErrorCode::Internal),
        ];

        for (message, expected) in cases {
            assert_eq!(
                classify(message),
                *expected,
                "the code for {message:?} is what a script branches on"
            );
        }
    }

    #[test]
    fn a_classified_refusal_is_never_left_as_the_fallback_by_accident() {
        // The fallback exists for the unknown, not as a shortcut. Every sentence this module
        // writes is in the table above, so if a rewording drops one out of its arm the failure
        // shows up here as `internal` rather than in someone's pipeline.
        let written = [
            "the plan contains a finding that fails the migration gate",
            "2 migration(s) drift from the ledger; the runner refuses to apply until this is \
             resolved",
            "could not connect to the database: refused",
            "could not connect to the scratch database: refused",
            "`verify-down` needs --version: rehearsing \"a migration\" is not an action",
            "`verify-down` needs --scratch. It is required and has no default on purpose",
            "unknown sub-action \"x\": expected up, status, plan, new or verify-down",
            "`--version` selects one migration for `verify-down`; an apply is always the whole set",
            "the reversal did not restore the structure: 1 table(s) left behind (t), 0 \
             table(s) created (). The ledger records this migration as NOT rehearsed.",
            "migrate new needs a name: `omnion migrate new add_invoices`",
            "a migration name must be lower_snake_case: \"Add Invoices\" is not lower_snake_case: \
             use a, z, 0-9 and underscores only",
            "a migration named \"add_invoices\" already exists (0004_add_invoices.sql); pick \
             another name",
            "could not find the migrations directory: no database/migrations next to a Cargo.toml \
             above the working directory. Run this from inside the checkout, or set \
             OMNION_MIGRATIONS_DIR.",
            "/tmp/migrations cannot be written to (Permission denied)",
            "could not write the migration file: disk full",
        ];

        for message in written {
            assert_ne!(
                classify(message),
                ErrorCode::Internal,
                "{message:?} is a message this command writes and must be classified"
            );
        }
    }

    #[test]
    fn the_two_classifications_agree() {
        // `migrate new` produces its refusals as a typed `Refusal`, whose `code()` the module
        // below calls directly — but `execute` funnels every error through a `String`, so the
        // message is re-classified on the way out. Two authorities for one decision is the shape
        // that drifts: the day somebody rewords "already exists", the direct path still answers
        // `usage` while the string path answers `internal`, and the script only sees whichever
        // one it happened to be triggered by.
        //
        // So the two are compared here over every refusal the new action can produce. Both read
        // the same sentences, so a reword that breaks one breaks this test rather than a
        // pipeline.
        use crate::new_migration::Refusal;
        let refusals = [
            Refusal::MissingName,
            Refusal::InvalidName("\"Add Invoices\" is not lower_snake_case".to_owned()),
            Refusal::DuplicateName {
                name: "add_invoices".to_owned(),
                existing: "0004_add_invoices.sql".to_owned(),
            },
            Refusal::NoDirectory,
            Refusal::NotWritable {
                directory: std::path::PathBuf::from("/tmp/migrations"),
                reason: "Permission denied".to_owned(),
            },
            Refusal::NotWritten("disk full".to_owned()),
        ];

        for refusal in refusals {
            let direct = refusal.code();
            let through_the_string = classify(&refusal.message());
            assert_eq!(
                through_the_string, direct,
                "{refusal:?}: the typed path says {} and the string path says {}. The message is \
                 what `execute` actually returns, so the string path is the one a script sees.",
                direct.as_str(),
                through_the_string.as_str()
            );
        }
    }
}
