//! Platform-wide rate-limit budgets and the pure decision the middleware and the panel's
//! dry-run both call (REQ-127, slice 1).
//!
//! The security centre already owns a per-route limiter (REQ-012) and it stays where it is:
//! this module is the **platform-wide** budget — user, organization, ip and route scopes an
//! operator edits, with a documented winner among overlapping policies. What is new here is
//! [`pick`], because REQ-012's limiter takes exactly one policy per scope and has nothing to
//! arbitrate between.
//!
//! ## Why `decide` is pure and stays that way
//!
//! The panel's dry-run form takes a scope, a target and a route and answers "which policy wins,
//! and how much budget is left". That answer is only useful if it is the answer the middleware
//! will give, and the only way to guarantee that rather than agree with it today is for both to
//! run the same function with no second implementation. So `decide` takes what Redis counted as
//! an argument: no clock, no network, no database. It can be unit-tested exhaustively, and the
//! dry-run cannot drift from the refusal.
//!
//! ## The arithmetic, stated once
//!
//! A window allows `limit + burst` requests. `count` is how many have already been spent.
//! `remaining` is what is left *including* the request being decided, and `Retry-After` names
//! the moment the window rolls. All three come out of the same subtraction, which is why the
//! header the client reads and the number the operator tunes are the same number.
//!
//! **Burst is headroom inside the window, not a second window.** `burst = 0` means exactly
//! `limit` requests. The off-by-one here is the difference between a limiter and a suggestion,
//! so it is asserted rather than described.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{ReliabilityError, Result};
use crate::vocabulary::{MAX_PAGE, MAX_RETRY_AFTER_SECONDS, RATE_SCOPES};

/// A window's ceiling and its headroom, as the panel edits them.
pub const MAX_WINDOW_SECONDS: i64 = 86_400;
/// A limit below one would refuse every request in the scope, which is a disable the operator
/// already has an `enabled` switch for.
pub const MIN_LIMIT: i32 = 1;
/// The largest limit one scope may carry.
pub const MAX_LIMIT: i32 = 1_000_000;
/// The largest burst one scope may forgive above its limit.
pub const MAX_BURST: i32 = 10_000;

/// One scope's policy, as stored and as edited.
///
/// `target_id` is `None` for "every subject in this scope"; `route_pattern` is a template like
/// `/api/v1/posts/{id}` and never a literal path, for the same reason the metric families use
/// templates — a literal id in a policy key is a policy per id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitPolicy {
    /// Stable identifier; `None` for a policy the walk is building in memory.
    pub id: Option<uuid::Uuid>,
    /// An operator-facing name.
    pub name: String,
    /// One of [`RATE_SCOPES`].
    pub scope: String,
    /// The subject this policy is about, or `None` for every subject in the scope.
    pub target_id: Option<String>,
    /// A route template the policy applies to, or `None` for every route.
    pub route_pattern: Option<String>,
    /// Requests allowed per window.
    pub limit_count: i32,
    /// The window the limit is spent against.
    pub window_seconds: i64,
    /// Extra requests forgiven inside the same window.
    pub burst: i32,
    /// Lower wins when two policies both match.
    pub priority: i32,
    /// Whether this row ships with the platform rather than being created by an operator.
    pub is_default: bool,
    /// Whether the policy is enforced at all.
    pub enabled: bool,
}

impl LimitPolicy {
    /// The number of requests this window allows in total.
    ///
    /// Saturating rather than `+`: the limits are capped at a million each, so an overflow is
    /// only reachable from a hand-edited database row, and a wrapped total would silently turn
    /// a huge budget into a tiny one — the refusal would then look correct and be the opposite.
    #[must_use]
    pub fn ceiling(&self) -> i64 {
        i64::from(self.limit_count) + i64::from(self.burst)
    }

