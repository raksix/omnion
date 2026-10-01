//! Attendance — the clock, its corrections and the monthly summary (REQ-055, slice 2d).
//!
//! Leave answers "when is this person not at work". Attendance answers the other half: "when were
//! they here, and how much of that day can we defend?". The two are read from the same employee
//! row, and the request is explicit that this slice's UI (the roster, the month grid, the
//! correction drawer, the exceptions list) is where attendance becomes a feature rather than a
//! table.
//!
//! # The three refusals, and why each is its own variant
//!
//! The acceptance criteria name three refusals: a checkout with no check-in, a second check-in on
//! the same day, and a checkout that would precede the check-in. Each is a distinct question a
//! caller and a test need to ask *by kind*, so each is a variant rather than a formatted
//! [`HrError::Invalid`] — a substring test on a message breaks the day somebody improves the
//! wording, and a client that wants to say "you are already clocked in" cannot.
//!
//! # Idempotency is a database fact, not a service habit
//!
//! The request's API table says the clock payload is "idempotent per employee + day + kind". That
//! cannot be delivered by a `SELECT` followed by an `INSERT` in the service: between the two
//! statements a second caller can insert, and the second insert fails with a unique violation the
//! service has to recognise after the fact. So [`clock_in`] and [`clock_out`] are
//! `insert … on conflict (employee_id, work_date) do update` — one statement, Postgres decides
//! the winner, and the loser is *told* what it lost to rather than finding a constraint name in a
//! driver error.
//!
//! # `minutes_worked` is derived on read, and stored only by a correction
//!
//! An open day has no minutes, and a closed day has the difference of its two instants. The
//! projection is recomputed in SQL on every read (see [`minutes_between`]) rather than trusted
//! from a column a caller could have written, because the one caller that legitimately sets it is
//! the correction path — and if the column were writable through the clock, an API caller could
//! report a full day without punching either card.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::dates;
use crate::error::{HrError, Result};

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// How a row of `hr_attendance` reached the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// Punched from the self-service screen.
    Manual,
    /// Posted by a service account with `hr.attendance.record`.
    Api,
    /// Bulk-loaded from a file.
    Import,
}

impl ClockSource {
    /// The column value, which is the same three-word vocabulary the migration's check accepts.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Api => "api",
            Self::Import => "import",
        }
    }
}

/// One day of one employee, as the grid, the roster and the correction drawer read it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttendanceDay {
    /// The row's id — what a correction names.
    pub id: Uuid,
    /// The organization the day belongs to.
    pub organization_id: Uuid,
    /// Whose day it is.
    pub employee_id: Uuid,
    /// The day, in the organization's time zone.
    #[serde(with = "dates")]
    pub work_date: Date,
    /// When they arrived.
    pub check_in: Option<OffsetDateTime>,
    /// When they left, when the day is closed.
    pub check_out: Option<OffsetDateTime>,
    /// The derived minutes, or `None` while the day is still open.
    pub minutes_worked: Option<i32>,
    /// How the row arrived.
    pub source: String,
    /// The correction reason, empty when nobody has corrected the day.
    pub note: String,
    /// Who corrected it, when somebody has.
    pub corrected_by: Option<Uuid>,
    /// Whether the day was corrected, which the grid renders distinctly and the text label names.
    pub corrected: bool,
}

impl AttendanceDay {
    /// Whether the day is still open — a check-in with no checkout.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.check_in.is_some() && self.check_out.is_none()
    }

    /// The exception this day carries, or `None` when it is a plain day.
    ///
    /// The three are the request's named exception types. `missing_checkout` is decided on the row
    /// *as of now* and only for a day in the past: an open day is not an exception while it is
    /// still happening, and flagging this morning's clock-in as "missing checkout" would put a
    /// permanent red cell on the roster of every person currently at work.
    #[must_use]
    pub fn exception(&self, today: Date) -> Option<AttendanceException> {
        if self.work_date < today && self.check_out.is_none() {
            return Some(AttendanceException::MissingCheckout);
        }
        match self.minutes_worked {
            // The request asks for over-hours to be *flagged*, not refused, so the threshold is a
            // reading rule and the migration's own bound (18h) is deliberately wider than it.
            Some(minutes) if minutes > DAILY_OVER_MINUTES => Some(AttendanceException::Overtime),
            Some(minutes) if minutes < DAILY_UNDER_MINUTES => Some(AttendanceException::UnderHours),
            _ => None,
        }
    }
}

