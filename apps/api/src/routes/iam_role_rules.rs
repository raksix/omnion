//! `/api/v1/iam/providers/{id}/role-rules` — the wizard's fourth step (REQ-065, slice 3).
//!
//! Read the ordered rules, replace them atomically, and dry-run a sample identity. The same three
//! shapes as the attribute map, for the same reason: the editor needs a catalogue it cannot drift
//! from, "save" has to be all-or-nothing, and a preview that re-implemented the evaluator would be
//! a preview of nothing.
//!
//! The dry run answers with the *matched rule*, not only the role, because that is the question an
//! operator is actually asking. "Which of my six rules fired, and why did the one I expected not
//! fire?" cannot be answered by a role id. Every rule's read values are reported alongside its
//! match verdict, so a rule that did not fire is visibly firing-on-nothing rather than
//! mysteriously wrong — which is the difference between a five-second fix and an afternoon.
//!
//! A dry run evaluates the **stored** rules, not the ones in the request. A preview of unsaved
//! rules is a preview of a configuration that does not exist, and it invites the reading that the
//! button tests what is on screen. There is a separate way to try a rule set out before saving it,
//! and it is the same evaluator, so this file does not pretend to offer a second one.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_identity::sso::claims::Identity;
use omnion_identity::sso::group_context::GroupContext;
use omnion_identity::sso::providers;
use omnion_identity::sso::role_rule_store;
use omnion_identity::sso::role_rules::{Resolution, RoleRules, ScopeType, WhenKind, WhenOperator};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::iam::record;
use crate::state::AppState;

/// Largest sample payload a dry run accepts. The same ceiling as the mapping preview, for the same
/// reason: a pasted claims document is a few kilobytes, and a megabyte is a mistake.
const MAX_SAMPLE_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The rule set, plus what the editor needs to render itself.
#[derive(Debug, Serialize)]
pub struct RoleRulesBody {
    /// The provider the rules belong to.
    pub provider_id: Uuid,
    /// The rules, in search order.
    pub rules: Vec<RuleBody>,
    /// The five `when_kind` values a rule may use.
    pub when_kinds: Vec<WhenKindBody>,
    /// The four `when_operator` values a rule may use.
    pub when_operators: Vec<WhenOperatorBody>,
    /// The scopes a rule may grant in.
    pub scope_types: Vec<ScopeTypeBody>,
    /// The default role applied when nothing matches — read from the provider, so the editor can
    /// show it next to the "no rule matched" branch instead of leaving it implicit.
    pub default_role_id: Option<Uuid>,
    /// Problems with what is stored right now.
    pub problems: Vec<omnion_identity::sso::role_rules::RuleProblem>,
}

/// One rule, as the editor reads it.
#[derive(Debug, Serialize)]
pub struct RuleBody {
    /// Row id, stable across a save that reorders.
    pub id: Option<Uuid>,
    /// Editor order. The panel shows this as `#N`.
    pub position: i32,
    /// What the rule reads.
    pub when_kind: &'static str,
    /// Whether the rule must name a key.
    pub needs_key: bool,
    /// The claim path or field it reads.
    pub when_key: String,
    /// How it compares.
    pub when_operator: &'static str,
    /// What it compares against.
    pub when_value: String,
    /// The role granted.
    pub role_id: Uuid,
    /// Where the role is granted.
    pub scope_type: &'static str,
    /// The site, when the scope is `site`.
    pub site_id: Option<Uuid>,
    /// Whether a match ends the search.
    pub stop: bool,
    /// Whether the rule is considered.
    pub enabled: bool,
}

/// A picker option that knows whether it needs an argument.
#[derive(Debug, Serialize)]
pub struct WhenKindBody {
    /// Wire name.
    pub name: &'static str,
    /// Whether the rule has to name what it reads.
    pub needs_key: bool,
    /// One sentence for the tooltip.
    pub hint: &'static str,
}

/// An operator option.
#[derive(Debug, Serialize)]
pub struct WhenOperatorBody {
    /// Wire name.
    pub name: &'static str,
    /// One sentence for the tooltip.
    pub hint: &'static str,
}

