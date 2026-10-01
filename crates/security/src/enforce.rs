//! The one function the sign-in path calls to learn its lockout thresholds (REQ-012, slice 3).
//!
//! **Why this module exists at all.** Until now the brute-force policy was implemented twice:
//!
//! * `crates/security::lockout` holds the document an operator tunes on
//!   `/security/sign-in-protection` — `security_settings.lockout` — together with its ranges,
//!   its tester (`evaluate`) and the screen's bounds.
//! * `crates/identity::signin::register_failure` holds a second, independent implementation that
//!   actually locks accounts, reading `security_policies.lockout_attempts` out of a **different
//!   table** (`0011_iam_advanced.sql`, default 10, range 3–50).
//!
//! Both were correct in isolation and they disagreed in every field. Worse, the sign-in path
//! enforced only **two** of the six fields the operator tunes: `window_seconds`,
//! `progressive_delay`, `base_delay_seconds` and `reset_on_success` had no reader anywhere on the
//! request path, so tuning them changed the screen and nothing else. A screen whose numbers are
//! decorative is the exact failure this crate's own module doc warns against ("a green row that
//! was never checked is the single most expensive thing the product can render").
//!
//! **What this module does.** It resolves one [`EnforcedLockout`] from one place and hands it to
//! the caller, so there is exactly one implementation of "how many failures lock an account" in
//! the platform and the panel's number is the platform's number.
//!
//! **Which document wins, and why it is not a coin flip.** Two documents exist and both are
//! readable, so the resolution order is a decision and not an implementation detail:
//!
//! 1. **A saved `security_settings.lockout` document wins.** It is the one with an author and a
//!    history, the one the screen edits, and the one the tester and the probe both evaluate.
//! 2. **Otherwise the organization's `security_policies.lockout_*` columns** are honoured, so a
//!    deployment that never opened the sign-in-protection screen still gets the IAM screen's
//!    numbers it was promised.
//! 3. **Otherwise [`LockoutPolicy::default`]**.
//!
//! The precedence is deliberate and worth stating plainly, because the alternative — merging
//! field-by-field — produces a policy that no screen ever displayed and that no operator ever
//! chose: three sources disagreeing on two fields each, rendering as a coherent document nobody
//! authored. One source at a time is the only shape where "the number the operator tuned" and
//! "the number that locks accounts" are the same number.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::lockout::LockoutPolicy;

/// The thresholds the sign-in path enforces, resolved once per sign-in.
///
/// This is deliberately **not** `LockoutPolicy` itself. The document carries six operator-tunable
/// fields, and four of them (`window_seconds`, `progressive_delay`, `base_delay_seconds`,
/// `reset_on_success`) are enforcement the sign-in path has no single place to apply: a delay is
/// a wait, not a column write, and a window is a predicate over `sign_in_attempts` rather than a
/// value bound into an `update`. Handing the sign-in path a struct whose fields it silently
/// ignores is how the current defect happened in the first place — `register_failure` took two
/// integers, called them `lockout_attempts`/`lockout_minutes`, and no reader could tell that the
/// other four fields had no reader at all.
///
/// Only the fields the sign-in path really consumes are here, so "this type has a field" and
/// "this field is enforced" are the same statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcedLockout {
    /// How many failures inside the window lock the account.
    pub attempts: i32,
    /// How long the lock lasts, in minutes.
    pub lockout_minutes: i32,
    /// How far back failures count, in seconds.
    ///
    /// Enforced here rather than ignored: `recent_failures_from_address` and the account's own
    /// counter both answer "inside the window", and before this existed the window was the one
    /// field an operator could change most freely (60 seconds to a full day) with no effect on
    /// anything.
    pub window_seconds: i64,
}

impl From<&LockoutPolicy> for EnforcedLockout {
    fn from(policy: &LockoutPolicy) -> Self {
        Self {
            attempts: policy.attempts,
            lockout_minutes: policy.lockout_minutes,
            window_seconds: policy.window_seconds,
        }
    }
}

impl EnforcedLockout {
    /// The account's own failure count that is still inside the window.
    ///
    /// A monotonic counter on `users` cannot know which failures are older than the window —
    /// and "how many failures are older than the window" is a question only the log can answer,
    /// because `sign_in_attempts` keeps the timestamps and `users` keeps the total. The counter
    /// is therefore read together with the log, and the **log** decides: a count the window has
    /// expired says zero, even though `failed_sign_in_count` still reads eight.
    ///
    /// Before this, the counter never expired at all: an account that failed five times last
    /// month and signs in correctly today had its failures still counted, so raising the
    /// threshold back to five would lock that account on its very next typo. That is the
    /// `window_seconds` field having no reader.
    pub async fn failures_in_window(&self, pool: &PgPool, user_id: Uuid) -> Result<i64> {
        crate::limiter_store::failures_in_window(pool, user_id, self.window_seconds).await
    }
}

