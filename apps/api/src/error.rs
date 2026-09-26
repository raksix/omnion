//! HTTP representation of core errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use omnion_ai_hub::AiHubError;
use omnion_audit::AuditError;
use omnion_automation::AutomationError;
use omnion_content::ContentError;
use omnion_core::CoreError;
use omnion_events::EventsError;
use omnion_identity::IdentityError;
use omnion_media::MediaError;
use omnion_onboarding::OnboardingError;
use omnion_permissions::PermissionsError;
use omnion_storage::StorageError;
use omnion_workflows::WorkflowError;
use serde::Serialize;

/// `true` when a database error means the dependency itself is unavailable (retryable).
fn dependency_unavailable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
    )
}

/// Error response shape used across `/api/v1`:
/// `{"error":{"code":"internal_error","message":"…"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    /// Build an error with an explicit status, code and message.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// `400` — the request body or its parameters are unusable.
    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    /// `401` — the caller is not signed in, or the session is gone.
    #[must_use]
    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    /// `403` — the caller is signed in but the action is not allowed.
    #[must_use]
    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    /// Map a core error onto the API surface.
    ///
    /// A dependency that did not answer becomes `503` (retryable); everything else is an
    /// internal `500` — the client can do nothing about it, but the operator can.
    #[must_use]
    pub fn from_core(error: CoreError) -> Self {
        match error {
            CoreError::Unavailable {
                dependency,
                message,
            } => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "dependency_unavailable",
                message: format!("{dependency}: {message}"),
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal_error",
                message: other.to_string(),
            },
        }
    }

    /// HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Stable, machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        Self::from_core(error)
    }
}

impl From<EventsError> for ApiError {
    /// Events and webhooks (docs/01-VISION.md §13, P12): an endpoint that was never connected is
    /// a `404`; a name already used inside the organization is a `409` the operator resolves in
    /// the panel; a definition the platform refuses to store is a `400`; and the store itself is
    /// the usual dependency/internal split.
    fn from(error: EventsError) -> Self {
        match error {
            EventsError::Store(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            EventsError::Store(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            EventsError::EndpointNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "webhook_endpoint_not_found",
                "no such webhook endpoint",
            ),
            EventsError::EndpointNameTaken(name) => Self::new(
                StatusCode::CONFLICT,
                "webhook_name_taken",
                format!("a webhook endpoint named \"{name}\" already exists in this organization"),
            ),
            EventsError::InvalidEndpoint(message) => {
                Self::bad_request("invalid_webhook_endpoint", message)
            }
            EventsError::InvalidEvent(message) => Self::bad_request("invalid_event", message),
            EventsError::Client(message) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
            }
        }
    }
}

impl From<IdentityError> for ApiError {
    fn from(error: IdentityError) -> Self {
        match error {
            // A pool that cannot hand out a connection is a retryable infrastructure
            // failure; everything else is an internal error the operator has to look at.
            IdentityError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            IdentityError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            // Tenancy: a missing row is a 404, a taken slug/key/host a 409, and everything the
            // store cannot accept (shape, status, host) is a bad request.
            IdentityError::OrganizationNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "organization_not_found",
                "no such organization",
            ),
            IdentityError::OrganizationSlugTaken => Self::new(
                StatusCode::CONFLICT,
                "organization_slug_taken",
                "an organization with this slug already exists",
            ),
            IdentityError::SiteNotFound => {
                Self::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
            }
            IdentityError::SiteKeyTaken => Self::new(
                StatusCode::CONFLICT,
                "site_key_taken",
                "a site with this key already exists in the organization",
            ),
            IdentityError::DomainTaken => Self::new(
                StatusCode::CONFLICT,
                "domain_taken",
                "this host already addresses a site",
            ),
            IdentityError::DomainNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "domain_not_found",
                "no such domain on this site",
            ),
            // Shape problems the store refuses (slug, key, status, host) are the caller's.
            IdentityError::InvalidOrganization(message)
            | IdentityError::InvalidSite(message)
            | IdentityError::InvalidHost(message) => Self::bad_request("invalid_request", message),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                other.to_string(),
            ),
        }
    }
}

impl From<AuditError> for ApiError {
    fn from(error: AuditError) -> Self {
        match error {
            AuditError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                other.to_string(),
            ),
        }
    }
}

impl From<ContentError> for ApiError {
    fn from(error: ContentError) -> Self {
        match error {
            ContentError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            ContentError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            // Content: a missing page or revision is a 404, a taken slug or a publish with
            // nothing to publish a 409, and everything the store cannot accept (shape, size,
            // unknown status) a bad request.
            ContentError::PageNotFound => {
                Self::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
            }
            ContentError::RevisionNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "revision_not_found",
                "no such revision on this page",
            ),
            ContentError::SlugTaken => Self::new(
                StatusCode::CONFLICT,
                "slug_taken",
                "this site already has a page with this slug",
            ),
            ContentError::NoDraftRevision => Self::new(
                StatusCode::CONFLICT,
                "no_draft_revision",
                "this page has no draft revision to publish",
            ),
            other => Self::bad_request("invalid_request", other.to_string()),
        }
    }
}

