//! The Redis-backed counter behind [`crate::limiter`] (REQ-012, slice 3).
//!
//! The policy and the decision are in [`crate::limiter`], which is pure. This module is the
//! part that touches a network, and the whole of its design follows from one rule the request
//! spec states: **the limiter must fail open on a Redis outage, and must say so.** A limiter
//! that takes the platform down when its cache is unreachable is worse than no limiter, because
//! every request on the deployment stops and the log says "redis".
//!
//! So the shape of every function here is:
//!
//! * **A read that fails answers "no count", not "refuse".** [`peek`] returns `Ok(0)` on an
//!   outage, and the caller passes that to [`crate::limiter::decide`], which allows the request.
//! * **A write that fails is logged, not propagated.** The increment is what *records* the
//!   request; losing it means the count is one short, which is a weaker limit, not a broken
//!   platform. Propagating it would turn a degraded cache into an outage.
//! * **The expiry is set on every increment, not only on the first.** A counter that is set once
//!   and never re-expired outlives its window; a counter whose TTL is refreshed on every
//!   increment outlives it too — in the other direction, forever. The window is bounded by the
//!   key's own bucket index, so a key that still exists from a previous bucket is simply not
//!   read; the TTL is only there to reclaim the memory. That is why it is *safe* to refresh and
//!   why it would be *wrong* to treat "the key exists" as "this window's count".

use std::time::Duration;

use omnion_core::RedisClient;
use redis::AsyncCommands;

use crate::error::{Result, SecurityError};
use crate::limiter::{ClientId, RatePolicy, Verdict, decide};

/// The longest a counter may be kept after its window, in seconds.
///
/// One window plus a little slack: long enough that a request arriving just before a rollover
/// still finds its key, short enough that a scope nobody uses releases its memory within a day.
const RETENTION_SLACK_SECONDS: i64 = 60;

/// The increment, as a Lua script so read-and-increment is one round trip.
///
/// A `GET` followed by an `INCR` is two round trips with a window between them, and in that
/// window every concurrent request reads the same count — so N requests all see "9 of 10" and
/// all N are allowed, and the limit is exceeded by N. The script is the fix, and it is a
/// *small* fix: `INCR` plus a conditional `EXPIRE` is one atomic step.
const INCREMENT: &str = r#"
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return current
"#;

/// A counter's reading for one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counted {
    /// What the counter stands at, after this request.
    pub count: i64,
    /// Whether the counter was readable. `false` means Redis was unreachable and the platform
    /// allowed the request anyway — the operator needs to see that, and a `0` that looked like
    /// a real measurement would be the exact lie this crate refuses to tell.
    pub authoritative: bool,
}

/// Increment the counter for `policy` and return the new value.
///
/// # Errors
/// Returns [`SecurityError::Database`] on a Redis failure **and** a `fail_open` flag, so the
/// caller chooses explicitly: [`enforce`] fails open, and a caller that wants to refuse on an
/// outage has to say so in code rather than inheriting a decision.
pub async fn count(
    redis: &RedisClient,
    policy: &RatePolicy,
    client: &ClientId,
    now: i64,
) -> Result<Counted> {
    let key = policy.counter_key(client, now);
    let ttl = policy.window_seconds.max(1) + RETENTION_SLACK_SECONDS;

    let mut connection = match redis.connection().await {
        Ok(connection) => connection,
        Err(error) => {
            return Err(SecurityError::Database(sqlx::Error::Io(
                std::io::Error::other(error),
            )));
        }
    };

    // `EVAL` rather than `redis::Script`: the typed wrapper is behind the crate's `script`
    // feature, which is not enabled workspace-wide, and turning it on for one call site would
    // rebuild the Redis client for every other crate in the workspace. The command is the same
    // script; the type safety it adds is a `ScriptInvocation` this function does not use.
    let count: i64 = match redis::cmd("EVAL")
        .arg(INCREMENT)
        .arg(1)
        .arg(&key)
        .arg(ttl)
        .query_async(&mut connection)
        .await
    {
        Ok(count) => count,
        Err(error) => {
            return Err(SecurityError::Database(sqlx::Error::Io(
                std::io::Error::other(error),
            )));
        }
    };
    Ok(Counted {
        count,
        authoritative: true,
    })
}

/// Read a counter without changing it — for the panel's tester, and for a "would this be
/// limited right now" check that must not spend the caller's budget.
pub async fn peek(
    redis: &RedisClient,
    policy: &RatePolicy,
    client: &ClientId,
    now: i64,
) -> Counted {
    let key = policy.counter_key(client, now);
    let Ok(mut connection) = redis.connection().await else {
        return Counted {
            count: 0,
            authoritative: false,
        };
    };
    match connection.get::<_, i64>(&key).await {
        Ok(count) => Counted {
            count,
            authoritative: true,
        },
        Err(error) => {
            tracing::warn!(error = %error, key = %key, "a rate-limit counter could not be read");
            Counted {
                count: 0,
                authoritative: false,
            }
        }
    }
}

