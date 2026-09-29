//! Server-side sessions.
//!
//! A session is a row in the `sessions` table; the client only ever holds the raw token in an
//! HttpOnly cookie. The database stores the SHA-256 hash of that token, so a leaked database
//! dump cannot be replayed as a session (docs/07-IAM.md).
//!
//! Three lifetimes come from the organization's security policy, never from a constant
//! (docs/07-IAM.md §12, REQ-006 slice 3):
//!
//! - **idle** — enforced when a session is resolved: `last_seen_at` older than the policy's
//!   idle window stops working, even though the row is still there (the panel reads it back as
//!   `idle`);
//! - **absolute** — `absolute_expires_at`, which no amount of activity can move;
//! - **concurrent** — the cap: opening one session past it revokes the oldest live one.
//!
//! `step_up_at` marks the last time the caller proved they are still the person at the
//! keyboard; dangerous operations demand a fresh mark (see `apps/api` route guards).

use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::security::SessionPolicy;
use crate::users::User;

/// Session lifetime in days used when an account has no policy; also the cookie `Max-Age`.
pub const SESSION_TTL_DAYS: i32 = 30;

/// Session lifetime in seconds (cookie `Max-Age`).
pub const SESSION_TTL_SECONDS: i64 = SESSION_TTL_DAYS as i64 * 24 * 60 * 60;

/// Idle lifetime assumed without a policy (matches the table default).
pub const DEFAULT_IDLE_MINUTES: i32 = 120;

/// How long a step-up stays fresh.
pub const STEP_UP_WINDOW_MINUTES: i64 = 10;

/// Entropy of a session token, in bytes (256 bits).
const TOKEN_BYTES: usize = 32;

/// Longest user-agent string kept for diagnostics.
const MAX_USER_AGENT_LENGTH: usize = 512;

/// Longest reason recorded on a revocation.
const MAX_REASON_LENGTH: usize = 120;

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Session {
    /// Primary key.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Expiry timestamp (the absolute lifetime, when one was set).
    pub expires_at: OffsetDateTime,
    /// Last time the session was seen (best effort, at most once a minute).
    pub last_seen_at: Option<OffsetDateTime>,
    /// Hard end of the session, when the policy names one.
    pub absolute_expires_at: Option<OffsetDateTime>,
    /// Device the session was created from.
    pub device_id: Option<Uuid>,
    /// How the caller authenticated (`password`, `recovery_code`, `passkey`, …).
    pub auth_methods: Vec<String>,
    /// When the session was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// Why it was revoked (`logout`, `admin_revoke`, `sign_out_all`, `concurrent_cap`).
    pub revoke_reason: Option<String>,
    /// When the caller last proved their identity for a dangerous operation.
    pub step_up_at: Option<OffsetDateTime>,
}

/// A resolved session together with the account it belongs to.
#[derive(Debug, Clone)]
pub struct AuthenticatedSession {
    /// The session row.
    pub session: Session,
    /// The account.
    pub user: User,
}

/// What a new session should record about its origin.
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    /// User agent of the sign-in request.
    pub user_agent: Option<String>,
    /// Remote address of the sign-in request.
    pub ip_address: Option<String>,
    /// Device the sign-in was attributed to.
    pub device_id: Option<Uuid>,
    /// How the caller authenticated.
    pub auth_methods: Vec<String>,
}

/// A session with the account and device a panel needs to show it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionView {
    /// Session id.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Account address.
    pub user_email: String,
    /// Account display name.
    pub user_display_name: String,
    /// Address the session came from.
    pub ip_address: Option<String>,
    /// User agent as recorded.
    pub user_agent: Option<String>,
    /// Device label, when the session carries a device.
    pub device_label: Option<String>,
    /// How the caller authenticated.
    pub auth_methods: Vec<String>,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last activity.
    pub last_seen_at: Option<OffsetDateTime>,
    /// Expiry.
    pub expires_at: OffsetDateTime,
    /// Hard end, when one was set.
    pub absolute_expires_at: Option<OffsetDateTime>,
    /// Revocation timestamp.
    pub revoked_at: Option<OffsetDateTime>,
    /// Revocation reason.
    pub revoke_reason: Option<String>,
    /// When the caller last stepped up.
    pub step_up_at: Option<OffsetDateTime>,
}

