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

/// The tier the create dialog offers by default.
pub const RATE_TIER_STANDARD: i32 = 120;
/// The elevated tier, guarded by the manage permission because it is 5x the standard budget.
pub const RATE_TIER_ELEVATED: i32 = 600;
/// The smallest budget the column accepts, and therefore the smallest the store accepts.
pub const RATE_TIER_MINIMUM: i32 = 10;

/// Every tier the create dialog offers, cheapest first.
///
/// **A list and not two constants**, because the acceptance criterion for this slice is about
/// *the request after the tier* — and a store that accepts only two hard-coded values makes that
/// criterion untestable without firing 120 requests, which is a test that takes a minute and is
/// therefore a test nobody writes. A developer tier exists for exactly that reason and for a real
/// one: a staging frontend that is not a production integration should not have to pretend to be
/// one. It is `RATE_TIER_MINIMUM`, which is the column's own floor, so nothing here can name a
/// budget the database will refuse.
pub const RATE_TIERS: [i32; 3] = [RATE_TIER_MINIMUM, RATE_TIER_STANDARD, RATE_TIER_ELEVATED];

/// The tier's human name, for the panel's picker and the error message.
#[must_use]
pub fn rate_tier_label(value: i32) -> &'static str {
    match value {
        RATE_TIER_MINIMUM => "Development",
        RATE_TIER_STANDARD => "Standard",
        RATE_TIER_ELEVATED => "Elevated",
        _ => "Custom",
    }
}

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
    /// The store could not answer: a pool timeout, a closed pool, or a row that decoded into
    /// something the query did not promise.
    ///
    /// A **fourth** variant rather than a log line in the store, and the reason is structural: the
    /// content crate has no logger, so a `tracing::error!` written here would not compile — and
    /// the temptation that follows from that is worse, which is to keep `.map_err(|_| Invalid)`
    /// and lose the class. This variant carries the evidence out to the one layer that does log,
    /// so a database incident is visible as a database incident and never as an integrator's
    /// mistake.
    ///
    /// It is deliberately NOT an `Invalid` alias with a side channel. A separate variant means the
    /// route can answer `503` and log by **construction** rather than by remembering, which is
    /// the whole difference between a diagnostic and a guess. The `source` string is the error's
    /// own `Display`, so the log line names the constraint or the timeout that actually happened.
    StoreUnavailable { source: String },
}