impl From<AutomationError> for ApiError {
    fn from(error: AutomationError) -> Self {
        match error {
            AutomationError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            AutomationError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            AutomationError::Audit(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            // The automation layer checks the event name, the conditions and the bindings; the
            // engine checks the trigger, the actions and the steps. Both are the caller's
            // problem, and both carry the stable code the request should be answered with.
            AutomationError::Workflows(err) => Self::bad_request(err.code(), err.to_string()),
            AutomationError::Invalid { code, message } => Self::bad_request(code, message),
        }
    }
}

impl From<PermissionsError> for ApiError {
    fn from(error: PermissionsError) -> Self {
        match error {
            PermissionsError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            PermissionsError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            PermissionsError::RoleNotFound => {
                Self::new(StatusCode::NOT_FOUND, "role_not_found", "no such role")
            }
            PermissionsError::RoleKeyTaken => Self::new(
                StatusCode::CONFLICT,
                "role_key_taken",
                "a role with this key already exists",
            ),
            PermissionsError::AlreadyBound => Self::new(
                StatusCode::CONFLICT,
                "already_bound",
                "the role is already assigned at this scope",
            ),
            PermissionsError::SystemRole => Self {
                status: StatusCode::FORBIDDEN,
                code: "system_role",
                message: "platform roles are managed by the platform".to_owned(),
            },
            // Everything else is a bad request: the caller handed in something the store
            // cannot accept (key shape, priority range, unknown permission, bad scope).
            other => Self::bad_request("invalid_request", other.to_string()),
        }
    }
}

impl From<MediaError> for ApiError {
    fn from(error: MediaError) -> Self {
        match error {
            MediaError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            MediaError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            MediaError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "media_not_found",
                "no such media in this library",
            ),
            MediaError::SizeTooLarge { limit } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("the file is larger than the {limit} byte limit"),
            ),
            MediaError::KeyTaken => Self::new(
                StatusCode::CONFLICT,
                "media_key_taken",
                "this object key is already in the media library",
            ),
            // Everything else is the caller's: an empty upload, an unusable file name, an
            // unusable content type or a key the store refuses.
            other => Self::bad_request("invalid_request", other.to_string()),
        }
    }
}

impl From<StorageError> for ApiError {
    /// The object store is a dependency like the database: a store that cannot answer is a
    /// retryable `503`, a missing object is a `404`, and a store that refuses a well-formed
    /// request is reported as `storage_error` so an operator sees the provider's own message.
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound { .. } => Self::new(
                StatusCode::NOT_FOUND,
                "object_not_found",
                "the stored object is missing from the object store",
            ),
            StorageError::Invalid(message) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_misconfigured",
                message,
            ),
            StorageError::Unavailable(message) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                format!("the object store is unreachable: {message}"),
            ),
            StorageError::Io(message) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                format!("the storage directory could not be used: {message}"),
            ),
            StorageError::Provider { status, message } => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_error",
                format!("the object store refused the request (status {status}): {message}"),
            ),
        }
    }
}

impl From<WorkflowError> for ApiError {
    /// Workflows: a definition the engine cannot accept is the caller's (its own stable code,
    /// e.g. `invalid_cron`), a store failure is internal, and an audit failure rides on the
    /// audit mapping.
    fn from(error: WorkflowError) -> Self {
        match error {
            WorkflowError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            WorkflowError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            WorkflowError::Audit(err) => err.into(),
            WorkflowError::Invalid { code, message } => Self::bad_request(code, message),
        }
    }
}

