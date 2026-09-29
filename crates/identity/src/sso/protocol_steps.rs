//! The connection test for the *protocol* kinds — OIDC, generic OAuth2 and SAML.
//!
//! A directory got a step ladder ([`crate::sso::directory`]) because a bind fails at exactly one
//! of six places and "connection failed" tells an operator nothing. The protocol kinds were given
//! one boolean instead, on the argument that "they fail in exactly one place". That argument is
//! what this module refutes: they do not. An OIDC provider can answer discovery from a *different
//! issuer* than the one that was configured, publish no key this platform can verify, or carry an
//! entity id that does not match the assertion it signs — and the registry showed one grey box for
//! three different repairs. Acceptance line 4 asks for the failed check to be *named*; this is the
//! ladder that can name it.
//!
//! The steps are the ones a protocol sign-in actually performs, in the order it performs them:
//! reach the provider → trust the identity → trust the key → read the claims. A failure stops the
//! ladder and leaves the rest `pending`, because a claim read against a key set that was never
//! trusted proves nothing.
//!
//! Nothing here opens a socket. The caller supplies what it fetched, so this stays a decision
//! table — and a decision table is what a test can prove without a network.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{IdentityError, Result};
use crate::sso::oidc::{Discovery, Jwk};
use crate::sso::saml::SamlConfig;

/// One step of a protocol connection test.
///
/// Reuses the directory's vocabulary (`ok` / `failed` / `pending`, one sentence) deliberately: the
/// panel already renders that ladder for directories, and a second shape would mean a second
/// renderer and a screen where the two provider kinds disagree about what a test looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolStep {
    /// The provider answered at all, and answered with something parseable (OIDC discovery).
    Discovery,
    /// The configured signing certificate is a readable key (SAML, which has no discovery).
    Certificate,
    /// The document — or, for SAML, a probe assertion — names the identity that was configured.
    Issuer,
    /// The provider publishes a key this platform can verify.
    KeySet,
    /// The claims the provider sends can be read against the configured wiring.
    Claims,
}

impl ProtocolStep {
    /// The steps an OIDC or OAuth2 sign-in performs, in the order [`test_oidc`] runs them.
    pub const CODE_FLOW: [Self; 4] = [Self::Discovery, Self::Issuer, Self::KeySet, Self::Claims];

    /// The steps a SAML sign-in performs, in the order [`test_saml`] runs them.
    ///
    /// SAML publishes no discovery document, so its first step is the certificate rather than a
    /// fetch. Padding it with a "discovery" row that always passes would make the two ladders
    /// look alike while reporting different things — and a step whose verdict is a constant is a
    /// step nobody reads.
    pub const ASSERTION_FLOW: [Self; 4] =
        [Self::Certificate, Self::Issuer, Self::KeySet, Self::Claims];

    /// The machine name, matching [`crate::sso::directory::TestStep::as_str`]'s shape so the
    /// panel's label table stays one table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Certificate => "certificate",
            Self::Issuer => "issuer",
            Self::KeySet => "key_set",
            Self::Claims => "claims",
        }
    }
}

/// One step's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepReport {
    /// Which step this is.
    pub step: ProtocolStep,
    /// `pending` until the step runs, then `ok` or `failed`.
    pub status: &'static str,
    /// A sentence the panel shows next to the step.
    pub detail: String,
}

impl StepReport {
    fn pending(step: ProtocolStep) -> Self {
        Self {
            step,
            status: "pending",
            detail: String::new(),
        }
    }

    fn done(step: ProtocolStep, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            step,
            status,
            detail: detail.into(),
        }
    }
}

/// What the caller saw when the test ran.
///
/// Same three states as the directory's [`crate::sso::directory::TestOutcome`], and for the same
/// reason: `incomplete` is *not* a pass. A provider whose form is sound but which nothing has
/// asked yet has not been tested, and a registry that calls it "working" has a `Last test`
/// column that lies — which is exactly the lie an enable gate must not be built on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestOutcome {
    /// `ok`, `incomplete` or `failed`.
    pub status: &'static str,
    /// Every step of this protocol's flow, `pending` ones included.
    pub steps: Vec<StepReport>,
    /// The provider's own words, when it answered, for the panel's endpoint box.
    pub endpoints: Option<Value>,
    /// Whether a network call succeeded. A SAML test reads the row, so it is never "reached".
    pub reached_server: bool,
}

