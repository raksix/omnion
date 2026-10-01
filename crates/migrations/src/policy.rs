//! The migration policy singleton: which banned shapes are enforced, and how long a blocked DDL
//! waits (docs/requests/REQ-129, slice 1).
//!
//! ## A pattern that is switched off is not "allowed"
//!
//! `PATTERNS` is the closed vocabulary; this table says which of them an installation enforces.
//! The distinction matters more than it looks: "not enforced by this installation" and "safe" are
//! different sentences, and a policy screen that renders the second one has told an operator that
//! a `drop column` is fine when what is actually true is that this build will not stop one. So
//! [`Plan`](crate::runner::Plan) carries the policy it read *and* the findings under it, and the
//! policy screen renders a disabled pattern as disabled rather than as passing.
//!
//! ## The timeouts are per-run, not per-session
//!
//! `lock_timeout_ms` and `statement_timeout_ms` are applied with `set local` on the session that
//! runs the statements (see [`crate::runner::apply_locked`]), which means they expire with the
//! transaction. A `set` on the session would leak one migration's timeout into the next one's DDL
//! on a pooled connection — the kind of inheritance that only shows up under load, on the
//! migration after the one that timed out.
//!
//! ## The defaults live in Rust and in SQL, and the walk proves they agree
//!
//! `0207_migration_safety.sql` seeds the row so a fresh installation has one, and
//! [`default_row`] is the same row in Rust. Two copies of a default drift, so the walk asserts
//! that a fresh install's stored row equals [`default_row`] rather than trusting either copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{MigrationSafetyError, Result};
use crate::lint::PATTERNS;

/// The policy row, as the screen reads and writes it.
///
/// `banned_patterns` is a `jsonb` map of `pattern → enabled`, kept as a `BTreeMap` rather than the
/// raw `serde_json::Value` so a caller cannot silently write a policy that names a pattern this
/// binary has never heard of — see [`enabled_patterns`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub require_down_scripts: bool,
    pub lock_timeout_ms: i32,
    pub statement_timeout_ms: i32,
    /// Which banned shapes this installation enforces. A pattern absent from the map is
    /// **enabled** — see the module doc for why the default direction is "on".
    #[serde(default)]
    pub banned_patterns: BTreeMap<String, bool>,
    pub backfill_batch_size: i32,
    pub backfill_rate_per_second: i32,
    pub require_approval_for_destructive: bool,
    /// Who last changed it, or `None` for the seeded default.
    pub updated_by: Option<String>,
    pub updated_at: Option<OffsetDateTime>,
}

/// The bounds the migration itself enforces, as constants.
///
/// The same numbers are check constraints on `migration_policy`, and they are written down here
/// because a refusal message that quotes a bound has to quote the bound that failed. A caller
/// that reads them from the table instead would have to trust a row it is trying to validate.
pub mod bounds {
    /// `lock_timeout_ms` below this is a table rewrite, not a lock wait.
    pub const MIN_LOCK_TIMEOUT_MS: i32 = 100;
    /// Above this, a blocked DDL is an outage rather than a failed migration.
    pub const MAX_LOCK_TIMEOUT_MS: i32 = 60_000;
    /// The floor on `statement_timeout_ms`; below it, ordinary DDL times out.
    pub const MIN_STATEMENT_TIMEOUT_MS: i32 = 1_000;
    /// The ceiling: an hour, after which the runner has lost its connection anyway.
    pub const MAX_STATEMENT_TIMEOUT_MS: i32 = 3_600_000;
    /// Backfill batch floor — below 100 the per-statement overhead dominates.
    pub const MIN_BACKFILL_BATCH: i32 = 100;
    /// Backfill batch ceiling — above 100 000 a batch holds its transaction too long to pause.
    pub const MAX_BACKFILL_BATCH: i32 = 100_000;
}

impl Policy {
    /// The row a fresh installation gets.
    ///
    /// Exported so the walk can assert the stored row equals it — the alternative is two copies of
    /// the default with nothing saying which one is right.
    #[must_use]
    pub fn default_row() -> Self {
        Self {
            require_down_scripts: true,
            lock_timeout_ms: 5_000,
            statement_timeout_ms: 300_000,
            banned_patterns: BTreeMap::new(),
            backfill_batch_size: 5_000,
            backfill_rate_per_second: 200,
            require_approval_for_destructive: true,
            updated_by: None,
            updated_at: None,
        }
    }

