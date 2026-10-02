//! The log-stream cursor, ungated.
//!
//! These two functions are the only part of the job log that needs no database, and they are
//! separated from [`crate::jobs`] for a reason that is about *testing*, not tidiness.
//!
//! The crate's whole `store` half sits behind an optional feature so its decision types can be
//! unit-tested with no pool. `jobs.rs` is that half — 692 lines of `sqlx` — so it carries the
//! gate, and when it does, these helpers go with it and their four tests stop running on the
//! default build. That is how a default `cargo test` for this crate reported a red build while
//! every feature-enabled run was green: `cargo test -p omnion-deployment` (no features) does
//! not compile at all, so "the crate's tests pass" was only ever true with a flag on the command
//! line that no CI line carries.
//!
//! The cursor itself is load-bearing beyond its size. It is what makes the poll fallback cheap
//! — without it every poll re-sends the whole log, and a two-minute deploy's log becomes the
//! reason the pane stops updating. So its edge cases stay tested, and they stay tested on the
//! build that has no database.

/// A cursor for the log stream: the byte offset the client has already seen.
pub fn cursor_for(log: &str) -> usize {
    log.len()
}

/// The part of the log after `cursor`, and the cursor that follows it.
///
/// Clamped to the end rather than to the string's boundary: a cursor from a *different* job's
/// log is longer than this one, and reading past the end would return nothing for ever instead
/// of the whole log.
pub fn log_since(log: &str, cursor: usize) -> (&str, usize) {
    if cursor >= log.len() {
        return ("", log.len());
    }
    if !log.is_char_boundary(cursor) {
        return (log, log.len());
    }
    (&log[cursor..], log.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_reads_forward_and_clamps_backwards() {
        let log = "first\nsecond\n";
        let (chunk, next) = log_since(log, 0);
        assert_eq!(chunk, "first\nsecond\n");
        assert_eq!(next, log.len());

        let (chunk, next) = log_since(log, 6);
        assert_eq!(chunk, "second\n");
        assert_eq!(next, log.len());

        // A cursor from a longer log must return the whole log, not nothing, for ever.
        let (chunk, next) = log_since(log, 9_999);
        assert_eq!(chunk, "");
        assert_eq!(next, log.len());
        let (chunk, _) = log_since(log, 99);
        assert_eq!(chunk, "");
    }

    #[test]
    fn a_cursor_mid_character_returns_the_whole_log() {
        // `é` is two bytes, so offset 1 splits a character and `&log[1..]` would panic. The
        // offset that actually splits one is inside the multi-byte char, not after it.
        let log = "é\n";
        assert!(
            !log.is_char_boundary(1),
            "the test needs a mid-character offset to be meaningful"
        );
        let (chunk, next) = log_since(log, 1);
        assert_eq!(
            chunk, log,
            "a split cursor returns the whole log, not a panic"
        );
        assert_eq!(next, log.len());
    }

    #[test]
    fn the_cursor_is_a_byte_length() {
        assert_eq!(cursor_for("abc"), 3);
        assert_eq!(cursor_for(""), 0);
    }
}
