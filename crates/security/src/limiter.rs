//! The rate-limit policy: which scope a request belongs to, and what that scope allows
//! (REQ-012, slice 3).
//!
//! Two halves that must never disagree, kept in one module for that reason:
//!
//! * [`RatePolicy`] is the **document** an operator edits — a row per scope, a window, a limit,
//!   a burst, an enabled switch. It is validated here so the form gets a field-level reason and
//!   the store cannot be handed a policy that is out of range.
//! * [`decide`] is the **pure function** the middleware and the panel's tester both call. It
//!   takes the policy, the scope, the client and what Redis counted, and answers
//!   [`Verdict::Limited`] or [`Verdict::Allowed`]. It is a pure function on purpose: the tester
//!   on `/security/rate-limits` has to be able to *predict* the middleware's answer, and the
//!   only way to guarantee it predicts it rather than agreeing with it today is for both to run
//!   the same code with no second implementation to drift.
//!
//! **The decision has four inputs, and all four are visible in the answer.** A limiter that
//! answers "limited" without saying which rule fired is a limiter whose tuning is guesswork:
//! `verdict.rule` names the scope and `verdict.retry_after` names how long to wait, because
//! `Retry-After` on the wire and the number in the panel's tester are the same number.
//!
//! **Burst is headroom inside the window, not a second window.** `burst` is how many requests
//! above `limit` are forgiven inside the same window, which is what makes a short spike of real
//! traffic survivable. `count > limit + burst` is the only refusal, so `burst = 0` means exactly
//! `limit` requests per window — the arithmetic is asserted in the tests because an off-by-one
//! here is the difference between a limiter and a suggestion.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SecurityError};
use crate::vocabulary::{MAX_PAGE, RATE_SCOPES};

/// Seconds a `Retry-After` may claim, and the ceiling a window may be set to.
///
/// 86 400 is a day: longer than that and a client that got refused waits longer than the
/// process that refused it is likely to have been restarted, so the wait is no longer meaningful.
pub const MAX_WINDOW_SECONDS: i64 = 86_400;

/// The most requests a scope may allow in its window, and the floor a burst may take.
///
/// A limit of zero would refuse every request in the scope, which is a disable the operator
/// already has a switch for — and a window scope with no limit is a configuration nobody
/// intended. Both are refused with a message that says so.
pub const MIN_LIMIT: i32 = 1;
/// Largest limit one scope may carry.
pub const MAX_LIMIT: i32 = 1_000_000;
/// Largest burst one scope may forgive above its limit.
pub const MAX_BURST: i32 = 10_000;

/// One scope's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatePolicy {
    /// The scope's name; one of [`RATE_SCOPES`].
    pub scope: String,
    /// The window in seconds (`1`–`86_400`).
    pub window_seconds: i64,
    /// Requests allowed per window (`1`–`1_000_000`).
    pub limit: i32,
    /// Extra requests forgiven inside the same window (`0`–`10_000`).
    pub burst: i32,
    /// Whether the scope is enforced at all.
    pub enabled: bool,
}

