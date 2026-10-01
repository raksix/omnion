//! Egress verification — proving the air gap still refuses (REQ-106, slice 4).
//!
//! # What a verification IS
//!
//! The request asks for "a check that attempts a documented non-local call and expects the
//! refusal; a refusal is a pass, a success is a loud failure". That inverts the usual meaning of
//! a probe, and the inversion is the whole point: this test **expects a network call to fail**.
//! The day one of these attempts *succeeds*, a request left the installation while the operator
//! believed nothing could — which is a compliance failure, not a green check.
//!
//! So the outcomes are named to refuse that misreading:
//!
//! | Outcome | Meaning |
//! |---|---|
//! | [`EgressOutcome::Blocked`] | The call was refused by the air gap. **This is the pass.** |
//! | [`EgressOutcome::Escaped`] | The call was permitted. A loud failure; it also flips the banner red. |
//! | [`EgressOutcome::Undetermined`] | The attempt never reached the check (bad URL, gap off). Not a pass. |
//!
//! `Undetermined` exists because folding "could not run" into `Blocked` would let a broken
//! configuration paint a false assurance — the request calls a failed verification "the loudest
//! alert in this request", and a check that cannot run is at least as loud.
//!
//! # The attempt is made through the platform's OWN client, not curl
//!
//! [`crate::local_host::local_http`] refuses redirects, and [`super::airgap_store::check_call`]
//! is the one function that decides. The verification routes the attempt through the same
//! decision the chat path takes, so a pass here is evidence about *that* path and not about a
//! parallel test harness. A verification that used a second code path could pass while the
//! chat path had drifted.
//!
//! # Nothing is sent to the internet, ever
//!
//! The probe asks for a URL the operator supplies. It resolves nothing, opens no socket, and
//! performs **no** HTTP request when the air-gap check has already refused it — which, with the
//! gap on, is the whole point. If the check *permits* the call, the attempt then proceeds to the
//! real endpoint and the outcome is `Escaped`: we genuinely tried, and the installation failed to
//! stop us. That is what makes the failure real rather than simulated.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::airgap_store::{self, Refusal};
use crate::error::{AiHubError, Result};
use crate::local_host::host_of;

/// The verdict of one verification attempt.
///
/// Serialized in **snake_case** and never renamed: the stored row and the API's `result` column
/// share these strings, and the settings screen branches on them. A rename here is a data
/// migration, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressOutcome {
    /// The air gap refused the call. The expected result — a pass.
    Blocked,
    /// The call was permitted and left the installation. The loudest failure in this request.
    Escaped,
    /// The attempt could not be made at all, so it proves nothing either way.
    Undetermined,
}

impl EgressOutcome {
    /// Whether this outcome means the installation is holding.
    ///
    /// Only [`EgressOutcome::Blocked`] does. `Escaped` and `Undetermined` both fail, because a
    /// check that could not run has not demonstrated anything and must never be recorded as
    /// reassuring.
    pub fn is_holding(self) -> bool {
        matches!(self, EgressOutcome::Blocked)
    }

    /// The word the stored row carries and the UI branches on.
    pub fn as_str(self) -> &'static str {
        match self {
            EgressOutcome::Blocked => "blocked",
            EgressOutcome::Escaped => "escaped",
            EgressOutcome::Undetermined => "undetermined",
        }
    }

    /// Read a stored word back into an outcome, defaulting to `Undetermined`.
    ///
    /// An unrecognized value is a failure, never a pass: a row written by a future version must
    /// not be read by an older binary as "it was fine".
    pub fn from_str_lossy(word: &str) -> Self {
        match word {
            "blocked" => EgressOutcome::Blocked,
            "escaped" => EgressOutcome::Escaped,
            _ => EgressOutcome::Undetermined,
        }
    }

    /// The one-line sentence the screen shows.
    pub fn message(self, host: &str) -> String {
        match self {
            EgressOutcome::Blocked => format!(
                "the call to {host} was refused by the air gap, as it must be"
            ),
            EgressOutcome::Escaped => format!(
                "the call to {host} was ALLOWED through — a request left the installation while \
                 the air gap was on"
            ),
            EgressOutcome::Undetermined => format!(
                "the attempt against {host} could not be made, so it proves nothing either way"
            ),
        }
    }
}

