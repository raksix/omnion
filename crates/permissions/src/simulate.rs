//! Permission simulator: the explanation behind one decision (docs/07-IAM.md §18).
//!
//! The verdict is not computed here — it comes from [`evaluate::authorize_subject`], the same
//! resolution the route guard runs (roles first, then the ABAC policies of the organization).
//! This module only explains it: every binding of the subject is listed with its state, the
//! bindings that count are traced through their roles, every policy is listed with its verdict,
//! so the reader sees which entry decided the answer — and which deny, role or policy, won.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::bindings;
use crate::catalogue;
use crate::error::{PermissionsError, Result};
use crate::evaluate::{self, Decision, DenyReason, Grant, Via};
use crate::model::{ResourceContext, Subject};
use crate::policies;

/// One binding considered while answering a simulator query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationStep {
    /// The binding.
    pub binding_id: Uuid,
    /// The role it carries.
    pub role_id: Uuid,
    /// Role key.
    pub role_key: String,
    /// Role name.
    pub role_name: String,
    /// Role priority.
    pub role_priority: i32,
    /// Subject the binding belongs to (`user:<id>`, `group:<id>`, `service_account:<id>`).
    pub subject: String,
    /// Where the binding applies.
    pub scope: String,
    /// `active`, `expired`, `revoked` or `out_of_scope`.
    pub state: &'static str,
    /// Whether the binding counted towards the decision.
    pub counts: bool,
    /// `allow` or `deny` when the role speaks about the key.
    pub effect: Option<&'static str>,
    /// How the effect reached the subject (`explicit_allow`, `inherited_deny`, …).
    pub via: Option<&'static str>,
    /// The bound role the effect was inherited through, when it was.
    pub inherited_from: Option<String>,
}

/// The role or policy behind a decision, as the verdict card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedSource {
    /// Role id.
    pub role_id: Uuid,
    /// Role key (or `policy` when a policy decided).
    pub role_key: String,
    /// Role name.
    pub role_name: String,
    /// Role priority.
    pub role_priority: i32,
    /// How the permission reached the subject.
    pub via: &'static str,
    /// The policy behind the decision, when a policy made it.
    pub policy: Option<SimulatedPolicy>,
}

impl From<&Grant> for SimulatedSource {
    fn from(grant: &Grant) -> Self {
        Self {
            role_id: grant.role_id,
            role_key: grant.role_key.clone(),
            role_name: grant.role_name.clone(),
            role_priority: grant.role_priority,
            via: via_name(grant.via),
            policy: grant.policy.as_ref().map(SimulatedPolicy::from_stamp),
        }
    }
}

/// The policy behind a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedPolicy {
    /// Policy id.
    pub policy_id: Uuid,
    /// Its name.
    pub policy_name: String,
    /// The priority it won with.
    pub priority: i32,
}

impl SimulatedPolicy {
    fn from_stamp(stamp: &evaluate::PolicyStamp) -> Self {
        Self {
            policy_id: stamp.policy_id,
            policy_name: stamp.policy_name.clone(),
            priority: stamp.priority,
        }
    }
}

/// One policy of the organization, with its verdict for this question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedPolicyStep {
    /// Policy id.
    pub policy_id: Uuid,
    /// Its name.
    pub policy_name: String,
    /// `allow` or `deny`.
    pub effect: &'static str,
    /// Its priority.
    pub priority: i32,
    /// Whether it is enabled at all.
    pub enabled: bool,
    /// Whether its targets cover the permission.
    pub targeted: bool,
    /// Whether its conditions hold for the attributes in question.
    pub satisfied: bool,
    /// Whether it therefore decided (or is a candidate that lost on priority).
    pub applies: bool,
}

/// The answer to one simulator query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    /// `true` when the subject holds the permission.
    pub allowed: bool,
    /// `allowed`, `explicit_deny`, `policy_denied` or `missing_permission`.
    pub reason: &'static str,
    /// The role or policy that decided it: the winning allow, or the winning deny.
    pub source: Option<SimulatedSource>,
    /// Every binding considered, the decisive ones first.
    pub steps: Vec<SimulationStep>,
    /// Every policy of the organization with its verdict for this question.
    pub policies: Vec<SimulatedPolicyStep>,
    /// How many bindings were looked at.
    pub considered: usize,
    /// How many of them counted.
    pub counted: usize,
    /// One line the reader can quote.
    pub note: String,
}

