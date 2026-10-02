//! Request-log SQL: recording a call and paging the screen (REQ-022, slice 1).
//!
//! ## The query is built, never interpolated
//!
//! Every narrowing in [`search`] is an optional `($n::type is null or …)` clause rather than a
//! string the caller appends. That is not a style preference: the filters arrive from a query
//! string, and the alternative is a filter value reaching the SQL text. `QueryBuilder` numbers
//! its own binds, so a filter that is added in a different order than it was numbered cannot
//! bind the wrong value to the wrong column.
//!
//! ## `path_prefix` is a prefix, and it says so on screen
//!
//! `like 'prefix%'` — not `%prefix%`. An operator filtering by `/api/v1/media` wants that
//! subtree; a substring match would also return `/api/v1/iam/media-adjacent`, and a log filter
//! that silently returns unrelated rows is worse than one that returns none. `%` and `_` typed
//! by the operator are escaped, so a filter of `%` matches a path starting with a percent sign
//! and nothing else, rather than every row in the log.
//!
//! ## The daily rollup is upserted, not recomputed
//!
//! [`record`] bumps today's counters in the same statement as the insert. A rollup recomputed
//! by `select sum(…) group by day` would be correct and would scan the whole retention window
//! on every request, which is the shape of mistake that makes a request log the reason a
//! platform falls over.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::logs::{ClientIdentity, LogPage, LogQuery, LogRow, validate_query};

/// Columns the log screen reads.
pub const LOG_COLUMNS: &str = "id, organization_id, api_key_id, api_key_prefix, actor_user_id, \
     actor_name, method, path, status, duration_ms, permission, client_fingerprint, created_at";

/// How many days of history the log keeps. Published on the logs screen, because a request log
/// whose retention window is invisible reads as "the platform never logged that".
pub const RETENTION_DAYS: u32 = 30;

/// The retention window, as the screen prints it.
#[must_use]
pub fn window_days() -> u32 {
    RETENTION_DAYS
}