impl RatePolicy {
    /// The document a platform starts from: every scope present, sign-in the strictest.
    ///
    /// The defaults are the four a reader would write by hand, and they are deliberately
    /// *written out* rather than derived: an operator looking at the table must be able to see
    /// what the platform believes without reading Rust.
    #[must_use]
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                scope: "global".to_owned(),
                window_seconds: 60,
                limit: 600,
                burst: 100,
                enabled: true,
            },
            Self {
                scope: "sign_in".to_owned(),
                window_seconds: 300,
                limit: 10,
                burst: 0,
                enabled: true,
            },
            Self {
                scope: "public_api".to_owned(),
                window_seconds: 60,
                limit: 120,
                burst: 20,
                enabled: true,
            },
            Self {
                scope: "authenticated_api".to_owned(),
                window_seconds: 60,
                limit: 1200,
                burst: 200,
                enabled: true,
            },
            Self {
                scope: "webhook_intake".to_owned(),
                window_seconds: 60,
                limit: 300,
                burst: 50,
                enabled: true,
            },
        ]
    }

    /// Build one scope's policy, refusing anything out of range with a field-level reason.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] naming the offending field — `scope`, `window_seconds`,
    /// `limit` or `burst` — so the form can put the message on the row that is wrong instead of
    /// toasting "invalid rate limit".
    pub fn new(
        scope: impl Into<String>,
        window_seconds: i64,
        limit: i32,
        burst: i32,
        enabled: bool,
    ) -> Result<Self> {
        let scope = scope.into();
        let scope = scope.trim().to_lowercase();
        if scope.is_empty() {
            return Err(SecurityError::invalid("a rate-limit scope cannot be empty"));
        }
        if !RATE_SCOPES.contains(&scope.as_str()) {
            return Err(SecurityError::invalid(format!(
                "\"{scope}\" is not a rate-limit scope — use one of: {}",
                RATE_SCOPES.join(", ")
            )));
        }
        if window_seconds < 1 || window_seconds > MAX_WINDOW_SECONDS {
            return Err(SecurityError::invalid(format!(
                "window_seconds must be between 1 and {MAX_WINDOW_SECONDS} — {window_seconds} is \
                 outside that range"
            )));
        }
        if limit < MIN_LIMIT || limit > MAX_LIMIT {
            return Err(SecurityError::invalid(format!(
                "limit must be between {MIN_LIMIT} and {MAX_LIMIT} — {limit} is outside that \
                 range, and a limit of 0 means \"refuse everything\", which is what the enabled \
                 switch is for"
            )));
        }
        if !(0..=MAX_BURST).contains(&burst) {
            return Err(SecurityError::invalid(format!(
                "burst must be between 0 and {MAX_BURST} — {burst} is outside that range"
            )));
        }
        Ok(Self {
            scope,
            window_seconds,
            limit,
            burst,
            enabled,
        })
    }

    /// Whether the scope's window admits a request.
    ///
    /// A scope that was never stored is **not** enforced here — the store merges the document
    /// with [`RatePolicy::defaults`] first, so a missing row is a default row, not a hole.
    /// This method is about the row in front of it and nothing else.
    #[must_use]
    pub fn admits(&self, count: i64) -> bool {
        count <= i64::from(self.limit + self.burst)
    }

    /// The most requests this scope allows inside its window, including burst.
    #[must_use]
    pub fn ceiling(&self) -> i64 {
        i64::from(self.limit + self.burst)
    }

    /// The Redis key a scope's counter lives under.
    ///
    /// The window is a *bucket*, not a sliding log: the key carries the window index, so a
    /// window ends whether or not anything is watching it, and a counter cannot be inflated by
    /// an attacker who simply never lets a window expire. The client part is **hashed** rather
    /// than written literally, and that is not tidiness — a client string is attacker-controlled
    /// and an address in IPv6 is 39 characters, so a literal key is long, and `short_hash` fixes
    /// both at once: a key whose length the caller cannot influence, and no address in the key
    /// space to collide with a scope name.
    #[must_use]
    pub fn counter_key(&self, client: &ClientId, now: i64) -> String {
        let bucket = now.div_euclid(self.window_seconds.max(1));
        format!(
            "omnion:rl:{}:{}:{}",
            self.scope,
            bucket,
            short_hash(&self.counter_identity(client))
        )
    }

    /// The clock the key's bucket is computed from, so a caller can prove the two agree.
    ///
    /// Exposed because the test that pins the bucket arithmetic needs a starting point it can
    /// reason about, and "pick a number that happens to be 41 seconds into a minute" is not one:
    /// 1_000_000_000 is not a multiple of 60, which is exactly the mistake the first version of
    /// that test made.
    #[must_use]
    pub fn aligned_epoch() -> i64 {
        1_000_000_020 // 60 × 16_666_667
    }

    /// The identity a scope's counter is keyed on — the **address**, always.
    ///
    /// This is the reason [`ClientId::key`] is not used directly for a request. At sign-in there
    /// is no session, so `user_id` is `None` in practice — but the platform must not *depend* on
    /// that. A limiter keyed on a submitted identity can be aimed: an attacker holding one
    /// account spreads their guesses across N of their own and never exhausts any one of them,
    /// or a body carrying a victim's id draws the victim's budget down. The address is what
    /// identifies the caller of an unauthenticated endpoint, so the address is what the counter
    /// is keyed on.
    ///
    /// The authenticated case is the exception and it is handled by the scope, not here:
    /// `authenticated_api` is only ever reached by a request that already has a session, and one
    /// office behind one NAT should not exhaust a shared limit by itself — so that scope's key
    /// *is* the user. [`decide`] and the middleware pick between them, and this method names the
    /// address so the choice is one line rather than a branch repeated in three places.
    #[must_use]
    pub fn counter_identity(&self, client: &ClientId) -> String {
        match self.scope.as_str() {
            "authenticated_api" => client.key(),
            _ => match client.ip {
                Some(ip) => format!("ip:{ip}"),
                None => client.key(),
            },
        }
    }
}

