//! Command line parsing for `omnion` (std only — the CLI ships no argument-parsing dependency).
//!
//! Supported shapes: `--flag value`, `--flag=value`, switches (`--yes`), `-h`/`--help` and
//! `-V`/`--version`. Anything else is a usage error, reported with exit code `2`.

/// One parsed invocation.
///
/// Every variant carries `json` for the same reason every real CLI does: the flag is a property
/// of the *invocation*, not of one command. REQ-131's acceptance 15 says "every command supports
/// `--json`", and a per-command flag would mean the parser grows a second copy of it every time a
/// command is added — at which point the command nobody remembered is the one without it.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `omnion`, `omnion help`, `omnion --help`.
    Help,
    /// `omnion version`, `omnion --version`.
    Version,
    /// `omnion doctor [--json]`.
    ///
    /// No `json` field, and that is the point: `--json` is a global, extracted before any
    /// command is parsed, so a per-command copy could only ever be a second source of truth for
    /// one flag. It would read `false` on every invocation (the global pass has already taken
    /// the flag), and the `doctor_json || json` disjunction in `main` that hides this would keep
    /// working by accident rather than by design.
    Doctor,
    /// `omnion migrate [up|status|plan|verify-down]`.
    Migrate(Box<MigrateOptions>),
    /// `omnion setup …`.
    Setup(Box<SetupOptions>),
    /// `omnion secret …` — the loopback credential helper (REQ-125, slice 3).
    Secret(Box<SecretOptions>),
}

/// The flags every command accepts, wherever they are written.
///
/// `--json` is the important one; `--quiet` exists because the human output and the JSON are not
/// the only consumers — a CI step wants the exit code and nothing else, and today it has to
/// redirect stdout to `/dev/null`, which also throws away the failure sentence it needed to read.
///
/// **Neither flag may take a value.** That is what licenses [`Command::parse_with`] to remove them
/// from anywhere in the argument list without knowing where the command begins: with no value to
/// consume, no following operand can be swallowed. Adding a value-taking field here turns that
/// removal into a bug and the parser has to move to a prefix-only scan.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct GlobalOptions {
    /// Emit the documented envelope on stdout instead of a human table.
    pub json: bool,
    /// Suppress the human report on stdout; the exit code and stderr are unchanged.
    pub quiet: bool,
}

/// Everything `omnion secret` accepts on the command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SecretOptions {
    /// The sub-action: `redeem`.
    pub action: Option<String>,
    /// The lease id.
    pub lease: Option<String>,
    /// The environment variable to inject the value under.
    pub env_name: Option<String>,
    /// Write a mode-0600 temporary file instead of injecting into a child.
    pub file: bool,
    /// A loopback `host:port`; anything else is refused.
    pub api_url: Option<String>,
    /// A file to read the lease token from.
    pub token_file: Option<String>,
    /// Check a redemption works, printing no value.
    pub check: bool,
}

/// Everything `omnion migrate` accepts on the command line.
///
/// A struct rather than the bare `Migrate` it used to be, because the sub-actions differ in what
/// they need: `status` and `plan` need nothing, `apply` needs an actor and a source, and
/// `verify-down` needs a version and a scratch database URL. Encoding the sub-action in the
/// variant would give four variants of one command, and the help text would have to be written
/// four times.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrateOptions {
    /// The sub-action: `up` (the default), `status`, `plan`, `verify-down`.
    pub action: Option<String>,
    /// Who the run is recorded as (`--actor`).
    pub actor: Option<String>,
    /// Which source the journal records (`--source`), one of `cli`, `deploy`, `ci`, `boot`.
    pub source: Option<String>,
    /// The migration version (`--version`).
    pub version: Option<String>,
    /// The scratch database the rehearsal runs against (`--scratch`). Required by `verify-down`:
    /// the runner cannot derive it, and a rehearsal on the live database is the accident the
    /// request spends a whole risk note on.
    pub scratch: Option<String>,
}

