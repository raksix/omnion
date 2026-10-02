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
//! `--json` is accepted by every command and writes the documented envelope (see
//! [`envelope`]) to stdout; humans get a table and progress on stderr.
//!
//! Exit codes: `0` success, `1` a check or the environment failed, `2` the command line itself
//! was wrong (usage).

mod args;
mod doctor;
mod envelope;
mod migrate;
mod output;
mod prompt;
mod secret;
mod setup;

use std::process::ExitCode;

use args::Command;
use envelope::{ErrorCode, Failure};

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let (command, globals) = match Command::parse_with(&argv) {
        Ok(parsed) => parsed,
        // A usage error is reported in the same shape as everything else: the sentence for a
        // human, the envelope for a script. Before this, `--json` on a broken command line was
        // silently ignored and the script got an empty stdout to parse.
        Err(message) => {
            let failure = Failure::with_hint(
                ErrorCode::Usage,
                message.clone(),
                "run `omnion --help` for the command list",
            );
            if json_requested(&argv) {
                envelope::print_failure("omnion", &failure, &[]);
            }
            eprintln!("omnion: {message}");
            eprintln!();
            eprintln!("Run `omnion --help` for the command list.");
            return ExitCode::from(2);
        }
    };

    // One flag, one source: `--json` was lifted out of the argument list before the command was
    // parsed, so `globals.json` is the whole answer and there is no per-command copy to disagree
    // with it.
    let json = globals.json;

    // The same rule the other commands follow: under `--json` the human report moves to stderr so
    // stdout stays a single document. `version` is where this is easiest to get wrong — printing
    // `omnion 0.1.0` and then the envelope produces two documents on one stream, and a consumer's
    // parser reports the error against the CLI rather than against the line that caused it.
    let sink = output::Sink::new(json, globals.quiet);

    match command {
        Command::Help => {
            if !json {
                args::print_help();
            }
            if json {
                envelope::print(&envelope::success(
                    "help",
                    serde_json::json!({ "commands": args::command_names() }),
                    &[],
                ));
            }
            ExitCode::SUCCESS
        }
        Command::Version => {
            sink.print(format_args!("omnion {}", env!("CARGO_PKG_VERSION")));
            if json {
                envelope::print(&envelope::success(
                    "version",
                    serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
                    &[],
                ));
            }
            ExitCode::SUCCESS
        }
        Command::Doctor => doctor::run(json, globals.quiet).await,
        Command::Migrate(options) => migrate::run(&options, json, globals.quiet).await,
        Command::Setup(options) => setup::run(*options, json, globals.quiet).await,
        Command::Secret(options) => {
            match secret::run(*options, api_url(), json, globals.quiet).await {
                Ok(code) => code,
                Err(message) => {
                    // The failure envelope goes to stdout under `--json` for the same reason the
                    // successes do: a script branching on `error.code` must not have to
                    // distinguish "the command failed" from "the command failed in a way that
                    // skipped the contract".
                    if json {
                        envelope::print_failure(
                            "secret",
                            &Failure::with_hint(
                                ErrorCode::Refused,
                                message.clone(),
                                "run `omnion secret redeem --help` for the options",
                            ),
                            &[],
                        );
                    }
                    eprintln!("omnion: {message}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Whether `--json` appears anywhere in the arguments, before they have been parsed.
///
/// Used only on the path where parsing has already failed: the caller asked for the envelope and
/// is entitled to it even when the rest of the command line did not make sense. Re-scanning the
/// raw arguments is the only way to know that, and it cannot misfire — a `--json` meant as some
/// other flag's value would already have been reported as a usage error by the parser.
fn json_requested(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "--json")
}

/// The address the helper redeems against, from the same configuration the API reads.
///
/// Only the authority is used, and the helper refuses anything that is not loopback — but
/// reading it from the typed config means an operator who has already set
/// `OMNION_API_URL` does not have to repeat themselves on every pipeline.
fn api_url() -> String {
    std::env::var("OMNION_API_URL").unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
}