    /// Reject a policy the platform will not store, with a message that names the field.
    ///
    /// Each refusal says *which* bound was crossed and *what* the bound is, because the panel
    /// renders this string next to the field and an operator who is told only "invalid" has to
    /// guess the ceiling they are looking for.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(ReliabilityError::invalid("name must not be empty"));
        }
        if !RATE_SCOPES.contains(&self.scope.as_str()) {
            return Err(ReliabilityError::invalid(format!(
                "scope must be one of {}, got '{}'",
                RATE_SCOPES.join(", "),
                self.scope
            )));
        }
        if !(MIN_LIMIT..=MAX_LIMIT).contains(&self.limit_count) {
            return Err(ReliabilityError::invalid(format!(
                "limit must be between {MIN_LIMIT} and {MAX_LIMIT}, got {}",
                self.limit_count
            )));
        }
        if !(1..=MAX_WINDOW_SECONDS).contains(&self.window_seconds) {
            return Err(ReliabilityError::invalid(format!(
                "window_seconds must be between 1 and {MAX_WINDOW_SECONDS}, got {}",
                self.window_seconds
            )));
        }
        if !(0..=MAX_BURST).contains(&self.burst) {
            return Err(ReliabilityError::invalid(format!(
                "burst must be between 0 and {MAX_BURST}, got {}",
                self.burst
            )));
        }
        if let Some(pattern) = &self.route_pattern {
            if !pattern.starts_with('/') {
                return Err(ReliabilityError::invalid(format!(
                    "route_pattern must start with '/', got '{pattern}'"
                )));
            }
            // A `*` in the middle reads as a glob and matches as a literal segment, so a policy
            // written as `/api/v1/*/admin` would store, appear in the table, and never fire. The
            // trailing form is the only one the matcher honours, so the other one is refused with
            // a message that says where the `*` has to be.
            let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
            if let Some(wildcard) = segments.iter().position(|segment| *segment == "*")
                && wildcard + 1 != segments.len()
            {
                return Err(ReliabilityError::invalid(format!(
                    "'*' is only supported as the last segment of a route_pattern, got '{pattern}'"
                )));
            }
        }
        Ok(())
    }
}

/// Who a request is being spent against.
///
/// One enum rather than a map of optional strings, because the four scopes are the whole model
/// and a `BTreeMap<String, String>` would let a caller invent a fifth key that no policy could
/// ever match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// The signed-in user, when there is one.
    pub user_id: Option<uuid::Uuid>,
    /// The organization the user is acting for.
    pub organization_id: Option<uuid::Uuid>,
    /// The address the request came from.
    pub ip: Option<IpAddr>,
    /// The matched route **template**, never the literal path.
    pub route: Option<String>,
}

impl Subject {
    /// The value a scope's budget is keyed on, or `None` when the subject has none.
    ///
    /// `None` for a missing subject is the reason `decide` skips the scope rather than
    /// treating it as the empty string: an anonymous request has no user budget, which is a
    /// different statement from "the user named `''` has a budget".
    #[must_use]
    pub fn key_for(&self, scope: &str) -> Option<String> {
        match scope {
            "user" => self.user_id.map(|id| id.to_string()),
            "organization" => self.organization_id.map(|id| id.to_string()),
            "ip" => self.ip.map(|ip| ip.to_string()),
            "route" => self.route.clone(),
            _ => None,
        }
    }
}