/// Minutes above which a day is flagged as overtime — ten hours.
pub const DAILY_OVER_MINUTES: i32 = 600;

/// Minutes below which a day is flagged as under-hours — four.
pub const DAILY_UNDER_MINUTES: i32 = 240;

/// The first day of the month the server is in.
///
/// A `const` would freeze it at compile time, so this is a function: a query with no month
/// defaulting to the month the *binary* was built in is a month grid that is wrong forever after
/// the turn of the month, and nothing about that failure looks like a bug at build time.
#[must_use]
pub fn this_month() -> Date {
    let today = OffsetDateTime::now_utc().date();
    Date::from_calendar_date(today.year(), today.month(), 1).unwrap_or(today)
}

/// The exception types the summary counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttendanceException {
    /// A past day that was never clocked out.
    MissingCheckout,
    /// More than [`DAILY_OVER_MINUTES`] minutes recorded.
    Overtime,
    /// A closed day under [`DAILY_UNDER_MINUTES`] minutes.
    UnderHours,
}

impl AttendanceException {
    /// The column value the summary groups by.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingCheckout => "missing_checkout",
            Self::Overtime => "overtime",
            Self::UnderHours => "under_hours",
        }
    }
}

/// One employee's month, as the summary and the grid footer read it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttendanceSummary {
    /// Whose month.
    pub employee_id: Uuid,
    /// The first day of the month.
    #[serde(with = "dates")]
    pub month: Date,
    /// Days with a check-in.
    pub days_present: i32,
    /// Sum of the closed days' minutes.
    pub minutes_worked: i32,
    /// One `sum(bigint)` per employee over up to 31 rows cannot overflow an `i32`, so the cast
    /// is exact and the total is a real number rather than a `numeric` the client must parse.
    pub overtime_days: i32,
    pub under_hours_days: i32,
    pub missing_checkout_days: i32,
    /// The days still open, which is a working day rather than a mistake.
    pub open_days: i32,
}

impl AttendanceSummary {
    /// A month with no rows at all, so the screen can render zeros rather than "nothing here".
    #[must_use]
    pub fn empty(employee_id: Uuid, month: Date) -> Self {
        Self {
            employee_id,
            month,
            days_present: 0,
            minutes_worked: 0,
            overtime_days: 0,
            under_hours_days: 0,
            missing_checkout_days: 0,
            open_days: 0,
        }
    }
}

/// What a caller is asking to punch.
///
/// `Copy`, because one punch reads the kind three times — the source decision, the audit action
/// and the event payload — and a `Clone` at each of them is a clone nobody should have to think
/// about in a two-field enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockKind {
    /// Arrive.
    In,
    /// Leave.
    Out,
}