    /// The effective enabled-set handed to [`crate::lint::lint`].
    ///
    /// Starts from "every pattern on", then applies the stored overrides. A key that is not in
    /// [`PATTERNS`] is dropped rather than passed on, so a policy written by a newer binary
    /// cannot make a lint ignore a pattern this build has no rule for.
    #[must_use]
    pub fn enabled_patterns(&self) -> BTreeMap<String, bool> {
        PATTERNS
            .iter()
            .map(|pattern| {
                let enabled = self
                    .banned_patterns
                    .get(pattern.key)
                    .copied()
                    .unwrap_or(true);
                (pattern.key.to_owned(), enabled)
            })
            .collect()
    }

    /// Pattern keys this policy names that no binary here knows about.
    ///
    /// Reported rather than silently dropped: an installation that switched off a rule its binary
    /// does not implement has a false sense of coverage, and the policy screen is the only place
    /// that can say so.
    #[must_use]
    pub fn unknown_patterns(&self) -> Vec<&str> {
        self.banned_patterns
            .keys()
            .filter(|key| !PATTERNS.iter().any(|pattern| pattern.key == key.as_str()))
            .map(String::as_str)
            .collect()
    }

    /// Reject a value the table's check constraints would reject.
    ///
    /// # Errors
    ///
    /// [`MigrationSafetyError::InvalidVersion`] with a message naming the field and the bound,
    /// because a `23514` from a `put policy` tells the form nothing about which input to fix.
    pub fn validate(&self) -> Result<()> {
        let checks: [(bool, String); 5] = [
            (
                (bounds::MIN_LOCK_TIMEOUT_MS..=bounds::MAX_LOCK_TIMEOUT_MS)
                    .contains(&self.lock_timeout_ms),
                format!(
                    "lock_timeout_ms must be {}..={}",
                    bounds::MIN_LOCK_TIMEOUT_MS,
                    bounds::MAX_LOCK_TIMEOUT_MS
                ),
            ),
            (
                (bounds::MIN_STATEMENT_TIMEOUT_MS..=bounds::MAX_STATEMENT_TIMEOUT_MS)
                    .contains(&self.statement_timeout_ms),
                format!(
                    "statement_timeout_ms must be {}..={}",
                    bounds::MIN_STATEMENT_TIMEOUT_MS,
                    bounds::MAX_STATEMENT_TIMEOUT_MS
                ),
            ),
            (
                (bounds::MIN_BACKFILL_BATCH..=bounds::MAX_BACKFILL_BATCH)
                    .contains(&self.backfill_batch_size),
                format!(
                    "backfill_batch_size must be {}..={}",
                    bounds::MIN_BACKFILL_BATCH,
                    bounds::MAX_BACKFILL_BATCH
                ),
            ),
            (
                self.backfill_rate_per_second > 0,
                "backfill_rate_per_second must be positive".to_owned(),
            ),
            (
                self.unknown_patterns().is_empty(),
                format!(
                    "unknown pattern(s): {} — this binary knows {}",
                    self.unknown_patterns().join(", "),
                    PATTERNS
                        .iter()
                        .map(|pattern| pattern.key)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        ];
        for (ok, reason) in checks {
            if !ok {
                return Err(MigrationSafetyError::InvalidVersion {
                    version: "migration_policy".to_owned(),
                    reason,
                });
            }
        }
        Ok(())
    }
}

/// Read the policy row, or the default when this installation has none.
///
/// A missing row is NOT an error: `migration_policy` is seeded by `0207`, but an installation
/// whose binary predates it has no row and needs the defaults to keep applying. Refusing here
/// would make the runner unusable on exactly the installations that most need it.
pub async fn read(pool: &PgPool) -> Result<Policy> {
    let row = sqlx::query_as::<_, (
        bool,
        i32,
        i32,
        serde_json::Value,
        i32,
        i32,
        bool,
        Option<String>,
        Option<OffsetDateTime>,
    )>(
        "select require_down_scripts, lock_timeout_ms, statement_timeout_ms, banned_patterns, \
                backfill_batch_size, backfill_rate_per_second, require_approval_for_destructive, \
                updated_by, updated_at \
         from migration_policy where id = 1",
    )
    .fetch_optional(pool)
    .await
    // An installation whose binary predates 0207 has no table and no row; both mean "the
    // defaults apply", and refusing here would make the runner unusable on exactly the
    // installations that most need it.
    .or_else(|err| if crate::ledger::is_missing_table(&err) { Ok(None) } else { Err(err) })?;

    Ok(match row {
        None => Policy::default_row(),
        Some((
            require_down_scripts,
            lock_timeout_ms,
            statement_timeout_ms,
            banned_patterns,
            backfill_batch_size,
            backfill_rate_per_second,
            require_approval_for_destructive,
            updated_by,
            updated_at,
        )) => {
            // A `banned_patterns` column written by hand as `[]` or `"all"` must not make the
            // policy unreadable: the fallback is the empty map, which means "everything enforced",
            // which is the direction a safety rule should fail in.
            let banned_patterns = banned_patterns
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(key, value)| value.as_bool().map(|enabled| (key.clone(), enabled)))
                        .collect()
                })
                .unwrap_or_default();
            Policy {
                require_down_scripts,
                lock_timeout_ms,
                statement_timeout_ms,
                banned_patterns,
                backfill_batch_size,
                backfill_rate_per_second,
                require_approval_for_destructive,
                updated_by,
                updated_at,
            }
        }
    })
}