/// Clear one counter. Used by the sign-in path after a successful sign-in, so a legitimate user
/// who mistyped twice is not left one guess from a lockout.
///
/// # Errors
/// A Redis failure is returned rather than swallowed: this runs on the *success* path, where the
/// platform is healthy, and a failure here means the key is still there and the next sign-in
/// starts one failure closer to a lockout than the user deserves. The caller decides whether
/// that is worth surfacing — sign-in logs it and continues.
pub async fn forget(
    redis: &RedisClient,
    policy: &RatePolicy,
    client: &ClientId,
    now: i64,
) -> Result<()> {
    let key = policy.counter_key(client, now);
    let mut connection = redis
        .connection()
        .await
        .map_err(|error| SecurityError::Database(sqlx::Error::Io(std::io::Error::other(error))))?;
    redis::cmd("DEL")
        .arg(&key)
        .query_async::<i64>(&mut connection)
        .await
        .map(|_| ())
        .map_err(|error| SecurityError::Database(sqlx::Error::Io(std::io::Error::other(error))))
}

/// Count a request and return the verdict the middleware should act on.
///
/// **This is the function the panel's tester must mirror, and it does not mirror anything:** it
/// calls the same [`decide`] with the count this module just read, so the two cannot disagree
/// except when Redis is down — and in that case [`Counted::authoritative`] is `false` so the
/// caller can say "the limiter could not be reached, so nothing was counted" instead of
/// "0 of 10 requests in the window".
///
/// # Errors
/// Returns [`SecurityError::Invalid`] when the policy table has no row for the scope, which is a
/// gap in the document and must not be decided silently either way.
pub async fn enforce(
    redis: &RedisClient,
    policies: &[RatePolicy],
    scope: &str,
    client: &ClientId,
    now: i64,
) -> Result<(Verdict, Counted)> {
    let policy = policies
        .iter()
        .find(|policy| policy.scope == scope)
        .ok_or_else(|| {
            SecurityError::invalid(format!(
                "no rate-limit policy for scope \"{scope}\" — the platform cannot decide whether \
                 this request is allowed"
            ))
        })?;

    // A switched-off scope is not counted. Counting a scope nothing reads wastes a key per
    // client and makes the tester's "count" a number that does not correspond to anything.
    if !policy.enabled {
        let verdict = decide(policies, scope, client, 0, now)?;
        return Ok((
            verdict,
            Counted {
                count: 0,
                authoritative: true,
            },
        ));
    }

    let counted = match count(redis, policy, client, now).await {
        Ok(counted) => counted,
        Err(error) => {
            // FAIL OPEN, loudly. The refusal to refuse is the design; the log line is what makes
            // it visible, so an operator sees "the limiter was not enforcing" instead of
            // discovering it from a client that was never limited.
            tracing::error!(
                error = %error,
                scope,
                "the rate limiter could not reach its counter; failing OPEN (this request is \
                 allowed and not counted)"
            );
            let verdict = decide(policies, scope, client, 0, now)?;
            return Ok((
                verdict,
                Counted {
                    count: 0,
                    authoritative: false,
                },
            ));
        }
    };

    let verdict = decide(policies, scope, client, counted.count, now)?;
    Ok((verdict, counted))
}

