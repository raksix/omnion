//! Service accounts: machine identities and the keys they authenticate with.
//!
//! A machine identity is a subject like any other — roles bind to it exactly the way they bind
//! to a person (docs/07-IAM.md §14). Its keys are issued once: the plaintext token is returned
//! by the call that creates it and never stored; the database keeps a prefix for lookup and the
//! SHA-256 hash of the secret, the same discipline the session store follows.
//!
//! Token shape: `omsa_<prefix>_<secret>` — the prefix is 10 characters of lowercase
//! alphanumerics, the secret 32. The prefix is the unique index the sign-in path looks up.

use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{PermissionsError, Result};

/// Longest accepted service-account name.
const MAX_NAME_LENGTH: usize = 80;

/// Prefix of every machine token, so one is recognisable in a log or a config file.
const TOKEN_NAMESPACE: &str = "omsa";

/// Length of the lookup prefix.
const PREFIX_LENGTH: usize = 10;

/// Length of the secret half.
const SECRET_LENGTH: usize = 32;

/// A machine identity.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ServiceAccount {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Display prefix of the account (its first key carries the same one).
    pub prefix: String,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// Last time any of its keys authenticated a request.
    pub last_used_at: Option<OffsetDateTime>,
    /// When the whole identity stops authenticating.
    pub expires_at: Option<OffsetDateTime>,
    /// Set when the identity was disabled.
    pub disabled_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

impl ServiceAccount {
    /// `true` when the identity may authenticate right now.
    #[must_use]
    pub fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.disabled_at.is_none() && self.expires_at.is_none_or(|expires| expires > now)
    }
}

/// A machine identity with the counts the list screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAccountSummary {
    /// The identity.
    pub account: ServiceAccount,
    /// Keys that can still authenticate.
    pub active_keys: i64,
    /// Live role bindings attached to the identity.
    pub role_count: i64,
}

/// A machine identity to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewServiceAccount {
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Who creates it.
    pub created_by: Option<Uuid>,
}

/// One issued key (the secret half is never stored, only its hash).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ServiceAccountKey {
    /// Primary key.
    pub id: Uuid,
    /// Owning identity.
    pub service_account_id: Uuid,
    /// Lookup prefix.
    pub prefix: String,
    /// Human label (`ci`, `backup`, …).
    pub label: String,
    /// When the key stops working.
    pub expires_at: Option<OffsetDateTime>,
    /// Last use.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

impl ServiceAccountKey {
    /// `true` when the key can still authenticate.
    #[must_use]
    pub fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expires| expires > now)
    }
}

/// An issued key and the one moment its token is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedKey {
    /// The stored row.
    pub key: ServiceAccountKey,
    /// The token to copy now — the server keeps no copy of it.
    pub token: String,
}

/// Column list of every service-account query.
const ACCOUNT_COLUMNS: &str = "id, organization_id, name, description, prefix, created_by, \
     last_used_at, expires_at, disabled_at, created_at";

/// Column list of every key query.
const KEY_COLUMNS: &str = "id, service_account_id, prefix, label, expires_at, last_used_at, \
     revoked_at, created_at";

/// Create a machine identity; it starts without keys and cannot authenticate until one is issued.
pub async fn create(pool: &PgPool, new: NewServiceAccount) -> Result<ServiceAccount> {
    let name = validate_name(&new.name)?;
    let prefix = random_chars(PREFIX_LENGTH);

    let sql = format!(
        "insert into service_accounts (organization_id, name, description, prefix, created_by) \
         values ($1, $2, $3, $4, $5) returning {ACCOUNT_COLUMNS}"
    );

    let account: ServiceAccount = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(&name)
        .bind(new.description.trim())
        .bind(&prefix)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
        .map_err(map_account_error)?;

    Ok(account)
}

/// Machine identities of one organization with their counts.
pub async fn list(pool: &PgPool, organization_id: Uuid) -> Result<Vec<ServiceAccountSummary>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        organization_id: Uuid,
        name: String,
        description: String,
        prefix: String,
        created_by: Option<Uuid>,
        last_used_at: Option<OffsetDateTime>,
        expires_at: Option<OffsetDateTime>,
        disabled_at: Option<OffsetDateTime>,
        created_at: OffsetDateTime,
        active_keys: i64,
        role_count: i64,
    }

    let rows: Vec<Row> = sqlx::query_as(
        "select a.id, a.organization_id, a.name, a.description, a.prefix, a.created_by, \
                a.last_used_at, a.expires_at, a.disabled_at, a.created_at, \
                (select count(*) from service_account_keys k \
                  where k.service_account_id = a.id and k.revoked_at is null \
                    and (k.expires_at is null or k.expires_at > now())) as active_keys, \
                (select count(*) from role_bindings b \
                  where b.subject_type = 'service_account' and b.subject_id = a.id \
                    and b.revoked_at is null \
                    and (b.expires_at is null or b.expires_at > now())) as role_count \
         from service_accounts a \
         where a.organization_id = $1 \
         order by lower(a.name) asc",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ServiceAccountSummary {
            account: ServiceAccount {
                id: row.id,
                organization_id: row.organization_id,
                name: row.name,
                description: row.description,
                prefix: row.prefix,
                created_by: row.created_by,
                last_used_at: row.last_used_at,
                expires_at: row.expires_at,
                disabled_at: row.disabled_at,
                created_at: row.created_at,
            },
            active_keys: row.active_keys,
            role_count: row.role_count,
        })
        .collect())
}