/// Save the policy row.
///
/// The `id = 1` singleton is written with an `on conflict do update`, which is what makes this an
/// upsert of the same row rather than a second policy somebody has to reconcile. The guard clause
/// is the safety property: the policy's own bounds are enforced **before** the write, so a rejected
/// value leaves the previous policy in place instead of leaving a policy nobody can run under.
pub async fn save(pool: &PgPool, policy: &Policy) -> Result<()> {
    policy.validate()?;
    sqlx::query(
        "insert into migration_policy \
             (id, require_down_scripts, lock_timeout_ms, statement_timeout_ms, banned_patterns, \
              backfill_batch_size, backfill_rate_per_second, require_approval_for_destructive, \
              updated_by, updated_at) \
         values (1, $1, $2, $3, $4, $5, $6, $7, $8, now()) \
         on conflict (id) do update set \
             require_down_scripts = excluded.require_down_scripts, \
             lock_timeout_ms = excluded.lock_timeout_ms, \
             statement_timeout_ms = excluded.statement_timeout_ms, \
             banned_patterns = excluded.banned_patterns, \
             backfill_batch_size = excluded.backfill_batch_size, \
             backfill_rate_per_second = excluded.backfill_rate_per_second, \
             require_approval_for_destructive = excluded.require_approval_for_destructive, \
             updated_by = excluded.updated_by, \
             updated_at = excluded.updated_at",
    )
    .bind(policy.require_down_scripts)
    .bind(policy.lock_timeout_ms)
    .bind(policy.statement_timeout_ms)
    .bind(serde_json::to_value(&policy.banned_patterns)?)
    .bind(policy.backfill_batch_size)
    .bind(policy.backfill_rate_per_second)
    .bind(policy.require_approval_for_destructive)
    .bind(policy.updated_by.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a waiver for one finding.
///
/// The key is `(version, pattern, line)` — the finding's own identity — so re-running the lint
/// re-detects the finding and carries the waiver forward instead of expiring it. The reason is
/// mandatory in Rust as well as in the table's check constraint: an empty reason is a waiver
/// nobody can audit, and the constraint's error (`migration_violations_waiver_complete`) names a
/// constraint rather than the field.
///
/// Returns the violation id.
pub async fn waive(
    pool: &PgPool,
    version: &str,
    pattern: &str,
    line: i32,
    by: &str,
    reason: &str,
) -> Result<i64> {
    if reason.trim().is_empty() {
        return Err(MigrationSafetyError::PolicyViolation(
            "a waiver needs a reason: it is the only record of why this finding was accepted"
                .to_owned(),
        ));
    }
    let id: i64 = sqlx::query_scalar(
        "insert into migration_violations \
             (version, pattern, severity, line, excerpt, waived_by, waived_at, waiver_reason) \
         values ($1, $2, 'warning', $3, '', $4, now(), $5) \
         on conflict (version, pattern, line) do update set \
             waived_by = excluded.waived_by, \
             waived_at = excluded.waived_at, \
             waiver_reason = excluded.waiver_reason \
         returning id",
    )
    .bind(version)
    .bind(pattern)
    .bind(line)
    .bind(by)
    .bind(reason.trim())
    .fetch_one(pool)
    .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_row_is_the_one_the_migration_seeds() {
        // These numbers are 0207's. Asserted rather than read from the file because the point is
        // that this copy and that file agree — a test that parsed the SQL would pass for both
        // being wrong the same way.
        let policy = Policy::default_row();
        assert!(policy.require_down_scripts);
        assert_eq!(policy.lock_timeout_ms, 5_000);
        assert_eq!(policy.statement_timeout_ms, 300_000);
        assert_eq!(policy.backfill_batch_size, 5_000);
        assert_eq!(policy.backfill_rate_per_second, 200);
        assert!(policy.require_approval_for_destructive);
    }

    #[test]
    fn an_absent_pattern_is_enforced_and_a_disabled_one_is_not() {
        // The direction of the default is a safety decision, so it is pinned in both directions:
        // an empty map enables everything, and only an explicit `false` disables.
        let mut policy = Policy::default_row();
        let enabled = policy.enabled_patterns();
        assert_eq!(enabled.len(), PATTERNS.len());
        assert!(
            enabled.values().all(|value| *value),
            "a fresh installation enforces every rule: {enabled:?}"
        );

        policy
            .banned_patterns
            .insert("drop_table".to_owned(), false);
        let enabled = policy.enabled_patterns();
        assert_eq!(enabled.get("drop_table"), Some(&false));
        assert_eq!(
            enabled.get("drop_column"),
            Some(&true),
            "disabling one rule must not disable its neighbours"
        );
    }

    #[test]
    fn a_pattern_this_binary_does_not_know_is_named_not_silently_ignored() {
        // The dangerous direction: a policy written by a newer binary disables a rule this build
        // has no pattern for, and the lint would then "pass" a drop column with nothing recorded.
        let mut policy = Policy::default_row();
        policy
            .banned_patterns
            .insert("truncate_table".to_owned(), false);
        assert_eq!(policy.unknown_patterns(), vec!["truncate_table"]);
        assert!(
            !policy.enabled_patterns().contains_key("truncate_table"),
            "the lint only ever receives the vocabulary it implements"
        );
        let err = policy.validate().unwrap_err().to_string();
        assert!(
            err.contains("truncate_table"),
            "the refusal names the pattern: {err}"
        );
    }

    #[test]
    fn the_bounds_are_refused_with_the_bound_in_the_message() {
        let mut policy = Policy::default_row();
        policy.lock_timeout_ms = 0;
        let err = policy.validate().unwrap_err().to_string();
        assert!(err.contains("lock_timeout_ms must be 100..=60000"), "{err}");

        policy.lock_timeout_ms = 5_000;
        policy.statement_timeout_ms = 10;
        assert!(
            policy
                .validate()
                .unwrap_err()
                .to_string()
                .contains("statement_timeout_ms must be 1000..=3600000")
        );

        policy.statement_timeout_ms = 300_000;
        policy.backfill_batch_size = 10;
        assert!(
            policy
                .validate()
                .unwrap_err()
                .to_string()
                .contains("backfill_batch_size must be 100..=100000")
        );

        policy.backfill_batch_size = 5_000;
        policy.backfill_rate_per_second = 0;
        assert!(
            policy
                .validate()
                .unwrap_err()
                .to_string()
                .contains("backfill_rate_per_second must be positive")
        );
        policy.backfill_rate_per_second = 200;
        assert!(policy.validate().is_ok(), "the default row is valid");
    }

    #[test]
    fn a_policy_survives_a_round_trip_through_the_jsonb_it_is_stored_in() {
        // The column is `jsonb`, so the screen's shape and the stored shape have to agree. A
        // `bool` is the only value the column is allowed to hold per key; a stored `1` or
        // `"on"` reads back as nothing and silently re-enables the rule.
        let policy = Policy {
            banned_patterns: BTreeMap::from([("drop_table".to_owned(), false)]),
            ..Policy::default_row()
        };
        let json = serde_json::to_value(&policy.banned_patterns).expect("serialises");
        assert_eq!(json["drop_table"], serde_json::json!(false));
        let back: BTreeMap<String, bool> = serde_json::from_value(json).expect("deserialises");
        assert_eq!(back, policy.banned_patterns);
    }
}
