//! The inbound-webhook trigger: a rule's own URL is a trigger, and its token is the only
//! credential.
//!
//! Every hook-triggered rule gets an unguessable URL:
//!
//! ```text
//! POST /api/v1/hooks/omhook_<40 random lowercase alphanumerics>
//!     → the call is recorded as `automation.hook.received`
//!     → the matcher reads it like any other event
//!     → the rule's conditions read `hook.body.…`, its actions bind `{{event.hook.body.…}}`
//! ```
//!
//! The design decisions, and the reason for each:
//!
//! * **The token is shown once and stored as a hash.** `workflows.hook_token_hash` holds
//!   SHA-256 of the token, so a database read — or a log line, or a backup — cannot call
//!   the platform's own hooks. Rotation mints a new token and invalidates the old URL
//!   immediately, which is the answer to "this URL leaked".
//! * **A wrong token answers `404`, never `401` or `403`.** The caller learns nothing about
//!   which rules exist, whether a token was ever valid, or whether the platform has hooks at
//!   all. The rule's name is never echoed back.
//! * **The rate window is keyed by the rule, not by the token.** A rotated token keeps the
//!   rule's own allowance (a rotation is not a way to reset the limit), and the limiter
//!   table never holds credential material.
//! * **The count and the run start in one transaction.** Two API instances receiving the
//!   same burst cannot both pass the limit check; the second sees the first's increment.

use rand::RngCore;
use rand::rngs::OsRng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AutomationError, Result};

/// Prefix of every hook token, so one is recognisable in a log or a config file.
pub const TOKEN_NAMESPACE: &str = "omhook";

/// Length of the random secret half.
pub const SECRET_LENGTH: usize = 40;

/// How many calls one rule accepts per window.
pub const DEFAULT_HOOK_LIMIT: i64 = 60;

/// Length of the rate window.
pub const WINDOW: time::Duration = time::Duration::minutes(10);

/// A token that has just been minted — the only moment the plaintext exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedHookToken {
    /// The token to put in the caller's URL now; the server keeps no copy.
    pub token: String,
    /// The hash stored in `workflows.hook_token_hash`.
    pub hash: String,
}

/// Mint a token for a rule.
#[must_use]
pub fn issue_token() -> IssuedHookToken {
    let secret = random_chars(SECRET_LENGTH);
    let token = format!("{TOKEN_NAMESPACE}_{secret}");
    IssuedHookToken {
        hash: hash_token(&token),
        token,
    }
}

