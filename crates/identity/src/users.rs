//! Users: account creation, lookup and the first-administrator bootstrap.
//!
//! Email addresses are stored lowercase-trimmed; the database enforces the same rule through
//! a unique index on `lower(email)`, so `Ada@Example.com` and `ada@example.com` are one
//! account (docs/07-IAM.md).

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::password::{hash_password, validate_password_strength};

/// Longest accepted email address (RFC 5321 forward-path limit).
const MAX_EMAIL_LENGTH: usize = 254;

/// Display name given to the account created by the bootstrap.
pub const FIRST_ADMIN_DISPLAY_NAME: &str = "Administrator";

/// Column list for every `User` query, so the row shape stays in one place.
const USER_COLUMNS: &str = "id, organization_id, email, display_name, status, created_at";

/// A user account as stored in the database.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct User {
    /// Primary key.
    pub id: Uuid,
    /// Primary organization (`None` = platform-level account).
    pub organization_id: Option<Uuid>,
    /// Email address, lowercase.
    pub email: String,
    /// Human-readable name.
    pub display_name: String,
    /// `active`, `invited` or `disabled` (schema constraint).
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
}

impl User {
    /// `true` when the account may sign in.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// Input for [`create_user`]; the plaintext password never reaches the database.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// Email address (normalized before insert).
    pub email: String,
    /// Plaintext password, hashed with Argon2id inside [`create_user`].
    pub password: String,
    /// Display name.
    pub display_name: String,
    /// Primary organization, if any.
    pub organization_id: Option<Uuid>,
}

/// A user together with the stored password hash — only used by the sign-in path.
#[derive(Debug, Clone)]
pub struct UserCredentials {
    /// The account.
    pub user: User,
    /// Stored Argon2 PHC hash.
    pub password_hash: String,
}

/// Row shape of a `users` row plus the password hash; split into [`User`] and the hash.
#[derive(sqlx::FromRow)]
struct CredentialsRow {
    id: Uuid,
    organization_id: Option<Uuid>,
    email: String,
    display_name: String,
    status: String,
    created_at: OffsetDateTime,
    password_hash: Option<String>,
}

impl CredentialsRow {
    fn split(self) -> (User, Option<String>) {
        (
            User {
                id: self.id,
                organization_id: self.organization_id,
                email: self.email,
                display_name: self.display_name,
                status: self.status,
                created_at: self.created_at,
            },
            self.password_hash,
        )
    }
}

/// Normalize and validate an email address.
///
/// The same function guards every write and lookup, so case cannot fork an account.
pub fn normalize_email(email: &str) -> Result<String> {
    let trimmed = email.trim();
    if trimmed.is_empty() {
        return Err(IdentityError::InvalidEmail("empty address".to_owned()));
    }
    if trimmed.len() > MAX_EMAIL_LENGTH {
        return Err(IdentityError::InvalidEmail(format!(
            "longer than {MAX_EMAIL_LENGTH} characters"
        )));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(IdentityError::InvalidEmail(
            "contains whitespace".to_owned(),
        ));
    }

    let (local, domain) = trimmed
        .split_once('@')
        .ok_or_else(|| IdentityError::InvalidEmail("missing `@` separator".to_owned()))?;
    let domain_valid = domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@');
    if local.is_empty() || domain.is_empty() || !domain_valid {
        return Err(IdentityError::InvalidEmail(
            "expected a `local@domain` address".to_owned(),
        ));
    }

    Ok(trimmed.to_lowercase())
}

/// Create an account with an Argon2id-hashed password.
///
/// Fails with [`IdentityError::EmailTaken`] when the address is already registered.
pub async fn create_user(pool: &PgPool, new: NewUser) -> Result<User> {
    let email = normalize_email(&new.email)?;
    validate_password_strength(&new.password)?;
    let password_hash = hash_password(new.password).await?;

    let sql = format!(
        "insert into users (email, password_hash, display_name, organization_id, status) \
         values ($1, $2, $3, $4, 'active') returning {USER_COLUMNS}"
    );
    sqlx::query_as::<_, User>(&sql)
        .bind(&email)
        .bind(&password_hash)
        .bind(new.display_name.trim())
        .bind(new.organization_id)
        .fetch_one(pool)
        .await
        .map_err(map_insert_error)
}

