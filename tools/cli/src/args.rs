//! Command line parsing for `omnion` (std only — the CLI ships no argument-parsing dependency).
//!
//! Supported shapes: `--flag value`, `--flag=value`, switches (`--yes`), `-h`/`--help` and
//! `-V`/`--version`. Anything else is a usage error, reported with exit code `2`.

/// One parsed invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `omnion`, `omnion help`, `omnion --help`.
    Help,
    /// `omnion version`, `omnion --version`.
    Version,
    /// `omnion doctor [--json]`.
    Doctor {
        /// Print the checks as JSON instead of a table.
        json: bool,
    },
    /// `omnion migrate [up|status|plan|verify-down]`.
    Migrate(Box<MigrateOptions>),
    /// `omnion setup …`.
    Setup(Box<SetupOptions>),
    /// `omnion secret …` — the loopback credential helper (REQ-125, slice 3).
    Secret(Box<SecretOptions>),
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
        let mut cursor = Cursor::new(argv);
        let Some((command, inline)) = cursor.next() else {
            return Ok(Self::Help);
        };

        match command {
            "help" | "--help" | "-h" => {
                no_value(command, inline)?;
                Ok(Self::Help)
            }
            "version" | "--version" | "-V" => {
                no_value(command, inline)?;
                Ok(Self::Version)
            }
            "doctor" => {
                let mut json = false;
                while let Some((flag, inline)) = cursor.next() {
                    match flag {
                        "--json" => {
                            no_value(flag, inline)?;
                            json = true;
                        }
                        other => {
                            return Err(format!("unknown option {other:?} for `omnion doctor`"));
                        }
                    }
                }
                Ok(Self::Doctor { json })
            }
            "migrate" => {
                let options = parse_migrate(&mut cursor)?;
                Ok(Self::Migrate(Box::new(options)))
            }
            "setup" => {
                let options = parse_setup(&mut cursor)?;
                Ok(Self::Setup(Box::new(options)))
            }
            "secret" => {
                let options = parse_secret(&mut cursor)?;
                Ok(Self::Secret(Box::new(options)))
            }
            other => Err(format!("unknown command {other:?}")),
        }
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
        // The first bare word is the action; after that it is part of the child command, so it
        // is collected and handed back untouched.
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
    fn doctor_takes_the_json_switch() {
        assert_eq!(
            Command::parse(&argv("doctor")).expect("parses"),
            Command::Doctor { json: false }
        );
        assert_eq!(
            Command::parse(&argv("doctor --json")).expect("parses"),
            Command::Doctor { json: true }
        );
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
}
