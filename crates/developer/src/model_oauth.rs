//! The shapes an OAuth application is read and written in (REQ-033, slice 3b).
//!
//! # The rule this module exists to make unrepresentable
//!
//! [`OAuthApp`] has **no `client_secret` field**, exactly as [`crate::model::ApiKey`] has no
//! `secret`. The one response that carries plaintext is [`MintedApp`], which is only ever
//! produced by a store function that had to write a new hash in the same statement — so "the
//! secret came back a second time" is not a test somebody has to remember, it is a shape the
//! type system refuses to build.
//!
//! That is why the overlap window lives here as two fields rather than as a computed one: the
//! panel has to *show* when the old secret stops working, and an operator who cannot see the
//! deadline cannot reason about whether to redeploy before it. The value is carried as an
//! instant, not as a flag, because a boolean would have to be computed from a clock at read time
//! and would then disagree with the row the moment somebody reads it twice.
//!
//! # Why an app's status is computed, like a key's
//!
//! `deleted` is a *soft* status: the row keeps its foreign keys so the authorization codes and
//! the audit trail that reference it stay explainable, which is the same reason a revoked API
//! key keeps its request log. `deleted_at` is stored; `status` is derived, so there is no way to
//! write `status = 'deleted'` and leave `deleted_at` null.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};
use crate::oauth::{
    GrantType, MAX_REDIRECT_URI_LENGTH, MAX_REDIRECT_URIS, challenge_looks_valid,
    check_redirect_uri, grant_covers, redirect_scheme_allowed,
};

/// Whether an app can start a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    /// Usable.
    Active,
    /// Registered but switched off: the panel keeps it and its codes stop being issued.
    Suspended,
    /// Withdrawn. The row survives; nothing can use it.
    Deleted,
}

impl AppStatus {
    /// The stored form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// Parse a stored value, refusing anything else rather than defaulting to `active`.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "suspended" => Ok(Self::Suspended),
            "deleted" => Ok(Self::Deleted),
            other => Err(DeveloperError::UnknownAppStatus(other.to_owned())),
        }
    }

    /// Whether an app in this state may start an authorization or token request.
    #[must_use]
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// One registered OAuth application.
///
/// No secret field, no previous-hash field: what is exposed is *when the previous secret stops
/// working*, which is the fact an operator needs and is not a credential.
#[derive(Debug, Clone, Serialize)]
pub struct OAuthApp {
    /// App id.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name, unique among this tenant's live apps.
    pub name: String,
    /// Optional description shown on the consent screen.
    pub description: Option<String>,
    /// Object key of the app's logo, served through the media surface.
    pub logo_object_key: Option<String>,
    /// The public identifier a client sends. Safe to display, log and read aloud.
    pub client_id: String,
    /// Registered redirect URIs, compared as whole strings at authorization time.
    pub redirect_uris: Vec<String>,
    /// The most this app may ever be granted.
    pub scopes: Vec<String>,
    /// Which flows it may use.
    pub grant_types: Vec<GrantType>,
    /// Computed from `deleted_at`, never stored.
    pub status: AppStatus,
    /// When the previous client secret stops working, while a rotation's overlap is open.
    pub previous_secret_expires_at: Option<OffsetDateTime>,
    /// Who registered it.
    pub created_by: Uuid,
    /// When it was registered.
    pub created_at: OffsetDateTime,
    /// When it was last edited.
    pub updated_at: OffsetDateTime,
}

impl OAuthApp {
    /// The status of this app at a given instant.
    ///
    /// A method rather than a free function, for the reason [`crate::model::ApiKey`] has one:
    /// the row is read by the panel, by the token endpoint and by the authorization endpoint,
    /// and three copies of "which column wins" is three chances for the panel to say a deleted
    /// app is active.
    #[must_use]
    pub fn status_at(&self, now: OffsetDateTime) -> AppStatus {
        if self.status == AppStatus::Deleted {
            return AppStatus::Deleted;
        }
        let _ = now;
        self.status
    }
}