/// Elapsed whole milliseconds between `started` and now, saturating at `0`.
///
/// `time`'s `whole_milliseconds` is an `i128` because a duration between two `OffsetDateTime`s
/// can exceed `i64` in principle. It cannot here — two `now_utc()` calls are microseconds apart —
/// so the narrowing is a documented clamp rather than a silent `as i64` cast, which would be a
/// wrapping conversion on an input nobody thought about.
fn elapsed_ms(started: OffsetDateTime) -> i64 {
    let millis = (OffsetDateTime::now_utc() - started).whole_milliseconds();
    i64::try_from(millis).unwrap_or(i64::MAX).max(0)
}

/// What one verification attempt produced.
#[derive(Debug, Clone)]
pub struct EgressResult {
    pub outcome: EgressOutcome,
    /// The host that was aimed at, as a bare host name.
    pub target: String,
    /// The refusal, when there was one — carries the provider and host the check reported.
    pub refusal: Option<Refusal>,
    /// How long the attempt took, when it got far enough to measure.
    pub latency_ms: Option<i64>,
    /// When the attempt finished.
    pub verified_at: OffsetDateTime,
}

impl EgressResult {
    /// Whether this attempt proves the installation is holding.
    pub fn holds(&self) -> bool {
        self.outcome.is_holding()
    }
}

/// Verify that the air gap still refuses a non-local call to `base_url`.
///
/// `provider_name` is the name the refusal should carry — it is what the operator reads in the
/// audit trail, and it must be a real registered provider's name so the check exercises the same
/// path a chat would.
///
/// # Ordering
///
/// The gap being **off** short-circuits before anything else. Verifying a switch that is not on
/// has no meaning: every call is permitted by definition, so the attempt would "escape" and
/// record a loud failure for a correct configuration. The caller gets `Undetermined` with a
/// sentence that says so instead.
pub async fn verify_egress(
    pool: &PgPool,
    provider_name: &str,
    base_url: &str,
) -> Result<EgressResult> {
    let started = OffsetDateTime::now_utc();
    let host = host_of(base_url).unwrap_or_default();

    if !airgap_store::is_enabled(pool).await? {
        return Ok(EgressResult {
            outcome: EgressOutcome::Undetermined,
            target: host,
            refusal: None,
            latency_ms: Some(0),
            verified_at: started,
        });
    }

    // A URL with no host cannot be aimed at anything, and calling `check_call` with it would
    // produce a refusal whose host is empty — a "pass" that verified nothing. Rejecting here
    // keeps `Blocked` meaning "a real host was refused".
    if host.is_empty() {
        return Ok(EgressResult {
            outcome: EgressOutcome::Undetermined,
            target: host,
            refusal: None,
            latency_ms: Some(0),
            verified_at: started,
        });
    }

    // The platform's own decision, in the same order the chat path uses it.
    let decision = airgap_store::check_call(pool, provider_name, base_url).await?;

    match decision {
        Some(refusal) => Ok(EgressResult {
            outcome: EgressOutcome::Blocked,
            target: host,
            refusal: Some(refusal),
            latency_ms: Some(elapsed_ms(started)),
            verified_at: OffsetDateTime::now_utc(),
        }),
        None => {
            // The check said yes. We must actually try, or "escaped" would be a prediction rather
            // than a measurement — and the whole request is about not taking predictions for
            // facts. The attempt is made and its result recorded; the outcome is the same either
            // way, because a permitted call that then times out at the network has still escaped
            // the switch.
            let latency = attempt_call(base_url).await;
            Ok(EgressResult {
                outcome: EgressOutcome::Escaped,
                target: host,
                refusal: None,
                latency_ms: latency.or_else(|| Some(elapsed_ms(started))),
                verified_at: OffsetDateTime::now_utc(),
            })
        }
    }
}

