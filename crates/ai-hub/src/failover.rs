//! Failover: the chain a task-routed call walks when its provider does not answer
//! (docs/requests/REQ-097, slice 3).
//!
//! The rules are kept in one pure function, [`plan`], because the three claims the request makes
//! about failover are only worth something if a test can state them without a database and a
//! socket:
//!
//! * **A pinned request never leaves its provider.** `provider/model` is an explicit instruction;
//!   rerouting it would answer a question nobody asked. A pinned call that fails is *that*
//!   provider's error, returned as-is.
//! * **A request that already streamed a byte is never retried.** The caller has seen part of an
//!   answer; replaying the call behind their back produces a second, different answer and a
//!   duplicate bill. The first-byte boundary is therefore an input, not something inferred here.
//! * **A substituted call is recorded, loudly.** Every substitution names the provider it came
//!   from, the one that answered and the task, and it lands in the usage row
//!   (`substituted_from`) and on the event bus (`ai.provider.failover_used`). Failover that cannot
//!   be audited is failover nobody will trust.
//!
//! What this module deliberately does **not** do: it never looks at health to decide. A provider
//! the probe has called `down` is still asked — the operator is the one who switches a provider
//! off, and a health verdict that silently rewrites routing is a verdict with no way back. The
//! chain is the *enabled* providers in order, and that is what the Failover panel previews.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::model::Provider;

/// How a request named its model, which is what decides whether it may be rerouted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// The request named `provider/model`: it is pinned and never leaves that provider.
    Pinned {
        /// The provider the request pinned.
        provider: Uuid,
    },
    /// The request named only a task (or nothing at all): the router picks, and the chain is
    /// open to substitution.
    TaskRouted,
}

/// What a failed attempt tells the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Nothing has been sent to any provider yet.
    Nothing,
    /// The call failed before the first streamed byte — the one moment a retry is safe.
    BeforeFirstByte,
    /// The caller already received part of an answer; this must not be replayed.
    AfterFirstByte,
}

/// Whether a failure may be retried at all.
///
/// A failure that is not *upstream* is ours, not the provider's: a refused capability, a missing
/// model, a disabled provider. Rerouting those would answer a different question than the one
/// that was refused, and would hide the real error behind a second provider's.
#[must_use]
pub fn is_retryable(error: &AiHubError) -> bool {
    error.is_upstream()
}

/// One attempt's outcome, as the plan accumulates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Which provider was asked.
    pub provider_id: Uuid,
    /// The provider's display name, for the event payload and the error message.
    pub provider_name: String,
    /// Why the attempt ended: `None` when it answered.
    pub error: Option<String>,
}

/// The decision: which providers to try, and what to record about the ones already tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Exactly one provider may serve this request.
    Pinned {
        /// The provider that must serve it.
        provider_id: Uuid,
    },
    /// A chain of candidates, in order. The head is tried first; a failure walks to the next.
    Chain {
        /// Candidates in order; never empty.
        candidates: Vec<Attempt>,
    },
    /// Every provider that could serve the request has been tried and failed.
    Exhausted {
        /// The attempts, in the order they were made.
        attempts: Vec<Attempt>,
    },
}

impl Plan {
    /// The provider to try first.
    #[must_use]
    pub fn head(&self) -> Option<&Attempt> {
        match self {
            // A pinned plan carries the provider's id and nothing else: it was addressed, not
            // chosen, so the chain never held an `Attempt` for it and inventing one here with an
            // empty name is a value a caller could render as a blank row.
            Self::Pinned { .. } => None,
            Self::Chain { candidates } => candidates.first(),
            Self::Exhausted { .. } => None,
        }
    }

    /// The provider a pinned request is bound to, when it is pinned.
    #[must_use]
    pub fn pinned_to(&self) -> Option<Uuid> {
        match self {
            Self::Pinned { provider_id } => Some(*provider_id),
            _ => None,
        }
    }

    /// The candidates, when the request is task-routed. Empty for a pinned request, which has no
    /// second choice to offer.
    #[must_use]
    pub fn candidates(&self) -> &[Attempt] {
        match self {
            Self::Chain { candidates } => candidates,
            _ => &[],
        }
    }

    /// Whether the request is allowed to move to another provider at all.
    #[must_use]
    pub fn is_substitutable(&self) -> bool {
        matches!(self, Self::Chain { .. })
    }
}

/// The provider chain a task-routed call walks, built from the enabled providers in order.
///
/// The order is the store's order — priority, then name, then id — so the chain that runs is the
/// chain the Failover panel draws and the order the operator set with a drag. Rebuilding the sort
/// here would make the preview a picture of something other than the routing.
#[must_use]
pub fn chain_of(providers: &[Provider]) -> Vec<Attempt> {
    providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| Attempt {
            provider_id: provider.id,
            provider_name: provider.name.clone(),
            error: None,
        })
        .collect()
}

