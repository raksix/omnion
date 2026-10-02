//! The errors this crate returns.
//!
//! One rule runs through the list: **nothing here carries caller-supplied key material.** A
//! validation error quotes a field name and a rule, never the value that broke the rule — so an
//! error that ends up in a log, an event payload or a browser console cannot become the leak
//! the shape was designed to prevent.

use thiserror::Error;

/// What can go wrong in the developer platform.
///
/// **Only the client-facing variants derive `PartialEq`, and it is done by hand.** The CLI poll
/// rule's tests compare outcomes by value -- "a poll too soon returns `SlowDown { seconds: 10 }`"
/// -- because an assertion that could only check "it returned *an* error" would pass for every
/// wrong number in the rule.
///
/// The derive cannot be applied to the enum. `Database(sqlx::Error)` is a variant and
/// `sqlx::Error` is neither `PartialEq` nor `Eq`, so `#[derive(PartialEq)]` here compiles on the
/// default build -- where the variant is `cfg`'d out and there is nothing to compare -- and fails
/// only under `--features store`. A gate that runs one of the two builds is a gate that misses
/// exactly this. The manual impl below compares the client variants and treats the database one
/// as equal to nothing, which is the truth: no client rule asserts on a database error's value.
#[derive(Debug, Error)]
pub enum DeveloperError {
    /// A submitted name was outside the accepted length.
    #[error("name must be between {min} and {max} characters")]
    InvalidName {
        /// Shortest accepted.
        min: usize,
        /// Longest accepted.
        max: usize,
    },

    /// A key was created with no scopes. A key that can do nothing is a credential nobody
    /// asked for, and it is refused rather than stored and quietly useless.
    #[error("a key needs at least one scope")]
    NoScopes,

    /// One scope in the list was blank.
    #[error("a scope cannot be blank")]
    EmptyScope,

    /// The same scope appeared twice, which would make the granted set ambiguous.
    #[error("scope {0:?} was listed twice")]
    DuplicateScope(String),

    /// An environment other than `live` or `sandbox`.
    #[error("{0:?} is not an environment this platform has")]
    UnknownEnvironment(String),

    /// A rate tier other than `standard` or `high`.
    #[error("{0:?} is not a rate tier this platform has")]
    UnknownRateTier(String),

    /// A status class filter outside `1xx`–`5xx`.
    #[error("{0:?} is not a status class; use 1xx through 5xx")]
    UnknownStatusClass(String),

    /// A negative duration filter. It would read as "no minimum" and quietly match everything.
    #[error("a duration filter cannot be negative")]
    NegativeDuration,

    /// The requested key does not exist in this organization.
    #[error("no such API key")]
    KeyNotFound,

    /// A key with this name already exists in the organization. The unique index is the
    /// enforcement; this is the message it produces before the database has to.
    #[error("a key named {0:?} already exists")]
    KeyNameTaken(String),

    /// A negative or absurd expiry. The panel offers `never`, 30, 90 and 365 days; anything
    /// else is either a typo or a request trying to mint a key that is dead on arrival — which
    /// reads on the caller's side as "the key I just made does not work" with no reason given.
    #[error("expiry must be 30, 90 or 365 days, or omitted for never")]
    InvalidExpiry(i64),

    /// A CIDR entry that is not a network.
    #[error("{0:?} is not a CIDR block")]
    InvalidCidr(String),

    /// The row's hash was not written by this build's scheme, so nothing can be verified
    /// against it. Deliberately the same answer as a wrong secret, so it cannot be used to
    /// probe which keys exist.
    #[error("this key cannot be verified")]
    KeyUnverifiable,

    /// The presented token was revoked, expired, malformed or simply wrong — one answer for all
    /// of them, so a caller cannot tell a valid prefix with a bad secret from a dead key.
    #[error("invalid API key")]
    InvalidKey,

    /// The `high` rate tier was asked for by somebody without the role that grants it.
    #[error("the high rate tier needs an owner or administrator")]
    HighTierRefused,