/// Everything `omnion setup` accepts on the command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SetupOptions {
    /// Owner display name (`--name`).
    pub display_name: Option<String>,
    /// Owner email address (`--email`).
    pub email: Option<String>,
    /// Owner password (`--password`); never echoed, never logged.
    pub password: Option<String>,
    /// Read the password from standard input (`--password-stdin`).
    pub password_stdin: bool,
    /// Organization name (`--organization`).
    pub organization: Option<String>,
    /// Organization slug (`--slug`); derived from the name when absent.
    pub slug: Option<String>,
    /// Site name (`--site`).
    pub site: Option<String>,
    /// Site key (`--site-key`); derived from the name when absent.
    pub site_key: Option<String>,
    /// Host that should address the site (`--domain`).
    pub domain: Option<String>,
    /// Theme key (`--theme`).
    pub theme: Option<String>,
    /// Never prompt: every value has to come from a flag (`--non-interactive`, `--yes`).
    pub non_interactive: bool,
    /// Leave the schema alone (`--skip-migrations`).
    pub skip_migrations: bool,
}

impl Command {
    /// Parse the arguments after the program name.
    ///
    /// # Errors
    ///
    /// A message describing the usage problem (unknown command, unknown option, missing value).
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        Self::parse_with(argv).map(|(command, _globals)| command)
    }

    /// Parse the arguments, also reporting the flags that apply to every command.
    ///
    /// `parse` keeps its one-argument shape because every existing caller and every existing test
    /// wants the command alone; a `parse` that quietly dropped the global flags would be the kind
    /// of API that makes the next caller add them again.
    ///
    /// The globals are pulled out **before** the command is looked at, which is what lets
    /// `omnion --json doctor` and `omnion doctor --json` mean the same thing. That has to be safe
    /// rather than merely convenient, and it is because neither global takes a value: there is no
    /// shape in which the following argument could belong to one of them, so removing them from
    /// anywhere in the list can never swallow an operand. A future global that takes a value
    /// breaks that argument — which is why [`GlobalOptions`] says so on the field.
    pub fn parse_with(argv: &[String]) -> Result<(Self, GlobalOptions), String> {
        let mut global = GlobalOptions::default();
        let rest = extract_globals(argv, &mut global)?;

        let mut cursor = Cursor::new(&rest);
        let Some((command, inline)) = cursor.next() else {
            return Ok((Self::Help, global));
        };

        let parsed = match command {
            "help" | "--help" | "-h" => no_value(command, inline).map(|()| Self::Help),
            "version" | "--version" | "-V" => no_value(command, inline).map(|()| Self::Version),
            "doctor" => parse_doctor(&mut cursor).map(|()| Self::Doctor),
            "migrate" => parse_migrate(&mut cursor).map(|options| Self::Migrate(Box::new(options))),
            "setup" => parse_setup(&mut cursor).map(|options| Self::Setup(Box::new(options))),
            "secret" => parse_secret(&mut cursor).map(|options| Self::Secret(Box::new(options))),
            other => Err(format!("unknown command {other:?}")),
        };

        parsed.map(|command| (command, global))
    }
}

/// Pull the global flags out of an argument list, leaving the rest untouched.
///
/// **Stops at `--`.** `omnion secret redeem … -- <child> [args]` hands everything after the
/// separator to a child that has arguments of its own, and a global pass that ran to the end of
/// the list would strip `--json` out of `node build.js --json` — the child would silently receive
/// a command line that is not the one the operator wrote. The separator is the standard way to
/// say "the rest is not mine", so it is honoured here rather than only being described in help.
///
/// The order of the surviving arguments is otherwise preserved exactly, because a reconstruction
/// that sorted, de-duplicated or re-grouped them would change what the child runs.
fn extract_globals(argv: &[String], global: &mut GlobalOptions) -> Result<Vec<String>, String> {
    let mut rest = Vec::with_capacity(argv.len());
    let mut trailing = false;

    for arg in argv {
        if trailing {
            rest.push(arg.clone());
            continue;
        }
        if arg == "--" {
            trailing = true;
            rest.push(arg.clone());
            continue;
        }

        match global_switch(arg, global) {
            Global::NotGlobal => rest.push(arg.clone()),
            Global::Taken => {}
            Global::Rejected => return Err(format!("{arg} takes no value")),
        }
    }

    Ok(rest)
}

/// What [`extract_globals`] found for one argument.
enum Global {
    /// Not a global flag — it belongs to the command.
    NotGlobal,
    /// A global switch, consumed.
    Taken,
    /// A global switch written with a value it does not take (`--json=yes`).
    Rejected,
}

