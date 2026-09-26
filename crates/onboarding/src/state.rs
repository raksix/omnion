//! The first-run record: what the installation has decided so far.
//!
//! One installation has one first run, so the state lives in a singleton row
//! (`onboarding_state.id = 1`, enforced by the schema) and each step keeps its own timestamp.
//! The wizard (and `omnion setup`) derive the step it is on from this row — a page refresh or a
//! new terminal therefore never loses the way forward.
//!
//! Two installations exist in practice and both are served here:
//!
//! * a fresh one, where the wizard creates the owner account itself;
//! * one whose first account came from the environment bootstrap (`OMNION_ADMIN_EMAIL`), which
//!   has no owner recorded — the oldest active account acts as the owner of the first run.

use omnion_identity::{organizations, sites, users};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{OnboardingError, Result};

/// Column list of every `OnboardingState` query.
const STATE_COLUMNS: &str = "id, owner_user_id, organization_id, site_id, theme_at, ai_step, \
                             completed_at, created_at, updated_at";

/// The AI step is still open.
pub const AI_STEP_PENDING: &str = "pending";
/// The AI step was deliberately left for later (the AI Hub phase connects providers).
pub const AI_STEP_SKIPPED: &str = "skipped";

/// The single first-run row of this installation.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OnboardingState {
    /// Always `1`.
    pub id: i16,
    /// Account created by the first-run wizard, when the wizard created one.
    pub owner_user_id: Option<Uuid>,
    /// Organization the first run created.
    pub organization_id: Option<Uuid>,
    /// First site of that organization.
    pub site_id: Option<Uuid>,
    /// When a theme was chosen for the first site.
    pub theme_at: Option<OffsetDateTime>,
    /// `pending` or `skipped` (the schema constrains the vocabulary).
    pub ai_step: String,
    /// When the first run was closed.
    pub completed_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl OnboardingState {
    /// `true` once the first run is closed.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.completed_at.is_some()
    }

    /// `true` when the AI step has been decided (today: skipped).
    #[must_use]
    pub fn ai_decided(&self) -> bool {
        self.ai_step != AI_STEP_PENDING
    }
}

/// Read the singleton row, if this installation has one.
pub async fn load(pool: &PgPool) -> Result<Option<OnboardingState>> {
    let sql = format!("select {STATE_COLUMNS} from onboarding_state where id = 1");
    let state = sqlx::query_as::<_, OnboardingState>(&sql)
        .fetch_optional(pool)
        .await?;
    Ok(state)
}

/// Read the singleton row, creating it when the installation does not have one yet.
///
/// Two callers racing here both succeed: the insert is `on conflict do nothing`, so the loser
/// reads the row the winner wrote.
pub async fn ensure(pool: &PgPool) -> Result<OnboardingState> {
    sqlx::query("insert into onboarding_state (id) values (1) on conflict (id) do nothing")
        .execute(pool)
        .await?;
    load(pool).await?.ok_or(OnboardingError::StateMissing)
}

/// Record the owner account of the first run.
pub async fn set_owner(pool: &PgPool, user_id: Uuid) -> Result<OnboardingState> {
    update(
        pool,
        "update onboarding_state set owner_user_id = $1, updated_at = now() \
         where id = 1 returning ",
        Some(user_id),
    )
    .await
}

/// Record the organization of the first run.
pub async fn set_organization(pool: &PgPool, organization_id: Uuid) -> Result<OnboardingState> {
    update(
        pool,
        "update onboarding_state set organization_id = $1, updated_at = now() \
         where id = 1 returning ",
        Some(organization_id),
    )
    .await
}

/// Record the first site of the first run.
pub async fn set_site(pool: &PgPool, site_id: Uuid) -> Result<OnboardingState> {
    update(
        pool,
        "update onboarding_state set site_id = $1, updated_at = now() \
         where id = 1 returning ",
        Some(site_id),
    )
    .await
}

/// Record that a theme was chosen (the value itself lives on the site row).
pub async fn set_theme_decided(pool: &PgPool) -> Result<OnboardingState> {
    update(
        pool,
        "update onboarding_state set theme_at = now(), updated_at = now() \
         where id = 1 returning ",
        None,
    )
    .await
}

/// Record that the AI step was left for the AI Hub phase.
pub async fn set_ai_skipped(pool: &PgPool) -> Result<OnboardingState> {
    let sql = format!(
        "update onboarding_state set ai_step = '{AI_STEP_SKIPPED}', updated_at = now() \
         where id = 1 returning {STATE_COLUMNS}"
    );
    let state = sqlx::query_as::<_, OnboardingState>(&sql)
        .fetch_one(pool)
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => OnboardingError::StateMissing,
            other => OnboardingError::Database(other),
        })?;
    Ok(state)
}