/// What the limiter answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Verdict {
    /// The request may proceed, and this is how much budget is left.
    Allowed {
        /// The winning policy's id, when it was stored.
        policy_id: Option<uuid::Uuid>,
        /// Which scope won, so the panel can name it.
        scope: String,
        /// Requests left in this window after the one being decided.
        remaining: i64,
        /// The ceiling the window allows.
        limit: i64,
    },
    /// The request is refused, and here is why and until when.
    Limited {
        /// The winning policy's id, when it was stored.
        policy_id: Option<uuid::Uuid>,
        /// Which scope won.
        scope: String,
        /// Seconds until the window rolls.
        retry_after: i64,
        /// The ceiling the window allows.
        limit: i64,
        /// How many requests the window allows in total, for the `X-RateLimit-Limit` header.
        ceiling: i64,
    },
    /// No policy applies, so the budget is not finite. Allowed, and *not* zero.
    ///
    /// A separate variant rather than `Allowed { remaining: 0 }` because the two read
    /// completely differently to a client: an `X-RateLimit-Limit: 0` header is a client that
    /// believes it has no budget and backs off, while the truth is that nobody has written a
    /// policy yet. There is no number to publish here, so there is no number to publish.
    Unlimited,
    /// A policy applies, but the counter could not be read, and the deployment fails open.
    ///
    /// The request proceeds and **nothing was counted**, so there is no meaningful `remaining`:
    /// a header claiming a number here would be a measurement nobody took. This is the third
    /// answer between "allowed with a budget" and "refused", and a `bool` cannot represent it —
    /// which is the whole reason [`Verdict`] is an enum.
    Uncounted {
        /// Which scope would have governed the request.
        scope: String,
    },
    /// A policy applies, the counter could not be read, and the deployment fails closed.
    ///
    /// Refused for a reason the client cannot act on. It carries no `Retry-After`, because a wait
    /// the platform cannot compute is not a promise: a client told to retry in a second would
    /// hammer a dependency that is already the thing being refused.
    RefusedUncounted {
        /// Which scope would have governed the request.
        scope: String,
    },
}