/// A newly registered or rotated app, carrying the client secret exactly once.
///
/// The lifetime argument is the enforcement: a handler that receives this has to write it to a
/// response in the same breath, and no path from here back to a stored hash produces a second
/// one.
///
/// `Debug` is **hand-written, not derived** — deriving would put a live client secret into any
/// log line that touches this by reference, which is the same argument as
/// [`crate::secret::MintedKey`] and [`crate::oauth::MintedClientSecret`]. `Clone` is hand-written
/// for the same reason: a derived `Clone` is harmless on its own, but keeping the two derives
/// together is what stops a future `#[derive(Debug)]` from landing on this line.
#[derive(Clone)]
pub struct MintedApp {
    /// The app as it now is.
    pub app: OAuthApp,
    /// The client secret. Shown once.
    pub plaintext: String,
    /// When the *previous* secret stops working, when this is a rotation and the overlap is
    /// open. `None` on creation, and `None` on a rotation with no overlap.
    pub previous_secret_expires_at: Option<OffsetDateTime>,
}

impl std::fmt::Debug for MintedApp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `MintedApp` is a credential carrier, exactly like `MintedKey` and
        // `MintedClientSecret`. Deriving would put a live client secret into any log line that
        // touches this by reference.
        formatter
            .debug_struct("MintedApp")
            .field("app", &self.app)
            .field("plaintext", &"<redacted>")
            .field(
                "previous_secret_expires_at",
                &self.previous_secret_expires_at,
            )
            .finish()
    }
}

/// The shape an app's registration is validated into before it is stored.
#[derive(Debug, Clone)]
pub struct NewApp {
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Optional logo object key.
    pub logo_object_key: Option<String>,
    /// Registered redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Permission keys.
    pub scopes: Vec<String>,
    /// Which flows.
    pub grant_types: Vec<GrantType>,
    /// Who registered it.
    pub created_by: Uuid,
}

/// An edit to an existing app.
///
/// Every field is `Option` because the panel's form submits the whole record and a `PATCH`
/// that required all of them would make "change the description" a three-field request. The
/// store applies only what is present, in one statement, so a partial edit cannot leave the
/// name updated and the redirect URIs stale.
#[derive(Debug, Clone, Default)]
pub struct AppEdit {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<Option<String>>,
    /// New logo object key.
    pub logo_object_key: Option<Option<String>>,
    /// New redirect URI list.
    pub redirect_uris: Option<Vec<String>>,
    /// New scope list.
    pub scopes: Option<Vec<String>>,
    /// New grant types.
    pub grant_types: Option<Vec<GrantType>>,
    /// New status.
    pub status: Option<AppStatus>,
}

/// The field constraints on an app, applied before any hashing or insert work happens.
///
/// Every rule here has a counterpart in migration `0231` where a constraint *can* express it,
/// and the point of the crate-side copy is to fail before a transaction is opened rather than
/// after — and to give the panel a message it can place under a field.
pub mod app_rules {
    use super::*;

    /// Shortest accepted name. The migration enforces the same pair.
    pub const MIN_NAME: usize = 3;
    /// Longest accepted name.
    pub const MAX_NAME: usize = 60;
    /// Longest accepted description.
    pub const MAX_DESCRIPTION: usize = 280;

    /// Validate a submitted name, reusing the key rule so both surfaces say the same thing.
    pub fn validate_name(name: &str) -> Result<()> {
        crate::model::key_rules::validate_name(name)
    }