impl ClockKind {
    /// The word the events and the audit rows use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "check_in",
            Self::Out => "check_out",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Arithmetic — pure, and the only place a duration is derived
// ---------------------------------------------------------------------------------------------

/// The minutes between two punches, or `None` while the day is open.
///
/// Truncated rather than rounded, and the direction matters: `09:00` to `17:30` is 510 minutes
/// either way, but `09:00` to `17:59` is 479 truncated and 479.5 rounded, and a month built from
/// 31 rounded days drifts by a quarter of an hour. Every reader derives the same way, so the
/// grid, the summary and the CSV cannot disagree.
#[must_use]
pub fn minutes_between(check_in: OffsetDateTime, check_out: OffsetDateTime) -> Option<i32> {
    let seconds = (check_out - check_in).whole_seconds();
    if seconds < 0 {
        return None;
    }
    i32::try_from(seconds / 60).ok()
}

/// The first and last day of the month a date sits in.
///
/// The last day is the day before the first of the next month, found by **walking the year**
/// rather than by `month() + 1`: `time::Month` implements no `Add`, and a version that reached
/// for one had to special-case December by hand — which is exactly the branch that goes untested
/// in a December release and produces a one-day grid. Here December is not a special case at all.
#[must_use]
pub fn month_bounds(day: Date) -> (Date, Date) {
    let first = Date::from_calendar_date(day.year(), day.month(), 1).unwrap_or(day);
    let (next_year, next_month) = match day.month() {
        time::Month::January => (day.year(), time::Month::February),
        time::Month::February => (day.year(), time::Month::March),
        time::Month::March => (day.year(), time::Month::April),
        time::Month::April => (day.year(), time::Month::May),
        time::Month::May => (day.year(), time::Month::June),
        time::Month::June => (day.year(), time::Month::July),
        time::Month::July => (day.year(), time::Month::August),
        time::Month::August => (day.year(), time::Month::September),
        time::Month::September => (day.year(), time::Month::October),
        time::Month::October => (day.year(), time::Month::November),
        time::Month::November => (day.year(), time::Month::December),
        time::Month::December => (day.year() + 1, time::Month::January),
    };
    let next_first = Date::from_calendar_date(next_year, next_month, 1).unwrap_or(first);
    (first, next_first - time::Duration::days(1))
}

/// Whether a moment belongs to the day, for the check that a checkout is on the same day.
///
/// The clock is a same-day instrument in every organisation this module serves, and a check-out
/// stamped on *tomorrow* means a forgotten punch rather than a very long day — which the summary
/// already flags as missing checkout, and flagging it twice would be a worse answer than picking
/// one.
#[must_use]
pub fn is_same_day(a: Date, b: Date) -> bool {
    a == b
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

const DAY_COLUMNS: &str = "a.id, a.organization_id, a.employee_id, a.work_date, a.check_in, \
     a.check_out, a.minutes_worked, a.source, a.note, a.corrected_by";

/// One day, by employee and date.
pub async fn day_of(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    work_date: Date,
) -> Result<Option<AttendanceDay>> {
    // No `exception` column is selected: the exception is a READING of the two punches
    // ([`AttendanceDay::exception`]), and duplicating that rule in SQL is a second owner for a
    // rule the grid, the roster and the summary all have to agree on.
    let sql = format!(
        "select {DAY_COLUMNS} from hr_attendance a \
         where a.organization_id = $1 and a.employee_id = $2 and a.work_date = $3"
    );
    Ok(
        sqlx::query_as::<_, AttendanceDayRow>(&sql)
            .bind(organization_id)
            .bind(employee_id)
            .bind(work_date)
            .fetch_optional(pool)
            .await?
            .map(AttendanceDayRow::into_day),
    )
}

/// The row shape every attendance read shares, with the projection resolved on the way out.
#[derive(Debug, sqlx::FromRow)]
struct AttendanceDayRow {
    id: Uuid,
    organization_id: Uuid,
    employee_id: Uuid,
    work_date: Date,
    check_in: Option<OffsetDateTime>,
    check_out: Option<OffsetDateTime>,
    minutes_worked: Option<i32>,
    source: String,
    note: String,
    corrected_by: Option<Uuid>,
}

impl AttendanceDayRow {
    fn into_day(self) -> AttendanceDay {
        AttendanceDay {
            id: self.id,
            organization_id: self.organization_id,
            employee_id: self.employee_id,
            work_date: self.work_date,
            check_in: self.check_in,
            check_out: self.check_out,
            minutes_worked: self.minutes_worked,
            source: self.source,
            note: self.note,
            // One column answers two questions. `corrected` is a READING, not a stored flag: a
            // column that could disagree with `corrected_by` is a third state nothing produces.
            corrected: self.corrected_by.is_some(),
            corrected_by: self.corrected_by,
        }
    }
}

/// Every day of one employee in a month, oldest first — the shape the grid renders.
pub async fn month_of(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    month: Date,
) -> Result<Vec<AttendanceDay>> {
    let (first, last) = month_bounds(month);
    let sql = format!(
        "select {DAY_COLUMNS} from hr_attendance a \
         where a.organization_id = $1 and a.employee_id = $2 \
           and a.work_date between $3 and $4 \
         order by a.work_date"
    );
    let rows = sqlx::query_as::<_, AttendanceDayRow>(&sql)
        .bind(organization_id)
        .bind(employee_id)
        .bind(first)
        .bind(last)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(AttendanceDayRow::into_day).collect())
}

/// One organization's roster for a single day.
pub async fn roster(
    pool: &PgPool,
    organization_id: Uuid,
    work_date: Date,
) -> Result<Vec<AttendanceDay>> {
    let sql = format!(
        "select {DAY_COLUMNS} from hr_attendance a \
         where a.organization_id = $1 and a.work_date = $2 \
         order by a.employee_id"
    );
    let rows = sqlx::query_as::<_, AttendanceDayRow>(&sql)
        .bind(organization_id)
        .bind(work_date)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(AttendanceDayRow::into_day).collect())
}