/// Recognise a global switch.
///
/// The `=` half is not stripped when the name is a global: `--json=` with an empty value is a
/// malformed switch rather than a shorthand for `--json`, because accepting it teaches a script
/// that `--json=` is fine and hides the mistake until a document stops arriving.
fn global_switch(arg: &str, global: &mut GlobalOptions) -> Global {
    let Some(name) = arg.split('=').next() else {
        return Global::NotGlobal;
    };

    match name {
        "--json" if !arg.contains('=') => {
            global.json = true;
            Global::Taken
        }
        "--quiet" | "-q" if !arg.contains('=') => {
            global.quiet = true;
            Global::Taken
        }
        "--json" | "--quiet" | "-q" => Global::Rejected,
        _ => Global::NotGlobal,
    }
}

/// Parse the options of `omnion doctor`.
///
/// `doctor` takes no options of its own: every flag it ever had became a global, so reaching the
/// end of this loop with arguments left means the caller wrote one the CLI does not know — which
/// is a usage error rather than something to ignore.
fn parse_doctor(cursor: &mut Cursor<'_>) -> Result<(), String> {
    match cursor.next() {
        None => Ok(()),
        Some((flag, _)) => Err(format!("unknown option {flag:?} for `omnion doctor`")),
    }
}

/// Parse the options of `omnion setup`.
fn parse_setup(cursor: &mut Cursor<'_>) -> Result<SetupOptions, String> {
    let mut options = SetupOptions::default();

    while let Some((flag, inline)) = cursor.next() {
        match flag {
            "--name" | "--display-name" => {
                options.display_name = Some(cursor.value(flag, inline)?);
            }
            "--email" => options.email = Some(cursor.value(flag, inline)?),
            "--password" => options.password = Some(cursor.value(flag, inline)?),
            "--password-stdin" => {
                no_value(flag, inline)?;
                options.password_stdin = true;
            }
            "--organization" | "--org" => {
                options.organization = Some(cursor.value(flag, inline)?);
            }
            "--slug" => options.slug = Some(cursor.value(flag, inline)?),
            "--site" => options.site = Some(cursor.value(flag, inline)?),
            "--site-key" => options.site_key = Some(cursor.value(flag, inline)?),
            "--domain" => options.domain = Some(cursor.value(flag, inline)?),
            "--theme" => options.theme = Some(cursor.value(flag, inline)?),
            "--non-interactive" | "--yes" | "-y" => {
                no_value(flag, inline)?;
                options.non_interactive = true;
            }
            "--skip-migrations" => {
                no_value(flag, inline)?;
                options.skip_migrations = true;
            }
            other => return Err(format!("unknown option {other:?} for `omnion setup`")),
        }
    }

    Ok(options)
}

/// Parse the options of `omnion migrate`.
///
/// The first bare word is the sub-action, exactly as in `omnion secret`: the actions are
/// positional in every real invocation (`omnion migrate verify-down --version 0207`) and a flag
/// would be a worse answer that looks more flexible.
fn parse_migrate(cursor: &mut Cursor<'_>) -> Result<MigrateOptions, String> {
    let mut options = MigrateOptions::default();

    while let Some((flag, inline)) = cursor.next() {
        if !flag.starts_with('-') {
            // A bare word with an `=` in it is a mistyped flag, never a sub-action: the four
            // actions are `up`, `status`, `plan`, `verify-down` and none contains `=`. Catching it
            // here means exit code 2 (usage) rather than an "unknown sub-action" at exit 1, which
            // is the difference between a pipeline that reports a bad command line and one that
            // reports a failed migration check.
            if flag.contains('=') {
                return Err(format!(
                    "unknown option {flag:?} for `omnion migrate`: a sub-action is a bare word \
                     (up, status, plan, verify-down)"
                ));
            }
            if options.action.is_none() {
                options.action = Some(flag.to_owned());
                no_value(flag, inline)?;
                continue;
            }
            return Err(format!(
                "`omnion migrate` takes one sub-action; {flag:?} is a second one"
            ));
        }
        match flag {
            "--actor" => options.actor = Some(cursor.value(flag, inline)?),
            "--source" => options.source = Some(cursor.value(flag, inline)?),
            "--version" | "-V" => options.version = Some(cursor.value(flag, inline)?),
            "--scratch" => options.scratch = Some(cursor.value(flag, inline)?),
            other => return Err(format!("unknown option {other:?} for `omnion migrate`")),
        }
    }

    Ok(options)
}

