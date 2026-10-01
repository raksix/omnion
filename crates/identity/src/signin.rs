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
use omnion_security::{EnforcedLockout, resolve_lockout};

/// How long a second-factor challenge stays valid.
pub const CHALLENGE_TTL_MINUTES: i64 = 5;

/// How many failures from ONE address refuse that address, when the organization has not
/// chosen a number of its own.
///
/// The account threshold and the address threshold are two different numbers and the module
/// doc above says why: an account lock alone lets one attacker spray every account from one
/// address for ever. Sharing one number made the account lock unreachable — the address rule
/// fired on the attempt that would have incremented the counter, so `users.failed_sign_in_count`
/// stayed at 0, no account was ever locked, and the "currently locked accounts" table on
/// `/security/sign-in-protection` had no possible content.
///
/// The address threshold is therefore a multiple of the account one, not the account one: an
/// attacker gets refused before reaching one account's threshold, and a legitimate user who
/// mistypes their own password a few times still reaches their account's threshold rather than
/// being locked out by a rule they cannot see.
pub const DEFAULT_ADDRESS_FAILURE_MULTIPLE: i32 = 3;

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
        /// Whether **this attempt** is what applied the lock, as opposed to finding one that was
        /// already in force.
        ///
        /// The two are different facts and only one of them is an event. "An account was locked"
        /// belongs in a telemetry row the attacker can generate at will — every guess against an
        /// already-locked account would emit it. "An account just crossed the threshold" is a
        /// fact about the account, and it is what an operator subscribing to it wants to know.
        /// Collapsing the two is how a lockout event becomes a volume metric of an attacker's
        /// patience instead of a record of the account it caught.
        newly_locked: bool,
        /// The account that was locked. Not decoration either: the caller refuses an
        /// anonymous request, so without the id the event could name nothing but a threshold
        /// and an operator subscribing to it would learn that *somebody* was locked.
        user_id: Uuid,
        /// The account's organization, so an emitter can attribute the event.
        ///
        /// This is not decoration. `store::enqueue_fanout` returns **zero** deliveries for an
        /// event with no organization — endpoints belong to organizations, and matching one
        /// against a fact that belongs to no tenant would leak. So an emitter that omitted it
        /// would write a row that exists, would appear in `/events` as a real record, and would
        /// reach **nobody**: the one shape of emitter this field is here to prevent.
        organization_id: Option<Uuid>,
        /// The threshold that was in force, which is the number the lock was applied against.
        attempts: i32,
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

/// Forget the recent failures recorded against an address.
///
/// The per-address refusal counts rows in `sign_in_attempts`, so it is a property of the LOG
/// rather than of a counter on a row. That makes it awkward to reset, which is why this exists
/// and why the integration tests need it: a walk that has just proved an account locks has,
/// as a side effect, filled the address log with the attempts it made — and the next assertion
/// about the lockout alone would then be answered by the address rule instead of by the lock.
///
/// A caller in production has no reason to want this: an operator who wants to let an address
/// try again waits out the window, or unlocks the account from the panel, which is the action
/// the screen exists to offer.
pub async fn clear_address_failures(pool: &PgPool) -> Result<()> {
    sqlx::query("delete from sign_in_attempts where outcome in ('failed', 'locked')")
        .execute(pool)
        .await?;
    Ok(())
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
    // **The lockout thresholds now come from the sign-in-protection document**, not from the
    // IAM policy's own two columns. That is the whole point of `omnion_security::enforce`:
    // `/security/sign-in-protection` writes `security_settings.lockout`, and until this line
    // nothing on the request path read it — the number an operator tuned there was the number
    // the screen showed and not the number that locked accounts.
    //
    // `resolve` already falls back to the IAM columns and then to the baseline, so a platform
    // that never opened that screen keeps enforcing the numbers it was promised. The legacy
    // read stays for the OTHER fields the IAM document owns (session lifetimes, device trust,
    // the IP lists), which is why both are still read rather than one replacing the other.
    //
    // The `?` is `From<SecurityError> for IdentityError`, whose mapping is deliberate: a
    // database failure stays a database failure rather than becoming an operator-facing "your
    // policy is invalid" at the moment the database is unreachable.
    let lockout = resolve_lockout(pool, account.as_ref().and_then(|account| account.organization_id())).await?;
    let lockout_attempts = lockout.attempts;
    let lockout_minutes = lockout.lockout_minutes;
    // The ADDRESS threshold is deliberately larger than the ACCOUNT one. Sharing the number is
    // what made the account lockout unreachable: the address rule fired on the same attempt
    // that would have incremented the counter, so the counter never moved. See
    // `DEFAULT_ADDRESS_FAILURE_MULTIPLE`.
    let address_failure_limit = i64::from(lockout_attempts) * i64::from(DEFAULT_ADDRESS_FAILURE_MULTIPLE);

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
        if failures >= address_failure_limit {
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
                    reason: Some(format!(
                        "{failures} recent failures from this address (limit {address_failure_limit})"
                    )),
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
            // `newly_locked: false` — this attempt FOUND the lock, it did not apply it. The
            // distinction is the whole reason the field exists: see `SignInOutcome`.
            return Ok(SignInOutcome::AccountLocked {
                until,
                newly_locked: false,
                user_id: account.user.id,
                organization_id: account.organization_id(),
                attempts: lockout_attempts,
            });
        }
    }

    // 4. The password.
    if !verify_password(password.to_owned(), account.password_hash.clone()).await? {
        let (failed_count, locked_until) = register_failure(pool, account.user.id, lockout).await?;
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
            // `locked_until.is_some_and(live)` is "this attempt set it". A `locked_until` that
            // is `None` here means the threshold was not reached, and a `locked_until` in the
            // past means `register_failure`'s CASE left an expired value alone — neither is a
            // lock, and both answer `InvalidCredentials`. `locked_until.is_some_and(until >
            // now())` is deliberately NOT used for the outcome: it is true of an account that was
            // ALREADY locked, which would make a correct password after an expired window report
            // a lockout it did not cause.
            Some(until) if locked => SignInOutcome::AccountLocked {
                until,
                newly_locked: true,
                user_id: account.user.id,
                organization_id: account.organization_id(),
                attempts: lockout_attempts,
            },
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

/// Whose live sign-in a challenge token belongs to, **without** consuming it.
///
/// The passkey ceremony needs to know which account it is finishing before it verifies anything
/// — the challenge that starts a session is consumed only once the assertion has matched, so a
/// failed attempt does not cost the caller their half-finished sign-in.
pub async fn peek_challenge(pool: &PgPool, token: &str, purpose: &str) -> Result<Option<Uuid>> {
    let token_hash = sessions::hash_token(token);
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "select user_id from mfa_challenges \
         where token_hash = $1 and purpose = $2 and consumed_at is null and expires_at > now()",
    )
    .bind(&token_hash)
    .bind(purpose)
    .fetch_optional(pool)
    .await?;
    Ok(user_id)
}