impl Verdict {
    /// Whether the request may proceed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(
            self,
            Self::Allowed { .. } | Self::Unlimited | Self::Uncounted { .. }
        )
    }

    /// Whether the answer came from a real reading of a real counter.
    ///
    /// The middleware branches on this before it writes any header: only an authoritative answer
    /// may carry `X-RateLimit-*` or a `Retry-After`, because a header is a promise and a promise
    /// needs a measurement behind it.
    #[must_use]
    pub fn is_authoritative(&self) -> bool {
        matches!(self, Self::Allowed { .. } | Self::Limited { .. })
    }

    /// The scope that produced this answer, or `None` when no policy matched at all.
    ///
    /// A request with no matching policy is **allowed** and says so, and the panel shows "no
    /// policy applies" rather than a zero budget: a limiter with no policy is off, and rendering
    /// that as "0 remaining" would send an operator hunting for a policy that does not exist.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        match self {
            Self::Allowed { scope, .. }
            | Self::Limited { scope, .. }
            | Self::Uncounted { scope }
            | Self::RefusedUncounted { scope } => Some(scope),
            Self::Unlimited => None,
        }
    }

    /// The budget the window allows in total, for the `X-RateLimit-Limit` header.
    ///
    /// `None` for the two answers that carry no measurement. A `Limit: 0` on an outage is a lie
    /// an operator will read as a policy, and `Unlimited` has no number to publish at all.
    #[must_use]
    pub fn ceiling(&self) -> Option<i64> {
        match self {
            Self::Allowed { limit, .. } => Some(i64::from(*limit)),
            Self::Limited { ceiling, .. } => Some(*ceiling),
            Self::Unlimited | Self::Uncounted { .. } | Self::RefusedUncounted { .. } => None,
        }
    }

    /// Requests left after the one being decided, for the `X-RateLimit-Remaining` header.
    #[must_use]
    pub fn remaining(&self) -> Option<i64> {
        match self {
            Self::Allowed { remaining, .. } => Some(*remaining),
            _ => None,
        }
    }

    /// Seconds a refused caller should wait, for `Retry-After`.
    #[must_use]
    pub fn retry_after(&self) -> Option<i64> {
        match self {
            Self::Limited { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

/// Pick the one policy that governs a request.
///
/// **Specificity first, priority second, id last.** Walking [`RATE_SCOPES`] in its documented
/// order is what makes "the most specific matching policy wins" a property of the data rather
/// than of the code: there is no ordering left to get wrong. Within one scope the operator's
/// `priority` decides, and the id is a final tie-break so two rows with the same priority still
/// produce one deterministic answer rather than whichever the database returned first.
///
/// A disabled policy never wins, and a policy that names a `target_id` the request does not
/// have does not match. An empty list is allowed-and-unlimited, which is a documented state and
/// not an error: a fresh instance has no budgets until an operator writes one.
#[must_use]
pub fn pick<'a>(policies: &'a [LimitPolicy], subject: &Subject) -> Option<&'a LimitPolicy> {
    policies
        .iter()
        .filter(|p| p.enabled)
        .filter(|p| matches_policy(p, subject))
        .min_by(|a, b| {
            specificity(a)
                .cmp(&specificity(b))
                .then_with(|| a.priority.cmp(&b.priority))
                .then_with(|| {
                    // `None` sorts first so an in-memory policy with no id still orders
                    // deterministically inside one test binary.
                    a.id.cmp(&b.id)
                })
        })
}

/// A lower number is more specific.
fn specificity(p: &LimitPolicy) -> u8 {
    match RATE_SCOPES
        .iter()
        .position(|s| *s == p.scope)
        .unwrap_or(RATE_SCOPES.len())
    {
        // `usize -> u8` is safe: RATE_SCOPES has four entries and the fallback is its length.
        i => i as u8,
    }
}

/// Whether one policy applies to one request.
#[must_use]
pub fn matches_policy(policy: &LimitPolicy, subject: &Subject) -> bool {
    if let Some(target) = &policy.target_id {
        if subject.key_for(&policy.scope).as_deref() != Some(target.as_str()) {
            return false;
        }
    }
    // A `route`-scoped policy spends a budget keyed on the route itself, so a subject with no
    // route — a worker tick, a health probe, a background job — has nothing for it to match.
    // Without this guard a route policy applies to every routeless request on the instance,
    // which is a budget the operator scoped for page loads silently spent on the background.
    if policy.scope == "route" && subject.route.is_none() {
        return false;
    }
    if let Some(pattern) = &policy.route_pattern {
        // A policy that names a route can only apply to a request that HAS one. A background
        // job, a health probe and a worker tick have no route, and a policy that matched them
        // anyway would spend a budget the operator scoped for a page load.
        let Some(route) = &subject.route else {
            return false;
        };
        // A literal template with a `{id}` placeholder matches any segment there; two policies
        // differing only in that placeholder are still different policies, which is correct —
        // `/posts/{id}` and `/posts/new` are different routes.
        if !route_matches(pattern, route) {
            return false;
        }
    }
    true
}

/// Match a route template against a route.
///
/// Two placeholder forms, and both were added because a template that silently never matches is
/// the worst shape a policy can take: it is stored, it appears in the table, an operator believes
/// it is protecting a route, and it never fires.
///
/// * `{name}` — exactly one segment. An empty segment is not a value.
/// * `*` — one or more remaining segments, and it must be the LAST segment. A trailing `*` is
///   what a prefix policy means ("every public path"), and without it the only way to write one
///   is a row per path — which is a policy per path, the cardinalty mistake the whole design is
///   built to avoid. It is refused in the middle, because `/api/v1/*/admin` reads as a glob and
///   matches as one, and a policy that looks like it covers a subtree and covers nothing is
///   worse than one that is obviously narrow.
#[must_use]
pub fn route_matches(pattern: &str, route: &str) -> bool {
    let mut p = pattern.split('/');
    let mut r = route.split('/');
    loop {
        match (p.next(), r.next()) {
            (None, None) => return true,
            // A trailing `*` swallows everything that is left, INCLUDING nothing: so
            // `/api/v1/public/*` matches `/api/v1/public` too, which is the shape an operator
            // means when they write it. The wildcard is greedy and terminal by construction —
            // there is no segment after it to be greedy about, because the split that produced it
            // had already consumed the pattern's last element.
            (Some("*"), _remaining) => return true,
            (Some(seg), Some(actual)) => {
                if seg.starts_with('{') && seg.ends_with('}') {
                    // An empty segment is not a value: `/posts//comments` must not satisfy
                    // `/posts/{id}/comments`, or a malformed URL would spend a real budget.
                    if actual.is_empty() {
                        return false;
                    }
                } else if seg != actual {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Decide one request against one policy.
///
/// Pure: `now` and `count` are arguments, so the panel's dry-run and the middleware run the same
/// arithmetic and cannot disagree. `count` is what the counter holds **before** this request is
/// spent, which is the definition that makes `remaining` and the `X-RateLimit-*` headers
/// describe the request the caller is actually making.
#[must_use]
pub fn decide(policy: &LimitPolicy, count: i64, now: OffsetDateTime, window_start: OffsetDateTime) -> Verdict {
    let ceiling = policy.ceiling();
    // The window rolls at its start plus its length; a policy whose window_start is already in
    // the future (clock skew between instances) rolls at zero rather than at a negative number.
    let elapsed = (now - window_start).whole_seconds().max(0);
    let retry_after = (policy.window_seconds - elapsed).clamp(1, MAX_RETRY_AFTER_SECONDS);

    if count + 1 > ceiling {
        Verdict::Limited {
            policy_id: policy.id,
            scope: policy.scope.clone(),
            retry_after,
            limit: i64::from(policy.limit_count),
            ceiling,
        }
    } else {
        Verdict::Allowed {
            policy_id: policy.id,
            scope: policy.scope.clone(),
            remaining: (ceiling - (count + 1)).max(0),
            limit: i64::from(policy.limit_count),
        }
    }
}

/// The refusal counters a window aggregates into one row.
///
/// The request is explicit that `reliability.limit.exceeded` is emitted **once per scope, target,
/// route and window rather than per request**, and that is what this type is for: the refusal
/// path increments a rollup, and only the rollup's first increment in a window emits. Storing
/// the route here rather than in the message is what makes "one event per window" a property of
/// a unique constraint instead of a promise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalRollup {
    /// Which scope spent the budget.
    pub scope: String,
    /// The subject that was refused, or `None` when the scope has none.
    pub target_id: Option<String>,
    /// The route template the request was on.
    pub route: String,
    /// The window these refusals belong to.
    pub window_start: OffsetDateTime,
    /// How many requests have been refused in this window.
    pub refusals: i64,
    /// When the last refusal landed.
    pub last_refusal_at: OffsetDateTime,
}

/// Decide whether a refusal should emit an event.
///
/// `None` for the first refusal in a window and `Some` for every one after it. The caller emits
/// on `None`, which makes the guarantee structural: a flood cannot produce a flood of events
/// because the event is not on the refusal path at all, it is on the rollup's *first* write.
#[must_use]
pub fn should_emit(existing_refusals: i64) -> Option<()> {
    if existing_refusals == 0 {
        Some(())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use time::macros::datetime;

    fn policy(scope: &str, limit: i32, burst: i32, window: i64) -> LimitPolicy {
        LimitPolicy {
            id: None,
            name: format!("{scope} policy"),
            scope: scope.into(),
            target_id: None,
            route_pattern: None,
            limit_count: limit,
            window_seconds: window,
            burst,
            priority: 100,
            is_default: false,
            enabled: true,
        }
    }

    fn subject() -> Subject {
        Subject {
            user_id: Some(uuid::Uuid::from_u128(1)),
            organization_id: Some(uuid::Uuid::from_u128(2)),
            ip: Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            route: Some("/api/v1/posts/42".into()),
        }
    }

    #[test]
    fn the_window_allows_limit_plus_burst_and_not_one_more() {
        let p = policy("user", 10, 0, 60);
        let t0 = datetime!(2026-01-01 00:00 UTC);
        // Ten requests fit, the eleventh does not. This is the off-by-one the doc warns about.
        for count in 0..10 {
            assert!(decide(&p, count, t0, t0).is_allowed(), "count {count}");
        }
        assert!(!decide(&p, 10, t0, t0).is_allowed());
    }

    #[test]
    fn a_burst_is_headroom_inside_the_window() {
        let p = policy("user", 10, 5, 60);
        let t0 = datetime!(2026-01-01 00:00 UTC);
        for count in 0..15 {
            assert!(decide(&p, count, t0, t0).is_allowed(), "count {count}");
        }
        assert!(!decide(&p, 15, t0, t0).is_allowed());
    }

    #[test]
    fn remaining_and_the_limit_header_are_the_same_subtraction() {
        let p = policy("user", 10, 0, 60);
        let t0 = datetime!(2026-01-01 00:00 UTC);
        let v = decide(&p, 2, t0, t0);
        let Verdict::Allowed { remaining, limit, .. } = v else {
            panic!("expected allowed");
        };
        assert_eq!(remaining, 7);
        assert_eq!(limit, 10);
    }

    #[test]
    fn retry_after_counts_down_to_the_roll_and_never_exceeds_the_cap() {
        let p = policy("user", 10, 0, 60);
        let t0 = datetime!(2026-01-01 00:00 UTC);
        let t10 = t0 + time::Duration::seconds(10);
        let Verdict::Limited { retry_after, .. } = decide(&p, 10, t10, t0) else {
            panic!("expected limited");
        };
        assert_eq!(retry_after, 50);

        // A window longer than the cap still answers the cap: a client told to wait longer
        // than an hour has given up on the wait.
        let long = policy("user", 10, 0, 86_400);
        let Verdict::Limited { retry_after, .. } = decide(&long, 10, t0, t0) else {
            panic!("expected limited");
        };
        assert_eq!(retry_after, MAX_RETRY_AFTER_SECONDS);
    }

    #[test]
    fn a_window_that_has_not_started_yet_answers_the_window_not_a_negative() {
        // Clock skew between instances can leave `window_start` in the future. The wait is
        // then the whole window, because that is when the window actually rolls — a `1` here
        // would be a lie the client acts on, retrying into a window that has not begun.
        let p = policy("user", 1, 0, 60);
        let future = datetime!(2026-01-01 00:05 UTC);
        let past = datetime!(2026-01-01 00:00 UTC);
        let Verdict::Limited { retry_after, .. } = decide(&p, 10, past, future) else {
            panic!("expected limited");
        };
        assert_eq!(retry_after, 60);
    }

    #[test]
    fn the_most_specific_matching_policy_wins() {
        let user = policy("user", 10, 0, 60);
        let org = policy("organization", 100, 0, 60);
        let ip = policy("ip", 1000, 0, 60);
        let all = vec![ip, org, user];
        assert_eq!(pick(&all, &subject()).unwrap().scope, "user");
        assert_eq!(pick(&all[..2], &subject()).unwrap().scope, "organization");
        assert_eq!(pick(&all[..1], &subject()).unwrap().scope, "ip");
    }

    #[test]
    fn priority_decides_within_one_scope() {
        let mut strict = policy("user", 10, 0, 60);
        strict.name = "strict".into();
        strict.priority = 10;
        let loose = policy("user", 999, 0, 60);
        assert_eq!(pick(&[loose.clone(), strict.clone()], &subject()).unwrap().name, "strict");
        // Order in the vector must not matter.
        assert_eq!(pick(&[strict, loose], &subject()).unwrap().name, "strict");
    }

    #[test]
    fn a_policy_named_for_another_subject_does_not_match() {
        let mut mine = policy("user", 10, 0, 60);
        mine.target_id = Some(uuid::Uuid::from_u128(99).to_string());
        assert!(pick(&[mine], &subject()).is_none());
    }

    #[test]
    fn a_disabled_policy_never_wins() {
        let mut off = policy("user", 10, 0, 60);
        off.enabled = false;
        assert!(pick(&[off], &subject()).is_none());
    }

    #[test]
    fn no_matching_policy_is_allowed_and_named_as_such() {
        // Not an error: a fresh instance has no budgets until an operator writes one.
        assert!(pick(&[], &subject()).is_none());
    }

    #[test]
    fn a_subject_without_the_scope_has_no_budget_in_it() {
        let anonymous = Subject {
            user_id: None,
            organization_id: None,
            ip: None,
            route: None,
        };
        assert_eq!(anonymous.key_for("user"), None);
        assert_eq!(anonymous.key_for("route"), None);
        // A route-scoped policy cannot apply to a request with no route.
        assert!(!matches_policy(&policy("route", 1, 0, 60), &anonymous));
    }

    #[test]
    fn a_route_template_matches_one_segment_and_an_empty_one_matches_none() {
        assert!(route_matches("/api/v1/posts/{id}", "/api/v1/posts/42"));
        assert!(route_matches("/api/v1/posts", "/api/v1/posts"));
        assert!(!route_matches("/api/v1/posts/{id}", "/api/v1/posts/42/comments"));
        assert!(!route_matches("/api/v1/posts", "/api/v1/pages"));
        // A malformed double slash must not spend a real budget.
        assert!(!route_matches(
            "/api/v1/posts/{id}/comments",
            "/api/v1/posts//comments"
        ));
    }

    /// A trailing `*` is a prefix policy, and a mid-pattern `*` is refused rather than stored.
    ///
    /// Both halves matter, and the first one was a defect the shipped migration's own
    /// `/api/v1/public/*` row found: the matcher compared segments literally, so the glob matched
    /// nothing, the row sat in the table looking like a budget on every public path, and not one
    /// request was ever counted against it. A policy that silently never fires is the worst shape
    /// a policy can take — it is the "documented but unreachable" shape this request's sibling has
    /// produced four times, arrived at through SQL.
    #[test]
    fn a_trailing_star_is_a_prefix_and_a_middle_one_is_refused() {
        assert!(
            route_matches("/api/v1/public/*", "/api/v1/public/pages/home"),
            "the glob must cover the subtree it claims"
        );
        assert!(
            route_matches("/api/v1/public/*", "/api/v1/public/forms/submit"),
            "and be greedy across as many segments as there are"
        );
        assert!(route_matches("/api/v1/public/*", "/api/v1/public"));
        assert!(
            !route_matches("/api/v1/public/*", "/api/v1/auth/login"),
            "and must NOT cover a sibling subtree"
        );

        // `{id}` and `*` do not overlap: one segment versus the rest.
        assert!(!route_matches("/api/v1/posts/{id}/*", "/api/v1/posts"));

        // And the store refuses a `*` that is not last, so the unreadable pattern never reaches
        // the table in the first place.
        let mut middle = policy("ip", 10, 0, 60);
        middle.route_pattern = Some("/api/v1/*/admin".into());
        let error = middle.validate().expect_err("a middle glob must be refused");
        assert!(
            error.to_string().contains("last segment"),
            "the message must say WHERE the wildcard belongs: {error}"
        );

        let mut trailing = policy("ip", 10, 0, 60);
        trailing.route_pattern = Some("/api/v1/public/*".into());
        assert!(trailing.validate().is_ok(), "the trailing form is the supported one");
    }

    #[test]
    fn refusals_emit_once_per_window_not_once_per_request() {
        assert_eq!(should_emit(0), Some(()));
        assert_eq!(should_emit(1), None);
        assert_eq!(should_emit(9_999), None);
    }

    #[test]
    fn validation_names_the_field_and_the_bound() {
        let mut p = policy("user", 10, 0, 60);
        p.window_seconds = 0;
        assert!(p.validate().unwrap_err().to_string().contains("window_seconds"));

        let mut p = policy("user", 0, 0, 60);
        assert!(p.validate().unwrap_err().to_string().contains("limit"));

        let mut p = policy("nope", 10, 0, 60);
        assert!(p.validate().unwrap_err().to_string().contains("scope"));

        let mut p = policy("user", 10, 0, 60);
        p.route_pattern = Some("posts".into());
        assert!(p.validate().unwrap_err().to_string().contains("route_pattern"));

        assert!(policy("user", 10, 0, 60).validate().is_ok());
    }

    #[test]
    fn a_huge_budget_from_a_hand_edited_row_cannot_wrap_into_a_tiny_one() {
        // Two caps that cannot be reached through validate(), so this is the database-only case.
        let mut p = policy("user", MAX_LIMIT, MAX_BURST, 60);
        p.limit_count = i32::MAX;
        p.burst = i32::MAX;
        assert!(p.ceiling() > 0);
    }

    #[test]
    fn max_page_bounds_a_list_read() {
        assert!(MAX_PAGE >= 50);
    }
}