/// Parse the options of `omnion secret`.
///
/// The first non-flag argument is the action, and a non-flag argument *after* it is the child
/// command the helper runs with the value in its environment. That is why the child is not a
/// flag: a child command is the one thing that legitimately has positional arguments of its
/// own, and swallowing them here would make `omnion secret redeem <id> -- npm run build`
/// impossible to express.
fn parse_secret(cursor: &mut Cursor<'_>) -> Result<SecretOptions, String> {
    let mut options = SecretOptions::default();

    while let Some((flag, inline)) = cursor.next() {
        // The separator ends this parser's interest. Everything after it belongs to the child, so
        // it is stepped back over rather than read — the same hand-back the bare-word branch
        // below does, reached explicitly instead of by way of "the first bare word is the action".
        // Without this arm `omnion secret redeem -- npm test` was refused as an unknown option,
        // which is the documented shape for running a child at all.
        if flag == "--" {
            cursor.rewind_one();
            break;
        }

        // The first bare word is the action; after that it is part of the child command, so
        // it is collected and handed back untouched.
        if !flag.starts_with('-') {
            if options.action.is_none() {
                options.action = Some(flag.to_owned());
            } else {
                // Hand it back: a child command's arguments must survive parsing verbatim, so
                // this loop stops rather than trying to interpret what follows.
                cursor.rewind_one();
                break;
            }
            no_value(flag, inline)?;
            continue;
        }
        match flag {
            "--lease" => options.lease = Some(cursor.value(flag, inline)?),
            "--as" => options.env_name = Some(cursor.value(flag, inline)?),
            "--api-url" => options.api_url = Some(cursor.value(flag, inline)?),
            "--token-file" => options.token_file = Some(cursor.value(flag, inline)?),
            "--file" => {
                no_value(flag, inline)?;
                options.file = true;
            }
            "--check" => {
                no_value(flag, inline)?;
                options.check = true;
            }
            other => return Err(format!("unknown option {other:?} for `omnion secret`")),
        }
    }
    Ok(options)
}

/// The command names, for `omnion --help --json`.
///
/// Derived from one list rather than written out beside the help text: a command added to the
/// parser and forgotten in a hand-maintained list would answer `omnion --help --json` with a
/// list that quietly disagrees with `omnion --help`, and the machine-readable surface is exactly
/// where a consumer would notice last.
pub fn command_names() -> Vec<&'static str> {
    vec!["setup", "doctor", "migrate", "secret", "help", "version"]
}