/// What the limiter decided, and everything needed to act on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the request is refused.
    pub limited: bool,
    /// The scope that decided it. Present on both answers: "allowed" says *which* policy let it
    /// through, which is the question an operator asks when a request unexpectedly was not
    /// limited.
    pub scope: String,
    /// Why, in one sentence, for the panel's tester and the log line.
    pub reason: String,
    /// What the scope counts, including the request being decided.
    pub count: i64,
    /// The ceiling the count was measured against.
    pub ceiling: i64,
    /// Seconds until the window frees a place, for `Retry-After`. `None` when allowed.
    pub retry_after: Option<i64>,
}

/// The client identity a limiter counts.
///
/// A request behind a proxy has an address in `X-Forwarded-For`; the API is told which headers
/// it may believe at boot, and this struct is the result of that decision rather than a guess
/// inside the limiter. Guessing here would let a client mint a new identity per request by
/// sending a random header, which is the one mistake that makes a limiter worse than none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientId {
    /// The authenticated user, when there is one. Authenticated scopes count per user, not per
    /// address, so one office behind one NAT does not exhaust a shared limit.
    pub user_id: Option<String>,
    /// The peer address.
    pub ip: Option<IpAddr>,
}

impl ClientId {
    /// The string the counter is keyed on.
    ///
    /// Prefixed by kind on purpose: the address `1.2.3.4` and the user id `1.2.3.4` are not the
    /// same client, and without the prefix they would share a counter.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.user_id {
            Some(user) => format!("user:{user}"),
            None => match self.ip {
                Some(ip) => format!("ip:{ip}"),
                None => "anon".to_owned(),
            },
        }
    }
}

/// A request, reduced to what the limiter needs to judge it.
#[derive(Debug, Clone)]
pub struct RequestFacts<'a> {
    /// The HTTP method.
    pub method: &'a str,
    /// The path.
    pub path: &'a str,
    /// Who is asking.
    pub client: &'a ClientId,
    /// Whether the request authenticated with a bearer machine key.
    pub machine_key: bool,
    /// Whether the route is the public renderer or a webhook intake path — the two surfaces
    /// that must not be able to lock each other out.
    pub exempt: bool,
}

/// Pick the scope a request belongs to.
///
/// The order is the design and each step is a question the next one cannot answer:
///
/// 1. **Exempt surfaces first.** The public renderer serves a cached page to anonymous traffic
///    and a webhook intake is called by someone else's server. Refusing either from *this* row
///    would make a rate limit a denial-of-service lever pointed at the platform's own users.
/// 2. **Sign-in second.** It is the narrowest path and the one worth attacking, and it is
///    counted by address even when a body is present — a rate limit keyed on a submitted email
///    lets an attacker lock a known account by using its name.
/// 3. **Unauthenticated API third.** A caller with no identity is capped at the public tier.
/// 4. **Webhook intake fourth**, matched by path, because an intake endpoint is authenticated
///    by a signature and is the one authenticated path that a stranger can aim at.
/// 5. **Global last.** Everything not claimed above falls to the catch-all.
#[must_use]
pub fn scope_of(facts: &RequestFacts<'_>) -> &'static str {
    if facts.exempt {
        return "exempt";
    }
    if is_sign_in(facts.method, facts.path) {
        return "sign_in";
    }
    if facts.client.user_id.is_none() {
        return "public_api";
    }
    if is_webhook_intake(facts.path) {
        return "webhook_intake";
    }
    "authenticated_api"
}

/// `true` for the sign-in endpoints, matched on the last path segment.
fn is_sign_in(method: &str, path: &str) -> bool {
    if !matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    ) {
        return false;
    }
    let path = path.trim_end_matches('/');
    path.ends_with("/sign-in")
        || path.ends_with("/signin")
        || path.ends_with("/login")
        || path.ends_with("/session")
        || path.ends_with("/mfa/verify")
}

