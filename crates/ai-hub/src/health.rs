//! Health status computation (docs/requests/REQ-097, slice 3).
//!
//! A provider's health is **computed, never stored as a fact the operator can argue with**: the
//! probe writes one sample per tick, and the verdict follows from the samples. The rules are
//! deliberately few, because a health model an operator cannot predict is a health model they
//! stop trusting:
//!
//! - `down` after three consecutive failures — one failure is a blip, three in a row is an outage,
//!   and waiting for three keeps a single dropped packet from flipping a provider off.
//! - `degraded` when the endpoint answered but is slow for *it* (latency over 1.5× its own 7-day
//!   median) or when any failure is in the recent window. The median is the provider's own, not a
//!   global constant: a local llama.cpp server on a desktop and a cloud API are not comparable, and
//!   a fixed threshold would call one of them broken.
//! - `ok` after two consecutive successes.
//! - `unknown` when there is nothing to compute from — a provider the runner has never reached.
//!
//! The functions here are pure: they take samples and return a verdict, so the rule is unit-tested
//! without a database and the same function answers the Health tab, the providers list and the
//! failover walk. Nothing else is allowed to write `ai_providers.last_health`.

use time::OffsetDateTime;

/// A provider's rolling health, as the panel and the router read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthStatus {
    /// Never probed, or every sample is gone.
    Unknown,
    /// Answering, but slow for itself or carrying a recent failure.
    Degraded,
    /// Answering normally.
    Ok,
    /// Three consecutive failures, or the probe itself could not reach it.
    Down,
}

impl HealthStatus {
    /// The stored value: what `ai_providers.last_health` holds and what the API renders.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Degraded => "degraded",
            Self::Ok => "ok",
            Self::Down => "down",
        }
    }

    /// Parse a stored value. An unrecognised status is `unknown` rather than an error: a bad row
    /// must not take the providers list down with it.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "ok" => Self::Ok,
            "degraded" => Self::Degraded,
            "down" => Self::Down,
            _ => Self::Unknown,
        }
    }

    /// Whether a provider in this state may serve a request. `degraded` still serves: a slow
    /// answer is better than none, and the operator is the one who switches a provider off.
    #[must_use]
    pub fn is_servable(self) -> bool {
        matches!(self, Self::Ok | Self::Degraded)
    }
}

/// One probe sample, as [`health_status`] sees it.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Whether the endpoint answered (`ok`/`degraded`/`down` are the stored verdict values; the
    /// computation only asks whether the probe *succeeded*).
    pub ok: bool,
    /// How long the probe took, in milliseconds.
    pub latency_ms: i32,
    /// When it was taken.
    pub checked_at: OffsetDateTime,
}

impl Sample {
    /// A sample the endpoint answered.
    #[must_use]
    pub fn success(latency_ms: i32, checked_at: OffsetDateTime) -> Self {
        Self {
            ok: true,
            latency_ms,
            checked_at,
        }
    }

    /// A sample the endpoint did not answer.
    #[must_use]
    pub fn failure(checked_at: OffsetDateTime) -> Self {
        Self {
            ok: false,
            latency_ms: 0,
            checked_at,
        }
    }
}

/// Consecutive failures before a provider is called down.
pub const FAILURES_BEFORE_DOWN: usize = 3;
/// Consecutive successes before a provider is called healthy.
pub const SUCCESSES_BEFORE_OK: usize = 2;
/// The window in which any failure keeps a provider degraded.
pub const DEGRADED_WINDOW_HOURS: i64 = 24;
/// How far back the latency baseline is measured.
pub const BASELINE_DAYS: i64 = 7;
/// A latency this far above the provider's own median is a symptom, not weather.
pub const SLOW_FACTOR: f64 = 1.5;
/// How many samples the latency baseline needs before it is allowed to judge anything.
///
/// A median of one sample is that sample. A local endpoint answers in a millisecond, so a
/// single-sample baseline makes ordinary jitter look like a regression and the verdict flaps.
/// Below this many samples the baseline is `None` and the consecutive-run rules decide alone.
pub const MIN_BASELINE_SAMPLES: i32 = 5;

