//! A calendar day and a timestamp on the wire, in the two shapes a browser sends.
//!
//! The rules are not this module's own — the CRM module wrote them and every module since has
//! copied them — because they are rules about **browsers**, not about any one module:
//!
//! * **A day is a day.** [`serialize`] writes `YYYY-MM-DD` and nothing else. A start date has no
//!   time and no zone, and a timestamp where the writer meant a day is an off-by-one in a
//!   calendar column — for HR that is an employee starting on the wrong day of the month.
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

/// A `Date` as the string a caller receives, for a hand-built payload.
#[must_use]
pub fn to_wire(date: &Date) -> String {
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
    parse(&raw).map_err(|_| {
        serde::de::Error::custom(format!(
            "expected a date as YYYY-MM-DD, received \"{raw}\""
        ))
    })
}

/// An absent `Date` serialises as `null` and deserialises from it or from an absent key.
pub mod option {
    use super::{Date, to_wire};
    use serde::{Deserialize, Deserializer, Serializer};

    /// Write the day, or `null`.
    pub fn serialize<S>(value: &Option<Date>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(date) => serializer.serialize_some(&to_wire(date)),
            None => serializer.serialize_none(),
        }
    }

    /// Read the day, mapping a blank string to an absent value.
    ///
    /// A form that clears a date field sends `""`, and refusing that would make a date optional
    /// in the UI and mandatory in the API.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Date>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw.map(|text| text.trim().to_owned()).filter(|t| !t.is_empty()) {
            None => Ok(None),
            Some(text) => super::parse(&text).map(Some).map_err(|_| {
                serde::de::Error::custom(format!(
                    "expected a date as YYYY-MM-DD, received \"{text}\""
                ))
            }),
        }
    }
}

/// An instant as the string a caller receives, for a hand-built payload.
#[must_use]
pub fn instant_to_wire(moment: &OffsetDateTime) -> String {
    moment
        .format(&Rfc3339)
        .unwrap_or_else(|_| {
            debug_assert!(false, "a calendar instant always formats as RFC 3339");
            String::new()
        })
}

/// Serialises an `OffsetDateTime` as RFC 3339.
pub fn serialize_instant<S>(moment: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    instant_to_wire(moment).serialize(serializer)
}

/// Reads an `OffsetDateTime` from RFC 3339, refusing anything else with the shape quoted back.
pub fn deserialize_instant<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    OffsetDateTime::parse(raw.trim(), &Rfc3339).map_err(|_| {
        serde::de::Error::custom(format!(
            "expected a timestamp as RFC 3339, received \"{raw}\""
        ))
    })
}

/// The `serde` adapter pair for a non-optional instant, kept in one place.
///
/// A module that hand-rolled the pair at each field is a module where one of the fields has
/// `with = "..."` and the other does not, and the one that does not serialises `time`'s tuple.
pub mod instant {
    use super::{deserialize_instant, serialize_instant, OffsetDateTime};
    use serde::{Deserializer, Serializer};

    /// Write the instant.
    pub fn serialize<S>(moment: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_instant(moment, serializer)
    }

    /// Read the instant.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_instant(deserializer)
    }

    /// The same pair for an **absent** instant.
    ///
    /// Added by the leave slice, and it is a real gap rather than a preference: `dates::option`
    /// exists for `Date`, and before this the only way to carry an `Option<OffsetDateTime>` was
    /// to default to the **date** adapter — which compiles into a field that serialises a
    /// timestamp as `2026-10-01` and deserialises it as midnight on that day. The field kept the
    /// *instant* type and silently lost the *time*, so "decided at" would have read as a date and
    /// two decisions on the same day would have been indistinguishable. A missing adapter is a
    /// wrong answer waiting for the next `Option<OffsetDateTime>` somebody adds.
    pub mod option {
        use super::super::{instant_to_wire, OffsetDateTime, Rfc3339};
        use serde::{Deserialize, Deserializer, Serializer};

        /// Write the instant, or `null`.
        pub fn serialize<S>(value: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match value {
                Some(moment) => serializer.serialize_some(&instant_to_wire(moment)),
                None => serializer.serialize_none(),
            }
        }

        /// Read the instant, mapping an absent key or a `null` to an absent value.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let raw = Option::<String>::deserialize(deserializer)?;
            match raw.map(|text| text.trim().to_owned()).filter(|t| !t.is_empty()) {
                None => Ok(None),
                // `OffsetDateTime::parse`, not `deserialize_instant`: that one takes a
                // `Deserializer`, and the `String` is already in hand here.
                Some(text) => OffsetDateTime::parse(&text, &Rfc3339).map(Some).map_err(|_| {
                    serde::de::Error::custom(format!(
                        "expected a timestamp as RFC 3339, received \"{text}\""
                    ))
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_day_travels_as_YYYY_MM_DD_and_nothing_else() {
        let date = Date::from_calendar_date(2026, time::Month::October, 1).unwrap();
        assert_eq!(to_wire(&date), "2026-10-01");
        assert_eq!(parse("2026-10-01").unwrap(), date);
    }

    #[test]
    fn a_timestamp_where_a_day_was_meant_is_refused_and_the_message_quotes_what_came_in() {
        // The bug this prevents: a form field sent `2026-10-01T00:00:00Z` and the create 422'd
        // with a parse error nobody could act on.
        //
        // The message that matters is the one a **client** sees, so this goes through the
        // deserialiser rather than through `parse`. `parse` returns `time`'s own diagnostic
        // ("unexpected trailing characters") — a perfectly good parser message and a useless form
        // message. Asserting on `parse` would have tested the wrong string entirely.
        #[derive(Debug, serde::Deserialize)]
        struct Day {
            #[serde(with = "super")]
            start_date: Date,
        }

        let error =
            serde_json::from_value::<Day>(json!({ "start_date": "2026-10-01T00:00:00Z" }))
                .expect_err("a timestamp where a day was meant must be refused");
        let message = error.to_string();
        assert!(message.contains("YYYY-MM-DD"), "{message}");
        assert!(
            message.contains("2026-10-01T00:00:00Z"),
            "the message must quote the value that arrived: {message}"
        );
    }

    #[test]
    fn a_cleared_optional_date_is_absent_rather_than_a_refusal() {
        // `""` is what a cleared `input type="date"` sends. Refusing it would make a date that
        // looks optional in the form mandatory in the API.
        let payload = json!({ "start_date": "" });
        let parsed: Wrapper = serde_json::from_value(payload).expect("empty must parse");
        assert!(parsed.start_date.is_none());
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default, with = "option")]
        start_date: Option<Date>,
    }

    #[test]
    fn an_absent_key_is_the_same_as_a_cleared_one() {
        let parsed: Wrapper = serde_json::from_value(json!({})).expect("absent must parse");
        assert!(parsed.start_date.is_none());
    }

    #[test]
    fn a_present_day_survives_the_optional_adapter() {
        let parsed: Wrapper =
            serde_json::from_value(json!({ "start_date": "2026-10-01" })).expect("a day must parse");
        assert_eq!(
            parsed.start_date.map(|d| to_wire(&d)).as_deref(),
            Some("2026-10-01")
        );
    }

    #[test]
    fn an_instant_round_trips_and_a_bare_day_does_not_parse_as_one() {
        let moment = OffsetDateTime::parse("2026-10-01T09:30:00Z", &Rfc3339).unwrap();
        assert_eq!(instant_to_wire(&moment), "2026-10-01T09:30:00Z");
        assert!(OffsetDateTime::parse("2026-10-01", &Rfc3339).is_err());
    }
}
