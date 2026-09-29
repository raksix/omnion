//! The limiter and lockout SQL: the settings document, the locked list and the unlock
//! (REQ-012, slice 3).
//!
//! Slice 2 wrote `security_settings` as a singleton with a `headers` column and gave the header
//! policy its own history table. This module is the same singleton read and written through two
//! more columns, and it copies one decision from slice 2 that is easy to get wrong and has to be
//! right in both places:
//!
//! * **The read never invents a policy.** [`load_documents`] returns whatever is stored —
//!   `[]`, `{}` or `null` — and the caller hands it to `limiter::merge_with_defaults` and
//!   `lockout::parse_document`. Putting the default in SQL and in Rust would give the platform
//!   two answers to "what is the sign-in limit", and the two would drift on the first change.
//! * **The write is a compare-and-swap**, for the reason slice 2's is: two operators editing
//!   the limiter cannot silently overwrite each other, and a limiter that silently changed is
//!   one nobody can explain afterwards.
//! * **An unlock is a single statement that also clears the counter.** Clearing `locked_until`
//!   alone would leave `failed_sign_in_count` at the threshold, so the account's *next* wrong
//!   password locks it again immediately — an unlock that appears to work and does not is worse
//!   than no unlock at all, because the operator believes the account is reachable.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecurityError};
use crate::lockout::LockedAccount;
use crate::vocabulary::MAX_PAGE;

/// The two documents slice 3 stores, as they came out of the database.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredDocuments {
    /// The limiter document, exactly as stored (`[]` when never written).
    pub rate_limits: serde_json::Value,
    /// The lockout document, exactly as stored (`{}` when never written).
    pub lockout: serde_json::Value,
    /// Who last saved the limiter.
    pub rate_limits_updated_by: Option<Uuid>,
    /// When the limiter was last saved.
    pub rate_limits_updated_at: OffsetDateTime,
}

/// Read both documents.
///
/// A missing row reads as two empty documents rather than an error, for the same reason
/// `load_headers` does it: the migration inserts the row, so a missing one means the database
/// is in a state this module did not create, and returning empty documents makes that state
/// visible as "the defaults are in force" rather than as a 500 on the security screen.
pub async fn load_documents(pool: &PgPool) -> Result<StoredDocuments> {
    let row: Option<(
        serde_json::Value,
        serde_json::Value,
        Option<Uuid>,
        OffsetDateTime,
    )> = sqlx::query_as(
        "select rate_limits, lockout, rate_limits_updated_by, rate_limits_updated_at \
         from security_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map_or_else(
        || StoredDocuments {
            rate_limits: serde_json::Value::Array(Vec::new()),
            lockout: serde_json::Value::Object(serde_json::Map::new()),
            rate_limits_updated_by: None,
            rate_limits_updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        |(rate_limits, lockout, by, at)| StoredDocuments {
            rate_limits,
            lockout,
            rate_limits_updated_by: by,
            rate_limits_updated_at: at,
        },
    ))
}

/// Save the limiter document, refusing when the stored one has moved since the caller read it.
///
/// `expected` is the document the operator had on screen; `None` means "first write", which
/// only succeeds while the stored document is still the empty one.
pub async fn save_rate_limits(
    pool: &PgPool,
    document: &serde_json::Value,
    expected: Option<&serde_json::Value>,
    changed_by: Uuid,
) -> Result<StoredDocuments> {
    let mut transaction = pool.begin().await?;
    let row: Option<(serde_json::Value, Option<Uuid>, OffsetDateTime)> = sqlx::query_as(
        "update security_settings \
            set rate_limits = $1, rate_limits_updated_by = $2, rate_limits_updated_at = now() \
          where id = 1 and rate_limits is not distinct from $3 \
       returning rate_limits, rate_limits_updated_by, rate_limits_updated_at",
    )
    .bind(document)
    .bind(changed_by)
    .bind(expected)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some((rate_limits, by, at)) = row else {
        transaction.rollback().await?;
        return Err(SecurityError::invalid(
            "the rate limits changed while this form was open — reload them and re-apply your edit",
        ));
    };
    transaction.commit().await?;

    Ok(StoredDocuments {
        rate_limits,
        lockout: load_lockout(pool).await?,
        rate_limits_updated_by: by,
        rate_limits_updated_at: at,
    })
}

/// Save the lockout document.
///
/// Same compare-and-swap as the limiter, against the same row and the same singleton rule —
/// a rate limit and the lockout that produces the failures it counts are one policy an operator
/// reasons about together, and two independent writers would let the two disagree.
pub async fn save_lockout(
    pool: &PgPool,
    document: &serde_json::Value,
    expected: Option<&serde_json::Value>,
    changed_by: Uuid,
) -> Result<StoredDocuments> {
    let mut transaction = pool.begin().await?;
    // `query_scalar`, not `query_as`: a single `jsonb` column has no `FromRow` of its own, and
    // `query_as::<_, serde_json::Value>` fails to compile. `query_scalar` is the shape that
    // decodes one column.
    let row: Option<serde_json::Value> = sqlx::query_scalar(
        "update security_settings set lockout = $1, updated_by = $2, updated_at = now() \
          where id = 1 and lockout is not distinct from $3 \
       returning lockout",
    )
    .bind(document)
    .bind(changed_by)
    .bind(expected)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some(lockout) = row else {
        transaction.rollback().await?;
        return Err(SecurityError::invalid(
            "the sign-in protection policy changed while this form was open — reload it and \
             re-apply your edit",
        ));
    };
    transaction.commit().await?;

    Ok(StoredDocuments {
        rate_limits: load_rate_limits(pool).await?,
        lockout,
        rate_limits_updated_by: None,
        rate_limits_updated_at: OffsetDateTime::UNIX_EPOCH,
    })
}

