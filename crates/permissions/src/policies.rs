//! ABAC policies: persistence, the overlay over an RBAC decision, the dry run and the version
//! history (docs/07-IAM.md §11).
//!
//! The evaluation itself lives in `omnion-policy-engine`, which is pure. This module is the half
//! that speaks to the database: it loads the `policies` rows of an organization, merges the
//! attributes a condition can read, applies the winning policy over the RBAC decision — and
//! stores what the builder saves, version by version.
//!
//! Attribute shape, in one place:
//!
//! * everything in `users.attributes` is readable **at the top level** and again under `user`
//!   (`{"plan": "pro"}` answers `plan` as well as `user.plan`);
//! * the request contributes `action` (the permission key), `subject.{type,id}`,
//!   `organization.id` and `resource.{site_id,path,department,module}` when they are known.
//!
//! An attribute that is not in the map resolves as `null` inside the engine, so a condition never
//! reads a value nobody supplied.

use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_policy_engine::{
    Attributes, Condition, LeafTrace, Policy, PolicyCandidate, PolicyEffect, PolicyMatch,
    PolicySet, trace, validate_target,
};

use crate::catalogue;
use crate::error::{PermissionsError, Result};
use crate::evaluate::{Decision, DenyReason, Grant, PolicyStamp};
use crate::model::{ResourceContext, Subject};

/// One policy row, as the panel reads it.
#[derive(Debug, Clone)]
pub struct PolicyRecord {
    /// Policy id.
    pub id: Uuid,
    /// The organization it belongs to.
    pub organization_id: Uuid,
    /// Name.
    pub name: String,
    /// What the reader should know about it.
    pub description: String,
    /// `allow` or `deny`.
    pub effect: PolicyEffect,
    /// Higher wins; equal priority resolves to deny.
    pub priority: i32,
    /// The condition tree.
    pub conditions: Condition,
    /// Permission keys (exact or with `*`) the policy speaks about.
    pub target_permissions: Vec<String>,
    /// Disabled policies are kept but never evaluated.
    pub enabled: bool,
    /// Version, bumped by every save.
    pub version: i32,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was last saved.
    pub updated_at: OffsetDateTime,
}

impl PolicyRecord {
    /// The engine's view of this row.
    #[must_use]
    pub fn to_policy(&self) -> Policy {
        Policy {
            id: self.id,
            name: self.name.clone(),
            effect: self.effect,
            priority: self.priority,
            enabled: self.enabled,
            target_permissions: self.target_permissions.clone(),
            conditions: self.conditions.clone(),
        }
    }
}

/// What a save carries.
#[derive(Debug, Clone)]
pub struct PolicyDraft {
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// `allow` or `deny`.
    pub effect: PolicyEffect,
    /// 0–1000.
    pub priority: i32,
    /// The condition tree.
    pub conditions: Condition,
    /// Target permissions.
    pub target_permissions: Vec<String>,
    /// Whether the policy is active.
    pub enabled: bool,
}

impl PolicyDraft {
    /// Check the draft against the catalogue and the documented ranges.
    ///
    /// Returns a sentence the reader can act on — the API turns it into `400 invalid_policy`.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("a policy needs a name".to_owned());
        }
        if name.chars().count() > 120 {
            return Err("a policy name may not exceed 120 characters".to_owned());
        }
        if !(0..=1000).contains(&self.priority) {
            return Err("priority must be between 0 and 1000".to_owned());
        }
        if self.target_permissions.is_empty() {
            return Err("a policy needs at least one target permission".to_owned());
        }

        let known: Vec<&str> = catalogue::CATALOGUE.iter().map(|entry| entry.key).collect();
        for pattern in &self.target_permissions {
            validate_target(pattern, &known)?;
        }

        let mut seen: Vec<&str> = Vec::new();
        for pattern in &self.target_permissions {
            let pattern = pattern.trim();
            if seen.contains(&pattern) {
                return Err(format!("{pattern:?} is listed twice"));
            }
            seen.push(pattern);
        }

        Ok(())
    }
}

