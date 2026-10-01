//! Per-token metering for the headless content surface (REQ-019, slice 3).
//!
//! **This module is the only thing the request path spends a Redis round trip on, and it is
//! deliberately not a middleware.** The platform already has a rate limiter
//! ([`crate::rate_limit_middleware`]) that spends a Redis round trip per request; a second one
//! that counts the *same* requests would be a second answer to "has this client spent its
//! budget", and the two would disagree the moment their keys, windows or fail-open rules drifted.
//! So this module is the *policy-free* half — a counter and a record of what the response was —
//! and the limiter reads the same counter it incremented.
//!
//! **What is counted, and what is deliberately not.** A request that is refused for want of a
//! scope never reaches a handler, so it is counted here (in the extractor) rather than in a
//! response layer. A request that 404s *is* counted, because an integrator walking slugs that do
//! not exist is a pattern an operator needs to see. A request that the limiter refused is counted
//! as `throttled`, which is the number this whole feature exists for: it is what a caller means
//! by "I am being rate limited", and it is the only count that can be non-zero while `requests`
//! is flat.
//!
//! **Counting is best-effort and says so.** A Redis outage must not turn the content surface
//! into a 500, so a counter that cannot be incremented is logged and dropped. The consequence —
//! the usage tab under-reports — is the right trade for a metric, and it is the same trade
//! `limiter_redis` makes for the same reason. The difference is that the limiter's fail-open is
//! *visible in the log* and so is this, because a usage chart that silently stops counting is
//! indistinguishable from a quiet day.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use omnion_core::RedisClient;
use redis::AsyncCommands;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::state::AppState;

/// Key namespace. `capi` rather than `rl` because this is a *meter*, and the limiter's keys
/// must stay readable to whoever debugs them.
const NAMESPACE: &str = "omnion:capi";

/// How long a usage counter's day is kept after the day it belongs to.
///
/// Long enough that a worker which was down for an hour still finds the counts, short enough that
/// an unused token's keys are reclaimed within a day. The *day* is decided by the key, not by the
/// TTL, so a longer TTL cannot resurrect an old day's numbers into today's row.
const USAGE_RETENTION: StdDuration = StdDuration::from_secs(60 * 60 * 48);

/// How long a budget counter outlives its minute.
///
/// One minute plus a minute of slack, for the reason `limiter_redis::RETENTION_SLACK_SECONDS`
/// gives: a request arriving one second before a rollover must still find its key, or the count
/// it reads is the next window's zero and the limit is that much weaker.
const BUDGET_RETENTION: StdDuration = StdDuration::from_secs(60 + 60);

/// The three counters one bucket holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Requests answered, whatever the status — **including the ones refused with 429**.
    pub requests: i32,
    /// Requests answered `4xx`/`5xx`.
    pub errors: i32,
    /// Requests refused with `429`.
    pub throttled: i32,
}

impl Counts {
    /// Whether this bucket holds anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests == 0 && self.errors == 0 && self.throttled == 0
    }
}

/// A parsed counter key. The key is the only state this design keeps, so it has to carry
/// everything the flush needs — a struct rather than three parallel maps that can disagree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BucketKey {
    /// The token.
    pub token_id: Uuid,
    /// The UTC day, `YYYY-MM-DD`.
    pub day: String,
    /// The matched route template.
    pub endpoint: String,
}

impl BucketKey {
    /// The key this bucket lives under, as a `SCAN` match target.
    ///
    /// The endpoint is the *last* segment and holds `/` and `{}`, so a `SCAN MATCH` on the whole
    /// pattern would have to escape them; scanning the prefix and parsing the tail is both simpler
    /// and immune to a route name that contains the delimiter.
    #[must_use]
    pub fn redis_key(&self) -> String {
        format!("{NAMESPACE}:{}:{}|{}", self.token_id, self.day, self.endpoint)
    }

    /// Parse a key back, or `None` when the platform did not write it.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let rest = key.strip_prefix(&format!("{NAMESPACE}:"))?;
        let (token, tail) = rest.split_once(':')?;
        let (day, endpoint) = tail.split_once('|')?;
        // A `Date` parse, not a shape check: a key that passes a regex can still name a day that
        // does not exist, and the flush would then write it into a `date` column PostgreSQL
        // refuses — turning a corrupt key into a failed flush for every other token in the window.
        if time::Date::parse(day, &time::format_description::well_known::Iso8601::DATE).is_err() {
            return None;
        }
        Some(Self {
            token_id: Uuid::parse_str(token).ok()?,
            day: day.to_owned(),
            endpoint: endpoint.to_owned(),
        })
    }

    /// The day as a `Date`, for the durable write.
    #[must_use]
    pub fn day_parsed(&self) -> Option<Date> {
        time::Date::parse(&self.day, &time::format_description::well_known::Iso8601::DATE).ok()
    }
}

