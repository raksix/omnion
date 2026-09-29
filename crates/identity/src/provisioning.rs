//! SCIM provisioning tokens and the sync log (docs/07-IAM.md §19; REQ-006, slice 4b).
//!
//! An identity provider that speaks SCIM 2.0 authenticates with a token minted here. The secret
//! is shown exactly once — at minting — and only its SHA-256 hash is stored, so a database read
//! cannot replay a token. Every call a token makes lands in `provisioning_log`: ids, actions and
//! outcomes, never personal payloads, because the log is read by support and reviewed long after
//! the request it describes.
//!
//! Tokens are rotatable (revoke the old one, mint a new one) and carry a public prefix so the
//! panel can name the exact token a sync came from without holding the secret.

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Namespace of every token this module mints (`omsc` = Omnion SCIM).
pub const TOKEN_NAMESPACE: &str = "omsc";

/// Characters of the public prefix — enough to tell two tokens apart, too few to use.
const PREFIX_LENGTH: usize = 8;

/// Characters of the secret half (hex of 20 random bytes).
const SECRET_LENGTH: usize = 40;

/// Page size of the sync log when the caller does not ask for one.
pub const DEFAULT_LOG_LIMIT: i64 = 50;

/// Largest page of the sync log a caller may ask for.
pub const MAX_LOG_LIMIT: i64 = 500;

/// Column list of every token query.
const TOKEN_COLUMNS: &str = "id, organization_id, name, prefix, created_by, last_used_at, \
     revoked_at, created_at, expires_at, rotated_at";

/// How long a token lives when the caller does not say. Ninety days: long enough that a connector
/// configured once keeps working through a holiday, short enough that a leaked token has a
/// deadline an operator can see on the list rather than a discovery.
pub const DEFAULT_TOKEN_TTL_DAYS: i64 = 90;

/// The longest a caller may ask a token to live.
pub const MAX_TOKEN_TTL_DAYS: i64 = 365;

/// One provisioning token as the panel reads it — never the secret.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProvisioningToken {
    /// Primary key.
    pub id: Uuid,
    /// Organization the token provisions into.
    pub organization_id: Uuid,
    /// Label the reader gave it.
    pub name: String,
    /// Public prefix (`omsc_ab12cd34`).
    pub prefix: String,
    /// Who minted it.
    pub created_by: Option<Uuid>,
    /// When it was last used, if ever.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it was revoked, if it was.
    pub revoked_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// When it stops being accepted; `None` only for tokens minted before expiry existed.
    pub expires_at: Option<OffsetDateTime>,
    /// Set when a newer token replaced this one.
    pub rotated_at: Option<OffsetDateTime>,
}

/// A minted token: the row, plus the secret the caller can see exactly this once.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// Stored row.
    pub token: ProvisioningToken,
    /// The full secret (`omsc_<prefix>_<secret>`); shown once and never stored.
    pub secret: String,
}

/// Column list of every sync-log query.
const LOG_COLUMNS: &str = "id, organization_id, direction, resource, external_id, entity_id, \
     action, outcome, detail, created_at";

/// One line of the sync log.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncLogEntry {
    /// Primary key (bigserial).
    pub id: i64,
    /// Organization the sync touched.
    pub organization_id: Uuid,
    /// `inbound` (the provider called us) or `outbound`.
    pub direction: String,
    /// `user` or `group`.
    pub resource: String,
    /// Id the provider uses for the entity, when it sent one.
    pub external_id: Option<String>,
    /// Our own id of the entity, when a row exists.
    pub entity_id: Option<Uuid>,
    /// What was attempted (`create`, `update`, `deactivate`, `delete`, …).
    pub action: String,
    /// `created`, `updated`, `deactivated`, `failed` or `skipped`.
    pub outcome: String,
    /// One readable line about what happened.
    pub detail: String,
    /// When it happened.
    pub created_at: OffsetDateTime,
}