/// Really try the call the switch permitted.
///
/// Returns `Some(latency)` when an HTTP response came back and `None` when the network itself
/// refused. Both mean the same thing for this feature — the switch let it go — but the caller
/// records the measurement when there is one.
async fn attempt_call(base_url: &str) -> Option<i64> {
    let started = OffsetDateTime::now_utc();
    let client = crate::local_host::local_http();
    // A cheap, side-effect-free path: the models listing a local server would answer. Any status
    // is enough — the point is whether the request left, not what came back.
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client.get(&url).send().await.ok()?;
    let _ = response.status();
    Some(elapsed_ms(started))
}

/// Record a verification on the single air-gap row.
///
/// Writes the outcome, the target host and the time in one statement, so a reader can never see
/// a time from one attempt beside a target from another.
pub async fn record_result(
    pool: &PgPool,
    outcome: EgressOutcome,
    target: &str,
    verified_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "update ai_airgap_state set egress_verify_result = $1, egress_verify_target = $2, \
         egress_verified_at = $3, updated_at = now() where id = $4",
    )
    .bind(outcome.as_str())
    .bind(target)
    .bind(verified_at)
    .bind(airgap_store::AIRGAP_ROW_ID)
    .execute(pool)
    .await?;

    Ok(())
}

/// Reject a verify request whose provider does not exist, naming what is available.
///
/// An operator who types a provider that was deleted should be told which ones are real rather
/// than watching an empty verification fail for a reason they cannot see.
pub async fn require_provider(pool: &PgPool, provider_name: &str) -> Result<()> {
    let exists: bool = sqlx::query_scalar(
        "select exists(select 1 from ai_providers where name = $1)",
    )
    .bind(provider_name)
    .fetch_one(pool)
    .await?;

    if exists {
        return Ok(());
    }

    let known = sqlx::query_scalar::<_, String>(
        "select name from ai_providers order by name limit 5",
    )
    .fetch_all(pool)
    .await?;

    let hint = if known.is_empty() {
        "no AI provider is registered yet".to_owned()
    } else {
        format!("registered providers: {}", known.join(", "))
    };

    Err(AiHubError::InvalidProvider(format!(
        "there is no provider named `{provider_name}` to verify against — {hint}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_holds() {
        assert!(EgressOutcome::Blocked.is_holding());
        // A call that left is a failure even though "the attempt ran".
        assert!(!EgressOutcome::Escaped.is_holding());
        // A check that could not run has proved nothing, so it must never read as reassuring.
        assert!(!EgressOutcome::Undetermined.is_holding());
    }

    #[test]
    fn the_stored_word_round_trips() {
        for outcome in [
            EgressOutcome::Blocked,
            EgressOutcome::Escaped,
            EgressOutcome::Undetermined,
        ] {
            assert_eq!(EgressOutcome::from_str_lossy(outcome.as_str()), outcome);
        }
    }

    #[test]
    fn an_unknown_stored_word_is_never_a_pass() {
        // A row written by a newer version must not be read by an older binary as "it was fine".
        assert_eq!(
            EgressOutcome::from_str_lossy("something_new"),
            EgressOutcome::Undetermined
        );
        assert!(!EgressOutcome::from_str_lossy("").is_holding());
    }

    #[test]
    fn each_outcome_says_something_different() {
        let blocked = EgressOutcome::Blocked.message("api.openai.com");
        let escaped = EgressOutcome::Escaped.message("api.openai.com");
        let undetermined = EgressOutcome::Undetermined.message("api.openai.com");

        assert!(blocked.contains("refused"));
        assert!(escaped.contains("ALLOWED"));
        assert!(undetermined.contains("proves nothing"));
        // The host must appear in every sentence: an operator needs to know which host was tried.
        for message in [&blocked, &escaped, &undetermined] {
            assert!(message.contains("api.openai.com"), "{message}");
        }
    }

    #[test]
    fn the_serialized_words_are_what_the_store_holds() {
        assert_eq!(
            serde_json::to_string(&EgressOutcome::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::to_string(&EgressOutcome::Escaped).unwrap(),
            "\"escaped\""
        );
    }
}