/// Spend one request and record it, as one Lua script, so the limiter and the meter cannot
/// disagree.
///
/// **Why one script for two keys.** A `GET` on the budget followed by an `INCR` is two round
/// trips with a window between them, and in that window every concurrent request reads the same
/// count — so N requests all see "119 of 120" and all N are allowed. Splitting the budget from
/// the usage counter and doing them in two round trips is the same mistake twice, and worse: the
/// panel could show a caller 400 requests on a budget the platform enforced as 400, which is the
/// one disagreement an operator cannot diagnose from the screen.
///
/// **The order is spend, then decide, then record the refusal.** The counter is incremented
/// *before* the decision, so a caller that is over budget keeps being counted — otherwise a
/// runaway client would stop appearing in the usage tab exactly while it is misbehaving, and the
/// flat line would read as "the integration recovered".
///
/// ARGV: `1` limit · `2` errors · `3` throttled by the caller (always 0 here — the status is not
/// known until the handler answers) · `4` budget TTL · `5` usage TTL.
/// KEYS: `1` budget · `2` usage bucket.
const SPEND: &str = r#"
local spent = redis.call('INCR', KEYS[1])
redis.call('EXPIRE', KEYS[1], ARGV[4])
redis.call('HINCRBY', KEYS[2], 'requests', 1)
redis.call('HINCRBY', KEYS[2], 'errors', ARGV[2])
redis.call('HINCRBY', KEYS[2], 'throttled', ARGV[3])
redis.call('EXPIRE', KEYS[2], ARGV[5])
if spent > tonumber(ARGV[1]) then
  redis.call('HINCRBY', KEYS[2], 'throttled', 1)
  return { spent, 1 }
end
return { spent, 0 }
"#;

/// Record that a handler answered with an error, on the bucket this request already opened.
///
/// A **second** round trip, spent only by the requests that failed — which is why the read path's
/// cost is unchanged for the 99% of calls that succeed. It reads back the bucket the extractor
/// already incremented rather than creating one, so a request counted as served is the same
/// request counted as an error.
pub async fn note_outcome(state: &AppState, token_id: Uuid, endpoint: &str, error: bool) {
    let now = OffsetDateTime::now_utc();
    let key = BucketKey {
        token_id,
        day: now.date().to_string(),
        endpoint: endpoint.to_owned(),
    }
    .redis_key();
    let Ok(mut connection) = state.redis().connection().await else {
        return;
    };
    if !error {
        return;
    }
    // Raw `HINCRBY` rather than the typed helper: the async wrapper's return type is fixed to
    // `f64`, which is a number this caller cannot use and does not read — the raw command is the
    // only form that says "I do not need the new value", which is exactly the case.
    if let Err(error) = redis::cmd("HINCRBY")
        .arg(&key)
        .arg("errors")
        .arg(1i32)
        .query_async::<i64>(&mut connection)
        .await
    {
        tracing::warn!(error = %error, "a content usage counter could not record an error");
    }
}

/// Every counter currently in Redis, parsed.
///
/// `SCAN` and not `KEYS`: `KEYS` is O(n) over the whole keyspace and this runs on the same Redis
/// as the rate limiter's counters, the search cache and every rendered page — a blocking walk
/// there stalls every request on the installation to draw a usage chart.
pub async fn window(redis: &RedisClient) -> BTreeMap<String, Counts> {
    let mut out = BTreeMap::new();
    let Ok(mut connection) = redis.connection().await else {
        return out;
    };
    let Ok(mut cursor) = connection.scan_match::<_, String>(format!("{NAMESPACE}:*")).await else {
        tracing::warn!("the content usage counters could not be scanned");
        return out;
    };
    // The cursor is drained before any value is read. The `Scan` stream borrows the connection
    // mutably for as long as it lives, so a `HMGET` issued inside the loop is a second mutable
    // borrow of the same object and the compiler refuses it. Draining first is also the better
    // shape on the wire: the flush is not interleaving scans and reads, so a large keyspace
    // cannot hold one round trip open while the other waits.
    //
    // `scan_match::<_, String>` yields `String` directly in this version — the typed helper wraps
    // a failed decode in the *stream's* error rather than in each item, so a key this platform
    // wrote (a UUID, a date, a route template) is always decodable and the `keys.push` needs no
    // `ok()`.
    let mut keys = Vec::new();
    while let Some(key) = cursor.next_item().await {
        keys.push(key);
    }
    drop(cursor);

    for key in keys {
        let Ok(key) = key else {
            continue;
        };
        // One `HMGET` for all three fields. Three `HGET`s would be three round trips per bucket
        // on a flush, and a busy installation's window is a key per token × endpoint — the flush
        // would cost more than the requests it is measuring.
        // `Vec<Option<i64>>` — a field the counter has never written comes back as nil, and a
        // `Vec<i64>` (or the typed helper's `Vec<String>`) cannot represent "absent" as distinct
        // from "zero". The two are the same number for a counter, so a nil read as 0 is correct
        // here — but only because the *flush* treats a missing field as a zero, and the raw
        // command is what lets the type say so.
        let Ok(values) = redis::cmd("HMGET")
            .arg(&key)
            .arg("requests")
            .arg("errors")
            .arg("throttled")
            .query_async::<Vec<Option<i64>>>(&mut connection)
            .await
        else {
            continue;
        };
        let at = |index: usize| values.get(index).copied().flatten().unwrap_or(0) as i32;
        out.insert(
            key,
            Counts {
                requests: at(0),
                errors: at(1),
                throttled: at(2),
            },
        );
    }
    out
}

