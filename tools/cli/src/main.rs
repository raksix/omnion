//! `omnion` — the command line of an Omnion installation.
//!
//! A server does not always have a browser (or a person in front of one), so the first run has a
//! terminal equivalent (docs/04-MONOREPO.md, docs/requests/REQ-050):
//!
//! ```text
//! omnion setup      create the owner account, organization, first site and theme
//! omnion doctor     check the environment: config, database, migrations, redis, storage
//! omnion migrate    apply pending database migrations
//! ```
//!
//! The connection comes from the same typed configuration the API reads (`OMNION_DATABASE_URL`
//! and friends, `crates/core/src/config.rs`), and the steps run through the very same code the
//! admin wizard uses (`crates/onboarding`) — one implementation, two front ends.
//!
//! Exit codes: `0` success, `1` a check or the environment failed, `2` the command line itself
//! was wrong (usage).

mod args;
mod doctor;
mod migrate;
mod output;
mod prompt;
mod setup;

use std::process::ExitCode;

use args::Command;

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    match Command::parse(&argv) {
        Ok(Command::Help) => {
            args::print_help();
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("omnion {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Doctor { json }) => doctor::run(json).await,
        Ok(Command::Migrate) => migrate::run().await,
        Ok(Command::Setup(options)) => setup::run(*options).await,
        Err(message) => {
            eprintln!("omnion: {message}");
            eprintln!();
            eprintln!("Run `omnion --help` for the command list.");
            ExitCode::from(2)
        }
    }
}