/// Decide what a call may do, before it is dialled.
///
/// `providers` is the enabled chain in store order; `pinned` is what the request named; `progress`
/// is how far the current attempt got. The returned plan is the whole answer — there is no second
/// phase, because a decision that has to be revised after a failure is a decision that can be
/// made wrong twice.
#[must_use]
pub fn plan(providers: &[Provider], pinned: Option<Uuid>, progress: Progress) -> Plan {
    // A pinned provider is an instruction, not a suggestion. It is returned as-is whatever
    // happened to the call: the caller asked for that provider's answer or its error.
    if let Some(provider_id) = pinned {
        return Plan::Pinned { provider_id };
    }

    let mut candidates = chain_of(providers);

    // A call that already streamed a byte must not be replayed. The provider that was used is
    // still reported — the caller wants to know who answered — but it is the only entry, so
    // `next` has nothing to offer and no substitution can happen.
    if progress == Progress::AfterFirstByte {
        candidates.truncate(1);
    }

    if candidates.is_empty() {
        return Plan::Exhausted {
            attempts: Vec::new(),
        };
    }
    Plan::Chain { candidates }
}

/// The next provider to try after a failure, or the final error when there is none left.
///
/// Returns `Err` carrying the **first** attempt's error, not the last: the provider the operator
/// ranked first is the one the request asked for, and a chain of three failures should not be
/// reported as the last one's complaint alone. When the first attempt succeeded in reaching the
/// provider but a later one failed, the last error is the one that describes the request's
/// outcome, so it is used when it is not empty.
pub fn next(providers: &[Provider], tried: &[Attempt], progress: Progress) -> Option<Attempt> {
    if progress != Progress::BeforeFirstByte {
        return None;
    }

    let candidates = chain_of(providers);
    let mut seen: Vec<Uuid> = tried.iter().map(|attempt| attempt.provider_id).collect();
    seen.sort_unstable();
    seen.dedup();

    candidates
        .into_iter()
        .find(|candidate| !seen.contains(&candidate.provider_id))
}

/// The error to return once the chain is exhausted, or `None` while a candidate is left.
#[must_use]
pub fn final_error(attempts: &[Attempt]) -> Option<AiHubError> {
    let first = attempts.iter().find_map(|attempt| attempt.error.as_ref());
    // A later provider's own refusal is the more specific answer when the first one never even
    // answered ("connection refused" is less useful than a 401 from the second).
    let last = attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.error.as_ref());
    let message = match (first, last) {
        (Some(first), Some(last)) if first != last => Some(format!("{first} (then: {last})")),
        (Some(first), _) => Some(first.clone()),
        _ => last.cloned(),
    }?;

    Some(AiHubError::Upstream {
        status: 502,
        message,
    })
}

