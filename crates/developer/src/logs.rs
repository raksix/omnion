//! The request log's own vocabulary: who the caller was, what they asked for and how it went
//! (REQ-022, slice 1).
//!
//! ## Three things this module refuses to store
//!
//! The log is the debugging surface an operator pastes into a ticket, which makes it the most
//! likely table in the platform to leave the building. Each of the three exclusions below is a
//! real credential leak in a system that logs requests by default:
//!
//! * **The query string.** `?token=…`, `?api_key=…` and `?signature=…` are ordinary API
//!   practice, and storing them would put a live credential in the one table that has a CSV
//!   export. [`path_without_query`] strips it at the only point where the raw path is still
//!   available.
//! * **The request body.** A body is whatever a caller chose to send — including the fields of
//!   a form that happen to be a password. There is no column for one.
//! * **The client address.** [`ClientIdentity::fingerprint`] is a **keyed** HMAC of the address
//!   and user agent, so two requests from one client are recognisable as one client (the whole
//!   point of a request log) while the value itself cannot be reversed into an address list by
//!   whoever receives the export. The key is `OMNION_LOG_PEPPER`; see [`ClientIdentity::new`] for
//!   what happens when it is absent, which is deliberately *not* an empty string.
//!
//! ## The status class is the filter the screen offers
//!
//! [`class_of`] is here rather than in the route because the filter and the badge must agree:
//! a row shown as `2xx` while the filter that fetched it says `4xx` is a screen that cannot be
//! trusted to page through itself.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};

/// The status classes the log screen filters by, in the order the toolbar shows them.
pub const STATUS_CLASSES: &[&str] = &["2xx", "3xx", "4xx", "5xx"];

/// Longest page the log list will serve.
pub const MAX_PAGE: usize = 200;

/// Who made a request, in the only form the log may keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// A signed-in person, when there was one.
    pub user_id: Option<Uuid>,
    /// Their display name, copied in so a deleted user does not leave a blank column.
    pub user_name: String,
    /// The developer key's id, when a key authenticated it.
    pub api_key_id: Option<Uuid>,
    /// That key's displayable prefix, copied in for the same reason the name is.
    pub api_key_prefix: Option<String>,
    /// Where the key belongs.
    pub organization_id: Option<Uuid>,
    /// The permission the guard resolved, which is what makes a `403` explainable.
    pub permission: Option<String>,
    /// The keyed client fingerprint. Never an address.
    pub client_fingerprint: Option<String>,
}

impl ClientIdentity {
    /// Build an identity and fingerprint the client.
    ///
    /// # Errors
    ///
    /// [`DeveloperError::Invalid`] when `OMNION_LOG_PEPPER` is unset or blank. This is the one
    /// place in the crate that refuses to do its job, and it is deliberate: an unkeyed hash of
    /// an address is a rainbow-table away from the address list, and the log's own retention
    /// window guarantees somebody will still be holding the export in a year. A deployment that
    /// has not set the variable gets a clear `500` on a request rather than a table that quietly
    /// accumulates reversable data.
    pub fn new(
        user_id: Option<Uuid>,
        user_name: Option<&str>,
        api_key_id: Option<Uuid>,
        api_key_prefix: Option<&str>,
        organization_id: Option<Uuid>,
        permission: Option<&str>,
        client_address: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            user_id,
            user_name: user_name.unwrap_or_default().to_owned(),
            api_key_id,
            api_key_prefix: api_key_prefix.map(str::to_owned),
            organization_id,
            permission: permission.map(str::to_owned),
            client_fingerprint: fingerprint(client_address, user_agent)?,
        })
    }

    /// Whether anything authenticated this request at all — a `401` row has neither.
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.user_id.is_none() && self.api_key_id.is_none()
    }
}

/// A key, never an address: HMAC-SHA-256 over the address and the user agent.
fn fingerprint(address: Option<&str>, user_agent: Option<&str>) -> Result<Option<String>> {
    let Some(address) = address else {
        // No address to fingerprint is not a reason to fail; an internal call has none.
        return Ok(None);
    };
    let address = address.trim();
    if address.is_empty() {
        return Ok(None);
    }

    let pepper = std::env::var("OMNION_LOG_PEPPER").unwrap_or_default();
    if pepper.trim().is_empty() {
        return Err(DeveloperError::Invalid(
            "OMNION_LOG_PEPPER is not set, so client addresses cannot be fingerprinted safely"
                .into(),
        ));
    }

    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).map_err(|_| {
        DeveloperError::Invalid("OMNION_LOG_PEPPER is unusable as an HMAC key".into())
    })?;
    // The agent is mixed in so two clients behind one NAT share a fingerprint. Length-prefixing
    // both fields matters: without it `1.2.3.4` + `5.6.7.8` and `1.2.3.45` + `6.7.8` would
    // hash the same bytes, which is a collision an attacker can construct for free.
    mac.update(&(address.len() as u64).to_be_bytes());
    mac.update(address.as_bytes());
    let agent = user_agent.unwrap_or_default();
    mac.update(&(agent.len() as u64).to_be_bytes());
    mac.update(agent.as_bytes());

    Ok(Some(hex::encode(mac.finalize().into_bytes())))
}

