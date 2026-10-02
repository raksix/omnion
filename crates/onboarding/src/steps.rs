//! The steps of the first run, in order: owner account → organization → first site → theme →
//! (optional) AI provider → done.
//!
//! Both front ends call these functions — the admin wizard over HTTP, `omnion setup` from a
//! terminal — so the rules live in one place:
//!
//! * the owner account can only be created while the installation has no accounts at all;
//! * every later step requires the account that owns the first run (the recorded owner, or the
//!   oldest active account when the installation bootstrapped from the environment);
//! * each step is audited with the account that asked for it;
//! * the flow closes only when the steps it needs are actually done.

use omnion_audit::NewAuditEntry;
use omnion_identity::organizations::{self, NewOrganization, Organization};
use omnion_identity::sites::{self, NewSite, Site, SiteChanges, SiteDomain};
use omnion_identity::users::{self, NewUser, User};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{OnboardingError, Result};
use crate::state::{self, OnboardingState, Status};
use crate::themes;

/// Longest slug a derived name produces (the schema's own bound).
const MAX_SLUG_LENGTH: usize = 63;

/// The first account of a fresh installation.
#[derive(Debug, Clone)]
pub struct FirstOwner {
    /// Display name.
    pub display_name: String,
    /// Email address (normalized by the identity store).
    pub email: String,
    /// Plaintext password, hashed before it is stored.
    pub password: String,
}

/// The first organization.
#[derive(Debug, Clone)]
pub struct FirstOrganization {
    /// Display name.
    pub name: String,
    /// Hand-written slug; `None` derives one from the name.
    pub slug: Option<String>,
}

/// The first site of the onboarding organization.
#[derive(Debug, Clone)]
pub struct FirstSite {
    /// Display name.
    pub name: String,
    /// Hand-written key; `None` derives one from the name.
    pub key: Option<String>,
    /// Host that should address the site, when the operator already knows it.
    pub domain: Option<String>,
}

/// Create the owner account of a fresh installation.
///
/// Refuses on an installation that already has accounts ([`OnboardingError::AlreadyInstalled`]),
/// so this can never quietly add a second "first" owner. The account receives the Owner role
/// through the same seeding path the API uses at boot (docs/07-IAM.md §20), and the account is
/// platform-level (`organization_id = null`): an Owner runs the platform, not one tenant.
pub async fn create_owner(pool: &PgPool, new: FirstOwner) -> Result<User> {
    if users::has_any(pool).await? {
        return Err(OnboardingError::AlreadyInstalled);
    }

    let user = users::create_user(
        pool,
        NewUser {
            email: new.email,
            password: new.password,
            display_name: new.display_name,
            organization_id: None,
        },
    )
    .await?;

    // The catalogue, the base roles and the Owner binding are what make the account usable.
    let report = omnion_permissions::seed::ensure(pool).await?;
    if let Some(bound) = report.owner_bound {
        record(
            pool,
            NewAuditEntry::system("iam.bootstrap.owner_bound")
                .target("user", bound.to_string())
                .metadata(json!({ "role": "owner", "scope": "global", "source": "onboarding" })),
        )
        .await?;
    }

    state::ensure(pool).await?;
    state::set_owner(pool, user.id).await?;

    record(
        pool,
        NewAuditEntry::by_user(user.id, "onboarding.owner_created")
            .target("user", user.id.to_string())
            .metadata(json!({ "email": user.email })),
    )
    .await?;

    Ok(user)
}

/// Create the first organization of the first run.
pub async fn create_organization(
    pool: &PgPool,
    actor: Uuid,
    new: FirstOrganization,
) -> Result<Organization> {
    let state = ensure_actor(pool, actor).await?;

    let existing = organizations::list_organizations(pool).await?;
    if !existing.is_empty() {
        return Err(OnboardingError::StepAlreadyDone("organization"));
    }
    if state.organization_id.is_some() {
        return Err(OnboardingError::StepAlreadyDone("organization"));
    }

    let slug = match new.slug {
        Some(slug) if !slug.trim().is_empty() => slug,
        _ => derive_slug(&new.name)?,
    };

    let organization = organizations::create_organization(
        pool,
        NewOrganization {
            name: new.name,
            slug,
        },
    )
    .await?;

    state::set_organization(pool, organization.id).await?;

    record(
        pool,
        NewAuditEntry::by_user(actor, "onboarding.organization_created")
            .target("organization", organization.id.to_string())
            .metadata(json!({ "name": organization.name, "slug": organization.slug }))
            .organization(organization.id),
    )
    .await?;

    Ok(organization)
}

