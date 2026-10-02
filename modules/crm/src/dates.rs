//! A calendar date on the wire, as `YYYY-MM-DD`.
//!
//! `time` ships `serde` helpers for `OffsetDateTime` (`rfc3339`) but **not** for a bare
//! `time::Date`, and its default representation would put `[2026, 270, 1]` on the wire. Both are
//! wrong for a field a person types into a date input: the API has to accept the same string the
//! browser's `<input type="date">` produces, and answer the same string back.
//!
//! So this is the module a CRM date field names. It is deliberately *date only* — a close date
//! has no time and no zone, and a timestamp where the writer meant a day is a bug that shows up
//! as an off-by-one in the calendar column.
//!
//! The second half of the file is [`instant`], the same idea for a field that *does* carry a time
//! (a task's due date). That one exists because a bare `OffsetDateTime` is the mistake this module
//! was written to end: `time`'s `serde` support for it is opt-in per field, and a field that
//! forgets the attribute accepts **no** JSON string at all — which reads, at the form, as a task
//! that refuses to be given a due date.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::macros::format_description;
use time::Date;

/// The wire format. `time`'s own well-known descriptions are all timestamps; this one is a day.
const FORMAT: &[time::format_description::FormatItem<'_>] =
    format_description!("[year]-[month]-[day]");

/// The wire format, for a caller that builds a JSON payload by hand.
///
/// `json!` never consults serde, so a `Date` placed in one is written as `time`'s tuple. Anything
/// assembling a payload containing a date by hand formats it through this, and the two routes
/// can then never disagree about what a date looks like on the wire.
pub fn wire_format() -> &'static [time::format_description::FormatItem<'static>] {
    FORMAT
}

/// A `Date` as the string a caller would receive, for a hand-built payload.
pub fn to_wire(date: &Date) -> String {
    // A `Date` always formats: the description is a fixed, always-valid one. The fallback is
    // unreachable rather than silent, so it says so.
    date.format(FORMAT).unwrap_or_else(|_| {
        debug_assert!(false, "a calendar date always formats as [year]-[month]-[day]");
        String::new()
    })
}

/// Serialises a `Date` as `YYYY-MM-DD`.
pub fn serialize<S>(date: &Date, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let text = date.format(FORMAT).map_err(serde::ser::Error::custom)?;
    text.serialize(serializer)
}

/// Reads a `Date` from `YYYY-MM-DD`, and from anything else answers a message naming the format.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Date, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse(&raw).map_err(|_| serde::de::Error::custom(message(&raw)))
}

/// The one parse, so both entry points give the same answer for the same input.
fn parse(raw: &str) -> Result<Date, time::error::Parse> {
    Date::parse(raw, FORMAT)
}

/// What a person is told when their date is not one. It quotes what they typed and names the
/// shape they want — a bare "expected a Date" is not actionable at a form field.
fn message(raw: &str) -> String {
    format!("a calendar date such as 2026-12-01, not {raw:?} — the format is YYYY-MM-DD")
}

/// The same, for a field that may be absent. A missing date is `None`, never "the epoch".
///
/// `null` is `None`. An **omitted** field is a different problem, and it is solved on the
/// **struct**, not here: naming a `with` path replaces the field's whole deserializer, which
/// silently discards a field-level `#[serde(default)]`. A `#[serde(default)]` on the container
/// still applies — the derive fills every missing field with `Default::default()` first — so the
/// owning struct has to derive `Default`.
pub mod option {
    use super::{Date, Deserializer, Serializer, message, parse};
    // `Option::deserialize` is a trait method, and the trait must be in scope to call it. It is
    // imported anonymously so it cannot collide with the `serialize` this module also defines.
    use serde::Deserialize as _;

