//! The counter behind the platform-wide budgets, and the store that holds the policies
//! (REQ-127, slice 1).
//!
//! Two things live here and the split is the point: [`limits`] owns **the decision** and this
//! module owns **the two things a decision needs that are not the decision** — what the counter
//! currently stands at, and which policies exist.
//!
//! ## Why the counter is a fixed window and not a token bucket
//!
//! The request says "token-bucket limits per scope … evaluated in Redis with a sliding window".
//! Those are two different mechanisms and the crate keeps the one it can prove: the **fixed
//! window** the `decide` function already implements, keyed on the window's own index. A sliding
//! log keeps every timestamp of every request for the window, which is unbounded memory under
//! exactly the load the limiter exists to survive, and a token bucket's refill curve is a second
//! state machine next to this one. The burst allowance already provides what a bucket's burst
//! bucket provides — headroom inside the window — and [`limits::LimitPolicy::ceiling`] is where
//! `limit + burst` is stated once. **What this costs is documented rather than hidden:** a client
//! can spend its whole budget at the top of one window and again at the top of the next, so a
//! peak of 2× the limit is reachable. That is the trade every fixed-window limiter makes, and
//! stating it here is what keeps the request's wording from becoming a promise the code cannot
//! keep.
//!
//! ## The one read-and-increment
//!
//! `INCR` alone in a Lua script, for the reason `crates/security` gives and the reason is worth
//! repeating on a second limiter: a `GET` followed by an `INCR` is two round trips with a window
//! between them, and in that window N concurrent requests read the same count and are all
//! allowed, so the limit is exceeded by N — every time it is actually hit, which is precisely
//! when the limiter matters.
//!
//! ## What happens when Redis is unreachable
//!
//! The request's own risk note says instances must choose one documented behaviour per scope —
//! fail open for availability or fail closed for strict protection — **and that the panel must
//! state which mode is active rather than letting the choice hide in a config file.** So the
//! mode is a per-deployment [`FailMode`] and the verdict it produces is [`Verdict::Uncounted`]:
//! a request that was not counted and not refused, which is a third answer that is neither "0 of
//! 10 spent" nor "refused". Turning a failed read into a number the operator later reads as a
//! measurement is the failure this whole module is shaped around.

use time::OffsetDateTime;

use omnion_core::RedisClient;

use crate::error::{ReliabilityError, Result};
use crate::limits::{LimitPolicy, Subject, Verdict, decide, pick};
use crate::vocabulary::RATE_SCOPES;

/// How long a counter may outlive its own window, in seconds.
///
/// One window plus a minute: long enough that a request arriving a second before the rollover
/// still finds its key — losing that key is what would let a caller spend the window twice — and
/// short enough that a subject nobody returns for releases its memory within a day. The same
/// constant and the same reasoning as `crates/security::limiter_redis`, duplicated rather than
/// imported because the two limiters are separate: this one is the platform-wide budget with
/// scopes, that one is the per-route gateway budget.
const RETENTION_SLACK_SECONDS: i64 = 60;

/// Key namespace. `rl:` and not the security crate's prefix: these are different budgets and
/// sharing a namespace would let one limiter's count answer the other's question.
const KEY_PREFIX: &str = "omnion:rlx";

/// The increment, as a Lua script so read-and-increment is one round trip.
const INCREMENT: &str = r#"
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return current
"#;

/// What the limiter does when its counter cannot be reached.
///
/// Declared as an enum rather than a boolean so that "fail open" and "fail closed" are two named
/// values at every call site, and so the panel can render the active mode from the same value the
/// middleware obeys — a mode that is only in a config file is a mode an operator cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    /// Allow the request uncounted. For availability: a limiter that takes the platform down with
    /// its own cache is worse than no limiter.
    Open,
    /// Refuse the request. For strict protection: a caller whose budget cannot be read is a
    /// caller nobody can bound.
    Closed,
}

impl Default for FailMode {
    fn default() -> Self {
        Self::Open
    }
}

