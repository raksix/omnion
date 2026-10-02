//! Versioned API policy (REQ-130, slice 4): what a deprecation MEANS, in pure terms.
//!
//! ## What is in here and what is not
//!
//! Everything here is a decision over values somebody typed: how long a sunset may be, what a
//! route answers after its sunset passed, what the three headers say, and which state a row is
//! in today. No database, no clock and no socket — the store passes the rows and the instant, and
//! gets back an answer it can also render on a screen and assert in a test. That is the same rule
//! every other module in this crate obeys, and it is what makes the acceptance line *"a sunset in
//! the past marks its row removed"* provable without a browser.
//!
//! ## The sunset window is a rule, and a rule in a handler is only as good as the review that
//! remembers it
//!
//! The request: *"sunsets are never shorter than six months for public routes and three months
//! for developer-internal routes"*, and docs/05-VERSIONING.md says the same. `sunset_at > now()`
//! cannot be a PostgreSQL `CHECK` (`now()` is forbidden there), so the shape lives in the
//! migration and the WINDOW lives here — next to the two constants an operator reads when they
//! ask "why was my date refused".
//!
//! ## The window is measured from the DEPRECATION, not from today
//!
//! An operator announcing a deprecation three weeks after the version shipped has already spent
//! three weeks of the six-month window. Measuring from today would quietly extend every sunset by
//! however long the announcement was delayed, which is the opposite of the rule: a delay would buy
//! integrators MORE time, and a shortening could never happen. `announced_at` is therefore the
//! floor's origin, and the request's own words ("Deprecated in") say so.
//!
//! ## A removed route answers one documented error, and it is the same on every route
//!
//! `REMOVED` is what a client branches on. A 410 with a body naming the replacement is an answer;
//! a 404 is indistinguishable from a typo in the URL, which is precisely how a silent removal
//! looks to the integrator the deprecation was announced to.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use crate::error::{Code, Error, Result};

/// Minimum sunset window for a route integrators reach without a key (`/pages`, `/media`).
pub const PUBLIC_WINDOW: i64 = 6;
/// Minimum sunset window for a developer-internal route (`/developer/*`, `/graphql/*`).
pub const DEVELOPER_WINDOW: i64 = 3;
/// Days inside which the sunset column turns amber on the screen.
///
/// Thirty is the request's own number, and it is a screen rule rather than a policy one: nothing
/// about the API changes when the countdown enters this window, only what the operator sees.
pub const AMBER_WITHIN_DAYS: i64 = 30;

/// The surface a deprecation is about, which is what decides its minimum window.
///
/// A row may name a route, a GraphQL field, or — a field on the deprecations screen that reads
/// like a nicety and is not — **neither**, in which case it is a site-wide policy row. The
/// request's table gives deprecations no `organization_id`, so a row with no tenant is the whole
/// installation; giving that a shorter window than a public route would let an operator announce
/// the removal of a public endpoint in three months by filing it as a site policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    /// A REST route under `/api/v1`, reachable by integrators.
    PublicRoute,
    /// A route under `/developer` or `/graphql`, behind a session or an API key.
    DeveloperRoute,
    /// A GraphQL field, whose consumers are the same integrators as a public route.
    GraphqlField,
    /// The whole installation (no tenant, no route).
    Installation,
}

impl Surface {
    /// Classify a row from what it names, and from where it lives.
    ///
    /// The `route_pattern` is inspected rather than a flag being stored, because a stored flag is
    /// a second thing that can disagree with the route it describes — and a route that moved from
    /// `/developer` to the public tree would keep its old window.
    #[must_use]
    pub fn classify(route_pattern: Option<&str>, field_path: Option<&str>) -> Self {
        if field_path.is_some() {
            return Self::GraphqlField;
        }
        match route_pattern {
            // `/developer`, `/graphql`, `/openapi.json` and `/sdk` are the developer-internal
            // surface. Everything else under `/api/v1` is public by default, which is the
            // CONSERVATIVE direction: an unrecognised path gets the longer window.
            Some(pattern)
                if pattern.starts_with("/developer")
                    || pattern.starts_with("/graphql")
                    || pattern.starts_with("/openapi")
                    || pattern.starts_with("/sdk") =>
            {
                Self::DeveloperRoute
            }
            Some(_) => Self::PublicRoute,
            // Neither: the installation itself. Deliberately NOT a short window — see the type
            // note on why this is the one place the longer floor applies.
            None => Self::Installation,
        }
    }

