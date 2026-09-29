//! A calendar day and a timestamp on the wire, in the two shapes a browser sends.
//!
//! The rules are not this module's own — CRM wrote them and sales and inventory repeated them —
//! because they are rules about **browsers**, not about any one module:
//!
//! * **A day is a day.** [`serialize`] writes `YYYY-MM-DD` and nothing else. An entry date has no
//!   time and no zone, and a timestamp where the writer meant a day is an off-by-one in a
//!   calendar column.
//! * **A timestamp that arrives without a zone is read as UTC.** Guessing the browser's zone
//!   from a header would make one payload mean different instants for two people.
//! * **A refusal names the format.** "expected a Date" tells a person nothing at a form field, so
//!   every message quotes what they typed and shows the shape they want.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

/// The wire format for a day: `[year]-[month]-[day]`, not a timestamp description.
const DAY: &[time::format_description::FormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]");

/// The shape `<input type="datetime-local">` sends: a date and a time with **no zone**.
///
/// Its own description rather than a reuse of [`DAY`]. The first version of this module parsed the
/// zoneless form with the day format and the unit test caught it immediately: a `DAY` parse of
/// `2026-10-01T09:00` is `InvalidLiteral`, so a `datetime-local` field would have been
/// **unsubmittable** — and nothing in the type system or the compiler says so.
const NAIVE_LOCAL: &[time::format_description::FormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]");

/// A `Date` as the string a caller receives, for a hand-built payload.
///
/// `json!` never consults serde, so a `Date` placed in one is written as `time`'s tuple. Anything
/// assembling a payload with a date by hand formats it through this, and the two routes can then
/// never disagree about what a date looks like on the wire.
#[must_use]
pub fn to_wire(date: &Date) -> String {
    // A `Date` always formats: the description is fixed and always valid. The fallback is
    // unreachable rather than silent, so it says so.
    date.format(DAY).unwrap_or_else(|_| {
        debug_assert!(false, "a calendar date always formats as [year]-[month]-[day]");
        String::new()
    })
}

/// Parse a day a client sent.
pub fn parse(raw: &str) -> Result<Date, time::error::Parse> {
    Date::parse(raw.trim(), DAY)
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

/// What a person is told when their date is not one.
fn day_message(raw: &str) -> String {
    format!("a calendar date such as 2026-12-01, not {raw:?} — the format is YYYY-MM-DD")
}

/// A timestamp in the only format the module writes: RFC 3339, in UTC.
#[must_use]
pub fn instant_to_wire(at: &OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| at.to_offset(time::UtcOffset::UTC).unix_timestamp().to_string())
}

/// Parse a timestamp a client sent.
///
/// A timestamp **without** a zone is read as UTC rather than refused: `<input
/// type="datetime-local">` sends `2026-10-01T09:00`, and refusing it would make a field
/// unsubmittable. Reading it as UTC is the alternative that does not move a document depending on
/// who filed it.
pub fn parse_instant(raw: &str) -> Result<OffsetDateTime, time::error::Parse> {
    let trimmed = raw.trim();
    match OffsetDateTime::parse(trimmed, &Rfc3339) {
        Ok(at) => Ok(at),
        Err(error) => {
            // The zoneless form gets its OWN description. Reusing the day description here is the
            // mistake a type cannot catch: it compiles, it looks right, and every
            // `datetime-local` field silently refuses to submit.
            let naive = time::PrimitiveDateTime::parse(trimmed, NAIVE_LOCAL).map_err(|_| error)?;
            Ok(naive.assume_utc())
        }
    }
}

/// The non-optional timestamp helpers, under the name the views use.
pub mod instant {
    use super::{OffsetDateTime, Serializer, instant_to_wire};
    use serde::{Deserialize, Deserializer};

    /// Serialize a timestamp.
    pub fn serialize<S>(at: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&instant_to_wire(at))
    }

    /// Deserialize a timestamp.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        super::parse_instant(&raw).map_err(serde::de::Error::custom)
    }

    /// The nullable variant, which is a JSON `null` rather than the string `"null"`.
    pub mod option {
        use super::{OffsetDateTime, Serializer, instant_to_wire};
        use serde::{Deserialize, Deserializer};

        /// Serialize an optional timestamp.
        pub fn serialize<S>(at: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match at {
                Some(at) => serializer.serialize_str(&instant_to_wire(at)),
                None => serializer.serialize_none(),
            }
        }

        /// Deserialize an optional timestamp, treating `""` as absent.
        ///
        /// A form that clears a field submits `""`, and refusing it would make the field
        /// unsubmittable rather than empty.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let raw = Option::<String>::deserialize(deserializer)?;
            match raw {
                None => Ok(None),
                Some(raw) if raw.trim().is_empty() => Ok(None),
                Some(raw) => super::super::parse_instant(&raw)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn a_day_crosses_the_wire_as_a_day() {
        let day = date!(2026 - 10 - 01);
        assert_eq!(to_wire(&day), "2026-10-01");
        assert_eq!(parse("2026-10-01").expect("parses"), day);
    }

    #[test]
    fn a_datetime_local_without_a_zone_is_read_as_utc() {
        // What `<input type="datetime-local">` actually sends. Refusing it would make a field
        // unsubmittable; reading it as local time would move the document depending on who filed.
        let at = parse_instant("2026-10-01T09:00").expect("parses");
        assert_eq!(instant_to_wire(&at), "2026-10-01T09:00:00Z");
    }

    #[test]
    fn a_timestamp_with_a_zone_keeps_it() {
        let at = parse_instant("2026-10-01T09:00:00+02:00").expect("parses");
        assert_eq!(instant_to_wire(&at), "2026-10-01T07:00:00Z");
    }
}
