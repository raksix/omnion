//! The cron subset the scheduler understands.
//!
//! Five fields — minute, hour, day of month, month, day of week — evaluated in **UTC**, with
//! `*`, `n`, `a-b`, `a,b,c`, `*/step` and `a-b/step`; month and weekday names (`jan`, `mon`, …)
//! are accepted as well. Day-of-month and day-of-week follow the classic rule: when *both* are
//! restricted a day matches if either one matches.
//!
//! The engine ships its own small parser on purpose: the schedule of a workflow is a stored
//! string, and the only operations the engine needs are "is this expression valid" and "when
//! does it run next" — both answered here without a dependency, in the same UTC clock the
//! database runs on.

use time::{Date, Duration, OffsetDateTime, Time, Weekday};

use crate::error::{Result, WorkflowError};

/// How far ahead the search for the next run looks before giving up.
const MAX_SEARCH_DAYS: i64 = 366 * 4 + 1;

/// Month names accepted in the month field.
const MONTH_NAMES: &[(&str, u8)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

/// Weekday names accepted in the day-of-week field (`7` is Sunday too).
const WEEKDAY_NAMES: &[(&str, u8)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// A parsed five-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<u8>,
    hours: Vec<u8>,
    days: Vec<u8>,
    months: Vec<u8>,
    weekdays: Vec<u8>,
    /// `true` when the field was `*` (needed for the day-of-month/day-of-week rule).
    day_wildcard: bool,
    /// `true` when the day-of-week field was `*`.
    weekday_wildcard: bool,
    /// The expression as it was written.
    expression: String,
}

impl CronSchedule {
    /// Parse an expression, or answer `invalid_cron`.
    pub fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(WorkflowError::invalid(
                "invalid_cron",
                format!(
                    "a schedule is five fields (minute hour day-of-month month day-of-week), \
                     got {}: {expression:?}",
                    fields.len()
                ),
            ));
        }

        let minutes = parse_field(fields[0], 0, 59, &[], "minute")?;
        let hours = parse_field(fields[1], 0, 23, &[], "hour")?;
        let days = parse_field(fields[2], 1, 31, &[], "day of month")?;
        let months = parse_field(fields[3], 1, 12, MONTH_NAMES, "month")?;
        let weekdays = parse_field(fields[4], 0, 7, WEEKDAY_NAMES, "day of week")?
            .into_iter()
            .map(|value| if value == 7 { 0 } else { value })
            .collect::<Vec<u8>>();

        let mut normalized_weekdays = weekdays;
        normalized_weekdays.sort_unstable();
        normalized_weekdays.dedup();

        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays: normalized_weekdays,
            day_wildcard: fields[2].trim() == "*",
            weekday_wildcard: fields[4].trim() == "*",
            expression: expression.trim().to_owned(),
        })
    }

    /// The expression this schedule was parsed from.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// First run strictly after `from`.
    pub fn next_after(&self, from: OffsetDateTime) -> Result<OffsetDateTime> {
        // Cron works in whole minutes; start at the next minute boundary.
        let start = (from + Duration::minutes(1))
            .replace_nanosecond(0)
            .and_then(|value| value.replace_second(0))
            .map_err(|_| self.unreachable())?;

        let mut date = start.date();
        for _ in 0..MAX_SEARCH_DAYS {
            if self.matches_day(date) {
                for hour in &self.hours {
                    for minute in &self.minutes {
                        let candidate = self.at(date, *hour, *minute)?;
                        if candidate <= from {
                            continue;
                        }
                        return Ok(candidate);
                    }
                }
            }
            date = match date.next_day() {
                Some(next) => next,
                None => return Err(self.unreachable()),
            };
        }

        Err(WorkflowError::invalid(
            "invalid_cron",
            format!("{} has no run inside the next four years", self.expression),
        ))
    }

    /// `true` when the date satisfies the month and day rules.
    fn matches_day(&self, date: Date) -> bool {
        if !self.months.contains(&(date.month() as u8)) {
            return false;
        }

        let day_of_month = self.days.contains(&date.day());
        let day_of_week = self.weekdays.contains(&weekday_index(date.weekday()));

        match (self.day_wildcard, self.weekday_wildcard) {
            (true, true) => true,
            (false, true) => day_of_month,
            (true, false) => day_of_week,
            // Classic behaviour: two restricted fields are a union, not an intersection.
            (false, false) => day_of_month || day_of_week,
        }
    }

    /// Build one UTC instant from a date and a time of day.
    fn at(&self, date: Date, hour: u8, minute: u8) -> Result<OffsetDateTime> {
        let time = Time::from_hms(hour, minute, 0).map_err(|_| self.unreachable())?;
        Ok(date.with_time(time).assume_utc())
    }

    /// A parsed-but-unrepresentable instant; the fields are range-checked at parse time, so
    /// this only fires on a calendar date that does not exist.
    fn unreachable(&self) -> WorkflowError {
        WorkflowError::invalid(
            "invalid_cron",
            format!("{} describes a moment that does not exist", self.expression),
        )
    }
}

/// `0` for Sunday … `6` for Saturday, the numbering the day-of-week field uses.
fn weekday_index(weekday: Weekday) -> u8 {
    weekday.number_days_from_sunday()
}