impl TestOutcome {
    fn with(steps: [ProtocolStep; 4]) -> Self {
        Self {
            status: "incomplete",
            steps: steps.iter().copied().map(StepReport::pending).collect(),
            endpoints: None,
            reached_server: false,
        }
    }

    /// A provider whose required fields were never filled in.
    ///
    /// This is a *different* failure from a provider that was filled in and does not work, and the
    /// wizard underlines a different input for each, so it gets its own step: a certificate row
    /// that reports "you did not paste one" instead of "the one you pasted is broken" is the
    /// difference between a form you can fill in and a dead end.
    pub fn unconfigured(kind: &str, missing: &[&str]) -> Self {
        let flow = if kind == "saml" {
            Self::with(ProtocolStep::ASSERTION_FLOW)
        } else {
            Self::with(ProtocolStep::CODE_FLOW)
        };
        let mut outcome = flow;
        outcome.steps[0] = StepReport::done(
            outcome.steps[0].step,
            "failed",
            format!(
                "a {} provider needs `{}` in its configuration before it can be tested at all",
                kind.to_uppercase(),
                missing.join("`, `")
            ),
        );
        outcome.status = "failed";
        outcome
    }

    /// The step that failed, if one did. Carried on the `iam.provider_test_*` event so a
    /// webhook subscriber can tell *which* check refused without parsing a sentence.
    #[must_use]
    pub fn failing_step(&self) -> Option<ProtocolStep> {
        self.steps
            .iter()
            .find(|step| step.status == "failed")
            .map(|step| step.step)
    }

    /// Whether the enable gate opens: every step passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == "ok"
    }

    /// One sentence for the top of the result, next to the ladder.
    #[must_use]
    pub fn headline(&self) -> String {
        match (self.status, self.failing_step()) {
            ("failed", Some(step)) => format!("the {} check refused this provider", step.as_str()),
            ("failed", None) => "this provider is not configured correctly".to_owned(),
            _ => "every step passed".to_owned(),
        }
    }
}

/// What the OIDC/OAuth2 test needs to decide, supplied by the caller.
///
/// The network call lives in the API layer (this crate reaches the network in exactly one place,
/// [`crate::sso::oidc`]), so the *decision* is separated from the *fetch*. That is what lets a
/// test prove a wrong issuer is refused without standing up an identity provider — and a wrong
/// issuer is precisely the case that must never be caught by a live round trip.
pub struct OidcProbe<'a> {
    /// The issuer the row was configured with.
    pub configured_issuer: Option<&'a str>,
    /// What the provider answered, or the error it answered with.
    pub document: Result<Discovery>,
    /// The keys read from the JWKS the discovery document pointed at, or why they could not be
    /// read. A [`Result`] rather than a `Vec` on purpose: "no usable key was published" and "the
    /// key set could not be fetched" are different failures with different repairs, and one empty
    /// vector would report both as the first — sending the operator to fix a provider that was
    /// working perfectly.
    pub keys: Result<Vec<Jwk>, String>,
    /// Whether the client secret reference resolves. A protocol sign-in needs it; a directory
    /// needs the bind password instead.
    pub secret_present: bool,
}

impl std::fmt::Debug for OidcProbe<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The probe carries key material; a `Debug` derive would print a modulus into any log
        // line that formats it. Public keys are not secret, but a 256-byte modulus in a log is
        // noise nobody asked for, and the type is easy to print by accident.
        formatter
            .debug_struct("OidcProbe")
            .field("configured_issuer", &self.configured_issuer)
            .field("document", &self.document.is_ok())
            .field("keys", &self.keys.as_ref().map(Vec::len).ok())
            .field("secret_present", &self.secret_present)
            .finish()
    }
}