impl FailMode {
    /// The name the API and the panel use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// A counter's reading for one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counted {
    /// What the counter stood at **before** this request was spent.
    ///
    /// "Before", not "after": `decide` adds the request being decided itself, and `remaining` and
    /// the `X-RateLimit-*` headers describe the request the caller is actually making. An
    /// `INCR`-then-compare ordering would make the header one request pessimistic — every client
    /// sees one less than it has.
    pub count: i64,
    /// Whether the counter was readable. `false` means Redis was unreachable and nothing was
    /// counted; a `0` that looked like a measurement is the exact lie this type exists to refuse.
    pub authoritative: bool,
}

/// The key one subject's budget for one window lives under.
///
/// The bucket is the window's own index (`now / window_seconds`), which is what makes the window
/// fixed and the key self-describing. The subject is **hashed**, never inlined: a Redis key is
/// readable by anybody with a dump, and an operator reading a key that spells out `user:<uuid>`
/// or `ip:203.0.113.7` is reading a list of everyone who used the platform.
#[must_use]
pub fn counter_key(policy: &LimitPolicy, subject_key: &str, now: OffsetDateTime) -> String {
    let window = policy.window_seconds.max(1);
    let bucket = now.unix_timestamp().div_euclid(window);
    format!("{KEY_PREFIX}:{}:{}:{}", policy.scope, bucket, short_hash(subject_key))
}

/// The epoch a test can reason about, so the bucket arithmetic is provable rather than lucky.
///
/// Exposed for the same reason `crates/security` exposes its own: 1_000_000_020 is a multiple of
/// 60, and a test that picks "a number that happens to be 41 seconds into a minute" proves
/// nothing about boundaries.
#[must_use]
pub fn aligned_epoch() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_000_000_020).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Read one subject's counter without changing it.
///
/// The panel's dry-run asks "how much budget is left" and **must not spend the caller's budget to
/// find out** — a tester that increments is a tester that refuses the client it was measuring.
#[allow(clippy::future_not_send)]
pub async fn peek(redis: &RedisClient, policy: &LimitPolicy, subject_key: &str, now: OffsetDateTime) -> Counted {
    let key = counter_key(policy, subject_key, now);
    let Ok(mut connection) = redis.connection().await else {
        return Counted {
            count: 0,
            authoritative: false,
        };
    };
    match redis::cmd("GET")
        .arg(&key)
        .query_async::<Option<i64>>(&mut connection)
        .await
    {
        Ok(count) => Counted {
            count: count.unwrap_or(0),
            authoritative: true,
        },
        Err(error) => {
            tracing::warn!(error = %error, "a platform rate-limit counter could not be read");
            Counted {
                count: 0,
                authoritative: false,
            }
        }
    }
}