/// Create the first site of the onboarding organization, with an optional primary domain.
pub async fn create_site(
    pool: &PgPool,
    actor: Uuid,
    new: FirstSite,
) -> Result<(Site, Option<SiteDomain>)> {
    let state = ensure_actor(pool, actor).await?;

    let organization_id = match state.organization_id {
        Some(id) => id,
        None => organizations::list_organizations(pool)
            .await?
            .first()
            .map(|organization| organization.id)
            .ok_or(OnboardingError::Incomplete {
                missing: String::from("organization"),
            })?,
    };

    if sites::count_for_organization(pool, organization_id).await? > 0 {
        return Err(OnboardingError::StepAlreadyDone("site"));
    }

    let key = match new.key {
        Some(key) if !key.trim().is_empty() => key,
        _ => derive_slug(&new.name)?,
    };

    let site = sites::create_site(
        pool,
        NewSite {
            organization_id,
            key,
            name: new.name,
            theme: None,
        },
    )
    .await?;

    let domain = match new
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(host) => Some(sites::add_domain(pool, site.id, host, true).await?),
        None => None,
    };

    state::set_organization(pool, organization_id).await?;
    state::set_site(pool, site.id).await?;

    record(
        pool,
        NewAuditEntry::by_user(actor, "onboarding.site_created")
            .target("site", site.id.to_string())
            .metadata(json!({
                "key": site.key,
                "name": site.name,
                "domain": domain.as_ref().map(|domain| domain.host.clone()),
            }))
            .organization(organization_id),
    )
    .await?;

    Ok((site, domain))
}

/// Choose the theme the first site renders with.
pub async fn choose_theme(pool: &PgPool, actor: Uuid, theme: &str) -> Result<Site> {
    let state = ensure_actor(pool, actor).await?;

    let bundled = themes::find(theme)
        .ok_or_else(|| OnboardingError::UnknownTheme(theme.trim().to_owned()))?;
    let site_id = resolve_site(pool, &state).await?;
    let Some(site_id) = site_id else {
        return Err(OnboardingError::SiteMissing);
    };

    let site = sites::update_site(
        pool,
        site_id,
        &SiteChanges {
            theme: Some(bundled.key.to_owned()),
            ..SiteChanges::default()
        },
    )
    .await?;
    state::set_theme_decided(pool).await?;

    record(
        pool,
        NewAuditEntry::by_user(actor, "onboarding.theme_chosen")
            .target("site", site.id.to_string())
            .metadata(json!({ "theme": site.theme }))
            .organization(site.organization_id),
    )
    .await?;

    Ok(site)
}

/// Decide the optional AI step.
///
/// Today the only decision a first run can make is to skip it: provider connections arrive with
/// the AI Hub (docs/06-AI-HUB.md, REQ-001), and pretending otherwise would leave a provider row
/// nothing could use.
pub async fn decide_ai(pool: &PgPool, actor: Uuid, provider: Option<&str>) -> Result<()> {
    let _state = ensure_actor(pool, actor).await?;

    if provider.is_some() {
        return Err(OnboardingError::AiHubPending);
    }

    state::set_ai_skipped(pool).await?;
    record(
        pool,
        NewAuditEntry::by_user(actor, "onboarding.ai_step_skipped")
            .metadata(json!({ "decision": "skipped", "reason": "ai-hub-phase" })),
    )
    .await?;

    Ok(())
}

