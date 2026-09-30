//! Content API tokens (REQ-019, slice 1).
//!
//! A **content API token** is a read-only credential for the headless surface under
//! `/api/v1/content/*`. It is deliberately not a service account: a service account authenticates
//! the panel's own write APIs, and a token that could do the same would hand a published-content
//! integration the power to delete files. So the store keeps its own table and its own halves.
//!
//! Four decisions shape this file, and each is one where the obvious implementation produces a
//! token that *works* while being wrong:
//!
//! 1. **The plaintext is returned once, by construction, not by policy.** [`create_token`] is
//!    the only function that can ever see the secret string, and it hands it back in
//!    [`NewToken`] before dropping it. Every read path — list, update, authenticate — works from
//!    `prefix` + `token_hash`. A later "show it again" button cannot be built from this state
//!    without a schema change, which is the point.
//!
//! 2. **A lookup by prefix then decides, and the decision is one function.** [`authenticate`]
//!    answers exactly one question — *is this presented secret the one this token was issued?* —
//!    and it answers it in constant time. Everything that makes a token unusable (expired,
//!    revoked) is a *distinct* outcome rather than a `None`, because the caller's error code
//!    differs: an expired token deserves `401 token_expired` so an integrator can tell "refresh
//!    your credential" from "your credential is wrong".
//!
//! 3. **`site_id = NULL` means "every site of THIS organization", never "every site on the
//!    box".** The scope is applied in the SQL, joined against the caller's own organization, and
//!    the cross-tenant case is a 404 at the route rather than an empty list here. A token that
//!    answered 200-with-nothing would be indistinguishable from a site with no content.
//!
//! 4. **A duplicate name is refused, and the refusal is about the name.** Two tokens called
//!    "Prod" in one organization is a support ticket; the unique index is on `lower(name)`
//!    because "prod" and "Prod" are the same name to the person looking at the list.

use rand::{Rng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};

/// Namespace every content token starts with. The panel's copy button shows
/// `omn_xxxxxxxx_…`, so a secret pasted into a chat is recognisable as a content token at a
/// glance — and, more usefully, is *not* recognisable as a password, so nobody rotates it like one.
pub const TOKEN_NAMESPACE: &str = "omn";

/// Length of the human-visible prefix (hex characters, excluding the `omn_` marker).
pub const PREFIX_LENGTH: usize = 8;

/// Length of the secret half.
pub const SECRET_LENGTH: usize = 32;

/// The read scopes a token may hold.
///
/// Closed in code and re-checked at save time, because a scope string that reaches the database
/// unvalidated is a scope nothing renders a badge for: the panel's Scopes column would show a
/// mystery value and the route layer would refuse or ignore it depending on which code ran.
pub const SCOPES: [&str; 3] = ["content:read", "media:read", "content:write"];

/// The longest accepted display name.
pub const MAX_NAME_LENGTH: usize = 64;

/// Rate-limit tiers offered by the create dialog.
pub const RATE_TIER_STANDARD: i32 = 120;
/// Elevated tier, guarded by the manage permission because it is 5x the shared budget.
pub const RATE_TIER_ELEVATED: i32 = 600;

/// A token as the panel's list shows it. Never carries the secret.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiToken {
    /// Row identity.
    pub id: Uuid,
    /// Owning organization; the tenancy boundary.
    pub organization_id: Uuid,
    /// Site scope, or `None` for every site of the organization.
    pub site_id: Option<Uuid>,
    /// Display name.
    pub name: String,
    /// The copyable `omn_xxxxxxxx` marker.
    pub prefix: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact origins allowed to call, empty for "any".
    pub allowed_origins: Vec<String>,
    /// Requests per minute before `429`.
    pub rate_limit_per_minute: i32,
    /// Expiry, or `None` for "never".
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked, or `None` while active.
    pub revoked_at: Option<OffsetDateTime>,
    /// Last successful use.
    pub last_used_at: Option<OffsetDateTime>,
    /// Author.
    pub created_by: Option<Uuid>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A token plus the one and only copy of its plaintext.
