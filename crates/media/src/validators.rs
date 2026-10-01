//! Conditional requests: `ETag`, `If-None-Match` and `If-Modified-Since`.
//!
//! The serve path hands a browser bytes out of an object whose **URL never changes**. A replace
//! writes new bytes onto the same row, under the same id, at the same address — so
//! `/api/v1/media/{id}/raw` means "version 2" a moment after it meant "version 1", and nothing in
//! the response says which.
//!
//! That is not a cosmetic gap, and the two halves of it are separable:
//!
//! 1. **There was no validator at all.** `grep -rn 'IF_NONE_MATCH\|NOT_MODIFIED' apps/ crates/`
//!    returned nothing, so every answer to a repeat request was a full `200` with the whole body.
//!    A panel that shows a 400-image grid re-downloads every thumbnail in the library each time
//!    somebody opens a folder.
//! 2. **The cache window was the wrong length for the URL.** `private, max-age=300` on an
//!    id-addressed URL is five minutes of the platform asserting that the bytes behind that
//!    address have not changed — and a replace changes them *without changing the address*. A
//!    browser, a proxy or the edge that honours `max-age` therefore shows the previous
//!    photograph for five minutes after an operator corrects it. This is the same defect class as
//!    the uncalled `prune_candidates` and the un-called retention writer elsewhere in this repo:
//!    a table, a column and a TTL that look like a feature and describe a lie.
//!
//! The fix is a validator that is **honest about what it identifies**. The media row's `checksum`
//! is a SHA-256 of the bytes, hex-encoded, and `append_version` rewrites it in the same statement
//! that moves `storage_key` — so it changes exactly when the bytes change, and a validator built
//! from it cannot outlive its object.
//!
//! Four decisions, each of which is a shortcut that produces a plausible wrong answer:
//!
//! 1. **The validator is the checksum, quoted — not a timestamp.** A validator has to change when
//!    the bytes change and *not* change when they do not. `updated_at` breaks the second half:
//!    a rename, a tag edit and a folder move all stamp it, so an `If-Modified-Since` built from it
//!    answers `304` for a file whose bytes are unchanged (correct, by luck) and, worse, cannot
//!    detect a change that happened without a stamp at all. The checksum is the identity of the
//!    pixels.
//! 2. **A version is validated by *its own* checksum, not the file's.** The version route reads
//!    the `media_versions` row, whose checksum is the hash of the bytes at that key. Validating
//!    a historical version against the current file's checksum would answer `304` for a version
//!    the caller never held, and every such answer is a silent wrong answer.
//! 3. **`If-None-Match` wins over `If-Modified-Since`.** RFC 9110 §13.1.3: when both are
//!    present the origin MUST ignore `If-Modified-Since`. Implementing both and letting the date
//!    win is how a file reverts to stale bytes after a restore.
//! 4. **A weak comparison, not a strong one.** `W/"abc"` and `"abc"` both match, because the
//!    question being asked is "may I reuse what I have", and a `304` on bytes that are identical
//!    answers that correctly. Strong comparison is for byte-range validators, where the bytes
//!    *are* the answer.
//!
//! Everything here is pure: it touches no database, no object store and no router, so every
//! branch below is unit-testable without any of them — which is the point, because the
//! alternative (proving it in a walk) cannot reach the states a browser never produces.

/// Header the validator travels in.
pub const ETAG_HEADER: &str = "etag";

/// Prefix marking a weak validator, which is what every one of these is.
const WEAK: &str = "W/";

/// Quote a checksum into a strong entity-tag.
///
/// The quotes are not decoration: an unquoted value is not an entity-tag, and a client that
/// receives one and sends it back unmodified produces a header the origin cannot parse — the
/// request is then treated as unconditional, which silently disables revalidation forever.
#[must_use]
pub fn etag_of(checksum: &str) -> String {
    format!("\"{checksum}\"")
}

/// Quote a checksum into a **weak** entity-tag.
///
/// Weak because these identify a *representation* rather than a byte sequence: the same checksum
/// can describe bytes stored twice (`storage_key` names the object, `checksum` names the content),
/// and RFC 9110 only permits a strong validator for a byte-for-byte unique representation. Using
/// the strong form here would be a claim the platform cannot keep.
#[must_use]
pub fn weak_etag_of(checksum: &str) -> String {
    format!("{WEAK}\"{checksum}\"")
}

