//! `omnion setup` — the first run of a server without a browser.
//!
//! Same steps, same rules and the same audit trail as the admin wizard (crates/onboarding):
//! owner account → organization → first site (+ domain) → theme → AI step (skipped; the AI Hub
//! connects providers) → done. Every value can be given as a flag, so a provisioning script can
//! run it end to end without a terminal:
//!
//! ```text
//! omnion setup --non-interactive --name 'Ada Lovelace' --email ada@example.com \
//!     --password-stdin --organization Acme --site Acme --domain acme.example.com
//! ```

use std::io::IsTerminal;
use std::process::ExitCode;

use omnion_core::Db;
use omnion_core::config::Config;
use omnion_identity::users;
use omnion_onboarding::themes::{self, BUNDLED_THEMES};
use omnion_onboarding::{self as onboarding, FirstOrganization, FirstOwner, FirstSite};

use crate::args::SetupOptions;
use crate::output;
use crate::prompt;

/// Why a setup run stopped.
enum SetupError {
    /// The command line (or the answers) cannot produce a working installation.
    Usage(String),
    /// The environment refused the work.
    Failed(String),
}

impl From<String> for SetupError {
    fn from(message: String) -> Self {
        Self::Usage(message)
    }
}

/// What a successful run created, for the closing report.
struct SetupReport {
    owner_email: String,
    organization_name: String,
    organization_slug: String,
    site_name: String,
    site_key: String,
    domain: Option<String>,
    theme: &'static str,
}

