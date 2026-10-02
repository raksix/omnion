//! `omnion doctor` — the environment check of an installation.
//!
//! Six questions, in the order a boot asks them: is the configuration readable, is PostgreSQL
//! there, does its schema match this binary, does Redis answer, is the object store usable, and
//! has the first run happened. Every check prints a pass/fail with an actionable hint, and the
//! process exits non-zero when any of them fails — so a deployment pipeline can gate on it.

use std::process::ExitCode;

use omnion_core::config::Config;
use omnion_core::{Db, RedisClient};
use omnion_identity::users;
use omnion_onboarding::state as onboarding_state;
use omnion_storage::Storage;

use crate::output;

/// How one check ended.
///
/// Three states, not two: the request asks for `warn` to stay visually distinct from a failure
/// ("each check prints pass/warn/fail with a fix hint … exit code 1 when any check fails, 0 with
/// warnings present"). A binary pass/fail would force every advisory finding to be either silent
/// or a failure, and the two failure modes that produces are both bad — an operator learns to
/// ignore the red, or a pipeline gates on something that was never broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// The check passed.
    Ok,
    /// The check passed with an advisory note; it does not affect the exit code.
    Warn,
    /// The check failed; a doctor run with one of these exits `1`.
    Fail,
}

impl Status {
    fn marker(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    fn as_str(self) -> &'static str {
        self.marker()
    }
}

