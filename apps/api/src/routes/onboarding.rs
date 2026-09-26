//! `/api/v1/onboarding` — the first-run surface of an installation (REQ-050, phase P10).
//!
//! A fresh Omnion has no account to sign in with, so this router is the one place under
//! `/api/v1` whose mutations carry **no permission guard**: there is nothing to check against
//! yet. What protects it instead is the shape of the flow itself (crates/onboarding):
//!
//! * `GET /onboarding` is open — a client has to be able to ask "is this installation set up?"
//!   before it can sign in, and the answer holds no secret (step booleans and static labels);
//! * `POST /onboarding/owner` only works while the installation has **no accounts at all**, so
//!   it cannot add a second first owner to a running platform;
//! * every later step requires the session of the account that owns the first run (the recorded
//!   owner, or the oldest active account when the installation bootstrapped from the
//!   environment), and refuses once the first run is closed.
//!
//! Every mutation answers with the fresh [`StatusBody`], so the wizard re-renders from the
//! server's answer instead of tracking state in the browser — that is what makes it resumable
//! after a refresh.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{SET_COOKIE, USER_AGENT};
use axum::response::{IntoResponse, Response};
use omnion_identity::sessions::{self, SESSION_TTL_SECONDS};
use omnion_onboarding::themes::{BUNDLED_THEMES, BundledTheme};
use omnion_onboarding::{self as onboarding, state as onboarding_state};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::cookies;
use crate::dto::UserBody;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// Per-step progress of the first run.
#[derive(Debug, Serialize)]
pub struct StepsBody {
    /// An account exists.
    pub owner: bool,
    /// An organization exists.
    pub organization: bool,
    /// A site exists.
    pub site: bool,
    /// A theme was chosen for the first site.
    pub theme: bool,
    /// The AI step was decided (today: skipped; the AI Hub connects providers later).
    pub ai: bool,
}

impl From<onboarding_state::Steps> for StepsBody {
    fn from(steps: onboarding_state::Steps) -> Self {
        Self {
            owner: steps.owner,
            organization: steps.organization,
            site: steps.site,
            theme: steps.theme,
            ai: steps.ai,
        }
    }
}

/// What the first run created — the names the done screen may show.
#[derive(Debug, Serialize)]
pub struct SummaryBody {
    /// Organization name, when there is one.
    pub organization_name: Option<String>,
    /// First site name, when there is one.
    pub site_name: Option<String>,
    /// Theme of the first site, when there is one.
    pub site_theme: Option<String>,
}

impl From<onboarding_state::Summary> for SummaryBody {
    fn from(summary: onboarding_state::Summary) -> Self {
        Self {
            organization_name: summary.organization_name,
            site_name: summary.site_name,
            site_theme: summary.site_theme,
        }
    }
}

/// One getting-started item.
#[derive(Debug, Serialize)]
pub struct ChecklistItemBody {
    /// Stable key.
    pub key: &'static str,
    /// Short label.
    pub label: &'static str,
    /// One line explaining the item.
    pub description: &'static str,
    /// Panel screen that does the work.
    pub href: &'static str,
    /// `true` when the installation satisfies the item.
    pub done: bool,
}

impl From<onboarding::ChecklistItem> for ChecklistItemBody {
    fn from(item: onboarding::ChecklistItem) -> Self {
        Self {
            key: item.key,
            label: item.label,
            description: item.description,
            href: item.href,
            done: item.done,
        }
    }
}

/// One theme the wizard may offer.
#[derive(Debug, Serialize)]
pub struct ThemeBody {
    /// Manifest key.
    pub key: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One line describing the look.
    pub description: &'static str,
}

impl From<&BundledTheme> for ThemeBody {
    fn from(theme: &BundledTheme) -> Self {
        Self {
            key: theme.key,
            name: theme.name,
            description: theme.description,
        }
    }
}

/// Response of every `/api/v1/onboarding` route.
#[derive(Debug, Serialize)]
pub struct StatusBody {
    /// `true` while the installation has no accounts at all.
    pub needs_setup: bool,
    /// `true` when accounts exist but the first run was never closed.
    pub in_progress: bool,
    /// `true` once the first run is closed.
    pub completed: bool,
    /// Per-step progress.
    pub steps: StepsBody,
    /// Names of what the first run created.
    pub summary: SummaryBody,
    /// Getting-started items.
    pub checklist: Vec<ChecklistItemBody>,
    /// Themes this installation bundles.
    pub themes: Vec<ThemeBody>,
}