/// Look an identity up by id.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<ServiceAccount>> {
    let sql = format!("select {ACCOUNT_COLUMNS} from service_accounts where id = $1");
    let account: Option<ServiceAccount> =
        sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(account)
}

/// Delete an identity: its keys go with it, and its role bindings are revoked — a deleted
/// machine identity must stop granting.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "update role_bindings set revoked_at = now() \
         where subject_type = 'service_account' and subject_id = $1 and revoked_at is null",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query("delete from service_accounts where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(deleted)
}

/// Issue a key. The token is returned once, here, and never again.
pub async fn issue_key(
    pool: &PgPool,
    account_id: Uuid,
    label: &str,
    expires_at: Option<OffsetDateTime>,
) -> Result<IssuedKey> {
    let account = find(pool, account_id)
        .await?
        .ok_or(PermissionsError::ServiceAccountNotFound)?;

    let prefix = random_chars(PREFIX_LENGTH);
    let secret = random_chars(SECRET_LENGTH);
    let token = format!("{TOKEN_NAMESPACE}_{prefix}_{secret}");

    let sql = format!(
        "insert into service_account_keys (service_account_id, prefix, secret_hash, label, \
         expires_at) values ($1, $2, $3, $4, $5) returning {KEY_COLUMNS}"
    );

    let key: ServiceAccountKey = sqlx::query_as(&sql)
        .bind(account.id)
        .bind(&prefix)
        .bind(hash_secret(&secret))
        .bind(label.trim())
        .bind(expires_at)
        .fetch_one(pool)
        .await?;

    Ok(IssuedKey { key, token })
}

/// Revoke one key. Returns `true` when a live key was revoked.
pub async fn revoke_key(pool: &PgPool, account_id: Uuid, key_id: Uuid) -> Result<bool> {
    let revoked = sqlx::query(
        "update service_account_keys set revoked_at = now() \
         where id = $1 and service_account_id = $2 and revoked_at is null",
    )
    .bind(key_id)
    .bind(account_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;

    Ok(revoked)
}

/// The keys of one identity, live ones first.
pub async fn list_keys(pool: &PgPool, account_id: Uuid) -> Result<Vec<ServiceAccountKey>> {
    let sql = format!(
        "select {KEY_COLUMNS} from service_account_keys where service_account_id = $1 \
         order by (revoked_at is null) desc, created_at desc, id desc"
    );

    let keys: Vec<ServiceAccountKey> = sqlx::query_as(&sql)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    Ok(keys)
}

/// What a presented token authenticates as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedMachine {
    /// The identity.
    pub account: ServiceAccount,
    /// The key that was presented.
    pub key: ServiceAccountKey,
}