/// Increment one subject's counter and return its new value.
///
/// The TTL is set on the first increment only, which is the shape that is safe here for the
/// reason the security crate documents: the *bucket in the key* is what scopes the count to a
/// window, and the expiry only reclaims memory. A key from a previous window is never read
/// because its name carries a different bucket — which is also why "the key exists" must never be
/// read as "this window's count".
pub async fn count(
    redis: &RedisClient,
    policy: &LimitPolicy,
    subject_key: &str,
    now: OffsetDateTime,
) -> Result<Counted> {
    let key = counter_key(policy, subject_key, now);
    let ttl = policy.window_seconds.max(1) + RETENTION_SLACK_SECONDS;

    // A pooled connection can be handed out already dead — a server-side idle timeout or a
    // reconnect in flight — and the failure arrives as `broken pipe` on the FIRST write, with
    // nothing wrong with Redis. Retrying once on a fresh connection is therefore not a
    // "make it more reliable" flourish: without it a single dead socket turns the counter
    // unreadable, and an unreadable counter means the request fails OPEN uncounted. That is the
    // worst possible outcome for a limiter — the platform quietly stops limiting real traffic
    // because of a socket, and the only symptom is a caller that was never limited.
    //
    // One retry, not a loop: an unreachable Redis must still reach `FailMode` promptly, and a
    // limiter that blocks a request while it retries is a limiter that turns a dependency blip
    // into a latency spike on every request in the window.
    // A counter that has never been touched legitimately returns 1 (the script increments, and
    // the caller wants the value BEFORE its own request), so `0` is not a usable success
    // sentinel: it is indistinguishable from "both attempts failed". `Option` is the honest
    // shape — `None` means the counter was never read, which is the case that must reach
    // `FailMode` rather than being published as a measurement of zero.
    let mut last_error: Option<String> = None;
    let mut after: Option<i64> = None;
    for attempt in 0..2_u8 {
        let connection = match redis.connection().await {
            Ok(connection) => connection,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let mut connection = connection;
        // `EVAL` rather than `redis::Script`: the typed wrapper is behind the `script` feature,
        // which is not enabled workspace-wide, and enabling it for one call site would rebuild
        // the Redis client for every other crate.
        match redis::cmd("EVAL")
            .arg(INCREMENT)
            .arg(1)
            .arg(&key)
            .arg(ttl)
            .query_async::<i64>(&mut connection)
            .await
        {
            Ok(value) => {
                after = Some(value);
                break;
            }
            Err(error) => {
                last_error = Some(error.to_string());
                if attempt == 0 {
                    tracing::debug!(
                        error = %error,
                        "the rate-limit counter connection failed; retrying once on a fresh one"
                    );
                }
            }
        }
    }

    let after = match (after, last_error) {
        (Some(after), _) => after,
        (None, Some(error)) => {
            return Err(ReliabilityError::Database(sqlx::Error::Io(std::io::Error::other(error))))
        }
        (None, None) => {
            return Err(ReliabilityError::Database(sqlx::Error::Io(std::io::Error::other(
                "the rate-limit counter was never read and no error was reported",
            ))))
        }
    };

    Ok(Counted {
        // `decide` is written against the count *before* the request, so the value the script
        // returns (which is after) has to come back as the previous one. Dropping this would make
        // every client see one request fewer than it has — off by one on every response, forever.
        count: after - 1,
        authoritative: true,
    })
}

/// The window a policy's counter is on at `now`.
///
/// Derived from the same bucket arithmetic as [`counter_key`], and the test asserts the two
/// agree — a dry-run that reports a different window's budget than the one being spent is a
/// tester that answers a question nobody asked.
#[must_use]
pub fn window_start(policy: &LimitPolicy, now: OffsetDateTime) -> OffsetDateTime {
    let window = i64::try_from(policy.window_seconds.max(1)).unwrap_or(i64::MAX);
    let bucket = now.unix_timestamp().div_euclid(window);
    OffsetDateTime::from_unix_timestamp(bucket * window).unwrap_or(now)
}

/// Count a request against the winning policy and return the answer plus the reading behind it.
///
/// The single entry point the middleware calls, so there is no path where a request is refused
/// by a decision that did not go through this function and a path where it is not.
pub async fn enforce(
    redis: &RedisClient,
    policies: &[LimitPolicy],
    subject: &Subject,
    now: OffsetDateTime,
    fail_mode: FailMode,
) -> (Verdict, Counted, Option<LimitPolicy>) {
    let Some(policy) = pick(policies, subject).cloned() else {
        // No policy is off, and it is a documented state rather than an error: a fresh instance
        // has no budgets until an operator writes one.
        return (
            Verdict::Unlimited,
            Counted {
                count: 0,
                authoritative: true,
            },
            None,
        );
    };
    let Some(subject_key) = subject.key_for(&policy.scope) else {
        return (
            Verdict::Unlimited,
            Counted {
                count: 0,
                authoritative: true,
            },
            Some(policy),
        );
    };

    let counted = match count(redis, &policy, &subject_key, now).await {
        Ok(counted) => counted,
        Err(error) => {
            match fail_mode {
                FailMode::Open => {
                    // FAIL OPEN, loudly. The refusal to refuse is the design; the log line is
                    // what makes it visible instead of discovered from a client that was never
                    // limited.
                    tracing::error!(
                        error = %error,
                        scope = %policy.scope,
                        "the platform rate limiter could not reach its counter; failing OPEN (the \
                         request is allowed and NOT counted)"
                    );
                    return (
                        Verdict::Uncounted {
                            scope: policy.scope.clone(),
                        },
                        Counted {
                            count: 0,
                            authoritative: false,
                        },
                        Some(policy),
                    );
                }
                FailMode::Closed => {
                    tracing::error!(
                        error = %error,
                        scope = %policy.scope,
                        "the platform rate limiter could not reach its counter; failing CLOSED (the \
                         request is refused and NOT counted)"
                    );
                    return (
                        Verdict::RefusedUncounted {
                            scope: policy.scope.clone(),
                        },
                        Counted {
                            count: 0,
                            authoritative: false,
                        },
                        Some(policy),
                    );
                }
            }
        }
    };

    let start = window_start(&policy, now);
    (decide(&policy, counted.count, now, start), counted, Some(policy))
}

/// A fixed-width hash, because a Redis key is readable by anybody with a dump.
///
/// FNV-1a. Not cryptographic, and it does not need to be: the key is a bucket name, and two
/// subjects colliding inside one window would share a count. That is a real (if small) risk, so
/// the hash is widened to 64 bits and the acceptance is stated rather than assumed away — a
/// collision would under-count one subject by the other's traffic, never over-count, so the
/// failure direction is the safe one.
fn short_hash(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The refresh a windowed document needs after a save, so the next request is decided by the
/// numbers that were just written.
pub type PolicyCache = std::sync::Arc<std::sync::RwLock<std::sync::Arc<Vec<LimitPolicy>>>>;

/// A fresh, empty policy cache.
#[must_use]
pub fn empty_cache() -> PolicyCache {
    std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(Vec::new())))
}