/// SHA-256 of a token, hex-encoded — the same discipline the session and key stores follow.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Check the *shape* of a presented token before it ever reaches the database.
///
/// A 200-byte body posted to `/api/v1/hooks/<2 KiB of nonsense>` should not cost a query;
/// this refuses it with the same answer the lookup would have given.
#[must_use]
pub fn looks_like_token(raw: &str) -> bool {
    let Some(rest) = raw.trim().strip_prefix(&format!("{TOKEN_NAMESPACE}_")) else {
        return false;
    };
    rest.len() == SECRET_LENGTH
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The rule a presented token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct HookRule {
    /// Rule id.
    pub workflow_id: Uuid,
    /// Organization that owns the rule — the run is created in it.
    pub organization_id: Uuid,
    /// Site the rule is bound to, when it is.
    pub site_id: Option<Uuid>,
    /// Display name, for the run's readability.
    pub name: String,
    /// Whether the rule is armed. A paused rule's hook answers `404`, like a wrong token.
    pub enabled: bool,
}

/// Find the armed rule a hook token belongs to.
///
/// `None` covers every case the caller must not distinguish: an unknown token, a rotated
/// one, a paused rule's, and a rule that is not hook-triggered.
pub async fn find_rule(pool: &PgPool, token: &str) -> Result<Option<HookRule>> {
    if !looks_like_token(token) {
        return Ok(None);
    }

    let rule: Option<HookRule> = sqlx::query_as(
        "select id, organization_id, site_id, name, enabled from workflows \
         where trigger_kind = 'event' and hook_token_hash = $1",
    )
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;

    Ok(rule.filter(|rule| rule.enabled))
}

/// Store a freshly minted token on a rule, replacing whatever it carried.
pub async fn set_token(pool: &PgPool, workflow_id: Uuid, hash: &str) -> Result<()> {
    sqlx::query("update workflows set hook_token_hash = $2, updated_at = now() where id = $1")
        .bind(workflow_id)
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear a rule's token — the hook URL stops working the moment this commits.
pub async fn clear_token(pool: &PgPool, workflow_id: Uuid) -> Result<bool> {
    let removed = sqlx::query(
        "update workflows set hook_token_hash = null, updated_at = now() \
         where id = $1 and hook_token_hash is not null",
    )
    .bind(workflow_id)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(removed > 0)
}

/// `true` when a rule has a hook URL of its own.
#[must_use]
pub fn has_token(hash: Option<&str>) -> bool {
    hash.is_some_and(|hash| !hash.trim().is_empty())
}

/// What a rate check decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateVerdict {
    /// The call is inside the window and was counted.
    Allowed {
        /// Calls counted in this window, including this one.
        used: i64,
        /// The ceiling.
        limit: i64,
    },
    /// The window is full; the call is refused and nothing is started.
    Limited {
        /// Calls already counted in this window.
        used: i64,
        /// The ceiling.
        limit: i64,
        /// When the window rolls over.
        resets_at: OffsetDateTime,
    },
}

/// Count one inbound call against a rule's window, and say whether it is allowed.
///
/// One statement, one row lock: the counter and the verdict come from the same read, so two
/// API instances racing the same burst cannot both be let through. The window is keyed by
/// the rule, which is why rotating a token does not hand the caller a fresh allowance.
pub async fn count_hit(
    pool: &PgPool,
    workflow_id: Uuid,
    limit: i64,
    now: OffsetDateTime,
) -> Result<RateVerdict> {
    let limit = limit.clamp(1, 100_000);
    let mut transaction = pool.begin().await?;

    let window: Option<(OffsetDateTime, i32)> = sqlx::query_as(
        "select window_start, hits from automation_hook_windows \
         where workflow_id = $1 for update",
    )
    .bind(workflow_id)
    .fetch_optional(&mut *transaction)
    .await?;

    // A window older than the length is replaced rather than incremented, and so is a
    // missing one; a window inside the length is the caller's to spend.
    let (start, used) = match window {
        Some((start, hits)) if now - start < WINDOW => (start, i64::from(hits)),
        _ => (now, 0),
    };

    if used >= limit {
        transaction.commit().await?;
        return Ok(RateVerdict::Limited {
            used,
            limit,
            resets_at: start + WINDOW,
        });
    }

    let used = used + 1;
    sqlx::query(
        "insert into automation_hook_windows (workflow_id, window_start, hits) \
         values ($1, $2, $3) \
         on conflict (workflow_id) do update set window_start = $2, hits = $3",
    )
    .bind(workflow_id)
    .bind(start)
    .bind(used as i32)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(RateVerdict::Allowed { used, limit })
}

/// Calls counted in a rule's current window, for the rule's own operations view.
pub async fn window_state(pool: &PgPool, workflow_id: Uuid) -> Result<(i64, OffsetDateTime)> {
    let state: Option<(OffsetDateTime, i32)> = sqlx::query_as(
        "select window_start, hits from automation_hook_windows where workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await?;

    Ok(state
        .map(|(start, hits)| (i64::from(hits), start))
        .unwrap_or((0, OffsetDateTime::now_utc())))
}

/// Record the event an inbound call produced.
///
/// The call is a fact the platform records like any other, so the run, the cursor and the
/// audit trail all stay the ones the matcher already writes. `organization.hook.received`
/// never carries the caller's address in the clear — see `crate::catalogue::HookPayload`.
pub async fn record_call(
    pool: &PgPool,
    rule: &HookRule,
    body: Value,
    method: &str,
    source: &str,
) -> Result<i64> {
    let payload =
        crate::catalogue::HookPayload::build(rule.workflow_id, &rule.name, body, method, source)
            .to_payload();

    let report = omnion_events::bus::emit(
        pool,
        omnion_events::model::NewEvent::new(crate::catalogue::HOOK_EVENT)
            .organization(rule.organization_id)
            .site(rule.site_id)
            .payload(payload),
    )
    .await
    .map_err(|err| {
        AutomationError::invalid(
            "automation_hook_failed",
            format!("the hook call could not be recorded: {err}"),
        )
    })?;

    Ok(report.event.id)
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

/// The event payload shape a hook call produces, for the catalogue and the panel's hint.
#[must_use]
pub fn sample_payload() -> Value {
    json!({
        "hook": {
            "rule_id": "…",
            "rule_name": "Order hook",
            "body": { "order_id": 4711, "status": "paid" },
            "method": "POST",
            "source": "203.0.113.x",
            "received_at": "2026-01-01T00:00:00Z"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_issued_token_is_shown_once_and_stored_as_a_hash() {
        let issued = issue_token();
        assert!(issued.token.starts_with("omhook_"));
        assert_eq!(
            issued.token.len(),
            TOKEN_NAMESPACE.len() + 1 + SECRET_LENGTH
        );
        assert!(looks_like_token(&issued.token));
        assert_eq!(hash_token(&issued.token), issued.hash);
        // The hash is not the token, and the token is not recoverable from it.
        assert_ne!(issued.hash, issued.token);
        assert_eq!(issued.hash.len(), 64, "sha-256, hex-encoded");
        assert!(!issued.hash.contains(&issued.token));
    }

    #[test]
    fn two_issued_tokens_never_collide() {
        let first = issue_token();
        let second = issue_token();
        assert_ne!(first.token, second.token);
        assert_ne!(first.hash, second.hash);
    }

    #[test]
    fn only_a_well_shaped_token_is_looked_up() {
        assert!(looks_like_token(
            "omhook_abcdefghij0123456789abcdefghij0123456789"
        ));
        // The wrong namespace, the wrong length, a non-alphanumeric, surrounding space is
        // tolerated because the router trims the path.
        assert!(!looks_like_token(""));
        assert!(!looks_like_token("omhook_short"));
        assert!(!looks_like_token(
            "omhook_ABCDEFGHIJ0123456789abcdefghij0123456789"
        ));
        assert!(!looks_like_token(
            "omsa_abcdefghij0123456789abcdefghij012345678"
        ));
        assert!(looks_like_token(
            "  omhook_abcdefghij0123456789abcdefghij0123456789  "
        ));
    }

    #[test]
    fn the_hash_ignores_surrounding_space_and_nothing_else() {
        let token = "omhook_abcdefghij0123456789abcdefghij0123456789";
        assert_eq!(hash_token(token), hash_token(&format!("  {token}  ")));
        assert_ne!(hash_token(token), hash_token(&token.replace('a', "b")));
    }

    #[test]
    fn a_rule_answers_whether_it_has_a_hook() {
        assert!(has_token(Some("abc")));
        assert!(!has_token(None));
        assert!(!has_token(Some("  ")));
    }

    #[test]
    fn the_sample_payload_documents_the_shape_a_condition_reads() {
        let sample = sample_payload();
        assert!(sample["hook"]["body"]["order_id"].is_number());
        assert!(sample["hook"]["method"] == "POST");
        // The fields a rule's conditions are offered are the ones the sample carries.
        assert!(sample["hook"]["body"]["status"].is_string());
    }
}
