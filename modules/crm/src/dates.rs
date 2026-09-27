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