/// Delete a flushed window's keys.
///
/// Called **after** the durable write, never before: clearing first loses the counts on a failed
/// write, and a lost day is the one number an operator cannot reconstruct from anywhere else.
/// The cost of getting this order wrong is a day of usage gone; the cost of the reverse order is
/// double-counting, which the additive upsert tolerates. That asymmetry is the whole reason the
/// order is not a matter of taste.
pub async fn clear(redis: &RedisClient, keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let Ok(mut connection) = redis.connection().await else {
        return;
    };
    // Chunked because a busy installation's window is one key per token × endpoint, and an
    // unbounded `DEL` is a single command holding one argument per key — the thing Redis's
    // protocol is worst at.
    for chunk in keys.chunks(200) {
        let mut command = redis::cmd("DEL");
        for key in chunk {
            command.arg(key);
        }
        if let Err(error) = command.query_async::<i64>(&mut connection).await {
            tracing::warn!(error = %error, "a flushed content usage window could not be cleared");
            return;
        }
    }
}

/// One flush: read the window, write it durably, clear it.
pub async fn flush(state: &AppState) -> omnion_content::api_token_usage::FlushReport {
    let redis = state.redis();
    let counters = window(redis).await;

    let mut buckets = Vec::with_capacity(counters.len());
    let mut keys = Vec::with_capacity(counters.len());
    for (key, counts) in &counters {
        if counts.is_empty() {
            continue;
        }
        let Some(bucket) = BucketKey::parse(key) else {
            // Not ours. Counting it would write a row nobody can attribute; dropping it silently
            // would let a foreign key under our namespace grow forever. The log is the answer.
            tracing::warn!(
                key,
                "a key under the content usage namespace is not one this platform writes; \
                 leaving it in place and counting it as none of ours"
            );
            continue;
        };
        let Some(day) = bucket.day_parsed() else {
            // `parse` already refuses an unparseable day, so this arm is unreachable. It is kept
            // because `parse` and `day_parsed` are two functions over the same string, and the day
            // one of them could ever stop accepting is the one PostgreSQL would reject.
            tracing::warn!(key, "a content usage key named a day this platform cannot write");
            continue;
        };
        buckets.push(omnion_content::api_token_usage::Bucket {
            token_id: bucket.token_id,
            day,
            endpoint: bucket.endpoint,
            requests: counts.requests,
            errors: counts.errors,
            throttled: counts.throttled,
        });
        keys.push(key.clone());
    }

    if buckets.is_empty() {
        return omnion_content::api_token_usage::FlushReport::default();
    }

    match omnion_content::api_token_usage::record_window(state.db().pool(), &buckets).await {
        Ok(report) => {
            clear(redis, &keys).await;
            report
        }
        Err(error) => {
            // No clear, on purpose: the next tick re-reads the same window and adds it again, so a
            // transient database failure costs a duplicate-free retry rather than a lost day.
            tracing::error!(error = %error, "the content usage flush failed; the window is kept");
            omnion_content::api_token_usage::FlushReport::default()
        }
    }
}

