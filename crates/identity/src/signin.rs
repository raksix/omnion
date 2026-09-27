//! Sign-in orchestration: the IP lists, the lockout, the password and the second factor.
//!
//! The order of the checks is the design. Address lists and an existing lockout are evaluated
//! **before** the password is verified, so a refused address never learns whether an account
//! exists from timing, and the failures are recorded in `sign_in_attempts` with the outcome the
//! reader sees: `success`, `failed`, `locked`, `blocked` or `mfa_required`.
//!
//! Lockout has two dimensions (docs/07-IAM.md §13): a **per-account** counter that sets
//! `users.locked_until`, and a **per-address** count of recent failures that refuses the
//! address itself — an account lock alone would let one attacker spray every account from one
//! address forever.
//!
//! When the account holds a confirmed factor, a correct password does not produce a session: it
//! produces a short-lived challenge ([`crate::mfa`] verifies the code that consumes it). That
//! is what makes the second factor a second factor rather than a decoration.

use std::net::IpAddr;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::mfa;
use crate::password::{dummy_verify, verify_password};
use crate::secrets::SecretBox;
use crate::security::{self, IpVerdict, SecurityPolicy, SessionPolicy};
use crate::sessions;
use crate::users::{User, normalize_email};

/// How long a second-factor challenge stays valid.
pub const CHALLENGE_TTL_MINUTES: i64 = 5;

/// Purpose of a challenge: the second factor of a sign-in.
pub const PURPOSE_LOGIN: &str = "login";

/// Purpose of a challenge: proving identity again for a dangerous operation.
pub const PURPOSE_STEP_UP: &str = "step_up";

/// What a sign-in attempt did.
#[derive(Debug, Clone)]
pub enum SignInOutcome {
    /// The password matched. `challenge` carries a second-factor token when the account holds a
    /// confirmed factor — the caller must finish the sign-in with `complete_mfa_login`.
    Authenticated {
        /// The account.
        user: User,
        /// The challenge token, when a second factor is demanded.
        challenge: Option<String>,
    },
    /// Unknown address or wrong password — one outcome for both, on purpose.
    InvalidCredentials,
    /// Password was correct but the account is not active (`invited`/`disabled`).
    AccountDisabled {
        /// Stored status of the account.
        status: String,
    },
    /// The account is locked until `until`.
    AccountLocked {
        /// When the lockout ends.
        until: OffsetDateTime,
    },
    /// The address is refused before any password check.
    IpBlocked {
        /// `denylist`, `allowlist` or `address_failures`.
        reason: &'static str,
        /// The entry or list that refused it.
        rule: String,
    },
}

/// One recorded attempt.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    /// The address the attempt named.
    pub email: String,
    /// The account, when the address matches one.
    pub user_id: Option<Uuid>,
    /// The account's organization, for the security centre.
    pub organization_id: Option<Uuid>,
    /// Remote address.
    pub ip: Option<String>,
    /// User agent.
    pub user_agent: Option<String>,
    /// `success`, `failed`, `locked`, `blocked` or `mfa_required`.
    pub outcome: &'static str,
    /// Short explanation, never a secret.
    pub reason: Option<String>,
}

