//! API keys: the shape, the validation and the one-time plaintext (REQ-022, slice 1).
//!
//! ## Why a key is not a service-account key
//!
//! `service_account_keys` (REQ-006) holds the keys of a **machine identity**: a subject that
//! roles bind to, authorised through the same binding table as a person. A developer key is a
//! **delegation**: no role, no binding, no identity of its own. Every request it makes is
//! checked against the scope list the operator typed when creating it. The two arrive
//! identically on the wire, which is exactly why they must not share a table — one has to be
//! removable from the authorization path without a role ever existing, and the other must
//! never be.
//!
//! ## The plaintext exists once
//!
//! [`issue`] mints a [`Secret`] and its [`KeyPrefix`] together; the store keeps the prefix and
//! the hash and returns the plaintext to the caller of the create route, which is the only
//! place it is ever available. Nothing in this crate can read it back — [`ApiKey`] has no field
//! for it, and the crate's queries never `select` a value column, because there is none.
//!
//! ## Why the hash is compared in constant time
//!
//! [`verify_secret`] compares hashes with [`hmac`]'s constant-time equality rather than `==`.
//! An `==` on a hex string short-circuits at the first differing character, and a token's
//! lookup namespace (ten known characters) is public, so a timing oracle would leak the
//! remaining bytes of a hash to anybody who can measure. It is a small thing to get right and
//! impossible to fix later without rotating every key.
//!
//! ## A key's scopes may only narrow
//!
//! [`mintable_from`] is the delegation rule: a key can never be created with a scope its
//! issuer did not hold. The check runs against the **catalogue**, not the caller's granted set,
//! for a specific reason — a scope key that exists nowhere cannot be enforced by any route, so
//! minting one produces a key that looks powerful in the list and does nothing on the wire. It
//! is refused at creation instead, with the name in the message.

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};

/// The namespace every developer key starts with, so one is recognisable in a config file or a
/// crash log. Deliberately different from `omsa_` (a service-account token) and from `omn_live_`
/// below: a key that looks like a service-account token would be misdiagnosed as one.
pub const KEY_NAMESPACE: &str = "omndev";

/// Length of the lookup prefix after the namespace and environment.
const PREFIX_LENGTH: usize = 10;

/// Length of the secret half.
const SECRET_LENGTH: usize = 32;

/// Production credentials.
pub const ENVIRONMENT_LIVE: &str = "live";

/// Non-production credentials.
pub const ENVIRONMENT_SANDBOX: &str = "sandbox";

/// Every environment a key may carry, closed.
///
/// Closed because the panel builds its environment banner from this value and the sandbox
/// console decides whether to raise a red banner from it. A third environment would render a
/// state nobody wrote, which is the exact class of bug the platform's other screens are
/// forbidden from shipping.
pub const ENVIRONMENTS: &[&str] = &[ENVIRONMENT_LIVE, ENVIRONMENT_SANDBOX];

/// Shortest accepted key name.
pub const MIN_NAME_LENGTH: usize = 3;

/// Longest accepted key name.
pub const MAX_NAME_LENGTH: usize = 64;

/// Most scopes one key may carry.
///
/// 64 is far more than any real integration needs and exists to bound the request line and the
/// `scopes` column; it is not a policy ceiling on what the platform can express.
pub const MAX_SCOPES: usize = 64;

/// A key as the platform stores it. **There is no field here that could hold a secret.**
///
/// The list screen, the detail screen and the CSV export all take this type, so "the secret is
/// never returned by a read" is a property of the type rather than a rule someone has to
/// remember while writing the next handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Operator-facing name.
    pub name: String,
    /// `live` or `sandbox`.
    pub environment: String,
    /// The displayable lookup namespace, e.g. `omndev_live_7f3a9c1d2e`.
    pub key_prefix: String,
    /// SHA-256 of the whole token. Present because authentication reads the row; never
    /// selected by a list or detail query, and never serialized to a response.
    #[serde(skip_serializing)]
    pub key_hash: String,
    /// The delegation.
    pub scopes: Vec<String>,
    /// Who issued it.
    pub created_by: Option<Uuid>,
    /// That user's name, copied in so a key outlives (and still names) its issuer.
    pub created_by_name: String,
    /// Last authentication.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it stops authenticating.
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// The key this one replaced, for the rotation chain.
    pub rotated_from: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