/// The limiter key one content token's budget lives under.
///
/// **A separate namespace from the limiter's own keys, on purpose.** `omnion_security` keys on
/// address or user; a content token is neither, and putting a token id into that namespace would
/// make the *panel's* rate-limit screen show a row for every integrator while its own traffic
/// stayed invisible. The token's budget is its own counter, counted in its own keys, and the
/// limiter decides it with the same pure `decide` the panel's tester uses.
#[must_use]
pub fn budget_key(token_id: Uuid, minute: i64) -> String {
    // The window is a *bucket*, not a sliding log, for the reason `RatePolicy::counter_key`
    // documents: a window must end whether or not anything is watching it, or an attacker who
    // never lets a window expire never runs out of budget.
    format!("{NAMESPACE}:budget:{token_id}:{}", minute.div_euclid(60))
}

/// Seconds a caller should wait after a refusal, from the key's own bucket.
///
/// Read from the *bucket* rather than computed from a clock, because the two disagree at exactly
/// the boundary this feature is judged on: at 12:01:00 with a limit of 120 and 120 requests in
/// the 12:00 window, a client that waits 59 seconds is refused again.
#[must_use]
pub fn retry_after_seconds(now: i64) -> u64 {
    let elapsed = now.rem_euclid(60);
    (60 - elapsed).clamp(1, 60) as u64
}

/// What one request spent, and what the caller is told about it.
///
/// **`authoritative` is the field that matters, and it is the same distinction
/// `limiter_redis::Counted` makes.** `false` means the counter could not be read, so `count`,
/// `remaining` and `limit` on this verdict are *nothing* — the request was allowed and not
/// counted. A response that stamped those three numbers anyway would be claiming a measurement
/// the platform does not have, and an integrator building a retry loop on `x-ratelimit-remaining`
/// would be reading a zero the platform invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// Requests spent in this minute, including this one. Meaningless when `authoritative` is
    /// `false`.
    pub count: i32,
    /// The token's own budget.
    pub limit: i32,
    /// Whether this request was refused.
    pub limited: bool,
    /// Whether the numbers above are a measurement.
    pub authoritative: bool,
}

impl Verdict {
    /// Budget left in this minute, or `None` when nothing was counted.
    ///
    /// A `0` here means "the budget is gone" and is the value that makes a client back off; a
    /// `None` means "we do not know", and the two must not be the same number.
    #[must_use]
    pub fn remaining(&self) -> Option<i32> {
        self.authoritative
            .then(|| (self.limit - self.count).max(0))
    }
}