impl From<OnboardingError> for ApiError {
    /// Onboarding: the flow answers with its own vocabulary — an installation that is set up,
    /// a caller that may not finish the first run, a step that is already done, a step that is
    /// still open, a theme this installation does not bundle, or a provider request that has to
    /// wait for the AI Hub.
    fn from(error: OnboardingError) -> Self {
        match error {
            OnboardingError::AlreadyInstalled => Self::new(
                StatusCode::CONFLICT,
                "already_installed",
                "this installation already has accounts — sign in instead",
            ),
            OnboardingError::NotOnboardingOwner => Self::forbidden(
                "not_onboarding_owner",
                "only the account that owns the first run may finish it",
            ),
            OnboardingError::AlreadyComplete => Self::new(
                StatusCode::CONFLICT,
                "onboarding_complete",
                "the first-run setup is already complete",
            ),
            OnboardingError::StepAlreadyDone(step) => Self::new(
                StatusCode::CONFLICT,
                "step_already_done",
                format!("the {step} step is already done"),
            ),
            OnboardingError::Incomplete { missing } => Self::new(
                StatusCode::CONFLICT,
                "setup_incomplete",
                format!("the setup still needs: {missing}"),
            ),
            OnboardingError::SiteMissing => Self::new(
                StatusCode::CONFLICT,
                "site_missing",
                "create the first site before choosing a theme",
            ),
            OnboardingError::UnknownTheme(theme) => Self::bad_request(
                "unknown_theme",
                format!("this installation does not bundle a theme called {theme:?}"),
            ),
            OnboardingError::AiHubPending => Self::new(
                StatusCode::CONFLICT,
                "ai_hub_pending",
                "AI provider connections arrive with the AI Hub — skip this step for now",
            ),
            OnboardingError::Invalid(message) => Self::bad_request("invalid_request", message),
            OnboardingError::StateMissing => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the first-run record could not be read back",
            ),
            OnboardingError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            OnboardingError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            // Account shape problems are the caller's (the wizard validates, the server proves).
            OnboardingError::Identity(IdentityError::InvalidEmail(message))
            | OnboardingError::Identity(IdentityError::InvalidOrganization(message))
            | OnboardingError::Identity(IdentityError::InvalidSite(message)) => {
                Self::bad_request("invalid_request", message)
            }
            OnboardingError::Identity(IdentityError::WeakPassword { min }) => Self::bad_request(
                "invalid_request",
                format!("password must be at least {min} characters long"),
            ),
            OnboardingError::Identity(IdentityError::EmailTaken) => Self::new(
                StatusCode::CONFLICT,
                "email_taken",
                "this email address already has an account",
            ),
            OnboardingError::Identity(err) => err.into(),
            OnboardingError::Permissions(err) => err.into(),
            OnboardingError::Content(err) => err.into(),
            OnboardingError::Audit(err) => err.into(),
        }
    }
}

impl From<AiHubError> for ApiError {
    /// AI Hub (docs/06-AI-HUB.md): a provider the installation never connected — or a model the
    /// registry does not carry — is a `404`; a taken provider name, a switched-off provider and
    /// an installation without a default model are `409`s the operator resolves in the panel;
    /// everything the platform refuses before it sends anything is a `400`; and a provider that
    /// cannot be reached or refuses the request is a `502` — the gateway answering for its
    /// upstream, which is exactly what happened.
    fn from(error: AiHubError) -> Self {
        match error {
            AiHubError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            AiHubError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            AiHubError::ProviderNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "provider_not_found",
                "no such AI provider",
            ),
            AiHubError::ModelNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "model_not_found",
                "no such AI model in the registry",
            ),
            AiHubError::ProviderNameTaken(name) => Self::new(
                StatusCode::CONFLICT,
                "provider_name_taken",
                format!("an AI provider named \"{name}\" already exists"),
            ),
            AiHubError::NoDefaultModel => Self::new(
                StatusCode::CONFLICT,
                "no_default_model",
                "connect an AI provider and choose a default model first",
            ),
            AiHubError::ProviderDisabled(name) => Self::new(
                StatusCode::CONFLICT,
                "provider_disabled",
                format!("the AI provider \"{name}\" is switched off"),
            ),
            AiHubError::InvalidProvider(message) => Self::bad_request("invalid_provider", message),
            AiHubError::InvalidModel(message) => Self::bad_request("invalid_model", message),
            AiHubError::InvalidChatRequest(message) => {
                Self::bad_request("invalid_chat_request", message)
            }
            AiHubError::Transport(message) => Self::new(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                format!("the AI provider could not be reached: {message}"),
            ),
            AiHubError::Upstream { status, message } => Self::new(
                StatusCode::BAD_GATEWAY,
                "provider_error",
                format!("the AI provider answered with status {status}: {message}"),
            ),
            AiHubError::Stream(message) => Self::new(
                StatusCode::BAD_GATEWAY,
                "stream_failed",
                format!("the AI provider's answer stream failed: {message}"),
            ),
            AiHubError::Malformed(message) => Self::new(
                StatusCode::BAD_GATEWAY,
                "provider_malformed",
                format!("the AI provider answered with an unusable body: {message}"),
            ),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}