impl From<onboarding_state::Status> for StatusBody {
    fn from(status: onboarding_state::Status) -> Self {
        Self {
            needs_setup: status.needs_setup,
            in_progress: status.in_progress,
            completed: status.completed,
            steps: StepsBody::from(status.steps),
            summary: SummaryBody::from(status.summary),
            checklist: status
                .checklist
                .into_iter()
                .map(ChecklistItemBody::from)
                .collect(),
            themes: BUNDLED_THEMES.iter().map(ThemeBody::from).collect(),
        }
    }
}

/// Response of `POST /api/v1/onboarding/owner` — the account plus the cookie that signs it in.
#[derive(Debug, Serialize)]
pub struct OwnerResponse {
    /// The owner account that was created.
    pub user: UserBody,
    /// First-run state after the step.
    pub onboarding: StatusBody,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/onboarding/owner`.
#[derive(Debug, Deserialize)]
pub struct OwnerRequest {
    /// Display name of the owner.
    pub display_name: String,
    /// Email address.
    pub email: String,
    /// Plaintext password (hashed before it is stored).
    pub password: String,
}

/// `POST /api/v1/onboarding/organization`.
#[derive(Debug, Deserialize)]
pub struct OrganizationRequest {
    /// Display name.
    pub name: String,
    /// Hand-written slug; the server derives one from the name when absent.
    #[serde(default)]
    pub slug: Option<String>,
}

/// `POST /api/v1/onboarding/site`.
#[derive(Debug, Deserialize)]
pub struct SiteRequest {
    /// Display name.
    pub name: String,
    /// Hand-written key; the server derives one from the name when absent.
    #[serde(default)]
    pub key: Option<String>,
    /// Host that should address the site, when the operator already knows it.
    #[serde(default)]
    pub domain: Option<String>,
}

/// `POST /api/v1/onboarding/theme`.
#[derive(Debug, Deserialize)]
pub struct ThemeRequest {
    /// Key of one of the bundled themes (`GET /onboarding` lists them).
    pub theme: String,
}

/// `POST /api/v1/onboarding/ai-provider`.
///
/// The only decision a first run can make today is to skip: provider connections arrive with
/// the AI Hub (REQ-001). Sending a provider is refused with `ai_hub_pending` rather than
/// silently dropped.
#[derive(Debug, Deserialize)]
pub struct AiRequest {
    /// Provider identifier, when one was requested.
    #[serde(default)]
    pub provider: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/onboarding` — how far the first run of this installation has come.
pub async fn status(State(state): State<AppState>) -> Result<Json<StatusBody>, ApiError> {
    let status = onboarding_state::status(state.db().pool()).await?;
    Ok(Json(StatusBody::from(status)))
}

/// `POST /api/v1/onboarding/owner` — create the owner account and sign it in.
pub async fn create_owner(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: ClientAddress,
    Json(body): Json<OwnerRequest>,
) -> Result<Response, ApiError> {
    if body.display_name.trim().is_empty()
        || body.email.trim().is_empty()
        || body.password.is_empty()
    {
        return Err(ApiError::bad_request(
            "invalid_request",
            "display_name, email and password are required",
        ));
    }

    let user = onboarding::create_owner(
        state.db().pool(),
        onboarding::FirstOwner {
            display_name: body.display_name,
            email: body.email,
            password: body.password,
        },
    )
    .await?;

    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let ip_address = client.as_text();

    let (session, token) = sessions::create_session(
        state.db().pool(),
        user.id,
        user_agent.as_deref(),
        ip_address.as_deref(),
    )
    .await?;
    tracing::info!(user_id = %user.id, session_id = %session.id, "first-run owner signed in");

    let secure = !state.config().env.is_development();
    let cookie = cookies::session_cookie(&token, SESSION_TTL_SECONDS, secure);

    let onboarding = onboarding_state::status(state.db().pool()).await?;
    let mut response = (
        StatusCode::CREATED,
        Json(OwnerResponse {
            user: UserBody::from(&user),
            onboarding: StatusBody::from(onboarding),
        }),
    )
        .into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("session cookie is valid header text"),
    );
    Ok(response)
}