#[derive(Debug, Clone)]
pub struct NewToken {
    /// The stored record, safe to show in a list.
    pub token: ApiToken,
    /// `omn_<prefix>_<secret>`. This value is never recoverable afterwards.
    pub plaintext: String,
}

/// A token edit.
#[derive(Debug, Default, Clone)]
pub struct TokenChanges {
    /// New name.
    pub name: Option<String>,
    /// Replacement scope set; at least one scope must remain.
    pub scopes: Option<Vec<String>>,
    /// Replacement origin allow-list.
    pub allowed_origins: Option<Vec<String>>,
    /// Replacement rate limit.
    pub rate_limit_per_minute: Option<i32>,
    /// Replacement expiry.
    pub expires_at: Option<Option<OffsetDateTime>>,
}

/// The expiry presets the create dialog offers, in days.
pub const EXPIRY_PRESETS: [(&str, i32); 4] = [
    ("30 days", 30),
    ("90 days", 90),
    ("365 days", 365),
    ("never", 0),
];

/// Why a presented secret was refused.
///
/// The variants are separate because the caller's answer differs: an expired token is a
/// rotation, a revoked one is a deletion, a scope failure is a different endpoint. Collapsing
/// them into `None` would make every one of them `401 invalid_token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// No token carries this prefix, or the secret does not match.
    Invalid,
    /// The token is past `expires_at`.
    Expired,
    /// The token was revoked.
    Revoked,
}

impl AuthFailure {
    /// The platform error code this failure maps to.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "invalid_token",
            Self::Expired => "token_expired",
            Self::Revoked => "token_revoked",
        }
    }
}

/// A token that authenticated successfully.
#[derive(Debug, Clone)]
pub struct AuthenticatedToken {
    /// The record, for scope and site checks.
    pub token: ApiToken,
}

/// The columns a list read selects. Centralised so the list and the summary cannot drift — a
/// list that forgets a column fails to compile, which is the cheap outcome; two hand-written
/// SELECTs that disagree show up as a panel that renders `null` in a column.
const LIST_COLUMNS: &str = "id, organization_id, site_id, name, prefix, scopes, \
     allowed_origins, rate_limit_per_minute, expires_at, revoked_at, last_used_at, created_by, \
     created_at";