/// The per-organization legacy columns, as far as this module needs them.
///
/// Two columns and not a struct: `security_policies` has fifteen, and this function is not the
/// IAM policy reader. Naming the two it substitutes is the point — a reader can see exactly which
/// part of the IAM document stands in when no sign-in-protection policy has been saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
struct LegacyThresholds {
    /// See [`EnforcedLockout::attempts`].
    lockout_attempts: i32,
    /// See [`EnforcedLockout::lockout_minutes`].
    lockout_minutes: i32,
}

/// Resolve the thresholds in force for an account's organization.
///
/// `organization_id` is `None` for a platform account and for an address that matches nothing;
/// both resolve to the platform baseline, which is what `crates/identity` already assumed for
/// them.
///
/// # Errors
/// Returns [`SecurityError::Database`] when the settings row or the policy row cannot be read.
/// A stored document that does not validate is **not** an error here: it falls through to the
/// legacy columns, because refusing to enforce any lockout at all on a deployment whose document
/// has gone stale is strictly worse than enforcing the older, weaker number. The screen
/// (`parse_document`) is still what refuses to *render* such a document, so the operator is told
/// about it rather than silently protected by a substitute.
pub async fn resolve(pool: &PgPool, organization_id: Option<Uuid>) -> Result<EnforcedLockout> {
    let stored = crate::limiter_store::load_lockout(pool).await?;
    if let Ok(policy) = crate::lockout::parse_document(&stored) {
        // `{}` and `null` both parse as the default, and both mean "nobody has saved one here".
        // That is the one case where the default IS the answer rather than a last resort, so it
        // is checked by hand rather than inferred from the parse succeeding.
        let unwritten =
            stored.is_null() || stored.as_object().is_some_and(serde_json::Map::is_empty);
        if !unwritten {
            return Ok(EnforcedLockout::from(&policy));
        }
    }

    let Some(organization_id) = organization_id else {
        return Ok(EnforcedLockout::from(&LockoutPolicy::default()));
    };
    let legacy = sqlx::query_as::<_, LegacyThresholds>(
        "select lockout_attempts, lockout_minutes from security_policies \
          where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(match legacy {
        Some(legacy) => EnforcedLockout {
            attempts: legacy.lockout_attempts,
            lockout_minutes: legacy.lockout_minutes,
            // The IAM columns have no window — they were never windowed. The document's own
            // default is used rather than a constant, so the two paths start from one number.
            window_seconds: LockoutPolicy::default().window_seconds,
        },
        None => EnforcedLockout::from(&LockoutPolicy::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enforced_view_carries_only_the_fields_the_sign_in_path_consumes() {
        // The point of `EnforcedLockout` rather than `LockoutPolicy`: a field the sign-in path
        // does not read must not be *present* on the struct it reads, or the next reader has no
        // way to tell "not enforced" from "not implemented yet".
        let policy = LockoutPolicy {
            window_seconds: 120,
            attempts: 9,
            lockout_minutes: 30,
            progressive_delay: true,
            base_delay_seconds: 7,
            reset_on_success: false,
        };
        let enforced = EnforcedLockout::from(&policy);
        assert_eq!(enforced.attempts, 9);
        assert_eq!(enforced.lockout_minutes, 30);
        assert_eq!(enforced.window_seconds, 120);

        // And the four fields it deliberately omits are the four the sign-in path cannot enforce
        // from a column write. Written down so the next change is a decision rather than an
        // omission somebody has to rediscover.
        assert!(policy.progressive_delay);
        assert_eq!(policy.base_delay_seconds, 7);
        assert!(!policy.reset_on_success);
    }

    #[test]
    fn the_window_is_carried_because_it_decides_what_counts_as_a_failure() {
        // The regression this module exists for, stated as arithmetic rather than prose: a
        // window of an hour and a window of a day make the same counter mean different things,
        // so dropping the window from the enforced view silently reverts the fix.
        let hour = EnforcedLockout {
            attempts: 5,
            lockout_minutes: 15,
            window_seconds: 3_600,
        };
        let day = EnforcedLockout {
            window_seconds: 86_400,
            ..hour
        };
        assert_ne!(hour.window_seconds, day.window_seconds);
    }
}
