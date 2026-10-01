//! HTTP representation of core errors.

use axum::Json;
use axum::http::header;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_ai_hub::AiHubError;
use omnion_audit::AuditError;
use omnion_automation::AutomationError;
use omnion_content::ContentError;
use omnion_core::CoreError;
use omnion_events::EventsError;
use omnion_identity::IdentityError;
use omnion_media::MediaError;
use omnion_module_analytics::AnalyticsError;
use omnion_onboarding::OnboardingError;
use omnion_permissions::PermissionsError;
use omnion_search::SearchError;
use omnion_storage::StorageError;
use omnion_workflows::WorkflowError;
use serde::Serialize;
use serde_json::Value;

/// `true` when a database error means the dependency itself is unavailable (retryable).
fn dependency_unavailable(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
    )
}

/// Error response shape used across `/api/v1`:
/// `{"error":{"code":"internal_error","message":"…"}}`.
///
/// A refusal that can explain itself carries `details` too — the permission, why it was refused
/// and the role that decided it (docs/07-IAM.md §18: the answer names its source).
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<Value>,
    /// Seconds the caller should wait, for `Retry-After`.
    ///
    /// `None` on every error that is not a refusal with a wait attached, and it is a distinct
    /// field rather than something dug out of `details` because the header has to be *absent*
    /// rather than wrong: a `Retry-After: 0` on a 403 would tell a client to retry immediately,
    /// and a `Retry-After` present on an error that is not a refusal teaches a client to wait on
    /// things that never needed waiting. One field, set only by the layer that knows the wait.
    retry_after: Option<u64>,
}

impl ApiError {
    /// Build an error with an explicit status, code and message.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
            retry_after: None,
        }
    }

    /// Attach a `Retry-After` in seconds.
    ///
    /// Zero is refused rather than clamped: "retry immediately" and "I do not know how long" are
    /// different claims, and a layer that computes a wait knows which one it means. A `0` here
    /// would be the platform telling every client to come straight back.
    #[must_use]
    pub fn with_retry_after(mut self, seconds: i64) -> Self {
        if seconds > 0 {
            self.retry_after = Some(seconds as u64);
        }
        self
    }

    /// Attach the structured explanation of a refusal.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
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
                details: None,
                retry_after: None,
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal_error",
                message: other.to_string(),
                details: None,
                retry_after: None,
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

    /// The human-readable explanation, for assertions and for a log line.
    ///
    /// The companion of [`ApiError::code`]: the code is what a client branches on and the
    /// message is what a person reads, so a test that checks one without the other pins half
    /// the contract — and it is the message that has to name the field and the three legal
    /// values, which is the part a client cannot reconstruct.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The structured explanation, when the refusal carries one.
    ///
    /// Read-only and borrowed so a caller can branch on `details.field` without cloning the
    /// value — the Explorer's form highlights the offending input from that field, and a
    /// `to_owned()` accessor would make every such check an allocation for no gain. `None` on
    /// an error that explains itself in its message, which is most of them.
    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }
}

/// The code and the message, and never the details.
///
/// `details` is excluded on purpose: it can carry a decision source, a considered list and a
/// count, which is right in a response body and wrong in a log line or an assertion message.
/// An error type that cannot be printed is one that never gets printed, so this exists even
/// though every call site in the codebase uses the accessors.
impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
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
            // A refused redelivery is a `409`, not a `400`: the request was well formed and the
            // row's *state* is what the operator has to change first (wait for the pending
            // attempt, or stop forcing a delivery the receiver already refused ten times). A
            // `400` would tell them to fix their request, which is not the problem.
            EventsError::RedeliveryRefused { code, message } => {
                Self::new(StatusCode::CONFLICT, code, message)
            }
            // A retention window outside the range is a `400` that names the field: the request
            // is what has to change. Clamping instead would answer `200` with a number the
            // operator did not choose, and a screen that shows the clamped value has no way to
            // know it was not what was asked for.
            EventsError::InvalidRetention(message) => {
                Self::bad_request("invalid_retention_window", message)
            }
            // An organization that does not exist is a `404`, not a `500`: the update matched
            // no row, which is a fact about the request rather than about the database, and a
            // caller that cannot tell the two apart retries a write that can never succeed.
            EventsError::OrganizationNotFound(_) => Self::new(
                StatusCode::NOT_FOUND,
                "organization_not_found",
                "no organization carries that id",
            ),
        }
    }
}