/// Filters of the session list.
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    /// Only sessions of this account.
    pub user_id: Option<Uuid>,
    /// Only sessions of accounts in this organization.
    pub organization_id: Option<Uuid>,
    /// Free text over account address and display name.
    pub search: Option<String>,
    /// Only sessions in this state (`live`, `idle`, `expired`, `revoked`); computed per row.
    pub state: Option<String>,
    /// Include revoked and expired rows.
    pub include_inactive: bool,
}

/// Generate a fresh session token (256 bits of entropy, hex encoded).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Hash a session token for storage and lookup.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Create a session for `user_id` and return it together with the raw token.
///
/// The raw token is returned exactly once — it belongs in the response cookie and nowhere
/// else; only its hash is persisted. Lifetimes come from `policy`: the absolute lifetime sets
/// both `expires_at` and `absolute_expires_at`, and opening a session past the concurrent cap
/// revokes the oldest live one instead of refusing the sign-in.
pub async fn create_session_with_policy(
    pool: &PgPool,
    user_id: Uuid,
    new: NewSession,
    policy: SessionPolicy,
) -> Result<(Session, String)> {
    let token = generate_token();
    let token_hash = hash_token(&token);
    let user_agent = new
        .user_agent
        .as_deref()
        .map(|value| truncate(value, MAX_USER_AGENT_LENGTH));
    let auth_methods: Vec<String> = new.auth_methods.clone();

    let session: Session = sqlx::query_as(
        "insert into sessions \
            (user_id, token_hash, user_agent, ip_address, expires_at, absolute_expires_at, \
             device_id, auth_methods) \
         values ($1, $2, $3, cast($4 as inet), \
                 now() + make_interval(days => $5), now() + make_interval(days => $5), $6, $7) \
         returning id, user_id, created_at, expires_at, last_seen_at, absolute_expires_at, \
                   device_id, auth_methods, revoked_at, revoke_reason, step_up_at",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(user_agent.as_deref())
    .bind(new.ip_address.as_deref())
    .bind(policy.absolute_days)
    .bind(new.device_id)
    .bind(&auth_methods)
    .fetch_one(pool)
    .await?;

    // The concurrent cap: keep the newest `max - 1` live sessions and retire anything older,
    // so the session about to be handed out fits inside the cap.
    let keep = i64::from(policy.concurrent_max.max(1)) - 1;
    if keep >= 0 {
        sqlx::query(
            "update sessions set revoked_at = now(), revoke_reason = 'concurrent_cap' \
             where id in ( \
                 select id from sessions \
                 where user_id = $1 and revoked_at is null and expires_at > now() and id <> $2 \
                 order by created_at desc offset $3 \
             )",
        )
        .bind(user_id)
        .bind(session.id)
        .bind(keep)
        .execute(pool)
        .await?;
    }

    Ok((session, token))
}

/// Create a session with the default lifetimes (no policy in scope).
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
) -> Result<(Session, String)> {
    create_session_with_policy(
        pool,
        user_id,
        NewSession {
            user_agent: user_agent.map(str::to_owned),
            ip_address: ip_address.map(str::to_owned),
            device_id: None,
            auth_methods: vec!["password".to_owned()],
        },
        SessionPolicy::default(),
    )
    .await
}

/// Resolve a raw token into its session and account.
///
/// Returns `None` for unknown, expired, idle, revoked or still-unused-in-time tokens, and for
/// accounts that are no longer active — the caller cannot tell those cases apart, which is
/// intentional. The idle check reads the account's organization policy, so a session that sat
/// unused past the window stops working without anyone deleting it.
pub async fn resolve_session(pool: &PgPool, token: &str) -> Result<Option<AuthenticatedSession>> {
    let token_hash = hash_token(token);

    let row: Option<ResolvedRow> = sqlx::query_as(
        "select s.id as session_id, s.user_id as session_user_id, \
                s.created_at as session_created_at, s.expires_at as session_expires_at, \
                s.last_seen_at as session_last_seen_at, \
                s.absolute_expires_at as session_absolute_expires_at, \
                s.device_id as session_device_id, s.auth_methods as session_auth_methods, \
                s.revoked_at as session_revoked_at, s.revoke_reason as session_revoke_reason, \
                s.step_up_at as session_step_up_at, \
                u.id as user_id, u.organization_id, u.email, u.display_name, u.status, \
                u.created_at as user_created_at \
         from sessions s \
         join users u on u.id = s.user_id \
         left join security_policies p on p.organization_id = u.organization_id \
         where s.token_hash = $1 \
           and s.revoked_at is null \
           and s.expires_at > now() \
           and (s.absolute_expires_at is null or s.absolute_expires_at > now()) \
           and coalesce(s.last_seen_at, s.created_at) > \
               now() - make_interval(mins => coalesce(p.session_idle_minutes, $2)) \
           and u.status = 'active'",
    )
    .bind(&token_hash)
    .bind(DEFAULT_IDLE_MINUTES)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(ResolvedRow::into_authenticated))
}

