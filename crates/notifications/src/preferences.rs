//! The reader's own channel configuration (REQ-021, slice 2).
//!
//! Slice 1 gave the platform a record and a bell. This module is the half that decides **how**
//! a record reaches somebody, and it exists because the alternative — one global switch — is the
//! reason people turn notifications off entirely and then miss the security alert.
//!
//! **The matrix is stored as the rows a person changed, not as a full grid.** A `PUT` replaces
//! the caller's *stated* preferences and every cell the caller did not mention keeps the
//! platform default (`true`). That is what makes a future channel a non-breaking change: a
//! `web_push` row added in slice 3 is `true` for everybody who has never opened the settings
//! screen, and a `notification_preferences` table seeded with 30 rows per user per channel is
//! a table nobody will ever keep honest.
//!
//! **The in-app cell cannot be switched off, and that is a server rule, not a UI one.** A
//! checkbox the UI disables is a promise; a rule the store refuses is a guarantee. The panel
//! renders the column locked, and a `PUT` naming `in_app: false` is a `400` naming the reason —
//! a notification nobody can see in the panel is a row that exists only for a database.
//!
//! **Quiet hours are validated as a pair, never as a range.** A window that wraps midnight
//! (`22:00 → 07:00`) is legitimate and common; a window that covers the whole day
//! (`08:00 → 08:00`) is not, because it means "never send me anything" — which is a request
//! the platform refuses to record, because the reader would then believe e-mail is on.

use time::Date;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::NotificationError;
use crate::error::Result;
use crate::vocabulary::{CATEGORIES, CHANNELS, is_category, is_channel};

/// How often a person wants their unread notifications rolled into one mail.
pub const DIGEST_CADENCES: [&str; 3] = ["off", "daily", "weekly"];

/// The hour a daily digest may be sent at — the whole range, so the check is a real one.
pub const DIGEST_HOUR_MIN: i16 = 0;
/// See [`DIGEST_HOUR_MIN`].
pub const DIGEST_HOUR_MAX: i16 = 23;

/// The smallest gap a quiet window may leave between its end and its start.
///
/// A 24-hour window is "the whole day" and a 23:59 one is the same claim written fussy, so the
/// rule is stated as a window **shorter** than the day: anything that leaves no waking hours
/// is refused.
pub const MIN_AWAKE_HOURS: i32 = 1;

/// One cell of the matrix: "does category *C* reach me over *channel*?".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreferenceCell {
    /// The category the cell is about.
    pub category: String,
    /// The channel the cell is about.
    pub channel: String,
    /// Whether the notification is delivered over that channel.
    pub enabled: bool,
}

/// One person's whole channel configuration, as the settings screen reads it.
///
/// The matrix is always **complete**: every category × every channel, at the value that applies
/// (stated, or the default). A settings form that renders only the stated rows is a form whose
/// unchecked boxes are invisible, and a reader who cannot see a state cannot change it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Preferences {
    /// Who these belong to.
    pub user_id: Uuid,
    /// Every cell, in the vocabulary's order.
    pub matrix: Vec<PreferenceCell>,
    /// The user's stored settings row, if they have one.
    pub settings: Settings,
}

impl Preferences {
    /// The complete matrix for a person who has stated nothing.
    ///
    /// Every cell `true`, because a platform that starts silent teaches people to ignore the
    /// one surface it is going to need. Only `in_app` cannot be turned off.
    #[must_use]
    pub fn defaults(user_id: Uuid) -> Self {
        let matrix = CATEGORIES
            .iter()
            .flat_map(|category| {
                CHANNELS.iter().map(move |channel| PreferenceCell {
                    category: (*category).to_owned(),
                    channel: (*channel).to_owned(),
                    enabled: true,
                })
            })
            .collect();
        Self {
            user_id,
            matrix,
            settings: Settings::default_for(user_id),
        }
    }