impl From<AnalyticsError> for ApiError {
    /// Analytics (docs/requests/REQ-007): a beacon the collector cannot use and settings the
    /// platform refuses are `400`s that name the field, a site without settings is a `404`, and
    /// the store itself keeps the platform's dependency/internal split.
    fn from(error: AnalyticsError) -> Self {
        match error {
            AnalyticsError::InvalidPayload(message) => Self::bad_request("invalid_beacon", message),
            AnalyticsError::InvalidQuery(message) => {
                Self::bad_request("invalid_report_query", message)
            }
            AnalyticsError::EmptyBeacon => Self::bad_request(
                "empty_beacon",
                "the beacon carries neither a pageview nor an event",
            ),
            AnalyticsError::InvalidSettings(message) => {
                Self::bad_request("invalid_analytics_settings", message)
            }
            AnalyticsError::SettingsNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "analytics_settings_not_found",
                "this site has no analytics settings",
            ),
            AnalyticsError::InvalidGoal(message) => Self::bad_request("invalid_goal", message),
            AnalyticsError::InvalidVisitor(message) => {
                Self::bad_request("invalid_visitor", message)
            }
            AnalyticsError::GoalNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "goal_not_found",
                "this site has no such goal",
            ),
            AnalyticsError::GoalNameTaken => Self::new(
                StatusCode::CONFLICT,
                "goal_name_taken",
                "another goal of this site carries this name",
            ),
            AnalyticsError::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            AnalyticsError::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
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
            // Security policy, second factors and stored secrets (REQ-006, slice 3). A policy
            // refused by a range check names the control the reader has to fix, so the panel can
            // point at the field instead of printing a sentence.
            IdentityError::InvalidPolicy { field, message } => {
                Self::bad_request("invalid_security_policy", format!("{field} {message}"))
                    .with_details(serde_json::json!({ "field": field }))
            }
            IdentityError::InvalidNetwork(message) => Self::bad_request("invalid_network", message),
            IdentityError::FactorNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "factor_not_found",
                "this account has no such second factor",
            ),
            IdentityError::InvalidFactor(message) => {
                Self::bad_request("invalid_factor_code", message)
            }
            // A refused ceremony (REQ-006, slice 3b): the message names the one check that did
            // not hold, and the panel shows it next to the passkey step.
            IdentityError::WebAuthn(message) => Self::bad_request("webauthn_refused", message),
            // An envelope that cannot be read is never the caller's fault: either the key this
            // installation uses changed, or the value was tampered with. Both are for the
            // operator, and the message says which knob to look at.
            IdentityError::Crypto => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "secret_unreadable",
                "the stored secret could not be read — check the key this installation uses",
            ),
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
            // A block tree the store cannot accept is a bad request, and it is not generic: the
            // message names the first failing block so the editor can put the cursor on it.
            ContentError::InvalidBlock(message) => Self::bad_request("invalid_blocks", message),
            // Patterns and page templates (REQ-063 slice 3) follow the same shape as pages: a
            // missing row is a 404, a key an organization already uses is a 409 — not a 400,
            // because nothing about the request is malformed, the name is simply taken.
            ContentError::PatternNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "pattern_not_found",
                "no such pattern",
            ),
            ContentError::PatternKeyTaken(key) => Self::new(
                StatusCode::CONFLICT,
                "pattern_key_taken",
                format!("this organization already has a pattern with the key {key:?}"),
            ),
            ContentError::TemplateNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "template_not_found",
                "no such page template",
            ),
            ContentError::TemplateKeyTaken(key) => Self::new(
                StatusCode::CONFLICT,
                "template_key_taken",
                format!("this organization already has a page template with the key {key:?}"),
            ),
            ContentError::TemplateIsSystem => Self::new(
                StatusCode::CONFLICT,
                "template_is_system",
                "this template ships with the platform and cannot be deleted",
            ),
            // Menus and scheduled publishing (REQ-064 slice 1). A location another menu holds
            // is a 409 rather than a 400: nothing about the request is malformed, the location
            // is simply taken — the same answer a taken slug gets, because it is the same
            // situation. The holder travels in the message so the editor can be pointed at the
            // menu to move rather than left guessing which of the site's menus it was.
            ContentError::MenuNotFound => {
                Self::new(StatusCode::NOT_FOUND, "menu_not_found", "no such menu")
            }
            ContentError::MenuKeyTaken(key) => Self::new(
                StatusCode::CONFLICT,
                "menu_key_taken",
                format!("this site already has a menu with the key {key:?}"),
            ),
            ContentError::MenuLocationTaken {
                location,
                holder_key,
                ..
            } => Self::new(
                StatusCode::CONFLICT,
                "menu_location_taken",
                format!(
                    "the {location} location is already held by the {holder_key:?} menu; \
                     move it there first"
                ),
            ),
            ContentError::PublishingEntryNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "publishing_entry_not_found",
                "no such publishing entry",
            ),
            // Forms (REQ-064 slice 2). The two statuses here are load-bearing and both were
            // wrong when the mapping was missing: without an arm, every ContentError fell
            // through to 400, so "no such form" answered "your request was malformed" and the
            // panel showed a validation error on a form that had been deleted. A missing row is
            // 404 and a taken key is 409 — the same two answers `MenuNotFound` gives, for the
            // same reason.
            ContentError::FormNotFound => {
                Self::new(StatusCode::NOT_FOUND, "form_not_found", "no such form")
            }
            // A file the page cannot feature. 400 rather than 404: the row either exists and is
            // unusable (in the trash, another site's, not an image) or does not exist, and the
            // message says which — a 404 here would be indistinguishable from "no such page",
            // which is the one answer an operator staring at a *working* page must not get.
            ContentError::FeaturedMediaUnavailable(message) => {
                Self::new(StatusCode::BAD_REQUEST, "featured_media_unavailable", message)
            }
            ContentError::FormKeyTaken(key) => Self::new(
                StatusCode::CONFLICT,
                "form_key_taken",
                format!("this site already has a form with the key {key:?}"),
            ),
            ContentError::SubmissionNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "submission_not_found",
                "no such submission in this form",
            ),
            ContentError::InvalidLocation(message) => {
                Self::bad_request("invalid_location", message)
            }
            ContentError::InvalidVisibility(message) => {
                Self::bad_request("invalid_visibility", message)
            }
            ContentError::InvalidMenuItem(message) => {
                Self::bad_request("invalid_menu_item", message)
            }
            ContentError::TooDeep(message) => Self::bad_request("menu_too_deep", message),
            ContentError::InvalidPublishAction(message) => {
                Self::bad_request("invalid_publish_action", message)
            }
            ContentError::InvalidSchedule(message) => {
                Self::bad_request("invalid_schedule", message)
            }
            // SEO toolkit (REQ-064 slice 3). A loop is 409, not 400: the request is well formed
            // and the site is simply not in a state that allows it — the same "that is taken"
            // shape a duplicate key gets, and the distinction matters because the panel offers a
            // different next step for each.
            ContentError::InvalidRedirect(message) => {
                Self::bad_request("invalid_redirect", message)
            }
            ContentError::RedirectLoop(message) => {
                Self::new(StatusCode::CONFLICT, "redirect_loop", message)
            }
            ContentError::RedirectNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "redirect_not_found",
                "no such redirect rule",
            ),
            ContentError::InvalidSeo(message) => Self::bad_request("invalid_seo", message),
            ContentError::BrokenLinkNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "broken_link_not_found",
                "no such broken link",
            ),
            ContentError::SiteNotFound => {
                Self::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
            }
            // A comment-shaped refusal keeps its OWN code rather than falling through to
            // `invalid_request`. This is the exact defect slice 2 recorded for the forms
            // module — a `ContentError` variant with no arm is not "unmapped", it is 400 with
            // the platform's least specific message, so a visitor who typed a bad address and a
            // panel that turned comments off both answered "your request was malformed".
            ContentError::InvalidComment(message) => {
                Self::bad_request("invalid_comment", message)
            }
            // Comments (REQ-064, slice 4a). The three statuses are load-bearing in three
            // different ways and the panel offers a different next step for each: a missing
            // comment is 404, a comment that is ALREADY in the state the request asked for is a
            // 409 (it is a stale page or a race, not an error to retry), and a banned sender is
            // 403 — a person is being refused, and the message says which ban.
            ContentError::CommentNotFound => {
                Self::new(StatusCode::NOT_FOUND, "comment_not_found", "no such comment")
            }
            ContentError::CommentAlreadyInState(message) => {
                Self::new(StatusCode::CONFLICT, "comment_already_in_state", message)
            }
            ContentError::CommentBanned(message) => {
                Self::new(StatusCode::FORBIDDEN, "comment_banned", message)
            }
            // 400, and the same code the trigger's `check_violation` translates to in
            // `comments::api_error_from_comment_write` — the store's pre-check and the schema's
            // trigger answer the same question and must answer it identically, or a client
            // sees two codes for one rule depending on which half caught it.
            ContentError::CommentThreadTooDeep => Self::bad_request(
                "comment_thread_too_deep",
                "a reply cannot answer another reply",
            ),
            // Newsletter / membership (REQ-064 slice 4b). The catch-all below turns every
            // unmapped variant into `invalid_request`, which is exactly the trap slice 4a
            // recorded: a new store error silently becomes "your request was malformed", so a
            // token that matched nothing answers as a form mistake. These are the STATUS
            // assignments, not the variants.
            ContentError::InvalidNewsletter(message) => {
                Self::bad_request("invalid_newsletter", message)
            }
            ContentError::NewsletterListNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "newsletter_list_not_found",
                "no such newsletter list",
            ),
            ContentError::SubscriberNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "subscriber_not_found",
                "no such subscriber",
            ),
            ContentError::IssueNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "newsletter_issue_not_found",
                "no such newsletter issue",
            ),
            ContentError::SubscriberAlreadyConfirmed(email) => Self::new(
                StatusCode::CONFLICT,
                "subscriber_already_confirmed",
                format!("{email} is already confirmed on this list"),
            ),
            ContentError::InvalidToken => Self::bad_request(
                "invalid_token",
                "that link is not valid — it may have expired or already been used",
            ),
            // Memberships (REQ-064, slice 4c). Every one of these is an explicit STATUS
            // assignment rather than falling through to the catch-all's `invalid_request`,
            // which is the trap the newsletter half above records: an unmapped variant turns a
            // refused sign-in into "your request was malformed" and a cross-tenant member into a
            // form mistake.
            ContentError::InvalidMember(message) => Self::bad_request("invalid_member", message),
            ContentError::MemberNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "member_not_found",
                "no such member",
            ),
            // 409 and not 404, and the distinction matters: this variant is only ever returned
            // by the PANEL's "add a member", where a signed-in operator has to be told their
            // click did not work. The public signup catches it and answers 202 with the same
            // body either way, so nothing is disclosed to a visitor.
            ContentError::MemberEmailTaken(email) => Self::new(
                StatusCode::CONFLICT,
                "member_email_taken",
                format!("{email} already has an account on this site"),
            ),
            // 401, not 403: the caller is not who they claim to be. And ONE message for the
            // four refusals behind it (unknown address, wrong password, not verified,
            // blocked) — a sign-in form that distinguishes them is a list of every address on
            // the site and a way to find out which of them are members.
            ContentError::InvalidCredentials => Self::unauthorized(
                "invalid_credentials",
                "that e-mail and password do not match an account here",
            ),
            ContentError::VerificationNotRequired => Self::bad_request(
                "verification_not_required",
                "this site does not require verification — turn it on in membership settings first",
            ),
            ContentError::WeakPassword(message) => Self::bad_request("weak_password", message),
            // Theme settings (REQ-062 slice 2). Four separate arms, and each status is a
            // different next step for the operator: a missing revision is a stale link (404),
            // "no draft" means the Publish button was the wrong button (409 — it is not a
            // malformed request, and a 400 would send the panel into a retry loop), a stale
            // draft is a race between two tabs (409, with both numbers in the message), and a
            // contrast failure is a legal payload the product wants acknowledged (422 — the
            // request is well-formed and the answer is "yes, but confirm first").
            ContentError::ThemeNotFound(key) => Self::new(
                StatusCode::NOT_FOUND,
                "theme_not_found",
                format!("no installed theme has the key '{key}'"),
            ),
            ContentError::RollbackUnavailable => Self::new(
                StatusCode::CONFLICT,
                "theme_rollback_unavailable",
                "this site has no theme activation to roll back from",
            ),
            ContentError::ThemeSettingsRevisionNotFound(revision_no) => Self::new(
                StatusCode::NOT_FOUND,
                "theme_settings_revision_not_found",
                format!("this site has no settings revision numbered {revision_no}"),
            ),
            ContentError::ThemeSettingsNothingToPublish => Self::new(
                StatusCode::CONFLICT,
                "theme_settings_nothing_to_publish",
                "this site has no saved draft to publish — save one first",
            ),
            ContentError::ThemeSettingsDraftStale {
                draft_no,
                published_no,
            } => Self::new(
                StatusCode::CONFLICT,
                "theme_settings_draft_stale",
                format!(
                    "the draft is revision {draft_no} but revision {published_no} is the one that \
                     is live; reload before publishing"
                ),
            ),
            ContentError::ThemeSettingsContrastRefused(message) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "theme_settings_contrast_required",
                message,
            ),
            // Theme layouts and packages (REQ-062 slice 3). The statuses are three different
            // next steps again: a slot the platform does not render is a bad request (the
            // caller named something that cannot exist), a reset with no shipped default is a
            // conflict (the request is well-formed; this site never had that default), a
            // package with errors is 422 with the whole report in the message so the upload
            // screen can point at the line — and a removal that is refused because the theme is
            // bundled or in use is a CONFLICT, because nothing about the request was malformed:
            // it is a resource in a state that forbids the write.
            ContentError::ThemeUnknownSlot(slot) => Self::bad_request(
                "theme_unknown_slot",
                format!("'{slot}' is not a slot this platform renders"),
            ),
            ContentError::ThemeSlotTooLong(slot, max) => Self::bad_request(
                "theme_slot_too_long",
                format!("the slot name must stay under {max} characters ('{slot}')"),
            ),
            ContentError::ThemeSlotTooManyBlocks(max) => Self::bad_request(
                "theme_slot_too_many_blocks",
                format!("a slot may hold at most {max} blocks"),
            ),
            ContentError::ThemeSlotInvalid(message) => Self::bad_request(
                "theme_slot_invalid",
                format!("this slot cannot be stored: {message}"),
            ),
            ContentError::ThemeSlotNoDefault(slot) => Self::new(
                StatusCode::CONFLICT,
                "theme_slot_no_default",
                format!("this theme ships no default layout for '{slot}'"),
            ),
            ContentError::ThemePackageInvalid(count) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "theme_package_invalid",
                format!("the package has {count} error(s) and was not installed"),
            ),
            ContentError::ThemeBundledCannotBeRemoved(key) => Self::new(
                StatusCode::CONFLICT,
                "theme_bundled_cannot_be_removed",
                format!("'{key}' is a bundled theme and cannot be removed"),
            ),
            ContentError::ThemeInUse(key) => Self::new(
                StatusCode::CONFLICT,
                "theme_in_use",
                format!("'{key}' is still in use and cannot be removed"),
            ),
            ContentError::ThemePackageTooLarge(max) => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "theme_package_too_large",
                format!("the package is larger than the {max} byte limit"),
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