/// A scope option.
#[derive(Debug, Serialize)]
pub struct ScopeTypeBody {
    /// Wire name.
    pub name: &'static str,
    /// Whether a rule in this scope has to name a site.
    pub needs_site: bool,
    /// One sentence for the tooltip.
    pub hint: &'static str,
}

/// The body of a dry-run request.
#[derive(Debug, serde::Deserialize)]
pub struct DryRunBody {
    /// The claims document, an LDAP entry or a SAML assertion summary. A JSON object is what every
    /// kind reduces to, so one shape serves all five provider kinds.
    #[serde(default)]
    pub sample: Value,
    /// An account in this organization whose **stored** group membership should be folded into the
    /// run alongside the sample's claim.
    ///
    /// This is what makes the preview able to rehearse a *provisioned* sign-in. A pasted sample
    /// is a claims document and a claims document is what an interactive IdP produces; the whole
    /// class of problems this endpoint has to answer — "does my group rule fire for the person the
    /// connector created?" — is about an account whose groups exist only in the database. Naming
    /// the subject is what lets the operator rehearse that case instead of rehearsing a token that
    /// will never arrive.
    ///
    /// Optional and never required: without it the run is exactly what it was before, and a
    /// sample that carries groups still works. A subject in another organization is refused rather
    /// than silently ignored — a cross-tenant answer here would tell an operator their rule fires
    /// when it would not, which is the one thing a preview must never do.
    #[serde(default)]
    pub subject: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The provider's rules plus the catalogue the editor needs.
pub async fn get_role_rules(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<RoleRulesBody>, ApiError> {
    let provider = load(&state, &current, id).await?;
    let rules = role_rule_store::load_rules(state.db().pool(), id)
        .await
        .map_err(internal)?;
    Ok(Json(rules_body(&provider, &rules)))
}

/// Replace the whole rule set.
pub async fn replace_role_rules(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<Value>,
) -> Result<Json<RoleRulesBody>, ApiError> {
    let provider = load(&state, &current, id).await?;

    // Parse first, validate second, write third — and refuse the whole set on any problem. Every
    // problem is in the 422, because a table editor that reports one error per submit is a table
    // editor somebody abandons halfway.
    let rules = RoleRules::from_value(&body).map_err(bad_request)?;
    let problems = rules.validate();
    if !problems.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "role_rules_invalid",
            problems
                .iter()
                .map(|problem| problem.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    // The role must exist **and** be in the caller's organization. A rule pointing at a platform
    // role from an organization that may not see it would grant access nobody can inspect, and the
    // foreign key alone does not catch that — `roles.organization_id` is nullable for platform
    // roles, so the row is valid and the grant is still wrong.
    if let Some(role_id) = first_role(&rules) {
        assert_role_visible(state.db().pool(), role_id, provider.organization_id).await?;
    }
    for role_id in all_roles(&rules) {
        assert_role_visible(state.db().pool(), role_id, provider.organization_id).await?;
    }

    let before = role_rule_store::load_rules(state.db().pool(), id)
        .await
        .map_err(internal)?;
    let stored = role_rule_store::replace_rules(state.db().pool(), id, rules)
        .await
        .map_err(bad_request)?;
    let after = role_rule_store::load_rules(state.db().pool(), id)
        .await
        .map_err(internal)?;

    // The audit entry carries the *shape* of the change, never the rule rows. A rule's
    // `when_value` is frequently a group name and, on a claim rule, frequently a value a person
    // would rather not have copied into an audit trail; the role ids and the order are what an
    // auditor needs and are what is recorded.
    let before_roles = role_ids(&before);
    let after_roles = role_ids(&after);
    let added: Vec<&Uuid> = after_roles
        .iter()
        .filter(|role| !before_roles.contains(role))
        .collect();
    let removed: Vec<&Uuid> = before_roles
        .iter()
        .filter(|role| !after_roles.contains(role))
        .collect();

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "iam.provider_role_rules_replaced")
            .target("auth_provider", id.to_string())
            .metadata(json!({
                "slug": provider.slug,
                "kind": provider.kind.as_str(),
                "row_count": stored.len(),
                "rule_count_before": before.rules.len(),
                "rule_count_after": after.rules.len(),
                "roles_added": added,
                "roles_removed": removed,
                "reordered": before.rules.len() == after.rules.len() && added.is_empty() && removed.is_empty(),
            }))
            .ip_address(address.as_text())
            .organization(Some(provider.organization_id)),
    )
    .await?;

    Ok(Json(rules_body(&provider, &after)))
}

/// Dry-run a sample identity against the **stored** rules.
///
/// The answer names the matched rule, the role, the scope and the reason string the audit line
/// will carry — so the operator reads the same sentence here that they will read in the audit log
/// after a real sign-in.
pub async fn preview_role_rules(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<DryRunBody>,
) -> Result<Json<Value>, ApiError> {
    let provider = load(&state, &current, id).await?;

    // A pasted sample is a document, not a query. The ceiling keeps this endpoint from becoming a
    // way to push arbitrary JSON through the evaluator.
    let size = serde_json::to_string(&body.sample)
        .map_err(|error| ApiError::bad_request("invalid_sample", error.to_string()))?
        .len();
    if size > MAX_SAMPLE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "sample_too_large",
            format!("the sample payload may be at most {MAX_SAMPLE_BYTES} bytes"),
        ));
    }
    if !body.sample.is_object() {
        return Err(ApiError::bad_request(
            "invalid_sample",
            "the sample must be a JSON object of claims, an LDAP entry or an assertion summary",
        ));
    }

    let rules = role_rule_store::load_rules(state.db().pool(), id)
        .await
        .map_err(internal)?;
    let identity = identity_from_sample(&body.sample, &provider);

    // A named subject folds its **stored** membership into the run, so the preview can rehearse
    // the sign-in a SCIM-provisioned account actually makes: a token with no group claim and a
    // group the connector wrote. Without this the preview is only ever able to rehearse the
    // interactive case, which is the case where group rules already worked.
    let (groups, membership_source) = match body.subject {
        None => (GroupContext::from_claim(identity.groups.clone()), None),
        Some(subject) => {
            let membership = subject_membership(&state, &provider, subject).await?;
            let mut context = GroupContext::from_claim(identity.groups.clone());
            let mut values = Vec::new();
            for group in &membership {
                values.push(group.slug.clone());
                if !group.name.eq_ignore_ascii_case(&group.slug) {
                    values.push(group.name.clone());
                }
            }
            context.push_membership(values);
            // Read *after* the push: the summary is a description of what the evaluator was
            // given, and taking it before the membership exists would describe the run that was
            // not performed.
            let source = context.summary();
            (context, Some(source))
        }
    };

    // The same call the callback makes, on the same rows, with the same group sources. This is the
    // entire point: a preview that evaluated a different context than the sign-in would be a
    // preview of nothing.
    let resolution = rules.resolve_with(&identity, &groups, default_role(&provider));
    let matched_position = match &resolution {
        Resolution::Matched { rule_position, .. } => Some(*rule_position),
        Resolution::Default { .. } => None,
    };

    // Every rule, with what it read and whether it matched. A rule that did not fire is reported
    // with an empty `read` array rather than omitted, so "this rule did not match" is visibly
    // different from "this rule read nothing at all" — which are different bugs with different
    // fixes.
    let trace: Vec<Value> = rules
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            // `index` is the rule's place in the stored set and `matched_position` is an `i32`
            // because that is what travels in the audit reason string. They are the same number,
            // so they are converted rather than compared across types — and the conversion is
            // written once, here, because a `usize` index compared against an `i32` is a compile
            // error at best and an off-by-one in a truncation cast at worst.
            let index = i32::try_from(index).unwrap_or(i32::MAX);
            let read = rule.read_with(&identity, &groups);
            // Which source supplied the value that matched. A group rule that fired on the
            // stored row and one that fired on the token are the same `Matched`, and an operator
            // debugging "why does this person have this role" needs to know which clock revokes
            // it — the next sign-in, or the next SCIM sync.
            let group_source = (rule.when_kind == WhenKind::Group)
                .then(|| groups.source_of(rule.when_value.trim()))
                .flatten();
            json!({
                "index": index,
                "position": rule.position,
                "label": format!("#{}", index + 1),
                "when_kind": rule.when_kind.as_str(),
                "when_key": rule.when_key,
                "when_operator": rule.when_operator.as_str(),
                "when_value": rule.when_value,
                "role_id": rule.role_id,
                "scope_type": rule.scope_type.as_str(),
                "site_id": rule.site_id,
                "stop": rule.stop,
                "enabled": rule.enabled,
                "read": read,
                "group_source": group_source.map(|source| source.as_str()),
                "matched": matched_position == Some(index),
                // Why a rule did not decide, in one word the panel can render as a chip. `disabled`
                // is separated from `no_match` because turning a rule off and having it not match
                // look the same is how a disabled rule silently becomes the cause of a support
                // ticket.
                "verdict": if !rule.enabled {
                    "disabled"
                } else if matched_position == Some(index) {
                    "matched"
                } else if matched_position.is_some_and(|position| position < index) {
                    "not_reached"
                } else {
                    "no_match"
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "provider_id": id,
        "resolution": resolution,
        "role_id": resolution.role_id(),
        "reason": resolution.reason(),
        "matched_rule_index": matched_position,
        "sample_email": identity.email,
        "sample_groups": identity.groups,
        // What the group rules were actually given, in the panel's vocabulary. Without it a
        // rule that did not fire is only observable as "no match", and the operator's next move
        // is to edit a rule that was correct all along.
        "group_source": groups.summary(),
        "group_source_detail": groups.summary().sentence(),
        "group_claim": groups.claim_values(),
        "group_membership": groups.membership_values(),
        "subject": body.subject,
        "membership_consulted": membership_source.map(|source| source.as_str()),
        "trace": trace,
        "problems": rules.validate(),
    })))
}