/// One recorded version.
#[derive(Debug, Clone)]
pub struct PolicyVersionRecord {
    /// Version number.
    pub version: i32,
    /// Effect at that version.
    pub effect: PolicyEffect,
    /// Priority at that version.
    pub priority: i32,
    /// Conditions at that version.
    pub conditions: Condition,
    /// Targets at that version.
    pub target_permissions: Vec<String>,
    /// Enabled state at that version.
    pub enabled: bool,
    /// When it was saved.
    pub created_at: OffsetDateTime,
    /// Who saved it.
    pub changed_by: Option<Uuid>,
}

const POLICY_COLUMNS: &str = "id, organization_id, name, description, effect, priority, \
     conditions, target_permissions, enabled, version, created_at, updated_at";

/// Every policy of one organization, highest priority first.
pub async fn list(pool: &PgPool, organization_id: Uuid) -> Result<Vec<PolicyRecord>> {
    let rows: Vec<PolicyRow> = sqlx::query_as(&format!(
        "select {POLICY_COLUMNS} from policies where organization_id = $1 \
         order by priority desc, name asc"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(PolicyRow::into_record).collect()
}

/// The enabled policies of one organization — what the decision path evaluates.
pub async fn active(pool: &PgPool, organization_id: Uuid) -> Result<Vec<PolicyRecord>> {
    let rows: Vec<PolicyRow> = sqlx::query_as(&format!(
        "select {POLICY_COLUMNS} from policies where organization_id = $1 and enabled \
         order by priority desc, name asc"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(PolicyRow::into_record).collect()
}

/// One policy by id.
pub async fn find(pool: &PgPool, policy_id: Uuid) -> Result<Option<PolicyRecord>> {
    let row: Option<PolicyRow> = sqlx::query_as(&format!(
        "select {POLICY_COLUMNS} from policies where id = $1"
    ))
    .bind(policy_id)
    .fetch_optional(pool)
    .await?;

    row.map(PolicyRow::into_record).transpose()
}

/// Create a policy and record its first version.
pub async fn create(
    pool: &PgPool,
    organization_id: Uuid,
    author: Uuid,
    draft: &PolicyDraft,
) -> Result<PolicyRecord> {
    draft.validate().map_err(PermissionsError::InvalidPolicy)?;

    let row: PolicyRow = sqlx::query_as(&format!(
        "insert into policies \
         (organization_id, name, description, effect, priority, conditions, target_permissions, \
          enabled, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         returning {POLICY_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(draft.name.trim())
    .bind(&draft.description)
    .bind(draft.effect.as_str())
    .bind(draft.priority)
    .bind(draft.conditions.to_json())
    .bind(&draft.target_permissions)
    .bind(draft.enabled)
    .bind(author)
    .fetch_one(pool)
    .await?;

    let record = row.into_record()?;
    snapshot(pool, &record, Some(author)).await?;
    Ok(record)
}

/// Save a policy: the version moves forward and the previous state stays readable in history.
pub async fn update(
    pool: &PgPool,
    policy_id: Uuid,
    author: Uuid,
    draft: &PolicyDraft,
) -> Result<PolicyRecord> {
    draft.validate().map_err(PermissionsError::InvalidPolicy)?;

    let row: Option<PolicyRow> = sqlx::query_as(&format!(
        "update policies set \
            name = $2, description = $3, effect = $4, priority = $5, conditions = $6, \
            target_permissions = $7, enabled = $8, version = version + 1, \
            updated_at = now() \
         where id = $1 \
         returning {POLICY_COLUMNS}"
    ))
    .bind(policy_id)
    .bind(draft.name.trim())
    .bind(&draft.description)
    .bind(draft.effect.as_str())
    .bind(draft.priority)
    .bind(draft.conditions.to_json())
    .bind(&draft.target_permissions)
    .bind(draft.enabled)
    .fetch_optional(pool)
    .await?;

    let record = row.ok_or(PermissionsError::PolicyNotFound)?.into_record()?;
    snapshot(pool, &record, Some(author)).await?;
    Ok(record)
}

/// Remove a policy (its versions follow the row).
pub async fn delete(pool: &PgPool, policy_id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from policies where id = $1")
        .bind(policy_id)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(removed > 0)
}

/// The recorded versions of a policy, newest first.
pub async fn versions(pool: &PgPool, policy_id: Uuid) -> Result<Vec<PolicyVersionRecord>> {
    #[derive(sqlx::FromRow)]
    struct VersionRow {
        version: i32,
        effect: String,
        priority: i32,
        conditions: Value,
        target_permissions: Vec<String>,
        enabled: bool,
        created_at: OffsetDateTime,
        changed_by: Option<Uuid>,
    }

    let rows: Vec<VersionRow> = sqlx::query_as(
        "select version, effect, priority, conditions, target_permissions, enabled, created_at, \
                changed_by \
         from policy_versions where policy_id = $1 order by version desc",
    )
    .bind(policy_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PolicyVersionRecord {
                version: row.version,
                effect: PolicyEffect::parse(&row.effect).ok_or_else(|| {
                    PermissionsError::InvalidPolicy(format!(
                        "version {} carries an unknown effect",
                        row.version
                    ))
                })?,
                priority: row.priority,
                conditions: Condition::parse(&row.conditions)
                    .map_err(PermissionsError::InvalidPolicy)?,
                target_permissions: row.target_permissions,
                enabled: row.enabled,
                created_at: row.created_at,
                changed_by: row.changed_by,
            })
        })
        .collect()
}

/// Write the current state as a version row (called by every save).
async fn snapshot(pool: &PgPool, record: &PolicyRecord, author: Option<Uuid>) -> Result<()> {
    sqlx::query(
        "insert into policy_versions \
         (policy_id, version, effect, priority, conditions, target_permissions, enabled, changed_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (policy_id, version) do nothing",
    )
    .bind(record.id)
    .bind(record.version)
    .bind(record.effect.as_str())
    .bind(record.priority)
    .bind(record.conditions.to_json())
    .bind(&record.target_permissions)
    .bind(record.enabled)
    .bind(author)
    .execute(pool)
    .await?;

    Ok(())
}

/// Apply the organization's policies over an RBAC decision.
///
/// No organization, no enabled policies — the RBAC answer stands untouched.
pub async fn apply(
    pool: &PgPool,
    subject: Subject,
    context: &ResourceContext,
    key: &str,
    base: Decision,
) -> Result<Decision> {
    let Some(organization_id) = context.organization_id else {
        return Ok(base);
    };

    let policies = active(pool, organization_id).await?;
    if policies.is_empty() {
        return Ok(base);
    }

    let attributes = attributes_for(pool, subject, context, key).await?;
    let set = PolicySet::new(policies.iter().map(PolicyRecord::to_policy).collect());

    Ok(overlay(base, set.decide(key, &attributes)))
}

/// The pure half of [`apply`]: what a winning policy does to a decision.
///
/// An allow policy grants even where RBAC said nothing; a deny policy refuses even where RBAC
/// granted; without a winner nothing changes.
#[must_use]
pub fn overlay(base: Decision, winner: Option<PolicyMatch>) -> Decision {
    let Some(winner) = winner else {
        return base;
    };

    let stamp = PolicyStamp {
        policy_id: winner.policy_id,
        policy_name: winner.policy_name,
        priority: winner.priority,
    };

    match winner.effect {
        PolicyEffect::Allow => Decision::Allowed(Grant::from_policy(stamp)),
        PolicyEffect::Deny => Decision::Denied {
            reason: DenyReason::PolicyDeny,
            source: Some(Grant::from_policy(stamp)),
        },
    }
}

/// The attribute set a condition reads for one question.
pub async fn attributes_for(
    pool: &PgPool,
    subject: Subject,
    context: &ResourceContext,
    key: &str,
) -> Result<Attributes> {
    let stored: Value = match subject {
        Subject::User(user_id) => sqlx::query_scalar("select attributes from users where id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_else(|| json!({})),
        Subject::Group(_) | Subject::ServiceAccount(_) => json!({}),
    };

    Ok(attributes_from(&stored, subject, context, key))
}

/// The pure half of [`attributes_for`] — the merge rules in one readable place.
#[must_use]
pub fn attributes_from(
    stored: &Value,
    subject: Subject,
    context: &ResourceContext,
    key: &str,
) -> Attributes {
    let mut attributes = Attributes::default();

    if let Some(object) = stored.as_object() {
        for (name, value) in object {
            attributes.set(name, value.clone());
        }
    }
    // The same object one level down, so `user.plan` and `plan` both read.
    attributes.set("user", stored.clone());

    attributes.set("action", json!(key));
    attributes.set(
        "subject",
        json!({
            "type": subject.subject_type(),
            "id": subject.id().to_string(),
        }),
    );

    if let Some(organization_id) = context.organization_id {
        attributes.set("organization.id", json!(organization_id));
    }
    if let Some(site_id) = context.site_id {
        attributes.set("resource.site_id", json!(site_id));
    }
    if let Some(path) = &context.path {
        attributes.set("resource.path", json!(path));
    }
    if let Some(department) = &context.department {
        attributes.set("resource.department", json!(department));
    }
    if let Some(module) = &context.module {
        attributes.set("resource.module", json!(module));
    }

    attributes
}

/// Every policy of an organization with its verdict for one question — the simulator's list and
/// the builder's dry run.
pub async fn candidates(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
    attributes: &Attributes,
) -> Result<Vec<PolicyCandidate>> {
    let policies = list(pool, organization_id).await?;
    let set = PolicySet::new(policies.iter().map(PolicyRecord::to_policy).collect());
    Ok(set.candidates(key, attributes))
}

/// The dry run of one condition tree against sample attributes: every leaf with its verdict.
#[must_use]
pub fn explain_conditions(conditions: &Condition, attributes: &Attributes) -> Vec<LeafTrace> {
    trace(conditions, attributes)
}

/// The row shape `sqlx` maps.
#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    description: String,
    effect: String,
    priority: i32,
    conditions: Value,
    target_permissions: Vec<String>,
    enabled: bool,
    version: i32,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl PolicyRow {
    fn into_record(self) -> Result<PolicyRecord> {
        let effect = PolicyEffect::parse(&self.effect).ok_or_else(|| {
            PermissionsError::InvalidPolicy(format!("unknown effect {:?}", self.effect))
        })?;
        let conditions =
            Condition::parse(&self.conditions).map_err(PermissionsError::InvalidPolicy)?;

        Ok(PolicyRecord {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            description: self.description,
            effect,
            priority: self.priority,
            conditions,
            target_permissions: self.target_permissions,
            enabled: self.enabled,
            version: self.version,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluate::Decision;
    use crate::model::Scope;
    use omnion_policy_engine::Operator;

    fn context() -> ResourceContext {
        ResourceContext {
            organization_id: Some(Uuid::new_v4()),
            site_id: None,
            department: None,
            module: None,
            path: Some("/blog/hello".to_owned()),
        }
    }

    #[test]
    fn a_deny_policy_takes_a_grant_away() {
        let base = Decision::Denied {
            reason: DenyReason::MissingPermission,
            source: None,
        };
        let winner = PolicyMatch {
            policy_id: Uuid::new_v4(),
            policy_name: "Legal hold".to_owned(),
            effect: PolicyEffect::Deny,
            priority: 700,
        };

        match overlay(base, Some(winner)) {
            Decision::Denied { reason, source } => {
                assert_eq!(reason, DenyReason::PolicyDeny);
                let source = source.expect("a policy denial carries its source");
                assert_eq!(source.role_key, "policy");
                assert_eq!(source.policy.expect("the stamp").policy_name, "Legal hold");
            }
            other => panic!("expected a policy denial, got {other:?}"),
        }
    }

    #[test]
    fn an_allow_policy_grants_what_rbac_did_not() {
        let base = Decision::Denied {
            reason: DenyReason::MissingPermission,
            source: None,
        };
        let winner = PolicyMatch {
            policy_id: Uuid::new_v4(),
            policy_name: "Publishers abroad".to_owned(),
            effect: PolicyEffect::Allow,
            priority: 620,
        };

        let decision = overlay(base, Some(winner));
        assert!(decision.is_allowed(), "the policy flips the decision");
        match decision {
            Decision::Allowed(grant) => {
                assert_eq!(grant.via, crate::evaluate::Via::Policy);
                assert_eq!(grant.role_name, "Publishers abroad");
            }
            other => panic!("expected an allow, got {other:?}"),
        }
    }

    #[test]
    fn without_a_winner_nothing_changes() {
        let base = Decision::Denied {
            reason: DenyReason::ExplicitDeny,
            source: None,
        };
        assert_eq!(overlay(base.clone(), None), base);
    }

    #[test]
    fn attributes_merge_stored_user_data_with_the_request() {
        let stored = json!({"plan": "pro", "region": {"country": "TR"}});
        let context = context();
        let attributes = attributes_from(
            &stored,
            Subject::User(Uuid::new_v4()),
            &context,
            "content.pages.publish",
        );

        assert_eq!(attributes.get("plan"), json!("pro"), "top level");
        assert_eq!(
            attributes.get("user.plan"),
            json!("pro"),
            "and under `user`"
        );
        assert_eq!(attributes.get("user.region.country"), json!("TR"));
        assert_eq!(attributes.get("action"), json!("content.pages.publish"));
        assert_eq!(attributes.get("resource.path"), json!("/blog/hello"));
        assert_eq!(
            attributes.get("resource.site_id"),
            Value::Null,
            "not supplied"
        );

        let org = context.organization_id.expect("an organization");
        assert_eq!(attributes.get("organization.id"), json!(org));
    }

    #[test]
    fn a_draft_is_validated_against_the_catalogue() {
        let mut draft = PolicyDraft {
            name: "Legal hold".to_owned(),
            description: String::new(),
            effect: PolicyEffect::Deny,
            priority: 700,
            conditions: Condition::Leaf {
                attribute: "resource.path".to_owned(),
                operator: Operator::StartsWith,
                value: json!("/legal"),
            },
            target_permissions: vec!["content.pages.*".to_owned()],
            enabled: true,
        };
        assert!(draft.validate().is_ok());

        draft.name = "  ".to_owned();
        assert!(draft.validate().is_err(), "a blank name is refused");

        draft.name = "Legal hold".to_owned();
        draft.priority = 1001;
        assert!(draft.validate().is_err(), "priority stays inside 0-1000");

        draft.priority = 700;
        draft.target_permissions = vec!["nope.nothing".to_owned()];
        assert!(draft.validate().is_err(), "an unknown target is refused");

        draft.target_permissions = vec![
            "content.pages.read".to_owned(),
            "content.pages.read".to_owned(),
        ];
        assert!(draft.validate().is_err(), "a duplicate target is refused");

        draft.target_permissions = vec![];
        assert!(draft.validate().is_err(), "at least one target is required");
    }

    #[test]
    fn a_scope_without_an_organization_has_no_attributes_to_merge() {
        let scope = Scope::Global;
        let context = ResourceContext::from_scope(scope);
        let attributes = attributes_from(
            &json!({}),
            Subject::ServiceAccount(Uuid::new_v4()),
            &context,
            "media.read",
        );

        assert_eq!(attributes.get("organization.id"), Value::Null);
        assert_eq!(attributes.get("action"), json!("media.read"));
        assert_eq!(attributes.get("user"), json!({}));
    }
}