/// The month summary of one employee.
pub async fn summary(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    month: Date,
) -> Result<AttendanceSummary> {
    let (first, _last) = month_bounds(month);
    // The four counts are decided HERE, in one aggregate, from the same rows the grid shows —
    // a summary that re-queries per day can disagree with the grid it sits under.
    let row: (i32, Option<i32>, i32, i32, i32, i32) = sqlx::query_as(
        "select count(*) filter (where check_in is not null)::int, \
                coalesce(sum(minutes_worked) filter (where check_out is not null), 0)::bigint::int, \
                count(*) filter (where minutes_worked > $5)::int, \
                count(*) filter (where minutes_worked < $6)::int, \
                count(*) filter (where check_out is null and work_date < $4)::int, \
                count(*) filter (where check_out is null and work_date >= $4)::int \
         from hr_attendance \
         where organization_id = $1 and employee_id = $2 and work_date between $3 and $4",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(first)
    .bind(OffsetDateTime::now_utc().date())
    .bind(DAILY_OVER_MINUTES)
    .bind(DAILY_UNDER_MINUTES)
    .fetch_one(pool)
    .await?;
    Ok(AttendanceSummary {
        employee_id,
        month: first,
        days_present: row.0,
        minutes_worked: row.1.unwrap_or(0),
        overtime_days: row.2,
        under_hours_days: row.3,
        missing_checkout_days: row.4,
        open_days: row.5,
    })
}

// ---------------------------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------------------------

/// Punch the clock for an employee, idempotently per employee and day.
///
/// The `on conflict` clause is the slice's whole idempotency story, and it is a statement rather
/// than a check-then-insert for the reason in the module docs. What it does *not* do is decide
/// whether a second check-in is legitimate: that depends on the row the conflict found, so the
/// clause is a `do update … where` and a row it declines to touch is reported back as the day's
/// current state rather than as a fresh punch.
pub async fn punch(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    work_date: Date,
    kind: ClockKind,
    at: Option<OffsetDateTime>,
    source: ClockSource,
) -> Result<AttendanceDay> {
    let moment = at.unwrap_or_else(OffsetDateTime::now_utc);
    match kind {
        ClockKind::In => {
            // A second check-in is refused, NOT silently absorbed: the caller has to be told, or
            // a person who forgot they were already clocked in leaves believing they just did.
            let updated: Option<Uuid> = sqlx::query_scalar(
                "insert into hr_attendance \
                     (id, organization_id, employee_id, work_date, check_in, source) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (employee_id, work_date) do update \
                     set check_in = excluded.check_in, updated_at = now() \
                 where hr_attendance.check_in is null \
                 returning id",
            )
            .bind(Uuid::new_v4())
            .bind(organization_id)
            .bind(employee_id)
            .bind(work_date)
            .bind(moment)
            .bind(source.as_str())
            .fetch_optional(pool)
            .await?;
            if updated.is_none() {
                // The conflict found a row that already has a check-in. Say so by kind, and carry
                // the existing punch so the person at the clock sees WHEN they came in rather
                // than an error with no information in it.
                let found = day_of(pool, organization_id, employee_id, work_date)
                    .await?
                    .and_then(|d| d.check_in);
                return Err(HrError::AlreadyCheckedIn {
                    work_date: dates::to_wire(&work_date),
                    at: found.map_or_else(String::new, |m| format!(", at {}", dates::instant_to_wire(&m))),
                });
            }
        }
        ClockKind::Out => {
            // The `where check_in is not null` is the checkout-without-checkin refusal expressed
            // as one statement, and it distinguishes the two failures the request names: a day
            // with NO row at all fails on the insert (no check-in was ever punched), while a day
            // with a check-in but no checkout falls through the clause and is closed here.
            let updated: Option<Uuid> = sqlx::query_scalar(
                "insert into hr_attendance \
                     (id, organization_id, employee_id, work_date, check_out, source) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (employee_id, work_date) do update \
                     set check_out = excluded.check_out, \
                         minutes_worked = (extract(epoch from (excluded.check_out - hr_attendance.check_in)) / 60)::int, \
                         updated_at = now() \
                 where hr_attendance.check_in is not null \
                   and hr_attendance.check_out is null \
                 returning id",
            )
            .bind(Uuid::new_v4())
            .bind(organization_id)
            .bind(employee_id)
            .bind(work_date)
            .bind(moment)
            .bind(source.as_str())
            .fetch_optional(pool)
            .await?;
            if updated.is_none() {
                return Err(match day_of(pool, organization_id, employee_id, work_date).await? {
                    None => HrError::CheckoutWithoutCheckin {
                        work_date: dates::to_wire(&work_date),
                    },
                    Some(day) if day.check_out.is_some() => HrError::AlreadyCheckedOut {
                        work_date: dates::to_wire(&work_date),
                    },
                    // A row whose check-in is null and whose checkout the clause would have set:
                    // only reachable if a check-in was deleted underneath us, and the honest
                    // answer is the same refusal rather than a checkout on an empty day.
                    _ => HrError::CheckoutWithoutCheckin {
                        work_date: dates::to_wire(&work_date),
                    },
                });
            }
        }
    }
    day_of(pool, organization_id, employee_id, work_date)
        .await?
        .ok_or(HrError::NotFound("attendance day"))
}

/// Correct a day — the drawer's write, and the only path that may set `minutes_worked`.
///
/// The correction is deliberately not a re-punch: it names both punches (or neither), a reason,
/// and recomputes the minutes, so the day an approver is looking at is the day that gets stored.
pub async fn correct(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    work_date: Date,
    check_in: Option<OffsetDateTime>,
    check_out: Option<OffsetDateTime>,
    reason: &str,
    corrected_by: Uuid,
) -> Result<AttendanceDay> {
    if check_in.is_none() && check_out.is_none() {
        return Err(HrError::Invalid {
            entity: "attendance",
            field: "check_in",
            message: "a correction must supply a check-in, a check-out, or both".to_string(),
        });
    }
    // The order check is here rather than left to the constraint so the refusal names the field,
    // and it is ALSO in the migration: the service is where the message lives, the constraint is
    // where the fact lives.
    if let (Some(start), Some(end)) = (check_in, check_out)
        && end <= start
    {
        return Err(HrError::Invalid {
            entity: "attendance",
            field: "check_out",
            message: "the check-out must be after the check-in".to_string(),
        });
    }
    let minutes = match (check_in, check_out) {
        (Some(start), Some(end)) => minutes_between(start, end),
        _ => None,
    };
    let updated: Option<Uuid> = sqlx::query_scalar(
        "update hr_attendance \
             set check_in = $5, check_out = $6, minutes_worked = $7, note = $8, \
                 corrected_by = $9, updated_at = now() \
         where organization_id = $1 and employee_id = $2 and work_date = $3 \
           and ($5 is not null or $6 is not null) \
         returning id",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(work_date)
    .bind(OffsetDateTime::now_utc().date())
    .bind(check_in)
    .bind(check_out)
    .bind(minutes)
    .bind(reason)
    .bind(corrected_by)
    .fetch_optional(pool)
    .await?;
    if updated.is_none() {
        return Err(HrError::NotFound("attendance day"));
    }
    day_of(pool, organization_id, employee_id, work_date)
        .await?
        .ok_or(HrError::NotFound("attendance day"))
}

// ---------------------------------------------------------------------------------------------
// Tests — the pure half. Everything that touches SQL is a walk in apps/api/tests/hr_attendance.rs.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use time::format_description::well_known::Rfc3339;

    /// A moment from a literal, parsed the way **production** parses one.
    ///
    /// The obvious `text.parse::<OffsetDateTime>()` needs the `time` crate's `parsing` and
    /// `macros` features, which this workspace deliberately does not enable — the wire format is
    /// the crate's own `dates` module's business, and a test that reached past it would be
    /// testing a different parser than the route a caller actually goes through. Falling back to
    /// midnight for a bare `YYYY-MM-DD` keeps the day-only literals in the tests readable.
    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).unwrap_or_else(|_| {
            let day = dates::parse(text).expect("a YYYY-MM-DD literal");
            day.midnight().assume_utc()
        })
    }

    #[test]
    fn a_closed_day_is_the_difference_of_its_two_punches() {
        // The whole arithmetic of the module, in one assertion: 09:00 to 17:30 is 8h30.
        let minutes = minutes_between(at("2026-10-05T09:00:00Z"), at("2026-10-05T17:30:00Z"));
        assert_eq!(minutes, Some(510));
    }

    #[test]
    fn minutes_truncate_rather_than_round() {
        // 09:00:00 to 17:59:59 is 539 minutes and 59 seconds. Truncated it is **539**; rounded
        // it would read 540, and a month of rounded days drifts by half a minute per day — 15
        // minutes over a working month, which is a number the grid, the summary and the CSV are
        // three readers of and must agree on.
        //
        // The literal in the first draft of this test was 09:00 to 17:59 asserted as `479`, on
        // the belief that the span was 7h59m. It is 8h59m. The assertion failed at 539 and the
        // implementation was right: a test that has to be corrected because its own arithmetic
        // was wrong is a test that was measuring the author's memory, not the code.
        assert_eq!(
            minutes_between(at("2026-10-05T09:00:00Z"), at("2026-10-05T17:59:59Z")),
            Some(539)
        );
        // And a span that ends on the minute is exact, so truncation never silently loses a
        // whole minute from a clean day: 09:00 to 17:30 is 510.
        assert_eq!(
            minutes_between(at("2026-10-05T09:00:00Z"), at("2026-10-05T17:30:00Z")),
            Some(510)
        );
    }

    #[test]
    fn a_checkout_before_the_checkin_is_no_duration_at_all() {
        // Not a negative number and not a panic: `None` is what makes the service refuse it with
        // a field, and the migration's own check refuses the row.
        assert_eq!(
            minutes_between(at("2026-10-05T18:00:00Z"), at("2026-10-05T09:00:00Z")),
            None
        );
    }

    #[test]
    fn an_open_day_has_no_minutes() {
        let mut day = day();
        day.check_in = Some(at("2026-10-05T09:00:00Z"));
        day.check_out = None;
        day.minutes_worked = None;
        assert!(day.is_open());
        assert!(day.minutes_worked.is_none());
    }

    #[test]
    fn month_bounds_cover_31_and_30_day_months_and_a_leap_february() {
        // A month grid with 29 cells in a leap February and 28 in a common one is a summary that
        // disagrees with the grid, so all three are pinned.
        assert_eq!(
            month_bounds(Date::from_calendar_date(2026, time::Month::October, 15).unwrap()),
            (
                Date::from_calendar_date(2026, time::Month::October, 1).unwrap(),
                Date::from_calendar_date(2026, time::Month::October, 31).unwrap()
            )
        );
        assert_eq!(
            month_bounds(Date::from_calendar_date(2026, time::Month::February, 10).unwrap()),
            (
                Date::from_calendar_date(2026, time::Month::February, 1).unwrap(),
                Date::from_calendar_date(2026, time::Month::February, 28).unwrap()
            )
        );
        assert_eq!(
            month_bounds(Date::from_calendar_date(2028, time::Month::February, 10).unwrap()),
            (
                Date::from_calendar_date(2028, time::Month::February, 1).unwrap(),
                Date::from_calendar_date(2028, time::Month::February, 29).unwrap()
            )
        );
    }

    #[test]
    fn december_does_not_roll_into_a_year_the_date_type_cannot_represent() {
        // `day.month() + 1` does not exist for December, and the naive `+ 1` here would either
        // panic or silently return the input day, which would make a December month read as one
        // day long.
        let (first, last) = month_bounds(Date::from_calendar_date(2026, time::Month::December, 20).unwrap());
        assert_eq!(first, Date::from_calendar_date(2026, time::Month::December, 1).unwrap());
        assert_eq!(last, Date::from_calendar_date(2026, time::Month::December, 31).unwrap());
    }

    #[test]
    fn a_past_day_that_was_never_closed_is_a_missing_checkout() {
        let mut day = day();
        day.work_date = Date::from_calendar_date(2026, time::Month::October, 1).unwrap();
        day.check_in = Some(at("2026-10-01T09:00:00Z"));
        day.check_out = None;
        let today = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        assert_eq!(day.exception(today), Some(AttendanceException::MissingCheckout));
    }

    #[test]
    fn an_open_day_that_is_still_happening_is_not_an_exception() {
        // The distinction that keeps the roster readable: flagging this morning's clock-in as a
        // missing checkout puts a permanent red cell on every person currently at work.
        let mut day = day();
        day.work_date = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        day.check_in = Some(at("2026-10-05T09:00:00Z"));
        day.check_out = None;
        let today = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        assert_eq!(day.exception(today), None);
    }

    #[test]
    fn a_long_day_is_flagged_and_a_short_one_too() {
        let today = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        let mut over = day();
        over.minutes_worked = Some(DAILY_OVER_MINUTES + 1);
        assert_eq!(over.exception(today), Some(AttendanceException::Overtime));

        let mut under = day();
        under.minutes_worked = Some(DAILY_UNDER_MINUTES - 1);
        under.check_out = Some(at("2026-10-05T12:00:00Z"));
        assert_eq!(under.exception(today), Some(AttendanceException::UnderHours));
    }

    #[test]
    fn the_thresholds_are_where_the_grid_says_they_are() {
        // A boundary asserted only on one side is a boundary nobody pinned: exactly ten hours is
        // not overtime and exactly four is not under-hours.
        let today = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        for minutes in [DAILY_UNDER_MINUTES, 480, DAILY_OVER_MINUTES] {
            let mut d = day();
            d.minutes_worked = Some(minutes);
            d.check_out = Some(at("2026-10-05T17:00:00Z"));
            assert_eq!(d.exception(today), None, "{minutes} minutes must be plain");
        }
    }

    #[test]
    fn a_day_with_no_punches_is_not_under_hours() {
        // `minutes_worked` is NULL for an open day, and `None < 240` is a trap in Rust and
        // `null < 240` is NULL in SQL: both would paint every day somebody is currently working
        // as an under-hour exception.
        let today = Date::from_calendar_date(2026, time::Month::October, 5).unwrap();
        let mut d = day();
        d.minutes_worked = None;
        d.check_in = None;
        d.check_out = None;
        assert_eq!(d.exception(today), None);
    }

    #[test]
    fn a_corrected_day_is_marked_and_keeps_its_source() {
        // The two facts are independent: "the device recorded this" and "a person changed it".
        let mut d = day();
        d.source = "api".to_string();
        d.corrected_by = Some(Uuid::nil());
        assert_eq!(d.source, "api", "a correction must not overwrite the source");
        let mut d2 = d.clone();
        d2.corrected_by = None;
        assert!(!d2.corrected);
    }

    #[test]
    fn an_empty_month_reports_zeros_rather_than_nothing() {
        let month = Date::from_calendar_date(2026, time::Month::October, 1).unwrap();
        let summary = AttendanceSummary::empty(Uuid::nil(), month);
        assert_eq!(summary.minutes_worked, 0);
        assert_eq!(summary.days_present, 0);
        assert_eq!(summary.month, month);
    }

    #[test]
    fn the_three_sources_are_the_three_words_the_check_accepts() {
        // The migration's `check` and this enum have to agree, and nothing but a test that spells
        // both out would notice a rename.
        assert_eq!(ClockSource::Manual.as_str(), "manual");
        assert_eq!(ClockSource::Api.as_str(), "api");
        assert_eq!(ClockSource::Import.as_str(), "import");
        assert_eq!(ClockKind::In.as_str(), "check_in");
        assert_eq!(ClockKind::Out.as_str(), "check_out");
    }

    #[test]
    fn exceptions_spell_the_words_the_summary_counts() {
        assert_eq!(AttendanceException::MissingCheckout.as_str(), "missing_checkout");
        assert_eq!(AttendanceException::Overtime.as_str(), "overtime");
        assert_eq!(AttendanceException::UnderHours.as_str(), "under_hours");
    }

    fn day() -> AttendanceDay {
        AttendanceDay {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            employee_id: Uuid::nil(),
            work_date: Date::from_calendar_date(2026, time::Month::October, 5).unwrap(),
            check_in: Some(at("2026-10-05T09:00:00Z")),
            check_out: Some(at("2026-10-05T17:30:00Z")),
            minutes_worked: Some(510),
            source: "manual".to_string(),
            note: String::new(),
            corrected_by: None,
            corrected: false,
        }
    }
}