/// `true` for a webhook delivery intake path.
fn is_webhook_intake(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.contains("/webhooks/intake") || path.contains("/hooks/intake") || path.ends_with("/intake")
}

/// Decide a request against a policy, from the count the store read.
///
/// Pure: no clock, no Redis, no database. `now` is an input so the tester can ask "what would
/// happen at 12:00" and get a repeatable answer.
///
/// # Errors
/// Returns [`SecurityError::Invalid`] when the document holds no policy for the scope that
/// [`scope_of`] selected — a gap that must be loud, because silently defaulting it would
/// decide a request the operator believes is limited by a number they never set.
pub fn decide(
    policies: &[RatePolicy],
    scope: &str,
    client: &ClientId,
    count: i64,
    now: i64,
) -> Result<Verdict> {
    let Some(policy) = policies.iter().find(|policy| policy.scope == scope) else {
        return Err(SecurityError::invalid(format!(
            "no rate-limit policy for scope \"{scope}\" — the platform cannot decide whether this \
             request is allowed"
        )));
    };

    if !policy.enabled {
        return Ok(Verdict {
            limited: false,
            scope: scope.to_owned(),
            reason: format!("the {scope} scope is switched off"),
            count,
            ceiling: policy.ceiling(),
            retry_after: None,
        });
    }

    // A machine key is a bearer credential: it is not ambient authority, so a human sharing one
    // address is not the same client as a service account. Counting them together would let one
    // office's browser traffic exhaust a service account's budget.
    if policy.scope == "public_api" && client.user_id.is_some() {
        return Ok(Verdict {
            limited: false,
            scope: scope.to_owned(),
            reason: "an authenticated request is not counted against the public scope".to_owned(),
            count: 0,
            ceiling: policy.ceiling(),
            retry_after: None,
        });
    }

    if policy.admits(count) {
        return Ok(Verdict {
            limited: false,
            scope: scope.to_owned(),
            reason: format!("{count} of {} requests in the window", policy.ceiling()),
            count,
            ceiling: policy.ceiling(),
            retry_after: None,
        });
    }

    // Seconds left in the current bucket. `div_euclid` on the same window the key uses, so the
    // number here is exactly what the next key rollover will do.
    let elapsed = now.rem_euclid(policy.window_seconds.max(1));
    let retry_after = (policy.window_seconds - elapsed).max(1);
    Ok(Verdict {
        limited: true,
        scope: scope.to_owned(),
        reason: format!(
            "{count} requests exceeds the ceiling of {} in the {}-second window",
            policy.ceiling(),
            policy.window_seconds
        ),
        count,
        ceiling: policy.ceiling(),
        retry_after: Some(retry_after),
    })
}

/// Merge a stored document with the defaults so a missing scope is a default, not a hole.
///
/// A stored document that omits a scope gets the default row, and a document with a row the
/// platform does not know is **dropped with the invalid row reported by validation** — this
/// function assumes it is already valid and exists so the middleware, which cannot surface a
/// form error, still has a complete table to work with.
#[must_use]
pub fn merge_with_defaults(document: &serde_json::Value) -> Vec<RatePolicy> {
    let mut merged = RatePolicy::defaults();
    let Some(rows) = document.as_array() else {
        return merged;
    };
    for row in rows {
        let Ok(stored) = serde_json::from_value::<RatePolicy>(row.clone()) else {
            continue;
        };
        if !RATE_SCOPES.contains(&stored.scope.as_str()) {
            continue;
        }
        if let Some(slot) = merged
            .iter_mut()
            .find(|policy| policy.scope == stored.scope)
        {
            *slot = stored;
        }
    }
    merged
}