impl From<SearchError> for ApiError {
    /// Search reads the panel's own tables through the shared pool: a pool failure is the usual
    /// retryable dependency split, an unknown provider is the caller's mistake, anything else is
    /// an internal error the operator has to see.
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Store(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            SearchError::Store(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
            SearchError::UnknownProvider(key) => Self::bad_request(
                "unknown_provider",
                format!("no search provider named \"{key}\""),
            ),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                other.to_string(),
            ),
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
                details: None,
                retry_after: None,
            },
            // REQ-006 role depth: the field-level refusals carry the field they belong to, so the
            // matrix screen can point at `inherits_role_id` instead of showing a generic message.
            PermissionsError::InheritanceCycle | PermissionsError::SelfInheritance => Self {
                status: StatusCode::CONFLICT,
                code: "role_inheritance_cycle",
                message:
                    "the role cannot inherit from itself or one of its own descendants (inherits_role_id)"
                        .to_owned(),
                details: None,
                retry_after: None,
            },
            PermissionsError::InheritanceDepthExceeded { max } => Self::bad_request(
                "role_inheritance_depth",
                format!("inheritance chains may not exceed {max} levels (inherits_role_id)"),
            ),
            PermissionsError::RoleHasBindings(count) => Self {
                status: StatusCode::CONFLICT,
                code: "role_has_bindings",
                message: format!(
                    "the role still carries {count} live binding(s); revoke them before deleting it"
                ),
                details: None,
                retry_after: None,
            },
            PermissionsError::VersionConflict { expected, current } => Self {
                status: StatusCode::CONFLICT,
                code: "role_version_conflict",
                message: format!(
                    "the role changed since it was read: expected version {expected}, current version {current}"
                ),
                details: None,
                retry_after: None,
            },
            PermissionsError::InvalidEntries { unknown, duplicates } => {
                let mut parts: Vec<String> = Vec::new();
                if !unknown.is_empty() {
                    parts.push(format!("unknown permission key(s): {}", unknown.join(", ")));
                }
                if !duplicates.is_empty() {
                    parts.push(format!("duplicate entr(ies): {}", duplicates.join(", ")));
                }
                Self::bad_request(
                    "invalid_entries",
                    format!("the permission set was refused — {}", parts.join("; ")),
                )
            }
            // Subjects and scopes (REQ-006, slice 2): the store's refusals map onto the surface
            // the screens point at — a missing group is a 404, a taken name a 409, and the rest
            // a field-level 400.
            PermissionsError::GroupNotFound => {
                Self::new(StatusCode::NOT_FOUND, "group_not_found", "no such group")
            }
            PermissionsError::GroupNameTaken => Self::new(
                StatusCode::CONFLICT,
                "group_name_taken",
                "a group with this name already exists in this organization",
            ),
            PermissionsError::ServiceAccountNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "service_account_not_found",
                "no such service account",
            ),
            PermissionsError::ServiceAccountNameTaken => Self::new(
                StatusCode::CONFLICT,
                "service_account_name_taken",
                "a service account with this name already exists in this organization",
            ),
            PermissionsError::InvalidMachineKey => Self::new(
                StatusCode::UNAUTHORIZED,
                "invalid_machine_key",
                "this machine key is unknown, revoked or does not match its secret",
            ),
            PermissionsError::UnknownSimulatedAction(key) => Self::bad_request(
                "unknown_action",
                format!("{key:?} is not a known permission key"),
            ),
            // The ABAC policy surface (REQ-006, slice 4a): a missing row is a 404, a policy the
            // engine cannot read (unknown operator, empty target, blank name) a 400 that repeats
            // the sentence the validator wrote.
            PermissionsError::PolicyNotFound => {
                Self::new(StatusCode::NOT_FOUND, "policy_not_found", "no such policy")
            }
            PermissionsError::InvalidPolicy(message) => {
                Self::bad_request("invalid_policy", message)
            }
            // Permission requests and approvals (REQ-006, slice 4b): a missing request is a 404,
            // a second decision on the same request a 409, and an unusable window a 400 that
            // repeats the range it refused.
            PermissionsError::RequestNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "request_not_found",
                "no such permission request",
            ),
            PermissionsError::RequestAlreadyDecided => Self::new(
                StatusCode::CONFLICT,
                "request_already_decided",
                "this request has already been decided",
            ),
            PermissionsError::InvalidRequest(message) => {
                Self::bad_request("invalid_request", message)
            }
            PermissionsError::InvalidGroupName(name) => Self::bad_request(
                "invalid_group_name",
                format!("{name:?} is not a usable group name"),
            ),
            PermissionsError::InvalidServiceAccountName(name) => Self::bad_request(
                "invalid_service_account_name",
                format!("{name:?} is not a usable service-account name"),
            ),
            // Everything else is a bad request: the caller handed in something the store
            // cannot accept (key shape, priority range, unknown permission, bad scope).
            other => Self::bad_request("invalid_request", other.to_string()),
        }
    }
}