/// Run the OIDC/OAuth2 ladder over what the caller fetched.
///
/// The issuer check is the reason this module exists. A discovery document is fetched **from** the
/// configured issuer and says who it *is*; RFC 8414 requires those to match, and the whole
/// security of OIDC rests on the fact that they do — a document served by the configured host
/// naming a different issuer is a host serving somebody else's identity. Without this check the
/// registry prints a green test for exactly that row.
pub fn test_oidc(probe: OidcProbe<'_>) -> TestOutcome {
    let mut outcome = TestOutcome::with(ProtocolStep::CODE_FLOW);

    let discovery = match &probe.document {
        Ok(discovery) => {
            outcome.reached_server = true;
            outcome.endpoints = Some(discovery_endpoints(discovery));
            outcome.steps[0] = StepReport::done(
                ProtocolStep::Discovery,
                "ok",
                "the provider answered with a complete discovery document",
            );
            discovery
        }
        Err(error) => {
            outcome.steps[0] = StepReport::done(
                ProtocolStep::Discovery,
                "failed",
                format!("the discovery document could not be read: {error}"),
            );
            return outcome.failed();
        }
    };

    // The issuer check. A provider that names no issuer cannot be checked against one, and is
    // not failed for that — its endpoints are entered by hand, so there is nothing to compare.
    // It is *told*, because "no issuer" is the configuration that will surprise somebody later.
    match probe.configured_issuer {
        Some(configured) => {
            if !issuers_match(configured, &discovery.issuer) {
                outcome.steps[1] = StepReport::done(
                    ProtocolStep::Issuer,
                    "failed",
                    format!(
                        "the provider publishes the issuer `{}`, but this provider is configured \
                         for `{configured}` — the document you fetched does not belong to the \
                         identity you configured",
                        discovery.issuer
                    ),
                );
                return outcome.failed();
            }
            outcome.steps[1] = StepReport::done(
                ProtocolStep::Issuer,
                "ok",
                format!("the document's issuer is `{configured}`"),
            );
        }
        None => {
            outcome.steps[1] = StepReport::done(
                ProtocolStep::Issuer,
                "ok",
                "the endpoints are entered explicitly, so there is no published issuer to \
                 compare — tokens will still have to name the configured one",
            );
        }
    }

    match &probe.keys {
        Err(error) => {
            outcome.steps[2] = StepReport::done(
                ProtocolStep::KeySet,
                "failed",
                format!("the provider's signing keys are not readable: {error}"),
            );
            return outcome.failed();
        }
        Ok(keys) if keys.is_empty() => {
            outcome.steps[2] = StepReport::done(
                ProtocolStep::KeySet,
                "failed",
                "the provider publishes no RSA signing key this platform can verify (a key must \
                 be RSA, at least 2048 bits, and carry a `kid`)",
            );
            return outcome.failed();
        }
        Ok(keys) => {
            outcome.steps[2] = StepReport::done(
                ProtocolStep::KeySet,
                "ok",
                format!("{} usable signing key(s) were published", keys.len()),
            );
        }
    }

    // The secret check is a note, not a failure: a public client (PKCE with no secret) is a real
    // deployment, and refusing to pass a configuration that is legal would send an operator
    // looking for a secret that should not exist. What it does is *say* the sign-in will be
    // refused, so the discovery is not the thing that surprises them later.
    outcome.steps[3] = StepReport::done(
        ProtocolStep::Claims,
        "ok",
        if probe.secret_present {
            "the issuer, the signing keys and the client secret are all in place"
        } else {
            "the issuer and the signing keys are in place, but the client secret is not defined \
             in this installation — a sign-in will be refused until it is"
        },
    );

    outcome.status = "ok";
    outcome
}

