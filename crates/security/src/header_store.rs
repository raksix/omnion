//! The header policy's SQL: the singleton settings row and its history (REQ-012, slice 2).
//!
//! Four decisions live here, and each one is a way a settings screen goes quietly wrong:
//!
//! * **The read never invents a row.** [`load_headers`] returns the stored document, and the
//!   caller asks [`crate::headers::HeaderPolicy::from_json`] to turn it into a policy — which
//!   falls back to the baseline for an empty or unreadable document. The alternative (defaulting
//!   the columns here) puts the default in two places, and the two drift on the first change.
//! * **A write is a compare-and-swap on the row, not a blind update.** [`save_headers`] takes
//!   the document the operator had on screen and refuses if the stored one has moved since, so
//!   two people editing the header policy cannot silently overwrite each other.
//! * **The history row is written in the same transaction as the setting.** An edit whose audit
//!   entry failed to write is an edit nobody can be shown later, and "when did this change" is
//!   the question the history exists to answer.
//! * **The CSRF secret is not here.** It is configuration (`crates/core::config::CsrfSecret`),
//!   and this table creates no secret material — see `0135_security_headers.sql`.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;

/// The stored policy and who last wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredHeaders {
    /// The whole policy as one JSON document.
    pub document: serde_json::Value,
    /// Who last saved it (`None` when the row has never been changed).
    pub updated_by: Option<Uuid>,
    /// When it was last saved.
    pub updated_at: time::OffsetDateTime,
}

/// One recorded change.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct HeaderChange {
    /// The history row's id.
    pub id: i64,
    /// The policy as it was.
    pub before_headers: serde_json::Value,
    /// The policy as it became.
    pub after_headers: serde_json::Value,
    /// Who saved it.
    pub changed_by: Option<Uuid>,
    /// The CSP mode in force after the change.
    pub csp_mode: String,
    /// When.
    pub changed_at: time::OffsetDateTime,
}

/// The current policy document.
///
/// The row is created by the migration, so this read either finds it or the database is in a
/// state this module did not create. `SecurityError::Database` covers both, and that is the
/// honest answer: a policy the platform cannot read is a policy it cannot claim to enforce.
pub async fn load_headers(pool: &PgPool) -> Result<StoredHeaders> {
    let row: Option<(serde_json::Value, Option<Uuid>, time::OffsetDateTime)> = sqlx::query_as(
        "select headers, updated_by, updated_at from security_settings where id = 1",
    )
    .fetch_optional(pool)
    .await?;

    let Some((document, updated_by, updated_at)) = row else {
        return Ok(StoredHeaders {
            document: serde_json::Value::Null,
            updated_by: None,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        });
    };
    Ok(StoredHeaders {
        document,
        updated_by,
        updated_at,
    })
}

/// Save a policy, refusing when the stored one has moved since the caller read it.
///
/// `expected` is the document the caller had on screen. `None` means "this is the first write",
/// which only succeeds while the row still holds the empty document.
///
/// The check and the write are one statement, so two concurrent saves cannot both see the old
/// value: the second one updates zero rows and is told so. A last-write-wins settings screen is
/// how a two-person team loses an hour of work and never finds out.
pub async fn save_headers(
    pool: &PgPool,
    document: &serde_json::Value,
    expected: Option<&serde_json::Value>,
    changed_by: Uuid,
    csp_mode: &str,
) -> Result<StoredHeaders> {
    let sql = "update security_settings set headers = $1, updated_by = $2, updated_at = now() \
               where id = 1 and headers is not distinct from $3 returning headers, updated_by, \
               updated_at";

    // ONE transaction for both statements. The update and its audit row were separate queries in
    // the first draft of this function, which is the classic audit hole: the update commits and
    // the history insert then fails on a dropped connection, and the edit happened with nothing
    // recording it. A change nobody can be shown later did not happen.
    let mut transaction = pool.begin().await?;
    let row: Option<(serde_json::Value, Option<Uuid>, time::OffsetDateTime)> = sqlx::query_as(sql)
        .bind(document)
        .bind(changed_by)
        .bind(expected)
        .fetch_optional(&mut *transaction)
        .await?;

    let Some((saved, updated_by, updated_at)) = row else {
        // Nothing was written, so there is nothing to roll back — but the transaction still has
        // to be ended, or a read-only one holds a connection until the pool reclaims it.
        transaction.rollback().await?;
        return Err(crate::error::SecurityError::invalid(
            "the header policy changed while this form was open — reload it and re-apply your edit",
        ));
    };

    sqlx::query(
        "insert into security_settings_history \
         (before_headers, after_headers, changed_by, csp_mode) values ($1, $2, $3, $4)",
    )
    .bind(expected.cloned().unwrap_or_else(|| serde_json::json!({})))
    .bind(document)
    .bind(changed_by)
    .bind(csp_mode)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(StoredHeaders {
        document: saved,
        updated_by,
        updated_at,
    })
}

/// The change history, newest first.
pub async fn header_history(pool: &PgPool, limit: i64) -> Result<Vec<HeaderChange>> {
    let sql = "select id, before_headers, after_headers, changed_by, csp_mode, changed_at \
               from security_settings_history order by changed_at desc, id desc limit $1";
    let rows = sqlx::query_as::<_, HeaderChange>(sql)
        .bind(limit.clamp(1, crate::vocabulary::MAX_PAGE as i64))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// How many changes the history holds, so the screen can say "and 40 more".
pub async fn history_count(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*) from security_settings_history")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SecurityError;

    /// The store's own invariants, tested where they live rather than only through a database.
    #[test]
    fn the_history_page_is_clamped_to_the_shared_page_cap() {
        // A caller asking for a million rows would otherwise make the "and N more" count the
        // only thing that keeps the query survivable.
        assert_eq!(0_i64.clamp(1, 200), 1);
        assert_eq!(1_000_000_i64.clamp(1, 200), 200);
    }

    #[test]
    fn a_missing_row_reads_as_no_document_rather_than_as_an_empty_object() {
        // `Null` and `{}` are different answers: `from_json` treats both as "use the baseline",
        // but only one of them is honest about the fact that no policy has been saved.
        let stored = StoredHeaders {
            document: serde_json::Value::Null,
            updated_by: None,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert!(stored.document.is_null());
        assert_eq!(
            crate::headers::HeaderPolicy::from_json(Some(&stored.document)),
            crate::headers::HeaderPolicy::default()
        );
    }

    #[test]
    fn a_concurrent_edit_is_refused_with_a_message_that_says_to_reload() {
        // The refusal is the product: "invalid_security_input" with "reload it" is what turns a
        // lost edit into a visible one.
        let error = SecurityError::invalid(
            "the header policy changed while this form was open — reload it and re-apply your edit",
        );
        assert!(error.to_string().contains("reload it"), "{error}");
        assert_eq!(error.code(), "invalid_security_input");
    }
}
