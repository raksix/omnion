//! Audit entries: append-only rows describing who did what, to which target.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;

/// Who performed an action (mirrors the `audit_log.actor_type` constraint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorType {
    /// A signed-in person.
    User,
    /// An AI agent acting through the AI Hub (docs/06-AI-HUB.md).
    Agent,
    /// A service account or API key.
    Service,
    /// The platform itself (bootstrapping, scheduled work).
    System,
}

impl ActorType {
    /// Value stored in `audit_log.actor_type`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Service => "service",
            Self::System => "system",
        }
    }
}

/// A stored audit row.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AuditEntry {
    /// Creation order (identity column).
    pub id: i64,
    /// Organization the action belongs to (`None` = platform level).
    pub organization_id: Option<Uuid>,
    /// Account that performed the action, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// `user`, `agent`, `service` or `system`.
    pub actor_type: String,
    /// Stable action name, e.g. `iam.role.created`.
    pub action: String,
    /// Kind of the target (`role`, `binding`, …).
    pub target_type: Option<String>,
    /// Identifier of the target, as text.
    pub target_id: Option<String>,
    /// Structured detail; never carries secrets.
    pub metadata: serde_json::Value,
    /// Peer address of the actor.
    pub ip_address: Option<String>,
    /// The request id handed to the caller, echoed back so a refusal is joinable (REQ-125).
    pub request_id: Option<Uuid>,
    /// The credential lease the operation touched, when it was one.
    pub lease_id: Option<Uuid>,
    /// The machine identity that spent the operation, when a deployment key was the actor.
    pub deployment_key_id: Option<Uuid>,
    /// The pipeline identity the machine presented, as text.
    pub pipeline: Option<String>,
    /// When the action was recorded.
    pub created_at: OffsetDateTime,
}

/// An audit row to be written.
///
/// Built with [`NewAuditEntry::by_user`] / [`NewAuditEntry::system`] and the `with_*` style
/// setters; `metadata` stays a JSON object so readers never have to parse free text.
#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    /// Organization the action belongs to.
    pub organization_id: Option<Uuid>,
    /// Account that performed the action.
    pub actor_user_id: Option<Uuid>,
    /// Kind of actor.
    pub actor_type: ActorType,
    /// Stable action name.
    pub action: &'static str,
    /// Kind of the target.
    pub target_type: Option<&'static str>,
    /// Identifier of the target.
    pub target_id: Option<String>,
    /// Structured detail.
    pub metadata: serde_json::Value,
    /// Peer address of the actor.
    pub ip_address: Option<String>,
    /// The request id handed to the caller, so a refusal joins to the row explaining it.
    ///
    /// Null on every row written before REQ-125 slice 4 — the honest value for "this action was
    /// not part of a request that was ever refused", and the reason the column is nullable with
    /// no default rather than a backfilled uuid.
    pub request_id: Option<Uuid>,
    /// The credential lease the operation touched, when it was one.
    pub lease_id: Option<Uuid>,
    /// The machine identity that spent the operation, when a deployment key was the actor.
    pub deployment_key_id: Option<Uuid>,
    /// The pipeline identity the machine presented, as text.
    pub pipeline: Option<String>,
}

impl NewAuditEntry {
    /// An entry for an action performed by a signed-in account.
    #[must_use]
    pub fn by_user(actor_user_id: Uuid, action: &'static str) -> Self {
        Self {
            organization_id: None,
            actor_user_id: Some(actor_user_id),
            actor_type: ActorType::User,
            action,
            target_type: None,
            target_id: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            ip_address: None,
            request_id: None,
            lease_id: None,
            deployment_key_id: None,
            pipeline: None,
        }
    }

    /// An entry for an action performed by the platform itself.
    #[must_use]
    pub fn system(action: &'static str) -> Self {
        Self {
            organization_id: None,
            actor_user_id: None,
            actor_type: ActorType::System,
            action,
            target_type: None,
            target_id: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            ip_address: None,
            request_id: None,
            lease_id: None,
            deployment_key_id: None,
            pipeline: None,
        }
    }

    /// Set the organization the action belongs to.
    #[must_use]
    pub fn organization(mut self, organization_id: impl Into<Option<Uuid>>) -> Self {
        self.organization_id = organization_id.into();
        self
    }

    /// Set the target of the action.
    #[must_use]
    pub fn target(mut self, target_type: &'static str, target_id: impl Into<String>) -> Self {
        self.target_type = Some(target_type);
        self.target_id = Some(target_id.into());
        self
    }

    /// Attach structured detail.
    #[must_use]
    pub fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Record the peer address of the actor.
    #[must_use]
    pub fn ip_address(mut self, ip_address: Option<String>) -> Self {
        self.ip_address = ip_address;
        self
    }

