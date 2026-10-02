//! A calendar date and a timestamp on the wire, in the shapes a form produces.
//!
//! `time`'s `serde` support is **opt-in per field**, and a field that forgets the attribute does
//! not fall back to something readable: a bare `time::Date` serialises as the crate's tuple, so
//! `valid_until` went out as `[2026, 270, 1]`, and a bare `OffsetDateTime` went out as
//! `[2026, 271, 22, 43, 23, 295199000, 0, 0, 0]`. Neither is a string any client can read, and a
//! timestamp that comes back as a tuple is refused on the way in — so a sales screen that tried to
//! show "valid until 2026-10-01" or "archived at …" would render nothing, and a form could not
//! send a date at all.
//!
//! This is the same pair CRM reached for its own date fields, and the same rules, because they are
//! rules about **browsers**, not about CRM: a `<input type="date">` sends `2026-10-01`, and a
//! `<input type="datetime-local">` sends `2026-10-01T09:00` with no zone. Both must be accepted, and
//! both must come back as strings.
//!
//! The three rules that follow are the whole module:
//!
//! * **A day is a day.** [`serialize`] writes `YYYY-MM-DD` and nothing else. A close date, a
//!   validity date and a quote's `valid_until` have no time and no zone; a timestamp where the
//!   writer meant a day is an off-by-one in a calendar column.
//! * **A timestamp that arrives without a zone is read as UTC.** The alternative — guessing the
//!   browser's zone from a header — would make one payload mean different instants for two
//!   people, and a document that moves by hours depending on who filed it is not a fixable bug.
//! * **A refusal names the format.** "expected a Date" tells a person nothing at a form field, so
//!   every message quotes what they typed and shows the shape they want.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::macros::format_description;
use time::Date;

/// The wire format for a day. `time`'s well-known descriptions are all timestamps; this one is a
/// day.
const DAY: &[time::format_description::FormatItem<'_>] =
    format_description!("[year]-[month]-[day]");

/// The wire format, for a caller that builds a JSON payload by hand.
///
/// `json!` never consults serde, so a `Date` placed in one is written as `time`'s tuple. Anything
/// assembling a payload with a date by hand formats it through this, and the two routes can then
/// never disagree about what a date looks like on the wire.
pub fn wire_format() -> &'static [time::format_description::FormatItem<'static>] {
    DAY
}

/// A `Date` as the string a caller receives, for a hand-built payload.
pub fn to_wire(date: &Date) -> String {
    // A `Date` always formats: the description is fixed and always valid. The fallback is
    // unreachable rather than silent, so it says so.
    date.format(DAY).unwrap_or_else(|_| {
        debug_assert!(false, "a calendar date always formats as [year]-[month]-[day]");
        String::new()
    })
}

/// Serialises a `Date` as `YYYY-MM-DD`.
pub fn serialize<S>(date: &Date, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    to_wire(date).serialize(serializer)
}

/// Reads a `Date` from `YYYY-MM-DD`, and from anything else answers a message naming the format.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Date, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse(&raw).map_err(|_| serde::de::Error::custom(day_message(&raw)))
}

/// The one parse, so both entry points give the same answer for the same input.
fn parse(raw: &str) -> Result<Date, time::error::Parse> {
    Date::parse(raw, DAY)
}

/// What a person is told when their date is not one.
fn day_message(raw: &str) -> String {
    format!("a calendar date such as 2026-12-01, not {raw:?} — the format is YYYY-MM-DD")
}

/// The same, for a field that may be absent. A missing date is `None`, never "the epoch".
///
/// `null` is `None`. An **omitted** field is a different problem and is solved on the **struct**,
/// not here: naming a `with` path replaces the field's whole deserializer, which silently
/// discards a field-level `#[serde(default)]`. A container-level `#[serde(default)]` still
/// applies — the derive fills every missing field with `Default::default()` first — so the owning
/// struct has to derive `Default`.
pub mod option {
    use super::{day_message, parse, Date, Deserializer, Serializer};
    // `Option::deserialize` is a trait method and the trait must be in scope to call it. It is
    // imported anonymously so it cannot collide with the `serialize` this module also defines.
    use serde::Deserialize as _;

