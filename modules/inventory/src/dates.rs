//! How a timestamp crosses the wire.
//!
//! `sqlx` gives the module an `OffsetDateTime` and `serde` has to hand a browser a string. The
//! two disagree about representation by default, and a disagreement here is invisible until a
//! screen sorts by a column and the order is subtly wrong — so the format is written down once,
//! in one place, and every timestamp in this module uses it.
//!
//! **UTC, always, with a `Z`.** An inventory ledger is read by somebody in a warehouse and
//! reconciled by somebody in an office, possibly in another country; a timestamp with no zone on it
//! is a question the reader has to answer alone, and the answer is usually wrong.

use serde::{Deserialize, Deserializer, Serializer};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The wire format every timestamp in this module is written in.
///
/// A function rather than a `const`, because the two shapes of `time`'s format items
/// (`FormatItem` and `BorrowedFormatItem`) are not the same type and a caller that names the
/// wrong one gets a type error rather than a silently different format. One function, one type.
#[must_use]
pub fn wire_format() -> &'static [time::format_description::FormatItem<'static>] {
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z")
}

/// The name used in the `serde(with = …)` attributes, so a reader of a view sees which format the
/// column is in rather than having to follow it to a macro.
pub use wire_format as WIRE;

/// Render a timestamp the way the API writes it.
#[must_use]
pub fn to_wire(at: &OffsetDateTime) -> String {
    // A timestamp out of the format's range is a bug in the writer, not a value to render as
    // something plausible: `to_string` would panic and a screen would lose the whole page.
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| at.to_offset(time::UtcOffset::UTC).unix_timestamp().to_string())
}

/// Parse a timestamp a client sent.
pub fn parse(raw: &str) -> Result<OffsetDateTime, time::error::Parse> {
    OffsetDateTime::parse(raw, &Rfc3339)
}

/// Serialize a non-optional timestamp.
pub fn serialize<S>(at: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&to_wire(at))
}

/// Deserialize a timestamp, naming the field the form got wrong.
pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse(&raw).map_err(serde::de::Error::custom)
}

/// The optional variants, which is where a `None` has to be a JSON `null` and not a string
/// `"null"` — a mistake that turns a missing movement into a parse error on the whole page.
pub mod option {
    use super::{OffsetDateTime, Serializer, to_wire};
    use serde::{Deserialize, Deserializer};

    /// Serialize an optional timestamp.
    pub fn serialize<S>(at: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match at {
            Some(at) => serializer.serialize_str(&to_wire(at)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize an optional timestamp.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw {
            None => Ok(None),
            // A client that sends an empty string means "no value", not "the epoch": a form that
            // clears a field submits "" and refusing it would make the field unsubmittable.
            Some(raw) if raw.trim().is_empty() => Ok(None),
            Some(raw) => super::parse(&raw).map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// The non-optional timestamp helpers, under the name the views use.
///
/// A **second name for the same functions** rather than a copy: `store.rs` writes
/// `#[serde(with = "crate::dates::instant")]` on its `created_at` fields and
/// `"crate::dates::instant::option"` on its nullable ones, and that reads as "an instant, maybe
/// absent" — which is exactly the distinction a reviewer is checking when they read a view. Two
/// modules with two implementations would be the thing that drifts; two names over one
/// implementation cannot.
pub mod instant {
    use super::{OffsetDateTime, Serializer, to_wire};
    use serde::{Deserialize, Deserializer};

    /// Serialize a timestamp.
    pub fn serialize<S>(at: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&to_wire(at))
    }

    /// Deserialize a timestamp.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        super::parse(&raw).map_err(serde::de::Error::custom)
    }

    /// The nullable variant, which is a JSON `null` rather than the string `"null"`.
    pub mod option {
        use super::{OffsetDateTime, Serializer, to_wire};
        use serde::{Deserialize, Deserializer};

        /// Serialize an optional timestamp.
        pub fn serialize<S>(at: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match at {
                Some(at) => serializer.serialize_str(&to_wire(at)),
                None => serializer.serialize_none(),
            }
        }

        /// Deserialize an optional timestamp, treating `""` as absent.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let raw = Option::<String>::deserialize(deserializer)?;
            match raw {
                None => Ok(None),
                Some(raw) if raw.trim().is_empty() => Ok(None),
                Some(raw) => super::super::parse(&raw)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_timestamp_round_trips_through_the_wire_format() {
        let at = OffsetDateTime::parse("2026-09-29T08:15:00Z", &Rfc3339).unwrap();
        assert_eq!(to_wire(&at), "2026-09-29T08:15:00Z");
        assert_eq!(parse(&to_wire(&at)).unwrap(), at);
    }

    #[test]
    fn a_non_utc_timestamp_is_converted_rather_than_relabelled() {
        // 11:15 in Istanbul is 08:15 UTC. Writing the local time with a `Z` would put every
        // movement in the ledger two hours out from when it happened.
        let local = OffsetDateTime::parse("2026-09-29T11:15:00+03:00", &Rfc3339).unwrap();
        assert_eq!(to_wire(&local), "2026-09-29T08:15:00Z");
    }

    /// A stand-in for a view, so the tests go through the **same** `serde(with = …)` the real
    /// columns use rather than through `Option<OffsetDateTime>`'s own derive.
    ///
    /// The first version of this test deserialized a bare `Option<OffsetDateTime>` and failed with
    /// "invalid type: string, expected an `OffsetDateTime`" — which is the derive's doing, not the
    /// module's, so the test was proving nothing about the code the views use.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Row {
        #[serde(with = "super::instant::option")]
        last_movement_at: Option<OffsetDateTime>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Required {
        #[serde(with = "super::instant")]
        created_at: OffsetDateTime,
    }

    #[test]
    fn an_absent_timestamp_is_null_and_not_the_string_null() {
        let rendered = serde_json::to_value(Row {
            last_movement_at: None,
        })
        .unwrap();
        assert!(
            rendered["last_movement_at"].is_null(),
            "a missing movement is JSON null: {rendered}"
        );
        let back: Row = serde_json::from_value(json!({
            "last_movement_at": "2026-09-29T08:15:00Z"
        }))
        .unwrap();
        assert!(back.last_movement_at.is_some());
    }

    #[test]
    fn a_cleared_field_is_absent_rather_than_an_error() {
        // A form that clears a field submits "", and the stock list has no last movement yet;
        // neither may fail to deserialize the whole row.
        let cleared: Row = serde_json::from_value(json!({ "last_movement_at": "" }))
            .expect("an empty string means no value");
        assert!(cleared.last_movement_at.is_none());
    }

    #[test]
    fn a_required_timestamp_round_trips_through_the_views_own_serializer() {
        let at = OffsetDateTime::parse("2026-09-29T08:15:00Z", &Rfc3339).unwrap();
        let rendered = serde_json::to_value(Required { created_at: at }).unwrap();
        assert_eq!(rendered["created_at"], "2026-09-29T08:15:00Z");
        let back: Required = serde_json::from_value(rendered).unwrap();
        assert_eq!(back.created_at, at);
    }
}