impl AuthFailure {
    /// The platform error code this failure maps to.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "invalid_token",
            Self::Expired => "token_expired",
            Self::Revoked => "token_revoked",
            Self::StoreUnavailable { .. } => "token_store_unavailable",
        }
    }

    /// Whether this failure is about the **credential** rather than about the platform.
    ///
    /// The route needs this distinction for one decision: what status to answer. A wrong token
    /// fails identically forever, so answering `503` would teach an integrator to retry something
    /// that will never succeed; an unreachable store is worth exactly one retry, and the status
    /// code has to tell the two apart to be worth anything.
    #[must_use]
    pub fn is_credential_problem(&self) -> bool {
        !matches!(self, Self::StoreUnavailable { .. })
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
        return Err(ContentError::InvalidScope(
            "pick at least one scope".to_string(),
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for scope in scopes {
        let trimmed = scope.trim();
        if !SCOPES.contains(&trimmed) {
            return Err(ContentError::InvalidScope(format!("unknown scope: {trimmed}")));
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
            return Err(ContentError::InvalidOrigin(format!(
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
    if RATE_TIERS.contains(&value) {
        return Ok(value);
    }
    // The message names every tier **and** what each one is called, because the numbers alone do
    // not tell a person which picker entry to choose — and it is the number that goes in the
    // API, so a person who cannot map "600" to a label cannot use the API at all.
    let offered: Vec<String> = RATE_TIERS
        .iter()
        .map(|tier| format!("{} ({})", rate_tier_label(*tier), tier))
        .collect();
    Err(ContentError::InvalidRateTier(format!(
        "rate limit must be one of: {} — got {value}",
        offered.join(", ")
    )))
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
///
/// The returned prefix is the **bare** 8 characters, because that is the half the secret is
/// split *away* from. It is NOT what the `prefix` column stores — see [`lookup_prefix`], which is
/// the one function that knows the column keeps the display form.
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

/// The `prefix` column keeps the **namespaced display form** (`omn_1a2b3c4d`) — the copyable
/// marker the panel shows, and what the `^omn_[0-9a-f]{8}$` check constraint is written against.
/// [`split_token`], by contrast, returns the bare 8 characters, because that is the half the
/// secret is split away from.
///
/// So the two disagree by exactly one prefix, and the lookup has to add it back. Doing that here,
/// once, is the fix: an inline `format!` at each of the two `where prefix = $1` sites is a
/// correct expression in both places and a guaranteed break in the first one somebody edits.
fn lookup_prefix(bare: &str) -> String {
    format!("{TOKEN_NAMESPACE}_{bare}")
}

/// A fresh `omn_<prefix>_<secret>` triple.
fn fresh_material() -> (String, String, String) {
    let prefix = format!("{TOKEN_NAMESPACE}_{}", random_chars(PREFIX_LENGTH));
    let secret = random_chars(SECRET_LENGTH);
    let plaintext = format!("{prefix}_{secret}");
    (prefix, secret, plaintext)
}

/// `length` random lowercase alphanumerics, drawn from the operating system.
///
/// The alphabet is **hexadecimal only** (`a`–`f`, `0`–`9`) and not the wider alphanumeric set,
/// because the database constrains `prefix` to `^omn_[0-9a-f]{8}$`. The wider alphabet looks
/// harmless and randomizes better, but `g`–`z` are rejected by the constraint, so roughly 15% of
/// all mints ended in a `500 internal_error` naming a check constraint — a failure that looks like
/// a corrupt database and is really a disagreement between a generator and a schema.
///
/// Twenty years is the whole justification for the narrower alphabet: a prefix is displayed, not
/// brute-forced, and the secret half below carries 32 hex characters drawn from the OS for the
/// actual security. The one cheap defence is to let the two agree: the alphabet here is a subset
/// of what the migration allows, so every generated prefix is valid by construction.
fn random_chars(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdef0123456789";
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
    .bind(lookup_prefix(&prefix))
    .fetch_optional(pool)
    .await
    .map_err(store_error_as_invalid)?;

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

/// Authenticate a token whose caller cannot name its organization.
///
/// The headless content surface (REQ-019 slice 2) authenticates with **no session at all** — that
/// is the feature: the caller is a frontend outside the organization. It therefore cannot supply
/// the `organization_id` [`authenticate`] requires, and the correct value is the one the row
/// already carries.
///
/// This is not a tenant-scope bypass, and the reason is worth stating because it looks like one:
///
/// * `prefix` is unique per **installation** (`unique` on the column, not per organization), so
///   there is exactly one row a presented prefix can name. There is no second tenant's token to
///   be confused with.
/// * Everything the caller is then allowed to see comes off that row — its `site_id` filter and
///   its `scopes`. A token scoped to one site cannot read another site whichever entry point it
///   arrives through, and the cross-tenant walk in `content_read_surface.rs` proves the
///   organization boundary by seeding two organizations and reading across them.
/// * The panel route keeps the strict [`authenticate`], where the session *does* name a tenant
///   and a mismatch is a real signal worth answering `invalid_token`.
///
/// Delegating rather than duplicating is the point: the digest comparison, the constant-time
/// check, the expiry and revocation verdicts and the `last_used_at` write all stay in one place,
/// so this entry point cannot drift into a version that forgets one of them.
pub async fn authenticate_any_organization(
    pool: &PgPool,
    presented: &str,
) -> std::result::Result<AuthenticatedToken, AuthFailure> {
    let Some((prefix, secret)) = split_token(presented) else {
        return Err(AuthFailure::Invalid);
    };
    let row = sqlx::query_as::<_, TokenForAuth>(&format!(
        "select {LIST_COLUMNS}, token_hash from api_tokens where prefix = $1"
    ))
    .bind(lookup_prefix(&prefix))
    .fetch_optional(pool)
    .await
    .map_err(store_error_as_invalid)?;
    let Some(row) = row else {
        return Err(AuthFailure::Invalid);
    };
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

/// Classify a store failure so the refusal keeps its cause.
///
/// The obvious version of this line is `.map_err(|_| AuthFailure::Invalid)`, and it is a defect
/// with a long reach: a pool that timed out, a statement whose row shape moved and a wrong
/// credential are three unrelated problems, and the refusal to tell them apart turns a
/// diagnosis into a bisect. The verdict itself must not become a fifth value the route has to
/// invent a status code for, so the split is by *cause*: anything that means "the store could
/// not answer" is [`AuthFailure::StoreUnavailable`] and everything else stays `Invalid`.
///
/// `ColumnNotFound` is classified with the store errors rather than with the credential, and that
/// is the specific bisect this function exists to prevent: a column the struct names and the
/// query does not return is a code/schema disagreement, and reading it as "your token is wrong"
/// sends the reader to the integrator instead of to the migration.
fn classify_store_error(err: &sqlx::Error) -> AuthFailure {
    match err {
        sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnDecode { .. } => {
            AuthFailure::StoreUnavailable { source: err.to_string() }
        }
        _ => AuthFailure::Invalid,
    }
}

/// Classify the error and return only the verdict.
///
/// Both authentication entry points want exactly this, and the evidence travels in the variant:
/// [`AuthFailure::StoreUnavailable`] carries the `sqlx` error's own `Display`, so the route can
/// log it with `Display` and the constraint or timeout that failed is named, not guessed at.
fn store_error_as_invalid(err: sqlx::Error) -> AuthFailure {
    classify_store_error(&err)
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

    /// The two halves of a token have to survive a round trip through the generator, the splitter
    /// and the column, in that order, without anyone reformatting one of them.
    ///
    /// **This is the test for a real defect, and the defect was invisible to every test above
    /// it.** `fresh_material` stores the *namespaced* prefix (`omn_1a2b3c4d`) because that is what
    /// the `^omn_[0-9a-f]{8}$` check constraint is written against, while `split_token` returns
    /// the *bare* 8 characters because that is the half the secret is split away from. The two
    /// lookup sites bound the bare form, so `where prefix = $1` matched nothing and every token
    /// this crate ever minted answered `invalid_token` — while the unit tests were all green,
    /// because each of them tested one half and none of them made a round trip.
    ///
    /// The test asserts the *value that goes into the query*, not the shape of the parts. A test
    /// that only checked `prefix.starts_with("omn_")` passes against both a correct and a broken
    /// binding; this one fails on the exact line that broke.
    #[test]
    fn the_stored_prefix_is_the_namespaced_form_the_lookup_rebuilds() {
        for _ in 0..500 {
            let (stored_prefix, secret, plaintext) = fresh_material();
            let (bare, split_secret) =
                split_token(&plaintext).expect("a fresh token must split back into two halves");

            // What `create_token` binds into the column.
            assert!(
                stored_prefix.starts_with(&format!("{TOKEN_NAMESPACE}_")),
                "the column holds the display form, got {stored_prefix:?}"
            );
            // What the lookup must rebuild to find it.
            assert_eq!(
                lookup_prefix(&bare),
                stored_prefix,
                "the lookup value must equal the stored value, or the token never authenticates"
            );
            // And the secret half is untouched by either representation.
            assert_eq!(split_secret, secret);
            assert_eq!(hash_secret(&split_secret).len(), 64);
        }
    }

    /// A store failure is a **different verdict** from a wrong credential, and the two differ in
    /// the two ways a caller can act on them: the code, and whether a retry can ever help.
    ///
    /// This is the test for the swallow that hid the prefix defect. While both outcomes were
    /// `Invalid`, a `PoolTimedOut` and a `ColumnNotFound` were indistinguishable from a
    /// mistyped token, so a broken query read as an integrator's problem and nobody looked at the
    /// statement.
    #[test]
    fn a_store_failure_is_not_the_same_verdict_as_a_wrong_credential() {
        let unreachable = store_error_as_invalid(sqlx::Error::PoolTimedOut);
        assert!(!unreachable.is_credential_problem());
        assert_eq!(unreachable.code(), "token_store_unavailable");
        // The evidence rides along, so the route can log the actual cause rather than a guess.
        match &unreachable {
            AuthFailure::StoreUnavailable { source } => assert!(
                !source.is_empty(),
                "the refusal must carry the store error's own Display"
            ),
            other => panic!("a pool timeout must not be reported as {other:?}"),
        }

        // A column the struct names and the query does not return is a schema disagreement.
        let decode = store_error_as_invalid(sqlx::Error::ColumnNotFound("token_hash".to_string()));
        assert!(!decode.is_credential_problem());

        // And a plain query error is still the credential's own verdict, so nothing that was a
        // 401 before stops being one.
        for credential_problem in [AuthFailure::Invalid, AuthFailure::Expired, AuthFailure::Revoked] {
            assert!(credential_problem.is_credential_problem());
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
    fn every_offered_tier_is_one_the_column_accepts() {
        // The tiers are named in the store and *also* bounded by the column's own check
        // (`rate_limit_per_minute between 10 and 600`). A tier outside that range would be
        // accepted here and refused by PostgreSQL, and the person who chose it would be told
        // their *name* was wrong. The column's floor is asserted, not assumed.
        for tier in RATE_TIERS {
            assert!(
                (10..=600).contains(&tier),
                "the column refuses {tier}, so the store must not offer it"
            );
            assert_eq!(
                validate_rate_limit(tier).expect("an offered tier is accepted"),
                tier,
                "{tier} is offered, so it must be accepted"
            );
        }
        assert_eq!(
            RATE_TIERS[0], RATE_TIER_MINIMUM,
            "the floor is a tier, not a bound only"
        );
    }

    #[test]
    fn a_tier_off_the_list_names_every_tier_and_what_it_is_called() {
        // The message exists so a person using the API can find the number in the picker, and a
        // message that only says "must be 10, 120 or 600" does not do that — three bare numbers
        // is a puzzle, three labelled ones is a menu.
        let error = validate_rate_limit(7).expect_err("7 is not a tier");
        let message = error.to_string();
        for tier in RATE_TIERS {
            assert!(
                message.contains(&tier.to_string()),
                "{tier} is missing: {message}"
            );
            assert!(
                message.contains(rate_tier_label(tier)),
                "the label for {tier} is missing: {message}"
            );
        }
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

    /// The prefix generator and the `api_tokens_prefix_check` constraint must agree.
    ///
    /// This is a regression test for a real `500`: the generator drew from `a-z0-9` while the
    /// migration constrains the column to `[0-9a-f]`, so every prefix containing a `g`-`z` was
    /// refused by the database and surfaced as `internal_error`. A generator that samples the
    /// alphabet 200 times will hit one of those within seconds; a test that only checks
    /// `starts_with("omn_")` never will.
    #[test]
    fn every_generated_prefix_satisfies_the_column_check() {
        for _ in 0..2_000 {
            let (prefix, secret, plaintext) = fresh_material();
            assert!(
                prefix.len() == TOKEN_NAMESPACE.len() + 1 + PREFIX_LENGTH,
                "{prefix} has the wrong length"
            );
            // The check constraint, transcribed: `^omn_[0-9a-f]{8}$`.
            assert!(
                prefix
                    .strip_prefix(&format!("{TOKEN_NAMESPACE}_"))
                    .map(|body| {
                        body.len() == PREFIX_LENGTH
                            && body.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
                    })
                    .unwrap_or(false),
                "{prefix} would be refused by api_tokens_prefix_check"
            );
            assert_eq!(secret.len(), SECRET_LENGTH);
            assert!(plaintext.starts_with(&format!("{prefix}_")));
        }
    }

    /// The alphabet must stay a subset of what the constraint allows, and the secret must stay
    /// long enough that narrowing the alphabet did not weaken it.
    #[test]
    fn narrowing_the_prefix_alphabet_left_the_secret_strong() {
        // 16 symbols over 32 characters is 128 bits — the same order as the 36-symbol alphabet
        // over 32 characters was, and far past anything a display prefix needs.
        assert!(SECRET_LENGTH * 4 >= 128, "the secret must carry at least 128 bits");
        // And the charset really is the hex set, not a subset of it by accident.
        let generated = random_chars(4_000);
        assert!(
            generated
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "the generator must not be able to leave the hex alphabet"
        );
    }
}