/// Explain whether `subject` holds `key` in `context`.
pub async fn simulate(
    pool: &PgPool,
    subject: Subject,
    key: &str,
    context: &ResourceContext,
) -> Result<SimulationReport> {
    if !catalogue::is_known(key) {
        return Err(PermissionsError::UnknownSimulatedAction(key.to_owned()));
    }

    // The verdict comes from the same resolution the guard runs: roles, then policies.
    let decision = evaluate::authorize_subject(pool, subject, context, key).await?;

    let graph = evaluate::load_role_graph(pool, context.organization_id).await?;
    let all = bindings::bindings_for(pool, subject).await?;
    let now = OffsetDateTime::now_utc();

    let mut steps = Vec::with_capacity(all.len());
    for binding in &all {
        let state = if binding.revoked_at.is_some() {
            "revoked"
        } else if binding.is_expired_at(now) {
            "expired"
        } else if !binding.scope.applies_to(context) {
            "out_of_scope"
        } else {
            "active"
        };
        let counts = state == "active";
        let trace = if counts {
            graph.trace(binding.role_id, key)
        } else {
            None
        };

        steps.push(SimulationStep {
            binding_id: binding.id,
            role_id: binding.role_id,
            role_key: trace.as_ref().map_or_else(
                || role_key_of(&graph, binding.role_id),
                |trace| trace.role_key.clone(),
            ),
            role_name: trace.as_ref().map_or_else(
                || role_name_of(&graph, binding.role_id),
                |trace| trace.role_name.clone(),
            ),
            role_priority: role_priority_of(&graph, binding.role_id),
            subject: binding.subject.describe(),
            scope: binding.scope.describe(),
            state,
            counts,
            effect: trace.as_ref().map(|trace| trace.effect.as_str()),
            via: trace.as_ref().map(|trace| via_name(trace.via)),
            inherited_from: trace
                .as_ref()
                .and_then(|trace| trace.inherited_from.as_ref().map(|(_, key)| key.clone())),
        });
    }

    // The decisive rows come first: denials, then allows, then the counted bindings that say
    // nothing about this key, then the rows that did not count at all.
    steps.sort_by_key(|step| {
        (
            !step.counts,
            step.effect != Some("deny"),
            step.effect.is_none(),
            std::cmp::Reverse(step.role_priority),
        )
    });

    let (reason, source) = match &decision {
        Decision::Allowed(grant) => ("allowed", Some(SimulatedSource::from(grant))),
        Decision::Denied {
            reason: DenyReason::PolicyDeny,
            source: Some(grant),
        } => ("policy_denied", Some(SimulatedSource::from(grant))),
        Decision::Denied {
            source: Some(grant),
            ..
        } => ("explicit_deny", Some(SimulatedSource::from(grant))),
        Decision::Denied { source: None, .. } => ("missing_permission", None),
    };

    // Every policy of the organization, with its verdict for this question.
    let mut evaluated: Vec<SimulatedPolicyStep> = Vec::new();
    if let Some(organization_id) = context.organization_id {
        let attributes = policies::attributes_for(pool, subject, context, key).await?;
        let candidates = policies::candidates(pool, organization_id, key, &attributes).await?;
        evaluated = candidates
            .into_iter()
            .map(|candidate| {
                let applies = candidate.applies();
                SimulatedPolicyStep {
                    policy_id: candidate.policy_id,
                    policy_name: candidate.policy_name,
                    effect: candidate.effect.as_str(),
                    priority: candidate.priority,
                    enabled: candidate.enabled,
                    targeted: candidate.targeted,
                    satisfied: candidate.satisfied,
                    applies,
                }
            })
            .collect();
    }

    let counted = steps.iter().filter(|step| step.counts).count();
    let mut note = format!(
        "{} binding(s) looked at, {} counted — revoked, expired and out-of-scope rows never decide.",
        steps.len(),
        counted
    );
    if !evaluated.is_empty() {
        let applying = evaluated.iter().filter(|policy| policy.applies).count();
        note.push_str(&format!(
            " {} policy(ies) evaluated, {} applies.",
            evaluated.len(),
            applying
        ));
    }

    Ok(SimulationReport {
        allowed: decision.is_allowed(),
        reason,
        source,
        steps,
        policies: evaluated,
        considered: all.len(),
        counted,
        note,
    })
}

/// The key of a role, when the graph knows it.
fn role_key_of(graph: &evaluate::RoleGraph, role_id: Uuid) -> String {
    graph
        .role(role_id)
        .map_or_else(|| role_id.to_string(), |role| role.role.key.clone())
}

/// The name of a role, when the graph knows it.
fn role_name_of(graph: &evaluate::RoleGraph, role_id: Uuid) -> String {
    graph
        .role(role_id)
        .map_or_else(|| role_id.to_string(), |role| role.role.name.clone())
}

/// The priority of a role, when the graph knows it.
fn role_priority_of(graph: &evaluate::RoleGraph, role_id: Uuid) -> i32 {
    graph.role(role_id).map_or(0, |role| role.role.priority)
}

/// Name of a provenance, as the API and the simulator report it.
#[must_use]
pub fn via_name(via: Via) -> &'static str {
    match via {
        Via::ExplicitAllow => "explicit_allow",
        Via::InheritedAllow => "inherited_allow",
        Via::ExplicitDeny => "explicit_deny",
        Via::InheritedDeny => "inherited_deny",
        Via::Policy => "policy",
    }
}