    /// Attach the request id the caller was handed in its error banner (REQ-125, slice 4).
    ///
    /// This is the field that makes a *refusal* joinable: the caller holds a request id and the
    /// row explaining it is found by filtering on exactly that value. Without it the id in the
    /// error banner is decoration — a human-readable string that no query can match.
    ///
    /// It is a setter rather than a constructor argument because a null here is the honest value
    /// for the overwhelming majority of rows in the platform: the vast majority of actions are not
    /// part of a lease, a deployment key or a secret operation, and making every caller think
    /// about it would be four arguments of `None` on every call site in the codebase.
    #[must_use]
    pub fn request_id(mut self, request_id: impl Into<Option<Uuid>>) -> Self {
        self.request_id = request_id.into();
        self
    }

    /// Attach the credential lease this operation touched.
    #[must_use]
    pub fn lease_id(mut self, lease_id: impl Into<Option<Uuid>>) -> Self {
        self.lease_id = lease_id.into();
        self
    }

    /// Attach the machine identity that spent the operation, when a deployment key was the actor.
    #[must_use]
    pub fn deployment_key_id(mut self, deployment_key_id: impl Into<Option<Uuid>>) -> Self {
        self.deployment_key_id = deployment_key_id.into();
        self
    }

    /// Attach the pipeline identity the machine presented.
    ///
    /// Text, not a relation: a leaked deployment key has to be traced to a *pipeline name* the
    /// operator recognises, and a name is not a row in any table.
    #[must_use]
    pub fn pipeline(mut self, pipeline: impl Into<Option<String>>) -> Self {
        self.pipeline = pipeline.into();
        self
    }

    /// The lease, machine identity and pipeline in one call, which is how a redemption writes.
    #[must_use]
    pub fn machine(
        mut self,
        deployment_key_id: Option<Uuid>,
        pipeline: Option<String>,
        lease_id: Option<Uuid>,
    ) -> Self {
        self.deployment_key_id = deployment_key_id;
        self.pipeline = pipeline;
        self.lease_id = lease_id;
        self
    }
}

/// Columns read back from `audit_log`, with `inet` rendered as text.
const AUDIT_COLUMNS: &str = "id, organization_id, actor_user_id, actor_type, action, \
     target_type, target_id, metadata, ip_address::text as ip_address, created_at, request_id, \
     lease_id, deployment_key_id, pipeline";

/// Append an entry to the audit trail.
///
/// Callers treat a failure as a failure of the action itself: an unrecorded privileged action
/// is worse than a reported one, because the trail is what the operator audits afterwards.
pub async fn record(pool: &PgPool, entry: NewAuditEntry) -> Result<AuditEntry> {
    let sql = format!(
        "insert into audit_log (organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata, ip_address, request_id, lease_id, deployment_key_id, pipeline) \
         values ($1, $2, $3, $4, $5, $6, $7, cast($8 as inet), $9, $10, $11, $12) \
         returning {AUDIT_COLUMNS}"
    );

    let stored: AuditEntry = sqlx::query_as(&sql)
        .bind(entry.organization_id)
        .bind(entry.actor_user_id)
        .bind(entry.actor_type.as_str())
        .bind(entry.action)
        .bind(entry.target_type)
        .bind(entry.target_id.as_deref())
        .bind(entry.metadata)
        .bind(entry.ip_address.as_deref())
        .bind(entry.request_id)
        .bind(entry.lease_id)
        .bind(entry.deployment_key_id)
        .bind(entry.pipeline.as_deref())
        .fetch_one(pool)
        .await?;

    Ok(stored)
}

/// Most recent entries, newest first. `organization_id` filters when given.
pub async fn recent(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AuditEntry>> {
    let sql = format!(
        "select {AUDIT_COLUMNS} from audit_log \
         where ($1::uuid is null or organization_id = $1) \
         order by created_at desc, id desc limit $2"
    );

    let entries: Vec<AuditEntry> = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_types_match_the_schema_constraint() {
        assert_eq!(ActorType::User.as_str(), "user");
        assert_eq!(ActorType::Agent.as_str(), "agent");
        assert_eq!(ActorType::Service.as_str(), "service");
        assert_eq!(ActorType::System.as_str(), "system");
    }

    #[test]
    fn builders_carry_the_action_and_target() {
        let actor = Uuid::nil();
        let entry = NewAuditEntry::by_user(actor, "iam.role.created")
            .target("role", actor.to_string())
            .metadata(serde_json::json!({ "key": "editor" }))
            .ip_address(Some("203.0.113.7".to_owned()));

        assert_eq!(entry.action, "iam.role.created");
        assert_eq!(entry.actor_user_id, Some(actor));
        assert_eq!(entry.actor_type, ActorType::User);
        assert_eq!(entry.target_type, Some("role"));
        assert_eq!(entry.target_id.as_deref(), Some(actor.to_string().as_str()));
        assert_eq!(entry.metadata["key"], "editor");
        assert_eq!(entry.ip_address.as_deref(), Some("203.0.113.7"));

        let system = NewAuditEntry::system("iam.bootstrap.owner_bound");
        assert_eq!(system.actor_user_id, None);
        assert_eq!(system.actor_type, ActorType::System);
    }
}
