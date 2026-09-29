//! The `Range` header, decided once and in one place.
//!
//! The serve path hands a browser bytes out of an object that may be four hundred megabytes
//! long, and a media player that cannot ask for a window re-downloads the whole file every time
//! somebody drags the scrub bar (REQ-010, slice 2 — the criterion that names "video plays with
//! range requests and a poster"). Deciding *which* window to send is therefore a real question
//! with a real answer, and it is a question about a **caller-controlled string**.
//!
//! Four decisions live here, and each of them is a shortcut that produces a plausible wrong
//! answer:
//!
//! 1. **An unparseable range is ignored, an unsatisfiable one is not.** RFC 9110 says an
//!    unrecognised `Range` must be *ignored* — the whole object, `200`. A well-formed range
//!    whose bytes are all past the end is a different statement: the client asked for something
//!    that does not exist, and answering `200` with the whole file is what makes a player loop
//!    forever on a truncated asset. That one is `416` with `Content-Range: bytes */<total>`.
//! 2. **A multi-range request gets the whole object, not a `multipart/byteranges` body.** The
//!    split form is legal and clients do send it, but a server that answers `416` to a legal
//!    request teaches the client to stop asking; a server that answers `200` teaches it nothing
//!    and still plays. Serving the whole object is the only answer that is correct without a
//!    multipart writer in the codebase.
//! 3. **A reversed or empty window is not a window.** `bytes=100-50` is a *malformed* range, not
//!    an empty one, and it is ignored like any other unparseable value rather than refused —
//!    again, ignore beats refuse, because a refusal is a status a player does not recover from.
//! 4. **An empty object ignores the range entirely.** `bytes=0-` on a zero-byte file is
//!    technically unsatisfiable, and answering `416` to it breaks a player that is about to be
//!    told the file has no frames anyway. There is nothing to window, so the whole object — the
//!    empty object — is the answer.
//!
//! The parser never touches storage and never reads a row, so every branch above is testable
//! without a database, an object store or a router.

/// A half-open window of an object, expressed as inclusive byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteWindow {
    /// First byte of the window.
    pub start: u64,
    /// Last byte of the window, inclusive.
    pub end: u64,
}

impl ByteWindow {
    /// How many bytes the window covers.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Always false: a window is at least one byte wide by construction.
    ///
    /// Present so `len` does not trip the `clippy::len_without_is_empty` lint, and because an
    /// empty window is a real state elsewhere in this crate — a range request against an object
    /// the store has truncated answers with no bytes at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// What to answer a `Range` request with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangePlan {
    /// Send the whole object with a `200`.
    Whole,
    /// Send this window with a `206`.
    Partial {
        /// The bytes to send.
        window: ByteWindow,
        /// Length of the whole object, which the `Content-Range` has to report after the slash.
        ///
        /// Carried beside the window rather than recomputed from it. The first version derived
        /// the total from the bytes that arrived, which makes `Content-Range: bytes 100-149/50` —
        /// a header whose total is smaller than its own end offset, and a player that reads it
        /// concludes the file is fifty bytes long and stops. A `Content-Range` answers "which part
        /// of how much", and both halves have to come from the same object.
        total: u64,
    },
    /// Send nothing: every byte the client named is past the end of the object.
    Unsatisfiable {
        /// Length of the object, which the `416` still has to report.
        total: u64,
    },
}

impl RangePlan {
    /// The `Accept-Ranges` value a response to this plan carries.
    ///
    /// Advertised on **every** answer, not only on a `206`. A client that has to fail one
    /// request to discover that ranges are supported never tries again, and a media player is
    /// the client that most needs the second attempt.
    pub const ACCEPT_RANGES: &'static str = "bytes";