    /// Serialises an optional date.
    pub fn serialize<S>(date: &Option<Date>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match date {
            Some(date) => super::serialize(date, serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Reads an optional date, and refuses a present-but-wrong value with a message a person can
    /// act on: a date is typed, and "expected a Date" tells them nothing.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Date>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<String>::deserialize(deserializer)? {
            Some(raw) => parse(&raw)
                .map(Some)
                .map_err(|_| serde::de::Error::custom(day_message(&raw))),
            None => Ok(None),
        }
    }
}

/// A timestamp on the wire, as RFC 3339 — plus the two shapes a browser sends instead.
pub mod instant {
    use time::format_description::well_known::Rfc3339;
    use time::macros::format_description;
    use time::{OffsetDateTime, PrimitiveDateTime};

    use super::{instant_message, Deserializer, Serialize, Serializer};
    use serde::Deserialize as _;

    /// What `<input type="datetime-local">` produces: a wall clock and nothing else.
    const LOCAL: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]");
    /// The same field, to the second — Safari and a person typing both do this.
    const LOCAL_SECONDS: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

    /// Read a timestamp from any of the three accepted shapes.
    pub fn parse(raw: &str) -> Result<OffsetDateTime, time::error::Parse> {
        if let Ok(parsed) = OffsetDateTime::parse(raw, &Rfc3339) {
            return Ok(parsed);
        }
        parse_as_utc(raw)
    }

    /// A zone-less wall clock, read as UTC.
    fn parse_as_utc(raw: &str) -> Result<OffsetDateTime, time::error::Parse> {
        let primitive = PrimitiveDateTime::parse(raw, LOCAL)
            .or_else(|_| PrimitiveDateTime::parse(raw, LOCAL_SECONDS))?;
        Ok(primitive.assume_utc())
    }

    /// A timestamp as the string a caller receives.
    #[must_use]
    pub fn to_wire(at: &OffsetDateTime) -> String {
        // Rfc3339 on a valid instant cannot fail; the fallback is unreachable, and says so.
        at.format(&Rfc3339).unwrap_or_else(|_| {
            debug_assert!(false, "an OffsetDateTime always formats as RFC 3339");
            String::new()
        })
    }

    /// Serialises a timestamp as RFC 3339.
    pub fn serialize<S>(at: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        to_wire(at).serialize(serializer)
    }

    /// Reads a timestamp, and answers a message naming the shape when it is not one.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).map_err(|_| serde::de::Error::custom(instant_message(&raw)))
    }

    /// The same, for a field that may be absent.
    pub mod option {
        use super::{parse, Deserializer, OffsetDateTime, Serializer};
        use super::super::instant_message;
        use serde::Deserialize as _;

        /// Serialises an optional timestamp.
        pub fn serialize<S>(at: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match at {
                Some(at) => super::serialize(at, serializer),
                None => serializer.serialize_none(),
            }
        }

        /// Reads an optional timestamp.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            match Option::<String>::deserialize(deserializer)? {
                Some(raw) => parse(&raw)
                    .map(Some)
                    .map_err(|_| serde::de::Error::custom(instant_message(&raw))),
                None => Ok(None),
            }
        }
    }
}

/// What a person is told when a timestamp is not one.
fn instant_message(raw: &str) -> String {
    format!(
        "a timestamp such as 2026-10-01T09:00:00Z, not {raw:?} — \
         the format is ISO 8601, and a time without a zone is read as UTC"
    )
}

#[cfg(test)]
mod tests {
    use super::{Date, Deserialize, Serialize};
    use time::macros::datetime;
    use time::Month;