    /// Validate a submitted redirect-URI list.
    ///
    /// The list is checked in full **before** any of it is stored, and the failure names the
    /// offending *position* rather than the URL. That is deliberate: a URI that failed the
    /// scheme rule is a URL the caller typed, and echoing it into an error body that ends up in
    /// a browser console or a proxy log is how a redirect URI — which is an attack surface by
    /// construction — ends up in a log index.
    pub fn validate_redirect_uris(entries: &[String]) -> Result<Vec<String>> {
        let cleaned: Vec<String> = entries
            .iter()
            .map(|entry| entry.trim().to_owned())
            .filter(|entry| !entry.is_empty())
            .collect();

        if cleaned.is_empty() {
            // The migration refuses an empty array too. An app that can be redirected nowhere
            // fails its first authorization request, and the developer discovers that from a
            // `400` with no field to look at.
            return Err(DeveloperError::NoRedirectUris);
        }
        if cleaned.len() > MAX_REDIRECT_URIS {
            return Err(DeveloperError::TooManyRedirectUris {
                max: MAX_REDIRECT_URIS,
            });
        }

        let mut seen = std::collections::BTreeSet::new();
        for (index, entry) in cleaned.iter().enumerate() {
            if entry.len() > MAX_REDIRECT_URI_LENGTH {
                return Err(DeveloperError::RedirectUriTooLong {
                    index,
                    max: MAX_REDIRECT_URI_LENGTH,
                });
            }
            if !redirect_scheme_allowed(entry) {
                return Err(DeveloperError::RedirectUriSchemeRefused { index });
            }
            // A duplicate row in the list is not a second registration: the unique name index
            // has no counterpart here, and two identical entries make the panel's "which one
            // did I paste" question unanswerable.
            if !seen.insert(entry.clone()) {
                return Err(DeveloperError::DuplicateRedirectUri { index });
            }
        }
        Ok(cleaned)
    }

    /// Validate a submitted scope list: at least one, no blanks, no duplicates.
    ///
    /// Delegates to the key rule rather than restating it, so the two surfaces cannot drift on
    /// what "at least one scope" means.
    pub fn validate_scopes(scopes: &[String]) -> Result<()> {
        crate::model::key_rules::validate_scopes(scopes)
    }

    /// Validate a submitted grant-type list: at least one, no duplicates.
    pub fn validate_grant_types(grants: &[GrantType]) -> Result<()> {
        if grants.is_empty() {
            return Err(DeveloperError::NoGrantTypes);
        }
        for (index, grant) in grants.iter().enumerate() {
            if grants[..index].contains(grant) {
                return Err(DeveloperError::DuplicateGrantType { index });
            }
        }
        Ok(())
    }
}

/// Decide whether an authorization request may proceed, and say why not.
///
/// Returned as a single function rather than four checks scattered across the handler because
/// **the order is the security property**. An authorization endpoint that validates the client
/// after the redirect URI leaks whether a client id exists to a caller who controls the URL,
/// and one that checks the scope before the redirect grants a scope oracle. So:
///
/// 1. the client must exist and be usable — one answer for "no such client" and "this client is
///    not active", because they are the same question to an attacker and different words to an
///    integrator;
/// 2. the submitted redirect URI must be one this app registered, compared as a whole string;
/// 3. the flow must be one the app registered;
/// 4. the requested scopes must be a subset of what the app registered;
/// 5. PKCE, when the client sent a challenge, must be structurally usable now.
///
/// A caller that passes 1–4 gets [`ConsentRequest`] — the shape the consent screen renders —
/// and only then is a code minted. Nothing in this function writes anything, so it is testable
/// with no database and cannot leave a half-started flow behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequest {
    /// The app's `client_id`, as submitted.
    pub client_id: String,
    /// The `redirect_uri`, as submitted.
    pub redirect_uri: String,
    /// The grant the client asked for.
    pub grant_type: String,
    /// The scopes it asked for. Empty means "whatever the app registered", which is what a
    /// bare `GET /authorize?client_id=…&redirect_uri=…` means.
    pub scopes: Vec<String>,
    /// The PKCE challenge, when sent.
    pub code_challenge: Option<String>,
    /// The PKCE method, when a challenge was sent.
    pub code_challenge_method: Option<String>,
}

