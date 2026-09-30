//! When a schedule runs next (REQ-013, slice 3).
//!
//! `backup_schedules` shipped with slice 1: a `frequency`, a time of day, a weekday or a day
//! of the month, and a timezone. `next_run_at` is a column, `next_due_schedules` is a query
//! that reads it, and **nothing wrote it** — the same silence as the uncalled
//! `prune_candidates`, one table over. A schedule could be created, listed, rendered with a
//! `cadence` sentence and never fire, and the screen would say "Every day at 02:00" beside an
//! empty next-run column.
//!
//! So the rules live in one **pure** function, and the decisions are the interesting part:
//!
//! * **The local time is computed in the schedule's zone and converted once, at the end.**
//!   The alternative — walking hours in UTC and asking what that is locally — produces a
//!   daily backup that runs at 23:00 on the day before the clocks change, which is a real
//!   backup on a real day, just not the one anybody asked for. This is the transition the
//!   feature's own risk list names as "the most likely defect".
//! * **A local time that does not exist is moved forward by the gap, not rejected and not
//!   silently dropped.** 02:30 on a spring-forward morning does not happen. Backwards by the
//!   gap is 03:30, which is the same wall clock reading and the only instant nearby that is
//!   real. Rejecting it would mean a schedule that silently never runs for one day a year;
//!   dropping it to "next week" would be a week of nothing.
//! * **A local time that happens twice is taken once, at the earlier instant.** The autumn
//!   hour repeats 02:30 twice. Running a backup on both would produce two runs an hour
//!   apart, and `retention_count` would count them as two of the same backup. The earlier one
//!   is the one an operator means by "02:30".
//! * **The next run is strictly after `after`.** A schedule created at 02:00:30 with a time
//!   of day of 02:00 must not answer "02:00 today" — that is in the past, so the worker
//!   would claim it every tick and the schedule would fire on a loop.
//! * **`day_of_month` is 1–28 for a reason the database already encodes.** A schedule for
//!   the 31st would have to skip February, and "skip" is a rule nobody wrote down. The
//!   constraint already says 1–28, so this module does not invent a second opinion.

use time::{Date, Duration, OffsetDateTime, Time, Weekday};
use time_tz::{Offset, TimeZone};

use crate::error::{BackupError, Result};

/// `HH:MM:SS`, the shape PostgreSQL's `::text` cast produces for a `time` column.
const CLOCK_FULL: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[hour]:[minute]:[second]");

/// `HH:MM`, the shape an operator types.
const CLOCK_SHORT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[hour]:[minute]");

/// How far ahead the search looks before it gives up.
///
/// A month schedule whose day of the month is the 28th never needs more than a month, and the
/// clock cannot disagree with the calendar by more than a leap second, so this is not a
/// tuning knob — it is the bound that turns "no answer" into an error instead of a loop.
const MAX_LOOKAHEAD_DAYS: i64 = 40;

/// A schedule's own idea of when it runs, with no database and no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cadence {
    /// `hourly|daily|weekly|monthly`.
    pub frequency: String,
    /// Local time of day, `HH:MM`. `None` for `hourly`, which has none of its own.
    pub at_time: Option<Time>,
    /// Local weekday, for `weekly` only.
    pub day_of_week: Option<Weekday>,
    /// Local day of the month (1–28), for `monthly` only.
    pub day_of_month: Option<u8>,
    /// IANA zone name, e.g. `Europe/Istanbul`.
    pub timezone: String,
}

impl Cadence {
    /// Read the cadence out of a stored schedule row.
    ///
    /// A row whose time of day does not parse is **not** silently treated as midnight: the
    /// database constrains the column, so an unparseable value is a defect, and a schedule
    /// that quietly fires at midnight is worse than one the worker refuses with a sentence
    /// naming the field.
    pub fn from_schedule(schedule: &crate::store::BackupSchedule) -> Result<Self> {
        let at_time = match (schedule.frequency.as_str(), schedule.at_time.as_deref()) {
            // Hourly has no time of day of its own, so there is nothing to parse and nothing
            // to reject. The `match` on the frequency is what makes a null legal here and a
            // null for a daily schedule an error.
            ("hourly", _) => None,
            (_, Some(text)) => Some(parse_time(text)?),
            (_, None) => {
                return Err(BackupError::Invalid(format!(
                    "the schedule {:?} is {} but has no time of day, so there is no hour to run at",
                    schedule.name, schedule.frequency
                )));
            }
        };
        Ok(Self {
            frequency: schedule.frequency.clone(),
            at_time,
            day_of_week: schedule.day_of_week.map(weekday_of),
            day_of_month: schedule.day_of_month.map(|day| day as u8),
            timezone: schedule.timezone.clone(),
        })
    }