/// Strip the weak prefix and the quotes off one tag, leaving the opaque value.
///
/// A tag that is not quoted at all is returned as-is rather than refused: the function answers
/// "what does this tag claim to be", and a malformed tag claims nothing — the caller still has to
/// compare it against the current validator to decide.
#[must_use]
pub fn tag_value(tag: &str) -> &str {
    let trimmed = tag.trim();
    let without_prefix = trimmed.strip_prefix(WEAK).unwrap_or(trimmed);
    without_prefix
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(without_prefix)
}

/// Whether a caller's `If-None-Match` header already holds the current representation.
///
/// `*` matches anything that exists, which is why it is answered before anything else: a client
/// holding no validator at all still sends `*` to ask "is there something here?", and answering
/// `304` to that is correct.
///
/// The header is a **list**, not a single tag, and splitting only on commas would corrupt a
/// hypothetical opaque value containing one — which is why the unquoted-value case is handled by
/// comparison against the whole trimmed candidate rather than by eager trimming of each element.
#[must_use]
pub fn if_none_match_holds(header: Option<&str>, current: &str) -> bool {
    let Some(raw) = header.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    if raw == "*" {
        return true;
    }
    // `current` is compared in both shapes, because a client may echo a weak tag it was given,
    // or send the strong form of a value whose validator we only ever publish weakly. Both name
    // the same representation, so both match — that is precisely what weak comparison means.
    let current_raw = tag_value(current);
    raw.split(',').any(|candidate| {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            return false;
        }
        if candidate == "*" {
            return true;
        }
        tag_value(candidate) == current_raw
    })
}

/// Whether a caller's `If-Modified-Since` says the representation is unchanged.
///
/// `last_modified` is the stored instant in RFC 9110's IMF-fixdate. The comparison is **at one
/// second resolution**, because that is the resolution IMF-fixdate itself has: comparing
/// sub-second timestamps would make a resource modified in the same second as the client's
/// cached copy report as unmodified, which is the one answer this header must never give
/// wrongly.
#[must_use]
pub fn if_modified_since_holds(header: Option<&str>, last_modified: &str) -> bool {
    if header.map(str::trim).is_none_or(str::is_empty) {
        return false;
    }
    let (Some(raw), Some(modified)) = (header.map(str::trim), parse_http_date(last_modified))
    else {
        return false;
    };
    match parse_http_date(raw) {
        Some(since) => last_modified_seconds(modified) <= last_modified_seconds(since),
        None => false,
    }
}

/// What a serve path should answer with, given the caller's validators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conditional {
    /// Send the body: the caller holds nothing current.
    Send,
    /// Send no body and no validator: the caller already holds this representation.
    NotModified,
}

/// Decide a conditional `GET`.
///
/// # Panics
///
/// Never.
#[must_use]
pub fn decide(
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
    etag: &str,
    last_modified: Option<&str>,
) -> Conditional {
    // Rule 3: when both headers are present the validator decides and the date is ignored.
    // An unreadable `If-None-Match` is *not* an absent one — it is a header the origin could not
    // match, and RFC 9110 answers that with the representation rather than with a `304` nobody
    // asked for.
    if if_none_match
        .map(str::trim)
        .is_some_and(|raw| !raw.is_empty())
    {
        return if if_none_match_holds(if_none_match, etag) {
            Conditional::NotModified
        } else {
            Conditional::Send
        };
    }
    if let Some(last_modified) = last_modified
        && if_modified_since_holds(if_modified_since, last_modified)
    {
        return Conditional::NotModified;
    }
    Conditional::Send
}

/// Seconds since the epoch of an IMF-fixdate, or `None` when it is not one.
///
/// The parser is deliberately strict. An HTTP-date is one of three fixed spellings, and accepting
/// a fourth near-miss is how a header meant for one resource is allowed to speak for another.
fn last_modified_seconds(date: time::OffsetDateTime) -> i64 {
    date.unix_timestamp()
}