/// The stored groups of the subject a dry run was asked to rehearse.
///
/// Two refusals, both deliberate and both about the preview rather than the data:
///
/// * **The subject must belong to the provider's organization.** A cross-tenant subject would
///   produce a trace saying a rule fires when it will not on the real sign-in, which is the one
///   answer a preview must never give.
/// * **A read failure is an error, not an empty list.** The whole point of naming a subject is to
///   see its membership; answering "no groups" when the read failed sends the operator to fix a
///   rule, and the rule is fine.
async fn subject_membership(
    state: &AppState,
    provider: &providers::AuthProvider,
    subject: Uuid,
) -> Result<Vec<omnion_permissions::groups::MembershipGroup>, ApiError> {
    let organization_id = provider.organization_id;

    // `internal` above takes an `IdentityError`, not a `sqlx::Error`, so the database error is
    // mapped by hand and kept in the log rather than in the body.
    let owner: Option<Option<Uuid>> =
        sqlx::query_scalar("select organization_id from users where id = $1")
            .bind(subject)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| {
                tracing::warn!(error = %error, %subject, "the dry run could not read the subject");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "subject_unreadable",
                    "the account could not be read",
                )
            })?;

    // A missing account and an account in another tenant are answered the same way, deliberately:
    // the difference would let an administrator of one tenant probe which ids exist in another.
    if owner.flatten() != Some(organization_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "subject_not_in_organization",
            "no such account in this organization",
        ));
    }

    omnion_permissions::groups::membership_groups(state.db().pool(), subject, Some(organization_id))
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, %subject, "the dry run could not read stored membership");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "membership_unavailable",
                "the account's groups could not be read",
            )
        })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Load a provider and prove the caller may see it.