/// The document the panel renders, as JSON.
#[must_use]
pub fn to_document(policies: &[RatePolicy]) -> serde_json::Value {
    serde_json::Value::Array(
        policies
            .iter()
            .map(|policy| serde_json::to_value(policy).unwrap_or(serde_json::Value::Null))
            .collect(),
    )
}

/// Read a whole document into policies, refusing the first invalid row.
///
/// # Errors
/// Returns [`SecurityError::Invalid`] when the document is not an array of policies, or names
/// the row that is out of range.
pub fn parse_document(document: &serde_json::Value) -> Result<Vec<RatePolicy>> {
    // An empty or unreadable document is the baseline, matching `HeaderPolicy::from_json`: a
    // settings row that was never written must not mean "no limits exist". `{}` is what the
    // migration inserts and `null` is what a missing row reads as — both take this arm.
    let Some(rows) = document.as_array() else {
        return Ok(RatePolicy::defaults());
    };
    rows.iter()
        .map(|row| {
            serde_json::from_value::<RatePolicy>(row.clone()).map_err(|err| {
                SecurityError::invalid(format!("a rate-limit row could not be read: {err}"))
            })
        })
        .collect()
}

/// The scopes' declared vocabulary, as the panel's dropdown options.
///
/// Clamped by the same page cap the store uses so a caller cannot ask for a thousand names and
/// get them; the count is small and fixed, and the clamp is only here so a future scope list
/// cannot make a dropdown unbounded.
#[must_use]
pub fn scope_options() -> Vec<&'static str> {
    RATE_SCOPES.iter().copied().take(MAX_PAGE).collect()
}