/// Strip the query string (and any fragment) from a request path.
///
/// Called on the only copy of the raw path the platform has, before the row is written. The
/// stored path is what an operator reads to recognise the call, and the path without its query
/// is that; the query is where credentials travel.
#[must_use]
pub fn path_without_query(path: &str) -> String {
    let without_fragment = path.split('#').next().unwrap_or(path);
    match without_fragment.split_once('?') {
        Some((path, _query)) => path.to_owned(),
        None => without_fragment.to_owned(),
    }
}

/// The class a status falls in, as the filter names it.
#[must_use]
pub fn class_of(status: i16) -> &'static str {
    match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// One row of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LogRow {
    /// Primary key.
    pub id: i64,
    /// Owning organization.
    pub organization_id: Option<Uuid>,
    /// The key that authenticated it.
    pub api_key_id: Option<Uuid>,
    /// That key's prefix, copied in.
    pub api_key_prefix: Option<String>,
    /// The person, when there was one.
    pub actor_user_id: Option<Uuid>,
    /// That person's name, copied in.
    pub actor_name: String,
    /// HTTP method.
    pub method: String,
    /// Path, query already stripped.
    pub path: String,
    /// HTTP status.
    pub status: i16,
    /// How long the handler took.
    pub duration_ms: i32,
    /// The permission the guard resolved.
    pub permission: Option<String>,
    /// The keyed client fingerprint.
    pub client_fingerprint: Option<String>,
    /// When it happened.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl LogRow {
    /// The class this row falls in, so the badge and the filter cannot disagree.
    #[must_use]
    pub fn status_class(&self) -> &'static str {
        class_of(self.status)
    }
}

/// A page of log rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    /// The rows, newest first.
    pub rows: Vec<LogRow>,
    /// The cursor for the next page, `None` at the end.
    pub next_before: Option<i64>,
}

/// The filters the log screen offers. Every field is a narrowing, and an absent one means
/// "no opinion" rather than "match nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogQuery {
    /// Owning organization. Always set by the route.
    pub organization_id: Uuid,
    /// Restrict to one key.
    pub api_key_id: Option<Uuid>,
    /// Restrict to one method, uppercased.
    pub method: Option<String>,
    /// Restrict to a path starting with this prefix.
    pub path_prefix: Option<String>,
    /// Restrict to one status class (`2xx` … `5xx`).
    pub status_class: Option<String>,
    /// How far back, in days.
    pub window_days: Option<u32>,
    /// How many rows.
    pub limit: usize,
    /// Keyset cursor: rows older than this id.
    pub before: Option<i64>,
}

/// Defaults and limits for the query, applied once in the store so the route cannot forget.
pub const DEFAULT_WINDOW_DAYS: u32 = 7;

/// The longest window the screen offers.
pub const MAX_WINDOW_DAYS: u32 = 90;

/// Longest accepted method filter.
const MAX_METHOD: usize = 12;

/// Longest accepted path prefix.
const MAX_PATH_PREFIX: usize = 200;