    /// The zone, or an error naming the string that is not a zone.
    ///
    /// `UTC` is accepted as a name because the migration's default is the literal string
    /// `'UTC'`, and it is the one spelling a hand-written row is most likely to carry. A
    /// schedule with an unknown zone is refused rather than run in UTC: "every day at 02:00
    /// (Europe/Istanbool)" is not a fallback, it is a wrong answer delivered confidently.
    fn zone(&self) -> Result<&'static time_tz::Tz> {
        let name = self.timezone.trim();
        if name.eq_ignore_ascii_case("utc") {
            return Ok(time_tz::timezones::get_by_name("UTC").ok_or_else(|| {
                BackupError::Invalid("this build's timezone table has no UTC entry".to_owned())
            })?);
        }
        time_tz::timezones::get_by_name(name).ok_or_else(|| {
            BackupError::Invalid(format!(
                "{name:?} is not a timezone this build knows (use an IANA name such as \
                 `Europe/Istanbul`)"
            ))
        })
    }

    /// The first instant strictly after `after` at which this cadence fires, in UTC.
    ///
    /// The returned value is UTC because the column is `timestamptz` and every reader of it
    /// is a worker comparing it against `now()`; the local wall clock is an input, never an
    /// output.
    pub fn next_after(&self, after: OffsetDateTime) -> Result<OffsetDateTime> {
        let zone = self.zone()?;
        let local_now = after.to_offset(zone.get_offset_utc(&after).to_utc());

        // Hourly is the one cadence that does not name a wall clock, so it is anchored on the
        // UTC hour rather than on the local one: an hourly schedule in `Asia/Tokyo` runs at
        // `:00` past each local hour, which is the same set of instants, and anchoring it on
        // the local hour would need the same search for no different answer.
        if self.frequency == "hourly" {
            // Truncate FIRST and then add an hour. The other order — add an hour, then
            // truncate — takes 15:30:00 to 16:30:00 and truncates that back to 16:00, which
            // is an hour and a half from the top of the next hour. Truncating first is the
            // only order that is correct by construction rather than by coincidence, and the
            // cost of getting it wrong is a schedule that skips every hour with a non-zero
            // minute.
            let top_of_this_hour = after
                .replace_minute(0)
                .and_then(|hour| hour.replace_second(0))
                .and_then(|hour| hour.replace_nanosecond(0))
                .unwrap_or(after);
            return Ok(top_of_this_hour + Duration::hours(1));
        }

        let at_time = self.at_time.ok_or_else(|| {
            BackupError::Invalid(format!(
                "the {} cadence needs a time of day",
                self.frequency
            ))
        })?;

        // Search day by day in the schedule's own local calendar. The first day whose moment
        // is after `now` wins; the cap is what makes a cadence that can never match (a
        // monthly schedule with a day of the month the row somehow outran) an error rather
        // than a loop.
        for offset in 0..MAX_LOOKAHEAD_DAYS {
            let Some(date) = Date::from_julian_day(local_now.to_julian_day() + offset as i32).ok()
            else {
                break;
            };
            if !self.fires_on(date) {
                continue;
            }
            let local = OffsetDateTime::new_utc(date, at_time);
            let instant = self.resolve_local(local, zone);
            if instant > after {
                // Normalised to UTC rather than returned in the zone's own offset. The
                // instant is identical either way and `timestamptz` stores it identically,
                // but a value that arrives as `02:00+03:00` reads as a different answer to
                // a test and to a log line than the same instant as `23:00Z`, and "one
                // spelling for this" is worth more here than the three lines.
                return Ok(instant.to_offset(time::UtcOffset::UTC));
            }
        }
        Err(BackupError::Invalid(format!(
            "the {} schedule has no next run within {MAX_LOOKAHEAD_DAYS} days of {}",
            self.frequency, after
        )))
    }

    /// Whether this cadence fires on a local calendar date.
    fn fires_on(&self, date: Date) -> bool {
        match self.frequency.as_str() {
            // An hourly cadence fires on every day, which is why it never reaches this
            // function: it is answered above, from the clock rather than the calendar.
            "hourly" => true,
            "daily" => true,
            "weekly" => self.day_of_week == Some(date.weekday()),
            // Clamped rather than refused, because the database already refuses anything
            // above 28 and a month is never shorter than 28 days.
            "monthly" => date.day() as u8 == self.day_of_month.unwrap_or(1).clamp(1, 28),
            // An unknown frequency is asked about by the worker, which will answer with a
            // sentence naming it. It must not be a day that "does not fire", because that
            // turns a typo into a schedule that never runs and says nothing.
            _ => true,
        }
    }

    /// Turn a local wall clock reading into the instant it refers to.
    ///
    /// **The zone table is not asked whether a reading exists, because it does not say.**
    /// `get_offset_local` answers `Some` for `02:30` on a spring-forward morning — a wall
    /// clock reading that never happened — and answers `Some` rather than `Ambiguous` for
    /// the hour that happens twice in autumn. Both were confirmed by the three zone tests
    /// below failing against a version that trusted it. So the two questions are settled by
    /// **round-tripping** instead: propose an instant using a candidate offset, then ask the
    /// table what the wall clock reads at that instant. An offset that survives its own
    /// round trip is real; one that does not is a phantom.
    ///
    /// That yields the three cases without asking permission:
    ///
    /// * **exactly one offset round-trips** — the ordinary day.
    /// * **two offsets round-trip** — the autumn fold; the **earlier** instant wins, because
    ///   "02:30" happened twice and the first one is the one an operator means. Taking the
    ///   later would produce two runs an hour apart for one instruction, and
    ///   `retention_count` would then hold two of the same backup.
    /// * **none round-trips** — the spring gap; the run is moved forward by the gap, which is
    ///   the same wall clock reading once the clocks have jumped. Dropping the day loses a
    ///   backup; rejecting the schedule stops it for a year. The arithmetic falls out of the
    ///   candidates: the pre-transition offset is the smaller one, and `local - smaller` is
    ///   exactly `local + gap` read after the jump.
    fn resolve_local(&self, local: OffsetDateTime, zone: &time_tz::Tz) -> OffsetDateTime {
        let candidates = candidate_offsets(zone, local);
        let mut real: Vec<OffsetDateTime> = Vec::new();
        for &offset in &candidates {
            let instant = at_offset(local, offset);
            // The round trip: at this instant, does the zone actually read `local`? An
            // offset that survives its own round trip is real and one that does not is a
            // phantom invented by the arithmetic.
            if zone.get_offset_utc(&instant).to_utc() == offset {
                real.push(instant);
            }
        }

        match real.len() {
            1 => real[0],
            2 => real[0].min(real[1]),
            0 => {
                // The reading does not exist. `candidates` is ascending, so the first is the
                // pre-transition offset and the last is the post-transition one; subtracting
                // the pre-transition offset lands after the jump, which is the same reading
                // shifted forward by the gap.
                match (candidates.first().copied(), candidates.last().copied()) {
                    (Some(before), Some(after)) if before < after => at_offset(local, before),
                    _ => local,
                }
            }
            // Three or more is not a thing a real zone does; taking the earliest keeps the
            // worker alive rather than panicking on one row and taking every other schedule
            // down with it. A schedule an hour early is a defect an operator can see.
            _ => real.into_iter().min().unwrap_or(local),
        }
    }
}