/// Authenticate a `Bearer` token: look the key up by prefix, compare the secret hash, then
/// require an active identity and an active key. Records the use on both.
///
/// `None` means "not a machine token, or not a valid one" — the caller must not distinguish the
/// two to its client.
pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Option<AuthenticatedMachine>> {
    let Some((prefix, secret)) = split_token(token) else {
        return Ok(None);
    };

    #[derive(sqlx::FromRow)]
    struct JoinRow {
        // Key columns.
        key_id: Uuid,
        service_account_id: Uuid,
        key_prefix: String,
        label: String,
        key_expires_at: Option<OffsetDateTime>,
        key_last_used_at: Option<OffsetDateTime>,
        key_revoked_at: Option<OffsetDateTime>,
        key_created_at: OffsetDateTime,
        // Account columns.
        account_id: Uuid,
        organization_id: Uuid,
        name: String,
        description: String,
        account_prefix: String,
        created_by: Option<Uuid>,
        last_used_at: Option<OffsetDateTime>,
        expires_at: Option<OffsetDateTime>,
        disabled_at: Option<OffsetDateTime>,
        account_created_at: OffsetDateTime,
        // The stored hash never leaves this function.
        secret_hash: String,
    }

    let row: Option<JoinRow> = sqlx::query_as(
        "select k.id as key_id, k.service_account_id, k.prefix as key_prefix, k.label, \
                k.expires_at as key_expires_at, k.last_used_at as key_last_used_at, \
                k.revoked_at as key_revoked_at, k.created_at as key_created_at, \
                a.id as account_id, a.organization_id, a.name, a.description, \
                a.prefix as account_prefix, a.created_by, a.last_used_at, a.expires_at, \
                a.disabled_at, a.created_at as account_created_at, k.secret_hash \
         from service_account_keys k join service_accounts a on a.id = k.service_account_id \
         where k.prefix = $1",
    )
    .bind(&prefix)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    if !constant_time_eq(&hash_secret(&secret), &row.secret_hash) {
        return Ok(None);
    }

    let now = OffsetDateTime::now_utc();
    let key = ServiceAccountKey {
        id: row.key_id,
        service_account_id: row.service_account_id,
        prefix: row.key_prefix,
        label: row.label,
        expires_at: row.key_expires_at,
        last_used_at: row.key_last_used_at,
        revoked_at: row.key_revoked_at,
        created_at: row.key_created_at,
    };
    let account = ServiceAccount {
        id: row.account_id,
        organization_id: row.organization_id,
        name: row.name,
        description: row.description,
        prefix: row.account_prefix,
        created_by: row.created_by,
        last_used_at: row.last_used_at,
        expires_at: row.expires_at,
        disabled_at: row.disabled_at,
        created_at: row.account_created_at,
    };

    if !account.is_active_at(now) || !key.is_active_at(now) {
        return Ok(None);
    }

    // Record the use on both rows; a failed bookkeeping write must not fail the request.
    let _ = sqlx::query("update service_account_keys set last_used_at = now() where id = $1")
        .bind(key.id)
        .execute(pool)
        .await;
    let _ = sqlx::query("update service_accounts set last_used_at = now() where id = $1")
        .bind(account.id)
        .execute(pool)
        .await;

    Ok(Some(AuthenticatedMachine { account, key }))
}

/// Validate a service-account name.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(PermissionsError::InvalidServiceAccountName(name));
    }
    Ok(name)
}

/// SHA-256 of the secret half, hex-encoded — the same discipline the session store follows.
#[must_use]
pub fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Split a presented token into its prefix and secret halves.
fn split_token(token: &str) -> Option<(String, String)> {
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

/// Compare two hex digests without leaking where they differ.
fn constant_time_eq(left: &str, right: &str) -> bool {
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

/// `length` random lowercase alphanumerics, drawn from the operating system.
fn random_chars(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0_u8; length];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| ALPHABET[(byte as usize) % ALPHABET.len()] as char)
        .collect()
}

fn map_account_error(err: sqlx::Error) -> PermissionsError {
    match err {
        sqlx::Error::Database(ref db_error) if db_error.is_unique_violation() => {
            PermissionsError::ServiceAccountNameTaken
        }
        other => PermissionsError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_into_prefix_and_secret() {
        let token = format!("{TOKEN_NAMESPACE}_abcdefghij_{}", "k".repeat(SECRET_LENGTH));
        let (prefix, secret) = split_token(&token).expect("a well-shaped token must split");
        assert_eq!(prefix, "abcdefghij");
        assert_eq!(secret.len(), SECRET_LENGTH);
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        for token in [
            "",
            "omsa_short_x",
            "wrong_abcdefghij_xxxxxxxx",
            &format!("{TOKEN_NAMESPACE}_ABCDEFGHIJ_{}", "x".repeat(SECRET_LENGTH)),
            &format!("{TOKEN_NAMESPACE}_abcdefghij_short"),
        ] {
            assert!(split_token(token).is_none(), "{token:?} must not parse");
        }
    }

    #[test]
    fn hashes_are_deterministic_and_hide_the_secret() {
        let a = hash_secret("s3cret");
        assert_eq!(a, hash_secret("s3cret"));
        assert_ne!(a, hash_secret("s3cres"));
        assert!(!a.contains("s3cret"));
        assert_eq!(a.len(), 64, "sha-256 hex");
    }

    #[test]
    fn constant_time_equality_is_exact() {
        assert!(constant_time_eq("abcd", "abcd"));
        assert!(!constant_time_eq("abcd", "abce"));
        assert!(!constant_time_eq("abcd", "abcde"));
    }

    #[test]
    fn random_strings_have_the_requested_shape() {
        let value = random_chars(32);
        assert_eq!(value.len(), 32);
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        );
        assert_ne!(value, random_chars(32), "two draws must differ");
    }

    #[test]
    fn names_are_bounded() {
        assert_eq!(validate_name("  CI Runner ").expect("valid"), "CI Runner");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"n".repeat(MAX_NAME_LENGTH + 1)).is_err());
    }
}