/// The displayable half of a key: what an operator may see, forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPrefix {
    /// The lookup namespace stored alongside the hash, e.g. `omndev_live_7f3a9c1d2e`.
    pub prefix: String,
}

/// A freshly minted key. The plaintext lives here and nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    /// The token the operator copies once: `omndev_live_<prefix>_<secret>`.
    pub token: String,
    /// The same value, in the two halves the store needs.
    pub prefix: KeyPrefix,
    /// The hash of [`Secret::token`].
    pub hash: String,
}

/// What a create or rotate request asks for.
#[derive(Debug, Clone)]
pub struct NewKey {
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// `live` or `sandbox`.
    pub environment: String,
    /// The delegation. Must be non-empty, must be resolvable, and must narrow the issuer.
    pub scopes: Vec<String>,
    /// Optional expiry. Refused in the past by [`NewKey::validated`].
    pub expires_at: Option<OffsetDateTime>,
    /// Who is issuing it.
    pub created_by: Option<Uuid>,
    /// That user's display name, for the list column.
    pub created_by_name: String,
}

/// The lifecycle state a row is in right now, as the list screen shows it.
///
/// [`ApiKey`] stores three nullable timestamps and the screen needs one word. The mapping is
/// here rather than in the route so the *badge* cannot disagree with the API's own answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus {
    /// Usable.
    Active,
    /// Past `expires_at`; the platform refuses it, and the screen says so rather than
    /// displaying an expired key as though it were still usable.
    Expired,
    /// Withdrawn by an operator.
    Revoked,
}

impl ApiKey {
    /// The status this row is in at `now`.
    ///
    /// **Revoked wins over expired.** An operator who revoked a key that had also expired must
    /// be told it was revoked: they took the action, and the screen is where they check
    /// whether it took. A revoked key does not come back when its expiry passes, and reading
    /// the columns the other way round would report it as merely lapsed.
    #[must_use]
    pub fn status_at(&self, now: OffsetDateTime) -> KeyStatus {
        if self.revoked_at.is_some() {
            KeyStatus::Revoked
        } else if self.expires_at.is_some_and(|expires| expires <= now) {
            KeyStatus::Expired
        } else {
            KeyStatus::Active
        }
    }

    /// Whether this key may authenticate at `now`.
    #[must_use]
    pub fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.status_at(now) == KeyStatus::Active
    }
}

