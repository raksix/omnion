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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// The check passed.
    Ok,
    /// The check failed; a doctor run with one of these exits `1`.
    Fail,
}

impl Status {
    fn marker(self) -> &'static str {
        match self {
            Self::Ok => "ok",
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
pub async fn run(json: bool) -> ExitCode {
    let checks = collect().await;
    let failures = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();

    if json {
        print_json(&checks);
    } else {
        print_table(&checks);
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
fn print_table(checks: &[Check]) {
    println!("omnion doctor");
    for check in checks {
        output::check(check.status.marker(), check.name, &check.detail);
        if let Some(hint) = &check.hint {
            output::hint(hint);
        }
    }

    let failures = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();
    println!();
    if failures == 0 {
        println!("{} checks, all passed.", checks.len());
    } else {
        println!("{} checks, {failures} failed.", checks.len());
    }
}

/// Render the checks as JSON (for pipelines that prefer a machine-readable answer).
fn print_json(checks: &[Check]) {
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
    let document = serde_json::json!({
        "checks": entries,
        "failures": failures,
        "ok": failures == 0,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("a JSON document serializes")
    );
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