    // Container-level `default` is what makes an *omitted* field legal: a field-level
    // `#[serde(default)]` is silently ignored once a `with` path is named, because the derive
    // replaces the field's whole deserializer.
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Row {
        #[serde(with = "super::option")]
        valid_until: Option<Date>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Day {
        #[serde(with = "super")]
        day: Date,
    }

    // An optional stamp names the *option* helper: a `with` path replaces the field's whole
    // (de)serializer, so the non-optional `instant` one is a type error here, not a default.
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Stamped {
        #[serde(with = "super::instant::option")]
        archived_at: Option<time::OffsetDateTime>,
    }

    fn december_first() -> Date {
        Date::from_calendar_date(2026, Month::December, 1).unwrap()
    }

    // ---- the day ---------------------------------------------------------------------------------

    #[test]
    fn a_date_round_trips_as_calendar_text() {
        let day = december_first();
        let json = serde_json::to_string(&Day { day }).unwrap();
        assert_eq!(json, r#"{"day":"2026-12-01"}"#);
        assert_eq!(serde_json::from_str::<Day>(&json).unwrap().day, day);
    }

    #[test]
    fn a_validity_window_arrives_as_the_date_input_sends_it() {
        // Exactly what `<input type="date">` hands the form. Before this module the same payload
        // was refused by the deserializer, so the price list editor's window fields could not be
        // filled in at all.
        let parsed: Row = serde_json::from_str(r#"{"valid_until":"2026-12-01"}"#)
            .expect("a calendar date parses");
        assert_eq!(parsed.valid_until, Some(december_first()));
    }

    #[test]
    fn an_absent_date_stays_absent() {
        let json = serde_json::to_string(&Row { valid_until: None }).unwrap();
        assert_eq!(json, r#"{"valid_until":null}"#);
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap().valid_until, None);
        assert_eq!(serde_json::from_str::<Row>("{}").unwrap().valid_until, None);
    }

    #[test]
    fn the_time_crate_tuple_form_is_refused_loudly() {
        // What the field looked like before this module existed: a tuple no form can produce. It
        // is still refused — the point is that it is refused *loudly*, not quietly accepted as a
        // date that happens to sort correctly.
        let error = serde_json::from_str::<Row>(r#"{"valid_until":[2026,270,1]}"#)
            .expect_err("a tuple is not a date");
        assert!(
            error.to_string().contains("expected a string")
                || error.to_string().contains("sequence"),
            "the type error names what arrived: {error}"
        );
    }

    #[test]
    fn a_timestamp_pasted_into_a_date_field_is_refused_by_name() {
        let error = serde_json::from_str::<Row>(r#"{"valid_until":"2026-12-01T10:00:00Z"}"#)
            .expect_err("a timestamp is not a calendar date");
        assert!(error.to_string().contains("2026-12-01"), "quotes the input: {error}");
    }

    // ---- the instant ------------------------------------------------------------------------------

    #[test]
    fn a_stamp_is_read_as_written_and_written_as_a_string() {
        // The bug this module was written for: `archived_at` reached the client as
        // `[2026,271,22,43,23,295199000,0,0,0]` and a screen had nothing to render.
        let at = datetime!(2026-10-01 9:00 UTC);
        let json = serde_json::to_string(&Stamped { archived_at: Some(at) }).unwrap();
        assert_eq!(json, r#"{"archived_at":"2026-10-01T09:00:00Z"}"#);

        let parsed: Stamped = serde_json::from_str(&json).expect("our own output reads back");
        assert_eq!(parsed.archived_at, Some(at));
    }

    #[test]
    fn a_browser_datetime_local_value_is_accepted() {
        for raw in [r#"{"archived_at":"2026-10-01T09:00"}"#, r#"{"archived_at":"2026-10-01T09:00:00"}"#] {
            let parsed: Stamped = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(parsed.archived_at, Some(datetime!(2026-10-01 9:00 UTC)), "{raw}");
        }
    }

    #[test]
    fn an_omitted_and_a_null_stamp_are_the_same_request() {
        let omitted: Stamped = serde_json::from_str("{}").expect("omitting a stamp is legal");
        let null: Stamped = serde_json::from_str(r#"{"archived_at":null}"#).expect("a null is legal");
        assert_eq!(omitted.archived_at, None);
        assert_eq!(null.archived_at, None);
    }

    #[test]
    fn free_text_is_refused_with_a_readable_message() {
        let error = serde_json::from_str::<Stamped>(r#"{"archived_at":"next tuesday"}"#)
            .expect_err("free text is not a timestamp");
        assert!(error.to_string().contains("next tuesday"), "quotes the input: {error}");
    }
}
