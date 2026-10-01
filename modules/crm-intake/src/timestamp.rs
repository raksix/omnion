//! Timestamps on the wire.
//!
//! **A Rust `Display` is not a wire format, and this module is the reason that sentence has
//! to be true here.** Every read path in `apps/api` that answered a `*_at` column with
//! `OffsetDateTime::to_string()` shipped a *human-readable* string, and `to_string()` is
//! specified as
//!
//! ```text
//! 2026-10-01 15:32:24.365355685 +00:00:00
//! ```
//!
//! — a **space** where RFC 3339 and ISO 8601 both put `T`, and a bare offset rather than a
//! `Z`. Every browser on earth parses the second form and none of them parse the first:
//! `new Date("2026-10-01 15:32:24.365355685 +00:00:00")` is `Invalid Date`, not a date
//! offset by anything. So the value was not wrong by a timezone, it was not a timestamp at
//! all, and the panel rendered the literal words *"Responded Invalid Date"* on the lead
//! detail screen — on the one field an operator looks at to answer "did we get to them in
//! time?", which is the question the whole SLA feature exists to answer.
//!
//! ## Why the fixture did not catch it
//!
//! It could not, and that is the transferable part. `to_string()` is a `Display` impl: it
//! cannot be wrong in a way a Rust type can check, because its output is not a type the
//! database or the client ever sees. The unit tests in this crate assert *values* — a due
//! time is before a breach, a state is `at_risk` — and every one of them keeps its
//! timestamps as [`OffsetDateTime`] and never crosses the wire. The four tests that *do*
//! read the wire all parsed the same wrong string with a permissive parser. A defect that
//! only exists in the serialised form is invisible to every test that does not serialise.
//!
//! ## The rule
//!
//! A timestamp crosses the boundary through [`rfc3339`] and nothing else. It is one
//! function on purpose: the alternative is `format(&Rfc3339)` typed at each of the fourteen
//! call sites, and a call site that forgets the format argument is a `panic!` at runtime
//! rather than a compile error, while a call site that keeps `.to_string()` compiles
//! perfectly and is wrong — so the *named* function is the thing that can be found, and
//! [`timestamp_is_wire_safe`] is the test that says the call sites use it.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The canonical wire spelling of an instant: RFC 3339, which is also what `Date.parse` and
/// `new Date()` in every browser accept.
///
/// # Panics
///
/// Never, in practice: `OffsetDateTime` always holds an offset in ±23:59:59, inside the
/// range RFC 3339 allows. The formatting error is handled rather than unwrapped so that a
/// future type change surfaces as a fallback string rather than a panic on a request.
#[must_use]
pub fn rfc3339(at: OffsetDateTime) -> String {
    match at.format(&Rfc3339) {
        Ok(text) => text,
        // Unreachable for any value `OffsetDateTime` can hold. Falling back to the `Display`
        // spelling would put the defect back with a comment, so this says what it is: a
        // formatting failure is a bug, and the string below still parses, because ISO 8601
        // permits a space in place of `T` for *readers that accept it* — and the whole point
        // of this module is not relying on which readers do.
        Err(_) => format!("{:?}", at),
    }
}

/// The same for a nullable column: `None` stays `None` rather than becoming a string.
#[must_use]
pub fn rfc3339_opt(at: Option<OffsetDateTime>) -> Option<String> {
    at.map(rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_wire_spelling_carries_the_t_separator() {
        // The single character whose absence made every panel date "Invalid Date". Asserted as
        // a character rather than by round-tripping through a parser, because the round trip
        // is the assertion that was already green while the product was broken.
        let at = datetime!(2026-10-01 15:32:24 UTC);
        assert!(rfc3339(at).contains('T'), "{}", rfc3339(at));
    }

    #[test]
    fn a_browser_can_parse_what_this_module_says() {
        // The assertion the branch's fixtures kept missing: hand the string to the *actual*
        // consumer. `node` is the same engine the panel's `new Date()` runs on, so this fails
        // for exactly the inputs that render as "Invalid Date" — and `Date::to_string()` is
        // the shape that does.
        let at = datetime!(2026-10-01 15:32:24 UTC);
        let good = rfc3339(at);
        let bad = at.to_string();
        assert!(
            node_says_invalid(&bad),
            "the defect this module exists for must still be reproducible: {bad:?}"
        );
        assert!(
            !node_says_invalid(&good),
            "node cannot parse this module's own output: {good:?}"
        );
    }

    #[test]
    fn the_offset_survives_the_round_trip() {
        let utc = datetime!(2026-10-01 15:32:24 UTC);
        let plus_two = utc.to_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
        let back = OffsetDateTime::parse(&rfc3339(plus_two), &Rfc3339).expect("reparses");
        assert_eq!(back, plus_two, "an offset instant must not come back as UTC");
    }

    #[test]
    fn a_none_column_stays_none() {
        assert_eq!(rfc3339_opt(None), None);
        assert_eq!(
            rfc3339_opt(Some(datetime!(2026-10-01 15:32:24 UTC))),
            Some(rfc3339(datetime!(2026-10-01 15:32:24 UTC)))
        );
    }

    /// Ask node whether `new Date(s)` yields a real instant. Skips rather than fails when node
    /// is absent: a gate that fails for want of a browser is a gate that gets deleted.
    fn node_says_invalid(value: &str) -> bool {
        let Ok(out) = std::process::Command::new("node")
            .arg("-e")
            .arg("const d=new Date(process.argv[1]);process.stdout.write(Number.isNaN(d.getTime())?'invalid':'ok')")
            .arg(value)
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout).trim() == "invalid"
    }
}