async fn load(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<providers::AuthProvider, ApiError> {
    let provider = providers::find_provider(state.db().pool(), id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    // A provider in another organization is answered as *absent* rather than forbidden: the
    // difference between "you may not see this" and "this does not exist" is an enumeration
    // oracle, and a provider id is not a secret.
    match (current.user.organization_id, provider.organization_id) {
        (Some(own), theirs) if own != theirs => return Err(not_found()),
        _ => {}
    }
    Ok(provider)
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "provider_not_found",
        "no such provider",
    )
}

fn bad_request(error: omnion_identity::IdentityError) -> ApiError {
    ApiError::bad_request("role_rules_invalid", error.to_string())
}

fn internal(error: omnion_identity::IdentityError) -> ApiError {
    // The message is deliberately generic: an `IdentityError` carrying a database message names the
    // table and sometimes the row. The detail belongs in the log, not in a response body.
    tracing::warn!(error = %error, "the role rules could not be read");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "role_rules_unreadable",
        "the role rules could not be read",
    )
}

/// The provider's default role, if it configures one.
fn default_role(provider: &providers::AuthProvider) -> Option<Uuid> {
    provider
        .config
        .get("default_role_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok())
}

/// Every role the set grants, deduplicated.
fn all_roles(rules: &RoleRules) -> Vec<Uuid> {
    let mut seen = std::collections::HashSet::new();
    rules
        .rules
        .iter()
        .filter(|rule| seen.insert(rule.role_id))
        .map(|rule| rule.role_id)
        .collect()
}

/// The first role the set grants, if any. Kept separate so the first check reads as the fast path.
fn first_role(rules: &RoleRules) -> Option<Uuid> {
    all_roles(rules).first().copied()
}

fn role_ids(rules: &RoleRules) -> Vec<Uuid> {
    rules.rules.iter().map(|rule| rule.role_id).collect()
}

/// Build an identity out of a pasted sample, leniently.
///
/// The sign-in path builds its identity with `identity_from_claims`, which **refuses** a claims
/// document with no `sub` — correctly, because a real assertion without a subject id is not an
/// identity. A dry run is not a real assertion: an operator pasting a fragment of a claims
/// document to see which rule fires is doing exactly the right thing, and refusing the rehearsal
/// for a field the *rules* never read would make the feature unusable for its most common case.
///
/// So the sample is built as far as it goes and the gaps are reported, rather than refused. The
/// rules read groups, department and title; none of them read the subject id, so a sample with
/// none of them is a perfectly good thing to evaluate.
fn identity_from_sample(sample: &Value, provider: &providers::AuthProvider) -> Identity {
    use omnion_identity::sso::attributes::flatten_values;
    use serde_json::Value as Json;

    let email_claim = provider.config.get("email_claim").and_then(Json::as_str);
    let email = email_claim
        .and_then(|claim| sample.get(claim))
        .map(flatten_values)
        .and_then(|values| values.into_iter().next())
        .or_else(|| {
            sample
                .get("email")
                .map(flatten_values)
                .and_then(|values| values.into_iter().next())
        })
        .unwrap_or_default();

    let group_claim = provider
        .config
        .get("group_claim")
        .and_then(Json::as_str)
        .unwrap_or("groups");
    let groups = sample
        .get(group_claim)
        .map(flatten_values)
        .unwrap_or_default();

    // Everything the sample carries is offered to the rules, so a claim rule on a claim the
    // provider never *maps* to a panel field still finds it. The attribute map only keeps the
    // eight fields it was told to keep; the claims document is where the rest of them live, and
    // an operator writing a rule against them is reading the document, not the projection.
    let mut attributes: serde_json::Map<String, Json> =
        sample.as_object().cloned().unwrap_or_default();
    for field in ["department", "title"] {
        if !attributes.contains_key(field) {
            if let Some(value) = mapped_field(sample, field) {
                attributes.insert(field.to_owned(), value);
            }
        }
    }

    Identity {
        subject: sample
            .get("sub")
            .and_then(Json::as_str)
            .unwrap_or("dry-run")
            .to_owned(),
        email,
        display_name: sample.get("name").and_then(Json::as_str).map(str::to_owned),
        groups,
        attributes,
    }
}

/// Read a panel field out of the provider's own attribute map, so a `department` rule evaluates
/// against what a real sign-in would have assigned rather than against a raw claim that happens
/// to share the name.
fn mapped_field(sample: &Value, field: &str) -> Option<Value> {
    let rows = sample.get("_mappings").and_then(Value::as_array)?;
    for row in rows {
        if row.get("target_field").and_then(Value::as_str) == Some(field) {
            if let Some(source) = row.get("source_attr").and_then(Value::as_str) {
                return sample.get(source).cloned();
            }
        }
    }
    None
}

/// Prove the role exists and is grantable by this organization.
///
/// `roles.organization_id` is nullable — a `null` row is a *platform* role, which every
/// organization may grant — and non-null means the role belongs to exactly one organization. The
/// foreign key on `provider_role_rules.role_id` catches a deleted role; this catches a role that
/// exists in somebody else's tenant, which is the case a rule must never be able to name.
async fn assert_role_visible(
    pool: &sqlx::PgPool,
    role_id: Uuid,
    organization_id: Uuid,
) -> Result<(), ApiError> {
    let row: Option<(Option<Uuid>,)> =
        sqlx::query_as("select organization_id from roles where id = $1")
            .bind(role_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                tracing::warn!(error = %error, "a role could not be read while saving rules");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "role_unreadable",
                    "the role could not be read",
                )
            })?;
    match row {
        // A role that does not exist is caught by the foreign key at write time, but the check
        // happens first so the answer names the role rather than the constraint.
        None => Err(ApiError::bad_request(
            "role_not_found",
            format!("role {role_id} does not exist"),
        )),
        Some((None,)) => Ok(()),
        Some((Some(own),)) if own == organization_id => Ok(()),
        Some((Some(_),)) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "role_not_grantable",
            format!(
                "role {role_id} belongs to another organization and cannot be granted by this provider"
            ),
        )),
    }
}