#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_dependency_maps_to_503() {
        let error = ApiError::from(CoreError::Unavailable {
            dependency: "redis",
            message: "PING timed out".to_owned(),
        });
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), "dependency_unavailable");
    }

    #[test]
    fn other_core_errors_map_to_500() {
        let error = ApiError::from(CoreError::Telemetry("boom".to_owned()));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.code(), "internal_error");
    }

    #[test]
    fn helpers_pick_the_right_status() {
        assert_eq!(
            ApiError::bad_request("invalid_request", "no").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::unauthorized("unauthenticated", "no").status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::forbidden("account_disabled", "no").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn exhausted_pool_maps_to_503_but_other_identity_errors_to_500() {
        let unavailable = ApiError::from(IdentityError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unavailable.code(), "dependency_unavailable");

        let internal = ApiError::from(IdentityError::EmailTaken);
        assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(internal.code(), "internal_error");
        assert_eq!(internal.message, "email address is already registered");
    }

    #[test]
    fn content_errors_map_onto_the_content_statuses() {
        assert_eq!(
            ApiError::from(ContentError::PageNotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(ContentError::RevisionNotFound).code(),
            "revision_not_found"
        );
        assert_eq!(
            ApiError::from(ContentError::SlugTaken).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::from(ContentError::NoDraftRevision).code(),
            "no_draft_revision"
        );

        let invalid = ApiError::from(ContentError::InvalidSlug("Nope!".to_owned()));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code(), "invalid_request");
    }

    #[test]
    fn tenancy_errors_map_onto_the_tenancy_statuses() {
        assert_eq!(
            ApiError::from(IdentityError::OrganizationNotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(IdentityError::OrganizationSlugTaken).code(),
            "organization_slug_taken"
        );
        assert_eq!(
            ApiError::from(IdentityError::SiteKeyTaken).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::from(IdentityError::DomainTaken).code(),
            "domain_taken"
        );
        assert_eq!(
            ApiError::from(IdentityError::DomainNotFound).status(),
            StatusCode::NOT_FOUND
        );

        let invalid = ApiError::from(IdentityError::InvalidHost("nope".to_owned()));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code(), "invalid_request");
    }

    #[test]
    fn media_errors_map_onto_the_library_statuses() {
        assert_eq!(
            ApiError::from(MediaError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(MediaError::NotFound).code(),
            "media_not_found"
        );
        assert_eq!(
            ApiError::from(MediaError::SizeTooLarge { limit: 25 }).status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ApiError::from(MediaError::KeyTaken).status(),
            StatusCode::CONFLICT
        );

        let empty = ApiError::from(MediaError::EmptyFile);
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        assert_eq!(empty.code(), "invalid_request");

        let unavailable = ApiError::from(MediaError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn workflow_errors_keep_their_own_codes() {
        let invalid = ApiError::from(WorkflowError::invalid("invalid_cron", "broken schedule"));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code(), "invalid_cron");

        let unavailable = ApiError::from(WorkflowError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);

        let audit = ApiError::from(WorkflowError::Audit(AuditError::Database(
            sqlx::Error::PoolClosed,
        )));
        assert_eq!(audit.code(), "dependency_unavailable");
    }

    #[test]
    fn ai_hub_errors_map_onto_the_provider_statuses() {
        assert_eq!(
            ApiError::from(AiHubError::ProviderNotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(AiHubError::ModelNotFound).code(),
            "model_not_found"
        );
        assert_eq!(
            ApiError::from(AiHubError::ProviderNameTaken("Local".to_owned())).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::from(AiHubError::ProviderDisabled("Local".to_owned())).code(),
            "provider_disabled"
        );
        assert_eq!(
            ApiError::from(AiHubError::NoDefaultModel).status(),
            StatusCode::CONFLICT
        );

        let refused = ApiError::from(AiHubError::Upstream {
            status: 429,
            message: "slow down".to_owned(),
        });
        assert_eq!(refused.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(refused.code(), "provider_error");
        assert!(
            refused.message.contains("429"),
            "message: {}",
            refused.message
        );

        let unreachable = ApiError::from(AiHubError::Transport("connection refused".to_owned()));
        assert_eq!(unreachable.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(unreachable.code(), "provider_unreachable");

        let invalid = ApiError::from(AiHubError::InvalidChatRequest("no messages".to_owned()));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(invalid.code(), "invalid_chat_request");

        let unavailable = ApiError::from(AiHubError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn storage_errors_map_onto_the_dependency_statuses() {
        let missing = ApiError::from(StorageError::NotFound {
            key: "sites/a/one.png".to_owned(),
        });
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.code(), "object_not_found");

        let unavailable =
            ApiError::from(StorageError::Unavailable("connection refused".to_owned()));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unavailable.code(), "storage_unavailable");

        let refused = ApiError::from(StorageError::Provider {
            status: 403,
            message: "AccessDenied".to_owned(),
        });
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.code(), "storage_error");
        assert!(
            refused.message.contains("403"),
            "message: {}",
            refused.message
        );

        let misconfigured = ApiError::from(StorageError::Invalid("blank bucket".to_owned()));
        assert_eq!(misconfigured.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(misconfigured.code(), "storage_misconfigured");
    }
}