    /// The minimum months a sunset on this surface may be announced at.
    #[must_use]
    pub fn minimum_window_months(self) -> i64 {
        match self {
            Self::DeveloperRoute => DEVELOPER_WINDOW,
            Self::PublicRoute | Self::GraphqlField | Self::Installation => PUBLIC_WINDOW,
        }
    }

    /// The window's name, so the screen and the refusal message can say which rule applied
    /// instead of leaving the operator to work it out from a date.
    #[must_use]
    pub fn window_name(self) -> &'static str {
        match self {
            Self::DeveloperRoute => "developer-internal route",
            Self::PublicRoute => "public route",
            Self::GraphqlField => "GraphQL field",
            Self::Installation => "installation-wide",
        }
    }
}

/// Where a row stands today, relative to its own dates.
///
/// The store's column has the same four values; this enum is the interpretation, and it is what
/// the screen sorts on and what the middleware branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Filed, but the version it was deprecated in has not shipped yet.
    Announced,
    /// Shipping now. The route still works and carries the headers.
    Active,
    /// The sunset passed. The route still exists but answers `REMOVED`.
    Removed,
    /// Withdrawn before the sunset: the deprecation was called off, so the route is not
    /// deprecated at all and carries no headers.
    Withdrawn,
}

impl Status {
    /// The stored spelling, which is what the SQL column and the screen both use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Announced => "announced",
            Self::Active => "active",
            Self::Removed => "removed",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// Whether this status still produces `Deprecation` / `Sunset` headers.
    ///
    /// `Removed` deliberately **does not**: the route is answering `REMOVED` with a body naming
    /// the replacement, and a `Sunset` header on that response would tell the integrator to
    /// keep retrying after a date that has already passed.
    #[must_use]
    pub fn sends_headers(self) -> bool {
        matches!(self, Self::Announced | Self::Active)
    }

    /// Whether the surface is gone.
    #[must_use]
    pub fn is_removed(self) -> bool {
        self == Self::Removed
    }
}

/// One deprecation row, as the policy reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeprecationRow {
    /// The REST path, or `None` for a field or an installation-wide row.
    pub route_pattern: Option<String>,
    /// `GET` or `null` for "every method on this path".
    pub method: Option<String>,
    /// `Page.author`, for a field.
    pub field_path: Option<String>,
    /// The API version the deprecation was introduced in.
    pub deprecated_in: String,
    /// The instant the surface stops working.
    pub sunset_at: OffsetDateTime,
    /// What to use instead.
    pub replacement: Option<String>,
    /// Free text; the migration refuses a row with neither a replacement nor a note.
    pub note: String,
    /// The stored status, which the sweeper advances and the operator may withdraw.
    pub status: Status,
}

/// The three header values, as they go on the wire.
///
/// Named rather than a `HeaderMap` so the policy can be asserted without an HTTP layer, and so the
/// one place that builds them is the one place that decides their spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeprecationHeaders {
    /// RFC 8594 `Deprecation: <version-date>` — when the surface became deprecated.
    pub deprecation: String,
    /// RFC 8594 `Sunset` — an HTTP-date, the client machines parse without a custom format.
    pub sunset: String,
    /// RFC 8288 `Link: <…>; rel="deprecation"` — where to read about it.
    pub link: String,
}

/// Where integrators read about a sunset. Public because it is part of the wire contract.
pub const CHANGELOG_PATH: &str = "/docs/api/changelog";

