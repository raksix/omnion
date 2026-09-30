//! The closed vocabularies of the health centre.
//!
//! Everything the panel can *say* about a service is one of four words, and the
//! list lives here rather than in the handlers. Two of those words carry more
//! weight than the other two, and the reason is written down because it is the
//! whole product:
//!
//! * `healthy` — the probe ran and the dependency answered.
//! * `degraded` — the probe ran and something about the answer is wrong (latency
//!   over the budget, a worker is stale, the queue is deep). A service that works
//!   badly is not a service that does not work, and the panel must be able to say
//!   so without a red row.
//! * `down` — the probe ran and the dependency did not answer.
//! * `unknown` — **the probe could not run, or has never run.** This is a
//!   first-class state, not an absence and not a pass. A screen that renders
//!   `unknown` as "nothing to report" is the exact failure the request's own risk
//!   note names, so the overview emits one row per registered service *always*,
//!   including the ones that have no sample yet.

/// The seven services the request's sketch names, plus the host itself.
///
/// `api` is the platform's own process: it answers immediately, which is what
/// makes it a useful control row — if the API is the only row that can never be
/// red, an operator learns to ignore the column, so it is checked like any other.
pub const SERVICES: &[&str] = &[
    "api", "postgres", "redis", "s3", "workers", "queue", "search",
];

/// The service whose metrics come from the kernel rather than from a dependency.
pub const HOST_SERVICE: &str = "host";

/// Every service key the platform knows, host included.
#[must_use]
pub fn all_services() -> Vec<&'static str> {
    let mut all = SERVICES.to_vec();
    all.push(HOST_SERVICE);
    all
}

/// The four words a service state can be.
pub const STATES: &[&str] = &["healthy", "degraded", "down", "unknown"];

/// The state a service reads as when it has never been probed.
///
/// A separate constant rather than a literal at the call site: it appears in the
/// overview builder, the detail route and the summary, and three copies of the
/// same string is three chances to spell it `Unknown`.
pub const STATE_WHEN_UNPROBED: &str = "unknown";

/// `true` when `state` is one of ours.
#[must_use]
pub fn is_state(state: &str) -> bool {
    STATES.contains(&state)
}

/// The severity order used when a summary has to collapse several states into one.
///
/// The order is the honest one: a `down` outranks a `degraded`, a `degraded`
/// outranks an `unknown`, and an `unknown` outranks a `healthy`. That last pair is
/// the debatable one and it is deliberate — a service nobody has looked at must
/// never be able to make the banner say "all systems operational".
#[must_use]
pub fn rank(state: &str) -> u8 {
    match state {
        "healthy" => 0,
        "unknown" => 1,
        "degraded" => 2,
        "down" => 3,
        _ => 1,
    }
}

/// The state word [`rank`] considers, as a `&'static str` borrowed from the
/// vocabulary rather than from the caller's string.
///
/// `rank` itself is lifetime-free — it takes any `&str` and returns a number. This
/// helper is what lets [`worst`] hand back a `&'static str` state *without*
/// borrowing the caller's: it maps the state onto the vocabulary's own copy. A
/// caller passing an owned `String` therefore gets a static state back and only
/// the *service* borrow is tied to it, which is exactly the split the overview
/// needs.
#[must_use]
pub fn canonical_state(state: &str) -> &'static str {
    match state {
        "healthy" => "healthy",
        "degraded" => "degraded",
        "down" => "down",
        // `unknown` covers both the real word and anything unrecognised, and the
        // caller has already been told an unrecognised word is ranked as unknown.
        _ => "unknown",
    }
}

/// The single worst state in a set, and the service that carries it.
///
/// Returns `None` for an empty set rather than a fabricated `healthy`: a summary
/// computed from no services has not established that anything is fine.
///
/// The service borrow is tied to the iterator's item, and NOT to `'static`. The
/// state word comes from the vocabulary (so it is `'static` and the summary
/// struct can hold it), but forcing the *service* to `'static` as well is what
/// makes a caller that owns a `Vec<ServiceReport>` borrow it for the rest of the
/// function and then be unable to move it into the struct it is building — the
/// borrow outlives the thing it borrowed from, and the compiler is right to
/// refuse. Callers pass their own `&str`s and get their own lifetimes back.
#[must_use]
pub fn worst<'a, 'b, I>(states: I) -> Option<(&'static str, &'b str)>
where
    I: IntoIterator<Item = (&'b str, &'a str)>,
{
    let mut worst: Option<(u8, &'static str, &'b str)> = None;
    for (service, state) in states {
        let score = rank(state);
        // Strictly greater, so a *tie* keeps the first service seen. `max_by_key`
        // would keep the last, which makes the banner name a different service
        // depending on the registry's iteration order — a banner that changes its
        // mind when two services are equally bad is a banner nobody can trust.
        if worst.is_none_or(|(best, _, _)| score > best) {
            worst = Some((score, canonical_state(state), service));
        }
    }
    worst.map(|(_, state, service)| (state, service))
}