/// A sync-log line to write.
#[derive(Debug, Clone)]
pub struct NewSyncEntry {
    /// `inbound` or `outbound`.
    pub direction: String,
    /// `user` or `group`.
    pub resource: String,
    /// Id the provider uses.
    pub external_id: Option<String>,
    /// Our id, when a row exists.
    pub entity_id: Option<Uuid>,
    /// What was attempted.
    pub action: String,
    /// What happened.
    pub outcome: String,
    /// One readable line.
    pub detail: String,
}

/// SHA-256 of a secret, hex encoded — the only form that reaches the database.
#[must_use]
pub fn hash_token(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Mint a token for an organization. The secret is returned here and never again.
///
/// `ttl_days` is the *requested* lifetime; the caller normally passes `None` and gets the default
/// rather than having to know the number. The expiry is computed by the database (`now() + …`)
/// rather than in Rust, because the row's `created_at` is the database's `now()` too — computing
/// it on two clocks is how a token ends up with an expiry *before* its creation and trips the
/// constraint, or a second in the past, which reads to an operator as "already expired".
pub async fn create_token(
    pool: &PgPool,
    organization_id: Uuid,
    name: &str,
    created_by: Option<Uuid>,
) -> Result<IssuedToken> {
    create_token_with_ttl(pool, organization_id, name, created_by, None).await
}

/// Mint a token with an explicit lifetime, in days.
pub async fn create_token_with_ttl(
    pool: &PgPool,
    organization_id: Uuid,
    name: &str,
    created_by: Option<Uuid>,
    ttl_days: Option<i64>,
) -> Result<IssuedToken> {
    let name = name.trim();
    if name.len() > 120 {
        return Err(IdentityError::InvalidProvisioning(
            "the token name is longer than 120 characters".to_owned(),
        ));
    }

    // A caller asking for zero or negative days wants a token that is already dead, which is a
    // mistake rather than a configuration: refused here, where the error names the number.
    if let Some(days) = ttl_days {
        if !(1..=MAX_TOKEN_TTL_DAYS).contains(&days) {
            return Err(IdentityError::InvalidProvisioning(format!(
                "a provisioning token must live between 1 and {MAX_TOKEN_TTL_DAYS} days, not {days}"
            )));
        }
    }

    let prefix = random_hex(PREFIX_LENGTH);
    let secret_half = random_hex(SECRET_LENGTH);
    let secret = format!("{TOKEN_NAMESPACE}_{prefix}_{secret_half}");

    let sql = format!(
        "insert into provisioning_tokens (organization_id, name, prefix, token_hash, created_by, \
         expires_at) \
         values ($1, $2, $3, $4, $5, \
             now() + coalesce($6::bigint, {DEFAULT_TOKEN_TTL_DAYS}) * interval '1 day') \
         returning {TOKEN_COLUMNS}"
    );

    let token: ProvisioningToken = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(name)
        .bind(&prefix)
        .bind(hash_token(&secret))
        .bind(created_by)
        .bind(ttl_days)
        .fetch_one(pool)
        .await?;

    Ok(IssuedToken { token, secret })
}

/// Replace a live token with a fresh one and return the replacement's secret.
///
/// Rotation is one transaction for the reason the acceptance criterion says a *rotated* token
/// refuses the old value: if the old token were revoked first and the insert then failed, the
/// connector would be left with no working credential and no way to recover except minting a
/// token by hand — a rotation that can strand a live integration is worse than a revoke button
/// the operator chose deliberately. Inside the transaction the old row is locked, so two
/// rotations of the same token cannot both mint a successor and leave two "current" tokens.
///
/// The old row is kept, revoked and linked, never deleted: it is the evidence that the secret
/// found in a log was the one that was replaced.
pub async fn rotate_token(
    pool: &PgPool,
    token_id: Uuid,
    rotated_by: Option<Uuid>,
) -> Result<IssuedToken> {
    let mut tx = pool.begin().await?;

    let current: ProvisioningToken = sqlx::query_as(&format!(
        "select {TOKEN_COLUMNS} from provisioning_tokens where id = $1 for update"
    ))
    .bind(token_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(IdentityError::InvalidProvisioning("no such provisioning token".to_owned()))?;

    if current.revoked_at.is_some() {
        // Revoking a dead token is idempotent by design, and rotating one is refused by name:
        // "replace this" against something already replaced has no single answer, and guessing
        // which of the chain the caller meant is how a live token gets orphaned.
        return Err(IdentityError::InvalidProvisioning(
            "this token was already revoked; mint a new one instead of rotating it".to_owned(),
        ));
    }

    // The successor keeps the *name* of the token it replaces: a connector list is read by
    // humans looking for "the Okta one", and a rotation that renames it makes the list lie about
    // its own history. The new secret and prefix are what differ.
    let prefix = random_hex(PREFIX_LENGTH);
    let secret_half = random_hex(SECRET_LENGTH);
    let secret = format!("{TOKEN_NAMESPACE}_{prefix}_{secret_half}");

    let replacement: ProvisioningToken = sqlx::query_as(&format!(
        "insert into provisioning_tokens (organization_id, name, prefix, token_hash, created_by, \
         expires_at) \
         values ($1, $2, $3, $4, $5, \
             now() + coalesce(nullif($6::bigint, 0), {DEFAULT_TOKEN_TTL_DAYS}) * interval '1 day') \
         returning {TOKEN_COLUMNS}"
    ))
    .bind(current.organization_id)
    .bind(&current.name)
    .bind(&prefix)
    .bind(hash_token(&secret))
    .bind(rotated_by)
    .bind(None::<i64>)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "update provisioning_tokens set revoked_at = now(), rotated_at = now() where id = $1",
    )
    .bind(current.id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "insert into provisioning_token_rotations (token_id, rotated_to, rotated_by) \
         values ($1, $2, $3)",
    )
    .bind(current.id)
    .bind(replacement.id)
    .bind(rotated_by)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IssuedToken {
        token: replacement,
        secret,
    })
}