/// Record that a session was used.
///
/// The write is throttled to once a minute: `last_seen_at` only has to be accurate enough for
/// the idle window, and a session touch on every request would write far more than it informs.
pub async fn touch_session(pool: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query(
        "update sessions set last_seen_at = now() \
         where id = $1 and (last_seen_at is null or last_seen_at < now() - interval '1 minute')",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke a session by raw token. Returns `true` when a live session was revoked.
pub async fn revoke_session(pool: &PgPool, token: &str) -> Result<bool> {
    let token_hash = hash_token(token);
    let result = sqlx::query(
        "update sessions set revoked_at = now(), revoke_reason = coalesce(revoke_reason, 'logout') \
         where token_hash = $1 and revoked_at is null",
    )
    .bind(&token_hash)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Revoke one session by id, recording who did it and why.
pub async fn revoke_session_by_id(
    pool: &PgPool,
    session_id: Uuid,
    revoked_by: Option<Uuid>,
    reason: &str,
) -> Result<Option<Session>> {
    let reason = truncate(reason, MAX_REASON_LENGTH);
    let session: Option<Session> = sqlx::query_as(
        "update sessions set revoked_at = now(), revoked_by = $3, revoke_reason = $2 \
         where id = $1 and revoked_at is null \
         returning id, user_id, created_at, expires_at, last_seen_at, absolute_expires_at, \
                   device_id, auth_methods, revoked_at, revoke_reason, step_up_at",
    )
    .bind(session_id)
    .bind(&reason)
    .bind(revoked_by)
    .fetch_optional(pool)
    .await?;
    Ok(session)
}

/// Revoke every live session of an account and answer their ids (one event each).
pub async fn sign_out_all(
    pool: &PgPool,
    user_id: Uuid,
    revoked_by: Option<Uuid>,
    reason: &str,
) -> Result<Vec<Uuid>> {
    let reason = truncate(reason, MAX_REASON_LENGTH);
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "update sessions set revoked_at = now(), revoked_by = $2, revoke_reason = $3 \
         where user_id = $1 and revoked_at is null \
         returning id",
    )
    .bind(user_id)
    .bind(revoked_by)
    .bind(&reason)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Revoke every live session of an account (used by password resets and admin actions).
pub async fn revoke_sessions_for_user(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    Ok(sign_out_all(pool, user_id, None, "password_reset")
        .await?
        .len() as u64)
}

/// Mark that the caller proved their identity again (step-up).
pub async fn mark_step_up(pool: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query("update sessions set step_up_at = now() where id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a session's step-up is recent enough for a dangerous operation.
#[must_use]
pub fn step_up_is_fresh(session: &Session, now: OffsetDateTime, window_minutes: i64) -> bool {
    match session.step_up_at {
        Some(at) => at >= now - time::Duration::minutes(window_minutes),
        None => false,
    }
}

/// The state a reader sees for a session.
#[must_use]
pub fn state_of(session: &SessionView, now: OffsetDateTime, idle_minutes: i32) -> &'static str {
    if session.revoked_at.is_some() {
        return "revoked";
    }
    if session.expires_at <= now || session.absolute_expires_at.is_some_and(|end| end <= now) {
        return "expired";
    }
    let idle_since = session.last_seen_at.unwrap_or(session.created_at);
    if idle_since <= now - time::Duration::minutes(i64::from(idle_minutes)) {
        return "idle";
    }
    "live"
}

/// List sessions for the panel, newest first.
pub async fn list_sessions(pool: &PgPool, filter: &SessionFilter) -> Result<Vec<SessionView>> {
    let search = filter
        .search
        .as_ref()
        .map(|value| format!("%{}%", value.trim().to_lowercase()));

    let rows: Vec<SessionView> = sqlx::query_as(
        "select s.id, s.user_id, u.email as user_email, u.display_name as user_display_name, \
                host(s.ip_address) as ip_address, s.user_agent, d.label as device_label, \
                s.auth_methods, s.created_at, s.last_seen_at, s.expires_at, \
                s.absolute_expires_at, s.revoked_at, s.revoke_reason, s.step_up_at \
         from sessions s \
         join users u on u.id = s.user_id \
         left join user_devices d on d.id = s.device_id \
         where ($1::uuid is null or s.user_id = $1) \
           and ($2::uuid is null or u.organization_id = $2) \
           and ($3::bool or (s.revoked_at is null and s.expires_at > now())) \
           and ($4::text is null or u.email ilike $4 or u.display_name ilike $4) \
         order by s.created_at desc \
         limit 500",
    )
    .bind(filter.user_id)
    .bind(filter.organization_id)
    .bind(filter.include_inactive)
    .bind(search.as_deref())
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// The idle window a session list should judge rows by: the strictest policy in scope, or the
/// default when the caller filters to no organization at all.
pub async fn idle_minutes_for_organization(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<i32> {
    let Some(organization_id) = organization_id else {
        return Ok(DEFAULT_IDLE_MINUTES);
    };
    let minutes: Option<i32> = sqlx::query_scalar(
        "select session_idle_minutes from security_policies where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(minutes.unwrap_or(DEFAULT_IDLE_MINUTES))
}

/// Row shape of the session/account join.
#[derive(sqlx::FromRow)]
struct ResolvedRow {
    session_id: Uuid,
    session_user_id: Uuid,
    session_created_at: OffsetDateTime,
    session_expires_at: OffsetDateTime,
    session_last_seen_at: Option<OffsetDateTime>,
    session_absolute_expires_at: Option<OffsetDateTime>,
    session_device_id: Option<Uuid>,
    session_auth_methods: Vec<String>,
    session_revoked_at: Option<OffsetDateTime>,
    session_revoke_reason: Option<String>,
    session_step_up_at: Option<OffsetDateTime>,
    user_id: Uuid,
    organization_id: Option<Uuid>,
    email: String,
    display_name: String,
    status: String,
    user_created_at: OffsetDateTime,
}

impl ResolvedRow {
    fn into_authenticated(self) -> AuthenticatedSession {
        AuthenticatedSession {
            session: Session {
                id: self.session_id,
                user_id: self.session_user_id,
                created_at: self.session_created_at,
                expires_at: self.session_expires_at,
                last_seen_at: self.session_last_seen_at,
                absolute_expires_at: self.session_absolute_expires_at,
                device_id: self.session_device_id,
                auth_methods: self.session_auth_methods,
                revoked_at: self.session_revoked_at,
                revoke_reason: self.session_revoke_reason,
                step_up_at: self.session_step_up_at,
            },
            user: User {
                id: self.user_id,
                organization_id: self.organization_id,
                email: self.email,
                display_name: self.display_name,
                status: self.status,
                created_at: self.user_created_at,
            },
        }
    }
}

/// Cut `value` to at most `max` characters, on a character boundary.
fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Cut a revocation reason to what the column is prepared to store.
///
/// `revoke_reason` is free text and the column has no length limit, but the panel renders it in
/// a session list beside five other columns. An unbounded reason written by a caller that
/// interpolated a provider's error message is a row that pushes the whole list sideways, so the
/// cap is here rather than at each call site — a reason that is too long is truncated, never
/// refused, because losing the tail of a reason is a cosmetic problem and failing a deactivation
/// over it is not.
#[must_use]
pub fn truncate_reason(reason: &str) -> String {
    truncate(reason, MAX_REASON_LENGTH)
}

/// Reject tokens that cannot have been produced by [`generate_token`].
pub fn validate_token_shape(token: &str) -> Result<()> {
    let shaped = token.len() == TOKEN_BYTES * 2 && token.chars().all(|c| c.is_ascii_hexdigit());
    if shaped {
        return Ok(());
    }
    Err(IdentityError::InvalidToken(
        "expected 64 hexadecimal characters".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn view(
        created_at: OffsetDateTime,
        last_seen_at: Option<OffsetDateTime>,
        expires_at: OffsetDateTime,
        revoked_at: Option<OffsetDateTime>,
    ) -> SessionView {
        SessionView {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            user_email: "qa@omnion.test".to_owned(),
            user_display_name: "QA".to_owned(),
            ip_address: None,
            user_agent: None,
            device_label: None,
            auth_methods: vec![],
            created_at,
            last_seen_at,
            expires_at,
            absolute_expires_at: None,
            revoked_at,
            revoke_reason: None,
            step_up_at: None,
        }
    }

    #[test]
    fn tokens_are_256_bits_of_hex_and_never_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..256 {
            let token = generate_token();
            assert_eq!(token.len(), 64, "token: {token}");
            validate_token_shape(&token).expect("generated tokens must pass the shape check");
            assert!(seen.insert(token), "tokens must not repeat");
        }
    }

    #[test]
    fn token_hashes_are_deterministic_and_hide_the_token() {
        let token = generate_token();
        let hash = hash_token(&token);
        assert_eq!(hash, hash_token(&token), "hashing must be deterministic");
        assert_ne!(hash, token, "the stored hash must not be the token");
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, hash_token(&generate_token()));
    }

    #[test]
    fn malformed_tokens_are_rejected_by_the_shape_check() {
        for bad in ["", "abc", &"z".repeat(64), &"a".repeat(63)] {
            assert!(validate_token_shape(bad).is_err(), "{bad:?} must fail");
        }
    }

    #[test]
    fn user_agent_is_truncated_on_a_character_boundary() {
        let long = "ü".repeat(MAX_USER_AGENT_LENGTH + 10);
        let cut = truncate(&long, MAX_USER_AGENT_LENGTH);
        assert_eq!(cut.chars().count(), MAX_USER_AGENT_LENGTH);
    }

    #[test]
    fn ttl_constants_agree() {
        assert_eq!(SESSION_TTL_SECONDS, 30 * 24 * 60 * 60);
    }

    #[test]
    fn the_state_a_reader_sees_follows_the_policy() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(100);
        let live = view(
            now,
            Some(now - time::Duration::minutes(5)),
            now + time::Duration::days(1),
            None,
        );
        assert_eq!(state_of(&live, now, 120), "live");

        // Idle: untouched past the window while the row is still live.
        let idle = view(
            now - time::Duration::days(1),
            Some(now - time::Duration::hours(3)),
            now + time::Duration::days(1),
            None,
        );
        assert_eq!(state_of(&idle, now, 120), "idle");

        // A session never touched falls back to its creation time.
        let never_touched = view(
            now - time::Duration::hours(4),
            None,
            now + time::Duration::days(1),
            None,
        );
        assert_eq!(state_of(&never_touched, now, 120), "idle");

        let expired = view(
            now - time::Duration::days(2),
            None,
            now - time::Duration::days(1),
            None,
        );
        assert_eq!(state_of(&expired, now, 120), "expired");

        let revoked = view(now, Some(now), now + time::Duration::days(1), Some(now));
        assert_eq!(state_of(&revoked, now, 120), "revoked");
    }

    #[test]
    fn step_up_freshness_is_a_window() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
        let mut session = Session {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            created_at: now,
            expires_at: now + time::Duration::days(1),
            last_seen_at: Some(now),
            absolute_expires_at: None,
            device_id: None,
            auth_methods: vec![],
            revoked_at: None,
            revoke_reason: None,
            step_up_at: None,
        };
        assert!(!step_up_is_fresh(&session, now, STEP_UP_WINDOW_MINUTES));

        session.step_up_at = Some(now - time::Duration::minutes(9));
        assert!(step_up_is_fresh(&session, now, STEP_UP_WINDOW_MINUTES));

        session.step_up_at = Some(now - time::Duration::minutes(11));
        assert!(!step_up_is_fresh(&session, now, STEP_UP_WINDOW_MINUTES));
    }
}