/// The status of one provider from its samples, newest first.
///
/// `baseline` is the provider's own median latency over the last [`BASELINE_DAYS`] days, read by
/// the caller. Passing `None` (no history yet, which is the case on a fresh install) means only
/// the consecutive-run rules apply — a first probe cannot be slow for a median that does not exist
/// yet, and treating the absence as a fault would mark every new provider degraded.
#[must_use]
pub fn health_status(samples: &[Sample], baseline_latency_ms: Option<i32>) -> HealthStatus {
    if samples.is_empty() {
        return HealthStatus::Unknown;
    }

    // The samples arrive newest first; the consecutive runs are read from that end.
    if consecutive_failures(samples) >= FAILURES_BEFORE_DOWN {
        return HealthStatus::Down;
    }

    if consecutive_successes(samples) >= SUCCESSES_BEFORE_OK
        && !slow_against(samples[0], baseline_latency_ms)
        && !recent_failure(samples, samples[0].checked_at)
    {
        return HealthStatus::Ok;
    }

    HealthStatus::Degraded
}

/// How many failures sit at the head of the newest-first sample list.
#[must_use]
pub fn consecutive_failures(samples: &[Sample]) -> usize {
    samples.iter().take_while(|s| !s.ok).count()
}

/// How many successes sit at the head of the newest-first sample list.
#[must_use]
pub fn consecutive_successes(samples: &[Sample]) -> usize {
    samples.iter().take_while(|s| s.ok).count()
}

/// Whether the newest sample is slow for this provider, judged against its own median.
#[must_use]
pub fn slow_against(sample: Sample, baseline_latency_ms: Option<i32>) -> bool {
    match baseline_latency_ms {
        Some(baseline) if baseline > 0 => sample.latency_ms as f64 > baseline as f64 * SLOW_FACTOR,
        // No baseline yet: nothing to be slow against.
        _ => false,
    }
}

/// Whether any failure falls inside the degraded window ending at `now`.
#[must_use]
pub fn recent_failure(samples: &[Sample], now: OffsetDateTime) -> bool {
    samples.iter().any(|sample| {
        !sample.ok
            && (now - sample.checked_at).whole_hours() < DEGRADED_WINDOW_HOURS
            && (now - sample.checked_at).whole_hours() >= 0
    })
}

/// The uptime of a provider over a window: successful samples over all samples, as a percentage.
///
/// A window with no samples is `None` rather than 100%: "uptime" for a provider that was never
/// probed is not a fact, and reporting 100% would put a green number on a Health tab that has
/// nothing behind it.
#[must_use]
pub fn uptime_percent(samples: &[Sample]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let ok = samples.iter().filter(|s| s.ok).count();
    Some((ok as f64 / samples.len() as f64) * 100.0)
}