/// What a token was replaced by, newest first for the token.
pub async fn rotation_of(
    pool: &PgPool,
    token_id: Uuid,
) -> Result<Option<(Uuid, Uuid, OffsetDateTime)>> {
    sqlx::query_as(
        "select rotated_to, rotated_by, rotated_at from provisioning_token_rotations \
         where token_id = $1",
    )
    .bind(token_id)
    .fetch_optional(pool)
    .await
    .map_err(IdentityError::from)
}

/// Every token of an organization, newest first (revoked ones included, flagged).
pub async fn list_tokens(pool: &PgPool, organization_id: Uuid) -> Result<Vec<ProvisioningToken>> {
    let sql = format!(
        "select {TOKEN_COLUMNS} from provisioning_tokens where organization_id = $1 \
         order by created_at desc"
    );
    sqlx::query_as(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await
        .map_err(IdentityError::from)
}

/// Revoke a token. Returns the row when a live token was revoked, `None` when it already was.
pub async fn revoke_token(pool: &PgPool, id: Uuid) -> Result<Option<ProvisioningToken>> {
    let sql = format!(
        "update provisioning_tokens set revoked_at = now() \
         where id = $1 and revoked_at is null returning {TOKEN_COLUMNS}"
    );
    sqlx::query_as(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(IdentityError::from)
}

/// Resolve a presented token: `None` for anything that is not a live token of this platform.
///
/// The comparison is constant time over the hash, and a successful lookup touches
/// `last_used_at` so the panel can show which token is actually in use.
pub async fn verify_token(pool: &PgPool, presented: &str) -> Result<Option<ProvisioningToken>> {
    let (prefix, secret) = match split_token(presented) {
        Some(parts) => parts,
        None => return Ok(None),
    };

    let sql = format!(
        "select {TOKEN_COLUMNS} from provisioning_tokens \
         where prefix = $1 and revoked_at is null and (expires_at is null or expires_at > now())"
    );
    let row: Option<ProvisioningToken> = sqlx::query_as(&sql)
        .bind(&prefix)
        .fetch_optional(pool)
        .await?;

    let Some(token) = row else {
        return Ok(None);
    };

    let expected =
        sqlx::query_scalar::<_, String>("select token_hash from provisioning_tokens where id = $1")
            .bind(token.id)
            .fetch_one(pool)
            .await?;

    let presented_hash = hash_token(&format!("{TOKEN_NAMESPACE}_{prefix}_{secret}"));
    if expected
        .as_bytes()
        .ct_eq(presented_hash.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Ok(None);
    }

    let touched: ProvisioningToken = sqlx::query_as(&format!(
        "update provisioning_tokens set last_used_at = now() where id = $1 returning {TOKEN_COLUMNS}"
    ))
    .bind(token.id)
    .fetch_one(pool)
    .await?;

    Ok(Some(touched))
}

/// Write one line of the sync log.
pub async fn log_sync(
    pool: &PgPool,
    organization_id: Uuid,
    entry: &NewSyncEntry,
) -> Result<SyncLogEntry> {
    if !matches!(entry.direction.as_str(), "inbound" | "outbound") {
        return Err(IdentityError::InvalidProvisioning(format!(
            "unknown direction `{}`",
            entry.direction
        )));
    }
    if !matches!(entry.resource.as_str(), "user" | "group") {
        return Err(IdentityError::InvalidProvisioning(format!(
            "unknown resource `{}`",
            entry.resource
        )));
    }
    if !matches!(
        entry.outcome.as_str(),
        "created" | "updated" | "deactivated" | "failed" | "skipped"
    ) {
        return Err(IdentityError::InvalidProvisioning(format!(
            "unknown outcome `{}`",
            entry.outcome
        )));
    }

    let sql = format!(
        "insert into provisioning_log \
         (organization_id, direction, resource, external_id, entity_id, action, outcome, detail) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning {LOG_COLUMNS}"
    );

    sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(&entry.direction)
        .bind(&entry.resource)
        .bind(entry.external_id.as_deref())
        .bind(entry.entity_id)
        .bind(&entry.action)
        .bind(&entry.outcome)
        .bind(&entry.detail)
        .fetch_one(pool)
        .await
        .map_err(IdentityError::from)
}

/// The most recent sync lines of an organization, newest first.
pub async fn list_log(
    pool: &PgPool,
    organization_id: Uuid,
    limit: Option<i64>,
) -> Result<Vec<SyncLogEntry>> {
    let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
    let sql = format!(
        "select {LOG_COLUMNS} from provisioning_log where organization_id = $1 \
         order by created_at desc, id desc limit $2"
    );
    sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(IdentityError::from)
}

/// Split `omsc_<prefix>_<secret>` into its two halves; anything else is not our token shape.
fn split_token(token: &str) -> Option<(String, String)> {
    let rest = token.strip_prefix(&format!("{TOKEN_NAMESPACE}_"))?;
    let (prefix, secret) = rest.split_once('_')?;
    if prefix.len() != PREFIX_LENGTH || secret.len() != SECRET_LENGTH {
        return None;
    }
    if !prefix.chars().all(|c| c.is_ascii_hexdigit())
        || !secret.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some((prefix.to_owned(), secret.to_owned()))
}

/// `length` random hex characters.
fn random_hex(length: usize) -> String {
    let mut bytes = vec![0u8; length.div_ceil(2)];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut value = hex::encode(bytes);
    value.truncate(length);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_has_the_documented_shape() {
        let secret = format!(
            "{TOKEN_NAMESPACE}_{}_{}",
            random_hex(PREFIX_LENGTH),
            random_hex(SECRET_LENGTH)
        );
        let (prefix, _) = split_token(&secret).expect("our own shape must parse");
        assert_eq!(prefix.len(), PREFIX_LENGTH);
        assert_eq!(hash_token(&secret).len(), 64);
    }

    #[test]
    fn anything_that_is_not_our_shape_is_refused() {
        assert!(split_token("").is_none());
        assert!(split_token("omsc_short_bad").is_none());
        assert!(split_token("omsa_12345678_1234").is_none());
        let wrong_chars = format!("{TOKEN_NAMESPACE}_zzzzzzzz_{}", "z".repeat(SECRET_LENGTH));
        assert!(split_token(&wrong_chars).is_none());
    }

    #[test]
    fn the_hash_is_stable_and_not_the_secret() {
        let secret = "omsc_12345678_abcdef";
        assert_eq!(hash_token(secret), hash_token(secret));
        assert_ne!(hash_token(secret), secret);
    }
}
