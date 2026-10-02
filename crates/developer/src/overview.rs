//! The `/developer` overview's numbers (REQ-022, slice 2).
//!
//! ## Why this is a module and not three queries in the route
//!
//! The overview answers "is anything actually being called?" and the whole card row is read as
//! one moment in time: a key count read before a request and a request count read after it is
//! not a card row, it is a coincidence. So all five numbers come from **one statement**, and the
//! route has no arithmetic of its own to get wrong.
//!
//! ## The counters use the log screen's own rules
//!
//! [`crate::logs::class_of`] decides what "an error" is, and the log's own retention window
//! decides how far back "today" can reach. A count computed by a different rule than the one the
//! operator filters with is a card that disagrees with the table beside it, and the table wins
//! every argument.
//!
//! ## Midnight is computed here, in Rust, and bound — never in SQL
//!
//! `date_trunc('day', now() at time zone 'UTC') at time zone 'UTC'` is correct and untestable:
//! it depends on the *database's* clock and its session time zone, so the number on the card is
//! not the number [`start_of_day`] would produce and there is no unit test that can compare them.
//! [`start_of_day`] is a pure function over the same instant the route already read, so "today"
//! is the boundary the caller chose rather than one the server picked behind its back.
//!
//! ## `requests_today` counts rows, not rollups
//!
//! The per-key rollup (`api_key_usage_daily`) is written by the same recorder and is the right
//! answer for *one key* over *many days*. The overview asks about a whole organization for
//! *today*, and that is one indexed range scan on `(organization_id, created_at desc)`.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;
use crate::logs::{class_of, path_without_query};

/// The numbers the overview screen draws.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Overview {
    /// Keys that can still authenticate.
    pub active_keys: i64,
    /// Keys that stopped because of an expiry.
    pub expired_keys: i64,
    /// Keys an operator revoked.
    pub revoked_keys: i64,
    /// Requests since midnight, in UTC.
    pub requests_today: i64,
    /// Of those, how many were a refusal.
    pub errors_today: i64,
    /// The most recent refusals, newest first. Never more than [`RECENT_FAILURES`].
    pub recent_failures: Vec<FailureLine>,
}

/// One line of the "what went wrong" list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailureLine {
    /// Which request.
    pub id: i64,
    /// When.
    pub created_at: OffsetDateTime,
    /// `GET`, `POST`, …
    pub method: String,
    /// The path, query already stripped by the recorder.
    pub path: String,
    /// The status, and therefore the class.
    pub status: i16,
    /// The key that made it, by prefix. `None` for a session.
    pub key_prefix: Option<String>,
    /// The scope the guard resolved, when one did.
    pub permission: Option<String>,
}

impl FailureLine {
    /// The class the log screen's filter names.
    #[must_use]
    pub fn status_class(&self) -> &'static str {
        class_of(self.status)
    }
}

/// How many failure lines the overview shows.
pub const RECENT_FAILURES: usize = 8;

/// The first instant of `now`'s UTC day.
///
/// Midnight UTC rather than a tenant's local midnight: the log's own `window_days` filter counts
/// back 24-hour periods from the same clock, and a card using one boundary beside a table using
/// the other is a discrepancy an operator cannot reason about. It is a **pure** function, which
/// is the point — the alternative (`date_trunc` in SQL) is correct and untestable.
///
/// # Panics
///
/// Never: `Date::midnight` is a total function, and `assume_utc` cannot fail for a UTC date.
#[must_use]
pub fn start_of_day(now: OffsetDateTime) -> OffsetDateTime {
    let date: Date = now.date();
    date.midnight().assume_utc()
}