/// Parse one field into the sorted values it allows.
fn parse_field(
    field: &str,
    min: u8,
    max: u8,
    names: &[(&str, u8)],
    label: &str,
) -> Result<Vec<u8>> {
    let mut values: Vec<u8> = Vec::new();

    for item in field.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(bad_field(label, field));
        }

        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u8 = step.trim().parse().map_err(|_| bad_field(label, field))?;
                if step == 0 {
                    return Err(WorkflowError::invalid(
                        "invalid_cron",
                        format!("the {label} field of \"{field}\" has a zero step"),
                    ));
                }
                (range.trim(), step)
            }
            None => (item, 1),
        };

        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((left, right)) = range.split_once('-') {
            (
                value_of(left, min, max, names, label, field)?,
                value_of(right, min, max, names, label, field)?,
            )
        } else {
            let single = value_of(range, min, max, names, label, field)?;
            // `n/step` means "from n to the top of the range".
            if step > 1 {
                (single, max)
            } else {
                (single, single)
            }
        };

        if start > end {
            return Err(WorkflowError::invalid(
                "invalid_cron",
                format!("the {label} field of \"{field}\" runs backwards ({start}-{end})"),
            ));
        }

        let mut value = start;
        while value <= end {
            if !values.contains(&value) {
                values.push(value);
            }
            value = value.saturating_add(step);
        }
    }

    if values.is_empty() {
        return Err(bad_field(label, field));
    }
    values.sort_unstable();
    Ok(values)
}

/// One value of a field: a number, or a three-letter name where names are allowed.
fn value_of(
    raw: &str,
    min: u8,
    max: u8,
    names: &[(&str, u8)],
    label: &str,
    field: &str,
) -> Result<u8> {
    let raw = raw.trim();
    if let Ok(value) = raw.parse::<u8>() {
        if value < min || value > max {
            return Err(WorkflowError::invalid(
                "invalid_cron",
                format!("the {label} field of \"{field}\" takes {min}-{max}, got {value}"),
            ));
        }
        return Ok(value);
    }

    let lower = raw.to_ascii_lowercase();
    if let Some((_, value)) = names.iter().find(|(name, _)| *name == lower) {
        return Ok(*value);
    }

    Err(bad_field(label, field))
}

/// The generic "this field is not understandable" error.
fn bad_field(label: &str, field: &str) -> WorkflowError {
    WorkflowError::invalid(
        "invalid_cron",
        format!("the {label} field of \"{field}\" is not a valid cron field"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> OffsetDateTime {
        OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
            .expect("the test instant parses")
    }

    #[test]
    fn a_step_field_expands_to_its_values() {
        let schedule = CronSchedule::parse("*/15 * * * *").expect("valid");
        assert_eq!(schedule.minutes, vec![0, 15, 30, 45]);
        assert_eq!(schedule.hours.len(), 24);
        assert!(schedule.day_wildcard && schedule.weekday_wildcard);
    }

    #[test]
    fn lists_ranges_and_names_are_understood() {
        let schedule = CronSchedule::parse("5,45 9-10 * jan,mar mon-fri").expect("valid");
        assert_eq!(schedule.minutes, vec![5, 45]);
        assert_eq!(schedule.hours, vec![9, 10]);
        assert_eq!(schedule.months, vec![1, 3]);
        assert_eq!(schedule.weekdays, vec![1, 2, 3, 4, 5]);
        assert_eq!(schedule.expression(), "5,45 9-10 * jan,mar mon-fri");
    }

    #[test]
    fn sunday_is_zero_and_seven() {
        let schedule = CronSchedule::parse("0 0 * * 7").expect("valid");
        assert_eq!(schedule.weekdays, vec![0]);
        assert!(!schedule.weekday_wildcard);
    }

    #[test]
    fn the_next_run_is_strictly_later() {
        let schedule = CronSchedule::parse("*/15 * * * *").expect("valid");
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T00:07:00Z"))
                .expect("next"),
            at("2026-09-26T00:15:00Z")
        );
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T00:15:00Z"))
                .expect("next"),
            at("2026-09-26T00:30:00Z")
        );
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T23:59:00Z"))
                .expect("next"),
            at("2026-09-27T00:00:00Z")
        );
    }

    #[test]
    fn a_daily_run_slips_to_the_next_day() {
        let schedule = CronSchedule::parse("0 3 * * *").expect("valid");
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T01:00:00Z"))
                .expect("next"),
            at("2026-09-26T03:00:00Z")
        );
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T03:00:00Z"))
                .expect("next"),
            at("2026-09-27T03:00:00Z")
        );
    }

    #[test]
    fn a_weekday_schedule_lands_on_that_weekday() {
        // 2026-09-26 is a Saturday; the next Monday at 02:30 is 2026-09-28.
        let schedule = CronSchedule::parse("30 2 * * mon").expect("valid");
        let next = schedule
            .next_after(at("2026-09-26T00:00:00Z"))
            .expect("next");
        assert_eq!(next, at("2026-09-28T02:30:00Z"));
        assert_eq!(next.weekday(), Weekday::Monday);
    }

    #[test]
    fn a_monthly_schedule_lands_on_the_first() {
        let schedule = CronSchedule::parse("0 0 1 * *").expect("valid");
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T00:00:00Z"))
                .expect("next"),
            at("2026-10-01T00:00:00Z")
        );
    }

    #[test]
    fn two_restricted_day_fields_are_a_union() {
        // Both the 1st and every Monday match — the classic cron rule.
        let schedule = CronSchedule::parse("0 0 1 * mon").expect("valid");
        assert_eq!(
            schedule
                .next_after(at("2026-09-26T00:00:00Z"))
                .expect("next"),
            at("2026-09-28T00:00:00Z")
        );
        assert!(
            schedule.matches_day(
                Date::from_calendar_date(2026, time::Month::October, 1).expect("date")
            )
        );
    }

    #[test]
    fn broken_expressions_are_refused() {
        for broken in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "x * * * *",
            "5-1 * * * *",
        ] {
            let error =
                CronSchedule::parse(broken).expect_err(&format!("\"{broken}\" must be refused"));
            assert_eq!(error.code(), "invalid_cron", "{broken}");
        }
    }
}