    /// Decide the answer for one request against an object of `total` bytes.
    ///
    /// `total` is the length the **row** claims. A store that returns fewer bytes than the row
    /// promised is a real state (a truncated object, a key rewritten by hand) and the route
    /// corrects the reported total from what actually came back — this function only has to
    /// decide what to *ask* for.
    #[must_use]
    pub fn decide(header: Option<&str>, total: u64) -> Self {
        // Nothing to window: every range names a byte that does not exist, and answering `416`
        // for a zero-byte object breaks the player that is about to learn the file has no frames.
        if total == 0 {
            return Self::Whole;
        }

        let Some(raw) = header.map(str::trim).filter(|value| !value.is_empty()) else {
            return Self::Whole;
        };

        // The range unit is case-insensitive; only `bytes` is understood, and an unknown unit is
        // ignored rather than refused (RFC 9110 §14.2).
        let Some(spec) = raw
            .strip_prefix("bytes=")
            .or_else(|| raw.strip_prefix("BYTES="))
            .or_else(|| {
                raw.get(..6)
                    .filter(|prefix| prefix.eq_ignore_ascii_case("bytes="))
                    .and_then(|_| raw.get(6..))
            })
        else {
            return Self::Whole;
        };

        let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
        // A multi-range request gets the whole object. See the module note.
        let [only] = parts.as_slice() else {
            return Self::Whole;
        };

        match parse_window(only, total) {
            Ok(window) => Self::Partial { window, total },
            // Well formed but naming nothing that exists: a `416`, not a `200` with the whole file.
            Err(Window::Unsatisfiable) => Self::Unsatisfiable { total },
            // Not a range a client could have meant. Ignored, per rule 1.
            Err(Window::Malformed) => Self::Whole,
        }
    }

    /// The `Content-Range` value this plan's response carries, if it carries one.
    ///
    /// `served` is how many bytes the store actually returned for the window. The **end offset**
    /// is derived from it and the **total** is not: a store that returned fewer bytes than the
    /// row promised must not be told the object is that short, and a total taken from `served`
    /// would do exactly that.
    #[must_use]
    pub fn content_range(&self, served: u64) -> Option<String> {
        match self {
            Self::Whole => None,
            // An unsatisfiable range still reports the total: `bytes * /<total>` is how a client
            // learns the object is shorter than the one it is holding.
            Self::Unsatisfiable { total } => Some(format!("bytes */{total}")),
            Self::Partial { window, total } => {
                // The end is computed from the bytes actually served, not from the window that
                // was asked for. A store that returns fewer bytes than the row promised must not
                // produce a `Content-Range` claiming more bytes arrived than did — a player that
                // trusts it waits for data that never comes.
                let end = window.start + served.saturating_sub(1);
                Some(format!("bytes {}-{end}/{total}", window.start))
            }
        }
    }

    /// The status this plan answers with.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Partial { .. } => 206,
            Self::Unsatisfiable { .. } => 416,
            Self::Whole => 200,
        }
    }
}

/// Why one window specification did not become a [`ByteWindow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// The specification is not a range any client could have meant, and is ignored.
    Malformed,
    /// The specification is a range, but every byte it names is past the end of the object.
    Unsatisfiable,
}