/// Validate a display name.
pub fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidName("a name is required".to_string()));
    }
    if trimmed.chars().count() > MAX_NAME_LENGTH {
        return Err(ContentError::InvalidName(format!(
            "a name may be at most {MAX_NAME_LENGTH} characters"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Validate a scope set: non-empty, known, deduplicated, order-preserving.
pub fn validate_scopes(scopes: &[String]) -> Result<Vec<String>> {
    if scopes.is_empty() {
        return Err(ContentError::InvalidText(
            "pick at least one scope".to_string(),
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for scope in scopes {
        let trimmed = scope.trim();
        if !SCOPES.contains(&trimmed) {
            return Err(ContentError::InvalidText(format!(
                "unknown scope: {trimmed}"
            )));
        }
        if !seen.iter().any(|existing| existing == trimmed) {
            seen.push(trimmed.to_owned());
        }
    }
    Ok(seen)
}

/// Validate an origin allow-list.
///
/// An origin is `scheme://host[:port]` with **no path, no trailing slash and no wildcard**. The
/// strictness is the point: `https://app.example.com/*` reads like a prefix match, and a check
/// that accepted it would either silently not match or silently match every path, and an origin
/// allow-list that does not mean what it says is worse than none.
pub fn validate_origins(origins: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for origin in origins {
        let trimmed = origin.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_valid_origin(trimmed) {
            return Err(ContentError::InvalidText(format!(
                "not an origin (scheme://host[:port]): {trimmed}"
            )));
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    Ok(out)
}

/// Whether a string is exactly `scheme://host[:port]`.
#[must_use]
pub fn is_valid_origin(candidate: &str) -> bool {
    let Some((scheme, rest)) = candidate.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    if rest.is_empty() || rest.contains('/') || rest.contains('*') || rest.contains(' ') {
        return false;
    }
    // `rsplit_once(':')` on a bare IPv6 literal returns a "port" that is not digits, so the
    // digit test is what keeps `https://[::1]` a host rather than a host with a bad port.
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => (host, Some(port)),
        _ => (rest, None),
    };
    if host.is_empty() || host.contains('/') {
        return false;
    }
    if let Some(port) = port {
        match port.parse::<u32>() {
            Ok(number) if (1..=65_535).contains(&number) => {}
            _ => return false,
        }
    }
    true
}

/// Validate a rate-limit tier.
pub fn validate_rate_limit(value: i32) -> Result<i32> {
    match value {
        RATE_TIER_STANDARD | RATE_TIER_ELEVATED => Ok(value),
        other => Err(ContentError::InvalidText(format!(
            "rate limit must be {RATE_TIER_STANDARD} or {RATE_TIER_ELEVATED}, got {other}"
        ))),
    }
}

/// The expiry for a preset, `None` for "never".
#[must_use]
pub fn expiry_from_preset(preset: i32, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if preset <= 0 {
        return None;
    }
    now.checked_add(time::Duration::days(i64::from(preset)))
}

/// SHA-256 of the secret half, hex-encoded.
///
/// `sha256` and not a password hash, for a reason worth stating once: this secret is 32 characters
/// drawn from a CSPRNG, so there is nothing to brute-force. The property wanted is that a leaked
/// table contains no working link — which is also why the comparison below is constant-time and
/// not a database `=`.
#[must_use]
pub fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compare two hex digests without leaking where they differ.
#[must_use]
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Split a presented token into its prefix and secret halves.
pub fn split_token(token: &str) -> Option<(String, String)> {
    let rest = token.strip_prefix(&format!("{TOKEN_NAMESPACE}_"))?;
    let (prefix, secret) = rest.split_once('_')?;
    if prefix.len() != PREFIX_LENGTH || secret.len() != SECRET_LENGTH {
        return None;
    }
    if !prefix
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    {
        return None;
    }
    if !secret
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    {
        return None;
    }
    Some((prefix.to_owned(), secret.to_owned()))
}

/// A fresh `omn_<prefix>_<secret>` triple.
fn fresh_material() -> (String, String, String) {
    let prefix = format!("{TOKEN_NAMESPACE}_{}", random_chars(PREFIX_LENGTH));
    let secret = random_chars(SECRET_LENGTH);
    let plaintext = format!("{prefix}_{secret}");
    (prefix, secret, plaintext)
}

/// `length` random lowercase alphanumerics, drawn from the operating system.
fn random_chars(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0_u8; length];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| ALPHABET[(byte as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Mint a token. The returned plaintext is the only copy that will ever exist.
pub async fn create_token(
    pool: &PgPool,
    organization_id: Uuid,
    site_id: Option<Uuid>,
    name: &str,
    scopes: &[String],
    allowed_origins: &[String],
    rate_limit_per_minute: i32,
    expires_at: Option<OffsetDateTime>,
    created_by: Option<Uuid>,
) -> Result<NewToken> {
    let name = validate_name(name)?;
    let scopes = validate_scopes(scopes)?;
    let allowed_origins = validate_origins(allowed_origins)?;
    let rate_limit_per_minute = validate_rate_limit(rate_limit_per_minute)?;

    // A collision on the 8-hex prefix is a 2^-32 event; the retry exists because "unique
    // violation" would otherwise surface as a 500 to a person who did nothing wrong.
    for _ in 0..5 {
        let (prefix, secret, plaintext) = fresh_material();
        let row = sqlx::query_as::<_, ApiToken>(&format!(
            "insert into api_tokens (organization_id, site_id, name, prefix, token_hash, scopes, \
             allowed_origins, rate_limit_per_minute, expires_at, created_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             returning {LIST_COLUMNS}"
        ))
        .bind(organization_id)
        .bind(site_id)
        .bind(&name)
        .bind(&prefix)
        .bind(hash_secret(&secret))
        .bind(&scopes)
        .bind(&allowed_origins)
        .bind(rate_limit_per_minute)
        .bind(expires_at)
        .bind(created_by)
        .fetch_optional(pool)
        .await;
        match row {
            Ok(Some(token)) => {
                return Ok(NewToken { token, plaintext });
            }
            Ok(None) => return Err(ContentError::InvalidName("token not created".to_string())),
            Err(sqlx::Error::Database(ref db_error)) if db_error.is_unique_violation() => {
                let message = db_error.message().to_lowercase();
                // A unique violation on `lower(name)` is a duplicate name, not a prefix clash, and
                // the two need different messages: one is the caller's fault, the other is ours.
                if message.contains("api_tokens_org_name_lower") || message.contains("name") {
                    return Err(ContentError::TokenNameTaken(name));
                }
                continue;
            }
            Err(other) => return Err(ContentError::Database(other)),
        }
    }
    Err(ContentError::InvalidName(
        "could not allocate a unique token prefix".to_string(),
    ))
}

/// List an organization's tokens, newest first.
pub async fn list_tokens(pool: &PgPool, organization_id: Uuid) -> Result<Vec<ApiToken>> {
    let tokens = sqlx::query_as::<_, ApiToken>(&format!(
        "select {LIST_COLUMNS} from api_tokens where organization_id = $1 \
         order by created_at desc"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(tokens)
}

/// One token, if it belongs to the organization.
pub async fn get_token(
    pool: &PgPool,
    organization_id: Uuid,
    token_id: Uuid,
) -> Result<Option<ApiToken>> {
    let token = sqlx::query_as::<_, ApiToken>(&format!(
        "select {LIST_COLUMNS} from api_tokens where id = $1 and organization_id = $2"
    ))
    .bind(token_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(token)
}

/// Apply an edit. A `None` field is left alone; a `Some` field replaces.
pub async fn update_token(
    pool: &PgPool,
    organization_id: Uuid,
    token_id: Uuid,
    changes: &TokenChanges,
) -> Result<Option<ApiToken>> {
    let name = changes.name.as_deref().map(validate_name).transpose()?;
    let scopes = changes.scopes.as_deref().map(validate_scopes).transpose()?;
    let origins = changes
        .allowed_origins
        .as_deref()
        .map(validate_origins)
        .transpose()?;
    let rate = changes.rate_limit_per_minute.map(validate_rate_limit).transpose()?;

    let row = sqlx::query_as::<_, ApiToken>(&format!(
        "update api_tokens set \
           name = coalesce($3, name), \
           scopes = coalesce($4, scopes), \
           allowed_origins = coalesce($5, allowed_origins), \
           rate_limit_per_minute = coalesce($6, rate_limit_per_minute), \
           expires_at = case when $7::boolean then $8 else expires_at end, \
           updated_at = now() \
         where id = $1 and organization_id = $2 \
         returning {LIST_COLUMNS}"
    ))
    .bind(token_id)
    .bind(organization_id)
    .bind(name)
    .bind(scopes)
    .bind(origins)
    .bind(rate)
    .bind(changes.expires_at.is_some())
    .bind(changes.expires_at.flatten())
    .fetch_optional(pool)
    .await
    .map_err(map_token_error)?;
    Ok(row)
}

/// Rotate: issue a new secret for the same token. The previous secret stops working at once,
/// because the row now holds one hash and there is no second column that could be consulted.
pub async fn rotate_token(
    pool: &PgPool,
    token_id: Uuid,
) -> Result<Option<(ApiToken, String)>> {
    let (_, secret, plaintext) = fresh_material();
    let row = sqlx::query_as::<_, ApiToken>(&format!(
        "update api_tokens set token_hash = $2, updated_at = now() where id = $1 \
         returning {LIST_COLUMNS}"
    ))
    .bind(token_id)
    .bind(hash_secret(&secret))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|token| (token, plaintext)))
}

/// Revoke. Idempotent: revoking twice answers the same as revoking once, so a double-click on a
/// confirm button is not a 404.
pub async fn revoke_token(pool: &PgPool, organization_id: Uuid, token_id: Uuid) -> Result<bool> {
    let result = sqlx::query(
        "update api_tokens set revoked_at = coalesce(revoked_at, now()), updated_at = now() \
         where id = $1 and organization_id = $2",
    )
    .bind(token_id)
    .bind(organization_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Authenticate a presented token.
///
/// One lookup by prefix, one constant-time comparison, one verdict. The `last_used_at` write is
/// deliberately best-effort: metering is a report, and a report that can fail a request is a
/// report that can take a site down.
pub async fn authenticate(
    pool: &PgPool,
    organization_id: Uuid,
    presented: &str,
) -> std::result::Result<AuthenticatedToken, AuthFailure> {
    let Some((prefix, secret)) = split_token(presented) else {
        return Err(AuthFailure::Invalid);
    };
    let row = sqlx::query_as::<_, TokenForAuth>(&format!(
        "select {LIST_COLUMNS}, token_hash from api_tokens where prefix = $1"
    ))
    .bind(&prefix)
    .fetch_optional(pool)
    .await
    .map_err(|_| AuthFailure::Invalid)?;

    let Some(row) = row else {
        return Err(AuthFailure::Invalid);
    };
    if row.token.organization_id != organization_id {
        // A token from another organization is *invalid*, not "forbidden": telling the caller it
        // exists is itself a leak, and the route answers 401 like any other unknown prefix.
        return Err(AuthFailure::Invalid);
    }
    if !constant_time_eq(&hash_secret(&secret), &row.token_hash) {
        return Err(AuthFailure::Invalid);
    }
    if row.token.revoked_at.is_some() {
        return Err(AuthFailure::Revoked);
    }
    if let Some(expiry) = row.token.expires_at {
        if OffsetDateTime::now_utc() >= expiry {
            return Err(AuthFailure::Expired);
        }
    }
    let _ = sqlx::query("update api_tokens set last_used_at = now() where id = $1")
        .bind(row.token.id)
        .execute(pool)
        .await;
    Ok(AuthenticatedToken { token: row.token })
}

/// The authenticated row, which is the only place the digest is ever read.
///
/// A second row type rather than a nullable field on [`ApiToken`]: the list, the panel payload and
/// every error path have no business holding a digest, and a struct that *can* hold one will
/// eventually be logged. Here the type system says the digest exists only in this function.
#[derive(Debug, sqlx::FromRow)]
struct TokenForAuth {
    #[sqlx(flatten)]
    token: ApiToken,
    token_hash: String,
}

/// Map a database error to the store's vocabulary.
fn map_token_error(err: sqlx::Error) -> ContentError {
    match err {
        sqlx::Error::Database(ref db_error) if db_error.is_unique_violation() => {
            ContentError::Database(err)
        }
        other => ContentError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_shaped_token_splits_into_prefix_and_secret() {
        let token = format!("{TOKEN_NAMESPACE}_{}_{}", "a".repeat(PREFIX_LENGTH), "b".repeat(SECRET_LENGTH));
        let (prefix, secret) = split_token(&token).expect("a well-shaped token must split");
        assert_eq!(prefix, "a".repeat(PREFIX_LENGTH));
        assert_eq!(secret.len(), SECRET_LENGTH);
    }

    #[test]
    fn malformed_tokens_are_refused_before_any_query() {
        for token in [
            String::new(),
            "short_abc_def".to_string(),
            format!("{}_{}", "A".repeat(PREFIX_LENGTH), "b".repeat(SECRET_LENGTH)),
            format!("{}_{}", "a".repeat(PREFIX_LENGTH), "b".repeat(SECRET_LENGTH - 1)),
            format!("{}_{}", "a".repeat(PREFIX_LENGTH), "B".repeat(SECRET_LENGTH)),
            "omsa_abcdefghij_kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk".to_string(),
        ] {
            assert!(
                split_token(&token).is_none(),
                "must refuse {token:?} before touching the database"
            );
        }
    }

    #[test]
    fn a_digest_compares_equal_only_to_itself() {
        let left = hash_secret("secret");
        assert!(constant_time_eq(&left, &hash_secret("secret")));
        assert!(!constant_time_eq(&left, &hash_secret("secreT")));
        assert!(!constant_time_eq(&left, "short"));
    }

    #[test]
    fn every_digest_is_sixty_four_hex_characters() {
        // The column carries `check (length(token_hash) = 64)`, so a wrong digest length is a
        // database error at INSERT time -- far from where the mistake is.
        let digest = hash_secret("anything");
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn an_origin_allow_list_refuses_anything_that_is_not_an_exact_origin() {
        for good in [
            "https://app.example.com",
            "http://localhost:3000",
            "https://example.com:8443",
        ] {
            assert!(is_valid_origin(good), "{good} must be accepted");
        }
        for bad in [
            "https://app.example.com/",
            "https://app.example.com/*",
            "https://app.example.com/path",
            "app.example.com",
            "ftp://example.com",
            "https://",
            "https://example.com:0",
            "https://example.com:99999",
            "https://exa mple.com",
            "javascript:alert(1)",
        ] {
            assert!(!is_valid_origin(bad), "{bad} must be refused");
        }
    }

    #[test]
    fn a_scope_set_needs_at_least_one_known_scope() {
        assert!(validate_scopes(&[]).is_err());
        assert!(validate_scopes(&["content:read".to_string()]).is_ok());
        assert!(validate_scopes(&["content:admin".to_string()]).is_err());
        // Duplicates collapse rather than storing the same scope twice.
        let deduped =
            validate_scopes(&["content:read".to_string(), "content:read".to_string()]).unwrap();
        assert_eq!(deduped, vec!["content:read".to_string()]);
    }

    #[test]
    fn a_reserved_write_scope_is_accepted_by_the_store_and_documented_as_unimplemented() {
        // REQ-019 reserves `content:write` by name. The store must accept it so a future
        // implementation does not need a migration, and it must be the *route* that refuses.
        let scopes = validate_scopes(&["content:write".to_string()]).expect("reserved by name");
        assert_eq!(scopes, vec!["content:write".to_string()]);
    }

    #[test]
    fn a_rate_limit_off_the_tier_list_is_refused() {
        assert_eq!(validate_rate_limit(RATE_TIER_STANDARD).unwrap(), 120);
        assert_eq!(validate_rate_limit(RATE_TIER_ELEVATED).unwrap(), 600);
        assert!(validate_rate_limit(0).is_err());
        assert!(validate_rate_limit(601).is_err());
    }

    #[test]
    fn each_refusal_has_its_own_error_code() {
        assert_eq!(AuthFailure::Invalid.code(), "invalid_token");
        assert_eq!(AuthFailure::Expired.code(), "token_expired");
        assert_eq!(AuthFailure::Revoked.code(), "token_revoked");
    }

    #[test]
    fn a_name_is_trimmed_and_bounded() {
        assert_eq!(validate_name("  Frontend  ").unwrap(), "Frontend");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_LENGTH)).is_ok());
        assert!(validate_name(&"x".repeat(MAX_NAME_LENGTH + 1)).is_err());
    }

    #[test]
    fn a_never_expiry_is_no_expiry() {
        let now = OffsetDateTime::now_utc();
        assert!(expiry_from_preset(0, now).is_none());
        let preset = expiry_from_preset(30, now).expect("30 days is an expiry");
        assert!(preset > now);
    }
}
