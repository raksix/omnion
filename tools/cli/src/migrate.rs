//! `omnion migrate` — apply the migrations this binary embeds.
//!
//! Idempotent, like the API's own boot: SQLx takes an advisory lock and records applied versions,
//! so running it twice, or on two hosts at once, is safe.

use std::process::ExitCode;

use omnion_core::Db;
use omnion_core::config::Config;

/// Apply every pending migration and report what happened.
pub async fn run() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("omnion migrate: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn execute() -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    let db = Db::connect(&config.database)
        .await
        .map_err(|err| format!("could not connect to the database: {err}"))?;

    let before = db
        .migration_status()
        .await
        .map_err(|err| format!("could not read the migration state: {err}"))?;

    if before.is_up_to_date() {
        println!(
            "database is up to date ({} migrations applied).",
            before.applied.len()
        );
        return Ok(());
    }

    println!(
        "applying {} pending migration(s): {}",
        before.pending.len(),
        before
            .pending
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    db.migrate()
        .await
        .map_err(|err| format!("the migrations failed: {err}"))?;

    let after = db
        .migration_status()
        .await
        .map_err(|err| format!("could not read the migration state: {err}"))?;
    println!(
        "database is up to date ({} of {} migrations applied).",
        after.applied.len(),
        after.total()
    );
    Ok(())
}
