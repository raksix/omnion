//! The request log's vocabulary: what a status is called and what a stored path looks like.
//!
//! ## Why this is a module of two functions
//!
//! `origin/main` (REQ-022 slice 2) put the same two functions in a 415-line `logs.rs` that also
//! owns `ClientIdentity`, the HMAC client fingerprint, the `LogQuery` validator and the
//! `LogRow`/`LogPage` sqlx shapes. That file cannot be compiled into this branch without two
//! things this branch deliberately does not have: an `hmac` dependency, and a
//! `DeveloperError::Invalid(String)` variant that this branch's error enum replaced with a
//! per-rule `code`/`field` pair (`crates/developer/src/error.rs`).
//!
//! So the merge takes the **two pure functions** and leaves the rest of main's file behind. That
//! is not a shortcut: these two are the log screen's *rules*, and the reason they live in their
//! own module is precisely that they are rules with no dependencies. Anything that counts a
//! request, filters a log or stores a path must answer from here, so an overview card and the
//! table beside it cannot disagree about what an error is or where a query string went.
//!
//! ## `class_of` is total on purpose
//!
//! Every `i16` lands in a class rather than reaching an "unknown" bucket: a status this build
//! has never seen is still a real response somebody needs to find in the log, and a filter that
//! silently dropped it would be a filter that hides rows. The `4xx` arm is explicit rather than
//! the catch-all so that adding a `1xx` class later is a visible decision at this match.
//!
//! ## `path_without_query` strips the fragment too
//!
//! A fragment never reaches a server — the browser removes it before the request is made — so a
//! path that carries one is being constructed somewhere else, and stripping it is the same
//! answer there as leaving it. The query is the half that matters: it is where credentials
//! travel (`?access_token=…`), so it is the half that must never reach a table an operator
//! exports as CSV.

/// The status classes the log screen filters by, in the order the toolbar shows them.
pub const STATUS_CLASSES: &[&str] = &["2xx", "3xx", "4xx", "5xx"];

/// Longest page the log list will serve.
pub const MAX_PAGE: usize = 200;

/// How far back the log reaches, in days.
///
/// **A constant rather than a setting, deliberately.** REQ-033's risk note for the request log
/// says "keep the retention window explicit (14 days)", and the number being a constant is what
/// makes that sentence true in the way the note means: an operator reading the log screen is
/// reading a window the platform does not renegotiate per tenant. A configurable retention is a
/// different product with a different compliance story, and it would need a policy to say which
/// tenant may choose a longer one.
///
/// It lives here, beside `MAX_PAGE`, because both are "how far the log screen reaches" and the
/// screen, the CSV export and the overview card all have to quote the same one.
pub const RETENTION_DAYS: u32 = 14;

/// Strip the query string (and any fragment) from a request path.
///
/// Called on the only copy of the raw path the platform has, before the row is written. The
/// stored path is what an operator reads to recognise the call, and the path without its query
/// is that; the query is where credentials travel.
#[must_use]
pub fn path_without_query(path: &str) -> String {
    let without_fragment = path.split('#').next().unwrap_or(path);
    match without_fragment.split_once('?') {
        Some((path, _query)) => path.to_owned(),
        None => without_fragment.to_owned(),
    }
}

/// The class a status falls in, as the filter names it.
#[must_use]
pub fn class_of(status: i16) -> &'static str {
    match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PAGE, RETENTION_DAYS, STATUS_CLASSES, class_of, path_without_query};

    #[test]
    fn the_status_class_a_log_reports_matches_its_code() {
        assert_eq!(class_of(200), "2xx");
        assert_eq!(class_of(204), "2xx");
        assert_eq!(class_of(301), "3xx");
        assert_eq!(class_of(403), "4xx");
        assert_eq!(class_of(500), "5xx");
    }

    #[test]
    fn a_status_this_build_has_never_seen_still_lands_in_a_class() {
        // Total on purpose: an unknown status is a real response somebody has to find, and a
        // filter that dropped it would hide rows rather than report them.
        assert_eq!(class_of(100), "5xx");
        assert_eq!(class_of(i16::MAX), "5xx");
    }

    #[test]
    fn every_class_the_helper_can_return_is_offered_by_the_filter() {
        // The constant and the helper are the two halves of one contract: a class the toolbar
        // offers that nothing can produce is a dead control, and a class a row can carry that
        // the toolbar cannot name is a row nobody can find.
        for status in [200, 301, 404, 500] {
            assert!(
                STATUS_CLASSES.contains(&class_of(status)),
                "class_of({status}) = {} is not in {STATUS_CLASSES:?}",
                class_of(status)
            );
        }
        assert_eq!(MAX_PAGE, 200, "the page ceiling is a published number");
        // Fourteen days is the number REQ-033's request-log risk note commits to, and it is
        // quoted by the log screen, the CSV export and the overview card. If this moves, all
        // three move with it — which is why it is one constant and not three literals.
        assert_eq!(RETENTION_DAYS, 14);
    }

    #[test]
    fn a_query_string_never_survives_into_a_stored_path() {
        assert_eq!(
            path_without_query("/api/v1/media?access_token=super-secret"),
            "/api/v1/media"
        );
        assert_eq!(path_without_query("/api/v1/media"), "/api/v1/media");
        assert_eq!(path_without_query("/a?b=1&c=2"), "/a");
    }

    #[test]
    fn a_fragment_is_stripped_too_though_it_never_reaches_a_server() {
        // A path carrying one is being constructed somewhere else, and stripping it is the same
        // answer there as leaving it.
        assert_eq!(path_without_query("/a?b=1#frag"), "/a");
        assert_eq!(path_without_query("/a#frag"), "/a");
    }

    #[test]
    fn an_empty_path_is_its_own_answer_rather_than_a_panic() {
        assert_eq!(path_without_query(""), "");
        assert_eq!(path_without_query("?"), "");
    }
}