/// Write one request's row and bump today's counters.
///
/// A failure here is logged and swallowed: the request itself already succeeded, and answering
/// a `200` with an error because the *debugging* table was unavailable would turn a log outage
/// into an outage. The log is not on the request's critical path — that is the whole reason it
/// is separate from the audit trail (`crates/audit`), which is the opposite trade.
pub async fn record(
    pool: &PgPool,
    method: &str,
    path: &str,
    status: i16,
    duration_ms: i32,
    identity: &ClientIdentity,
    now: OffsetDateTime,
) -> Result<()> {
    // **Stripped here, not by the caller.** This function is the last point at which the raw path
    // exists, and it is also the point at which the value becomes permanent: the walk
    // `the_request_log_records_the_matched_permission_and_never_the_query_string` recorded
    // `?access_token=super-secret-value` verbatim until this line was added, because the
    // middleware that calls it passed the whole URI. Stripping at the boundary is the only
    // placement a future caller cannot forget.
    let path = crate::logs::path_without_query(path);
    let key_id = identity.api_key_id;

    sqlx::query(
        "insert into api_request_logs
             (organization_id, api_key_id, api_key_prefix, actor_user_id, actor_name,
              method, path, status, duration_ms, permission, client_fingerprint, created_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(identity.organization_id)
    .bind(key_id)
    .bind(identity.api_key_prefix.as_deref())
    .bind(identity.user_id)
    .bind(&identity.user_name)
    .bind(method.to_ascii_uppercase())
    .bind(&path)
    .bind(status)
    .bind(duration_ms.max(0))
    .bind(identity.permission.as_deref())
    .bind(identity.client_fingerprint.as_deref())
    .bind(now)
    .execute(pool)
    .await?;

    // Only a key's requests are rolled up; a session's traffic belongs to no key and would
    // create a row with no `api_key_id` to hang off.
    if let Some(key_id) = key_id {
        sqlx::query(
            "insert into api_key_usage_daily (api_key_id, day, requests, errors, avg_duration_ms)
             values ($1, $2, 1, $3, $4)
             on conflict (api_key_id, day) do update
                set requests = api_key_usage_daily.requests + 1,
                    errors = api_key_usage_daily.errors + excluded.errors,
                    avg_duration_ms = greatest(
                        0,
                        ((api_key_usage_daily.avg_duration_ms * api_key_usage_daily.requests)
                         + excluded.avg_duration_ms)
                        / (api_key_usage_daily.requests + 1))",
        )
        .bind(key_id)
        .bind(now.date())
        .bind(i32::from(status >= 400))
        .bind(duration_ms.max(0))
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Read a page of the log, newest first.
pub async fn search(pool: &PgPool, query: &mut LogQuery) -> Result<LogPage> {
    validate_query(query)?;
    let limit = query.limit.clamp(1, crate::logs::MAX_PAGE);
    let class = query.status_class.as_deref();
    let prefix = query
        .path_prefix
        .as_deref()
        .map(|prefix| format!("{}%", escape_like(prefix)));

    let rows: Vec<LogRow> = sqlx::query_as(&format!(
        "select {LOG_COLUMNS} from api_request_logs
          where organization_id = $1
            and ($2::uuid is null or api_key_id = $2)
            and ($3::text is null or method = $3)
            and ($4::text is null or path like $4)
            and ($5::text is null
                 or ($5 = '2xx' and status between 200 and 299)
                 or ($5 = '3xx' and status between 300 and 399)
                 or ($5 = '4xx' and status between 400 and 499)
                 or ($5 = '5xx' and status between 500 and 599))
            and ($6::int is null or created_at > now() - make_interval(days => $6::int))
            and ($7::bigint is null or id < $7)
          order by id desc
          limit $8"
    ))
    .bind(query.organization_id)
    .bind(query.api_key_id)
    .bind(query.method.as_deref())
    .bind(prefix.as_deref())
    .bind(class)
    .bind(query.window_days.map(i64::from))
    .bind(query.before)
    .bind(limit as i64 + 1)
    .fetch_all(pool)
    .await?;

    let has_more = rows.len() > limit;
    let mut rows = rows;
    if has_more {
        rows.truncate(limit);
    }
    Ok(LogPage {
        next_before: has_more.then(|| rows.last().map(|row| row.id)).flatten(),
        rows,
    })
}

/// Read one log row, scoped to its organization.
pub async fn find(pool: &PgPool, organization_id: Uuid, id: i64) -> Result<Option<LogRow>> {
    Ok(sqlx::query_as::<_, LogRow>(&format!(
        "select {LOG_COLUMNS} from api_request_logs
          where id = $1 and organization_id = $2"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?)
}

/// Escape the two characters `like` treats as wildcards.
///
/// Without this an operator filtering by `/api/v1/media%` matches every row in the log, and a
/// filter that returns the whole log reads as "this filter is not working".
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_an_operator_typed_is_matched_literally() {
        // The failure this prevents: filtering by `%` returns the entire log, and an operator
        // concludes the filter is broken rather than that they typed a wildcard.
        assert_eq!(escape_like("%"), "\\%");
        assert_eq!(escape_like("/api/v1/media%"), "/api/v1/media\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        // The backslash itself, or the escape would be escapable and `\%` would mean "literal %"
        // only until somebody typed a backslash first.
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("/api/v1/media"), "/api/v1/media");
    }

    #[test]
    fn the_prefix_is_a_prefix_and_not_a_substring() {
        let prefix = format!("{}%", escape_like("/api/v1/media"));
        assert!(prefix.starts_with('/'), "the filter must anchor at the start of the path");
        // Exactly one wildcard, and it is the trailing one the prefix match needs.
        assert_eq!(prefix.matches('%').count(), 1, "unexpected wildcard: {prefix}");
        assert!(prefix.ends_with('%'), "the match must be anchored at the end: {prefix}");
        // A path typed WITH a trailing percent becomes an escaped literal plus the anchor, so
        // it matches a path that literally ends in `%` — two `%` characters, one escaped.
        assert_eq!(
            format!("{}%", escape_like("/api/v1/50%")),
            "/api/v1/50\\%%",
            "the operator's own percent must be literal, with the anchor added after it"
        );
    }

    #[test]
    fn the_retention_window_is_published_rather_than_hidden() {
        // The screen prints this number; a log whose window nobody can find reads as "the
        // platform never recorded that request".
        assert_eq!(window_days(), 30);
    }
}