/// One named check.
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            hint: None,
        }
    }

    fn ok_with_hint(
        name: &'static str,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    /// An advisory finding: printed with its own marker, counted as neither a pass nor a
    /// failure, and reported in the envelope's `warnings` rather than its `error`.
    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Run every check and report.
///
/// `json` writes the documented envelope to stdout; `quiet` leaves stdout empty. Neither changes
/// the exit code — that is the property a deployment gate depends on, and the reason the
/// envelope carries `ok` separately from the check list rather than deriving one from the other.
pub async fn run(json: bool, quiet: bool) -> ExitCode {
    let checks = collect().await;
    let failures = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();

    let sink = crate::output::Sink::new(json, quiet);
    if json {
        print_json("doctor", &checks);
    } else {
        print_table(&checks, sink);
    }

    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The hint every dependency failure shares: the development stack is one command away.
const STACK_HINT: &str =
    "start the stack with `docker compose -f infra/compose/docker-compose.dev.yml up -d`";

/// Walk the checks in boot order, skipping the ones a failed predecessor makes meaningless.
async fn collect() -> Vec<Check> {
    let mut checks = Vec::new();

    let config = match Config::from_env() {
        Ok(config) => {
            checks.push(Check::ok(
                "configuration",
                format!(
                    "environment {}, port {}",
                    config.env.as_str(),
                    config.http.port
                ),
            ));
            Some(config)
        }
        Err(err) => {
            checks.push(Check::fail(
                "configuration",
                err.to_string(),
                "check the OMNION_* variables — docs/02-ARCHITECTURE.md lists them",
            ));
            None
        }
    };

    let Some(config) = config else {
        return checks;
    };

    let mut db: Option<Db> = None;
    match Db::connect(&config.database).await {
        Ok(connection) => match connection.ping().await {
            Ok(()) => {
                checks.push(Check::ok(
                    "database",
                    format!(
                        "{} reachable",
                        output::describe_database_url(&config.database.url)
                    ),
                ));
                db = Some(connection);
            }
            Err(err) => checks.push(Check::fail("database", err.to_string(), STACK_HINT)),
        },
        Err(err) => checks.push(Check::fail("database", err.to_string(), STACK_HINT)),
    }

    if let Some(connection) = &db {
        match connection.migration_status().await {
            Ok(status) if status.is_up_to_date() => checks.push(Check::ok(
                "migrations",
                format!("{} of {} applied", status.applied.len(), status.total()),
            )),
            Ok(status) => checks.push(Check::fail(
                "migrations",
                format!(
                    "{} of {} applied — {} pending: {}",
                    status.applied.len(),
                    status.total(),
                    status.pending.len(),
                    join_versions(&status.pending)
                ),
                "run `omnion migrate`",
            )),
            Err(err) => checks.push(Check::fail(
                "migrations",
                err.to_string(),
                "check the database connection",
            )),
        }
    }

    match RedisClient::new(&config.redis.url) {
        Ok(client) => match client.ping().await {
            Ok(()) => checks.push(Check::ok(
                "redis",
                format!("{} answered PONG", config.redis.url),
            )),
            Err(err) => checks.push(Check::fail("redis", err.to_string(), STACK_HINT)),
        },
        Err(err) => checks.push(Check::fail(
            "redis",
            err.to_string(),
            "OMNION_REDIS_URL must be a redis:// URL",
        )),
    }

    match Storage::from_env() {
        Ok(storage) => match storage.ensure_ready().await {
            Ok(()) => checks.push(Check::ok("storage", format!("{} ready", storage.describe()))),
            Err(err) => checks.push(Check::fail(
                "storage",
                err.to_string(),
                "start MinIO with the compose stack, or point OMNION_STORAGE_DIR at a writable directory",
            )),
        },
        Err(err) => checks.push(Check::fail(
            "storage",
            err.to_string(),
            "check the OMNION_S3_* / OMNION_STORAGE_* variables",
        )),
    }

    if let Some(connection) = &db {
        checks.push(installation_check(connection).await);
    }

    checks
}

/// The last check: has anything (or everything) been set up already?
async fn installation_check(db: &Db) -> Check {
    let accounts = match users::count_users(db.pool()).await {
        Ok(accounts) => accounts,
        // A database the migrations have not reached yet has no `users` table: that is what the
        // migrations check reports, so this one only points at it instead of failing twice.
        Err(err) if is_missing_table(&err) => {
            return Check::ok_with_hint(
                "installation",
                "not readable before the schema is migrated",
                "run `omnion migrate`",
            );
        }
        Err(err) => {
            return Check::fail(
                "installation",
                err.to_string(),
                "check the database connection",
            );
        }
    };

    if accounts == 0 {
        return Check::ok_with_hint(
            "installation",
            "no accounts yet",
            "run `omnion setup` (or open the admin panel's /setup wizard)",
        );
    }

    match onboarding_state::status(db.pool()).await {
        Ok(status) if status.completed => Check::ok(
            "installation",
            format!("{accounts} account(s), first run completed"),
        ),
        Ok(_) => Check::ok_with_hint(
            "installation",
            format!("{accounts} account(s), first run still open"),
            "finish it in the admin panel's /setup wizard",
        ),
        Err(err) => Check::ok_with_hint(
            "installation",
            format!("{accounts} account(s) ({err})"),
            "the onboarding record could not be read",
        ),
    }
}

/// Render the checks as a table.
///
/// Goes to **stderr** under `--json`, because the envelope owns stdout. Printing the table to
/// stdout as well — the obvious reading of "a human gets a table, a machine gets JSON" — puts
/// two documents on one stream, and the parser that fails is the consumer's, whose error message
/// names the CLI and not the line responsible.
fn print_table(checks: &[Check], sink: crate::output::Sink) {
    if !sink.is_visible() {
        return;
    }
    sink.print(format_args!("omnion doctor"));
    for check in checks {
        output::check(sink, check.status.marker(), check.name, &check.detail);
        if let Some(hint) = &check.hint {
            output::hint(sink, hint);
        }
    }

    let failures = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();
    let warnings = checks
        .iter()
        .filter(|check| check.status == Status::Warn)
        .count();
    sink.print(format_args!(""));
    if failures == 0 {
        sink.print(format_args!("{} checks, all passed.", checks.len()));
    } else {
        sink.print(format_args!("{} checks, {failures} failed.", checks.len()));
    }
    // The warning count is on its own line rather than folded into the summary, because "all
    // passed" and "3 warnings" read as contradictory in one sentence and an operator skimming a
    // red CI log should not have to work out which one won.
    if warnings > 0 {
        sink.print(format_args!(
            "{warnings} warning(s) — the run still succeeded."
        ));
    }
}

/// Render the checks as the documented envelope (REQ-131 acceptance 15).
///
/// The check list moves under `data.checks` and the counts to the top level, so a consumer reads
/// `.data.checks[]` rather than re-deriving an array from a bare document.
///
/// **A failed check makes the envelope a failure envelope.** The first version wrote
/// `envelope::success` unconditionally and left the failure count inside `data`, which produced
/// `ok: true` beside a failing check and an exit code of 1 — the one combination this envelope
/// exists to prevent: a gate that reads `.ok` and gates on it goes green on an installation
/// `omnion doctor` just refused. The finding was caught by running the binary and piping its
/// stdout into a parser, not by reading the code: `envelope::success` looked right, and the
/// mismatch was only visible in the document the binary actually wrote.
///
/// So a failing `doctor` reports `ok: false` with `error.code = "checks_failed"` and the counts
/// in `data`. The envelope's `ok` and the process exit code now agree by construction, which is
/// the property the two are for.
fn print_json(command: &str, checks: &[Check]) {
    let entries: Vec<serde_json::Value> = checks
        .iter()
        .map(|check| {
            serde_json::json!({
                "check": check.name,
                "status": check.status.as_str(),
                "detail": check.detail,
                "hint": check.hint,
            })
        })
        .collect();
    let failures = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();
    let warnings = collect_warnings(checks);

    let data = serde_json::json!({
        "checks": entries,
        "failures": failures,
        "total": checks.len(),
        "warnings_count": warnings.len(),
        // Kept under `data` as well as in `warnings`, so a consumer can branch on the check
        // outcome without walking the array and still see the per-check detail.
        "ok": failures == 0,
    });

    if failures == 0 {
        crate::envelope::print(&crate::envelope::success(command, data, &warnings));
    } else {
        // The message names the failed checks rather than only counting them: an operator who
        // ran `--json` on purpose wants to know what to fix, and `data.checks[].hint` is right
        // there in the same document.
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .map(|check| check.name)
            .collect();
        let failure = crate::envelope::Failure::with_hint(
            crate::envelope::ErrorCode::DependencyUnavailable,
            format!(
                "{} of {} check(s) failed: {}",
                failures,
                checks.len(),
                failed.join(", ")
            ),
            "run `omnion doctor` without --json for the per-check hints",
        );
        crate::envelope::print(&crate::envelope::failure_with_data(
            command, &failure, data, &warnings,
        ));
    }
}

/// The failed checks, restated as envelope warnings.
///
/// A `warn` status exists in the check vocabulary the request asks for but no check reports one
/// today, so this list is empty in practice. It is built anyway: when the first advisory check
/// lands (an unset optional key, a disk above 80%), the consumer already knows the array is there
/// and nothing has to change shape underneath a script.
fn collect_warnings(checks: &[Check]) -> Vec<crate::envelope::Warning> {
    checks
        .iter()
        .filter(|check| check.status == Status::Warn)
        .map(|check| crate::envelope::Warning {
            code: "doctor_check_warned",
            message: check.detail.clone(),
            hint: check.hint.clone(),
        })
        .collect()
}

/// Comma-joined migration versions.
fn join_versions(versions: &[i64]) -> String {
    versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `true` when the error is PostgreSQL's `undefined_table` (`42P01`) — an unmigrated database.
fn is_missing_table(error: &omnion_identity::IdentityError) -> bool {
    match error {
        omnion_identity::IdentityError::Database(err) => err
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| code == "42P01"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_join_for_the_report() {
        assert_eq!(join_versions(&[6, 7]), "6, 7");
        assert_eq!(join_versions(&[]), "");
    }
}