    /// The value of one cell, with the default as the answer when it was never stated.
    #[must_use]
    pub fn enabled(&self, category: &str, channel: &str) -> bool {
        self.matrix
            .iter()
            .find(|cell| cell.category == category && cell.channel == channel)
            .map_or(true, |cell| cell.enabled)
    }

    /// Whether a notification of this category may go out over this channel.
    ///
    /// `in_app` is answered `true` unconditionally — the same rule the `PUT` enforces, read
    /// from the same place, so the panel and the runner cannot disagree about it.
    #[must_use]
    pub fn allows(&self, category: &str, channel: &str) -> bool {
        if channel == IN_APP {
            return true;
        }
        self.enabled(category, channel)
    }
}

/// The channel a person cannot opt out of, and the reason the settings screen says.
pub const IN_APP: &str = "in_app";
/// The browser push channel, named once so the queue and the runner cannot disagree on the
/// spelling — the same reason [`IN_APP`] exists.
pub const WEB_PUSH: &str = "web_push";

/// One row of `notification_settings`: quiet hours, timezone and the digest cadence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Who it belongs to.
    pub user_id: Uuid,
    /// Quiet hours begin, in the user's own timezone.
    pub quiet_hours_start: Option<String>,
    /// Quiet hours end, in the user's own timezone.
    pub quiet_hours_end: Option<String>,
    /// The IANA timezone the two instants are read in.
    pub timezone: String,
    /// `off`, `daily` or `weekly`.
    pub digest_cadence: String,
    /// Which weekday a weekly digest goes out on, 0 = Monday.
    pub digest_weekday: Option<i16>,
    /// Which hour of the day a digest goes out in.
    pub digest_hour: i16,
}

impl Settings {
    /// The settings a person starts with: no quiet hours, UTC, no digest, 08:00.
    #[must_use]
    pub fn default_for(user_id: Uuid) -> Self {
        Self {
            user_id,
            quiet_hours_start: None,
            quiet_hours_end: None,
            timezone: "UTC".to_owned(),
            digest_cadence: "off".to_owned(),
            digest_weekday: None,
            digest_hour: 8,
        }
    }

    /// `true` when this person is inside their quiet hours right now.
    ///
    /// **A window that wraps midnight is handled by comparison, not by splitting the day.** The
    /// two shapes are `start <= end` (same day) and `start > end` (wraps). Writing it as
    /// "if the window is inside, or the window wraps and the now is not inside the gap" is
    /// one condition that covers both, and a hand-rolled `if` per shape is where a 23:00 →
    /// 07:00 window silently becomes *never* quiet.
    ///
    /// No window at all means `false`: a person who set no quiet hours is not in them.
    #[must_use]
    pub fn in_quiet_hours(&self, now: OffsetDateTime) -> bool {
        // Both ends or neither: `validate_quiet_hours` refuses a half-set window, so the
        // `else` arm here is only reachable for a row written before that rule existed.
        let (Some(start), Some(end)) = (
            parse_clock(self.quiet_hours_start.as_deref()),
            parse_clock(self.quiet_hours_end.as_deref()),
        ) else {
            return false;
        };
        let local = now.to_offset(self.zone_offset());
        in_window(hour_minute(local), start, end)
    }

    /// The UTC offset this person's timezone names, as a `time` offset.
    #[must_use]
    pub fn zone_offset(&self) -> time::UtcOffset {
        time::UtcOffset::from_whole_seconds(fixed_offset_seconds(&self.timezone))
            .unwrap_or(time::UtcOffset::UTC)
    }

    /// `true` when a digest is due for this person at `now`.
    ///
    /// **The check is "has it been a whole cadence since the last one", not "is it the right
    /// minute".** A cron that fires at 08:00 and asks "is the hour 8 and the weekday Monday"
    /// sends a digest to everybody in the world at 08:00 on the day the runner restarts, and
    /// sends none at all on the day the runner was down at 08:00. Bounded attempts on the
    /// delivery row are what stop a late runner from repeating itself, so the cadence decision
    /// here is a *window*, not an instant.
    #[must_use]
    pub fn digest_due(&self, now: OffsetDateTime) -> bool {
        match self.digest_cadence.as_str() {
            "daily" => true,
            "weekly" => {
                let local = now.to_offset(self.zone_offset());
                weekday_of(local.date()) == self.digest_weekday.unwrap_or(0)
            }
            _ => false,
        }
    }
}