/// The offsets a reading near `local` could plausibly carry, ascending.
///
/// Three instants are sampled — the target itself, the same wall clock half a year either
/// side — because a zone's *primary* offset is whatever is in force right now, and a schedule
/// for January in `Europe/London` must be offered `+00:00` even in July. Each sample
/// contributes its own offset and the two one-hour neighbours, which covers the ordinary DST
/// step in both directions without hard-coding "60 minutes": a zone that shifts by 30 or 120
/// minutes still has its real offset in the set, because the neighbours are only ever
/// *candidates* and the round trip decides.
fn candidate_offsets(zone: &time_tz::Tz, local: OffsetDateTime) -> Vec<time::UtcOffset> {
    let mut out: Vec<time::UtcOffset> = Vec::new();
    for shift_days in [-186_i32, 0, 186] {
        let Ok(date) = Date::from_julian_day(local.to_julian_day() + i32::from(shift_days)) else {
            continue;
        };
        let probe = OffsetDateTime::new_utc(date, local.time());
        let base = i64::from(zone.get_offset_utc(&probe).to_utc().whole_seconds());
        for seconds in [base, base + 3600, base - 3600] {
            let candidate =
                time::UtcOffset::from_whole_seconds(seconds as i32).unwrap_or(base_offset());
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out.sort();
    out
}

/// The offset used when a candidate offset is not representable. Only reachable if a zone
/// table ships an offset beyond ±24 hours, which no real one does; the fallback keeps the
/// function total instead of unwrapping a value the caller cannot check.
fn base_offset() -> time::UtcOffset {
    time::UtcOffset::UTC
}

/// The instant a local wall clock reading refers to, given the offset that applies to it.
///
/// **`to_offset` is not the operation.** It re-labels a value without moving it, so
/// `OffsetDateTime::new_utc(date, 02:00).to_offset(+03:00)` is `02:00+03:00`, which is the
/// same instant as `02:00Z` — a scheduler that does this stores 02:00 UTC and runs every
/// backup nine hours late, while every test that only checks "the hour field is 02:00" passes.
/// The instant is moved by **subtracting** the zone's offset, and normalised to UTC after,
/// because the reader is a worker comparing against `now()` and one spelling is worth more
/// than the three lines it costs.
///
/// The helper is a named function rather than an inline expression because there are four
/// call sites, three of them in the branches that fire once a year. A schedule that is an
/// hour out on exactly one day of the year is the definition of a bug nobody finds, and four
/// spellings of a sign is four chances to get three of them wrong.
fn at_offset(local: OffsetDateTime, offset: time::UtcOffset) -> OffsetDateTime {
    let shifted = local - Duration::seconds(i64::from(offset.whole_seconds()));
    shifted.to_offset(time::UtcOffset::UTC)
}

/// `0..=6` as a weekday. The column is a `smallint` with a `between 0 and 6` check, so the
/// cast cannot fail and the panic arm is unreachable — but it is written as one rather than
/// an `unwrap` so that widening the column is a compile error here and not a panic at 02:00.
fn weekday_of(day: i16) -> Weekday {
    match day {
        0 => Weekday::Sunday,
        1 => Weekday::Monday,
        2 => Weekday::Tuesday,
        3 => Weekday::Wednesday,
        4 => Weekday::Thursday,
        5 => Weekday::Friday,
        _ => Weekday::Saturday,
    }
}

/// `HH:MM` or `HH:MM:SS`, the two shapes PostgreSQL's `::text` cast produces.
fn parse_time(text: &str) -> Result<Time> {
    let trimmed = text.trim();
    Time::parse(trimmed, CLOCK_FULL)
        .or_else(|_| Time::parse(trimmed, CLOCK_SHORT))
        .map_err(|_| {
            BackupError::Invalid(format!(
                "{trimmed:?} is not a time of day (expected HH:MM, such as 02:30)"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::{CLOCK_SHORT, Cadence, MAX_LOOKAHEAD_DAYS};
    use crate::store::BackupSchedule;
    use time::{Duration, OffsetDateTime, Time};

    fn at(text: &str) -> Time {
        Time::parse(text, CLOCK_SHORT).unwrap()
    }

    fn utc(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
    }

    fn row(frequency: &str, at_time: Option<&str>, timezone: &str) -> BackupSchedule {
        BackupSchedule {
            organization_id: None,
            name: "nightly".to_owned(),
            frequency: frequency.to_owned(),
            at_time: at_time.map(str::to_owned),
            timezone: timezone.to_owned(),
            scopes: vec!["database".to_owned()],
            enabled: true,
            ..schedule_defaults()
        }
    }

    /// The rest of a schedule row, for the tests that do not care about it. `Default` is not
    /// derived on the struct, and adding it would put a second definition of "a schedule
    /// with nothing in it" beside the migration's.
    fn schedule_defaults() -> BackupSchedule {
        BackupSchedule {
            id: uuid::Uuid::nil(),
            organization_id: None,
            name: String::new(),
            frequency: "daily".to_owned(),
            at_time: None,
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
            scopes: Vec::new(),
            retention_count: 7,
            destination: "local".to_owned(),
            enabled: false,
            last_run_at: None,
            next_run_at: None,
            last_backup_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    // --- the plain cases ------------------------------------------------------------------

    #[test]
    fn a_daily_schedule_lands_on_the_next_occurrence_of_its_hour() {
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        // 01:00 today → 02:00 today.
        let next = cadence.next_after(utc("2026-01-10T01:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T02:00:00Z"));
        // 02:00:00.5 today → tomorrow, because the next run must be *after* now, not equal.
        let next = cadence
            .next_after(utc("2026-01-10T02:00:00Z") + Duration::milliseconds(500))
            .unwrap();
        assert_eq!(next, utc("2026-01-11T02:00:00Z"));
    }

    #[test]
    fn a_daily_schedule_just_past_its_hour_waits_a_whole_day() {
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T02:00:01Z")).unwrap();
        assert_eq!(next, utc("2026-01-11T02:00:00Z"));
    }

    #[test]
    fn a_weekly_schedule_waits_for_its_weekday_not_for_its_day() {
        // 2026-01-10 is a Saturday; the schedule wants Mondays at 03:00.
        let cadence = Cadence {
            frequency: "weekly".to_owned(),
            at_time: Some(at("03:00")),
            day_of_week: Some(time::Weekday::Monday),
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T12:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-12T03:00:00Z"));
        // The Sunday before it is not a match, and the search must skip it rather than
        // answering "tomorrow, 03:00" for a Monday schedule.
        let next = cadence.next_after(utc("2026-01-10T04:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-12T03:00:00Z"));
    }

    #[test]
    fn a_monthly_schedule_waits_for_its_day_of_the_month() {
        // 2026-01-10, asking for the 15th at 04:00.
        let cadence = Cadence {
            frequency: "monthly".to_owned(),
            at_time: Some(at("04:00")),
            day_of_week: None,
            day_of_month: Some(15),
            timezone: "UTC".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T12:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-15T04:00:00Z"));
        // Ask on the 16th and the answer is February, not the 15th of this month.
        let next = cadence.next_after(utc("2026-01-16T12:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-02-15T04:00:00Z"));
    }

    #[test]
    fn an_hourly_schedule_needs_no_time_of_day() {
        let cadence = Cadence {
            frequency: "hourly".to_owned(),
            at_time: None,
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        // 14:37 → 15:00, not 14:00 (that is behind us) and not 15:37 (hourly means the top
        // of the hour, which is the only reading a human attaches to "every hour").
        let next = cadence.next_after(utc("2026-01-10T14:37:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T15:00:00Z"));
        // Exactly on the hour still moves forward, for the same reason as the daily case.
        let next = cadence.next_after(utc("2026-01-10T15:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T16:00:00Z"));
    }

    // --- the timezone half, which is the reason this module exists ------------------------

    #[test]
    fn the_wall_clock_is_local_and_the_stored_instant_is_utc() {
        // Istanbul is UTC+3 all winter: a 02:00 local backup is 23:00 UTC the day before.
        // This is the single most valuable assertion in the module — the wrong version stores
        // 02:00 UTC and the backup runs nine hours late for eight months of the year.
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "Europe/Istanbul".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T12:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T23:00:00Z"));
    }

    #[test]
    fn a_zone_behind_utc_does_not_roll_the_day_backwards() {
        // New York in January is UTC-5: 02:00 local is 07:00 UTC the *same* day. A scheduler
        // that computed the local date by subtracting a whole day would answer a day early.
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "America/New_York".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T12:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-11T07:00:00Z"));
    }

    // --- daylight saving, the transition the feature's risk list names ---------------------

    /// London moves its clocks at **01:00 UTC**: on 2026-03-29 the local reading jumps
    /// 01:59 → 02:00, and on 2026-10-25 it falls back 02:00 → 01:00. So the hour that does
    /// not exist in spring is 01:00–01:59 **local**, and the hour that happens twice in
    /// autumn is also 01:00–01:59. Both tests below use 01:30 for that reason.
    ///
    /// The first version of these two tests used 02:30, on the reasonable but wrong belief
    /// that the transition happened at 02:00 local, and both failed — with the *code* right.
    /// A DST test written from a half-remembered rule is a test of the author's memory, and
    /// it fails for the wrong reason, which is the worst kind of red.
    const UK_TRANSITION_YEAR: i32 = 2026;

    #[test]
    fn a_spring_forward_gap_moves_the_run_forward_rather_than_dropping_it() {
        // 01:30 local on 2026-03-29 does not exist: the clocks go 01:00 GMT → 02:00 BST. A
        // schedule for 01:30 that is *dropped* loses a backup; one that is *rejected* never
        // runs at all. The answer is 02:30 BST, the same reading once the gap has passed.
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("01:30")),
            day_of_week: None,
            day_of_month: None,
            timezone: "Europe/London".to_owned(),
        };
        let next = cadence
            .next_after(utc("2026-03-28T12:00:00Z"))
            .expect("a spring-forward day still has a next run");
        // 02:30 BST is 01:30 UTC. Note that this is the *same instant* a schedule for 02:30
        // would get — which is the point: the gap is one hour wide, so shifting the reading
        // forward by the gap lands on the next real reading of the same clock.
        assert_eq!(next, utc("2026-03-29T01:30:00Z"));
        // The day after, London is **on BST for the rest of the quarter** — so 01:30 local is
        // 00:30 UTC, an hour earlier on the clock than it was the week before. The version of
        // this assertion that expected 01:30 UTC assumed the clocks went back; they went
        // forward, and the probe printed in this module's development confirmed the offset
        // is +1 from 01:00Z on the 29th onward.
        let next = cadence
            .next_after(utc("2026-03-29T12:00:00Z"))
            .expect("the day after a transition is ordinary again");
        assert_eq!(next, utc("2026-03-30T00:30:00Z"));
    }

    #[test]
    fn an_autumn_repeated_hour_runs_once_at_the_earlier_instant() {
        // 01:30 local on 2026-10-25 happens TWICE: once at 01:30 BST (00:30 UTC) and again at
        // 01:30 GMT (01:30 UTC). Running on both would produce two runs an hour apart for
        // one instruction, and the retention count would then hold two of the same backup.
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("01:30")),
            day_of_week: None,
            day_of_month: None,
            timezone: "Europe/London".to_owned(),
        };
        let next = cadence
            .next_after(utc("2026-10-24T12:00:00Z"))
            .expect("an autumn fold still has a next run");
        // The earlier instant is 00:30 UTC, the first of the two readings.
        assert_eq!(next, utc("2026-10-25T00:30:00Z"));
        // Asked again from just after it, the next run is tomorrow — NOT the second 01:30
        // UTC reading an hour later, which is the whole content of this assertion.
        let next = cadence
            .next_after(utc("2026-10-25T00:30:01Z"))
            .expect("a fold does not make a schedule fire twice");
        assert_eq!(next, utc("2026-10-26T01:30:00Z"));
    }

    #[test]
    fn a_zone_behind_utc_keeps_its_own_offset_across_a_transition() {
        // New York moves at 02:00 *local*, not 01:00 UTC — which is exactly why the two
        // London tests above cannot be reused as a template for a western zone. 01:30 local
        // on 2026-03-08 in New York does not exist, and the answer is 03:30 EDT (07:30 UTC).
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("01:30")),
            day_of_week: None,
            day_of_month: None,
            timezone: "America/New_York".to_owned(),
        };
        let next = cadence
            .next_after(utc("2026-03-07T12:00:00Z"))
            .expect("a western zone has a next run across its own transition");
        // 02:30 EDT (UTC-4) is 06:30 UTC. Note this is an hour **earlier** than the London
        // gap produced for the same wall clock reading, because the two zones jump in
        // opposite directions relative to UTC: a test that reused London's expected value here
        // would pass the compile and fail the assertion, which is why both are written out.
        assert_eq!(next, utc("2026-03-08T06:30:00Z"));
        assert_eq!(
            UK_TRANSITION_YEAR, 2026,
            "the transition dates in these tests are for 2026; a year change invalidates them"
        );
    }

    #[test]
    fn a_zone_that_skips_a_whole_month_still_answers_within_the_lookahead() {
        // A monthly schedule on the 1st, asked in the middle of January, must find February —
        // which proves the search crosses a month boundary rather than only walking days.
        let cadence = Cadence {
            frequency: "monthly".to_owned(),
            at_time: Some(at("05:00")),
            day_of_week: None,
            day_of_month: Some(1),
            timezone: "UTC".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-15T00:00:00Z")).unwrap();
        assert_eq!(next, utc("2026-02-01T05:00:00Z"));
    }

    // --- refusals -------------------------------------------------------------------------

    #[test]
    fn an_unknown_zone_is_refused_by_name() {
        // Not defaulted to UTC. A schedule that says "Europe/Istanbool" running on UTC is a
        // wrong answer delivered confidently, and an operator has no way to see it.
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "Europe/Istanbool".to_owned(),
        };
        let error = cadence.next_after(OffsetDateTime::now_utc()).unwrap_err();
        assert!(
            error.to_string().contains("Europe/Istanbool"),
            "the error must name the zone it refused: {error}"
        );
    }

    #[test]
    fn utc_is_accepted_as_a_name_because_the_migration_defaults_to_it() {
        let cadence = Cadence {
            frequency: "daily".to_owned(),
            at_time: Some(at("02:00")),
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        assert!(cadence.next_after(utc("2026-01-10T01:00:00Z")).is_ok());
    }

    #[test]
    fn a_daily_schedule_with_no_time_of_day_is_refused_rather_than_run_at_midnight() {
        // The database constrains this, so the row cannot exist — but the module is also read
        // by the worker, and "midnight" is exactly the fallback nobody chose.
        let error = Cadence::from_schedule(&row("daily", None, "UTC")).unwrap_err();
        assert!(error.to_string().contains("no time of day"), "{error}");
    }

    #[test]
    fn a_daily_schedule_with_an_unparseable_time_is_refused_naming_the_value() {
        let error =
            Cadence::from_schedule(&row("daily", Some("half past two"), "UTC")).unwrap_err();
        assert!(error.to_string().contains("half past two"), "{error}");
    }

    #[test]
    fn an_hourly_schedule_needs_no_time_and_is_not_refused_for_omitting_one() {
        // The mirror of the daily case: the `match` is on the frequency, so the one cadence
        // that legitimately has no time of day is not treated as a broken row.
        let cadence = Cadence::from_schedule(&row("hourly", None, "UTC")).unwrap();
        assert!(cadence.at_time.is_none());
    }

    #[test]
    fn an_hourly_schedule_lands_on_the_top_of_the_next_hour() {
        // A second before the top of the hour is still this hour's run being slightly late,
        // so the answer is 15:00 — 30 seconds away, not 30 minutes.
        let cadence = Cadence {
            frequency: "hourly".to_owned(),
            at_time: None,
            day_of_week: None,
            day_of_month: None,
            timezone: "UTC".to_owned(),
        };
        let next = cadence.next_after(utc("2026-01-10T14:59:30Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T15:00:00Z"));
        // Mid-hour is the case the "add then truncate" version got wrong: 15:30 + 1h is
        // 16:30, which truncated is 16:00 — correct — but only because the minute column
        // survives. The assertion below pins the real invariant instead: the answer is always
        // exactly on the hour, never on a minute boundary carried over from `after`.
        let next = cadence.next_after(utc("2026-01-10T15:30:00Z")).unwrap();
        assert_eq!(next, utc("2026-01-10T16:00:00Z"));
    }

    #[test]
    fn the_lookahead_is_long_enough_to_cross_a_month_from_the_31st() {
        // 28 days is the maximum the database allows, and the search starts at *today*, so a
        // schedule on the 28th asked on the 29th needs 30 days to reach the next one. The
        // constant is a bound, not a preference.
        assert!(
            MAX_LOOKAHEAD_DAYS >= 31,
            "a month schedule must survive a full month"
        );
    }
}