/// Run the SAML ladder.
///
/// SAML has no discovery — every endpoint is entered — so the ladder is the configuration's own
/// checks rather than a fetch: a readable certificate, then the reader's claim half against a
/// probe document that carries every attribute the configuration names.
///
/// The split matters for the same reason it does in the directory: the certificate step and the
/// claim step fail for different reasons and are fixed differently (re-paste the key vs. fix the
/// entity id), and one message for both sends the operator to the wrong one.
pub fn test_saml(config: &SamlConfig) -> TestOutcome {
    let mut outcome = TestOutcome::with(ProtocolStep::ASSERTION_FLOW);

    if let Err(error) = crate::sso::saml::certificate_is_readable(&config.certificate_pem) {
        outcome.steps[0] = StepReport::done(
            ProtocolStep::Certificate,
            "failed",
            format!("the configured certificate could not be read: {error}"),
        );
        return outcome.failed();
    }
    outcome.steps[0] = StepReport::done(
        ProtocolStep::Certificate,
        "ok",
        "the signing certificate is a readable RSA key",
    );

    // A SAML provider has no discovery document to disagree with — the configuration *is* the
    // claim — so the check is that the reader can read an assertion built from these very
    // settings, which covers the entity id, the audience, the window and the attribute names in
    // one pass. Every one of those is something an operator types.
    let (issuer, audience) = (config.issuer.trim(), config.audience.trim());
    if issuer.is_empty() || audience.is_empty() {
        outcome.steps[1] = StepReport::done(
            ProtocolStep::Issuer,
            "failed",
            "a SAML provider needs both an issuer (the provider's entity id) and an audience \
             (this application's entity id)",
        );
        return outcome.failed();
    }

    let probe = crate::sso::saml::probe_document(
        issuer,
        audience,
        &config.email_attribute,
        config.group_attribute.as_deref(),
        config.display_name_attribute.as_deref(),
    );
    let assertion = match crate::sso::saml::probe_claims(&probe, config) {
        Ok(assertion) => assertion,
        Err(error) => {
            outcome.steps[1] = StepReport::done(
                ProtocolStep::Issuer,
                "failed",
                format!("the assertion reader cannot read this configuration: {error}"),
            );
            return outcome.failed();
        }
    };
    outcome.steps[1] = StepReport::done(
        ProtocolStep::Issuer,
        "ok",
        format!("an assertion from `{issuer}` for `{audience}` reads back"),
    );

    // SAML signs the assertion itself, so the key set *is* the certificate — already proved in
    // step one. Saying so is the difference between a ladder that reports what happened and one
    // padded out to look the same length as the OIDC one.
    outcome.steps[2] = StepReport::done(
        ProtocolStep::KeySet,
        "ok",
        "the certificate is the key set — a SAML provider publishes no separate JWKS",
    );

    // The counts come from the assertion that was actually read, not from constants dressed as
    // measurements: a group attribute wired wrongly reads back zero groups, and a sentence
    // claiming "1 group" would report the wiring as sound.
    outcome.steps[3] = StepReport::done(
        ProtocolStep::Claims,
        "ok",
        format!(
            "a probe assertion read back an address from `{}` and {} group value(s) — the \
             attribute names in the configuration are wired correctly",
            config.email_attribute,
            assertion.groups.len()
        ),
    );

    outcome.endpoints = Some(serde_json::json!({
        "issuer": config.issuer,
        "audience": config.audience,
    }));
    outcome.status = "ok";
    outcome
}

impl TestOutcome {
    /// Mark the outcome failed, keeping the endpoints an operator needs to fix the row.
    fn failed(mut self) -> Self {
        self.status = "failed";
        self
    }
}

/// The endpoints the panel shows next to a test.
///
/// Lives here rather than in the API layer because the *sign-in path* and the *test* must show the
/// same thing: two renderers of one document is how a panel ends up displaying an endpoint the
/// platform never actually used.
#[must_use]
pub fn discovery_endpoints(discovery: &Discovery) -> Value {
    serde_json::json!({
        "issuer": discovery.issuer,
        "authorization_endpoint": discovery.authorization_endpoint,
        "token_endpoint": discovery.token_endpoint,
        "jwks_uri": discovery.jwks_uri,
        "userinfo_endpoint": discovery.userinfo_endpoint,
    })
}

/// Whether a discovery document's issuer is the one that was configured.
///
/// Trailing slashes are the one difference allowed, and they are allowed *because* a discovery
/// URL is built by appending `/.well-known/openid-configuration` to an issuer an operator may have
/// typed with or without the slash — every provider in practice publishes one form and operators
/// type the other. Everything else is compared exactly: this string is the root of trust, and a
/// comparison that trims more than a trailing slash would accept a different host.
#[must_use]
pub fn issuers_match(configured: &str, published: &str) -> bool {
    // A free function rather than a closure: the closure form needs the two borrows to unify,
    // and older toolchains infer two unrelated lifetimes for it, so the same comparison that
    // reads correctly fails to compile.
    fn normalize(value: &str) -> &str {
        value.trim().trim_end_matches('/')
    }
    !normalize(configured).is_empty() && normalize(configured) == normalize(published)
}