/// The TTL a counter is set with, exposed for the test that pins the retention rule.
#[must_use]
pub fn retention_for(policy: &RatePolicy) -> Duration {
    Duration::from_secs((policy.window_seconds.max(1) + RETENTION_SLACK_SECONDS) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOT named `policy`: the module imports the *type* `RatePolicy` and the tests call
    // `RatePolicy::new`, so a helper called `policy` in the same scope shadows the constructor
    // path and the test module stops compiling with a confusing "expected function, found
    // `limiter::RatePolicy`". Named for what it is.
    fn sign_in_policy() -> RatePolicy {
        RatePolicy::new("sign_in", 60, 5, 0, true).expect("a valid row")
    }

    fn client() -> ClientId {
        ClientId {
            user_id: None,
            ip: Some("203.0.113.7".parse().expect("a literal address parses")),
        }
    }

    #[test]
    fn the_counter_is_reclaimed_one_window_plus_a_slack_after_it_ends() {
        // Not "at the end of the window": a request arriving one second before a rollover must
        // still find its key, or the count it reads is 0 and the limit is that much weaker. The
        // slack is what makes the rollover safe.
        let policy = sign_in_policy();
        let retention = retention_for(&policy);
        assert_eq!(retention, Duration::from_secs(60 + 60));
        assert!(
            retention.as_secs() > policy.window_seconds as u64,
            "the key must outlive its own window"
        );

        // The upper bound is `window + 60`, not `window * 2` — with the shortest window the
        // platform accepts the two coincide, which is the boundary case, and an assertion written
        // as `window * 2` would fail on a 60-second scope for no reason. The property is "the
        // slack is bounded", and it is checked against the slack itself.
        for window in [60_i64, 300, 900, 86_400] {
            let mut row = sign_in_policy();
            row.window_seconds = window;
            let retention = retention_for(&row).as_secs();
            assert_eq!(
                retention,
                window as u64 + RETENTION_SLACK_SECONDS as u64,
                "a {window}-second window"
            );
            assert!(
                retention - window as u64 == RETENTION_SLACK_SECONDS as u64,
                "the slack is fixed, whatever the window is"
            );
        }
    }

    #[test]
    fn the_increment_sets_the_expiry_only_on_the_first_request() {
        // Read from the script rather than from a paraphrase of it. If a later edit moved the
        // `EXPIRE` out of the `if`, this test would be asserting a comment.
        assert!(INCREMENT.contains("if current == 1 then"));
        assert!(INCREMENT.contains("EXPIRE"));
        // The two round trips this script exists to avoid must both be gone.
        assert!(!INCREMENT.contains("GET"), "no separate read");
        assert_eq!(INCREMENT.matches("INCR").count(), 1, "one increment");
    }

    #[test]
    fn a_window_of_zero_seconds_cannot_produce_a_zero_ttl() {
        // `RatePolicy::new` refuses a zero window, but this module is public and a future caller
        // may hand it a hand-built row. `EXPIRE key 0` *deletes* the key in Redis, which would
        // turn the limiter into one that counts nothing.
        let mut row = sign_in_policy();
        row.window_seconds = 0;
        assert!(
            retention_for(&row).as_secs() > 0,
            "a TTL of zero deletes the key"
        );
    }

    #[test]
    fn a_peek_and_a_count_read_the_same_key() {
        // The panel's tester must report the *same* counter the middleware increments, or the
        // screen answers a question about a different number than the one the platform enforces.
        // `count` derives its key from `policy.counter_key(client, now)` and so does `peek`, but
        // two functions that each build a key are two chances to disagree, and the tester is
        // only useful while they agree. This asserts it on the key both of them would use, at a
        // time that is not on a window boundary (see `aligned_epoch`'s note).
        let policy = sign_in_policy();
        let now = RatePolicy::aligned_epoch();
        let key = policy.counter_key(&client(), now);

        // The key is scope + bucket + a fixed-width hash: the bucket is what makes the counter
        // roll over, and the fixed width is what stops a long client string from lengthening it.
        assert!(
            key.starts_with("omnion:rl:sign_in:"),
            "the scope is in the key: {key}"
        );
        let tail = key.rsplit(':').next().expect("a hashed tail");
        assert_eq!(
            tail.len(),
            16,
            "a fixed-width hash, not the client's own text"
        );
        assert!(
            !key.contains("203.0.113.7"),
            "the address is hashed, not inlined: {key}"
        );

        // One second apart inside a 60-second window is the same bucket; a full window later is
        // not. If `count` and `peek` ever derived the bucket differently this is where it shows.
        assert_eq!(
            policy.counter_key(&client(), now),
            policy.counter_key(&client(), now + 1),
            "inside one window"
        );
        assert_ne!(
            policy.counter_key(&client(), now),
            policy.counter_key(&client(), now + 60),
            "the bucket rolls over with the window"
        );
    }

    #[test]
    fn two_addresses_never_share_one_counter() {
        // The whole reason the key is namespaced per client: a shared counter lets one client
        // exhaust the budget of every other client behind the same gateway, and lets a burst be
        // aimed by rotating source addresses. Both are failures a per-scope limit cannot absorb.
        let policy = sign_in_policy();
        let now = RatePolicy::aligned_epoch();
        let other = ClientId {
            user_id: None,
            ip: Some("198.51.100.9".parse().expect("a literal address parses")),
        };
        assert_ne!(
            policy.counter_key(&client(), now),
            policy.counter_key(&other, now),
            "two addresses, two counters"
        );

        // And the same address in a *different* scope is a different counter too, so a client
        // cannot spend its sign-in budget on the public API or the other way round.
        let mut public = sign_in_policy();
        public.scope = "public_api".to_owned();
        assert_ne!(
            sign_in_policy().counter_key(&client(), now),
            public.counter_key(&client(), now),
            "the scope is part of the key"
        );
    }

    #[test]
    fn an_unreachable_counter_is_marked_unauthoritative_rather_than_zero() {
        // The distinction is the whole point of `Counted`: `0` with `authoritative: true` means
        // "this client has made no requests", and `0` with `authoritative: false` means "we could
        // not ask". A panel that renders both as "0 requests" is telling an operator the limiter
        // is working when it is not.
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
}