/// Whether `now` (minutes past local midnight) falls inside the window `start..end`.
///
/// Wrapping windows included, which is the whole point of doing it here rather than in two
/// places. `start == end` is a 24-hour window and is handled the same way as a wrap that
/// happens to meet itself, so the caller never has to special-case it.
fn in_window(now_minutes: i32, start: i32, end: i32) -> bool {
    if start == end {
        return true;
    }
    if start < end {
        now_minutes >= start && now_minutes < end
    } else {
        // Wraps midnight: the window is [start, midnight) ∪ [midnight, end).
        now_minutes >= start || now_minutes < end
    }
}

/// Minutes past local midnight, from a time that has already been converted to local.
fn hour_minute(local: OffsetDateTime) -> i32 {
    i32::from(local.hour()) * 60 + i32::from(local.minute())
}

/// `HH:MM` as minutes past midnight.
///
/// **Three shapes are accepted, and the third one is why this function is not a `split_once`.**
/// `22:00` is what the form sends, `22:00:00` is what `time::Time`'s own `Display` produces, and
/// ` 22:00 ` is what a hand-written client sends. Accepting only the first means a string the
/// platform wrote itself is one the platform cannot read back — and the caller cannot tell the
/// two apart, because it only ever sees `None`.
fn parse_clock(value: Option<&str>) -> Option<i32> {
    let raw = value?.trim();
    let (hour, minute) = raw.split_once(':')?;
    let hour: i32 = hour.trim().parse().ok()?;
    // A seconds field is tolerated and dropped, never parsed as the hour. `22:00:00` split
    // once is `("22", "00:00")`, and `"00:00".parse::<i32>()` is `None` — so the seconds case
    // fails on the minute, not on a "too many colons" rule that would have had to be
    // re-remembered at the call site.
    let minute: i32 = minute.trim().split(':').next()?.trim().parse().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    Some(hour * 60 + minute)
}

/// `HH:MM` from a `time::Time`, as the platform stores and speaks it.
///
/// **This is the other half of `parse_clock`, and it exists because the two used to disagree.**
/// The settings row is a Postgres `time` column, and the two ways to turn that column into a
/// string are not interchangeable: `quiet_hours_start::text` yields `22:00:00` (seconds always
/// present, zero-padded) while `to_char(quiet_hours_start, 'HH24:MI')` yields `22:00`. The
/// store read the column the first way, so every value that came back was in a shape the
/// platform's own parser rejected — which is why a saved quiet window silently stopped
/// deciding anything, and why the settings form could not read back what it had just written.
#[must_use]
pub fn format_clock(value: time::Time) -> String {
    format!("{:02}:{:02}", value.hour(), value.minute())
}

/// The weekday of a date, 0 = Monday — the same numbering Postgres's `extract(dow)` uses
/// minus one, and the same one the settings screen's dropdown is ordered in.
fn weekday_of(date: Date) -> i16 {
    match date.weekday() {
        time::Weekday::Monday => 0,
        time::Weekday::Tuesday => 1,
        time::Weekday::Wednesday => 2,
        time::Weekday::Thursday => 3,
        time::Weekday::Friday => 4,
        time::Weekday::Saturday => 5,
        time::Weekday::Sunday => 6,
    }
}