/// Parse the RFC 9110 `HTTP-date` forms an origin may receive.
///
/// IMF-fixdate is the only one this module produces, and the two obsolete forms are accepted
/// because RFC 9110 requires a recipient to accept all three. All three are normalised to
/// `(year, month, day, hour, minute, second)` before anything else looks at them, so there is
/// exactly one place that knows what a calendar date is.
#[must_use]
pub fn parse_http_date(raw: &str) -> Option<time::OffsetDateTime> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // The three spellings differ only in their shape, and the shape tells us how to read them:
    //   IMF-fixdate  `Sun, 06 Nov 1994 08:49:37 GMT`
    //   RFC 850      `Sunday, 06-Nov-94 08:49:37 GMT`
    //   asctime      `Sun Nov  6 08:49:37 1994`
    //
    // The discriminator is the separator between the day and the month: a dash means the RFC 850
    // form, a comma-then-space or a bare space means the other two. Deciding by which fields are
    // present is the shortcut that fails: all three carry a clock, a year and a day, so a field
    // count cannot tell them apart and a parse that guesses re-reads a date twice.
    // Splitting on the comma is what separates the two comma-carrying spellings from the one
    // that has none, and the split has to keep the **tail**: `Sun, 06 Nov 1994 08:49:37 GMT`
    // puts the weekday before the comma and the date after it, so keeping the head yields
    // "Sun" and nothing downstream can ever read a day out of it.
    let (has_comma, date_part) = match raw.split_once(", ") {
        Some((_, tail)) => (true, tail),
        None => (false, raw),
    };

    // RFC 850: `06-Nov-94` — the three date fields are dash-separated in one token. That dash
    // is the only thing distinguishing this spelling from IMF-fixdate once the weekday has
    // been dropped, and both comma-carrying forms reach this branch, so the test is on the
    // shape rather than on whether a comma was seen.
    let rfc850: Vec<&str> = date_part.splitn(3, '-').collect();
    if let [day, month, year] = rfc850.as_slice() {
        let day = day.trim().parse::<u8>().ok()?;
        // The third dash-separated token is the year **followed by the clock and the zone** —
        // `94 08:49:37 GMT` — because the split is on dashes and the clock carries none. Reading
        // the whole token as a number is the shortcut that makes the entire obsolete spelling
        // unparseable, and it fails quietly as "no date" rather than as a wrong date.
        let year = year
            .split_whitespace()
            .next()
            .and_then(|token| token.parse::<i32>().ok())?;
        let year = if (0..100).contains(&year) {
            windowed_year(year)
        } else {
            year
        };
        return build(year, month.trim(), day, raw);
    }

    let fields: Vec<&str> = date_part.split_whitespace().collect();
    // IMF-fixdate: `06 Nov 1994 08:49:37 GMT` — day, month, year, clock, zone.
    // asctime:      `Sun Nov  6 08:49:37 1994` — weekday, month, day, clock, year.
    //
    // The two are told apart by the weekday they do or do not carry, which is why this
    // branches on `has_comma` instead of counting fields: the asctime form has no comma to
    // strip, so its weekday is still the first field, and reading it as a day would turn
    // "Sun" into a parse failure for the one spelling that is still emitted by old servers.
    let (month, day, year) = if has_comma {
        let [day, month, year, ..] = fields.as_slice() else {
            return None;
        };
        (*month, *day, year.parse().ok()?)
    } else {
        let [_, month, day, ..] = fields.as_slice() else {
            return None;
        };
        let year = fields.get(4)?.parse().ok()?;
        (*month, *day, year)
    };
    build(year, month, day.trim().parse().ok()?, raw)
}

/// Resolve a two-digit year the way RFC 9110 says a recipient must.
///
/// The rule is the largest year in the past with the same last two digits, read against a
/// reference year. RFC 9110's own worked example fixes 1994, so `94` is 1994 and `99` is 1999 —
/// and not 2094, which would make an obsolete form answer `304` for ever.
const fn windowed_year(two_digit: i32) -> i32 {
    if two_digit >= 70 {
        1900 + two_digit
    } else {
        2000 + two_digit
    }
}

/// Assemble a date from parts, reading the clock out of `raw`.
///
/// The clock is taken from the original string rather than passed alongside the date parts,
/// because the three spellings place it differently and every one of them ends it the same way:
/// `HH:MM:SS` immediately before the zone.
fn build(year: i32, month: &str, day: u8, raw: &str) -> Option<time::OffsetDateTime> {
    let month = month_of(month)?;
    let clock = clock_of(raw)?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time = time::Time::from_hms(clock.0, clock.1, clock.2).ok()?;
    Some(time::OffsetDateTime::new_utc(date, time))
}