/// Close the first run.
pub async fn mark_completed(pool: &PgPool) -> Result<OnboardingState> {
    let sql = format!(
        "update onboarding_state set completed_at = coalesce(completed_at, now()), \
         updated_at = now() where id = 1 returning {STATE_COLUMNS}"
    );
    let state = sqlx::query_as::<_, OnboardingState>(&sql)
        .fetch_one(pool)
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => OnboardingError::StateMissing,
            other => OnboardingError::Database(other),
        })?;
    Ok(state)
}

/// Run one single-value update and read the row back.
async fn update(pool: &PgPool, sql_prefix: &str, value: Option<Uuid>) -> Result<OnboardingState> {
    let sql = format!("{sql_prefix}{STATE_COLUMNS}");
    let query = sqlx::query_as::<_, OnboardingState>(&sql);
    let query = match value {
        Some(value) => query.bind(value),
        None => query,
    };
    query.fetch_one(pool).await.map_err(|err| match err {
        sqlx::Error::RowNotFound => OnboardingError::StateMissing,
        other => OnboardingError::Database(other),
    })
}

/// What the first run has finished so far — the wizard's progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Steps {
    /// An account exists.
    pub owner: bool,
    /// An organization exists.
    pub organization: bool,
    /// A site exists.
    pub site: bool,
    /// A theme was chosen for the first site.
    pub theme: bool,
    /// The AI step was decided (connected later, or skipped).
    pub ai: bool,
}

/// Names of the first-run screens, in order.
impl Steps {
    /// Index of the first open step, or `None` when everything is done.
    #[must_use]
    pub fn first_open(&self) -> Option<usize> {
        let order = [
            self.owner,
            self.organization,
            self.site,
            self.theme,
            self.ai,
        ];
        order.iter().position(|done| !done)
    }
}

/// Names of the steps that are still open.
#[must_use]
pub fn open_steps(steps: &Steps) -> Vec<&'static str> {
    let mut open = Vec::new();
    if !steps.owner {
        open.push("owner account");
    }
    if !steps.organization {
        open.push("organization");
    }
    if !steps.site {
        open.push("site");
    }
    if !steps.theme {
        open.push("theme");
    }
    if !steps.ai {
        open.push("ai provider");
    }
    open
}

/// What the installation would answer a fresh visitor asking "am I installed?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Name of the organization the first run created, when there is one.
    pub organization_name: Option<String>,
    /// Name of the first site, when there is one.
    pub site_name: Option<String>,
    /// Theme the first site renders with, when there is one.
    pub site_theme: Option<String>,
}

/// The complete first-run picture: progress, the resume point and the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `true` while the installation has no accounts at all.
    pub needs_setup: bool,
    /// `true` when accounts exist but the first run was never closed.
    pub in_progress: bool,
    /// `true` once the first run is closed.
    pub completed: bool,
    /// Per-step progress.
    pub steps: Steps,
    /// Names the wizard may show on its "done" screen.
    pub summary: Summary,
    /// Getting-started items (settings, content, a domain).
    pub checklist: Vec<crate::checklist::ChecklistItem>,
}

/// Read the first-run picture of this installation.
///
/// Every value is derived, never guessed: an installation that predates the
/// `onboarding_state` table (accounts, organizations and sites created directly through the API)
/// reports the steps it has actually finished, so the wizard can pick up from there.
pub async fn status(pool: &PgPool) -> Result<Status> {
    let state = load(pool).await?;
    let has_users = users::has_any(pool).await?;
    let organizations = organizations::list_organizations(pool).await?;
    let all_sites = sites::list_sites(pool).await?;

    let organization_id = state
        .as_ref()
        .and_then(|state| state.organization_id)
        .or_else(|| organizations.first().map(|organization| organization.id));
    let site_id = state
        .as_ref()
        .and_then(|state| state.site_id)
        .or_else(|| all_sites.first().map(|site| site.id));

    let steps = Steps {
        owner: has_users,
        organization: organization_id.is_some(),
        site: site_id.is_some(),
        theme: state.as_ref().is_some_and(|state| state.theme_at.is_some()),
        ai: state.as_ref().is_some_and(OnboardingState::ai_decided),
    };
    let completed = state.as_ref().is_some_and(OnboardingState::is_completed);

    let site = site_id.and_then(|id| all_sites.iter().find(|site| site.id == id));
    let summary = Summary {
        organization_name: organization_id
            .and_then(|id| {
                organizations
                    .iter()
                    .find(|organization| organization.id == id)
            })
            .map(|organization| organization.name.clone()),
        site_name: site.map(|site| site.name.clone()),
        site_theme: site.map(|site| site.theme.clone()),
    };

    let checklist = crate::checklist::build(pool, site_id, organization_id.is_some()).await?;

    Ok(Status {
        needs_setup: !has_users,
        in_progress: has_users && !completed,
        completed,
        steps,
        summary,
        checklist,
    })
}