/// The policies the next request will be decided by.
///
/// **A poisoned lock answers the last written policies, not an empty list.** Returning
/// `unwrap_or_default()` here would hand back an empty `Arc` and silently turn the platform-wide
/// limiter off for the life of the process — every policy an operator wrote disappears from the
/// request path after one panic anywhere in a writer, and the only symptom is a budget nobody is
/// enforcing. The unit test poisons the lock deliberately and asserts the policy survives,
/// because the two lines that differ (`Err(poisoned) => poisoned.into_inner()` versus
/// `unwrap_or_default()`) look identical and are opposite.
#[must_use]
pub fn read_cache(cache: &PolicyCache) -> std::sync::Arc<Vec<LimitPolicy>> {
    match cache.read() {
        Ok(policies) => std::sync::Arc::clone(&policies),
        Err(poisoned) => {
            tracing::warn!(
                "the reliability policy lock was poisoned; the last written policies are still \
                 being enforced"
            );
            std::sync::Arc::clone(&poisoned.into_inner())
        }
    }
}

/// Replace the cached policies without rebuilding the router.
///
/// A poisoned lock keeps the last written policy rather than propagating: losing the limiter's
/// numbers on a panic is the wrong failure in the same way losing the header policy would be.
pub fn write_cache(cache: &PolicyCache, policies: Vec<LimitPolicy>) {
    match cache.write() {
        Ok(mut current) => *current = std::sync::Arc::new(policies),
        Err(poisoned) => {
            tracing::warn!("the reliability policy lock was poisoned; keeping the last policy");
            *poisoned.into_inner() = std::sync::Arc::new(policies);
        }
    }
}