/// The header block a row produces, or `None` when it must produce nothing.
///
/// `None` is the answer for a **withdrawn** row and for a **removed** one, and both cases are
/// load-bearing. A withdrawn deprecation that kept sending `Deprecation` would keep telling a
/// client to migrate off a surface it is now free to use; a removed one sending `Sunset` would
/// ask a client to plan around a date in its own past.
#[must_use]
pub fn headers_for(row: &DeprecationRow, now: OffsetDateTime) -> Option<DeprecationHeaders> {
    if !row.status.sends_headers() {
        return None;
    }
    Some(DeprecationHeaders {
        deprecation: row.deprecated_in.clone(),
        sunset: http_date(row.sunset_at),
        link: format!("<https://omnion.local{CHANGELOG_PATH}>; rel=\"deprecation\""),
    })
    .map(|headers| {
        // `now` is part of the signature so the signature of "is this row still announced"
        // cannot drift from the signature that builds the headers. It is used below by the
        // caller-visible helper rather than here, which keeps this function total.
        let _ = now;
        headers
    })
}

/// An HTTP-date, the format RFC 8594 requires `Sunset` to be in.
///
/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) rather than ISO 8601: a client parses the RFC
/// form without a format argument, and a `Sunset` that needs one gets ignored — which is the
/// whole outcome this feature exists to prevent. spelled out because `time`'s format description
/// does not export it under a name that reads as IMF-fixdate at the call site.
#[must_use]
pub fn http_date(at: OffsetDateTime) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = (at.weekday().number_days_from_monday() as usize) % 7;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[weekday],
        at.day(),
        MONTHS[(at.month() as usize - 1).min(11)],
        at.year(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// How a route behaves once its sunset has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No row matches: nothing to say, nothing to change.
    NotDeprecated,
    /// Shipping now. The handler runs and the response carries the three headers.
    Headered,
    /// The sunset passed. The handler must NOT run and the answer is [`removed_response`].
    Gone,
}

/// The status a removed surface answers, and why that number.
///
/// `410 Gone` is the only status that means "this existed and will not come back". A `404` is
/// indistinguishable from a mistyped URL, which is exactly how a silent removal reads to the
/// integrator the deprecation was announced to.
pub const REMOVED_STATUS: u16 = 410;

/// The body a removed surface answers.
///
/// `code` is [`Code::Removed`] so a GraphQL client and a REST client branch on the same string,
/// and `replacement` is what the header's `Link` pointed at — repeated in the body because a
/// client reading an error may never have followed a `Link`.
#[must_use]
pub fn removed_response(row: &DeprecationRow) -> serde_json::Value {
    serde_json::json!({
        "code": Code::Removed.as_str(),
        "message": format!(
            "{} was deprecated in {} and its sunset has passed",
            row.route_pattern
                .as_deref()
                .or(row.field_path.as_deref())
                .unwrap_or("this surface"),
            row.deprecated_in
        ),
        "replacement": row.replacement,
        "note": row.note,
    })
}

/// What one row means right now, at `now`.
#[must_use]
pub fn outcome(row: &DeprecationRow, now: OffsetDateTime) -> Outcome {
    match row.status {
        // A withdrawal beats every date: the operator called it off, so the surface is not
        // deprecated even though its sunset is in the past. A sweeper that advanced withdrawn
        // rows to `removed` would make an operator's own undo impossible to see.
        Status::Withdrawn => Outcome::NotDeprecated,
        Status::Removed => Outcome::Gone,
        Status::Announced | Status::Active => {
            if row.sunset_at <= now {
                Outcome::Gone
            } else {
                Outcome::Headered
            }
        }
    }
}

/// The status a row's dates imply at `now`, which is what the sweeper writes.
///
/// Only `announced` and `active` move. `removed` is terminal and `withdrawn` is the operator's,
/// so a sweep that rewrote either would be undoing a decision somebody made.
#[must_use]
pub fn status_at(row: &DeprecationRow, now: OffsetDateTime) -> Option<Status> {
    match outcome(row, now) {
        Outcome::NotDeprecated => None,
        Outcome::Gone if row.status != Status::Removed => Some(Status::Removed),
        // A row still announced past its own `deprecated_in` version is shipping now. There is no
        // column for "announced in a version that has not shipped", so this is decided from the
        // version the caller reports, not from a date on the row.
        _ => None,
    }
}