/// `POST /api/v1/onboarding/organization` — create the first organization.
pub async fn create_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<OrganizationRequest>,
) -> Result<Json<StatusBody>, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("invalid_request", "name is required"));
    }

    onboarding::create_organization(
        state.db().pool(),
        current.user.id,
        onboarding::FirstOrganization {
            name: body.name,
            slug: body.slug,
        },
    )
    .await?;

    let status = onboarding_state::status(state.db().pool()).await?;
    Ok(Json(StatusBody::from(status)))
}

/// `POST /api/v1/onboarding/site` — create the first site (and its domain, when given).
pub async fn create_site(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<SiteRequest>,
) -> Result<Json<StatusBody>, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("invalid_request", "name is required"));
    }

    onboarding::create_site(
        state.db().pool(),
        current.user.id,
        onboarding::FirstSite {
            name: body.name,
            key: body.key,
            domain: body.domain,
        },
    )
    .await?;

    let status = onboarding_state::status(state.db().pool()).await?;
    Ok(Json(StatusBody::from(status)))
}

/// `POST /api/v1/onboarding/theme` — choose the theme the first site renders with.
pub async fn choose_theme(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<ThemeRequest>,
) -> Result<Json<StatusBody>, ApiError> {
    onboarding::choose_theme(state.db().pool(), current.user.id, &body.theme).await?;

    let status = onboarding_state::status(state.db().pool()).await?;
    Ok(Json(StatusBody::from(status)))
}

/// `POST /api/v1/onboarding/ai-provider` — decide the optional AI step (skip it for now).
pub async fn decide_ai(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<AiRequest>,
) -> Result<Json<StatusBody>, ApiError> {
    onboarding::decide_ai(state.db().pool(), current.user.id, body.provider.as_deref()).await?;

    let status = onboarding_state::status(state.db().pool()).await?;
    Ok(Json(StatusBody::from(status)))
}

/// `POST /api/v1/onboarding/complete` — close the first run.
pub async fn complete(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<StatusBody>, ApiError> {
    let status = onboarding::complete(state.db().pool(), current.user.id).await?;
    Ok(Json(StatusBody::from(status)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_body_lists_the_bundled_themes() {
        let body = StatusBody::from(onboarding_state::Status {
            needs_setup: true,
            in_progress: false,
            completed: false,
            steps: onboarding_state::Steps {
                owner: false,
                organization: false,
                site: false,
                theme: false,
                ai: false,
            },
            summary: onboarding_state::Summary {
                organization_name: None,
                site_name: None,
                site_theme: None,
            },
            checklist: vec![onboarding::ChecklistItem {
                key: "account",
                label: "Create the owner account",
                description: "The first account owns the installation.",
                href: "/setup",
                done: false,
            }],
        });

        assert!(body.needs_setup);
        assert!(!body.completed);
        assert_eq!(body.checklist.len(), 1);
        assert_eq!(body.themes.len(), BUNDLED_THEMES.len());
        assert!(
            body.themes.iter().any(|theme| theme.key == "minimal"),
            "the bundled theme must be offered"
        );
    }

    #[test]
    fn an_ai_request_may_omit_the_provider() {
        let body: AiRequest = serde_json::from_str("{}").expect("an empty body skips the step");
        assert!(body.provider.is_none());

        let body: AiRequest =
            serde_json::from_str(r#"{"provider":null}"#).expect("null skips the step");
        assert!(body.provider.is_none());

        let body: AiRequest =
            serde_json::from_str(r#"{"provider":"openai"}"#).expect("a provider parses");
        assert_eq!(body.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn site_and_organization_requests_default_to_derived_identifiers() {
        let organization: OrganizationRequest =
            serde_json::from_str(r#"{"name":"Acme"}"#).expect("valid body");
        assert!(organization.slug.is_none());

        let site: SiteRequest =
            serde_json::from_str(r#"{"name":"Main site","domain":"acme.test"}"#)
                .expect("valid body");
        assert!(site.key.is_none());
        assert_eq!(site.domain.as_deref(), Some("acme.test"));
    }
}