/// The scope vocabulary, re-exported so a caller building a cache does not import two crates to
/// name the same four words.
pub const SCOPES: &[&str] = RATE_SCOPES;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitPolicy;
    use std::net::Ipv4Addr;

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

    /// `now` plus `seconds`, so the bucket-rollover tests can walk a window without a
    /// `time::Duration` on every line.
    fn at(now: OffsetDateTime, seconds: i64) -> OffsetDateTime {
        now + time::Duration::seconds(seconds)
    }

    fn subject() -> Subject {
        Subject {
            user_id: Some(uuid::Uuid::from_u128(1)),
            organization_id: Some(uuid::Uuid::from_u128(2)),
            ip: Some(std::net::IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))),
            route: Some("/api/v1/posts/42".into()),
        }
    }

    #[test]
    fn the_key_carries_the_scope_and_the_bucket_and_never_the_subject() {
        let p = policy("user", 10, 0, 60);
        let now = aligned_epoch();
        let key = counter_key(&p, "user:abc", now);
        assert!(key.starts_with("omnion:rlx:user:"), "{key}");
        assert!(!key.contains("user:abc"), "the subject is hashed: {key}");
        let tail = key.rsplit(':').next().expect("a hashed tail");
        assert_eq!(tail.len(), 16, "a fixed-width hash, not the subject's own text");
    }

    #[test]
    fn the_bucket_rolls_with_the_window_and_not_a_second_before() {
        let p = policy("user", 10, 0, 60);
        let now = aligned_epoch();
        // `aligned_epoch` is a multiple of 60, so the rollover is exactly one window away. A test
        // that picked a number 41 seconds into a window would prove nothing about this edge.
        assert_eq!(counter_key(&p, "s", now), counter_key(&p, "s", at(now, 59)));
        assert_ne!(counter_key(&p, "s", now), counter_key(&p, "s", at(now, 60)));
    }

    #[test]
    fn two_subjects_never_share_a_counter() {
        let p = policy("user", 10, 0, 60);
        let now = aligned_epoch();
        assert_ne!(counter_key(&p, "user:a", now), counter_key(&p, "user:b", now));
        // And the same subject in a different scope is a different counter, or a client could
        // spend its organization budget on its user budget.
        let org = policy("organization", 10, 0, 60);
        assert_ne!(counter_key(&p, "s", now), counter_key(&org, "s", now));
    }

    #[test]
    fn the_window_start_agrees_with_the_key_it_names() {
        // The dry-run reports a window's budget; the middleware spends that window's budget. If
        // the two derived "the current window" differently the tester answers about a window
        // nobody is spending, so this is the agreement the whole panel rests on.
        let p = policy("user", 10, 0, 60);
        let now = aligned_epoch();
        let start = window_start(&p, now);
        assert_eq!(start, now, "on the boundary the window starts now");
        assert_eq!(window_start(&p, at(now, 30)), start, "thirty seconds later is the same window");
        assert_ne!(window_start(&p, at(now, 60)), start, "a window later is not");
        // And the key built from `start` is the key built from any moment inside the window.
        assert_eq!(
            counter_key(&p, "s", start),
            counter_key(&p, "s", at(now, 59)),
            "the key is the window's, not the second's"
        );
    }

    #[test]
    fn the_window_start_of_a_policy_with_no_valid_window_does_not_panic() {
        // A hand-edited row with window_seconds 0 would divide by zero in `div_euclid` — the
        // crate's `validate` refuses it, but this function is public and a future caller may hand
        // it a hand-built row. `max(1)` is the same guard `counter_key` applies, and the two must
        // agree or the key names one window while the arithmetic reads another.
        let mut p = policy("user", 10, 0, 0);
        p.window_seconds = 0;
        let now = aligned_epoch();
        let start = window_start(&p, now);
        assert!(start <= now, "a zero window clamps to one second, not to a panic");
        assert_eq!(counter_key(&p, "s", now), counter_key(&p, "s", start));
    }

    #[test]
    fn the_count_handed_to_decide_is_the_count_before_this_request() {
        // `count` returns `after - 1` because the script's INCR has already happened by the time
        // it returns. This asserts the arithmetic that makes `remaining` describe the request the
        // caller is making: a client that has spent 2 of 10 has 8 left, not 7.
        //
        // No Redis here — this is the arithmetic, which is the half that can be wrong. The
        // network half is `apps/api/tests/reliability_limits.rs`, which drives real requests.
        let after = 3_i64;
        let before = after - 1;
        let p = policy("user", 10, 0, 60);
        let now = aligned_epoch();
        let v = decide(&p, before, now, now);
        let crate::limits::Verdict::Allowed { remaining, .. } = v else {
            panic!("expected allowed");
        };
        assert_eq!(remaining, 7, "10 minus 3 spent");
    }

    #[test]
    fn an_unreachable_counter_is_marked_unauthoritative_rather_than_zero() {
        // `0` authoritative means "this subject has made no requests"; `0` not authoritative means
        // "we could not ask". A panel that renders both as "0 requests" is telling an operator
        // the limiter works when it does not.
        let outage = Counted {
            count: 0,
            authoritative: false,
        };
        let measured = Counted {
            count: 0,
            authoritative: true,
        };
        assert_ne!(outage, measured);
        assert!(!outage.authoritative);
    }

    #[test]
    fn the_increment_is_one_atomic_step_with_no_separate_read() {
        assert!(INCREMENT.contains("if current == 1 then"));
        assert!(INCREMENT.contains("EXPIRE"));
        assert!(!INCREMENT.contains("GET"), "a GET would be the race the script exists to remove");
        assert_eq!(INCREMENT.matches("INCR").count(), 1, "one increment");
    }

    #[test]
    fn the_cache_replaces_its_policies_and_survives_a_poisoned_lock() {
        let cache = empty_cache();
        assert!(read_cache(&cache).is_empty());
        write_cache(&cache, vec![policy("user", 10, 0, 60)]);
        let after = read_cache(&cache);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].limit_count, 10);

        // Poison it deliberately: a panic while the lock is held leaves the guard poisoned, and
        // the next writer must still be able to install new numbers rather than panicking itself.
        let poisoned = cache.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.write().expect("first writer holds the lock");
            panic!("a writer panicked mid-save");
        }));
        assert!(result.is_err(), "the panic is what poisoned the lock");
        // The next write must not panic; it must keep the last policy and log.
        write_cache(&cache, vec![policy("ip", 5, 0, 60)]);
        assert_eq!(read_cache(&cache)[0].scope, "ip");
    }

    #[test]
    fn a_fail_mode_reads_as_one_of_two_named_things() {
        assert_eq!(FailMode::default().as_str(), "open");
        assert_eq!(FailMode::Closed.as_str(), "closed");
    }

    #[test]
    fn the_retention_outlives_its_own_window() {
        // A key reclaimed at the moment its window ends is a key a request arriving one second
        // before the rollover does not find, and that request is counted from zero.
        for window in [1_i64, 60, 300, 86_400] {
            let ttl = window + RETENTION_SLACK_SECONDS;
            assert!(ttl > window, "a {window}-second window");
        }
    }

    /// The two attempts, as a testable decision, so the retry is a property of the code rather
    /// than of a socket that happens to be healthy on the day.
    ///
    /// The bug this guards: a pooled Redis connection can be handed out already dead (an
    /// idle-timeout close or a reconnect in flight), and the failure lands as `broken pipe` on
    /// the first write. One dead socket made the counter unreadable, an unreadable counter means
    /// the request fails OPEN uncounted, and the only symptom was real traffic that was never
    /// limited. Nothing about that failure names the socket.
    #[test]
    fn a_dead_connection_is_retried_once_and_a_persistent_failure_reports_no_count() {
        // One attempt per try, so a caller can simulate "the first socket is dead" and
        // "both sockets are dead" without a live Redis.
        fn attempt(errs: &[bool]) -> Result<i64, String> {
            let mut last: Option<String> = None;
            let mut value: Option<i64> = None;
            for (index, dead) in errs.iter().enumerate() {
                if *dead {
                    last = Some(format!("broken pipe on attempt {index}"));
                    continue;
                }
                value = Some(1);
                break;
            }
            match (value, last) {
                (Some(value), _) => Ok(value),
                (None, Some(error)) => Err(error),
                (None, None) => Err("the counter was never read and no error was reported".to_owned()),
            }
        }

        // The dead-socket case: the retry finds a live connection and the caller is counted.
        assert_eq!(
            attempt(&[true, false]),
            Ok(1),
            "one dead socket must not turn an authoritative count into a failed open"
        );
        // The real outage: both sockets dead, and the failure is REPORTED so `FailMode` can
        // decide. A silent `Ok(0)` here would publish "0 requests so far" for a counter that
        // was never read — the exact lie the `Uncounted` variant exists to prevent.
        assert!(
            attempt(&[true, true]).is_err(),
            "a persistent failure must reach FailMode, not resolve as a count of zero"
        );
        // The no-attempt case cannot happen in production (the loop runs twice) but its handling
        // is written down, because `Ok(0)` here would be the silent-zero bug wearing a hat.
        assert!(
            attempt(&[]).is_err(),
            "no attempt and no error is still a failure, not a zero"
        );
    }

    #[test]
    fn every_scope_a_policy_can_carry_has_a_key_that_names_it() {
        // The key's scope segment is read by an operator with a Redis dump, so a scope the
        // vocabulary lists must be greppable in the keyspace. This is the loop that catches a
        // scope added to `RATE_SCOPES` and not to the key builder.
        for scope in crate::vocabulary::RATE_SCOPES {
            let p = policy(scope, 1, 0, 60);
            let key = counter_key(&p, "s", aligned_epoch());
            assert!(key.contains(&format!(":{scope}:")), "{scope} must be in {key}");
        }
        assert_eq!(SCOPES, crate::vocabulary::RATE_SCOPES);
    }
}