/// Shape the answer: the rules, the catalogue the editor needs, and the problems with what is
/// stored right now.
fn rules_body(provider: &providers::AuthProvider, rules: &RoleRules) -> RoleRulesBody {
    RoleRulesBody {
        provider_id: provider.id,
        rules: rules
            .rules
            .iter()
            .map(|rule| RuleBody {
                id: None,
                position: rule.position,
                when_kind: rule.when_kind.as_str(),
                needs_key: rule.when_kind.needs_key(),
                when_key: rule.when_key.clone(),
                when_operator: rule.when_operator.as_str(),
                when_value: rule.when_value.clone(),
                role_id: rule.role_id,
                scope_type: rule.scope_type.as_str(),
                site_id: rule.site_id,
                stop: rule.stop,
                enabled: rule.enabled,
            })
            .collect(),
        when_kinds: WhenKind::names()
            .into_iter()
            .filter_map(|name| WhenKind::parse(name).ok())
            .map(|kind| WhenKindBody {
                name: kind.as_str(),
                needs_key: kind.needs_key(),
                hint: when_hint(kind),
            })
            .collect(),
        when_operators: WhenOperator::names()
            .into_iter()
            .filter_map(|name| WhenOperator::parse(name).ok())
            .map(|operator| WhenOperatorBody {
                name: operator.as_str(),
                hint: operator_hint(operator),
            })
            .collect(),
        scope_types: ScopeType::names()
            .into_iter()
            .filter_map(|name| ScopeType::parse(name).ok())
            .map(|scope| ScopeTypeBody {
                name: scope.as_str(),
                needs_site: matches!(scope, ScopeType::Site),
                hint: match scope {
                    ScopeType::Organization => "the role applies across the whole organization",
                    ScopeType::Site => "the role applies to one site only",
                },
            })
            .collect(),
        default_role_id: default_role(provider),
        problems: rules.validate(),
    }
}

const fn when_hint(kind: WhenKind) -> &'static str {
    match kind {
        WhenKind::Claim => "one value from the claims document, by dotted path",
        WhenKind::Group => "one of the groups the configured group claim carried",
        WhenKind::Department => "the department the attribute map assigned",
        WhenKind::Title => "the job title the attribute map assigned",
        WhenKind::Always => "match everybody — put this last",
    }
}

const fn operator_hint(operator: WhenOperator) -> &'static str {
    match operator {
        WhenOperator::Equals => "the same text, ignoring case and surrounding spaces",
        WhenOperator::Contains => "the text appears somewhere inside the value",
        WhenOperator::StartsWith => "the value begins with the text",
        WhenOperator::Regex => "a regular expression, matched against the whole value",
    }
}
