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

/// Set an account's status (`active`, `invited` or `disabled`) and answer the updated row.
///
/// `None` means no such account. The status decides `is_active`, which every sign-in path
/// already reads, so a deactivation takes effect on the next request without touching sessions.
///
/// **This is the display-only half.** [`set_status_and_end_sessions`] is the one that revokes,
/// and it is the one every code path that takes an account out of service must call: a status
/// flag alone is not a revocation, because the flag is a *query filter* while a session token is
/// a *bearer credential* already in somebody's browser. See that function for the failure this
/// distinction produces.
pub async fn set_status(pool: &PgPool, id: Uuid, status: &str) -> Result<Option<User>> {
    if !matches!(status, "active" | "invited" | "disabled") {
        return Err(IdentityError::InvalidUser(format!(
            "unknown account status `{status}`"
        )));
    }

    let sql = format!("update users set status = $2 where id = $1 returning {USER_COLUMNS}");
    sqlx::query_as::<_, User>(&sql)
        .bind(id)
        .bind(status)
        .fetch_optional(pool)
        .await
        .map_err(IdentityError::from)
}

/// What a status change did to the account's live sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusChange {
    /// Sessions ended by this change.
    pub revoked_sessions: u64,
}

/// The row a status change returns: the account as it now reads, and the status it read as
/// before this statement ran.
///
/// A named struct rather than a tuple, because `sqlx::FromRow` is derived and does not exist for
/// tuples — and because the second field is the whole reason the query exists, and a positional
/// accessor would hide what it is.
#[derive(sqlx::FromRow)]
struct StatusUpdateRow {
    id: Uuid,
    organization_id: Option<Uuid>,
    email: String,
    display_name: String,
    status: String,
    created_at: OffsetDateTime,
    previous_status: String,
}

/// Set an account's status **and end every live session it has**, in one transaction.
///
/// The status flip and the revocation are the same fact: an account taken out of service must
/// not keep handing out access through a token minted a second earlier. Doing them in one
/// transaction means there is no window in which the account reads `disabled` while its
/// sessions are still live, or the reverse.
///
/// **Why the revocation is not optional, stated as the bug it fixes.** `resolve_session` already
/// filters on `u.status = 'active'`, so a deactivated account's sessions *stop resolving* — and
/// that is exactly why the missing revocation is invisible to a test that only checks "the
/// session no longer works". It works again. A directory that deactivates an account because it
/// left the company, and reactivates it weeks later when the sync notices the stale row, hands
/// the *old* browser tabs back a working session: `status` is back to `active`, the session row
/// was never touched, and a token that was believed dead is alive again with its original
/// expiry. The session must be *ended*, not merely masked, so that reactivating cannot resurrect
/// it.
///
/// A status that does not take the account out of service (`active`, `invited`) revokes
/// nothing: an admin re-activating an account is not an instruction to log the person out of the
/// tabs they just used to sign in with.
///
/// **A no-op change revokes nothing.** Setting `disabled` on an account that is already disabled
/// must not clear its sessions — an operator pressing the button twice, or a connector
/// re-sending the same document on its timer, would silently sign a colleague out. That is why
/// the read of the current status is part of the same statement as the write rather than a
/// separate `find_by_id` before it.
pub async fn set_status_and_end_sessions(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    reason: &str,
) -> Result<Option<(User, StatusChange)>> {
    // Validated before anything is written, and by this function alone: the variant a caller can
    // pass is a `&str`, so the check the other setter makes is the one that has to be repeated.
    if !matches!(status, "active" | "invited" | "disabled") {
        return Err(IdentityError::InvalidUser(format!(
            "unknown account status `{status}`"
        )));
    }

    let mut tx = pool.begin().await?;

    // `for update` in the CTE is what makes "read the old status, write the new one" one
    // statement: two concurrent deactivations cannot both read `active` and both believe they
    // were the one that ended the sessions.
    let row: Option<StatusUpdateRow> = sqlx::query_as(
        "with previous as (select id, status from users where id = $1 for update) \
         update users u set status = $2 from previous p where u.id = p.id \
         returning u.id, u.organization_id, u.email, u.display_name, u.status, u.created_at, \
                 p.status as previous_status",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(&mut *tx)
    .await?;

    // No such account. The row lock is released with the rollback rather than held to the end of
    // the function for a result that needs no further writes.
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(None);
    };

    // A status change that does not take the account out of service, or one that leaves it where
    // it already was, ends nothing. The comparison borrows: `into_user` below needs the row.
    let revokes = status == "disabled" && row.previous_status != status;
    if !revokes {
        tx.commit().await?;
        return Ok(Some((row.into_user(), StatusChange::default())));
    }

    let reason = crate::sessions::truncate_reason(reason);
    let revoked: Vec<Uuid> = sqlx::query_scalar(
        "update sessions set revoked_at = now(), revoke_reason = coalesce(revoke_reason, $2) \
         where user_id = $1 and revoked_at is null returning id",
    )
    .bind(id)
    .bind(&reason)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some((
        row.into_user(),
        StatusChange {
            revoked_sessions: revoked.len() as u64,
        },
    )))
}

impl StatusUpdateRow {
    /// The account half of the row, with the `previous_status` column dropped.
    fn into_user(self) -> User {
        User {
            id: self.id,
            organization_id: self.organization_id,
            email: self.email,
            display_name: self.display_name,
            status: self.status,
            created_at: self.created_at,
        }
    }
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

/// The oldest active account, if any.
///
/// The order is the one the Owner invariant uses (docs/07-IAM.md §20): earliest first, ties
/// broken by id, so two processes agree on which account is "the first one".
pub async fn earliest_active(pool: &PgPool) -> Result<Option<Uuid>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "select id from users where status = 'active' order by created_at asc, id asc limit 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(id)
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