/// Record one attempt in `sign_in_attempts`.
pub async fn record_attempt(pool: &PgPool, attempt: &AttemptRecord) -> Result<()> {
    sqlx::query(
        "insert into sign_in_attempts \
            (email, user_id, organization_id, ip_address, user_agent, outcome, reason) \
         values ($1, $2, $3, cast($4 as inet), $5, $6, $7)",
    )
    .bind(&attempt.email)
    .bind(attempt.user_id)
    .bind(attempt.organization_id)
    .bind(attempt.ip.as_deref())
    .bind(attempt.user_agent.as_deref())
    .bind(attempt.outcome)
    .bind(attempt.reason.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether an address has failed too often inside the lockout window.
pub async fn recent_failures_from_address(
    pool: &PgPool,
    ip: &str,
    window_minutes: i32,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from sign_in_attempts \
         where ip_address = cast($1 as inet) \
           and outcome in ('failed', 'locked') \
           and created_at > now() - make_interval(mins => $2)",
    )
    .bind(ip)
    .bind(window_minutes)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Failures recorded against an address in the last `window_minutes`, for the panel's overview.
pub async fn recent_failures_total(pool: &PgPool, window_minutes: i32) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from sign_in_attempts \
         where outcome in ('failed', 'locked', 'blocked') \
           and created_at > now() - make_interval(mins => $2)",
    )
    .bind(window_minutes)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The policy in force for an account, or the defaults when it has no organization.
async fn policy_for(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Option<SecurityPolicy>> {
    security::policy_for_account(pool, organization_id).await
}

/// Sign in with email and password, honouring the address lists, the lockout and the factor.
pub async fn sign_in(
    pool: &PgPool,
    email: &str,
    password: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<SignInOutcome> {
    let normalized = normalize_email(email).ok();
    let attempt_email = normalized
        .clone()
        .unwrap_or_else(|| email.trim().to_lowercase());

    let account = match normalized {
        Some(normalized) => find_account(pool, &normalized).await?,
        None => None,
    };

    // The policy of the account's organization drives every threshold below; an account
    // without one (an unknown address, or a platform account) is evaluated against the
    // table's conservative defaults instead.
    let policy = match &account {
        Some(account) => policy_for(pool, account.organization_id()).await?,
        None => None,
    };
    let lockout_attempts = policy.as_ref().map_or(10, |policy| policy.lockout_attempts);
    let lockout_minutes = policy.as_ref().map_or(15, |policy| policy.lockout_minutes);

    // 1. The address lists — deny wins, an allowlist narrows. This runs before the password.
    if let Some(policy) = &policy {
        let ip_address: Option<IpAddr> = ip.and_then(|text| text.parse().ok());
        if let IpVerdict::Denied { rule, reason } = security::check_ip(policy, ip_address) {
            record_attempt(
                pool,
                &AttemptRecord {
                    email: attempt_email,
                    user_id: account.as_ref().map(|account| account.user.id),
                    organization_id: account
                        .as_ref()
                        .and_then(|account| account.organization_id()),
                    ip: ip.map(str::to_owned),
                    user_agent: user_agent.map(str::to_owned),
                    outcome: "blocked",
                    reason: Some(format!("address {reason}: {rule}")),
                },
            )
            .await?;
            return Ok(SignInOutcome::IpBlocked { reason, rule });
        }
    }

    // 2. An address that has failed too often is refused regardless of the account.
    if let Some(ip) = ip {
        let failures = recent_failures_from_address(pool, ip, lockout_minutes).await?;
        if failures >= i64::from(lockout_attempts) {
            record_attempt(
                pool,
                &AttemptRecord {
                    email: attempt_email,
                    user_id: account.as_ref().map(|account| account.user.id),
                    organization_id: account
                        .as_ref()
                        .and_then(|account| account.organization_id()),
                    ip: Some(ip.to_owned()),
                    user_agent: user_agent.map(str::to_owned),
                    outcome: "blocked",
                    reason: Some(format!("{failures} recent failures from this address")),
                },
            )
            .await?;
            return Ok(SignInOutcome::IpBlocked {
                reason: "address_failures",
                rule: ip.to_owned(),
            });
        }
    }

    let Some(account) = account else {
        dummy_verify(password.to_owned()).await?;
        record_attempt(
            pool,
            &AttemptRecord {
                email: attempt_email,
                user_id: None,
                organization_id: None,
                ip: ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                outcome: "failed",
                reason: Some("unknown address".to_owned()),
            },
        )
        .await?;
        return Ok(SignInOutcome::InvalidCredentials);
    };

    // 3. An existing lockout is checked before the password: a locked account cannot be
    //    unlocked by guessing it correctly.
    if let Some(until) = account.locked_until {
        if until > OffsetDateTime::now_utc() {
            record_attempt(
                pool,
                &AttemptRecord {
                    email: attempt_email,
                    user_id: Some(account.user.id),
                    organization_id: account.organization_id(),
                    ip: ip.map(str::to_owned),
                    user_agent: user_agent.map(str::to_owned),
                    outcome: "locked",
                    reason: Some("the account is locked".to_owned()),
                },
            )
            .await?;
            return Ok(SignInOutcome::AccountLocked { until });
        }
    }

    // 4. The password.
    if !verify_password(password.to_owned(), account.password_hash.clone()).await? {
        let (failed_count, locked_until) =
            register_failure(pool, account.user.id, lockout_attempts, lockout_minutes).await?;
        let locked = locked_until.is_some_and(|until| until > OffsetDateTime::now_utc());
        record_attempt(
            pool,
            &AttemptRecord {
                email: attempt_email,
                user_id: Some(account.user.id),
                organization_id: account.organization_id(),
                ip: ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                outcome: if locked { "locked" } else { "failed" },
                reason: Some(format!("attempt {failed_count} of {lockout_attempts}")),
            },
        )
        .await?;

        return Ok(match locked_until {
            Some(until) if locked => SignInOutcome::AccountLocked { until },
            _ => SignInOutcome::InvalidCredentials,
        });
    }

    if !account.user.is_active() {
        record_attempt(
            pool,
            &AttemptRecord {
                email: attempt_email,
                user_id: Some(account.user.id),
                organization_id: account.organization_id(),
                ip: ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                outcome: "failed",
                reason: Some(format!("account status {}", account.user.status)),
            },
        )
        .await?;
        return Ok(SignInOutcome::AccountDisabled {
            status: account.user.status.clone(),
        });
    }

    // 5. Correct password: clear the counters, then decide whether a factor is needed.
    clear_failures(pool, account.user.id).await?;

    if mfa::has_confirmed_factor(pool, account.user.id).await? {
        let challenge =
            create_challenge(pool, account.user.id, PURPOSE_LOGIN, ip, user_agent).await?;
        record_attempt(
            pool,
            &AttemptRecord {
                email: attempt_email,
                user_id: Some(account.user.id),
                organization_id: account.organization_id(),
                ip: ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                outcome: "mfa_required",
                reason: None,
            },
        )
        .await?;
        return Ok(SignInOutcome::Authenticated {
            user: account.user,
            challenge: Some(challenge),
        });
    }

    record_attempt(
        pool,
        &AttemptRecord {
            email: attempt_email,
            user_id: Some(account.user.id),
            organization_id: account.organization_id(),
            ip: ip.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
            outcome: "success",
            reason: None,
        },
    )
    .await?;

    Ok(SignInOutcome::Authenticated {
        user: account.user,
        challenge: None,
    })
}

/// Finish a sign-in: verify the second factor against a login challenge.
///
/// Returns `Ok(None)` when the challenge is unknown, expired or already used — the caller
/// answers `401 invalid_challenge` — and `Ok(Some(user))` when the code matched.
pub async fn complete_mfa_login(
    pool: &PgPool,
    secret_box: &SecretBox,
    challenge_token: &str,
    code: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Option<(User, mfa::Verification)>> {
    let Some(user_id) = consume_challenge(pool, challenge_token, PURPOSE_LOGIN).await? else {
        return Ok(None);
    };
    let Some(user) = find_user(pool, user_id).await? else {
        return Ok(None);
    };

    let Some(verification) = mfa::verify_code(
        pool,
        user_id,
        code,
        secret_box,
        OffsetDateTime::now_utc().unix_timestamp(),
    )
    .await?
    else {
        record_attempt(
            pool,
            &AttemptRecord {
                email: user.email.clone(),
                user_id: Some(user.id),
                organization_id: user.organization_id,
                ip: ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                outcome: "failed",
                reason: Some("second factor did not match".to_owned()),
            },
        )
        .await?;
        return Err(crate::error::IdentityError::InvalidFactor(
            "that code does not match".to_owned(),
        ));
    };

    record_attempt(
        pool,
        &AttemptRecord {
            email: user.email.clone(),
            user_id: Some(user.id),
            organization_id: user.organization_id,
            ip: ip.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
            outcome: "success",
            reason: None,
        },
    )
    .await?;

    Ok(Some((user, verification)))
}

/// Create a short-lived challenge token and return it (only its hash is stored).
pub async fn create_challenge(
    pool: &PgPool,
    user_id: Uuid,
    purpose: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    let token = sessions::generate_token();
    let token_hash = sessions::hash_token(&token);
    let expires_at = OffsetDateTime::now_utc() + time::Duration::minutes(CHALLENGE_TTL_MINUTES);

    // One live challenge per purpose: asking again replaces the previous one.
    sqlx::query(
        "update mfa_challenges set consumed_at = now() \
         where user_id = $1 and purpose = $2 and consumed_at is null",
    )
    .bind(user_id)
    .bind(purpose)
    .execute(pool)
    .await?;

    sqlx::query(
        "insert into mfa_challenges \
            (user_id, purpose, token_hash, ip_address, user_agent, expires_at) \
         values ($1, $2, $3, cast($4 as inet), $5, $6)",
    )
    .bind(user_id)
    .bind(purpose)
    .bind(&token_hash)
    .bind(ip)
    .bind(user_agent)
    .bind(expires_at)
    .execute(pool)
    .await?;

    Ok(token)
}

/// Consume a challenge and answer whose it was, or `None` when it is not usable.
pub async fn consume_challenge(pool: &PgPool, token: &str, purpose: &str) -> Result<Option<Uuid>> {
    let token_hash = sessions::hash_token(token);
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "update mfa_challenges set consumed_at = now() \
         where token_hash = $1 and purpose = $2 and consumed_at is null and expires_at > now() \
         returning user_id",
    )
    .bind(&token_hash)
    .bind(purpose)
    .fetch_optional(pool)
    .await?;
    Ok(user_id)
}

/// Increment the failure counter and lock the account when the threshold is reached.
async fn register_failure(
    pool: &PgPool,
    user_id: Uuid,
    lockout_attempts: i32,
    lockout_minutes: i32,
) -> Result<(i32, Option<OffsetDateTime>)> {
    let (count, locked_until): (i32, Option<OffsetDateTime>) = sqlx::query_as(
        "update users set \
            failed_sign_in_count = failed_sign_in_count + 1, \
            locked_until = case \
                when failed_sign_in_count + 1 >= $2 \
                then now() + make_interval(mins => $3) \
                else locked_until \
            end \
         where id = $1 \
         returning failed_sign_in_count, locked_until",
    )
    .bind(user_id)
    .bind(lockout_attempts)
    .bind(lockout_minutes)
    .fetch_one(pool)
    .await?;
    Ok((count, locked_until))
}

/// Clear the failure counter after a successful sign-in.
async fn clear_failures(pool: &PgPool, user_id: Uuid) -> Result<()> {
    sqlx::query(
        "update users set failed_sign_in_count = 0, locked_until = null, \
                          last_sign_in_at = now() \
         where id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The account row a sign-in needs, with the fields the checks read.
#[derive(Debug, Clone)]
struct Account {
    user: User,
    password_hash: String,
    locked_until: Option<OffsetDateTime>,
}

impl Account {
    fn organization_id(&self) -> Option<Uuid> {
        self.user.organization_id
    }
}

/// Look an account up by normalized address.
async fn find_account(pool: &PgPool, email: &str) -> Result<Option<Account>> {
    let row: Option<AccountRow> = sqlx::query_as(
        "select id, organization_id, email, display_name, status, created_at, password_hash, \
                locked_until \
         from users where email = $1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(AccountRow::into_account))
}

/// Look a user up by id.
async fn find_user(pool: &PgPool, user_id: Uuid) -> Result<Option<User>> {
    crate::users::find_by_id(pool, user_id).await
}

/// Row shape of the sign-in lookup.
#[derive(sqlx::FromRow)]
struct AccountRow {
    id: Uuid,
    organization_id: Option<Uuid>,
    email: String,
    display_name: String,
    status: String,
    created_at: OffsetDateTime,
    password_hash: Option<String>,
    locked_until: Option<OffsetDateTime>,
}

impl AccountRow {
    fn into_account(self) -> Option<Account> {
        let password_hash = self.password_hash?;
        Some(Account {
            user: User {
                id: self.id,
                organization_id: self.organization_id,
                email: self.email,
                display_name: self.display_name,
                status: self.status,
                created_at: self.created_at,
            },
            password_hash,
            locked_until: self.locked_until,
        })
    }
}

/// The session lifetimes in force for an account (its organization's policy, or the defaults).
pub async fn session_policy_for(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<SessionPolicy> {
    Ok(match policy_for(pool, organization_id).await? {
        Some(policy) => SessionPolicy::from(&policy),
        None => SessionPolicy::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_window_is_minutes_not_days() {
        assert_eq!(CHALLENGE_TTL_MINUTES, 5);
        assert_ne!(PURPOSE_LOGIN, PURPOSE_STEP_UP);
    }

    #[test]
    fn an_account_without_a_password_hash_cannot_sign_in() {
        let row = AccountRow {
            id: Uuid::nil(),
            organization_id: None,
            email: "qa@omnion.test".to_owned(),
            display_name: "QA".to_owned(),
            status: "invited".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            password_hash: None,
            locked_until: None,
        };
        assert!(row.into_account().is_none());

        let with_hash = AccountRow {
            id: Uuid::nil(),
            organization_id: None,
            email: "qa@omnion.test".to_owned(),
            display_name: "QA".to_owned(),
            status: "active".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            password_hash: Some("$argon2id$…".to_owned()),
            locked_until: None,
        };
        let account = with_hash.into_account().expect("hash present");
        assert_eq!(account.organization_id(), None);
        assert!(
            account.locked_until.is_none(),
            "a fresh account starts unlocked"
        );
    }
}