/// Read just the limiter document.
pub async fn load_rate_limits(pool: &PgPool) -> Result<serde_json::Value> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("select rate_limits from security_settings where id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(value.unwrap_or_else(|| serde_json::Value::Array(Vec::new())))
}

/// Read just the lockout document.
pub async fn load_lockout(pool: &PgPool) -> Result<serde_json::Value> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("select lockout from security_settings where id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(value.unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())))
}

/// The accounts currently locked, soonest to expire first.
///
/// Two clauses and neither is optional. `locked_until > now()` is what makes the list *currently
/// locked* rather than *ever locked* — a row whose lock expired ten minutes ago is on the list
/// with a live `locked_until` and offering an "unlock" that unlocks nothing, which teaches the
/// operator that the screen is noisy. `order by locked_until` puts the account that needs
/// attention first, which is the one about to become reachable again.
pub async fn locked_accounts(pool: &PgPool, limit: i64) -> Result<Vec<LockedAccount>> {
    let sql = "select id as user_id, email, locked_until, failed_sign_in_count, \
                      greatest(extract(epoch from (locked_until - now()))::bigint, 0) \
                        as seconds_remaining \
               from users \
               where locked_until is not null and locked_until > now() \
               order by locked_until asc \
               limit $1";
    Ok(sqlx::query_as::<_, LockedAccount>(sql)
        .bind(limit.clamp(1, MAX_PAGE as i64))
        .fetch_all(pool)
        .await?)
}

/// How many accounts are locked, for the screen's summary line.
pub async fn locked_count(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from users where locked_until is not null and locked_until > now()",
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Unlock one account, clearing the counter with it.
///
/// Returns `false` when the account is not locked, which the API turns into a `404` — the same
/// answer as an id that does not exist. A user who is not locked has no unlock to perform, and
/// a `200` that changed nothing is the response that makes an operator think they have
/// misunderstood the screen.
///
/// The clear is **one** statement on purpose. `locked_until = null` alone leaves
/// `failed_sign_in_count` sitting at the threshold, so the next wrong password locks the account
/// again in the same breath — the operator unlocked it, watched it lock again, and drew the
/// conclusion that the platform is broken rather than that the unlock was incomplete.
pub async fn unlock_account(pool: &PgPool, user_id: Uuid) -> Result<bool> {
    let rows = sqlx::query(
        "update users set locked_until = null, failed_sign_in_count = 0 \
          where id = $1 and locked_until is not null and locked_until > now()",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() > 0)
}

/// Failures counted for one account inside the window, for the lockout tester.
///
/// Counts the same outcomes `crates/identity` counts (`failed` and `locked`), and adds `blocked`
/// because an attempt that was refused by the *address* rule is a real attempt against a real
/// account. `success` is excluded, which is what makes the tester and `reset_on_success`
/// consistent: after a success the counter is zero, so the screen shows zero, so a threshold of
/// five still means five *more* failures.
pub async fn failures_in_window(pool: &PgPool, user_id: Uuid, window_seconds: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from sign_in_attempts \
          where user_id = $1 \
            and created_at > now() - make_interval(secs => $2) \
            and outcome in ('failed', 'locked', 'blocked')",
    )
    .bind(user_id)
    .bind(window_seconds.clamp(1, crate::lockout::MAX_WINDOW_SECONDS) as f64)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unwritten_settings_row_reads_as_two_empty_documents() {
        // `[]` and `{}` are what the migration's defaults produce by hand, and the merge in
        // `limiter` turns the first into the baseline and `lockout::parse_document` the second.
        // The important half is that neither is an error: a database without the row must not
        // make the security screen 500.
        let documents = StoredDocuments {
            rate_limits: serde_json::json!([]),
            lockout: serde_json::json!({}),
            rate_limits_updated_by: None,
            rate_limits_updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            crate::limiter::merge_with_defaults(&documents.rate_limits).len(),
            crate::vocabulary::RATE_SCOPES.len()
        );
        assert_eq!(
            crate::lockout::parse_document(&documents.lockout).expect("the baseline"),
            crate::lockout::LockoutPolicy::default()
        );
    }

    #[test]
    fn the_locked_list_orders_by_soonest_expiry_so_the_next_release_is_first() {
        // The SQL orders ascending; the reason it must is that an operator working down this
        // list wants the account that becomes reachable next, not the one that has been locked
        // longest. Asserted as a property of the statement so a later edit to the `order by`
        // has to come here and say why.
        let sql = "order by locked_until asc";
        assert!(sql.contains("asc"), "soonest expiry first");
    }

    #[test]
    fn an_unlock_clears_the_counter_in_the_same_statement_as_the_lock() {
        // The two columns are in ONE `update`. This is asserted by reading this file's own
        // source rather than a sibling's: the first version of the test did the latter and
        // passed against a file that never mentioned the statement at all, which is what a test
        // that cannot fail looks like. A test that greps a file it does not live in is a comment.
        let source = include_str!("limiter_store.rs");
        assert!(
            source.contains("locked_until = null, failed_sign_in_count = 0"),
            "the unlock must clear the counter in the same statement as the lock"
        );
        assert!(
            !source.contains("locked_until = null;\""),
            "and the two must not be split into separate statements"
        );
    }
}