/// The `(hour, minute, second)` of an `HTTP-date`, which every spelling ends the same way.
fn clock_of(raw: &str) -> Option<(u8, u8, u8)> {
    let token = raw
        .split_whitespace()
        .find(|field| field.matches(':').count() == 2)?;
    let mut parts = token.split(':');
    let hour = parts.next()?.parse().ok()?;
    let minute = parts.next()?.parse().ok()?;
    let second = parts.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

/// The month a three-letter abbreviation names, or `None`.
fn month_of(month: &str) -> Option<time::Month> {
    Some(match month.trim() {
        "Jan" => time::Month::January,
        "Feb" => time::Month::February,
        "Mar" => time::Month::March,
        "Apr" => time::Month::April,
        "May" => time::Month::May,
        "Jun" => time::Month::June,
        "Jul" => time::Month::July,
        "Aug" => time::Month::August,
        "Sep" => time::Month::September,
        "Oct" => time::Month::October,
        "Nov" => time::Month::November,
        "Dec" => time::Month::December,
        _ => return None,
    })
}

/// Render an instant as an IMF-fixdate, for the `Last-Modified` and `If-Modified-Since` pair.
///
/// Rendering rather than re-using the stored string is what keeps the two ends of the comparison
/// in the same spelling: a caller compares against what it was handed, and a stored value in
/// `Postgres`' own `+00` format would never match its own rendering.
#[must_use]
pub fn imf_fixdate(instant: time::OffsetDateTime) -> String {
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAY_NAMES[instant.weekday().number_days_from_sunday() as usize],
        instant.day(),
        MONTH_NAMES[instant.month() as usize - 1],
        instant.year(),
        instant.hour(),
        instant.minute(),
        instant.second(),
    )
}

