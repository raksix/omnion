//! `/api/v1/security/events` — the security-event timeline (REQ-012, slice 4).
//!
//! ## The trap in the requirement's own wording
//!
//! The REQ says "a security-event table **from the audit trail**", and that sentence names the
//! wrong single source. The audit trail holds privileged actions somebody took; it holds **no
//! sign-ins at all**, because a failed sign-in happens before there is a session and therefore
//! before there is an actor to write an audit entry for. A route built over `audit_log` alone
//! would answer `200` with an empty table on a platform where every one of its own requirements
//! is met, and the empty table would look like a working filter.
//!
//! So this route reads the **union**: [`omnion_security::list_security_events`] merges the audit
//! trail with `sign_in_attempts`, and every row says which table it came from.
//!
//! ## What this route will not claim
//!
//! **Permission denials are absent, and the screen says so rather than implying otherwise.** The
//! guard in `apps/api/src/guards.rs` answers `403 permission_denied` and records nothing, so
//! there is no row to project. The `denial` category therefore resolves to the address rule's
//! `blocked` outcome only. This is stated here and on the screen instead of being quietly
//! filled in with a synthetic row: recording an audit entry for every refusal would put a
//! database write on the path of every refused request, and an attacker would decide how fast
//! the audit table fills.

use axum::extract::{RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use omnion_security::{
    EventCategory, EventPage, EventQuery, EventSource, MAX_EVENT_EXPORT_ROWS, SecurityEvent,
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a store error onto the API surface — the same mapping the other security routes use.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    match error {
        omnion_security::SecurityError::Invalid(message) => {
            ApiError::bad_request("invalid_security_input", message)
        }
        omnion_security::SecurityError::NotFound => {
            ApiError::new(axum::http::StatusCode::NOT_FOUND, "not_found", "not found")
        }
        omnion_security::SecurityError::Database(inner) => ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security store: {inner}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One event as the screen reads it.
#[derive(Debug, Serialize)]
pub struct EventBody {
    /// `"<source>:<id>"` — unique across both tables.
    pub id: String,
    /// `audit` or `sign_in`. The screen renders it, because a merged list whose rows do not say
    /// where they came from is a list an operator cannot reason about during an incident.
    pub source: String,
    /// When it happened.
    pub occurred_at: String,
    /// The stable action name, or the sign-in outcome word.
    pub action: String,
    /// The screen's category for it.
    pub category: String,
    /// Who acted — `None` on a sign-in attempt, and `None` there is a **fact**: nobody was
    /// authenticated. The screen renders "no actor — the attempt was refused before sign-in".
    pub actor: Option<String>,
    /// The account an action was *about*, which is not the actor for a lockout.
    pub subject_user_id: Option<String>,
    /// The peer address, when recorded.
    pub client_ip: Option<String>,
    /// The user agent, when recorded.
    pub user_agent: Option<String>,
    /// One line an operator reads instead of parsing the action name.
    pub outcome: String,
    /// A key-level digest. Never the audit metadata itself — see the crate module.
    pub detail: Option<String>,
    /// Whether this row is a refusal the operator should look at.
    pub refused: bool,
}

impl From<&SecurityEvent> for EventBody {
    fn from(event: &SecurityEvent) -> Self {
        Self {
            id: event.id.clone(),
            source: event.source.as_str().to_owned(),
            occurred_at: event.occurred_at.to_string(),
            action: event.action.clone(),
            category: event.category.as_str().to_owned(),
            actor: event.actor.clone(),
            subject_user_id: event.subject_user_id.map(|id| id.to_string()),
            client_ip: event.client_ip.clone(),
            user_agent: event.user_agent.clone(),
            outcome: event.outcome.clone(),
            detail: event.detail.clone(),
            refused: omnion_security::is_refusal(&event.action),
        }
    }
}

/// The page, plus the counters the header shows and the two honesty flags.
#[derive(Debug, Serialize)]
pub struct EventsBody {
    /// The rows, newest first.
    pub events: Vec<EventBody>,
    /// How many rows the filter matched in total, so the screen can say "50 of 312".
    pub total: i64,
    /// How many came from the audit trail.
    pub audit_count: i64,
    /// How many came from the sign-in log.
    pub sign_in_count: i64,
    /// Whether the page is shorter than the total.
    pub truncated: bool,
    /// The categories the filter offers. Served rather than hard-coded in the panel, because a
    /// drop-down that offers a category the API cannot produce is a filter that returns nothing
    /// and reads as "no events matched".
    pub categories: Vec<&'static str>,
}

impl From<EventPage> for EventsBody {
    fn from(page: EventPage) -> Self {
        Self {
            events: page.events.iter().map(EventBody::from).collect(),
            total: page.total,
            audit_count: page.audit_count,
            sign_in_count: page.sign_in_count,
            truncated: page.truncated,
            categories: EventCategory::ALL.iter().map(|c| c.as_str()).collect(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter parsing
// ---------------------------------------------------------------------------------------------

/// Read one query parameter, trimmed, treating blank as absent.
fn param<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    for pair in raw.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == key {
            let trimmed = value.trim();
            return (!trimmed.is_empty()).then_some(trimmed);
        }
    }
    None
}

/// Parse the filter. Every unknown value is refused **by name**.
///
/// The rule the findings route already follows, and the reason matters more here: an unparsed
/// `category=denials` that quietly becomes "no filter" shows the operator an unfiltered
/// timeline that they read as a filtered one. A refusal names the field and the valid words.
fn parse_query(raw: Option<&str>) -> Result<EventQuery, ApiError> {
    let Some(raw) = raw else {
        return Ok(EventQuery {
            limit: Some(50),
            ..EventQuery::default()
        });
    };

    let category = match param(raw, "category") {
        Some(value) => Some(
            EventCategory::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == value)
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "invalid_security_input",
                        format!(
                            "\"{value}\" is not a security-event category — choose one of: {}",
                            EventCategory::ALL
                                .iter()
                                .map(|c| c.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?,
        ),
        None => None,
    };

    let source = match param(raw, "source") {
        Some("audit") => Some(EventSource::Audit),
        Some("sign_in") => Some(EventSource::SignIn),
        Some(other) => {
            return Err(ApiError::bad_request(
                "invalid_security_input",
                format!("\"{other}\" is not a source — choose audit or sign_in"),
            ));
        }
        None => None,
    };

    let since = parse_timestamp(raw, "since")?;
    let until = parse_timestamp(raw, "until")?;

    if let (Some(from), Some(to)) = (since, until)
        && from > to
    {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            format!(
                "the window starts {from} and ends {to} — an event cannot happen outside its own \
                 range, so this filter would always be empty"
            ),
        ));
    }

    let limit = match param(raw, "limit") {
        Some(value) => Some(
            value
                .parse::<usize>()
                .map_err(|_| {
                    ApiError::bad_request(
                        "invalid_security_input",
                        format!("\"{value}\" is not a number of rows"),
                    )
                })?
                .clamp(1, 200),
        ),
        None => Some(50),
    };

    Ok(EventQuery {
        search: param(raw, "q").map(str::to_owned),
        category,
        source,
        since,
        until,
        limit,
    })
}

/// Parse an RFC 3339 instant, refusing the value **with what was typed**.
fn parse_timestamp(raw: &str, key: &str) -> Result<Option<OffsetDateTime>, ApiError> {
    let Some(value) = param(raw, key) else {
        return Ok(None);
    };
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_security_input",
                format!("\"{value}\" is not a timestamp for {key} — use an RFC 3339 value like 2026-01-01T00:00:00Z"),
            )
        })
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /security/events` — the merged timeline.
pub async fn get(
    State(state): State<AppState>,
    session: CurrentSession,
    RawQuery(raw): RawQuery,
) -> Result<Json<EventsBody>, ApiError> {
    let query = parse_query(raw.as_deref())?;
    let page = omnion_security::list_security_events(state.db().pool(), &query)
        .await
        .map_err(map_store)?;
    Ok(Json(EventsBody::from(page)))
}

/// `GET /security/events.csv` — the current filter as a CSV document.
///
/// A separate path rather than a flag on the list, for the reason the findings export gives: the
/// two answers have different content types, and a client that asks for CSV and receives JSON has
/// to guess. The page size is **dropped** — the export is the whole filter, and a file an
/// operator attaches to a ticket that silently stops at 50 rows is the failure worth naming.
pub async fn export(
    State(state): State<AppState>,
    _session: CurrentSession,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    use axum::response::IntoResponse as _;

    let mut query = parse_query(raw.as_deref())?;
    query.limit = None;

    let rows = omnion_security::export_security_events(state.db().pool(), &query)
        .await
        .map_err(map_store)?;

    // The cap is checked here rather than inside the renderer, because the refusal the operator
    // sees must **name the count** — "50,001 events match" is actionable, and a generic 413 is
    // not.
    if rows.len() > MAX_EVENT_EXPORT_ROWS {
        return Err(ApiError::bad_request(
            "invalid_security_input",
            format!(
                "{} events match this filter; an export carries at most {MAX_EVENT_EXPORT_ROWS} — \
                 narrow the filter",
                rows.len()
            ),
        ));
    }

    let document = omnion_security::render_events_csv(&rows).map_err(map_store)?;

    let stamp = OffsetDateTime::now_utc().date().to_string();
    let filename = format!("security-events-{stamp}.csv");
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    // `attachment`, never `inline`: a CSV served inline from the API origin is one click from
    // being opened as a document on it.
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("an ASCII filename always parses"),
    );
    // The export is a file that leaves the platform, so it carries the one header that stops a
    // browser from deciding it is something else — the same one the findings export sends.
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));

    Ok((StatusCode::OK, headers, document).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_is_a_page_of_fifty_and_not_an_unbounded_read() {
        // A timeline with no limit is a statement the client can make by omitting a parameter,
        // so the default is a page rather than "everything".
        let query = parse_query(None).expect("an absent query parses");
        assert_eq!(query.limit, Some(50));
        let query = parse_query(Some("")).expect("an empty query parses");
        assert_eq!(query.limit, Some(50));
    }

    #[test]
    fn a_blank_parameter_is_absent_rather_than_an_unknown_value() {
        // `?category=` in a pasted URL must not be refused as a category named "" — it is a
        // filter with nothing selected.
        let query = parse_query(Some("category=&q=")).expect("blank values are absent");
        assert!(query.category.is_none());
        assert!(query.search.is_none());
    }

    #[test]
    fn an_unknown_category_is_refused_by_name_and_lists_the_valid_ones() {
        // The alternative is worse than a refusal: an unparsed category silently becomes "no
        // filter", and the operator reads an unfiltered timeline as a filtered one.
        let error = parse_query(Some("category=denials")).expect_err("a typo is refused");
        let message = error.to_string();
        assert!(message.contains("denials"), "{message} does not name the value");
        assert!(
            message.contains("ip_rule_change") && message.contains("lockout"),
            "{message} does not list the valid categories"
        );
    }

    #[test]
    fn every_offered_category_parses_back() {
        // The drop-down is served from `EventCategory::ALL`, so this closes the loop: every
        // word the panel can render is a word the API accepts.
        for category in EventCategory::ALL {
            let query = parse_query(Some(&format!("category={}", category.as_str())))
                .expect("every offered category parses");
            assert_eq!(query.category, Some(*category));
        }
    }

    #[test]
    fn an_unknown_source_is_refused_and_the_two_are_named() {
        let error = parse_query(Some("source=logs")).expect_err("a typo is refused");
        let message = error.to_string();
        assert!(message.contains("audit") && message.contains("sign_in"), "{message}");
    }

    #[test]
    fn a_source_of_one_table_is_the_only_kind_accepted() {
        assert_eq!(
            parse_query(Some("source=audit")).expect("audit").source,
            Some(EventSource::Audit)
        );
        assert_eq!(
            parse_query(Some("source=sign_in")).expect("sign_in").source,
            Some(EventSource::SignIn)
        );
    }

    #[test]
    fn a_window_that_ends_before_it_starts_is_refused() {
        // A filter that can only ever be empty is a filter an operator will debug for ten
        // minutes. The message says so rather than answering with an empty table.
        let error = parse_query(Some("since=2026-02-01T00:00:00Z&until=2026-01-01T00:00:00Z"))
            .expect_err("an inverted window is refused");
        assert!(
            error.to_string().contains("outside its own range"),
            "{} does not explain itself",
            error
        );
    }

    #[test]
    fn a_well_formed_window_parses_on_both_ends() {
        let query = parse_query(Some("since=2026-01-01T00:00:00Z&until=2026-01-31T23:59:59Z"))
            .expect("a valid window parses");
        assert!(query.since.is_some());
        assert!(query.until.is_some());
        assert!(query.since < query.until);
    }

    #[test]
    fn a_malformed_timestamp_is_refused_with_what_was_typed() {
        let error =
            parse_query(Some("since=yesterday")).expect_err("a word is not a timestamp");
        assert!(error.to_string().contains("yesterday"), "{error}");
    }

    #[test]
    fn a_page_size_is_clamped_rather_than_refused() {
        // 0 would be an empty page, which reads as "no events matched"; it becomes one row
        // instead, which reads as a page that asked for a very small one.
        assert_eq!(
            parse_query(Some("limit=0")).expect("zero clamps").limit,
            Some(1)
        );
        assert_eq!(
            parse_query(Some("limit=99999")).expect("huge clamps").limit,
            Some(200)
        );
        assert_eq!(
            parse_query(Some("limit=25")).expect("a real page").limit,
            Some(25)
        );
    }

    #[test]
    fn a_non_numeric_page_size_is_refused() {
        let error = parse_query(Some("limit=lots")).expect_err("a word is not a count");
        assert!(error.to_string().contains("lots"), "{error}");
    }

    #[test]
    fn the_drop_down_offers_every_category_the_filter_accepts() {
        // The body's `categories` is what the panel renders, so this is the assertion that keeps
        // a dead filter out of the drop-down.
        let body = EventsBody::from(EventPage {
            events: Vec::new(),
            total: 0,
            audit_count: 0,
            sign_in_count: 0,
            truncated: false,
        });
        assert_eq!(body.categories.len(), EventCategory::ALL.len());
        for category in &body.categories {
            assert!(
                EventCategory::ALL.iter().any(|c| c.as_str() == *category),
                "{category} is offered but not parseable"
            );
        }
    }
}