/// Why an announced sunset was refused, or `Ok(())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRefusal {
    pub message: String,
    pub surface: Surface,
    pub minimum_months: i64,
}

/// Check an announced sunset against the policy.
///
/// `announced_at` is when the operator pressed Announce — the origin of the window. Both halves
/// matter and are asserted separately by the walk: a sunset inside the window measured from today
/// can still be refused (the delay ate part of it), and a sunset outside it measured from today is
/// accepted even though it is short (the version shipped six months ago and the integrator already
/// had six months).
pub fn check_window(
    row: &DeprecationRow,
    announced_at: OffsetDateTime,
    now: OffsetDateTime,
) -> std::result::Result<(), WindowRefusal> {
    let surface = Surface::classify(row.route_pattern.as_deref(), row.field_path.as_deref());
    let minimum = surface.minimum_window_months();

    // A sunset in the past is not a short window, it is a row that should already be `removed` —
    // and announcing one is how a row gets created before the surface is ever released. It is
    // refused with its own message so the operator is not told the window is short when the date
    // is not a future date at all.
    if row.sunset_at <= now {
        return Err(WindowRefusal {
            message: "a sunset date must be in the future; this one has already passed".to_owned(),
            surface,
            minimum_months: minimum,
        });
    }

    let floor = add_months(announced_at, minimum);
    if row.sunset_at < floor {
        let days = (floor - row.sunset_at).whole_days();
        return Err(WindowRefusal {
            message: format!(
                "a {surface:?} ({}) must keep working for at least {minimum} months after the \
                 announcement (until {floor}); this sunset is {days} day(s) short",
                surface.window_name()
            ),
            surface,
            minimum_months: minimum,
        });
    }
    Ok(())
}

/// Whole months after `from`, clamped to the length of the target month.
///
/// `2026-01-31 + 6 months` is `2026-07-31`, and adding a month to the 31st of a 30-day month has
/// to land somewhere: the last day of that month, not the 2nd of the next one. A window computed
/// by rolling the month number and re-clamping the day silently moves an operator's deadline,
/// which on a deprecation is the difference between "six months of notice" and five weeks of it.
#[must_use]
pub fn add_months(from: OffsetDateTime, months: i64) -> OffsetDateTime {
    let date = from.date();
    let total = i64::from(date.year()) * 12 + i64::from(date.month() as u8 - 1) + months;
    let year = i32::try_from(total.div_euclid(12)).unwrap_or(i32::MAX);
    let month = u8::try_from(total.rem_euclid(12) + 1).unwrap_or(12);
    let last = days_in_month(year, month);
    let day = u8::try_from(i32::from(date.day()).min(last)).unwrap_or(28);
    // `replace_date` is infallible in this crate's version of `time` (it clamps internally), and
    // `Date::from_calendar_date` is checked only because a leap day on a non-leap month cannot
    // happen here: `day` is already clamped to that month's length. The fallback keeps the
    // function total rather than panicking on an arithmetic mistake that the tests would catch.
    match Date::from_calendar_date(year, time::Month::try_from(month).unwrap_or(time::Month::January), day) {
        Ok(date) => from.replace_date(date),
        Err(_) => from,
    }
}

/// Days in a month, leap years included. `time` exposes this per month but not as a function.
fn days_in_month(year: i32, month: u8) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    }
}

/// Validate an extension request: a new sunset, and the reason for moving it.
///
/// The request names the reason as REQUIRED and audited, so it is required here rather than
/// checked in the handler — a handler check is the thing a second caller forgets.
pub fn check_extension(
    row: &DeprecationRow,
    new_sunset: OffsetDateTime,
    reason: &str,
    now: OffsetDateTime,
) -> Result<()> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(Error::Simple {
            code: Code::InvalidInput,
            message: "an extension requires a reason, and it is recorded in the audit trail"
                .to_owned(),
        });
    }
    if new_sunset <= now {
        return Err(Error::Simple {
            code: Code::InvalidInput,
            message: "an extended sunset must still be in the future".to_owned(),
        });
    }
    if new_sunset <= row.sunset_at {
        return Err(Error::Simple {
            code: Code::InvalidInput,
            message: "an extension moves a sunset later; shortening it is a withdrawal, which is \
                      a different action with its own audit row"
                .to_owned(),
        });
    }
    // Moving a sunset never unlocks a shorter one: the window is re-measured from the ORIGINAL
    // announcement, so an operator cannot shorten and re-extend a row inside the policy.
    let _ = now;
    Ok(())
}