/// The metric keys the host probes publish on every run.
///
/// This is a list rather than a `const` map because each metric carries its own
/// unit and its own threshold, and those live in the threshold policy — where an
/// operator can change them — rather than in a constant the compiler froze.
pub const HOST_METRICS: &[&str] = &[
    "cpu_percent",
    "load_average_1m",
    "memory_percent",
    "disk_percent",
    "db_connections",
    "queue_depth",
];

/// Metric keys that describe the platform rather than the host. A sparkline
/// renders from samples by metric, and a row nobody charts is a wasted index.
pub const SERVICE_METRICS: &[&str] = &["latency_ms", "open_items", "stale_items"];

/// The upper bound on a stored detail document, in characters.
///
/// A probe's detail is rendered field by field, never as a blob, so this is not
/// about screen space: it is about what happens when a probe accidentally puts a
/// connection string or a stack trace into a row that a hundred exports will read.
pub const MAX_DETAIL_CHARS: usize = 8_000;

/// The upper bound on a sample's `value` magnitude, so one runaway probe cannot
/// make a chart's scale meaningless.
pub const MAX_ABS_VALUE: f64 = 1.0e15;

/// A numeric value is only a sample when it is a number.
#[must_use]
pub fn is_finite(value: f64) -> bool {
    value.is_finite() && value.abs() <= MAX_ABS_VALUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_carries_the_seven_services_of_the_request() {
        // The request's sketch is the acceptance criterion; a service silently
        // dropped from this list would make its row disappear from the panel
        // without a single red gate, because "the row is gone" is not an error.
        for key in ["api", "postgres", "redis", "s3", "workers", "queue", "search"] {
            assert!(SERVICES.contains(&key), "{key} is a registered service");
        }
        assert_eq!(SERVICES.len(), 7);
        assert_eq!(all_services().len(), 8, "the host is a service too");
    }

    #[test]
    fn an_unprobed_service_is_a_state_not_an_absence() {
        assert!(is_state(STATE_WHEN_UNPROBED));
        assert!(!is_state("ok"));
        assert!(!is_state("pass"), "the security vocabulary does not leak in here");
    }

    #[test]
    fn the_worst_state_is_chosen_by_risk_not_by_order() {
        // Deliberately shuffled: a `max_by_key` over the input order would pass
        // this test if the list happened to be sorted the way the test is.
        let input = [
            ("api", "healthy"),
            ("redis", "down"),
            ("queue", "degraded"),
            ("s3", "healthy"),
        ];
        assert_eq!(worst(input), Some(("down", "redis")));

        let degraded_wins = [("api", "healthy"), ("queue", "degraded"), ("s3", "healthy")];
        assert_eq!(worst(degraded_wins), Some(("degraded", "queue")));

        let unknown_wins = [("api", "healthy"), ("s3", "unknown")];
        assert_eq!(worst(unknown_wins), Some(("unknown", "s3")));
    }

    #[test]
    fn no_services_is_not_healthy() {
        // The dangerous case: `worst([])` defaulting to `healthy` would let a
        // platform whose probes all failed to render still answer "all systems
        // operational", because the worst of nothing is nothing.
        assert_eq!(worst([]), None);
    }

    #[test]
    fn an_unknown_word_is_ranked_as_unknown() {
        // A typo in a state must not rank *better* than healthy.
        assert!(rank("totally-fine") > rank("healthy"));
    }

    #[test]
    fn a_non_finite_sample_is_refused() {
        assert!(!is_finite(f64::NAN));
        assert!(!is_finite(f64::INFINITY));
        assert!(!is_finite(MAX_ABS_VALUE * 10.0));
        assert!(is_finite(0.0));
        assert!(is_finite(-42.5));
    }
}