/// The 95th percentile latency of a window, in milliseconds.
///
/// Computed with the nearest-rank method over a copy: the sample list is not sorted in place,
/// because the caller reuses it for the sparkline afterwards.
#[must_use]
pub fn p95_latency_ms(samples: &[Sample]) -> Option<i32> {
    if samples.is_empty() {
        return None;
    }
    let mut latencies: Vec<i32> = samples.iter().map(|s| s.latency_ms).collect();
    latencies.sort_unstable();
    let rank = (((latencies.len() as f64) * 0.95).ceil() as usize).max(1) - 1;
    latencies.get(rank).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    /// A newest-first list from `(ok, minutes_ago)` pairs.
    fn samples(pairs: &[(bool, i64)]) -> Vec<Sample> {
        let at = now();
        pairs
            .iter()
            .map(|(ok, minutes)| {
                let when = at - Duration::minutes(*minutes);
                if *ok {
                    Sample::success(100, when)
                } else {
                    Sample::failure(when)
                }
            })
            .collect()
    }

    #[test]
    fn the_stored_values_are_the_documented_four() {
        for status in [
            HealthStatus::Unknown,
            HealthStatus::Degraded,
            HealthStatus::Ok,
            HealthStatus::Down,
        ] {
            assert_eq!(HealthStatus::parse(status.as_str()), status);
        }
        assert_eq!(HealthStatus::parse("nonsense"), HealthStatus::Unknown);
    }

    #[test]
    fn a_provider_with_no_samples_is_unknown() {
        assert_eq!(health_status(&[], None), HealthStatus::Unknown);
    }

    #[test]
    fn one_failure_is_degraded_not_down() {
        let list = samples(&[(false, 1), (true, 2), (true, 3)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Degraded);
    }

    #[test]
    fn three_consecutive_failures_are_down() {
        let list = samples(&[(false, 1), (false, 2), (false, 3), (true, 4)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Down);
    }

    #[test]
    fn an_old_failure_does_not_hold_a_recovered_provider_degraded() {
        // A failure two days back, then fresh successes: the provider is back, and the run rules
        // (not the 24h window) decide it. This is the other half of the precedence above — the
        // window has to end, or a provider could never recover.
        let list = samples(&[(true, 1), (true, 2), (true, 3), (false, 2_880)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Ok);
    }

    #[test]
    fn a_failure_inside_the_window_keeps_a_provider_degraded() {
        let list = samples(&[(true, 1), (true, 2), (true, 3), (false, 600)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Degraded);
    }

    #[test]
    fn recovery_waits_for_the_degraded_window_to_clear() {
        // Two clean samples, but the last failure is 30 minutes old: the 24h window holds it at
        // degraded. This is the precedence the two rules need — a failure is visible before the
        // recovery is, so a flapping provider is never shown as healthy between the two.
        let list = samples(&[(true, 1), (true, 2), (false, 30)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Degraded);
    }

    #[test]
    fn a_single_success_after_a_long_outage_is_not_yet_ok() {
        // The run rule earns its keep here: one clean sample after three failures is not
        // evidence, and the provider stays degraded until the second one lands.
        let list = samples(&[(true, 1), (false, 10), (false, 20), (false, 30)]);
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Degraded);
    }

    #[test]
    fn a_slow_answer_is_degraded_even_after_two_successes() {
        let at = now();
        let list = vec![
            Sample::success(500, at - Duration::minutes(1)),
            Sample::success(480, at - Duration::minutes(2)),
        ];
        // The provider's own median is 100 ms, so 500 is five times it.
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Degraded);
    }

    #[test]
    fn a_fast_answer_is_ok_against_the_same_baseline() {
        let at = now();
        let list = vec![
            Sample::success(105, at - Duration::minutes(1)),
            Sample::success(102, at - Duration::minutes(2)),
        ];
        assert_eq!(health_status(&list, Some(100)), HealthStatus::Ok);
    }

    #[test]
    fn without_a_baseline_a_two_second_answer_is_not_a_regression() {
        // The loopback case, and the reason `MIN_BASELINE_SAMPLES` exists. A local endpoint
        // answers in ~1 ms; against a baseline of that one sample the next probe at 2 ms is
        // "half again slower" and the provider degrades. With no baseline the run rules decide
        // alone, so two clean samples read as healthy — which is the truth.
        let at = now();
        let list = vec![
            Sample::success(2, at - Duration::minutes(1)),
            Sample::success(1, at - Duration::minutes(2)),
        ];
        assert_eq!(health_status(&list, None), HealthStatus::Ok);
    }

    #[test]
    fn no_baseline_means_the_first_probe_is_not_slow() {
        let at = now();
        let list = vec![Sample::success(9_000, at), Sample::success(9_000, at)];
        assert_eq!(health_status(&list, None), HealthStatus::Ok);
    }

    #[test]
    fn a_degraded_provider_still_serves() {
        assert!(HealthStatus::Degraded.is_servable());
        assert!(HealthStatus::Ok.is_servable());
        assert!(!HealthStatus::Down.is_servable());
        assert!(!HealthStatus::Unknown.is_servable());
    }

    #[test]
    fn uptime_is_a_percentage_of_samples_not_of_time() {
        let list = samples(&[(true, 1), (false, 2), (true, 3), (false, 4)]);
        let uptime = uptime_percent(&list).expect("samples");
        assert!((uptime - 50.0).abs() < 0.01, "got {uptime}");
    }

    #[test]
    fn an_unprobed_provider_has_no_uptime_number() {
        assert!(uptime_percent(&[]).is_none());
    }

    #[test]
    fn p95_ignores_the_single_slowest_sample() {
        let at = now();
        let list: Vec<Sample> = (0..100)
            .map(|i| Sample::success(10 + i, at - Duration::minutes(i64::from(100 - i))))
            .collect();
        // 100 samples of 10..109: the nearest-rank 95th is the 95th smallest = 104.
        assert_eq!(p95_latency_ms(&list), Some(104));
    }

    #[test]
    fn p95_of_one_sample_is_that_sample() {
        let at = now();
        assert_eq!(p95_latency_ms(&[Sample::success(42, at)]), Some(42));
        assert!(p95_latency_ms(&[]).is_none());
    }
}