/// Read the overview for one organization.
///
/// `now` is passed in rather than read here so the caller hands the same instant to the key list
/// and the log page; a status computed against a second clock read is how "0 active keys"
/// appears on a screen that is showing one.
pub async fn read(pool: &PgPool, organization_id: Uuid, now: OffsetDateTime) -> Result<Overview> {
    // One statement, one snapshot, five answers. Four separate `select count(*)`s would each
    // read their own snapshot, and a key created between two of them makes the "N keys" card
    // disagree with the table it sits above with no way for the operator to tell which is lying.
    let counted: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "select
             (select count(*) from api_keys
               where organization_id = $1 and revoked_at is null
                 and (expires_at is null or expires_at > $3)),
             (select count(*) from api_keys
               where organization_id = $1 and revoked_at is null
                 and expires_at is not null and expires_at <= $3),
             (select count(*) from api_keys
               where organization_id = $1 and revoked_at is not null),
             (select count(*) from api_request_logs
               where organization_id = $1 and created_at >= $2),
             (select count(*) from api_request_logs
               where organization_id = $1 and created_at >= $2 and status >= 400)",
    )
    .bind(organization_id)
    .bind(start_of_day(now))
    .bind(now)
    .fetch_one(pool)
    .await?;

    let (active_keys, expired_keys, revoked_keys, requests_today, errors_today) = counted;

    let rows: Vec<(i64, OffsetDateTime, String, String, i16, Option<String>, Option<String>)> =
        sqlx::query_as(
            "select id, created_at, method, path, status, api_key_prefix, permission
               from api_request_logs
              where organization_id = $1 and status >= 400
              order by id desc
              limit $2",
        )
        .bind(organization_id)
        .bind(RECENT_FAILURES as i64)
        .fetch_all(pool)
        .await?;

    let recent_failures = rows
        .into_iter()
        .map(|(id, created_at, method, path, status, key_prefix, permission)| FailureLine {
            id,
            created_at,
            method,
            // Re-stripped rather than trusted: the recorder strips the column on the way in,
            // and a path that arrived another way would otherwise be the one row on this screen
            // carrying a query string — which is where a `?token=` would have survived the whole
            // hygiene design and then been rendered on a screen.
            path: path_without_query(&path),
            status,
            key_prefix,
            permission,
        })
        .collect();

    Ok(Overview {
        active_keys,
        expired_keys,
        revoked_keys,
        requests_today,
        errors_today,
        recent_failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_failure_line_agrees_with_the_filter_that_would_fetch_it() {
        // The screen badges this row with `status_class()` and the toolbar filters with the same
        // four names, so a row that badges `4xx` while the filter that would fetch it answers
        // `5xx` is a screen that cannot be trusted to page through itself.
        for (status, class) in [
            (200, "2xx"),
            (204, "2xx"),
            (301, "3xx"),
            (404, "4xx"),
            (422, "4xx"),
            (500, "5xx"),
            (503, "5xx"),
        ] {
            let line = FailureLine {
                id: 1,
                created_at: OffsetDateTime::UNIX_EPOCH,
                method: "GET".into(),
                path: "/api/v1/developer/logs".into(),
                status,
                key_prefix: None,
                permission: None,
            };
            assert_eq!(line.status_class(), class, "status {status}");
        }
    }

    #[test]
    fn the_day_boundary_is_the_start_of_the_callers_own_day() {
        // 23:59:59 and the following 00:00:00 are the pair the boundary is wrong on, and they
        // are the pair a request at midnight belongs to. The literal shape (space separator,
        // `UTC` suffix) is this crate's existing convention — `keys.rs` writes
        // `datetime!(2026-10-02 12:00 UTC)`, and the `T`/`Z` ISO form fails to parse here with
        // `invalid component: day was 02T00`, which reads as a malformed literal rather than as
        // a syntax the macro does not take.
        let last = datetime!(2026-10-02 23:59:59.999 UTC);
        let first = datetime!(2026-10-03 00:00:00 UTC);
        assert_eq!(start_of_day(last), datetime!(2026-10-02 00:00:00 UTC));
        assert_eq!(start_of_day(first), datetime!(2026-10-03 00:00:00 UTC));
        // "Today" must include the very start of the day: a request at 00:00:00.000 is today's,
        // and `>` instead of `>=` would drop it onto yesterday's card.
        assert!(first >= start_of_day(first));
        // And must exclude the last instant of the previous day, which is what makes a request
        // made at 23:59 yesterday's rather than today's.
        assert!(last < start_of_day(first));
    }

    #[test]
    fn the_boundary_is_independent_of_the_time_of_day_it_is_asked_at() {
        // A boundary that moved with the instant would make "requests today" a function of when
        // the screen was opened — the number would tick backwards at midnight instead of
        // restarting, and two operators comparing screens would see two different answers.
        let morning = start_of_day(datetime!(2026-10-02 00:00:01 UTC));
        let evening = start_of_day(datetime!(2026-10-02 23:59:59 UTC));
        assert_eq!(morning, evening);
    }

    #[test]
    fn the_failure_list_is_bounded() {
        // An unbounded "recent" list turns a glanceable card into a page that grows until it
        // needs a scrollbar, and the overview's job is to be read at a glance.
        assert!(RECENT_FAILURES <= 10);
    }

    #[test]
    fn a_failure_path_never_carries_its_query_string() {
        // The same rule the recorder applies on the way in, applied on the way out — because the
        // only thing this screen renders from a row is the path, and `?access_token=` in it
        // would defeat the whole hygiene design at the last possible step.
        assert_eq!(
            path_without_query("/api/v1/media?access_token=super-secret"),
            "/api/v1/media"
        );
    }
}