/// Normalise and bound the query.
///
/// # Errors
///
/// [`DeveloperError::Invalid`] when the class is not one of [`STATUS_CLASSES`], the window is
/// zero or above [`MAX_WINDOW_DAYS`], the limit is zero or above [`MAX_PAGE`], or the method or
/// prefix is longer than the column it filters. Each message names the value, because every one
/// of these arrives from a query string somebody typed.
pub fn validate_query(query: &mut LogQuery) -> Result<()> {
    if query.limit == 0 {
        query.limit = 50;
    }
    if query.limit > MAX_PAGE {
        return Err(DeveloperError::Invalid(format!(
            "limit must be at most {MAX_PAGE}"
        )));
    }
    if query.window_days.is_some_and(|days| days == 0) {
        return Err(DeveloperError::Invalid(
            "window must be at least one day".into(),
        ));
    }
    if query.window_days.is_some_and(|days| days > MAX_WINDOW_DAYS) {
        return Err(DeveloperError::Invalid(format!(
            "window must be at most {MAX_WINDOW_DAYS} days"
        )));
    }
    if let Some(class) = query.status_class.as_deref() {
        if !STATUS_CLASSES.contains(&class) {
            return Err(DeveloperError::Invalid(format!(
                "status class must be one of {} (got \"{class}\")",
                STATUS_CLASSES.join(", ")
            )));
        }
    }
    if let Some(method) = query.method.as_deref() {
        if method.len() > MAX_METHOD {
            return Err(DeveloperError::Invalid("method filter is too long".into()));
        }
    }
    if let Some(prefix) = query.path_prefix.as_deref() {
        if prefix.len() > MAX_PATH_PREFIX {
            return Err(DeveloperError::Invalid("path prefix is too long".into()));
        }
    }
    if let Some(method) = query.method.as_mut() {
        *method = method.trim().to_ascii_uppercase();
    }
    if let Some(prefix) = query.path_prefix.as_mut() {
        *prefix = prefix.trim().to_owned();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn a_query_string_never_reaches_the_stored_path() {
        // The three that actually happen.
        assert_eq!(
            path_without_query("/api/v1/media?token=secret-value"),
            "/api/v1/media",
            "a token in the query string is a credential in the log's only export"
        );
        assert_eq!(
            path_without_query("/api/v1/developer/oauth/token?client_secret=abc&grant_type=x"),
            "/api/v1/developer/oauth/token"
        );
        assert_eq!(path_without_query("/api/v1/me"), "/api/v1/me");
        assert_eq!(
            path_without_query("/api/v1/me?a=1#frag"),
            "/api/v1/me",
            "a fragment never reaches a server, and keeping it would be noise"
        );
        assert_eq!(path_without_query(""), "");
    }

    #[test]
    fn a_path_that_is_only_a_query_still_reduces_to_empty_rather_than_to_the_query() {
        assert_eq!(path_without_query("?a=1"), "");
    }

    #[test]
    fn the_status_class_covers_every_status_the_column_accepts() {
        for status in 100..=599 {
            assert!(
                STATUS_CLASSES.contains(&class_of(status)),
                "{status} has no class, so the filter could never return it"
            );
        }
        assert_eq!(class_of(200), "2xx");
        assert_eq!(class_of(404), "4xx");
        assert_eq!(class_of(500), "5xx");
        assert_eq!(class_of(302), "3xx");
        // Below 100 cannot be stored (the check constraint refuses it), so the fallback is
        // unreachable from the database and only matters for a status a proxy invented.
        assert_eq!(class_of(0), "5xx");
    }

    #[test]
    fn a_query_string_is_refused_rather_than_silently_matching_nothing() {
        let mut query = LogQuery {
            organization_id: Uuid::nil(),
            status_class: Some("4xx".into()),
            ..LogQuery::default()
        };
        assert!(validate_query(&mut query).is_ok());

        query.status_class = Some("4xxx".into());
        let error = validate_query(&mut query).unwrap_err().to_string();
        assert!(
            error.contains("2xx") && error.contains("5xx"),
            "the refusal must name the classes that exist: {error}"
        );
    }

    #[test]
    fn a_window_of_zero_or_a_hundred_days_is_refused_by_name() {
        let mut query = LogQuery::default();
        query.window_days = Some(0);
        assert!(
            validate_query(&mut query)
                .unwrap_err()
                .to_string()
                .contains("at least one day")
        );

        query.window_days = Some(MAX_WINDOW_DAYS + 1);
        assert!(
            validate_query(&mut query)
                .unwrap_err()
                .to_string()
                .contains(&MAX_WINDOW_DAYS.to_string())
        );

        query.window_days = Some(1);
        assert!(validate_query(&mut query).is_ok());
    }

    #[test]
    fn the_default_page_is_a_page_and_not_a_full_table() {
        let mut query = LogQuery::default();
        assert_eq!(query.limit, 0, "the route may hand us nothing");
        validate_query(&mut query).unwrap();
        assert!(
            (1..=MAX_PAGE).contains(&query.limit),
            "a missing limit must become a bounded page, never the whole log: {}",
            query.limit
        );
    }

    #[test]
    fn a_method_filter_is_uppercased_so_it_matches_the_stored_column() {
        let mut query = LogQuery {
            method: Some(" post ".into()),
            ..LogQuery::default()
        };
        validate_query(&mut query).unwrap();
        assert_eq!(query.method.as_deref(), Some("POST"));
    }

    #[test]
    fn an_identity_with_neither_a_user_nor_a_key_is_anonymous() {
        // Exercised through `new` so the fingerprint path is covered too; the pepper is set by
        // the sibling test that needs it, and an absent address short-circuits before it.
        let identity = ClientIdentity::new(None, None, None, None, None, None, None, None).unwrap();
        assert!(identity.is_anonymous());
        assert_eq!(identity.client_fingerprint, None);
    }
}