    pub fn serialize<S>(date: &Option<Date>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match date {
            Some(date) => super::serialize(date, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Date>, D::Error>
    where
        D: Deserializer<'de>,
    {
        // What this must get right is that a present-but-wrong value is refused with a message a
        // person can act on: a close date is typed, and "expected a Date" tells them nothing.
        let raw: Option<String> = Option::deserialize(deserializer)?;
        match raw {
            Some(raw) => parse(&raw).map(Some).map_err(|_| serde::de::Error::custom(message(&raw))),
            None => Ok(None),
        }
    }
}

/// A timestamp on the wire, as RFC 3339 — plus the two shapes a browser sends instead.
///
/// `time`'s `rfc3339` helper is strict and correct, but a `<input type="datetime-local">` hands
/// the form `2026-10-01T09:00` with **no zone**, and a person in Turkey meant that as their own
/// wall clock. Two rules follow, and both belong here rather than in each route:
///
/// * **RFC 3339 with an offset** is read as written (`2026-10-01T09:00:00Z`).
/// * **A local wall clock with no offset** is read as UTC. The alternative — guessing the
///   browser's zone from a header — would make the same payload mean different instants for two
///   people, and a task that moves by hours depending on who created it is not a fixable bug.
///   A timestamp field stores what the server can agree on, and the panel sends an offset when it
///   has one.
///
/// The zone-less value is therefore *not* a silent assumption in the output: `to_wire` always
/// answers an explicit offset, so a value read from this module and written back is unambiguous
/// even though the value that came in was not.
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
        // `instant_message` is a private helper of the parent `dates` module, one level further
        // up, so the refusal a blank-or-wrong field gets is the same sentence either way in.
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde::{Deserialize, Serialize};
        use time::macros::datetime;
        use time::{Date, Month, Time};

        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        struct Task {
            #[serde(with = "super::option")]
            due_at: Option<OffsetDateTime>,
        }

        #[test]
        fn a_zoned_timestamp_is_read_as_written() {
            let row: Task = serde_json::from_str(r#"{"due_at":"2026-10-01T09:00:00Z"}"#)
                .expect("an RFC 3339 instant parses");
            assert_eq!(row.due_at, Some(datetime!(2026-10-01 9:00 UTC)));
            assert_eq!(super::to_wire(&datetime!(2026-10-01 9:00 UTC)), "2026-10-01T09:00:00Z");
        }

        #[test]
        fn a_browser_datetime_local_value_is_accepted() {
            // Exactly what `<input type="datetime-local">` hands the form.
            for raw in [r#"{"due_at":"2026-10-01T09:00"}"#, r#"{"due_at":"2026-10-01T09:00:00"}"#] {
                let row: Task = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
                assert_eq!(row.due_at, Some(datetime!(2026-10-01 9:00 UTC)), "{raw}");
            }
        }

        #[test]
        fn an_omitted_and_a_null_due_date_are_the_same_request() {
            let omitted: Task = serde_json::from_str("{}").expect("omitting a due date is legal");
            let null: Task = serde_json::from_str(r#"{"due_at":null}"#).expect("a null is legal");
            assert_eq!(omitted.due_at, None);
            assert_eq!(null.due_at, None);
        }

        #[test]
        fn what_comes_back_always_carries_an_offset() {
            // The value that came in had no zone; the value that goes out must, or the next
            // reader cannot tell whether it was local or UTC.
            let row: Task = serde_json::from_str(r#"{"due_at":"2026-10-01T09:00"}"#).unwrap();
            let out = serde_json::to_string(&row).unwrap();
            assert!(out.contains('Z'), "{out}");
        }

        #[test]
        fn a_timestamp_the_wire_cannot_describe_is_refused_with_a_readable_message() {
            let error = serde_json::from_str::<Task>(r#"{"due_at":"next tuesday"}"#)
                .expect_err("free text is not a timestamp");
            assert!(error.to_string().contains("next tuesday"), "quotes the input: {error}");
        }

        #[test]
        fn the_time_crate_tuple_form_is_refused() {
            let error = serde_json::from_str::<Task>(r#"{"due_at":[2026,273,9,0,0,0,0,0,0]}"#)
                .expect_err("a tuple is not a timestamp");
            assert!(error.to_string().contains("string"), "names the type: {error}");
        }

        #[test]
        fn midnight_on_the_first_is_accepted_as_a_day_and_a_time() {
            let at = parse("2026-01-01T00:00").expect("midnight parses");
            assert_eq!(at.date(), Date::from_calendar_date(2026, Month::January, 1).unwrap());
            assert_eq!(at.time(), Time::MIDNIGHT);
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
    use time::Month;

    // Container-level `default` is what makes an *omitted* field legal. A field-level
    // `#[serde(default)]` is silently ignored once a `with` path is named — the derive replaces
    // the field's whole deserializer — so the attribute has to sit on the struct, where it fills
    // every missing field with `Default::default()` before any field deserializer runs. `Row`
    // derives `Default` for exactly that reason: `Option<Date>` is `None` and nothing else is
    // needed to supply it.
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Row {
        #[serde(with = "super::option")]
        close_on: Option<Date>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Day {
        #[serde(with = "super")]
        day: Date,
    }

    fn december_first() -> Date {
        Date::from_calendar_date(2026, Month::December, 1).unwrap()
    }

    #[test]
    fn a_date_round_trips_as_calendar_text() {
        let day = december_first();
        let json = serde_json::to_string(&Day { day }).unwrap();
        assert_eq!(json, r#"{"day":"2026-12-01"}"#);
        assert_eq!(serde_json::from_str::<Day>(&json).unwrap().day, day);
    }

    #[test]
    fn an_absent_date_stays_absent() {
        let json = serde_json::to_string(&Row { close_on: None }).unwrap();
        assert_eq!(json, r#"{"close_on":null}"#);
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap().close_on, None);
        // A field missing from the payload is the same as an explicit null.
        assert_eq!(serde_json::from_str::<Row>("{}").unwrap().close_on, None);
    }

    #[test]
    fn a_date_comes_back_whole() {
        let parsed: Row =
            serde_json::from_str(r#"{"close_on":"2026-09-30"}"#).expect("a calendar date parses");
        let expected = Date::from_calendar_date(2026, Month::September, 30).unwrap();
        assert_eq!(parsed.close_on, Some(expected));
    }

    #[test]
    fn the_time_crate_tuple_form_is_refused() {
        // What the field looked like before this module existed: a tuple no form can produce.
        // It is still refused — the point is that it is refused *loudly*, and not quietly
        // accepted as a date that happens to sort correctly.
        let error = serde_json::from_str::<Row>(r#"{"close_on":[2026,270,1]}"#)
            .expect_err("a tuple is not a date");
        assert!(
            error.to_string().contains("expected a string") || error.to_string().contains("sequence"),
            "the type error names what arrived: {error}"
        );
    }

    #[test]
    fn an_omitted_field_and_a_null_field_are_the_same_request() {
        // The two shapes a real form produces when the person left the date blank. If these ever
        // disagree, one of them starts 422-ing and the blank field silently becomes uneditable.
        let omitted: Row = serde_json::from_str("{}").expect("an omitted date is not an error");
        let null: Row = serde_json::from_str(r#"{"close_on":null}"#).expect("a null date is not an error");
        assert_eq!(omitted.close_on, None);
        assert_eq!(null.close_on, None);
    }

    #[test]
    fn a_timestamp_is_refused_with_a_readable_message() {
        // A person pasting a full timestamp into a date field is the mistake worth naming.
        let error = serde_json::from_str::<Row>(r#"{"close_on":"2026-12-01T10:00:00Z"}"#)
            .expect_err("a timestamp is not a calendar date");
        assert!(error.to_string().contains("2026-12-01"), "quotes the input: {error}");
    }
}