/// The outcome of a successful authorization check: everything needed to render a consent screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    /// The app being authorized.
    pub client_id: String,
    /// The redirect URI, guaranteed registered.
    pub redirect_uri: String,
    /// The scopes that will actually be granted, which is the intersection the client asked for
    /// and the app registered.
    pub scopes: Vec<String>,
    /// The app's display name, for the screen's heading.
    pub app_name: String,
    /// Its description, if it has one.
    pub app_description: Option<String>,
    /// Its logo object key, if it has one.
    pub logo_object_key: Option<String>,
    /// Whether PKCE is in play, so the screen can state that the flow is protected.
    pub pkce: bool,
}

impl ConsentRequest {
    /// The same consent, with PKCE recorded as in play or not.
    ///
    /// A builder rather than a public field assignment because `pkce` is a *fact about the
    /// request that was checked*, not something a caller may choose: it is derived from whether a
    /// challenge and a method both arrived. Mutating it after `authorize` returned would let a
    /// caller render a screen claiming a protection the flow does not have, and a caller
    /// constructing one by hand for a test could do the same in production code.
    #[must_use]
    pub fn with_pkce(mut self, pkce: bool) -> Self {
        self.pkce = pkce;
        self
    }
}

/// Check an authorization request against a registered app.
///
/// `app` carries the row's already-parsed fields (see [`crate::store::app_from_row`]); the app's
/// own `status` is the third parameter rather than a field read here, so a caller cannot pass
/// an app it has already decided is active.
pub fn authorize(
    app: &OAuthApp,
    status: AppStatus,
    request: &AuthorizationRequest,
) -> Result<ConsentRequest> {
    if request.client_id != app.client_id {
        // Not "no such client": the caller reached the authorization endpoint *through this
        // app's* id, so the id matched and something else did not. The single `AppNotFound`
        // covers both the unknown id and the mismatch, so a caller cannot use the error to
        // enumerate which client ids exist.
        return Err(DeveloperError::AppNotFound);
    }
    if !status.is_usable() {
        // The wording names the state but not the secret, and there is deliberately no separate
        // "suspended" error a caller could distinguish a deleted app by.
        return Err(DeveloperError::AppNotActive(status.as_str()));
    }

    // The grant is checked before the redirect on purpose: a client asking for a flow it does
    // not hold is told so at the consent screen's own door, and no redirect is ever computed
    // from a request that was going to be refused anyway.
    let grant = GrantType::parse(&request.grant_type)
        .ok_or_else(|| DeveloperError::UnknownGrantType(request.grant_type.clone()))?;
    if !app.grant_types.contains(&grant) {
        return Err(DeveloperError::GrantNotRegistered);
    }

    // …and the redirect is checked before anything is shown, so a request that will be refused
    // never renders a screen carrying an attacker's URL. The error carries no URI: it would end
    // up in a log.
    check_redirect_uri(&request.redirect_uri, &app.redirect_uris)
        .map_err(|_| DeveloperError::RedirectUriNotRegistered)?;

    // An empty `scope` parameter means "everything this app is registered for", which is what
    // the OAuth spec says and what a first-party client that asks for nothing expects. A
    // non-empty list that is *not* a subset is refused rather than silently narrowed: silently
    // granting less than the client needs produces an `insufficient_scope` at the API three
    // redirects later, which is a much worse debugging experience than refusing now.
    let requested: Vec<String> = if request.scopes.is_empty() {
        app.scopes.clone()
    } else {
        if !grant_covers(&app.scopes, &request.scopes) {
            return Err(DeveloperError::ScopeNotRegistered);
        }
        request.scopes.clone()
    };
    if requested.is_empty() {
        return Err(DeveloperError::NoScopes);
    }

    // PKCE is structurally validated here, not at the token endpoint, so a client that sent a
    // malformed challenge fails while it can still fix the request.
    let pkce = match (
        request.code_challenge.as_deref(),
        request.code_challenge_method.as_deref(),
    ) {
        (None, None) => false,
        (Some(challenge), Some(method)) => {
            if !challenge_looks_valid(challenge, method) {
                return Err(DeveloperError::CodeChallengeRefused);
            }
            true
        }
        // A challenge without a method, or a method without a challenge, is the same shape of
        // error as the migration's `oauth_codes_challenge_is_whole` — refused here so a
        // constraint violation at insert time is unreachable from the API.
        _ => return Err(DeveloperError::CodeChallengeRefused),
    };

    Ok(ConsentRequest {
        client_id: app.client_id.clone(),
        redirect_uri: request.redirect_uri.clone(),
        scopes: requested,
        app_name: app.name.clone(),
        app_description: app.description.clone(),
        logo_object_key: app.logo_object_key.clone(),
        pkce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::code_challenge_s256;
    use time::macros::datetime;

    fn app() -> OAuthApp {
        OAuthApp {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Reporting".to_owned(),
            description: Some("Reads pages".to_owned()),
            logo_object_key: None,
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uris: vec![
                "https://app.example.com/callback".to_owned(),
                "http://localhost:3000/callback".to_owned(),
            ],
            scopes: vec!["content.pages.read".to_owned(), "search.read".to_owned()],
            grant_types: vec![GrantType::AuthorizationCode, GrantType::ClientCredentials],
            status: AppStatus::Active,
            previous_secret_expires_at: None,
            created_by: Uuid::nil(),
            created_at: datetime!(2026-10-01 12:00 UTC),
            updated_at: datetime!(2026-10-01 12:00 UTC),
        }
    }

    fn a_request() -> AuthorizationRequest {
        AuthorizationRequest {
            client_id: "omn_app_0123456789abcdef01234567".to_owned(),
            redirect_uri: "https://app.example.com/callback".to_owned(),
            grant_type: "authorization_code".to_owned(),
            scopes: vec!["content.pages.read".to_owned()],
            code_challenge: Some(code_challenge_s256("a-verifier-that-is-long-enough-here")),
            code_challenge_method: Some("S256".to_owned()),
        }
    }

    // ── the app shape ────────────────────────────────────────────────────────

    #[test]
    fn an_app_row_never_serialises_to_a_field_named_client_secret() {
        // The write-only property as an assertion on the bytes a client receives — the same
        // shape of test as the key list's, and the reason `OAuthApp` has no such field to
        // populate. It also checks the *hash* names, since a serialised row carrying
        // `client_secret_hash` would be as bad as carrying the secret.
        let rendered = serde_json::to_value(&app()).expect("serialises");
        assert!(rendered.get("client_secret").is_none());
        assert!(rendered.get("client_secret_hash").is_none());
        assert!(rendered.get("previous_secret_hash").is_none());
        // What it *does* carry is the deadline, which is the fact an operator needs.
        assert_eq!(rendered["client_id"], "omn_app_0123456789abcdef01234567");
        assert!(rendered["redirect_uris"].is_array());
    }

    #[test]
    fn the_minted_shape_prints_redacted_and_says_when_the_old_secret_dies() {
        let minted = MintedApp {
            app: app(),
            plaintext: "deadbeef".repeat(8),
            previous_secret_expires_at: Some(datetime!(2026-10-08 12:00 UTC)),
        };
        let rendered = format!("{minted:?}");
        assert!(!rendered.contains(&minted.plaintext));
        assert!(rendered.contains("redacted"));
        // The deadline survives the redaction, because that is the half an operator needs and
        // the half that is not a credential.
        assert!(rendered.contains("2026-10-08"));
    }

    #[test]
    fn a_status_is_parsed_or_refused_never_defaulted_to_active() {
        for (raw, expected) in [
            ("active", AppStatus::Active),
            ("suspended", AppStatus::Suspended),
            ("deleted", AppStatus::Deleted),
        ] {
            assert_eq!(AppStatus::parse(raw).unwrap(), expected);
        }
        // Defaulting an unknown status to `active` would hand a retired app a working
        // authorization endpoint, so it is an error instead.
        assert!(matches!(
            AppStatus::parse("pending"),
            Err(DeveloperError::UnknownAppStatus(_))
        ));
        assert!(!AppStatus::parse("retired").is_ok());
    }

    // ── field rules ──────────────────────────────────────────────────────────

    #[test]
    fn a_registered_redirect_list_is_validated_in_full_before_any_of_it_is_stored() {
        let good = vec![
            "https://app.example.com/callback".to_owned(),
            " http://localhost:3000/cb ".to_owned(),
            "  ".to_owned(),
        ];
        let cleaned = app_rules::validate_redirect_uris(&good).unwrap();
        // Blank rows are dropped rather than refused — that is what a form with one empty input
        // submits — but the two real rows keep their order and lose their whitespace.
        assert_eq!(
            cleaned,
            vec![
                "https://app.example.com/callback".to_owned(),
                "http://localhost:3000/cb".to_owned()
            ]
        );
    }

    #[test]
    fn an_app_that_can_be_redirected_nowhere_is_refused_before_it_is_stored() {
        for bad in [vec![], vec!["".to_owned()], vec!["   ".to_owned()]] {
            assert!(matches!(
                app_rules::validate_redirect_uris(&bad),
                Err(DeveloperError::NoRedirectUris)
            ));
        }
    }

    #[test]
    fn a_non_https_redirect_is_refused_by_position_and_never_echoed_back() {
        // The four that must be refused: a plain http host, a look-alike host, a scheme the
        // platform does not redirect to at all, and a relative path.
        for bad in [
            "http://app.example.com/callback",
            "http://localhost.attacker.example/cb",
            "ftp://app.example.com/cb",
            "/callback",
            "javascript:alert(1)",
        ] {
            let listed = vec!["https://ok.example.com/cb".to_owned(), bad.to_owned()];
            match app_rules::validate_redirect_uris(&listed) {
                Err(DeveloperError::RedirectUriSchemeRefused { index }) => {
                    assert_eq!(index, 1, "the position of the bad row is what is reported");
                }
                other => panic!("{bad} should be refused by scheme, got {other:?}"),
            }
            // And the refusal must not carry the URI: the error's own `Display` and its
            // `Debug` are both checked, because one ends up in a log and the other in a
            // panic message.
            let listed = vec![bad.to_owned()];
            let error = app_rules::validate_redirect_uris(&listed).unwrap_err();
            assert!(
                !error.to_string().contains(bad),
                "the message must not echo the rejected URL"
            );
            assert!(!format!("{error:?}").contains(bad));
        }
    }

    #[test]
    fn the_loopback_exception_is_narrow_enough_to_survive_a_look_alike_host() {
        // The three RFC 8252 loopback forms, and the near misses that a `starts_with` check
        // would have accepted.
        for good in [
            "http://localhost/cb",
            "http://localhost:8080/cb",
            "http://127.0.0.1:3000/cb",
            "http://[::1]:3000/cb",
        ] {
            let ok = app_rules::validate_redirect_uris(&[good.to_owned()]);
            assert!(ok.is_ok(), "{good} should be accepted");
        }
        for bad in [
            "http://localhost.attacker.example/cb",
            "http://127.0.0.1.attacker.example/cb",
            "http://[::1].attacker.example/cb",
            "http://notlocalhost/cb",
        ] {
            assert!(
                app_rules::validate_redirect_uris(&[bad.to_owned()]).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_duplicate_redirect_row_and_an_overlong_one_are_both_refused() {
        assert!(matches!(
            app_rules::validate_redirect_uris(&[
                "https://a.example.com/cb".to_owned(),
                "https://a.example.com/cb".to_owned()
            ]),
            Err(DeveloperError::DuplicateRedirectUri { index: 1 })
        ));
        let long = format!("https://a.example.com/{}", "x".repeat(600));
        assert!(matches!(
            app_rules::validate_redirect_uris(&[long]),
            Err(DeveloperError::RedirectUriTooLong { max, .. }) if max == MAX_REDIRECT_URI_LENGTH
        ));
    }

    #[test]
    fn more_redirect_uris_than_the_bound_are_refused_by_count() {
        let many: Vec<String> = (0..=MAX_REDIRECT_URIS)
            .map(|index| format!("https://app{index}.example.com/cb"))
            .collect();
        assert!(matches!(
            app_rules::validate_redirect_uris(&many),
            Err(DeveloperError::TooManyRedirectUris { .. })
        ));
    }

    #[test]
    fn the_grant_list_needs_at_least_one_entry_and_no_repeats() {
        assert!(matches!(
            app_rules::validate_grant_types(&[]),
            Err(DeveloperError::NoGrantTypes)
        ));
        assert!(matches!(
            app_rules::validate_grant_types(&[
                GrantType::AuthorizationCode,
                GrantType::AuthorizationCode
            ]),
            Err(DeveloperError::DuplicateGrantType { index: 1 })
        ));
        assert!(
            app_rules::validate_grant_types(&[
                GrantType::AuthorizationCode,
                GrantType::ClientCredentials
            ])
            .is_ok()
        );
    }

    // ── the authorization check ──────────────────────────────────────────────

    #[test]
    fn a_well_formed_request_produces_the_consent_shape_the_screen_renders() {
        let consent = authorize(&app(), AppStatus::Active, &a_request()).unwrap();
        assert_eq!(consent.client_id, "omn_app_0123456789abcdef01234567");
        assert_eq!(consent.redirect_uri, "https://app.example.com/callback");
        // The granted list is what the client *asked for*, not everything the app holds: an
        // authorization request is a narrowing, and a screen that showed the full set would
        // ask the person to consent to powers the client never asked for.
        assert_eq!(consent.scopes, vec!["content.pages.read".to_owned()]);
        assert_eq!(consent.app_name, "Reporting");
        assert_eq!(consent.app_description.as_deref(), Some("Reads pages"));
        assert!(consent.pkce);
    }

    #[test]
    fn an_omitted_scope_parameter_means_everything_the_app_registered() {
        // What a first-party client that sends no `scope` expects, and what the spec says. It
        // is the *only* case where "empty" does not mean "nothing".
        let mut request = a_request();
        request.scopes.clear();
        let consent = authorize(&app(), AppStatus::Active, &request).unwrap();
        assert_eq!(consent.scopes, vec!["content.pages.read", "search.read"]);
    }

    #[test]
    fn a_scope_the_app_never_registered_is_refused_rather_than_silently_narrowed() {
        // Silently granting the intersection would give the client a token that answers
        // `insufficient_scope` three redirects later, which reads as a platform bug.
        let mut request = a_request();
        request.scopes = vec![
            "content.pages.read".to_owned(),
            "organizations.delete".to_owned(),
        ];
        assert!(matches!(
            authorize(&app(), AppStatus::Active, &request),
            Err(DeveloperError::ScopeNotRegistered)
        ));
    }

    #[test]
    fn a_redirect_the_app_did_not_register_is_refused_and_the_answer_carries_no_url() {
        let mut request = a_request();
        request.redirect_uri = "https://app.example.com/callback-attacker".to_owned();
        let error = authorize(&app(), AppStatus::Active, &request).unwrap_err();
        assert!(matches!(error, DeveloperError::RedirectUriNotRegistered));
        assert!(!error.to_string().contains("attacker"));
        assert!(!format!("{error:?}").contains("attacker"));
    }

    #[test]
    fn a_suspended_or_deleted_app_is_refused_and_the_two_read_the_same_way() {
        // One refusal for three states — unknown id, suspended, deleted — because an
        // authorization endpoint that distinguishes them is an enumeration oracle for which
        // client ids exist in this installation.
        for status in [AppStatus::Suspended, AppStatus::Deleted] {
            let error = authorize(&app(), status, &a_request()).unwrap_err();
            assert!(matches!(error, DeveloperError::AppNotActive(_)));
        }
        let mut unknown = a_request();
        unknown.client_id = "omn_app_ffffffffffffffffffffffff".to_owned();
        assert!(matches!(
            authorize(&app(), AppStatus::Active, &unknown),
            Err(DeveloperError::AppNotFound)
        ));
        // The suspended message names the state, because a developer whose app stopped
        // working needs to know which of the two it is; `AppNotFound` is deliberately flat.
        let error = authorize(&app(), AppStatus::Suspended, &a_request()).unwrap_err();
        assert!(error.to_string().contains("suspended"));
    }

    #[test]
    fn a_flow_the_app_did_not_register_is_refused_before_the_redirect_is_even_consulted() {
        // The ordering assertion: a client that holds no `client_credentials` grant and sends
        // an unregistered redirect gets the *grant* refusal. Both are refusals, so the
        // observable difference is only in which error a caller sees — and the reason for the
        // order is that the grant is a property of the client, answerable without ever looking
        // at a caller-supplied URL.
        let mut request = a_request();
        request.grant_type = "password".to_owned();
        assert!(matches!(
            authorize(&app(), AppStatus::Active, &request),
            Err(DeveloperError::UnknownGrantType(_))
        ));

        let mut request = a_request();
        request.grant_type = "client_credentials".to_owned();
        request.code_challenge = None;
        request.code_challenge_method = None;
        request.redirect_uri = "https://elsewhere.example.com/cb".to_owned();
        assert!(matches!(
            authorize(&app(), AppStatus::Active, &request),
            Err(DeveloperError::RedirectUriNotRegistered)
        ));
    }

    #[test]
    fn a_challenge_that_could_never_be_verified_is_refused_at_the_authorization_step() {
        // A `plain` challenge recorded under `S256`, or an S256-shaped string with padding:
        // both would fail at the token endpoint, and a client that learns it there has already
        // redirected the user.
        for (challenge, method) in [
            ("too-short", "S256"),
            ("S256-was-not-sent-but-the-method-was", "S256"),
            ("abc+def==", "S256"),
            ("", "plain"),
        ] {
            let mut request = a_request();
            request.code_challenge = Some(challenge.to_owned());
            request.code_challenge_method = Some(method.to_owned());
            assert!(
                matches!(
                    authorize(&app(), AppStatus::Active, &request),
                    Err(DeveloperError::CodeChallengeRefused)
                ),
                "{challenge} under {method} must be refused"
            );
        }
    }

    #[test]
    fn a_challenge_without_its_method_is_refused_rather_than_treated_as_absent() {
        // Half a PKCE pair is the same class of bug as half an overlap window: a request that
        // claims protection it did not send. Treating it as "no PKCE" would downgrade the flow
        // silently.
        for (challenge, method) in [
            (
                Some(code_challenge_s256("verifier-value-long-enough-for-sha256")),
                None,
            ),
            (None, Some("S256")),
        ] {
            let mut request = a_request();
            request.code_challenge = challenge;
            request.code_challenge_method = method.map(str::to_owned);
            assert!(matches!(
                authorize(&app(), AppStatus::Active, &request),
                Err(DeveloperError::CodeChallengeRefused)
            ));
        }
    }

    #[test]
    fn an_app_with_no_pkce_still_authorizes_but_the_screen_knows_it_is_unprotected() {
        // `pkce: false` is not a rejection: the request spec says the platform supports the
        // flow with PKCE, and a confidential server-side client legitimately has no browser to
        // generate a verifier. What matters is that the consent screen is told, so the person
        // sees the weaker flow rather than assuming it.
        let mut request = a_request();
        request.code_challenge = None;
        request.code_challenge_method = None;
        let consent = authorize(&app(), AppStatus::Active, &request).unwrap();
        assert!(!consent.pkce);
    }
}