/// Close the first run.
///
/// The steps the flow depends on must exist: an owner account, an organization and a site. The
/// theme and AI steps are decisions with defaults, so they never block the close.
pub async fn complete(pool: &PgPool, actor: Uuid) -> Result<Status> {
    let state = ensure_actor(pool, actor).await?;

    let mut missing = Vec::new();
    if state.owner_user_id.is_none() && !users::has_any(pool).await? {
        missing.push("owner account");
    }
    if state.organization_id.is_none() && organizations::list_organizations(pool).await?.is_empty()
    {
        missing.push("organization");
    }
    if resolve_site(pool, &state).await?.is_none() {
        missing.push("site");
    }
    if !missing.is_empty() {
        return Err(OnboardingError::Incomplete {
            missing: missing.join(", "),
        });
    }

    state::mark_completed(pool).await?;
    record(pool, NewAuditEntry::by_user(actor, "onboarding.completed")).await?;

    state::status(pool).await
}

/// The account that owns the first run: the recorded owner, else the oldest active account.
pub async fn owner_account(pool: &PgPool) -> Result<Option<Uuid>> {
    let state = state::load(pool).await?;
    if let Some(owner) = state.as_ref().and_then(|state| state.owner_user_id) {
        return Ok(Some(owner));
    }
    Ok(users::earliest_active(pool).await?)
}

/// Resolve the first site: the recorded one, else the oldest site of the installation.
async fn resolve_site(pool: &PgPool, state: &OnboardingState) -> Result<Option<Uuid>> {
    if let Some(site_id) = state.site_id {
        return Ok(Some(site_id));
    }
    Ok(sites::list_sites(pool).await?.first().map(|site| site.id))
}

/// Prove that `actor` may work on the first run and that it is still open.
async fn ensure_actor(pool: &PgPool, actor: Uuid) -> Result<OnboardingState> {
    let state = state::ensure(pool).await?;
    if state.is_completed() {
        return Err(OnboardingError::AlreadyComplete);
    }
    match owner_account(pool).await? {
        Some(owner) if owner == actor => Ok(state),
        _ => Err(OnboardingError::NotOnboardingOwner),
    }
}

/// Derive a slug (organization slug, site key) from a display name.
///
/// Everything that is not an ASCII letter or digit becomes a dash, runs collapse and the edges
/// are trimmed; the result satisfies both schema checks (lowercase, dashes, 1-63 characters).
pub fn derive_slug(value: &str) -> Result<String> {
    let mut slug = String::new();
    let mut pending_dash = false;

    for character in value.trim().chars() {
        let lower = character.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(lower);
        } else {
            pending_dash = true;
        }
    }

    slug.truncate(MAX_SLUG_LENGTH);
    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        return Err(OnboardingError::Invalid(format!(
            "{value:?} does not contain a letter or digit to build a slug from"
        )));
    }
    Ok(slug)
}

/// Write one audit row.
async fn record(pool: &PgPool, entry: NewAuditEntry) -> Result<()> {
    omnion_audit::record(pool, entry).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_come_from_names() {
        assert_eq!(
            derive_slug("  Acme Corporation ").expect("valid"),
            "acme-corporation"
        );
        assert_eq!(derive_slug("Main Site").expect("valid"), "main-site");
        assert_eq!(derive_slug("Acme & Co.").expect("valid"), "acme-co");
        // Letters a slug cannot carry are separators: `Ünïcode & Dãshes!` keeps its ASCII
        // skeleton and every run of everything else collapses into one dash.
        assert_eq!(
            derive_slug("Ünïcode & Dãshes!").expect("valid"),
            "n-code-d-shes"
        );
        assert_eq!(derive_slug("İşletme").expect("valid"), "letme");
        assert_eq!(
            derive_slug(&"a".repeat(200)).expect("valid").len(),
            MAX_SLUG_LENGTH
        );
    }

    #[test]
    fn a_name_without_letters_or_digits_is_refused() {
        for bad in ["", "   ", "---", "üöç"] {
            assert!(derive_slug(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_truncated_slug_never_ends_in_a_dash() {
        let name = format!("{}-tail", "a".repeat(MAX_SLUG_LENGTH - 1));
        let slug = derive_slug(&name).expect("valid");
        assert!(slug.len() <= MAX_SLUG_LENGTH);
        assert!(!slug.ends_with('-'), "slug: {slug}");
    }
}