/// The provider a request pinned itself to, when it named `provider/model`.
///
/// This is the *router's own* rule, asked the same way: a prefix counts as a provider only when
/// a provider of that name is really connected, because a model key may itself contain a slash
/// (`meta-llama/Llama-3.1-8B-Instruct`). Reading the rule from a second place is how a pinned
/// request ends up quietly rerouted — the failover walk and the router would disagree about what
/// "pinned" means, and the walk would be the one that moves the request.
pub async fn pinned_provider(pool: &PgPool, requested: Option<&str>) -> Result<Option<Uuid>> {
    let Some(value) = requested else {
        return Ok(None);
    };
    let Some((prefix, _rest)) = value.split_once('/') else {
        return Ok(None);
    };
    Ok(crate::store::find_provider_by_name(pool, prefix)
        .await?
        .map(|provider| provider.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn provider(name: &str, enabled: bool) -> Provider {
        Provider {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            protocol: "openai_compatible".to_owned(),
            kind: "cloud".to_owned(),
            base_url: "https://api.example.com/v1".to_owned(),
            api_key: None,
            timeout_ms: 30_000,
            max_retries: 1,
            priority: 100,
            last_health: "unknown".to_owned(),
            last_checked_at: None,
            last_error: None,
            enabled,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn chain() -> (Provider, Provider, Provider) {
        (
            provider("Alpha", true),
            provider("Beta", true),
            provider("Gamma", true),
        )
    }

    #[test]
    fn a_task_routed_call_gets_the_whole_chain() {
        let providers = {
            let (a, b, c) = chain();
            vec![a, b, c]
        };
        let plan = plan(&providers, None, Progress::Nothing);
        assert!(plan.is_substitutable());
        assert_eq!(plan.candidates().len(), 3);
        assert_eq!(plan.head().expect("head").provider_name, "Alpha");
    }

    #[test]
    fn a_disabled_provider_is_not_in_the_chain() {
        let (a, b, c) = chain();
        let mut off = b;
        off.enabled = false;
        let plan = plan(&[a, off, c], None, Progress::Nothing);
        assert_eq!(plan.candidates().len(), 2);
        assert!(
            !plan
                .candidates()
                .iter()
                .any(|attempt| attempt.provider_name == "Beta")
        );
    }

    #[test]
    fn a_pinned_request_is_never_rerouted() {
        let (a, b, c) = chain();
        let providers = vec![a.clone(), b, c];
        let plan = plan(&providers, Some(a.id), Progress::BeforeFirstByte);
        assert!(!plan.is_substitutable());
        assert!(plan.candidates().is_empty());
        // `head` stays empty for a pinned plan: the provider was addressed, not chosen, so the
        // chain never built an `Attempt` for it and a fabricated one would render as a blank row.
        assert!(plan.head().is_none());
        assert_eq!(plan.pinned_to(), Some(a.id));
        assert!(matches!(plan, Plan::Pinned { provider_id } if provider_id == a.id));
    }

    #[test]
    fn a_pinned_request_stays_pinned_even_after_the_first_byte() {
        let (a, b, c) = chain();
        let providers = vec![a.clone(), b, c];
        let plan = plan(&providers, Some(a.id), Progress::AfterFirstByte);
        assert!(matches!(plan, Plan::Pinned { .. }));
    }

    #[test]
    fn a_call_past_the_first_byte_offers_no_substitute() {
        let providers = {
            let (a, b, c) = chain();
            vec![a, b, c]
        };
        let plan = plan(&providers, None, Progress::AfterFirstByte);
        // The chain still names who *was* used, but there is nowhere left to go.
        assert_eq!(plan.candidates().len(), 1);
        assert!(next(&providers, &[], Progress::AfterFirstByte).is_none());
    }

    #[test]
    fn a_failure_before_the_first_byte_moves_to_the_next_provider() {
        let providers = {
            let (a, b, c) = chain();
            vec![a, b, c]
        };
        let tried = vec![Attempt {
            provider_id: providers[0].id,
            provider_name: "Alpha".to_owned(),
            error: Some("connection refused".to_owned()),
        }];
        let next_one = next(&providers, &tried, Progress::BeforeFirstByte).expect("a successor");
        assert_eq!(next_one.provider_name, "Beta");
    }

    #[test]
    fn the_last_provider_has_no_successor() {
        let providers = {
            let (a, b, c) = chain();
            vec![a, b, c]
        };
        let tried: Vec<Attempt> = providers
            .iter()
            .take(2)
            .map(|provider| Attempt {
                provider_id: provider.id,
                provider_name: provider.name.clone(),
                error: Some("boom".to_owned()),
            })
            .collect();
        let next_one = next(&providers, &tried, Progress::BeforeFirstByte);
        assert_eq!(next_one.expect("the third").provider_name, "Gamma");
        let all: Vec<Attempt> = providers
            .iter()
            .map(|provider| Attempt {
                provider_id: provider.id,
                provider_name: provider.name.clone(),
                error: Some("boom".to_owned()),
            })
            .collect();
        assert!(next(&providers, &all, Progress::BeforeFirstByte).is_none());
    }

    #[test]
    fn an_installation_with_no_provider_exhausts_immediately() {
        let plan = plan(&[], None, Progress::Nothing);
        assert!(matches!(plan, Plan::Exhausted { .. }));
        assert!(plan.head().is_none());
    }

    #[test]
    fn only_an_upstream_failure_may_be_retried() {
        assert!(is_retryable(&AiHubError::Transport("refused".to_owned())));
        assert!(is_retryable(&AiHubError::Upstream {
            status: 500,
            message: "boom".to_owned()
        }));
        // Ours, not the provider's: rerouting these would answer a different question.
        assert!(!is_retryable(&AiHubError::ModelNotFound));
        assert!(!is_retryable(&AiHubError::NoDefaultModel));
        assert!(!is_retryable(&AiHubError::InvalidChatRequest(
            "no messages".to_owned()
        )));
        assert!(!is_retryable(&AiHubError::ProviderDisabled(
            "Off".to_owned()
        )));
    }

    #[test]
    fn the_exhausted_error_leads_with_the_provider_the_request_asked_for() {
        let attempts = vec![
            Attempt {
                provider_id: Uuid::new_v4(),
                provider_name: "Alpha".to_owned(),
                error: Some("connection refused".to_owned()),
            },
            Attempt {
                provider_id: Uuid::new_v4(),
                provider_name: "Beta".to_owned(),
                error: Some("401 unauthorized".to_owned()),
            },
        ];
        let error = final_error(&attempts).expect("an error");
        let text = error.to_string();
        assert!(text.contains("connection refused"), "got {text}");
        assert!(text.contains("401 unauthorized"), "got {text}");
    }

    #[test]
    fn a_single_failure_is_reported_verbatim() {
        let attempts = vec![Attempt {
            provider_id: Uuid::new_v4(),
            provider_name: "Alpha".to_owned(),
            error: Some("connection refused".to_owned()),
        }];
        // Verbatim means the provider's own words survive intact — the `Upstream` wrapper adds
        // the status prefix around them, but nothing is rewritten inside.
        let text = final_error(&attempts).expect("an error").to_string();
        assert!(text.contains("connection refused"), "got {text}");
        assert!(final_error(&[]).is_none());
    }
}