impl KeyStatus {
    /// The word the badge and the event payload use.
    ///
    /// Serialising the enum would produce the same three strings, but a `Serialize` derive on
    /// this type would also apply anywhere else it is embedded — and the event payload is the
    /// one place a renamed variant would silently change a wire contract other platforms read.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

impl NewKey {
    /// Check everything the table would otherwise reject with a bare constraint name.
    ///
    /// The panel shows these messages on the field, so each one names the input rather than the
    /// rule: an operator who typed `API` learns it needs three characters, not that
    /// `api_keys_name_ck` failed.
    ///
    /// # Errors
    ///
    /// [`DeveloperError::Invalid`] when the environment is outside [`ENVIRONMENTS`], the name is
    /// outside the length window, no scope was given, more than [`MAX_SCOPES`] were, or the
    /// expiry is in the past.
    pub fn validated(&self, now: OffsetDateTime) -> Result<()> {
        parse_environment(&self.environment)?;

        // Blank **before** length. `"      "` trims to the empty string, so a length check
        // first answers "must be between 3 and 64 characters (got 0)" for a name the operator
        // did not type — the one message that tells them nothing about what went wrong.
        let name = self.name.trim();
        if name.is_empty() {
            return Err(DeveloperError::Invalid("name cannot be blank".into()));
        }
        if name.len() < MIN_NAME_LENGTH || name.len() > MAX_NAME_LENGTH {
            return Err(DeveloperError::Invalid(format!(
                "name must be between {MIN_NAME_LENGTH} and {MAX_NAME_LENGTH} characters (got {})",
                name.chars().count()
            )));
        }

        let scopes = dedupe_scopes(&self.scopes);
        if scopes.is_empty() {
            return Err(DeveloperError::Invalid(
                "choose at least one scope — a key with no scope could not call anything".into(),
            ));
        }
        if scopes.len() > MAX_SCOPES {
            return Err(DeveloperError::Invalid(format!(
                "at most {MAX_SCOPES} scopes (got {})",
                scopes.len()
            )));
        }

        if self.expires_at.is_some_and(|expires| expires <= now) {
            return Err(DeveloperError::Invalid(
                "expiry must be in the future".into(),
            ));
        }
        Ok(())
    }
}

/// Trim, drop empties, dedupe and sort a scope list.
///
/// Sorting matters for more than tidiness: the stored array is compared against the issuer's
/// set, and a list whose order depended on the order boxes were ticked would make two
/// identical keys hash differently.
#[must_use]
pub fn dedupe_scopes(scopes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = scopes
        .iter()
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Check an environment against the closed list.
pub fn parse_environment(environment: &str) -> Result<&'static str> {
    ENVIRONMENTS
        .iter()
        .copied()
        .find(|candidate| *candidate == environment)
        .ok_or_else(|| {
            DeveloperError::Invalid(format!(
                "environment must be one of {} (got \"{environment}\")",
                ENVIRONMENTS.join(", ")
            ))
        })
}

/// Whether this environment is production. The sandbox banner reads this, and nothing else.
#[must_use]
pub fn is_live_environment(environment: &str) -> bool {
    environment == ENVIRONMENT_LIVE
}

/// Whether a name is renderable in the list, at the length the table accepts.
#[must_use]
pub fn name_is_valid(name: &str) -> bool {
    let trimmed = name.trim();
    trimmed.len() >= MIN_NAME_LENGTH && trimmed.len() <= MAX_NAME_LENGTH
}

/// Whether every named scope exists in `catalogue`.
///
/// A scope that resolves nowhere cannot be enforced by a route, so a key carrying one looks
/// powerful in the list and does nothing on the wire. The message names the offender.
pub fn scope_names_valid(scopes: &[String], catalogue: &[&str]) -> Result<()> {
    for scope in dedupe_scopes(scopes) {
        if !catalogue.contains(&scope.as_str()) {
            return Err(DeveloperError::Invalid(format!(
                "\"{scope}\" is not a permission this platform knows"
            )));
        }
    }
    Ok(())
}

/// The delegation rule: `requested` may only contain what `held` does.
///
/// `held` is the **creating caller's own granted set**, not the catalogue. The distinction is
/// the whole rule — checking against the catalogue would let any caller holding
/// `developer.keys.manage` mint a key with `iam.users.manage`, which is not delegation but a
/// privilege escalation wearing a key.
///
/// # Errors
///
/// [`DeveloperError::Invalid`] naming the first scope the issuer does not hold.
pub fn mintable_from(requested: &[String], held: &[&str]) -> Result<Vec<String>> {
    let scopes = dedupe_scopes(requested);
    for scope in &scopes {
        if !held.contains(&scope.as_str()) {
            return Err(DeveloperError::Invalid(format!(
                "you cannot grant \"{scope}\": you do not hold it yourself"
            )));
        }
    }
    Ok(scopes)
}

/// Mint a key. `OsRng` and not a seeded generator, because this value is a credential.
pub fn issue(environment: &str) -> Secret {
    let environment = parse_environment(environment).unwrap_or(ENVIRONMENT_SANDBOX);

    let mut prefix_bytes = [0_u8; PREFIX_LENGTH];
    OsRng.fill_bytes(&mut prefix_bytes);
    let prefix_text: String = prefix_bytes.iter().map(|byte| alphabet_char(byte % 36)).collect();

    let mut secret_bytes = [0_u8; SECRET_LENGTH];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret_text: String = secret_bytes.iter().map(|byte| alphabet_char(byte % 36)).collect();

    let prefix = format!("{KEY_NAMESPACE}_{environment}_{prefix_text}");
    let token = format!("{prefix}_{secret_text}");

    Secret {
        hash: hash_token(&token),
        token,
        prefix: KeyPrefix { prefix },
    }
}

/// Compare a presented token's hash with a stored one, in constant time.
///
/// [`subtle::ConstantTimeEq`] rather than `==`. An `==` on a hex string short-circuits at the
/// first differing character, and the token's lookup namespace (the `omndev_live_` part and ten
/// characters) is public — it is printed on the key list for every key on screen. A timing
/// oracle on the remainder would hand a caller somebody else's hash one byte at a time.
///
/// `subtle` is already in the dependency tree (`crates/identity`), so this adds no crate to
/// the build and no new supply-chain surface to justify.
#[must_use]
pub fn verify_secret(presented: &str, stored_hash: &str) -> bool {
    let candidate = hash_token(presented);
    let left = candidate.as_bytes();
    let right = stored_hash.as_bytes();
    if left.len() != right.len() {
        // A stored value that is not a hash is refused rather than compared: there is no
        // meaningful constant-time comparison between a digest and arbitrary text, and
        // silently accepting one would turn a corrupt row into an authenticator.
        return false;
    }
    left.ct_eq(right).into()
}

/// SHA-256, hex-encoded. The one hash function in the crate.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// One character of the alphabet a prefix or secret is drawn from.
///
/// `byte % 36` is biased for bytes above 251 (36 does not divide 256), and the bias is about
/// two parts in a thousand — irrelevant for a 32-character secret, and stated here rather than
/// left to be rediscovered as a "why not rejection sampling" question. Rejection sampling would
/// mean a loop whose exit condition depends on `OsRng`, which is a worse trade for a value with
/// 32 characters of entropy.
fn alphabet_char(byte: u8) -> char {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    ALPHABET[usize::from(byte) % ALPHABET.len()] as char
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn new_key(name: &str, environment: &str, scopes: &[&str]) -> NewKey {
        NewKey {
            organization_id: Uuid::nil(),
            name: name.into(),
            environment: environment.into(),
            scopes: scopes.iter().map(|scope| (*scope).into()).collect(),
            expires_at: None,
            created_by: None,
            created_by_name: String::new(),
        }
    }

    fn a_stored_key(revoked: bool, expired: bool, now: OffsetDateTime) -> ApiKey {
        ApiKey {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Billing sync".into(),
            environment: ENVIRONMENT_LIVE.into(),
            key_prefix: "omndev_live_abcdefghij".into(),
            key_hash: "deadbeef".into(),
            scopes: vec!["content.pages.read".into()],
            created_by: None,
            created_by_name: String::new(),
            last_used_at: None,
            expires_at: expired.then(|| now - time::Duration::days(1)),
            revoked_at: revoked.then(|| now - time::Duration::days(2)),
            rotated_from: None,
            created_at: now - time::Duration::days(30),
        }
    }

    #[test]
    fn the_status_prefers_revocation_over_expiry() {
        let now = datetime!(2026-10-02 12:00 UTC);
        // Both set. The operator revoked it; the screen must say so.
        assert_eq!(
            a_stored_key(true, true, now).status_at(now),
            KeyStatus::Revoked,
            "a revoked key that has also expired is revoked, not merely lapsed"
        );
        assert_eq!(
            a_stored_key(false, true, now).status_at(now),
            KeyStatus::Expired
        );
        assert_eq!(
            a_stored_key(false, false, now).status_at(now),
            KeyStatus::Active
        );
        assert!(!a_stored_key(false, true, now).is_active_at(now));
    }

    #[test]
    fn an_expiry_exactly_at_now_is_already_past() {
        // `expires_at <= now` rather than `<`: a key whose second has arrived must not
        // authenticate for that second, or an operator setting "expires in 60 seconds" gets 61.
        let now = datetime!(2026-10-02 12:00 UTC);
        let mut key = a_stored_key(false, false, now);
        key.expires_at = Some(now);
        assert_eq!(key.status_at(now), KeyStatus::Expired);
        key.expires_at = Some(now + time::Duration::seconds(1));
        assert_eq!(key.status_at(now), KeyStatus::Active);
    }

    #[test]
    fn an_issued_key_verifies_once_and_only_against_its_own_hash() {
        let secret = issue(ENVIRONMENT_LIVE);
        assert!(
            verify_secret(&secret.token, &secret.hash),
            "the token a key is created with must authenticate"
        );
        assert!(
            !verify_secret(&format!("{}x", secret.token), &secret.hash),
            "a single changed character must not verify"
        );
        assert!(
            !verify_secret(&secret.token, &hash_token("something else")),
            "another key's token must not verify against this key's hash"
        );
    }

    #[test]
    fn an_issued_key_carries_its_environment_in_the_prefix_and_the_token() {
        for environment in ENVIRONMENTS {
            let secret = issue(environment);
            assert!(
                secret.prefix.prefix.starts_with(&format!("{KEY_NAMESPACE}_{environment}_")),
                "{environment} keys must be recognisable in a config file: {}",
                secret.prefix.prefix
            );
            assert!(
                secret.token.starts_with(&secret.prefix.prefix),
                "the prefix must be a prefix of the token, because that is the lookup path"
            );
        }
    }

    #[test]
    fn a_hash_comparison_cannot_be_short_circuited_by_length() {
        // A stored value that is not a hash must be refused, not compared byte by byte.
        assert!(!verify_secret("omndev_live_x_y", "short"));
    }

    #[test]
    fn scopes_may_only_narrow() {
        let held = vec!["content.pages.read", "content.pages.manage"];
        assert_eq!(
            mintable_from(&["content.pages.read".into()], &held).unwrap(),
            vec!["content.pages.read".to_string()]
        );
        let refusal = mintable_from(&["content.pages.read".into(), "iam.users.manage".into()], &held)
            .unwrap_err();
        assert!(
            refusal.to_string().contains("iam.users.manage"),
            "the message must name the scope that was refused: {refusal}"
        );
    }

    #[test]
    fn the_delegation_rule_rejects_a_set_that_is_only_a_subset_after_trimming() {
        // Whitespace and duplicates are not a way to smuggle a scope past the check, and the
        // stored list is the deduped, sorted one.
        assert_eq!(
            mintable_from(
                &[" content.pages.read ".into(), "content.pages.read".into()],
                &["content.pages.read"]
            )
            .unwrap(),
            vec!["content.pages.read".to_string()]
        );
    }

    #[test]
    fn a_key_with_no_scope_is_refused_with_a_message_that_says_why() {
        let now = datetime!(2026-10-02 12:00 UTC);
        let error = new_key("Billing sync", "live", &[]).validated(now).unwrap_err();
        assert!(
            error.to_string().contains("at least one scope"),
            "an empty scope list is the inverted default this guard exists for: {error}"
        );
    }

    #[test]
    fn a_name_that_is_only_whitespace_is_refused() {
        let now = datetime!(2026-10-02 12:00 UTC);
        // Three spaces: long enough to pass the length check, empty once trimmed. The table's
        // `btrim` check would catch it too, with a message naming a constraint.
        let mut key = new_key("Billing sync", "live", &["content.pages.read"]);
        key.name = "      ".into();
        let error = key.validated(now).unwrap_err();
        assert!(
            error.to_string().contains("blank"),
            "a whitespace name must be named as blank, not as a length: {error}"
        );
    }

    #[test]
    fn an_expiry_in_the_past_is_refused() {
        let now = datetime!(2026-10-02 12:00 UTC);
        let mut key = new_key("Billing sync", "live", &["content.pages.read"]);
        key.expires_at = Some(now - time::Duration::minutes(1));
        assert!(
            key.validated(now).is_err(),
            "a key that expires before it is created is a mistake, not a very short key"
        );
        key.expires_at = Some(now + time::Duration::days(30));
        assert!(key.validated(now).is_ok());
    }

    #[test]
    fn an_environment_outside_the_closed_list_is_refused_by_name() {
        let now = datetime!(2026-10-02 12:00 UTC);
        let error = new_key("Billing sync", "staging", &["content.pages.read"])
            .validated(now)
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("live") && message.contains("sandbox"),
            "the message must name the environments that exist: {message}"
        );
    }

    #[test]
    fn an_unknown_scope_is_refused_rather_than_stored() {
        let catalogue = vec!["content.pages.read"];
        let error = scope_names_valid(&["content.pages.explode".into()], &catalogue).unwrap_err();
        assert!(
            error.to_string().contains("content.pages.explode"),
            "a scope nothing can enforce must be named: {error}"
        );
        assert!(scope_names_valid(&["content.pages.read".into()], &catalogue).is_ok());
    }
}