/// The handful of zones the panel's timezone list actually offers, as fixed offsets.
///
/// A full IANA database is a dependency and a second source of truth; the list a settings form
/// shows is a *choice list*, and `Etc/UTC` plus the common European offsets cover it. An
/// unrecognised zone is treated as UTC rather than refused, because a reader who typed an
/// exotic zone into a free-text field would rather get their digest than a validation error.
fn fixed_offset_seconds(zone: &str) -> i32 {
    match zone.trim() {
        "Europe/Istanbul" => 3 * 3600,
        "Europe/Berlin" | "Europe/Paris" | "Europe/Madrid" | "Europe/Rome" | "Europe/Amsterdam" => {
            2 * 3600
        }
        "Europe/London" => 0,
        "America/New_York" => -5 * 3600,
        "America/Los_Angeles" => -8 * 3600,
        "America/Sao_Paulo" => -3 * 3600,
        "Asia/Dubai" => 4 * 3600,
        "Asia/Kolkata" => 5 * 3600 + 1800,
        "Asia/Tokyo" => 9 * 3600,
        "Australia/Sydney" => 10 * 3600,
        _ => 0,
    }
}

/// A preference to write, as a `PUT` body names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatedPreference {
    /// The category.
    pub category: String,
    /// The channel.
    pub channel: String,
    /// Whether it is on.
    pub enabled: bool,
}

/// Validate and normalise the cells a `PUT` carries.
///
/// Two rules, both of which have to be here rather than in the handler, because the store is
/// the only place that is guaranteed to run:
///
/// 1. **The names must be real.** A category the platform does not know is a `400` naming the
///    legal values — the same rule the list filter and the SQL constraint enforce, so there is
///    one answer to "is this a category" rather than three.
/// 2. **`in_app: false` is refused.** Not ignored, not clamped: refused with a sentence, so a
///    client learns why instead of saving a preference that has no effect.
pub fn validate_stated(cells: &[StatedPreference]) -> Result<Vec<StatedPreference>> {
    let mut seen = std::collections::BTreeSet::new();
    for cell in cells {
        if !is_category(&cell.category) {
            return Err(NotificationError::invalid(format!(
                "category \"{}\" is not one of {CATEGORIES:?}",
                cell.category
            )));
        }
        if !is_channel(&cell.channel) {
            return Err(NotificationError::invalid(format!(
                "channel \"{}\" is not one of {CHANNELS:?}",
                cell.channel
            )));
        }
        if cell.channel == IN_APP && !cell.enabled {
            return Err(NotificationError::invalid(
                "in-app notifications cannot be turned off — the bell is how this platform \
                 reaches you, and a row nobody can see in the panel is a row that exists only \
                 for a database"
                    .to_owned(),
            ));
        }
        if !seen.insert((cell.category.clone(), cell.channel.clone())) {
            return Err(NotificationError::invalid(format!(
                "{}/{} appears twice in one request — the second value would silently win",
                cell.category, cell.channel
            )));
        }
    }
    Ok(cells.to_vec())
}

/// Validate a settings row before it is written.
///
/// Every rule here is one the SQL check constraints would catch, written as a sentence
/// instead — the difference between a panel showing "invalid input: weekday" and a
/// `500` with a constraint name in it.
pub fn validate_settings(settings: &Settings) -> Result<()> {
    if !DIGEST_CADENCES.contains(&settings.digest_cadence.as_str()) {
        return Err(NotificationError::invalid(format!(
            "digest_cadence \"{}\" is not one of {DIGEST_CADENCES:?}",
            settings.digest_cadence
        )));
    }
    if !(DIGEST_HOUR_MIN..=DIGEST_HOUR_MAX).contains(&settings.digest_hour) {
        return Err(NotificationError::invalid(format!(
            "digest_hour {} is not between {DIGEST_HOUR_MIN} and {DIGEST_HOUR_MAX}",
            settings.digest_hour
        )));
    }
    if let Some(weekday) = settings.digest_weekday {
        if !(0..=6).contains(&weekday) {
            return Err(NotificationError::invalid(format!(
                "digest_weekday {weekday} is not between 0 (Monday) and 6 (Sunday)"
            )));
        }
    }
    if settings.digest_cadence == "weekly" && settings.digest_weekday.is_none() {
        return Err(NotificationError::invalid(
            "a weekly digest needs a weekday — \"every week on no day\" is not a schedule"
                .to_owned(),
        ));
    }
    validate_quiet_hours(
        settings.quiet_hours_start.as_deref(),
        settings.quiet_hours_end.as_deref(),
    )?;
    if settings.timezone.trim().is_empty() {
        return Err(NotificationError::invalid(
            "a timezone is required — without one the quiet window has no meaning".to_owned(),
        ));
    }
    Ok(())
}