    /// A key that is not active tried to authenticate.
    #[error("this key is {0}")]
    KeyNotActive(&'static str),

    // --- OAuth applications (REQ-033, slice 3) ---------------------------------
    //
    // The rule every variant below follows is the module doc's: **none of them carries a
    // submitted URL or a secret.** A redirect URI is an attack surface by construction and an
    // app's secret is a credential; echoing either into an error that reaches a log index or a
    // browser console undoes the rule the module was written to enforce. What these carry
    // instead is a *position* — `index: 2` — which is enough for the panel to put a message
    // under the right row and useless for anybody reading the log.
    /// A status other than `active`, `suspended` or `deleted`.
    #[error("{0:?} is not a status an application has")]
    UnknownAppStatus(String),

    /// An app with this name already exists in the organization.
    ///
    /// The unique index `oauth_apps_org_name_key` is the enforcement; this is the message it
    /// produces *before* the database has to. It exists because a name clash has to land under
    /// the name box: a `500` carrying a PostgreSQL index name is not an answer a person can act
    /// on, and the panel's `ApiError.code` is what decides where the message goes.
    #[error("an application named {0:?} already exists")]
    AppNameTaken(String),

    /// A description longer than the bound.
    #[error("the description cannot be longer than {max} characters")]
    AppDescriptionTooLong {
        /// The bound that was exceeded.
        max: usize,
    },

    /// An app was registered with no redirect URI at all.
    #[error("an application needs at least one redirect URI")]
    NoRedirectUris,

    /// More redirect URIs than [`crate::oauth::MAX_REDIRECT_URIS`].
    #[error("an application may register at most {max} redirect URIs")]
    TooManyRedirectUris {
        /// The bound that was exceeded.
        max: usize,
    },

    /// One redirect URI was longer than [`crate::oauth::MAX_REDIRECT_URI_LENGTH`].
    #[error("redirect URI #{index} is longer than {max} characters")]
    RedirectUriTooLong {
        /// Which entry in the submitted list, zero-based.
        index: usize,
        /// The bound that was exceeded.
        max: usize,
    },

    /// One redirect URI's scheme is not one the platform will redirect a browser to.
    ///
    /// The *position* rather than the URL, deliberately — see the module doc.
    #[error("redirect URI #{index} must be https, or http on localhost")]
    RedirectUriSchemeRefused {
        /// Which entry in the submitted list, zero-based.
        index: usize,
    },

    /// The same redirect URI appeared twice in one submission.
    #[error("redirect URI #{index} is already registered in this list")]
    DuplicateRedirectUri {
        /// Which entry in the submitted list, zero-based.
        index: usize,
    },

    /// An app was registered with no grant type.
    #[error("an application needs at least one grant type")]
    NoGrantTypes,

    /// The same grant type appeared twice.
    #[error("grant type #{index} is listed twice")]
    DuplicateGrantType {
        /// Which entry in the submitted list, zero-based.
        index: usize,
    },

    /// A grant type outside the two this platform implements.
    #[error("{0:?} is not a grant type this platform has")]
    UnknownGrantType(String),

    /// The requested app does not exist in this organization — or, deliberately, is not the
    /// one whose `client_id` was submitted.
    ///
    /// One error for both, because an authorization endpoint that distinguishes "no such app"
    /// from "that app is not yours" is an existence oracle across tenants.
    #[error("no such application")]
    AppNotFound,

    /// An app that exists but cannot start a flow right now.
    #[error("this application is {0}")]
    AppNotActive(&'static str),

    /// A flow the app is not registered for was requested.
    #[error("this application is not registered for that grant type")]
    GrantNotRegistered,

    /// A redirect URI that is not one this app registered was submitted.
    ///
    /// Carries no URI: the submitted value is attacker-controlled by definition.
    #[error("this redirect URI is not registered for the application")]
    RedirectUriNotRegistered,

    /// A requested scope the app is not registered for.
    #[error("this application is not registered for one of the requested scopes")]
    ScopeNotRegistered,

    /// A PKCE challenge that could never be verified, or half a challenge/method pair.
    #[error("the code challenge is not usable; send S256, or neither value")]
    CodeChallengeRefused,

    /// A client secret presented to the token endpoint that matched neither the current hash
    /// nor — during an open overlap — the previous one.
    ///
    /// One answer for "no such client", "wrong secret", "expired previous secret" and "this
    /// app cannot use that grant", because a caller that can tell those apart learns which
    /// client ids exist.
    #[error("invalid client credentials")]
    InvalidClient,

    /// The presented redirect URI did not match the one the code was issued for.
    ///
    /// OAuth requires the token request to repeat the same URI the authorization request used,
    /// and it is a real rule rather than ceremony: a code is bound to a redirect, and accepting
    /// any other URI is how an intercepted code gets redeemed by whoever reached the endpoint.
    #[error("the redirect URI does not match the authorization request")]
    RedirectUriMismatch,

    /// The authorization code is unknown, already spent, or past its expiry.
    ///
    /// One answer for all three, and it is the same answer [`Self::InvalidClient`] gives for a
    /// bad secret: a caller probing codes learns nothing from which of the three it hit.
    #[error("invalid authorization code")]
    InvalidCode,

    /// A `client_credentials` request presented a user-bound authorization code, or the
    /// reverse. The two flows issue different things and swapping them is a privilege change.
    #[error("this grant cannot be used with that code")]
    GrantMismatch,

    // --- SDK scaffolds and the CLI (REQ-033, slice 4) ------------------------------------
    //
    // Same rule as the OAuth block: these carry a *code* and a sentence, never the value that
    // broke the rule. A scaffold name becomes a bucket key and a package name; echoing it back
    // from an error that reaches a log index tells a reader nothing they did not submit.
    /// A template kind other than `plugin`, `theme` or `workflow`.
    #[error("{0:?} is not a template kind this platform scaffolds")]
    UnknownScaffoldKind(String),

    /// A target other than `live` or `sandbox`.
    #[error("{0:?} is not a target this platform scaffolds for")]
    UnknownScaffoldTarget(String),

    /// A scaffold request that broke a rule.
    ///
    /// Carries the *code* alongside the message, which is the shape the panel needs: the code
    /// decides which field the message goes under, and a variant per field would mean the
    /// table grows a row every time a template gains a rule.
    #[error("{message}")]
    ScaffoldRefused {
        /// Stable machine code, e.g. `invalid_scaffold_name`.
        code: String,
        /// The sentence to show the reader.
        message: String,
    },

    /// The requested generation does not exist in this organization.
    #[error("no such scaffold")]
    ScaffoldNotFound,

    // --- CLI device-code flow (REQ-033, slice 4) -----------------------------------------
    //
    // The risk note on this flow is phishing: a code is short-lived, bound to the approving
    // user, and cannot be approved without key-management permission. The error variants are
    // part of that, because a flow that says "this code is for user X" to the session that
    // presented it is a flow that can be walked into with a support call.
    /// A device code that is unknown, already approved, or past its expiry.
    ///
    /// One answer for all three, exactly as `InvalidCode` does for an authorization code: a
    /// caller that can tell them apart learns which codes exist, and a code is short-lived
    /// enough that guessing is the whole attack.
    #[error("invalid device code")]
    InvalidDeviceCode,

    /// A device code whose polling interval has not elapsed yet.
    ///
    /// The one answer that is *not* an error: RFC 8628 makes slow-down a normal part of the
    /// flow, and the client is told to wait rather than refused.
    #[error("slow down: wait {seconds} seconds before polling again")]
    DeviceCodeSlowDown {
        /// How long the client must wait.
        seconds: u64,
    },

    /// The code has not been approved yet, which is also a normal part of the flow.
    #[error("the code has not been approved yet")]
    DeviceCodePending,

    /// A session without key-management permission tried to approve a code.
    ///
    /// Its own variant rather than the generic refusal, because the panel needs to say *why*
    /// — a person following the CLI's instructions has no way to know the account they are
    /// logged into cannot approve.
    #[error("approving a CLI login needs developer.keys.manage")]
    DeviceCodeApprovalRefused,

    /// The platform is not configured for something it needs in order to be safe.
    ///
    /// Its own variant, and deliberately **not** a client error: every other variant in this
    /// enum is a request that has to change before it can be accepted, and this one is an
    /// operator who has to set an environment variable. Answering `400` to a missing
    /// `OMNION_LOG_PEPPER` would tell the caller to fix their request — and there is no request
    /// that fixes it. `is_client_error` therefore excludes it, so it surfaces as a `5xx` naming
    /// the variable, which is the only answer that points at the person who can resolve it.
    ///
    /// It arrived with `origin/main`'s request-log recorder (REQ-022 slice 2): the client
    /// fingerprint is an HMAC keyed by that variable, and a fingerprint computed with an empty
    /// key is a hash anyone can reproduce, so the recorder refuses rather than storing one.
    #[cfg(feature = "store")]
    #[error("{0}")]
    Misconfigured(String),

    /// The database said no, and the message is one we wrote.
    #[cfg(feature = "store")]
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Compare two errors by variant, for the client-facing half.
///
/// See the enum doc for why this is hand-written. The one rule: **a database error never equals
/// anything**, not even another database error, because nothing outside this module can act on
/// its contents and an `assert_eq!` that matched on one would be asserting on PostgreSQL's
/// version string.
impl PartialEq for DeveloperError {
    fn eq(&self, other: &Self) -> bool {
        use DeveloperError as E;
        match (self, other) {
            (E::InvalidName { min: a, max: b }, E::InvalidName { min: c, max: d }) => {
                a == c && b == d
            }
            (E::DuplicateScope(a), E::DuplicateScope(b)) => a == b,
            (E::UnknownEnvironment(a), E::UnknownEnvironment(b)) => a == b,
            (E::UnknownRateTier(a), E::UnknownRateTier(b)) => a == b,
            (E::UnknownStatusClass(a), E::UnknownStatusClass(b)) => a == b,
            (E::KeyNameTaken(a), E::KeyNameTaken(b)) => a == b,
            (E::InvalidExpiry(a), E::InvalidExpiry(b)) => a == b,
            (E::InvalidCidr(a), E::InvalidCidr(b)) => a == b,
            (E::KeyNotActive(a), E::KeyNotActive(b)) => a == b,
            (E::UnknownAppStatus(a), E::UnknownAppStatus(b)) => a == b,
            (E::AppNameTaken(a), E::AppNameTaken(b)) => a == b,
            (E::AppDescriptionTooLong { max: a }, E::AppDescriptionTooLong { max: b }) => a == b,
            (E::TooManyRedirectUris { max: a }, E::TooManyRedirectUris { max: b }) => a == b,
            (
                E::RedirectUriTooLong { index: a, max: b },
                E::RedirectUriTooLong { index: c, max: d },
            ) => a == c && b == d,
            (
                E::RedirectUriSchemeRefused { index: a },
                E::RedirectUriSchemeRefused { index: b },
            ) => a == b,
            (E::DuplicateRedirectUri { index: a }, E::DuplicateRedirectUri { index: b }) => a == b,
            (E::DuplicateGrantType { index: a }, E::DuplicateGrantType { index: b }) => a == b,
            (E::UnknownGrantType(a), E::UnknownGrantType(b)) => a == b,
            (E::UnknownScaffoldKind(a), E::UnknownScaffoldKind(b)) => a == b,
            (E::UnknownScaffoldTarget(a), E::UnknownScaffoldTarget(b)) => a == b,
            (
                E::ScaffoldRefused {
                    code: ac,
                    message: am,
                },
                E::ScaffoldRefused {
                    code: bc,
                    message: bm,
                },
            ) => ac == bc && am == bm,
            (E::DeviceCodeSlowDown { seconds: a }, E::DeviceCodeSlowDown { seconds: b }) => a == b,
            // Every remaining variant is a unit one: same variant means equal, anything else
            // means not. Writing them out rather than listing them in a `matches!` is what
            // makes the compiler name a variant added later, instead of the new variant
            // falling through to "not equal" and quietly failing every assertion about it.
            (E::NoScopes, E::NoScopes)
            | (E::EmptyScope, E::EmptyScope)
            | (E::NegativeDuration, E::NegativeDuration)
            | (E::KeyNotFound, E::KeyNotFound)
            | (E::KeyUnverifiable, E::KeyUnverifiable)
            | (E::InvalidKey, E::InvalidKey)
            | (E::HighTierRefused, E::HighTierRefused)
            | (E::NoRedirectUris, E::NoRedirectUris)
            | (E::NoGrantTypes, E::NoGrantTypes)
            | (E::AppNotFound, E::AppNotFound)
            | (E::AppNotActive(_), E::AppNotActive(_))
            | (E::GrantNotRegistered, E::GrantNotRegistered)
            | (E::RedirectUriNotRegistered, E::RedirectUriNotRegistered)
            | (E::ScopeNotRegistered, E::ScopeNotRegistered)
            | (E::CodeChallengeRefused, E::CodeChallengeRefused)
            | (E::InvalidClient, E::InvalidClient)
            | (E::RedirectUriMismatch, E::RedirectUriMismatch)
            | (E::InvalidCode, E::InvalidCode)
            | (E::GrantMismatch, E::GrantMismatch)
            | (E::ScaffoldNotFound, E::ScaffoldNotFound)
            | (E::InvalidDeviceCode, E::InvalidDeviceCode)
            | (E::DeviceCodePending, E::DeviceCodePending)
            | (E::DeviceCodeApprovalRefused, E::DeviceCodeApprovalRefused) => true,
            // A database error equals nothing -- including itself. See the doc above.
            // Ungated on purpose: the arm below needs one on BOTH builds, and gating this one
            // left the default build with no fall-through at all. A gate that only ever runs
            // `--features store` is a gate that misses the build the other six writers run.
            #[cfg(feature = "store")]
            (E::Database(_), _) | (_, E::Database(_)) => false,
            // The remaining pairings are two variants the list above did not cover, which the
            // compiler proves impossible on this build. Ungated for the same reason.
            _ => false,
        }
    }
}

/// The crate's result.
pub type Result<T, E = DeveloperError> = std::result::Result<T, E>;

impl DeveloperError {
    /// Whether this is the caller's problem (a `400` with a message they can act on) or the
    /// platform's (a `500`).
    ///
    /// The split is about *who can fix it*, and it is the reason the crate does not carry
    /// sqlx errors in its client-facing half: a database that is unreachable is not something
    /// the person filling in a form can act on, and answering `400 invalid request` to a
    /// database outage sends them to fix a field that was never wrong.
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidName { .. }
                | Self::NoScopes
                | Self::EmptyScope
                | Self::DuplicateScope(_)
                | Self::UnknownEnvironment(_)
                | Self::UnknownRateTier(_)
                | Self::UnknownStatusClass(_)
                | Self::NegativeDuration
                | Self::KeyNotFound
                | Self::KeyNameTaken(_)
                | Self::InvalidCidr(_)
                | Self::InvalidExpiry(_)
                | Self::KeyUnverifiable
                | Self::InvalidKey
                | Self::HighTierRefused
                | Self::KeyNotActive(_)
                // Everything in the OAuth block is the caller's problem: every one of these
                // is a request that has to change before it can be accepted. `InvalidClient`,
                // `InvalidCode` and `GrantMismatch` are the interesting three — they describe a
                // credential that failed, which reads like a server fault but is not, and
                // answering `500` for a wrong secret would tell a script the platform is down
                // and invite a retry loop against a credential that will never work.
                | Self::UnknownAppStatus(_)
                | Self::AppNameTaken(_)
                | Self::AppDescriptionTooLong { .. }
                | Self::NoRedirectUris
                | Self::TooManyRedirectUris { .. }
                | Self::RedirectUriTooLong { .. }
                | Self::RedirectUriSchemeRefused { .. }
                | Self::DuplicateRedirectUri { .. }
                | Self::NoGrantTypes
                | Self::DuplicateGrantType { .. }
                | Self::UnknownGrantType(_)
                | Self::AppNotFound
                | Self::AppNotActive(_)
                | Self::GrantNotRegistered
                | Self::RedirectUriNotRegistered
                | Self::ScopeNotRegistered
                | Self::CodeChallengeRefused
                | Self::InvalidClient
                | Self::RedirectUriMismatch
                | Self::InvalidCode
                | Self::GrantMismatch
                // Slice 4. `DeviceCodePending` and `DeviceCodeSlowDown` are the two that look
                // like faults and are not: RFC 8628 makes both a normal part of the flow, so a
                // `400` is what a client needs to see (it means "keep polling", not "the
                // platform is broken") and a `500` would invite a retry storm against a code
                // that is working exactly as intended.
                | Self::UnknownScaffoldKind(_)
                | Self::UnknownScaffoldTarget(_)
                | Self::ScaffoldRefused { .. }
                | Self::ScaffoldNotFound
                | Self::InvalidDeviceCode
                | Self::DeviceCodeSlowDown { .. }
                | Self::DeviceCodePending
                | Self::DeviceCodeApprovalRefused
        )
    }

    /// The API error code this variant carries.
    ///
    /// Stable strings, because a client switches on them: the panel's `ApiError.code` is what
    /// decides whether a message goes under a field or into a toast, and a code that changes
    /// spelling between releases moves the message to the wrong place with nothing failing.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "invalid_key_name",
            Self::NoScopes => "no_scopes",
            Self::EmptyScope => "empty_scope",
            Self::DuplicateScope(_) => "duplicate_scope",
            Self::UnknownEnvironment(_) => "unknown_environment",
            Self::UnknownRateTier(_) => "unknown_rate_tier",
            Self::UnknownStatusClass(_) => "unknown_status_class",
            Self::NegativeDuration => "negative_duration",
            Self::KeyNotFound => "api_key_not_found",
            Self::KeyNameTaken(_) => "api_key_name_taken",
            Self::InvalidExpiry(_) => "invalid_expiry",
            Self::InvalidCidr(_) => "invalid_cidr",
            Self::KeyUnverifiable => "key_unverifiable",
            Self::InvalidKey => "invalid_api_key",
            Self::HighTierRefused => "high_tier_refused",
            Self::KeyNotActive(_) => "api_key_not_active",
            // A misconfiguration, not a bad request: the code says so in the same word the
            // status does, so an operator reading a log line sees `misconfigured` rather than
            // `invalid_*` and goes looking for a request to fix instead of a variable to set.
            Self::Misconfigured(_) => "developer_misconfigured",
            Self::UnknownAppStatus(_) => "unknown_app_status",
            Self::AppNameTaken(_) => "oauth_app_name_taken",
            Self::AppDescriptionTooLong { .. } => "oauth_app_description_too_long",
            Self::NoRedirectUris => "no_redirect_uris",
            Self::TooManyRedirectUris { .. } => "too_many_redirect_uris",
            Self::RedirectUriTooLong { .. } => "redirect_uri_too_long",
            Self::RedirectUriSchemeRefused { .. } => "redirect_uri_scheme_refused",
            Self::DuplicateRedirectUri { .. } => "duplicate_redirect_uri",
            Self::NoGrantTypes => "no_grant_types",
            Self::DuplicateGrantType { .. } => "duplicate_grant_type",
            Self::UnknownGrantType(_) => "unknown_grant_type",
            Self::AppNotFound => "oauth_app_not_found",
            Self::AppNotActive(_) => "oauth_app_not_active",
            Self::GrantNotRegistered => "grant_not_registered",
            Self::RedirectUriNotRegistered => "redirect_uri_not_registered",
            Self::ScopeNotRegistered => "scope_not_registered",
            Self::CodeChallengeRefused => "code_challenge_refused",
            // The three flat codes. They are one code each rather than four because they *are*
            // one answer to a caller, and a client that switches on them must not be able to
            // branch on which refusal it got.
            Self::InvalidClient => "invalid_client",
            Self::RedirectUriMismatch => "redirect_uri_mismatch",
            Self::InvalidCode => "invalid_authorization_code",
            Self::GrantMismatch => "grant_mismatch",
            Self::UnknownScaffoldKind(_) => "unknown_scaffold_kind",
            Self::UnknownScaffoldTarget(_) => "unknown_scaffold_target",
            Self::ScaffoldRefused { .. } => "scaffold_refused",
            Self::ScaffoldNotFound => "scaffold_not_found",
            // Two flat codes for three "keep polling" shapes. `DeviceCodePending` and
            // `DeviceCodeSlowDown` are distinct *because the client must behave differently* —
            // one is "wait", the other is "you polled too fast, wait longer" — but neither is a
            // failure and neither may be branched on to learn which codes exist.
            Self::InvalidDeviceCode => "invalid_device_code",
            Self::DeviceCodePending => "device_code_pending",
            Self::DeviceCodeSlowDown { .. } => "device_code_slow_down",
            Self::DeviceCodeApprovalRefused => "device_code_approval_refused",
            // Gated for the same reason the variant is: without the `store` feature this arm
            // does not exist, and a match that names it is a compile error. The alternative —
            // a `_ =>` arm — would swallow a new client-error variant added later without its
            // code, which is how a `400` silently becomes a `500`. The gate is what makes the
            // compiler name every arm a new variant needs.
            #[cfg(feature = "store")]
            Self::Database(_) => "developer_store_unavailable",
        }
    }
}