/// Parse one `bytes=<first>-<last>` specification against an object of `total` bytes.
///
/// The two refusals are separate types because they answer differently: `Malformed` becomes a
/// `200` with the whole object (rule 1 — ignore what you cannot read) and `Unsatisfiable`
/// becomes a `416` (the client asked for bytes that do not exist and must be told so).
///
/// The obvious single `Option` conflates them, and the conflation is not cosmetic: it turns
/// `bytes=0-99999999999999999999999` — a number too large to parse — into a `416` that a media
/// player treats as "this file is not seekable", which is a lie the client cannot discover.
fn parse_window(spec: &str, total: u64) -> Result<ByteWindow, Window> {
    let malformed = || Window::Malformed;
    let Some((first, last)) = spec.split_once('-') else {
        return Err(malformed());
    };

    // `bytes=-N`: the final N bytes. Zero is the one value that is well formed and names
    // nothing — `-0` is a suffix of length zero, which is not a byte.
    if first.trim().is_empty() {
        let suffix: u64 = last.trim().parse().map_err(|_| malformed())?;
        if suffix == 0 {
            return Err(Window::Unsatisfiable);
        }
        let start = total.saturating_sub(suffix);
        return Ok(ByteWindow {
            start,
            end: total - 1,
        });
    }

    // `bytes=N-` and `bytes=N-M`. A window whose end is before its start is malformed, not
    // unsatisfiable, and an end past the object is clamped rather than refused — that is what
    // "the client asked for the rest of a file it thinks is longer than it is" means.
    let start: u64 = first.trim().parse().map_err(|_| malformed())?;
    if start >= total {
        return Err(Window::Unsatisfiable);
    }
    let end = match last.trim() {
        "" => total - 1,
        digits => digits
            .parse::<u64>()
            .map_err(|_| malformed())?
            .min(total - 1),
    };
    if end < start {
        return Err(malformed());
    }
    Ok(ByteWindow { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole object when the client says nothing.
    #[test]
    fn an_absent_or_empty_header_asks_for_everything() {
        assert_eq!(RangePlan::decide(None, 100), RangePlan::Whole);
        assert_eq!(RangePlan::decide(Some(""), 100), RangePlan::Whole);
        assert_eq!(RangePlan::decide(Some("   "), 100), RangePlan::Whole);
    }

    /// A closed window is exactly the bytes it names.
    #[test]
    fn a_closed_window_is_the_bytes_it_names() {
        assert_eq!(
            RangePlan::decide(Some("bytes=0-9"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 9 },
                total: 100
            }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=10-19"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 10, end: 19 },
                total: 100
            }
        );
    }

    /// An open window runs to the last byte, and a suffix window is measured from the end.
    #[test]
    fn an_open_window_and_a_suffix_window_measure_from_both_ends() {
        assert_eq!(
            RangePlan::decide(Some("bytes=90-"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 90, end: 99 },
                total: 100
            }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=-10"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 90, end: 99 },
                total: 100
            }
        );
    }

    /// A suffix longer than the object is the whole object, not an error: a player asking for
    /// the last ten megabytes of a one-megabyte file wants all of it.
    #[test]
    fn a_suffix_longer_than_the_object_asks_for_all_of_it() {
        assert_eq!(
            RangePlan::decide(Some("bytes=-500"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 99 },
                total: 100
            }
        );
    }

    /// An end past the object is clamped to the last byte — the window exists, it is just shorter
    /// than the client believed. This is the difference between "ask again" and "416".
    #[test]
    fn an_end_past_the_object_is_clamped() {
        assert_eq!(
            RangePlan::decide(Some("bytes=90-9999"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 90, end: 99 },
                total: 100
            }
        );
    }

    /// A window that starts past the end names nothing at all: a `416`, never the whole file.
    #[test]
    fn a_window_past_the_end_is_unsatisfiable() {
        assert_eq!(
            RangePlan::decide(Some("bytes=100-"), 100),
            RangePlan::Unsatisfiable { total: 100 }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=500-600"), 100),
            RangePlan::Unsatisfiable { total: 100 }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=-0"), 100),
            RangePlan::Unsatisfiable { total: 100 }
        );
    }

    /// An unreadable or unknown-unit header is **ignored**, not refused. The whole object is the
    /// RFC's answer and the only one a client recovers from.
    #[test]
    fn an_unusable_header_is_ignored_rather_than_refused() {
        for header in [
            "items=0-9",
            "bytes=abc-def",
            "bytes=",
            "bytes=-",
            "bytes=5-1",
            "0-9",
            "bytes=0-9x",
        ] {
            assert_eq!(
                RangePlan::decide(Some(header), 100),
                RangePlan::Whole,
                "header {header:?} must be ignored"
            );
        }
    }

    /// A multi-range request is legal and unsupported; the whole object is the honest answer.
    #[test]
    fn a_multi_range_request_asks_for_the_whole_object() {
        assert_eq!(
            RangePlan::decide(Some("bytes=0-9,20-29"), 100),
            RangePlan::Whole
        );
        // Including the trailing-comma form, which some players send.
        assert_eq!(RangePlan::decide(Some("bytes=0-9,"), 100), RangePlan::Whole);
    }

    /// A zero-byte object ignores the range: there is no window inside it, and a `416` would
    /// break a player that is about to learn the file has no frames.
    #[test]
    fn an_empty_object_ignores_every_range() {
        for header in [None, Some("bytes=0-"), Some("bytes=-1"), Some("bytes=5-9")] {
            assert_eq!(
                RangePlan::decide(header, 0),
                RangePlan::Whole,
                "an empty object is always whole"
            );
        }
    }

    /// The unit is case-insensitive, and surrounding space is not an error.
    #[test]
    fn the_range_unit_is_case_insensitive() {
        assert_eq!(
            RangePlan::decide(Some("BYTES=0-9"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 9 },
                total: 100
            }
        );
        assert_eq!(
            RangePlan::decide(Some("Bytes=0-9"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 9 },
                total: 100
            }
        );
        assert_eq!(
            RangePlan::decide(Some("  bytes=0-9  "), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 9 },
                total: 100
            }
        );
    }

    /// The last byte of an object is reachable: `bytes=<total>-` is unsatisfiable but
    /// `bytes=<total - 1>-` is the final byte, and an off-by-one here loses a video's last frame.
    #[test]
    fn the_last_byte_is_inside_the_object() {
        assert_eq!(
            RangePlan::decide(Some("bytes=99-"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 99, end: 99 },
                total: 100
            }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=99-99"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 99, end: 99 },
                total: 100
            }
        );
    }

    /// A single-byte object windows correctly, which is the smallest case where an off-by-one
    /// turns `bytes=0-` into a `416`.
    #[test]
    fn a_one_byte_object_answers_a_window() {
        assert_eq!(
            RangePlan::decide(Some("bytes=0-"), 1),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 0 },
                total: 1
            }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=0-0"), 1),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 0 },
                total: 1
            }
        );
        assert_eq!(
            RangePlan::decide(Some("bytes=1-"), 1),
            RangePlan::Unsatisfiable { total: 1 }
        );
    }

    /// The end offset is the bytes that actually arrived; the total is the **object**, never the
    /// window.
    ///
    /// This is the assertion the walk over the real router forced. The first implementation
    /// derived both halves from `served`, so a fifty-byte window out of a three-hundred-byte
    /// object answered `Content-Range: bytes 100-149/50` — a header whose total is smaller than
    /// its own end offset, which every unit test in this file accepted, because a unit test can
    /// only see the number it handed in. Only a walk that uploads three hundred bytes and asks
    /// for fifty can see that the two numbers are supposed to come from different facts.
    #[test]
    fn the_content_range_reports_the_object_and_not_the_window() {
        let plan = RangePlan::decide(Some("bytes=100-149"), 300);
        assert_eq!(
            plan.content_range(50).as_deref(),
            Some("bytes 100-149/300"),
            "the total is the object; the window is 50 bytes and the object is 300"
        );

        // A window that ends short of the object still reports the object.
        let open = RangePlan::decide(Some("bytes=290-"), 300);
        assert_eq!(open.content_range(10).as_deref(), Some("bytes 290-299/300"));

        // The suffix form is the same shape measured from the other end.
        let suffix = RangePlan::decide(Some("bytes=-10"), 300);
        assert_eq!(
            suffix.content_range(10).as_deref(),
            Some("bytes 290-299/300")
        );
    }

    /// A store that returns fewer bytes than the row promised shortens the **end**, never the
    /// total — the object did not become smaller, the store under-delivered.
    #[test]
    fn a_short_read_shortens_the_end_and_leaves_the_total_alone() {
        let plan = RangePlan::decide(Some("bytes=0-99"), 100);
        assert_eq!(
            plan.content_range(40).as_deref(),
            Some("bytes 0-39/100"),
            "40 bytes arrived out of a 100-byte object: the end moves, the total does not"
        );
    }

    /// A `416` still tells the client how long the object really is.
    #[test]
    fn an_unsatisfiable_range_still_reports_the_total() {
        let plan = RangePlan::decide(Some("bytes=500-"), 100);
        assert_eq!(plan.content_range(0).as_deref(), Some("bytes */100"));
        assert_eq!(plan.status(), 416);
    }

    /// A `200` has no `Content-Range`; a `206` and a `416` both do.
    #[test]
    fn only_a_partial_answer_carries_a_content_range() {
        assert!(RangePlan::Whole.content_range(100).is_none());
        assert!(RangePlan::Whole.content_range(0).is_none());
        assert!(
            RangePlan::decide(Some("bytes=0-9"), 100)
                .content_range(10)
                .is_some()
        );
    }

    /// The window's own length, which the response sets as `Content-Length`.
    #[test]
    fn a_window_reports_its_own_length() {
        let window = match RangePlan::decide(Some("bytes=10-19"), 100) {
            RangePlan::Partial { window, .. } => window,
            other => panic!("expected a window, got {other:?}"),
        };
        assert_eq!(window.len(), 10);
        assert!(!window.is_empty());
    }

    /// A huge number a client sends is not a panic and not a `416` when it fits: it is clamped.
    #[test]
    fn an_oversized_end_does_not_overflow() {
        assert_eq!(
            RangePlan::decide(Some("bytes=0-18446744073709551615"), 100),
            RangePlan::Partial {
                window: ByteWindow { start: 0, end: 99 },
                total: 100
            }
        );
        // One past `u64::MAX` does not parse at all, so it is ignored rather than refused.
        assert_eq!(
            RangePlan::decide(Some("bytes=0-99999999999999999999999"), 100),
            RangePlan::Whole
        );
    }
}