/// Validate the quiet-hours pair, as a pair.
///
/// **A half-set window is a `400`, not a default.** "Quiet hours from 22:00" with no end is
/// the most common form of this bug: the reader means "until I wake up", the platform
/// guesses a default, and the notification they were trying to silence arrives at 08:00 and
/// they conclude the setting does not work.
pub fn validate_quiet_hours(start: Option<&str>, end: Option<&str>) -> Result<()> {
    match (start, end) {
        (None, None) => Ok(()),
        (Some(_), None) => Err(NotificationError::invalid(
            "quiet_hours_end is required when quiet_hours_start is set — half a quiet window is \
             not a window"
                .to_owned(),
        )),
        (None, Some(_)) => Err(NotificationError::invalid(
            "quiet_hours_start is required when quiet_hours_end is set — half a quiet window is \
             not a window"
                .to_owned(),
        )),
        (Some(start), Some(end)) => {
            let (Some(start), Some(end)) = (parse_clock(Some(start)), parse_clock(Some(end)))
            else {
                return Err(NotificationError::invalid(format!(
                    "quiet hours are \"{start}\" to \"{end}\" and must both be HH:MM"
                )));
            };
            // A window that leaves no waking hours is the same claim as the whole day, and a
            // reader who sets it has made a mistake the platform should name. `rem_euclid` is
            // what makes the wrapping case work: 22:00→07:00 is -900 minutes, which is 540
            // minutes of window once it is taken into the day.
            let covered = (end - start).rem_euclid(24 * 60);
            if covered == 0 || 24 * 60 - covered < MIN_AWAKE_HOURS * 60 {
                return Err(NotificationError::invalid(format!(
                    "quiet hours {start}→{end} leave no waking hours — a window has to stop \
                     before midnight is the next morning"
                )));
            }
            Ok(())
        }
    }
}