/// Run the first-run setup.
///
/// `json` and `quiet` come from the global flags rather than from `SetupOptions` so that every
/// command reports through the same envelope (REQ-131 acceptance 15). `--json` writes the report
/// to stdout as a document; `--quiet` leaves stdout empty and keeps stderr and the exit code,
/// which is what a provisioning script that only cares whether the owner now exists wants.
pub async fn run(options: SetupOptions, json: bool, quiet: bool) -> ExitCode {
    match execute(options).await {
        Ok(report) => {
            if json {
                crate::envelope::print(&crate::envelope::success(
                    "setup",
                    serde_json::json!({
                        "owner_email": report.owner_email,
                        "organization_name": report.organization_name,
                        "organization_slug": report.organization_slug,
                        "site_name": report.site_name,
                        "site_key": report.site_key,
                        "domain": report.domain,
                        "theme": report.theme,
                    }),
                    &[],
                ));
            } else if !quiet {
                print_report(&report, crate::output::Sink::new(json, quiet));
            }
            ExitCode::SUCCESS
        }
        Err(SetupError::Usage(message)) => {
            let failure =
                crate::envelope::Failure::new(crate::envelope::ErrorCode::Usage, message.clone());
            if json {
                crate::envelope::print_failure("setup", &failure, &[]);
            }
            eprintln!("omnion setup: {message}");
            ExitCode::from(2)
        }
        Err(SetupError::Failed(message)) => {
            let failure = crate::envelope::Failure::new(
                crate::envelope::ErrorCode::DatabaseUnreachable,
                message.clone(),
            );
            if json {
                crate::envelope::print_failure("setup", &failure, &[]);
            }
            eprintln!("omnion setup: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(options: SetupOptions) -> Result<SetupReport, SetupError> {
    let config = Config::from_env().map_err(|err| SetupError::Failed(err.to_string()))?;
    let db = Db::connect(&config.database)
        .await
        .map_err(|err| SetupError::Failed(format!("could not connect to the database: {err}")))?;

    if !options.skip_migrations {
        db.migrate()
            .await
            .map_err(|err| SetupError::Failed(format!("the migrations failed: {err}")))?;
    }

    if users::has_any(db.pool())
        .await
        .map_err(|err| SetupError::Failed(err.to_string()))?
    {
        return Err(SetupError::Usage(
            "this installation already has accounts — sign in to the admin panel instead (the \
             wizard lives at /setup)"
                .to_owned(),
        ));
    }

    let interactive = std::io::stdin().is_terminal() && !options.non_interactive;

    println!("Omnion first-run setup");
    println!(
        "  database  {}",
        output::describe_database_url(&config.database.url)
    );
    if interactive {
        println!("  (press Enter to accept a default; --help lists every flag)");
    }
    println!();

    let display_name = prompt::required(
        "Owner display name",
        "--name",
        options.display_name,
        interactive,
    )?;
    let email = prompt::required("Owner email", "--email", options.email, interactive)?;
    let password = prompt::resolve_password(
        options.password.as_deref(),
        options.password_stdin,
        options.non_interactive,
    )?;

    let organization_name = prompt::required(
        "Organization name",
        "--organization",
        options.organization,
        interactive,
    )?;
    let organization_slug = prompt::optional(
        "Organization slug",
        options.slug,
        &derive(&organization_name),
        interactive,
    )?;

    let site_name = prompt::required("Site name", "--site", options.site, interactive)?;
    let site_key = prompt::optional(
        "Site key",
        options.site_key,
        &derive(&site_name),
        interactive,
    )?;
    let domain = prompt::optional("Primary domain", options.domain, "", interactive)?;

    let theme_key = prompt::optional("Theme", options.theme, themes::default_theme(), interactive)?;
    let theme = themes::find(&theme_key).ok_or_else(|| {
        SetupError::Usage(format!(
            "unknown theme {theme_key:?} — this installation bundles: {}",
            BUNDLED_THEMES
                .iter()
                .map(|theme| theme.key)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;

    if interactive {
        println!();
        println!(
            "  Note: AI provider connections arrive with the AI Hub phase; the setup records the"
        );
        println!("  AI step as skipped and you can connect a provider later.");
        println!();
    }

    let pool = db.pool();
    let owner = onboarding::create_owner(
        pool,
        FirstOwner {
            display_name,
            email,
            password,
        },
    )
    .await
    .map_err(failed)?;

    let organization = onboarding::create_organization(
        pool,
        owner.id,
        FirstOrganization {
            name: organization_name,
            slug: Some(organization_slug),
        },
    )
    .await
    .map_err(failed)?;

    let (site, domain_row) = onboarding::create_site(
        pool,
        owner.id,
        FirstSite {
            name: site_name,
            key: Some(site_key),
            domain: Some(domain).filter(|host| !host.is_empty()),
        },
    )
    .await
    .map_err(failed)?;

    onboarding::choose_theme(pool, owner.id, theme.key)
        .await
        .map_err(failed)?;
    onboarding::decide_ai(pool, owner.id, None)
        .await
        .map_err(failed)?;
    onboarding::complete(pool, owner.id).await.map_err(failed)?;

    Ok(SetupReport {
        owner_email: owner.email,
        organization_name: organization.name,
        organization_slug: organization.slug,
        site_name: site.name,
        site_key: site.key,
        domain: domain_row.map(|domain| domain.host),
        theme: theme.key,
    })
}

/// Shape one onboarding failure for a terminal.
fn failed(error: onboarding::OnboardingError) -> SetupError {
    SetupError::Failed(error.to_string())
}

/// A slug derived from a display name, falling back to a generic one when the name has no
/// letters or digits a slug could use.
fn derive(name: &str) -> String {
    onboarding::derive_slug(name).unwrap_or_else(|_| "organization".to_owned())
}

/// The closing report: what exists now and where to sign in.
///
/// Routed through the sink like every other report, so `omnion setup --json` leaves stdout
/// holding one document. This function is only reached on the non-JSON path today, but a helper
/// that reached for stdout on its own would make that an accident of the call site rather than a
/// property of the code.
fn print_report(report: &SetupReport, sink: crate::output::Sink) {
    sink.print(format_args!("Setup complete."));
    output::pair(sink, "Owner", &report.owner_email);
    output::pair(
        sink,
        "Organization",
        &format!(
            "{} ({})",
            report.organization_name, report.organization_slug
        ),
    );
    let site = match report.domain.as_deref() {
        Some(domain) => format!("{} ({}) — {domain}", report.site_name, report.site_key),
        None => format!("{} ({})", report.site_name, report.site_key),
    };
    output::pair(sink, "Site", &site);
    output::pair(sink, "Theme", report.theme);
    sink.print(format_args!(""));
    sink.print(format_args!(
        "  Sign in to the admin panel with the owner account above (apps/admin, `/login`)."
    ));
    sink.print(format_args!(
        "  `omnion doctor` re-checks the environment; `omnion migrate` applies schema changes."
    ));
}