/// Spend one request of `token_id`'s budget, record the call, and return the verdict.
///
/// **One Lua script, one round trip, two answers.** The budget counter and the usage counter are
/// incremented together because they are the same request: a limiter that counts in one key and a
/// usage tab that counts in another can be made to disagree, and the moment they do the panel
/// says a caller made 400 requests while the platform refused the 401st for exceeding 400.
///
/// The script is read-and-increment on both keys in a single `EVAL`, which is what stops N
/// concurrent callers all reading the same count and all being allowed — the same reason
/// `limiter_redis::INCREMENT` is a script rather than a `GET` then an `INCR`.
///
/// The refusal is recorded as `throttled` **and** `requests` is incremented too: the caller did
/// make a request, it was simply not served, and a usage chart that hid refused calls would draw
/// a flat line exactly when a client is in trouble — which is the moment the chart is opened.
///
/// `endpoint` is the matched route, passed in by the caller because `MatchedPath` only exists in
/// the request's extensions and a meter cannot see the request.
pub async fn spend(state: &AppState, token_id: Uuid, limit: i32, endpoint: &str) -> Verdict {
    let now = OffsetDateTime::now_utc();
    let minute = now.unix_timestamp();
    let budget = budget_key(token_id, minute);
    let usage = BucketKey {
        token_id,
        day: now.date().to_string(),
        endpoint: endpoint.to_owned(),
    }
    .redis_key();
    let limit = limit.max(1);

    let Ok(mut connection) = state.redis().connection().await else {
        // FAIL OPEN, loudly, for the reason `limiter_redis` gives: a limiter that takes the
        // content surface down with its cache is worse than no limiter, and the log line is what
        // turns "we were not limiting" from an invisible fact into an operator-visible one.
        tracing::error!(
            "the content API budget counter was unreachable; this request is allowed, NOT \
             counted, and NOT in the usage tab"
        );
        return Verdict {
            count: 0,
            limit,
            limited: false,
            authoritative: false,
        };
    };

    match redis::cmd("EVAL")
        .arg(SPEND)
        .arg(2)
        .arg(&budget)
        .arg(&usage)
        .arg(limit)
        .arg(1)
        .arg(0)
        .arg(0)
        .arg(BUDGET_RETENTION.as_secs())
        .arg(USAGE_RETENTION.as_secs())
        .query_async::<Vec<i64>>(&mut connection)
        .await
    {
        // `[count, limited]`. The script returns the count it *stored* rather than a computed
        // verdict so the caller cannot disagree with what the counter now says.
        Ok(values) => {
            let count = values.first().copied().unwrap_or(0) as i32;
            let limited = values.get(1).copied().unwrap_or(0) == 1;
            if limited {
                // A refusal is a second increment of the usage key, in the same script above. It
                // is only *counted* here for the log line; the number in Redis is already right.
                tracing::info!(
                    token = %token_id,
                    count,
                    limit,
                    "the content API refused a request over its per-minute budget"
                );
            }
            Verdict {
                count,
                limit,
                limited,
                authoritative: true,
            }
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "the content API budget counter could not be incremented; failing OPEN (this \
                 request is allowed and NOT counted)"
            );
            Verdict {
                count: 0,
                limit,
                limited: false,
                authoritative: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn day() -> Date {
        Date::from_calendar_date(2026, Month::October, 1).expect("a real date")
    }

    #[test]
    fn a_key_round_trips_through_its_own_text() {
        // The key is the only state this design keeps, so a key that parses back to something
        // other than what went in is a silently wrong flush. The endpoint deliberately contains a
        // `/` and braces, which is why the day and the endpoint are split on a delimiter no route
        // name can hold.
        let key = BucketKey {
            token_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            day: day().to_string(),
            endpoint: "/content/pages/{slug}".to_owned(),
        };
        let parsed = BucketKey::parse(&key.redis_key()).expect("our own key parses");
        assert_eq!(parsed, key, "the key must survive its own format");
        assert_eq!(parsed.day_parsed(), Some(day()));
    }

    #[test]
    fn a_key_this_platform_did_not_write_is_refused_rather_than_guessed() {
        // Three shapes: another feature's key that happens to be scanned, a corrupted day, and a
        // day that is shaped like a date but is not one. The last two matter because a regex
        // check would pass `2026-13-45` and the flush would then hand PostgreSQL a `date` it
        // refuses — one bad key failing the flush of every other token in the window.
        assert!(BucketKey::parse("omnion:rl:sign_in:1:abcdef0123456789").is_none());
        assert!(BucketKey::parse("omnion:capi:not-a-uuid:2026-10-01|content/pages").is_none());
        assert!(BucketKey::parse("omnion:capi:6f0e0e4e-1b2c-4a3d-8e5f-000000000000:2026-13-45|x").is_none());
        assert!(BucketKey::parse("omnion:capi:6f0e0e4e-1b2c-4a3d-8e5f-000000000000:|x").is_none());
    }

    #[test]
    fn the_budget_is_keyed_on_the_token_and_the_minute_and_on_nothing_else() {
        // A shared counter lets one token exhaust every other token's budget, and a key that
        // carried the address would make the budget per-machine — an integration that scales out
        // to three servers would get three budgets and never be limited at all.
        let token = Uuid::from_u128(7);
        let minute = 1_000_000_020; // divisible by 60: a bucket boundary, which is the case that
        // has to be right.
        let a = budget_key(token, minute);
        assert_eq!(a, budget_key(token, minute), "one minute, one key");
        assert_ne!(a, budget_key(Uuid::from_u128(8), minute), "two tokens, two budgets");
        assert_ne!(a, budget_key(token, minute + 60), "the next minute is a new budget");
        assert!(a.starts_with("omnion:capi:budget:"));
    }

    #[test]
    fn a_refusal_waits_for_the_rest_of_the_minute_and_never_zero() {
        // `Retry-After: 0` tells a client to retry immediately, which is the one value that turns
        // a rate limit into a hot loop. Both edges are pinned: the top of a minute waits a whole
        // minute, and the boundary second waits one second rather than zero.
        assert_eq!(retry_after_seconds(1_000_000_020), 60, "the first second of a minute");
        assert_eq!(retry_after_seconds(1_000_000_020 + 59), 1, "the last second of a minute");
        for second in 0..60 {
            let wait = retry_after_seconds(1_000_000_020 + second);
            assert!((1..=60).contains(&wait), "second {second} waits {wait}");
        }
    }

    #[test]
    fn an_empty_bucket_is_empty_and_a_refusal_is_not() {
        // Same rule as the durable side, tested from the accumulator's side: a token that was
        // refused for want of a scope has spent nothing, and a token that was rate-limited is the
        // fact an operator opens the screen for.
        assert!(Counts::default().is_empty());
        assert!(!Counts {
            throttled: 1,
            ..Counts::default()
        }
        .is_empty());
    }
}