/// Increment the failure counter and lock the account when the threshold is reached.
///
/// **The threshold comes from the lockout document the operator edits**, resolved by
/// `omnion_security::enforce::resolve` — not from the two integers this function used to be
/// handed. Those came from `security_policies.lockout_attempts` (a different table, from
/// `0011_iam_advanced.sql`), while `/security/sign-in-protection` edits
/// `security_settings.lockout`. Two documents, one policy, and only one of them on the request
/// path: an operator who tuned the threshold to three kept getting ten.
///
/// **The count that decides the lock comes from the log, not from `users.failed_sign_in_count`.**
/// The column is a monotonic counter with no timestamps, so it cannot honour a failure *window*
/// — and the window is the field an operator could vary most freely (60 seconds to a day). An
/// account that failed five times last month had those failures counted forever, so raising the
/// threshold back to five locked that account on its very next typo. `sign_in_attempts` keeps the
/// timestamps, so the log is what decides; the column is kept in step for the panel and for the
/// lockout list, which render it.
///
/// The two numbers can legitimately disagree by one: this function is called *before*
/// `record_attempt` writes the row for the attempt being judged, so the log read here excludes
/// it. `failures_now + 1` is therefore the count *including* the current guess, which is what
/// the threshold compares against. Getting that off-by-one wrong would lock one attempt late.
async fn register_failure(
    pool: &PgPool,
    user_id: Uuid,
    lockout: EnforcedLockout,
) -> Result<(i32, Option<OffsetDateTime>)> {
    let failures_now = lockout.failures_in_window(pool, user_id).await? as i32;
    // The current guess is not in the log yet, so the count that *includes* it is one higher —
    // and that is the number the threshold compares against. Getting this off-by-one wrong
    // would lock one attempt late, which is one guess too many for the attacker.
    let failures_including_this_one = failures_now + 1;
    let (count, locked_until): (i32, Option<OffsetDateTime>) = sqlx::query_as(
        "update users set \
            failed_sign_in_count = $2, \
            locked_until = case \
                when $2 >= $3 \
                then now() + make_interval(mins => $4) \
                else locked_until \
            end \
         where id = $1 \
         returning failed_sign_in_count, locked_until",
    )
    .bind(user_id)
    .bind(failures_including_this_one)
    .bind(lockout.attempts)
    .bind(lockout.lockout_minutes)
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

    /// The invariant that was broken: an address is refused for spraying, an account is locked
    /// for guessing *its own* password, and the second can still happen.
    ///
    /// A unit test is enough for the arithmetic, but it is worth stating why. The broken version
    /// compiled, passed every unit test, and answered `403 address_blocked` on exactly the
    /// attempt that would have incremented the account counter — so the counter never moved and
    /// no account was ever locked. The reader has to be able to see that one number is strictly
    /// larger than the other, because the code is where a future edit would undo it.
    #[test]
    fn the_address_threshold_is_strictly_larger_than_the_account_threshold() {
        assert!(
            DEFAULT_ADDRESS_FAILURE_MULTIPLE > 1,
            "a multiple of 1 makes the address rule and the account rule fire on the same attempt"
        );
        for account_threshold in [3_i32, 5, 10, 50] {
            let address_limit = i64::from(account_threshold) * i64::from(DEFAULT_ADDRESS_FAILURE_MULTIPLE);
            assert!(
                address_limit > i64::from(account_threshold),
                "the address limit ({address_limit}) must leave room for the account threshold \
                 ({account_threshold}) to be reached first"
            );
        }
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