impl From<omnion_backup::BackupError> for ApiError {
    fn from(error: omnion_backup::BackupError) -> Self {
        use omnion_backup::BackupError as B;
        match error {
            // A missing run or schedule is a 404, and it is a 404 rather than a 403 even
            // when the row exists in another tenant: a 403 confirms the id is real, and a
            // backup's existence is itself information about the platform.
            B::NotFound => Self::new(StatusCode::NOT_FOUND, "backup_not_found", "no such backup"),
            B::ScheduleNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "backup_schedule_not_found",
                "no such backup schedule",
            ),
            // A refusal is a 409, not a 400: the request was legal and the platform has
            // moved on. That is the same split the permissions and media modules use, and
            // the two answer different questions for the caller.
            B::Rejected(message) => Self::new(StatusCode::CONFLICT, "backup_rejected", message),
            B::Invalid(message) => Self::bad_request("invalid_backup", message),
            B::Partial {
                failed,
                total,
                message,
            } => Self::new(
                StatusCode::CONFLICT,
                "backup_partial",
                format!("{failed} of {total} parts failed: {message}"),
            ),
            B::Database(err) if dependency_unavailable(&err) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            B::Database(err) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            ),
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
            // A folder that is gone is a `404` naming the folder, not a generic bad request: the
            // browser deep-links a folder id, and a stale link has to say what is missing.
            MediaError::FolderNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "folder_not_found",
                "no such folder in this library",
            ),
            // A name that collides with a sibling is a `409`, so a client can distinguish "try
            // again" from "you typed something impossible".
            MediaError::FolderNameTaken => Self::new(
                StatusCode::CONFLICT,
                "folder_name_taken",
                "a folder with this name already exists here",
            ),
            // The library root is structural. Renaming or deleting it is not a bad request, it is
            // a refusal of an operation that has no valid form.
            MediaError::RootFolderProtected => Self::new(
                StatusCode::CONFLICT,
                "root_folder_protected",
                "the library root cannot be renamed, moved or deleted",
            ),
            MediaError::FolderNotEmpty { what, count } => Self::new(
                StatusCode::CONFLICT,
                "folder_not_empty",
                format!("the folder still holds {count} {what}"),
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
            // unusable content type, a key the store refuses, a cycle, a folder of another site
            // or a file that is in the trash. Each one names itself so the panel can put the
            // message on the field that caused it.
            MediaError::InvalidFolderName(message) => {
                Self::bad_request("invalid_folder_name", message)
                    .with_details(serde_json::json!({ "field": "name" }))
            }
            MediaError::FolderCycle { path } => Self::new(
                StatusCode::CONFLICT,
                "folder_cycle",
                format!("a folder cannot be moved inside itself (target path `{path}`)"),
            ),
            MediaError::FolderSiteMismatch => {
                Self::bad_request("folder_site_mismatch", "the folder belongs to another site")
            }
            MediaError::FileTrashed => Self::new(
                StatusCode::CONFLICT,
                "file_trashed",
                "the file is in the trash; restore it before changing it",
            ),
            // A preset name is part of a public URL, so a missing one is a `404` *naming the
            // name*: "card is not a preset" and "card produced no bytes" are different answers,
            // and someone debugging a broken page needs to know which one they got.
            MediaError::PresetNotFound { name } => Self::new(
                StatusCode::NOT_FOUND,
                "preset_not_found",
                format!("no transformation preset named `{name}` on this site"),
            ),
            MediaError::PresetNameTaken { name } => Self::new(
                StatusCode::CONFLICT,
                "preset_name_taken",
                format!("a preset named `{name}` already exists on this site"),
            ),
            // A file that cannot be transformed is a `422`: the request was well-formed, the
            // thing it named is simply not transformable. Reporting it as a bad request would
            // tell an editor their form was malformed when the form is fine and the PNG is not.
            MediaError::NotAnImage { content_type } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_transformable",
                format!("`{content_type}` is not a raster image, so it cannot be transformed"),
            ),
            MediaError::Undecodable { reason } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_transformable",
                format!("the file could not be decoded as an image: {reason}"),
            ),
            // An enlargement is refused rather than honoured, and the reason says so: the
            // operator's fix is to serve the original, not to pick a smaller number.
            MediaError::TransformFailed { reason } if reason.contains("enlarge") => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "transform_would_enlarge",
                reason,
            ),
            MediaError::TransformFailed { reason } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "transform_failed",
                format!("the image could not be transformed: {reason}"),
            ),
            // Every preset field error names the field that caused it, so the settings form can
            // put the message under the right input instead of in a banner nobody reads.
            MediaError::InvalidPreset(message) => {
                let field = if message.contains("quality") {
                    Some("quality")
                } else if message.contains("width") || message.contains("height") {
                    Some("width")
                } else if message.contains("name") {
                    Some("name")
                } else {
                    None
                };
                match field {
                    Some(field) => Self::bad_request("invalid_preset", message)
                        .with_details(serde_json::json!({ "field": field })),
                    None => Self::bad_request("invalid_preset", message),
                }
            }
            // A merge that cannot happen is a `409`, not a `400`: every value in the request was
            // legal and the library simply does not have the group the caller believes it has —
            // usually because a colleague merged it a minute ago. A `400` would tell an operator
            // their form was wrong when their *click* was late.
            MediaError::MergeRefused { reason } => {
                Self::new(StatusCode::CONFLICT, "duplicate_merge_refused", reason)
                    .with_details(serde_json::json!({ "field": "keep" }))
            }
            MediaError::TooManySites { limit, requested } => Self::bad_request(
                "too_many_sites",
                format!(
                    "a cross-site report may cover at most {limit} sites; {requested} were \
                         named"
                ),
            ),
            // Every storage field error names the field that caused it, and carries it as a
            // detail — the settings form puts the message under that input, and a *save* and a
            // *connection test* of the same bad value produce the same field, so a person is
            // never told to fix a field on one path that the other path accepted.
            MediaError::InvalidStorageSetting { field, message } => {
                Self::bad_request("invalid_storage_setting", message)
                    .with_details(serde_json::json!({ "field": field }))
            }
            // Same rule for the scanning settings, with its own code so a client can tell a
            // bad scanner endpoint from a bad storage endpoint — they are different screens
            // and different people fix them.
            MediaError::InvalidScanSetting { field, reason } => {
                Self::bad_request("invalid_scan_setting", reason)
                    .with_details(serde_json::json!({ "field": field }))
            }
            // A release with no reason is a `400`, not a `409`: the request was well-formed and
            // the refusal is about an empty field, so the caller can fix it and try again
            // without a state having changed underneath them.
            MediaError::InvalidReleaseReason => Self::bad_request(
                "release_reason_required",
                MediaError::InvalidReleaseReason.to_string(),
            )
            .with_details(serde_json::json!({ "field": "reason" })),
            // Same rule for the retention settings, with its own code so a client can tell a
            // bad window from a bad scanner endpoint — they are two tabs of one screen, and
            // the message is rendered under the input that caused it.
            MediaError::InvalidRetentionSetting { field, reason } => {
                Self::bad_request("invalid_retention_setting", reason)
                    .with_details(serde_json::json!({ "field": field }))
            }
            // Same rule for the custom metadata pairs, with its own code: the pair editor puts
            // the message under the row that caused it, and `metadata.<key>` means a refusal
            // about a licence number cannot land under the campaign field beside it.
            MediaError::InvalidMetadata { field, reason } => {
                Self::bad_request("invalid_metadata", reason)
                    .with_details(serde_json::json!({ "field": field }))
            }
            // A missing policy is a `404`, and the tenancy scope lives *inside* the lookup
            // rather than being applied afterwards — the same lesson `media_grants::delete_one`
            // learned from a walk that got a `403` for another tenant's grant id and thereby
            // confirmed the id exists.
            MediaError::RetentionPolicyNotFound => Self::new(
                StatusCode::NOT_FOUND,
                "retention_policy_not_found",
                "no such retention policy on this site",
            ),
            MediaError::PolicyNameTaken { name } => Self::new(
                StatusCode::CONFLICT,
                "retention_policy_name_taken",
                format!("a retention policy named `{name}` already exists on this site"),
            )
            .with_details(serde_json::json!({ "field": "name" })),
            // A purge that cannot happen: the request was legal, the file is past its window,
            // and something in the platform still resolves to it. A `400` would send an
            // operator to fix a form that was never wrong — the fix is to repoint a page.
            MediaError::PurgeRefused { reason } => {
                Self::new(StatusCode::CONFLICT, "purge_refused", reason)
            }
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
            // A `409`, not a `503`: nothing is unavailable and retrying will not help, because
            // the object and the row disagree about its length and only a repair fixes that. A
            // retryable status here would have a client — or a media player — asking for ever.
            StorageError::RangeNotSatisfiable { key, requested } => Self::new(
                StatusCode::CONFLICT,
                "object_range_not_satisfiable",
                format!(
                    "the stored object is shorter than the range that was asked for \
                     ({requested} of {key:?})"
                ),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message,
                details: self.details,
            },
        };
        let mut response = (self.status, Json(body)).into_response();
        // Only ever set when the error carries a wait. Absent is not the same as zero: a client
        // that sees no `Retry-After` retries on its own schedule, which is the correct behaviour
        // for every error that is not a refusal with a window behind it.
        if let Some(seconds) = self.retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
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