/// Look an account up by email address.
pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<User>> {
    let email = normalize_email(email)?;
    let sql = format!("select {USER_COLUMNS} from users where lower(email) = $1");
    sqlx::query_as::<_, User>(&sql)
        .bind(&email)
        .fetch_optional(pool)
        .await
        .map_err(IdentityError::from)
}

/// Look an account up by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>> {
    let sql = format!("select {USER_COLUMNS} from users where id = $1");
    sqlx::query_as::<_, User>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(IdentityError::from)
}

/// Load an account together with its password hash for verification.
///
/// Accounts without a password (invited, or future external sign-in) yield `None`.
pub async fn find_credentials(pool: &PgPool, email: &str) -> Result<Option<UserCredentials>> {
    let email = normalize_email(email)?;
    let sql = format!("select {USER_COLUMNS}, password_hash from users where lower(email) = $1");
    let row: Option<CredentialsRow> = sqlx::query_as(&sql)
        .bind(&email)
        .fetch_optional(pool)
        .await?;

    Ok(row.and_then(|row| {
        let (user, password_hash) = row.split();
        password_hash.map(|password_hash| UserCredentials {
            user,
            password_hash,
        })
    }))
}

/// `true` when at least one account exists.
pub async fn has_any(pool: &PgPool) -> Result<bool> {
    let exists: bool = sqlx::query_scalar("select exists (select 1 from users)")
        .fetch_one(pool)
        .await?;
    Ok(exists)
}

/// Number of accounts in the database.
pub async fn count_users(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*) from users")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Result of [`bootstrap_first_admin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// The first administrator was created.
    Created {
        /// New account id.
        user_id: Uuid,
        /// Normalized email address.
        email: String,
    },
    /// Nothing to do: the database already has accounts.
    SkippedExistingUsers,
}

/// Create the first administrator when the database is still empty.
///
/// Idempotent and safe with several instances booting at once: the emptiness check is backed
/// by the unique email index, so a race can only produce [`BootstrapOutcome::SkippedExistingUsers`].
pub async fn bootstrap_first_admin(
    pool: &PgPool,
    email: &str,
    password: &str,
) -> Result<BootstrapOutcome> {
    let email = normalize_email(email)?;
    validate_password_strength(password)?;

    if has_any(pool).await? {
        return Ok(BootstrapOutcome::SkippedExistingUsers);
    }

    let password_hash = hash_password(password.to_owned()).await?;
    let inserted: Result<Uuid> = sqlx::query_scalar(
        "insert into users (email, password_hash, display_name, status) \
         values ($1, $2, $3, 'active') returning id",
    )
    .bind(&email)
    .bind(&password_hash)
    .bind(FIRST_ADMIN_DISPLAY_NAME)
    .fetch_one(pool)
    .await
    .map_err(map_insert_error);

    match inserted {
        Ok(user_id) => Ok(BootstrapOutcome::Created { user_id, email }),
        // Another instance inserted the same account first; the database is no longer empty.
        Err(IdentityError::EmailTaken) => Ok(BootstrapOutcome::SkippedExistingUsers),
        Err(other) => Err(other),
    }
}

fn map_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            IdentityError::EmailTaken
        }
        other => IdentityError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_and_lowercased() {
        assert_eq!(
            normalize_email("  Ada@Example.COM ").expect("valid address"),
            "ada@example.com"
        );
    }

    #[test]
    fn unusable_addresses_are_rejected() {
        for bad in [
            "",
            "   ",
            "ada",
            "ada@",
            "@example.com",
            "a b@example.com",
            "ada@localhost",
            "ada@.com",
        ] {
            assert!(normalize_email(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn over_long_addresses_are_rejected() {
        let long = format!("{}@example.com", "a".repeat(MAX_EMAIL_LENGTH));
        assert!(normalize_email(&long).is_err());
    }

    #[test]
    fn user_active_flag_follows_status() {
        let mut user = User {
            id: Uuid::nil(),
            organization_id: None,
            email: "ada@example.com".to_owned(),
            display_name: "Ada".to_owned(),
            status: "active".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(user.is_active());
        user.status = "disabled".to_owned();
        assert!(!user.is_active());
    }
}