/// Refuse a provider whose configured issuer is not the published one.
///
/// Used by the sign-in path as well as by the test, because a test that can be green while a
/// sign-in would fail is the same lie in a different place. A row configured with an issuer the
/// document does not claim must not mint a session — and the metadata cache makes that sharp: a
/// document fetched once is served to every later sign-in, so a mismatch the *test* reports and
/// the *callback* ignores is a mismatch an operator has been told about and nothing acted on.
pub fn require_issuer(discovery: &Discovery, configured: Option<&str>) -> Result<()> {
    let Some(configured) = configured else {
        // Nothing to compare against: the endpoints were entered by hand and the token's own
        // `iss` claim is checked later, which is where that configuration is enforced.
        return Ok(());
    };
    if issuers_match(configured, &discovery.issuer) {
        return Ok(());
    }
    Err(IdentityError::InvalidProvider(format!(
        "the provider publishes the issuer `{}`, which is not the configured `{configured}`",
        discovery.issuer
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn discovery(issuer: &str) -> Discovery {
        Discovery {
            issuer: issuer.to_owned(),
            authorization_endpoint: "https://idp.example/authorize".into(),
            token_endpoint: "https://idp.example/token".into(),
            jwks_uri: "https://idp.example/jwks".into(),
            userinfo_endpoint: Some("https://idp.example/userinfo".into()),
        }
    }

    fn jwk() -> Jwk {
        // 256 bytes of modulus — the length `parse_jwks` requires, and nothing here verifies a
        // signature, so the bytes need only be the right size to represent "a usable key".
        Jwk {
            key_id: "k1".into(),
            n: vec![1; 256],
            e: vec![1, 0, 1],
        }
    }

    // The probe borrows the configured issuer, so the helper is generic over that borrow rather
    // than pretending every caller passes a literal — a `'static` return would only compile for
    // string constants, and the first non-literal caller would get a lifetime error in a test
    // helper instead of at the call site.
    fn ok_probe(configured: &str) -> OidcProbe<'_> {
        OidcProbe {
            configured_issuer: Some(configured),
            document: Ok(discovery("https://idp.example")),
            keys: Ok(vec![jwk()]),
            secret_present: true,
        }
    }

    #[test]
    fn a_matching_issuer_passes_every_step() {
        let outcome = test_oidc(ok_probe("https://idp.example"));
        assert_eq!(outcome.status, "ok", "{outcome:?}");
        assert_eq!(outcome.steps.len(), ProtocolStep::CODE_FLOW.len());
        assert!(
            outcome.steps.iter().all(|step| step.status == "ok"),
            "{outcome:?}"
        );
        assert!(outcome.reached_server);
        assert!(outcome.passed());
    }

    #[test]
    fn a_wrong_issuer_is_refused_and_names_the_step() {
        // The case acceptance line 4 is about: the document is fetched from the configured host
        // and says it is somebody else. A green box here would be a registry that calls a host
        // serving another identity "correctly configured".
        let outcome = test_oidc(ok_probe("https://other.example"));
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::Issuer));
        let detail = &outcome.steps[1].detail;
        assert!(
            detail.contains("https://idp.example") && detail.contains("https://other.example"),
            "the sentence must name both sides of the mismatch: {detail}"
        );
        // The claim step never ran — a claim read against an issuer that is not yours is not a
        // claim about anything, so it must not be reported as ok.
        assert_eq!(outcome.steps[3].status, "pending");
        assert!(!outcome.passed());
    }

    #[test]
    fn a_trailing_slash_is_not_a_mismatch() {
        // Every discovery URL is built by appending to the configured issuer, so the operator's
        // trailing slash and the published one routinely disagree by exactly that character.
        assert!(issuers_match("https://idp.example/", "https://idp.example"));
        assert!(issuers_match("https://idp.example", "https://idp.example/"));
        assert!(!issuers_match(
            "https://idp.example",
            "https://idp.example.evil"
        ));
        assert!(!issuers_match("", "https://idp.example"));
    }

    #[test]
    fn an_unreadable_discovery_document_fails_at_the_first_step() {
        let outcome = test_oidc(OidcProbe {
            configured_issuer: Some("https://idp.example"),
            document: Err(IdentityError::InvalidProvider("no such host".into())),
            keys: Ok(vec![]),
            secret_present: true,
        });
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::Discovery));
        assert!(!outcome.reached_server);
        assert!(
            outcome.steps[1..]
                .iter()
                .all(|step| step.status == "pending"),
            "nothing after an unreadable document may claim to have run"
        );
    }

    #[test]
    fn a_provider_with_no_usable_key_is_refused_at_the_key_step() {
        let outcome = test_oidc(OidcProbe {
            keys: Ok(vec![]),
            ..ok_probe("https://idp.example")
        });
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::KeySet));
        assert_eq!(outcome.steps[3].status, "pending");
    }

    #[test]
    fn an_unreadable_key_set_is_not_reported_as_an_absent_one() {
        // The two failures are different repairs: one is the provider publishing keys this
        // platform refuses, the other is a wrong `jwks_uri` or a network. Reporting the second as
        // the first sends the operator to fix a provider that was working.
        let outcome = test_oidc(OidcProbe {
            keys: Err("the host does not answer".to_owned()),
            ..ok_probe("https://idp.example")
        });
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::KeySet));
        let detail = &outcome.steps[2].detail;
        assert!(
            detail.contains("not readable") && detail.contains("does not answer"),
            "the sentence must carry the underlying reason: {detail}"
        );
        assert!(
            !detail.contains("publishes no RSA signing key"),
            "an unreadable key set must not be reported as an absent one: {detail}"
        );
    }

    #[test]
    fn a_missing_secret_passes_but_says_so() {
        // A public client is a legal deployment; refusing the test would send an operator
        // looking for a secret that should not exist. The step says what will be refused instead.
        let outcome = test_oidc(OidcProbe {
            secret_present: false,
            ..ok_probe("https://idp.example")
        });
        assert_eq!(outcome.status, "ok");
        assert!(
            outcome.steps[3]
                .detail
                .contains("client secret is not defined"),
            "{}",
            outcome.steps[3].detail
        );
    }

    #[test]
    fn a_provider_with_no_issuer_is_told_rather_than_failed() {
        let outcome = test_oidc(OidcProbe {
            configured_issuer: None,
            ..ok_probe("https://idp.example")
        });
        assert_eq!(outcome.status, "ok");
        assert!(
            outcome.steps[1].detail.contains("entered explicitly"),
            "{}",
            outcome.steps[1].detail
        );
    }

    #[test]
    fn require_issuer_refuses_the_same_mismatch_the_test_refuses() {
        let published = discovery("https://idp.example");
        assert!(require_issuer(&published, Some("https://idp.example/")).is_ok());
        assert!(require_issuer(&published, None).is_ok());
        let error = require_issuer(&published, Some("https://other.example"))
            .expect_err("a mismatch must refuse");
        assert!(error.to_string().contains("not the configured"), "{error}");
    }

    #[test]
    fn endpoints_are_reported_even_when_the_issuer_is_wrong() {
        // The operator has to be able to see *what* the provider claims in order to fix the
        // configuration, so the mismatch does not throw the endpoints away.
        let outcome = test_oidc(ok_probe("https://other.example"));
        let endpoints = outcome.endpoints.expect("endpoints");
        assert_eq!(endpoints["issuer"], json!("https://idp.example"));
    }

    #[test]
    fn the_headline_names_the_refusing_check() {
        let outcome = test_oidc(ok_probe("https://other.example"));
        assert!(
            outcome.headline().contains("issuer"),
            "the one line above the ladder must say which check refused: {}",
            outcome.headline()
        );
    }

    #[test]
    fn the_probe_does_not_print_key_material() {
        // A derived Debug would dump the modulus of every JWK into any log line that formats the
        // probe. The count is enough for a log, and that is all this implementation prints.
        let printed = format!("{:?}", ok_probe("https://idp.example"));
        assert!(
            !printed.contains("n: ") && !printed.contains("e: "),
            "the modulus must not reach a log line: {printed}"
        );
        assert!(printed.contains("keys: Some(1)"), "{printed}");
    }

    fn saml_config() -> SamlConfig {
        let (_private, certificate) = crate::sso::saml::tests::test_key();
        SamlConfig {
            issuer: "https://idp.example/saml".into(),
            audience: "https://omnion.example".into(),
            certificate_pem: certificate,
            email_attribute: "email".into(),
            group_attribute: Some("groups".into()),
            display_name_attribute: Some("displayName".into()),
        }
    }

    #[test]
    fn a_saml_configuration_is_walked_through_its_own_flow() {
        let outcome = test_saml(&saml_config());
        assert_eq!(outcome.status, "ok", "{outcome:?}");
        assert!(
            outcome.steps.iter().all(|step| step.status == "ok"),
            "{outcome:?}"
        );
        // The first step is the certificate, not a discovery that SAML does not have: a row whose
        // verdict is a constant is a row nobody reads.
        assert_eq!(outcome.steps[0].step, ProtocolStep::Certificate);
        assert!(
            !outcome.reached_server,
            "SAML reads the row, it fetches nothing"
        );
    }

    #[test]
    fn a_saml_configuration_reports_the_groups_it_actually_read() {
        // The probe writes the group attribute twice, so a configuration that reads a list must
        // report two. A hard-coded "1 group" would pass here and hide a broken group attribute.
        let outcome = test_saml(&saml_config());
        assert!(
            outcome.steps[3].detail.contains("2 group value(s)"),
            "{}",
            outcome.steps[3].detail
        );
    }

    #[test]
    fn a_saml_provider_with_no_group_attribute_says_zero_rather_than_one() {
        let mut config = saml_config();
        config.group_attribute = None;
        let outcome = test_saml(&config);
        assert_eq!(outcome.status, "ok");
        assert!(
            outcome.steps[3].detail.contains("0 group value(s)"),
            "{}",
            outcome.steps[3].detail
        );
    }

    #[test]
    fn a_saml_certificate_nobody_can_read_fails_at_the_first_step() {
        let mut config = saml_config();
        config.certificate_pem =
            "-----BEGIN CERTIFICATE-----\nnot base64!!\n-----END CERTIFICATE-----\n".into();
        let outcome = test_saml(&config);
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::Certificate));
        // The sentence on the *failing* step names the thing that is wrong. Checking step 1 here
        // would pass for a blank audience and prove nothing about the certificate.
        let detail = &outcome.steps[0].detail;
        assert!(detail.contains("certificate"), "{detail}");
        assert!(
            outcome.steps[1..]
                .iter()
                .all(|step| step.status == "pending"),
            "nothing after an unreadable certificate may claim to have run"
        );
    }

    #[test]
    fn a_saml_provider_with_a_blank_audience_is_refused_by_name() {
        let mut config = saml_config();
        config.audience = "   ".into();
        let outcome = test_saml(&config);
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.failing_step(), Some(ProtocolStep::Issuer));
        assert!(outcome.steps[1].detail.contains("audience"));
    }

    #[test]
    fn a_provider_that_was_never_filled_in_names_what_is_missing() {
        // A missing certificate and a broken certificate are different problems with different
        // inputs to fix, and the wizard underlines a different field for each.
        let outcome = TestOutcome::unconfigured("saml", &["certificate_pem"]);
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.steps[0].step, ProtocolStep::Certificate);
        assert!(
            outcome.steps[0].detail.contains("certificate_pem"),
            "{}",
            outcome.steps[0].detail
        );
    }

    #[test]
    fn an_unconfigured_oidc_provider_starts_at_discovery() {
        // Same constructor, two flows: padding the OIDC ladder with a certificate row would be the
        // same constant-verdict row the SAML ladder avoids.
        let outcome = TestOutcome::unconfigured("oidc", &["issuer"]);
        assert_eq!(outcome.steps[0].step, ProtocolStep::Discovery);
        assert_eq!(outcome.steps.len(), ProtocolStep::CODE_FLOW.len());
    }
}