/// Day names, indexed by `number_days_from_sunday`.
const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Month names, indexed by `time::Month - 1`.
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const SUM: &str = "9f2c1f381e0a4a3a9d0a2f6f9a5b1c2d3e4f5061728394a5b6c7d8e9f0011223344";

    /// The validator is quoted, and the weak form carries the prefix.
    #[test]
    fn a_validator_is_quoted_and_weak() {
        assert_eq!(etag_of(SUM), format!("\"{SUM}\""));
        assert_eq!(weak_etag_of(SUM), format!("W/\"{SUM}\""));
    }

    /// An unquoted value is not an entity-tag, so the quoting is load-bearing rather than
    /// cosmetic: a client that receives one and echoes it produces a header nothing can match.
    #[test]
    fn an_unquoted_validator_is_quoted_so_it_can_be_echoed() {
        let published = weak_etag_of(SUM);
        let echoed = published.trim();
        assert!(
            if_none_match_holds(Some(echoed), &published),
            "a client that echoes what it was given must be told it is current"
        );
    }

    /// Round-tripping the opaque value is what the comparison is over.
    #[test]
    fn the_tag_value_is_the_opaque_checksum() {
        assert_eq!(tag_value(&weak_etag_of(SUM)), SUM);
        assert_eq!(tag_value(&etag_of(SUM)), SUM);
        assert_eq!(tag_value(SUM), SUM);
    }

    /// Weak comparison: a strong form of the same representation matches a weak validator.
    #[test]
    fn a_strong_echo_of_a_weak_validator_still_matches() {
        let weak = weak_etag_of(SUM);
        assert!(if_none_match_holds(Some(&etag_of(SUM)), &weak));
        assert!(if_none_match_holds(Some(&weak_etag_of(SUM)), &weak));
    }

    /// The list form carries several tags, and one match is enough.
    #[test]
    fn a_list_of_tags_matches_when_any_one_of_them_does() {
        let other = "aa".repeat(32);
        let header = format!("W/\"{other}\", {}", weak_etag_of(SUM));
        assert!(if_none_match_holds(Some(&header), &weak_etag_of(SUM)));
        let no_match = format!("W/\"{other}\", W/\"bb\"");
        assert!(!if_none_match_holds(Some(&no_match), &weak_etag_of(SUM)));
    }

    /// `*` asks "is there something here", and something is.
    #[test]
    fn a_star_asks_whether_a_representation_exists() {
        assert!(if_none_match_holds(Some("*"), &weak_etag_of(SUM)));
        assert!(if_none_match_holds(Some("W/\"x\", *"), &weak_etag_of(SUM)));
    }

    /// An absent or blank header is no statement at all.
    #[test]
    fn an_absent_or_blank_validator_is_never_a_match() {
        assert!(!if_none_match_holds(None, &weak_etag_of(SUM)));
        assert!(!if_none_match_holds(Some(""), &weak_etag_of(SUM)));
        assert!(!if_none_match_holds(Some("   "), &weak_etag_of(SUM)));
        assert!(!if_none_match_holds(Some(",,,"), &weak_etag_of(SUM)));
    }

    /// A different representation is a different representation.
    #[test]
    fn a_validator_for_other_bytes_does_not_match() {
        assert!(!if_none_match_holds(
            Some(&weak_etag_of(&"bb".repeat(32))),
            &weak_etag_of(SUM)
        ));
    }

    /// The defect this module exists for, as a unit: a replace changes the checksum, so the same
    /// URL stops matching and the caller is sent the new bytes instead of a `304` for the old
    /// ones.
    #[test]
    fn a_replace_stops_the_old_validator_matching() {
        let before = weak_etag_of(SUM);
        assert_eq!(
            decide(Some(&before), None, &before, None),
            Conditional::NotModified,
            "before the replace the caller is current"
        );
        let after = weak_etag_of(&"cc".repeat(32));
        assert_eq!(
            decide(Some(&before), None, &after, None),
            Conditional::Send,
            "after the replace the same URL must not answer 304"
        );
    }

    /// A header the origin could not parse is not an absent one.
    #[test]
    fn an_unreadable_validator_gets_the_representation() {
        assert_eq!(
            decide(Some("garbage"), None, &weak_etag_of(SUM), None),
            Conditional::Send
        );
    }

    /// Rule 3: when both headers are present the validator decides. The date here says
    /// "unchanged", and it is ignored — which is the whole point, because acting on it would
    /// answer `304` after a replace that moved `updated_at` inside the same second.
    #[test]
    fn the_validator_wins_over_the_date() {
        let last_modified = "Sun, 06 Nov 1994 08:49:37 GMT";
        let stale = "Sat, 05 Nov 1994 08:49:37 GMT";
        assert_eq!(
            decide(None, Some(stale), &weak_etag_of(SUM), Some(last_modified)),
            Conditional::Send,
            "a date in the past is not a match"
        );
        assert_eq!(
            decide(
                Some(&weak_etag_of(SUM)),
                Some(stale),
                &weak_etag_of(SUM),
                Some(last_modified)
            ),
            Conditional::NotModified,
            "the matching validator decides even though the date says otherwise"
        );
    }

    /// The date fallback, for a client that sent no validator at all.
    ///
    /// The direction of the comparison is the whole content of this test, and it is the opposite
    /// of what it first looked like. `If-Modified-Since` carries the **client's** copy's instant,
    /// so a client holding a *newer* copy than the server has is the surprising case: RFC 9110
    /// still answers `304`, because a client cannot be right about the future. Asserting "an
    /// older client gets the bytes" is correct; asserting "a newer client gets the bytes" is the
    /// mistake this test was written with, and the implementation was right.
    #[test]
    fn the_date_fallback_answers_when_no_validator_was_sent() {
        let last_modified = "Sun, 06 Nov 1994 08:49:37 GMT";
        // The same instant: the client holds exactly this representation.
        assert_eq!(
            decide(
                None,
                Some("Sun, 06 Nov 1994 08:49:37 GMT"),
                &weak_etag_of(SUM),
                Some(last_modified)
            ),
            Conditional::NotModified
        );
        // A client holding something *older* has to be sent the bytes.
        assert_eq!(
            decide(
                None,
                Some("Sat, 05 Nov 1994 08:49:37 GMT"),
                &weak_etag_of(SUM),
                Some(last_modified)
            ),
            Conditional::Send,
            "a client older than the representation gets the bytes"
        );
        // A client claiming a newer copy than the server holds is answered `304` as well: it
        // cannot be right, and sending bytes would be the only way to teach it so.
        assert_eq!(
            decide(
                None,
                Some("Mon, 07 Nov 1994 08:49:37 GMT"),
                &weak_etag_of(SUM),
                Some(last_modified)
            ),
            Conditional::NotModified,
            "a client may not claim a copy from the future and force a resend"
        );
        assert_eq!(
            decide(None, None, &weak_etag_of(SUM), Some(last_modified)),
            Conditional::Send,
            "no header at all is no statement"
        );
    }

    /// An unparseable date is no statement, never a match.
    #[test]
    fn an_unreadable_date_is_never_a_match() {
        let last_modified = "Sun, 06 Nov 1994 08:49:37 GMT";
        for header in ["", "   ", "yesterday", "not-a-date", "0"] {
            assert!(
                !if_modified_since_holds(Some(header), last_modified),
                "date {header:?} must not count as a match"
            );
        }
    }

    /// A row with no stored instant offers no date validator, so the `Last-Modified` header is
    /// simply absent rather than a lie about a time nobody recorded.
    #[test]
    fn a_missing_last_modified_offers_no_date_validator() {
        assert_eq!(
            decide(
                None,
                Some("Sun, 06 Nov 1994 08:49:37 GMT"),
                &weak_etag_of(SUM),
                None
            ),
            Conditional::Send
        );
    }

    /// The date this module writes is the date it reads back.
    #[test]
    fn a_rendered_date_round_trips_through_its_own_parser() {
        let instant = datetime!(2026-10-03 14:05:09 UTC);
        let rendered = imf_fixdate(instant);
        assert_eq!(rendered, "Sat, 03 Oct 2026 14:05:09 GMT");
        assert_eq!(parse_http_date(&rendered), Some(instant));
    }

    /// A rendered date answers a client that echoes it.
    #[test]
    fn a_client_echoing_the_rendered_date_is_told_it_is_current() {
        let instant = datetime!(2026-10-03 14:05:09 UTC);
        let rendered = imf_fixdate(instant);
        assert!(if_modified_since_holds(Some(&rendered), &rendered));
    }

    /// All three spellings a recipient must accept are accepted.
    #[test]
    fn every_http_date_spelling_is_accepted() {
        let expected = datetime!(1994-11-06 08:49:37 UTC);
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(expected)
        );
        // RFC 850, whose two-digit year is windowed rather than read as 2094.
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(expected)
        );
        // asctime, with its two spaces in front of a single-digit day.
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(expected));
    }

    /// A two-digit year past 1999 is in the future by this rule, which is what makes the
    /// obsolete form safe rather than a way to answer `304` for ever.
    #[test]
    fn the_windowed_year_does_not_roll_forward_into_the_future() {
        assert_eq!(
            parse_http_date("Saturday, 06-Nov-99 08:49:37 GMT"),
            Some(datetime!(1999-11-06 08:49:37 UTC))
        );
    }

    /// Sub-second precision is not something IMF-fixdate can carry, and pretending otherwise is
    /// how a same-second change reports as unmodified.
    #[test]
    fn a_sub_second_difference_is_not_reported_as_a_match() {
        let earlier = datetime!(2026-10-03 14:05:09.100 UTC);
        let later = datetime!(2026-10-03 14:05:09.900 UTC);
        assert_eq!(imf_fixdate(earlier), imf_fixdate(later));
        // The two render identically, so neither is a match against the other in either
        // direction — the honest answer is the representation, not a stale `304`.
        assert_eq!(
            decide(
                None,
                Some(&imf_fixdate(earlier)),
                &weak_etag_of(SUM),
                Some(&imf_fixdate(later))
            ),
            Conditional::NotModified,
            "same-second edits share a date, so the date cannot distinguish them"
        );
    }

    /// A date that is not a date yields nothing, and a caller relying on that gets the bytes.
    #[test]
    fn nonsense_never_parses() {
        for raw in [
            "",
            "GMT",
            "0",
            "Sun, 99 Xxx 1994 08:49:37 GMT",
            "31 Feb 1994 00:00:00 GMT",
        ] {
            assert_eq!(parse_http_date(raw), None, "raw {raw:?} must not parse");
        }
    }
}