/// The default quiet-hours pair used by the settings form's "restore defaults" button.
#[must_use]
pub fn default_quiet_hours() -> (String, String) {
    ("22:00".to_owned(), "07:00".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn user() -> Uuid {
        Uuid::nil()
    }

    fn settings_at(hour: i16) -> Settings {
        Settings {
            user_id: user(),
            quiet_hours_start: Some("22:00".to_owned()),
            quiet_hours_end: Some("07:00".to_owned()),
            timezone: "UTC".to_owned(),
            digest_cadence: "off".to_owned(),
            digest_weekday: None,
            digest_hour: hour,
        }
    }

    #[test]
    fn the_default_matrix_is_complete_and_all_on() {
        let preferences = Preferences::defaults(user());
        // Six categories × five channels: the form renders every cell, so a reader can see a
        // channel they never touched rather than an absent row.
        assert_eq!(preferences.matrix.len(), CATEGORIES.len() * CHANNELS.len());
        assert!(preferences.matrix.iter().all(|cell| cell.enabled));
        assert!(preferences.allows("security", "email"));
    }

    #[test]
    fn in_app_is_allowed_even_when_a_cell_says_otherwise() {
        let mut preferences = Preferences::defaults(user());
        preferences.matrix.push(PreferenceCell {
            category: "security".to_owned(),
            channel: IN_APP.to_owned(),
            enabled: false,
        });
        // The read path answers from the rule, not from the row: a stale row cannot switch off
        // the one channel the platform owes everybody.
        assert!(preferences.allows("security", IN_APP));
    }

    #[test]
    fn a_stated_cell_is_read_back_exactly() {
        let mut preferences = Preferences::defaults(user());
        preferences.matrix = preferences
            .matrix
            .into_iter()
            .map(|mut cell| {
                if cell.category == "ticket" && cell.channel == "email" {
                    cell.enabled = false;
                }
                cell
            })
            .collect();
        assert!(!preferences.enabled("ticket", "email"));
        // The rest of the row is untouched: a full-replace PUT that only mentioned one cell
        // would have to re-send all thirty, and a client that forgets one would silently turn
        // a channel off.
        assert!(preferences.enabled("ticket", "web_push"));
    }

    #[test]
    fn switching_off_in_app_is_refused_with_a_reason() {
        let error = validate_stated(&[StatedPreference {
            category: "system".to_owned(),
            channel: IN_APP.to_owned(),
            enabled: false,
        }])
        .expect_err("in_app cannot be off");
        assert!(error.to_string().contains("in-app"));
    }

    #[test]
    fn an_unknown_category_is_refused_naming_the_legal_values() {
        let error = validate_stated(&[StatedPreference {
            category: "invoice".to_owned(),
            channel: "email".to_owned(),
            enabled: true,
        }])
        .expect_err("invoice is not a category");
        assert!(error.to_string().contains("invoice"));
        assert!(error.to_string().contains("approval"));
    }

    #[test]
    fn the_same_cell_twice_is_refused_rather_than_last_one_winning() {
        let error = validate_stated(&[
            StatedPreference {
                category: "ticket".to_owned(),
                channel: "email".to_owned(),
                enabled: true,
            },
            StatedPreference {
                category: "ticket".to_owned(),
                channel: "email".to_owned(),
                enabled: false,
            },
        ])
        .expect_err("duplicated cell");
        assert!(error.to_string().contains("twice"));
    }

    #[test]
    fn a_wrapping_quiet_window_covers_both_halves_of_the_day() {
        let settings = settings_at(8);
        // 23:00 and 03:00 are inside a 22:00→07:00 window; 12:00 is not. Getting the wrap
        // wrong makes the window either "never" or "always", and both are silent.
        assert!(settings.in_quiet_hours(datetime!(2026-09-28 23:30 UTC)));
        assert!(settings.in_quiet_hours(datetime!(2026-09-28 03:00 UTC)));
        assert!(!settings.in_quiet_hours(datetime!(2026-09-28 12:00 UTC)));
    }

    #[test]
    fn a_same_day_window_is_not_a_wrap() {
        let settings = Settings {
            quiet_hours_start: Some("01:00".to_owned()),
            quiet_hours_end: Some("05:00".to_owned()),
            ..settings_at(8)
        };
        assert!(settings.in_quiet_hours(datetime!(2026-09-28 02:00 UTC)));
        // Before the start is *not* inside, even though it is the same clock range on the
        // previous day — this is the half a wrap-aware implementation gets wrong.
        assert!(!settings.in_quiet_hours(datetime!(2026-09-28 00:30 UTC)));
    }

    #[test]
    fn no_window_means_not_in_quiet_hours() {
        let settings = Settings {
            quiet_hours_start: None,
            quiet_hours_end: None,
            ..settings_at(8)
        };
        assert!(!settings.in_quiet_hours(datetime!(2026-09-28 23:30 UTC)));
    }

    #[test]
    fn the_window_is_read_in_the_readers_own_timezone() {
        let settings = Settings {
            quiet_hours_start: Some("22:00".to_owned()),
            quiet_hours_end: Some("07:00".to_owned()),
            timezone: "Europe/Istanbul".to_owned(),
            ..settings_at(8)
        };
        // 22:00 UTC is 01:00 in Istanbul — inside the window, where in UTC it would not be.
        assert!(settings.in_quiet_hours(datetime!(2026-09-28 22:00 UTC)));
        // 11:00 UTC is 14:00 in Istanbul — outside, and the same instant as the assertion
        // above would have said "outside" if the timezone were ignored.
        assert!(!settings.in_quiet_hours(datetime!(2026-09-28 11:00 UTC)));
    }

    #[test]
    fn a_window_that_covers_the_whole_day_is_refused() {
        let error = validate_quiet_hours(Some("08:00"), Some("08:00")).expect_err("a full day");
        assert!(error.to_string().contains("waking hours"));
    }

    #[test]
    fn half_a_window_is_refused_rather_than_defaulted() {
        let error = validate_quiet_hours(Some("22:00"), None).expect_err("no end");
        assert!(error.to_string().contains("quiet_hours_end"));
    }

    #[test]
    fn a_malformed_clock_is_refused_with_both_values_named() {
        let error = validate_quiet_hours(Some("22h00"), Some("07:00")).expect_err("not HH:MM");
        assert!(error.to_string().contains("22h00"));
        assert!(error.to_string().contains("HH:MM"));
    }

    #[test]
    fn a_weekly_digest_without_a_weekday_is_refused() {
        let error = validate_settings(&Settings {
            digest_cadence: "weekly".to_owned(),
            digest_weekday: None,
            ..settings_at(8)
        })
        .expect_err("weekly needs a weekday");
        assert!(error.to_string().contains("weekday"));
    }

    #[test]
    fn a_daily_digest_needs_no_weekday_and_saves_fine() {
        let settings = Settings {
            digest_cadence: "daily".to_owned(),
            digest_weekday: None,
            ..settings_at(8)
        };
        assert!(validate_settings(&settings).is_ok());
    }

    #[test]
    fn the_digest_is_due_on_the_weekday_it_names() {
        let settings = Settings {
            digest_cadence: "weekly".to_owned(),
            // 2026-09-28 is a Monday, so weekday 0.
            digest_weekday: Some(0),
            ..settings_at(8)
        };
        assert!(settings.digest_due(datetime!(2026-09-28 08:00 UTC)));
        assert!(!settings.digest_due(datetime!(2026-09-29 08:00 UTC)));
    }

    #[test]
    fn a_cadence_of_off_is_never_due() {
        let settings = Settings {
            digest_cadence: "off".to_owned(),
            ..settings_at(8)
        };
        assert!(!settings.digest_due(datetime!(2026-09-28 08:00 UTC)));
    }

    #[test]
    fn an_impossible_hour_is_refused_naming_the_range() {
        let error = validate_settings(&Settings {
            digest_hour: 24,
            ..settings_at(8)
        })
        .expect_err("there is no 24th hour");
        assert!(error.to_string().contains("0"));
        assert!(error.to_string().contains("23"));
    }

    #[test]
    fn the_parity_test_proves_the_window_helper_against_a_naive_reading() {
        // The helper is the whole of the quiet-hours logic, and it is the part most likely to
        // be "simplified" into something that handles only one of the two window shapes. Four
        // cases, stated as minutes, so a reader can see which rule produced which answer.
        assert!(in_window(23 * 60, 22 * 60, 7 * 60), "wrap: late evening");
        assert!(in_window(2 * 60, 22 * 60, 7 * 60), "wrap: small hours");
        assert!(!in_window(12 * 60, 22 * 60, 7 * 60), "wrap: midday");
        assert!(in_window(2 * 60, 1 * 60, 5 * 60), "same day: inside");
        assert!(!in_window(6 * 60, 1 * 60, 5 * 60), "same day: after");
    }

    #[test]
    fn a_window_that_meets_itself_is_the_whole_day_and_never_quiet_outside_it() {
        // `start == end` is the 24-hour case, and the helper answers "always inside" — which
        // the validator refuses to let a person save, so the two agree.
        assert!(in_window(9 * 60, 3 * 60, 3 * 60));
        assert!(validate_quiet_hours(Some("03:00"), Some("03:00")).is_err());
    }

    // ---------------------------------------------------------------------------------------
    // The clock's two spellings. Everything below is about one fact: a string this module
    // *writes* has to be a string this module can *read*. These are the tests for the case
    // where it was not — where `read_settings` produced `22:00:00` (Postgres' `::text` on a
    // `time` column) and `parse_clock` answered `None` to it, so a quiet window that had been
    // saved correctly came back as no window at all.
    //
    // The shape is deliberately a *round trip through the real cast*, not a pair of
    // `assert_eq!`s over hand-written literals: a test that feeds `parse_clock` the literal
    // `"22:00:00"` proves the parser tolerates seconds, and says nothing about whether the
    // store still *produces* that shape. The defect lived in the seam between the two, so the
    // test has to own the seam.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn the_parser_reads_every_spelling_of_a_clock_the_platform_produces() {
        assert_eq!(parse_clock(Some("22:00")), Some(22 * 60), "the form's own shape");
        assert_eq!(
            parse_clock(Some("22:00:00")),
            Some(22 * 60),
            "Postgres `::text` on a time column — the shape the store used to hand back"
        );
        assert_eq!(parse_clock(Some(" 22:00 ")), Some(22 * 60), "a hand-written client");
        assert_eq!(parse_clock(Some("7:05")), Some(7 * 60 + 5), "unpadded hour");
        assert_eq!(parse_clock(Some("00:00")), Some(0), "midnight");
    }

    #[test]
    fn the_parser_still_refuses_what_is_not_a_clock() {
        // Widening the accepted shapes must not turn the function into "everything that has
        // a colon in it": these are the values a `None` is *for*, and losing them would make a
        // corrupt row look like a quiet window that happens to cover the whole day.
        assert_eq!(parse_clock(Some("")), None);
        assert_eq!(parse_clock(Some("not a time")), None);
        assert_eq!(parse_clock(Some("24:00")), None, "there is no 24th hour");
        assert_eq!(parse_clock(Some("12:60")), None, "there is no 60th minute");
        assert_eq!(parse_clock(Some("12")), None);
        assert_eq!(parse_clock(None), None);
    }

    #[test]
    fn formatting_a_clock_produces_exactly_what_the_parser_reads() {
        // The round trip that matters, in the direction that was broken. `format_clock` is the
        // store's answer shape and `parse_clock` is every consumer's input, so the invariant
        // is one line: what one writes, the other reads.
        for (hour, minute) in [(0u8, 0u8), (7, 5), (9, 30), (22, 0), (23, 59)] {
            let value = time::Time::from_hms(hour, minute, 0).expect("a valid instant of a day");
            let written = format_clock(value);
            assert_eq!(
                parse_clock(Some(&written)),
                Some(i32::from(hour) * 60 + i32::from(minute)),
                "format_clock wrote {written:?}, which parse_clock could not read back"
            );
            // And the two always-past or always-future edges, because `22:00` is the default
            // the form offers and a parser that read it but not `00:00` would be bizarre.
            assert_eq!(written.len(), 5, "the platform's clock shape is HH:MM, five characters");
        }
    }

    #[test]
    fn a_window_survives_the_trip_through_a_padded_clock_string() {
        // The end-to-end shape of the defect, expressed without a database: a settings row
        // whose clock fields arrived in Postgres' padded spelling — the exact strings
        // `quiet_hours_start::text` produced — must still be a *window*, not the absence of
        // one. Before the fix this pair fell through `parse_clock` to `None`, and
        // `in_quiet_hours` documented "no window means false" for exactly this reason.
        let padded = Settings {
            quiet_hours_start: Some("22:00:00".to_owned()),
            quiet_hours_end: Some("07:00:00".to_owned()),
            ..settings_at(8)
        };
        assert!(
            padded.in_quiet_hours(datetime!(2026-09-28 23:30 UTC)),
            "23:30 is inside 22:00→07:00 even when the row spells the bounds with seconds"
        );
        assert!(
            !padded.in_quiet_hours(datetime!(2026-09-28 12:00 UTC)),
            "midday is outside the same window"
        );
        // The validator has to agree with the parser on the same strings, or the API accepts a
        // row and then cannot honour it: validation is the door, parsing is the room.
        assert!(
            validate_quiet_hours(
                padded.quiet_hours_start.as_deref(),
                padded.quiet_hours_end.as_deref()
            )
            .is_ok(),
            "a window that the runtime understands must pass the door"
        );
    }
}