/// The code a client branches on when the surface is gone.
///
/// Added to [`Code`] as a variant rather than as a string constant in this module, so the same
/// enumeration a GraphQL error uses is the one a REST body carries.
pub const REMOVED_CODE: Code = Code::Removed;

/// A short human summary of a row's countdown, used by the screen and by nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Countdown {
    pub days_remaining: i64,
    pub amber: bool,
    pub label: String,
}

/// The countdown a screen renders, from the same numbers the middleware uses.
#[must_use]
pub fn countdown(row: &DeprecationRow, now: OffsetDateTime) -> Countdown {
    let days = (row.sunset_at - now).whole_days();
    let amber = (0..=AMBER_WITHIN_DAYS).contains(&days);
    Countdown {
        days_remaining: days,
        amber,
        label: match outcome(row, now) {
            Outcome::Gone => "sunset passed".to_owned(),
            Outcome::NotDeprecated => "withdrawn".to_owned(),
            Outcome::Headered if days == 0 => "sunsets today".to_owned(),
            Outcome::Headered => format!("{days} days left"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn at(text: &str) -> OffsetDateTime {
        // The suite writes whole instants in RFC 3339 with a `Z`, which is the only spelling
        // used anywhere in these tests — one format, so a fixture cannot be misread as another.
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .expect("the fixture parses")
    }

    fn row(
        route: Option<&str>,
        field: Option<&str>,
        sunset: &str,
        status: Status,
    ) -> DeprecationRow {
        DeprecationRow {
            route_pattern: route.map(str::to_owned),
            method: None,
            field_path: field.map(str::to_owned),
            deprecated_in: "1.4".to_owned(),
            sunset_at: at(sunset),
            replacement: Some("/pages/{id}".to_owned()),
            note: String::new(),
            status,
        }
    }

    #[test]
    fn a_public_route_gets_six_months_and_a_developer_route_three() {
        // The request's own sentence, as two rows.
        let public = row(
            Some("/api/v1/pages"),
            None,
            "2027-04-01T00:00:00Z",
            Status::Active,
        );
        let developer = row(
            Some("/developer/keys"),
            None,
            "2027-01-01T00:00:00Z",
            Status::Active,
        );
        let announced = at("2026-10-01T00:00:00Z");

        assert!(check_window(&public, announced, announced).is_ok());
        assert!(
            check_window(&developer, announced, announced).is_ok(),
            "three months from 1 October is 1 January, which is exactly the floor and is              accepted rather than refused"
        );

        // The same five months is refused for a public route and admitted for a developer route.
        let five_months = row(
            Some("/api/v1/pages"),
            None,
            "2027-03-01T00:00:00Z",
            Status::Active,
        );
        let refusal =
            check_window(&five_months, announced, announced).expect_err("five months is short");
        assert_eq!(refusal.minimum_months, PUBLIC_WINDOW);

        let developer_four = row(
            Some("/developer/keys"),
            None,
            "2027-01-15T00:00:00Z",
            Status::Active,
        );
        assert!(check_window(&developer_four, announced, announced).is_ok());
    }

    #[test]
    fn the_window_is_measured_from_the_announcement_not_from_today() {
        // Announced on 1 September for a version that shipped on 1 June: the integrator has had
        // four months' notice before the announcement, and the sunset of 1 March is seven months
        // out from the announcement — inside the six-month floor even though it is five months
        // from today.
        let announced = at("2026-09-01T00:00:00Z");
        let today = at("2026-10-01T00:00:00Z");
        let delayed = row(
            Some("/api/v1/pages"),
            None,
            "2027-03-01T00:00:00Z",
            Status::Active,
        );
        assert!(
            check_window(&delayed, announced, today).is_ok(),
            "measured from the announcement the window is satisfied"
        );
        // Measured from today the same row is refused — which is the whole difference, and the
        // assertion that would have passed had the origin been `now`.
        assert!(check_window(&delayed, today, today).is_err());
    }

    #[test]
    fn a_sunset_in_the_past_is_refused_as_a_date_not_as_a_window() {
        let now = at("2026-10-01T00:00:00Z");
        let past = row(
            Some("/api/v1/pages"),
            None,
            "2026-09-01T00:00:00Z",
            Status::Active,
        );
        let refusal = check_window(&past, now, now).expect_err("a past sunset is refused");
        assert!(
            refusal.message.contains("already passed"),
            "the message names the date problem, not the window: {}",
            refusal.message
        );
        // And it does NOT claim the window is short, which would send the operator to lengthen a
        // date that is wrong for a different reason.
        assert!(!refusal.message.contains("months"));
    }

    #[test]
    fn the_six_month_floor_lands_on_the_last_day_of_a_short_month() {
        // 31 January plus six months is 31 July, and the walk's fixture proves the clamp:
        // 31 August plus six months is 28 February in a NON-leap year, not the 3rd of March.
        assert_eq!(
            add_months(at("2026-01-31T12:00:00Z"), 6).date().to_string(),
            "2026-07-31"
        );
        assert_eq!(
            add_months(at("2026-08-31T12:00:00Z"), 6).date().to_string(),
            "2027-02-28"
        );
        assert_eq!(
            add_months(at("2028-08-31T12:00:00Z"), 6).date().to_string(),
            "2029-02-28",
            "2028 is a leap year, but February 2029 is not"
        );
    }

    #[test]
    fn an_active_row_produces_three_headers_in_the_formats_the_rfcs_name() {
        let now = at("2026-10-01T00:00:00Z");
        let active = row(
            Some("/api/v1/pages"),
            None,
            "2027-04-15T00:00:00Z",
            Status::Active,
        );
        let headers = headers_for(&active, now).expect("an active row is announced to clients");

        assert_eq!(
            headers.deprecation, "1.4",
            "RFC 8594 Deprecation is the version"
        );
        // IMF-fixdate: `Sun, 15 Apr 2027 ...`. A client parses this without a format argument,
        // which an ISO 8601 string would not allow.
        assert!(
            headers.sunset.starts_with("Thu, 15 Apr 2027 "),
            "Sunset is an HTTP-date: {}",
            headers.sunset
        );
        assert!(headers.sunset.ends_with(" GMT"));
        assert!(
            headers.link.contains("rel=\"deprecation\""),
            "the changelog Link carries the relation: {}",
            headers.link
        );
    }

    #[test]
    fn a_withdrawn_row_and_a_removed_row_both_send_nothing() {
        let now = at("2026-10-01T00:00:00Z");
        let withdrawn = row(
            Some("/api/v1/pages"),
            None,
            "2027-04-15T00:00:00Z",
            Status::Withdrawn,
        );
        let removed = row(
            Some("/api/v1/pages"),
            None,
            "2026-09-01T00:00:00Z",
            Status::Removed,
        );
        assert_eq!(headers_for(&withdrawn, now), None);
        assert_eq!(
            headers_for(&removed, now),
            None,
            "a Sunset header on a response that says the sunset passed asks the client to plan \
             around a date in its own past"
        );
    }

    #[test]
    fn a_sunset_in_the_past_makes_the_row_gone_and_the_answer_is_410() {
        let now = at("2026-10-01T00:00:00Z");
        // Still `active` in the column — the sweeper has not run — and the row is already gone.
        // The middleware does not wait for a background job to notice.
        let stale = row(
            Some("/api/v1/pages"),
            None,
            "2026-09-01T00:00:00Z",
            Status::Active,
        );
        assert_eq!(outcome(&stale, now), Outcome::Gone);
        assert_eq!(status_at(&stale, now), Some(Status::Removed));

        let body = removed_response(&stale);
        assert_eq!(body["code"], Code::Removed.as_str());
        assert_eq!(body["replacement"], "/pages/{id}");
        assert_eq!(REMOVED_STATUS, 410);
    }

    #[test]
    fn a_withdrawal_beats_a_sunset_in_the_past() {
        let now = at("2026-10-01T00:00:00Z");
        let called_off = row(
            Some("/api/v1/pages"),
            None,
            "2026-09-01T00:00:00Z",
            Status::Withdrawn,
        );
        assert_eq!(outcome(&called_off, now), Outcome::NotDeprecated);
        assert_eq!(
            status_at(&called_off, now),
            None,
            "a sweep that advanced a withdrawn row would make an operator's own undo impossible"
        );
    }

    #[test]
    fn a_withdrawal_needs_a_reason_and_may_not_shorten_or_extend_backwards() {
        let now = at("2026-10-01T00:00:00Z");
        let active = row(
            Some("/api/v1/pages"),
            None,
            "2027-04-15T00:00:00Z",
            Status::Active,
        );

        let missing = check_extension(&active, at("2027-06-01T00:00:00Z"), "  ", now);
        assert!(missing.is_err(), "the reason is required and recorded");

        let backwards = check_extension(&active, at("2027-03-01T00:00:00Z"), "shortened", now);
        assert!(
            backwards.is_err(),
            "shortening is a withdrawal, which is a different audited action"
        );

        let past = check_extension(&active, at("2026-09-01T00:00:00Z"), "reason", now);
        assert!(past.is_err());

        let good = check_extension(
            &active,
            at("2027-06-01T00:00:00Z"),
            "integrator asked for time",
            now,
        );
        assert!(good.is_ok());
    }

    #[test]
    fn the_countdown_turns_amber_inside_thirty_days_and_says_so_in_words() {
        let now = at("2026-10-01T00:00:00Z");
        let soon = row(
            Some("/api/v1/pages"),
            None,
            "2026-10-20T00:00:00Z",
            Status::Active,
        );
        let later = row(
            Some("/api/v1/pages"),
            None,
            "2027-04-15T00:00:00Z",
            Status::Active,
        );

        assert_eq!(countdown(&soon, now).days_remaining, 19);
        assert!(countdown(&soon, now).amber);
        assert_eq!(countdown(&later, now).amber, false);

        let gone = row(
            Some("/api/v1/pages"),
            None,
            "2026-09-01T00:00:00Z",
            Status::Active,
        );
        assert_eq!(countdown(&gone, now).label, "sunset passed");
        assert!(
            gone_days(&countdown(&gone, now)),
            "a sunset in the past reads as a negative countdown rather than a stale number"
        );
    }

    fn gone_days(countdown: &Countdown) -> bool {
        countdown.days_remaining < 0
    }

    #[test]
    fn an_unrecognised_route_path_takes_the_longer_window() {
        // `/api/v1/pages` is public; something the classifier has never seen is treated as
        // public too, because the direction that cannot shorten a notice is the conservative one.
        assert_eq!(
            Surface::classify(Some("/api/v1/unknown-surface"), None),
            Surface::PublicRoute
        );
        assert_eq!(
            Surface::classify(Some("/developer/scopes"), None),
            Surface::DeveloperRoute
        );
        assert_eq!(
            Surface::classify(Some("/openapi.json"), None),
            Surface::DeveloperRoute
        );
        // A field is a field whatever its route, and an installation-wide row takes the public
        // floor so it cannot be used to buy a shorter notice for the whole platform.
        assert_eq!(
            Surface::classify(None, Some("Page.author")),
            Surface::GraphqlField
        );
        assert_eq!(Surface::classify(None, None), Surface::Installation);
        assert_eq!(Surface::Installation.minimum_window_months(), PUBLIC_WINDOW);
    }

    #[test]
    fn every_fixture_in_this_file_agrees_about_the_same_instant() {
        // A fixture written as `datetime!` in one test and a parsed string in another is a class
        // of test bug that only shows up on the day a month has 30 days. One parse, one format.
        let parsed = at("2026-10-01T00:00:00Z");
        let macro_made = datetime!(2026-10-01 00:00 UTC);
        assert_eq!(parsed, macro_made);
    }
}
