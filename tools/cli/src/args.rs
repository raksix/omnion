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
    /// `omnion migrate`.
    Migrate,
    /// `omnion setup …`.
    Setup(Box<SetupOptions>),
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
                if let Some((flag, inline)) = cursor.next() {
                    return Err(match inline {
                        Some(_) => format!("`omnion migrate` takes no options, got {flag:?}"),
                        None => format!("unknown option {flag:?} for `omnion migrate`"),
                    });
                }
                Ok(Self::Migrate)
            }
            "setup" => {
                let options = parse_setup(&mut cursor)?;
                Ok(Self::Setup(Box::new(options)))
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
    migrate    Apply pending database migrations.
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

ENVIRONMENT
    The connection comes from the same variables the API reads
    (OMNION_DATABASE_URL, OMNION_REDIS_URL, …). See docs/02-ARCHITECTURE.md.

EXAMPLES
    omnion doctor
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
    fn migrate_takes_nothing() {
        assert_eq!(
            Command::parse(&argv("migrate")).expect("parses"),
            Command::Migrate
        );
        assert!(Command::parse(&argv("migrate --force")).is_err());
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