/// A short, stable hash of a client string, for use inside a Redis key.
///
/// FNV-1a: the requirement is that a client cannot influence the key's length or inject a
/// separator, not that it is cryptographically strong. Sixteen hex characters is 64 bits, which
/// keeps a key short while making a collision a debugging curiosity rather than a routing
/// problem.
#[must_use]
fn short_hash(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(user: Option<&str>) -> ClientId {
        ClientId {
            user_id: user.map(str::to_owned),
            ip: Some("203.0.113.7".parse().expect("a literal address parses")),
        }
    }

    fn facts<'a>(method: &'a str, path: &'a str, client: &'a ClientId) -> RequestFacts<'a> {
        RequestFacts {
            method,
            path,
            client,
            machine_key: false,
            exempt: false,
        }
    }

    #[test]
    fn a_limit_of_zero_is_refused_because_switching_the_scope_off_is_the_same_thing() {
        // A `limit = 0` row looks like a policy and behaves like a kill switch, and an operator
        // who typed it into a form meant a number. The message has to say what to do instead.
        let error = RatePolicy::new("public_api", 60, 0, 0, true).expect_err("zero is refused");
        assert!(
            error.to_string().contains("switch"),
            "the refusal should name the alternative: {error}"
        );
    }

    #[test]
    fn burst_is_headroom_inside_the_window_and_zero_is_exact() {
        let strict = RatePolicy::new("public_api", 60, 10, 0, true).expect("a valid row");
        assert!(strict.admits(10), "the tenth request is inside the limit");
        assert!(!strict.admits(11), "the eleventh is the first refusal");

        let forgiving = RatePolicy::new("public_api", 60, 10, 5, true).expect("a valid row");
        assert!(forgiving.admits(15), "burst extends the ceiling to 15");
        assert!(!forgiving.admits(16), "and the sixteenth is still refused");
    }

    #[test]
    fn the_refusal_says_how_long_to_wait_and_the_key_rolls_over_on_the_same_clock() {
        let policy = RatePolicy::new("sign_in", 60, 2, 0, true).expect("a valid row");
        // The clock starts on a window boundary, because the whole point of the assertion is
        // that `retry_after` and the key's bucket are the *same* window. Starting 41 seconds in
        // would make the two disagree and the test would be asserting an accident.
        let now = RatePolicy::aligned_epoch();
        let verdict =
            decide(&[policy.clone()], "sign_in", &client(None), 3, now).expect("the policy");
        assert!(verdict.limited, "3 is over a ceiling of 2");
        assert_eq!(
            verdict.retry_after,
            Some(60),
            "a full window, because the clock is on the boundary"
        );

        // One second before the rollover: the wait is 1, and the key changes.
        let verdict = decide(&[policy.clone()], "sign_in", &client(None), 3, now + 59)
            .expect("the policy is there");
        assert_eq!(verdict.retry_after, Some(1));
        assert_ne!(
            policy.counter_key(&client(None), now),
            policy.counter_key(&client(None), now + 60),
            "the counter's key must change when the window does, or the old count lives on"
        );

        // And the count is *not* carried across: a new window starts at zero, which is the whole
        // reason a bucket is used instead of a decaying total.
        let verdict =
            decide(&[policy], "sign_in", &client(None), 1, now + 60).expect("the policy is there");
        assert!(!verdict.limited, "the new window starts empty");
    }

    #[test]
    fn a_window_of_zero_seconds_would_never_roll_over_so_it_is_refused() {
        let error = RatePolicy::new("global", 0, 10, 0, true).expect_err("zero is refused");
        assert!(error.to_string().contains("window_seconds"), "{error}");
        assert!(
            RatePolicy::new("global", MAX_WINDOW_SECONDS + 1, 10, 0, true).is_err(),
            "a day-long window is the ceiling"
        );
    }

    #[test]
    fn a_scope_the_platform_does_not_know_is_refused_by_name() {
        // A typo in a scope name must not silently create a scope nothing enforces.
        let error = RatePolicy::new("publik_api", 60, 10, 0, true).expect_err("unknown scope");
        assert!(error.to_string().contains("publik_api"), "{error}");
        assert!(error.to_string().contains("sign_in"), "{error}");
    }

    #[test]
    fn sign_in_is_counted_by_address_even_when_a_body_names_an_account() {
        // A limiter keyed on the submitted email would let anyone lock a known account out by
        // using its name; the policy is chosen from the method and path, and the counter from
        // the address, so the submitted name never reaches the key.
        let signed_out = client(None);
        let with_email = ClientId {
            user_id: Some("admin@fermag.com.tr".to_owned()),
            ip: Some("203.0.113.7".parse().expect("a literal address parses")),
        };
        let a = scope_of(&facts("POST", "/api/v1/auth/sign-in", &signed_out));
        let b = scope_of(&facts("POST", "/api/v1/auth/sign-in", &with_email));
        assert_eq!(a, "sign_in");
        assert_eq!(b, "sign_in");

        // The *counter* is where this matters, and it is keyed on the address in every scope
        // except the authenticated one — a request carrying a victim's id must not draw the
        // victim's budget down.
        let policy = RatePolicy::new("sign_in", 300, 10, 0, true).expect("a valid row");
        assert!(
            policy.counter_identity(&with_email).starts_with("ip:"),
            "the sign-in counter is the address: {}",
            policy.counter_identity(&with_email)
        );

        // The authenticated scope is the deliberate exception: one office behind one NAT must
        // not exhaust a shared budget by itself.
        let authenticated =
            RatePolicy::new("authenticated_api", 60, 100, 0, true).expect("a valid row");
        assert_eq!(
            authenticated.counter_identity(&with_email),
            "user:admin@fermag.com.tr",
            "a signed-in caller is counted as themselves"
        );
    }

    #[test]
    fn the_public_surface_and_webhook_intake_are_exempt_before_any_policy_is_consulted() {
        let anonymous = client(None);
        let mut renderer = facts("GET", "/api/v1/public/pages/home", &anonymous);
        renderer.exempt = true;
        assert_eq!(scope_of(&renderer), "exempt");

        // An exempt scope has no policy row, which is exactly the point: `decide` must never be
        // called with it, because it would refuse for want of a row.
        assert!(
            !RatePolicy::defaults().iter().any(|p| p.scope == "exempt"),
            "no row for the exempt scope exists to be found"
        );
    }

    #[test]
    fn a_machine_key_is_not_counted_against_the_public_scope() {
        let policy = RatePolicy::new("public_api", 60, 1, 0, true).expect("a valid row");
        let mut machine = client(None);
        machine.user_id = Some("svc_9f2".to_owned());
        let verdict =
            decide(&[policy], "public_api", &machine, 9_999, 0).expect("the policy is there");
        assert!(
            !verdict.limited,
            "a service account is not anonymous traffic"
        );
    }

    #[test]
    fn a_gap_in_the_document_is_a_loud_error_rather_than_a_default_decision() {
        // If a scope is missing the middleware would either refuse everything or allow
        // everything. Both are guesses about a limit the operator believes is set.
        let error = decide(&[], "sign_in", &client(None), 1, 0).expect_err("no row for the scope");
        assert!(error.to_string().contains("sign_in"), "{error}");
    }

    #[test]
    fn the_defaults_cover_every_scope_the_vocabulary_names() {
        let defaults = RatePolicy::defaults();
        for scope in RATE_SCOPES {
            assert!(
                defaults.iter().any(|policy| policy.scope == *scope),
                "the defaults must include {scope}"
            );
        }
        assert_eq!(defaults.len(), RATE_SCOPES.len(), "no duplicate scopes");
    }

    #[test]
    fn a_user_id_and_an_address_that_happen_to_match_do_not_share_a_counter() {
        // Without the prefix, a user whose id is a literal address would draw down the counter of
        // the machine at that address.
        let as_user = ClientId {
            user_id: Some("203.0.113.7".to_owned()),
            ip: None,
        };
        let as_ip = client(None);
        assert_eq!(as_user.key(), "user:203.0.113.7");
        assert_eq!(as_ip.key(), "ip:203.0.113.7");
        assert_ne!(as_user.key(), as_ip.key());
    }

    #[test]
    fn a_client_string_cannot_lengthen_the_key_it_is_hashed_into() {
        let policy = RatePolicy::new("global", 60, 10, 0, true).expect("a valid row");
        let short = policy.counter_key(&client(None), 0);
        let mut wide = client(None);
        wide.ip = Some(
            "2001:db8:0:0:0:0:2:1"
                .parse()
                .expect("a literal address parses"),
        );
        let long = policy.counter_key(&wide, 0);
        assert_eq!(short.len(), long.len(), "the hash fixes the key's length");
    }

    #[test]
    fn a_stored_document_missing_a_scope_gets_the_default_row() {
        let mut stored = RatePolicy::defaults();
        stored.retain(|policy| policy.scope != "webhook_intake");
        let document = serde_json::to_value(&stored).expect("a serialisable document");
        let merged = merge_with_defaults(&document);
        assert_eq!(merged.len(), RATE_SCOPES.len());
        assert!(merged.iter().any(|policy| policy.scope == "webhook_intake"));
    }

    #[test]
    fn a_document_that_is_not_a_list_reads_as_the_baseline() {
        // `{}` is what an unwritten settings row holds; it must not mean "no limits exist".
        for document in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!("nonsense"),
        ] {
            let parsed = parse_document(&document).expect("an unreadable document is the baseline");
            assert_eq!(parsed.len(), RATE_SCOPES.len(), "{document}");
        }
    }

    #[test]
    fn a_switched_off_scope_allows_everything_and_says_so() {
        let policy = RatePolicy::new("global", 60, 1, 0, false).expect("a valid row");
        let verdict =
            decide(&[policy], "global", &client(None), 1_000, 0).expect("the policy is there");
        assert!(!verdict.limited);
        assert!(
            verdict.reason.contains("switched off"),
            "{}",
            verdict.reason
        );
    }

    #[test]
    fn the_webhook_intake_path_is_claimed_before_the_authenticated_catch_all() {
        let authenticated = client(Some("svc_1"));
        assert_eq!(
            scope_of(&facts("POST", "/api/v1/webhooks/intake", &authenticated)),
            "webhook_intake"
        );
        assert_eq!(
            scope_of(&facts("GET", "/api/v1/pages", &authenticated)),
            "authenticated_api"
        );
    }
}