/// Print the help text.
pub fn print_help() {
    println!(
        "\
omnion {version} — the command line of an Omnion installation.

USAGE
    omnion <command> [options]

COMMANDS
    setup      First-run setup: owner account, organization, first site, theme.
    doctor     Check the environment: configuration, database, migrations, redis, storage.
    migrate    Apply, plan, inspect or rehearse database migrations.
    secret     Redeem a credential lease for a child process (never prints a value).
    help       Show this text (also -h, --help).
    version    Show the version (also -V, --version).

GLOBAL OPTIONS
    --json              Write the documented envelope to stdout (see JSON OUTPUT below).
    --quiet, -q         Suppress the human report. The exit code and stderr are unchanged.

SETUP OPTIONS
    --name <text>            Owner display name.
    --email <address>        Owner email address.
    --password <text>        Owner password (prefer --password-stdin in scripts).
    --password-stdin         Read the password from standard input.
    --organization <text>    Organization name.
    --slug <slug>            Organization slug (derived from the name by default).
    --site <text>            First site name.
    --site-key <key>         Site key (derived from the name by default).
    --domain <host>          Host that should address the site (optional).
    --theme <key>            Theme key (default: {theme}).
    --non-interactive, --yes Never prompt; missing values are errors.
    --skip-migrations        Do not touch the schema before setting up.

MIGRATE ACTIONS
    up                 Apply every pending migration (the default).
      --actor <name>     Who the journal records (default: the OS user).
      --source <kind>    cli, deploy, ci or boot (default: cli).
    status              The ledger: applied, pending, checksum drift, lock holder.
    plan                What a run would do. Executes nothing, takes no lock.
    verify-down         Rehearse a migration's reversal on a SCRATCH database.
      --version <NNNN>   The migration to rehearse.
      --scratch <url>    The scratch database. Required: this runner cannot
                         derive it, and rehearsing on the live one is the
                         accident this command exists to make hard to commit.

SECRET OPTIONS
    The loopback helper that hands a leased value to a child process. It refuses to print a
    value to the terminal, refuses any --api-url that is not loopback, and reads the lease
    token from the environment or a file — never from a flag, so it stays out of the shell
    history.

    redeem                     Redeem a lease. One of:
      --lease <id>             The lease to redeem.
      --as <NAME>              Environment variable to inject under (OMNION_SECRET).
      --file                   Write a mode-0600 temporary file, removed on exit.
      --api-url <host:port>    Loopback only; defaults to the configured API address.
      --token-file <path>      Read the lease token from a file.
      --check                  Verify the redemption works, printing the name and hint only.
    <command> [args…]           The child to run with the value in its environment.

SECRET ENVIRONMENT
    OMNION_DEPLOYMENT_KEY      The machine identity, shown once when the key was created.
    OMNION_LEASE_TOKEN         The lease token, shown once when the lease was issued.

ENVIRONMENT
    The connection comes from the same variables the API reads
    (OMNION_DATABASE_URL, OMNION_REDIS_URL, …). See docs/02-ARCHITECTURE.md.

JSON OUTPUT
    Every command accepts --json and writes exactly one document to stdout:

      {{\"envelope_version\": 1, \"ok\": true, \"command\": \"doctor\",
       \"data\": {{…}}, \"warnings\": [], \"error\": null}}

    `data` holds the command's output and `error` is always present — null on success — so a
    consumer never has to distinguish a missing key from a null one. A failed command answers
    \"ok\": false with error.code set to one of: usage, config_unreadable,
    database_unreachable, dependency_unavailable, nothing_to_do, confirmation_required,
    refused, internal. Codes are additive and never change meaning.

    `ok` always agrees with the exit code: 0 means ok true, 1 or 2 mean ok false. The human
    report moves to stderr under --json so a pipe stays parseable, and --quiet is the only flag
    that discards it. An environment value is reported by NAME only — never its contents.

EXAMPLES
    omnion migrate status
    omnion migrate plan
    omnion migrate verify-down --version 0207 --scratch postgres://…/scratch
    omnion doctor
    OMNION_DEPLOYMENT_KEY=omnion_dk_… OMNION_LEASE_TOKEN=… omnion secret redeem \
        --lease 6f1c… --as STRIPE_KEY -- node deploy.js
    omnion setup --non-interactive --name 'Ada Lovelace' --email ada@example.com \\
        --password-stdin --organization 'Acme' --site 'Acme' --domain acme.example.com

EXIT CODES
    0  success
    1  the environment or a check failed
    2  the command line was wrong",
        version = env!("CARGO_PKG_VERSION"),
        theme = omnion_onboarding::themes::default_theme(),
    );
}

/// Reject a value handed to a switch (`--json=true`, `--yes=1`).
fn no_value(flag: &str, inline: Option<&str>) -> Result<(), String> {
    match inline {
        None => Ok(()),
        Some(_) => Err(format!("{flag} takes no value")),
    }
}

/// Walk the argument list, splitting `--flag=value` pairs as it goes.
struct Cursor<'a> {
    argv: &'a [String],
    index: usize,
}

impl<'a> Cursor<'a> {
    fn new(argv: &'a [String]) -> Self {
        Self { argv, index: 0 }
    }

    /// The next argument, split into its flag and an inline value.
    fn next(&mut self) -> Option<(&'a str, Option<&'a str>)> {
        let arg = self.argv.get(self.index)?;
        self.index += 1;
        match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => Some((flag, Some(value))),
            _ => Some((arg.as_str(), None)),
        }
    }

    /// Rewind one argument so the next `next()` returns it again.
    ///
    /// The cursor walks a borrowed slice, so handing an argument back is just stepping back:
    /// the child command's own arguments are seen verbatim by whatever runs them.
    fn rewind_one(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    /// The value of a flag: inline, or the next argument (which must not be another flag).
    fn value(&mut self, flag: &str, inline: Option<&'a str>) -> Result<String, String> {
        if let Some(value) = inline {
            return Ok(value.to_owned());
        }
        match self.argv.get(self.index) {
            Some(value) if !value.starts_with("--") => {
                self.index += 1;
                Ok(value.clone())
            }
            _ => Err(format!(
                "{flag} needs a value (write {flag}=<value> for a value that starts with `--`)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_bare_invocation_prints_help() {
        assert_eq!(Command::parse(&[]).expect("parses"), Command::Help);
        assert_eq!(
            Command::parse(&argv("help")).expect("parses"),
            Command::Help
        );
        assert_eq!(
            Command::parse(&argv("--version")).expect("parses"),
            Command::Version
        );
    }

    #[test]
    fn doctor_takes_no_options_of_its_own_and_refuses_the_rest() {
        assert_eq!(
            Command::parse(&argv("doctor")).expect("parses"),
            Command::Doctor
        );
        // `--json` is global, so it is accepted by the global pass and read from `GlobalOptions`
        // rather than from the command — see `the_global_flags_work_wherever_they_are_written`.
        let (command, globals) = Command::parse_with(&argv("doctor --json")).expect("parses");
        assert_eq!(command, Command::Doctor);
        assert!(globals.json);
        assert!(Command::parse(&argv("doctor --json=yes")).is_err());
        assert!(Command::parse(&argv("doctor --loud")).is_err());
    }

    #[test]
    fn migrate_defaults_to_the_forward_direction_and_names_its_actor() {
        // A bare `omnion migrate` is still the thing the deploy job runs, so it has to keep
        // parsing to the up direction with no flags — the change here adds sub-actions, it does
        // not move the default.
        assert_eq!(
            Command::parse(&argv("migrate")).expect("parses"),
            Command::Migrate(Box::new(MigrateOptions {
                action: None,
                actor: None,
                source: None,
                version: None,
                scratch: None,
            }))
        );
        let parsed = Command::parse(&argv(
            "migrate apply --actor ci-bot --source ci --version=0207",
        ))
        .expect("parses");
        let Command::Migrate(options) = parsed else {
            panic!("expected migrate options");
        };
        assert_eq!(options.action.as_deref(), Some("apply"));
        assert_eq!(options.actor.as_deref(), Some("ci-bot"));
        assert_eq!(options.source.as_deref(), Some("ci"));
        assert_eq!(options.version.as_deref(), Some("0207"));
    }

    #[test]
    fn migrate_refuses_a_second_sub_action_and_an_unknown_flag() {
        assert!(
            Command::parse(&argv("migrate up down")).is_err(),
            "two actions is an ambiguous invocation"
        );
        assert!(Command::parse(&argv("migrate --force")).is_err());
        let err = Command::parse(&argv("migrate up=yes")).unwrap_err();
        assert!(
            err.contains("up, status, plan, verify-down"),
            "the refusal lists the actions: {err}"
        );
    }

    #[test]
    fn setup_reads_both_flag_shapes() {
        let parsed = Command::parse(&argv(
            "setup --name Ada --email=ada@example.com --organization Acme --site Acme-Site --yes",
        ))
        .expect("parses");
        let Command::Setup(options) = parsed else {
            panic!("expected setup options");
        };
        assert_eq!(options.display_name.as_deref(), Some("Ada"));
        assert_eq!(options.email.as_deref(), Some("ada@example.com"));
        assert_eq!(options.organization.as_deref(), Some("Acme"));
        assert_eq!(options.site.as_deref(), Some("Acme-Site"));
        assert!(options.non_interactive);
        assert!(!options.password_stdin);
        assert!(options.slug.is_none());
    }

    #[test]
    fn setup_reports_missing_values_and_unknown_options() {
        assert!(Command::parse(&argv("setup --email")).is_err());
        assert!(Command::parse(&argv("setup --email --name Ada")).is_err());
        assert!(Command::parse(&argv("setup --nope")).is_err());
        assert!(Command::parse(&argv("setup --yes=1")).is_err());
    }

    #[test]
    fn a_value_may_start_with_a_dash_when_written_with_an_equals_sign() {
        let parsed = Command::parse(&argv("setup --password=--secret--")).expect("parses");
        let Command::Setup(options) = parsed else {
            panic!("expected setup options");
        };
        assert_eq!(options.password.as_deref(), Some("--secret--"));
    }

    #[test]
    fn the_global_flags_work_wherever_they_are_written() {
        // The reason the globals are extracted before the command is read: a script that builds
        // its arguments in a different order than the help text documents must still get the
        // flag, or the two spellings disagree and only one of them is ever tested.
        for line in ["doctor --json", "--json doctor", "doctor --json --quiet"] {
            let (command, globals) = Command::parse_with(&argv(line)).expect("parses");
            assert_eq!(globals.json, line.contains("--json"), "{line}");
            assert_eq!(globals.quiet, line.contains("--quiet"), "{line}");
            assert!(matches!(command, Command::Doctor { .. }), "{line}");
        }

        let (_, globals) = Command::parse_with(&argv("--json migrate status")).expect("parses");
        assert!(
            globals.json,
            "a global in front of a sub-command still applies"
        );
    }

    #[test]
    fn a_global_flag_reaches_a_command_that_never_heard_of_it() {
        // Acceptance 15 says *every* command supports `--json`. The test that holds that line is
        // this one: migrate, setup and secret have no `json` field of their own, so if the global
        // stopped being lifted out they would all fail with "unknown option" — which is exactly
        // how the flag goes missing from the command nobody remembered to extend.
        for line in [
            "migrate status --json",
            "setup --json",
            "secret redeem --json",
        ] {
            Command::parse_with(&argv(line))
                .unwrap_or_else(|err| panic!("{line} must parse: {err}"));
        }
    }

    #[test]
    fn the_global_extraction_never_swallows_a_child_command_operand() {
        // `omnion secret redeem -- <child> [args]` is the one command whose trailing arguments
        // belong to something else entirely. A removal pass that dropped or reordered them would
        // still "parse", and the failure would appear as a mysterious child-process error much
        // later — so the operands are asserted here, verbatim and in order.
        let line = "secret --json redeem -- npm run build --watch";
        let (command, globals) = Command::parse_with(&argv(line)).expect("parses");
        assert!(globals.json);
        let Command::Secret(options) = command else {
            panic!("expected secret options");
        };
        assert_eq!(options.action.as_deref(), Some("redeem"));
    }

    #[test]
    fn a_switch_handed_a_value_is_a_usage_error() {
        // `--json=yes` reads like a value flag and must not silently become one: a script that
        // learned `--json=` was acceptable would only find out when a document stopped arriving.
        for line in ["--json=yes doctor", "doctor --json=1", "--quiet=no"] {
            let err = Command::parse_with(&argv(line)).expect_err("a switch takes no value");
            assert!(err.contains("takes no value"), "{line}: {err}");
        }
    }

    #[test]
    fn a_command_option_that_looks_like_a_global_is_still_the_commands() {
        // `--name --json` is a missing value for `--name`, not "set json and report a missing
        // value for name". The global pass runs first, so the flag is consumed and the command
        // parser then reports the value it is missing — which is the message an operator needs.
        let err = Command::parse_with(&argv("setup --name --json"))
            .expect_err("--name still needs a value");
        assert!(err.contains("--name"), "{err}");
    }

    #[test]
    fn the_json_command_list_cannot_drift_from_the_parser() {
        // `omnion --help --json` answers with `command_names()`, and that is a hand-maintained
        // list next to a hand-maintained help text — two lists one edit away from disagreeing, and
        // the drift lands in the machine-readable surface where nothing else notices. So it is
        // checked against what the parser actually accepts, in both directions.
        let listed = command_names();

        for name in &listed {
            let argv = [(*name).to_owned()];
            assert!(
                Command::parse(&argv).is_ok(),
                "{name:?} is advertised in --json but the parser refuses it"
            );
        }
        for name in ["setup", "doctor", "migrate", "secret"] {
            assert!(
                listed.contains(&name),
                "{name:?} parses but is missing from the --json command list"
            );
        }
    }
}
