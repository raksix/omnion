//! Permission evaluation: from role bindings to one decision.
//!
//! The resolver is pure and deterministic ([`RoleGraph::effective`]); the database functions
//! only load bindings and roles and hand them over. Security precedence (docs/07-IAM.md §5):
//!
//! ```text
//! Explicit Deny  >  Explicit Allow  >  Inherited Allow  >  Default Deny
//! ```
//!
//! Denies reach the principal both directly (a bound role refuses the key) and through
//! inheritance (an ancestor of a bound role refuses it). The document ranks inherited denies
//! nowhere, so this implementation keeps the safe reading: a deny anywhere on a chain beats an
//! allow gathered anywhere else, and the provenance of the decision is returned with it, so
//! the permission simulator (docs/07-IAM.md §18) can explain every verdict.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgPool;
use uuid::Uuid;

use crate::bindings;
use crate::error::Result;
use crate::model::{Effect, RoleAssignment, Scope};
use crate::roles;

/// How a permission reached the principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The role bound to the principal allows the key itself.
    ExplicitAllow,
    /// An ancestor of a bound role allows the key.
    InheritedAllow,
    /// The role bound to the principal refuses the key itself.
    ExplicitDeny,
    /// An ancestor of a bound role refuses the key.
    InheritedDeny,
}

impl Via {
    /// Strength used to pick the provenance when several roles speak about one key.
    fn rank(self) -> u8 {
        match self {
            Self::ExplicitDeny => 3,
            Self::InheritedDeny => 2,
            Self::ExplicitAllow => 1,
            Self::InheritedAllow => 0,
        }
    }

    /// `true` when this provenance refuses the permission.
    #[must_use]
    pub fn is_denial(self) -> bool {
        matches!(self, Self::ExplicitDeny | Self::InheritedDeny)
    }
}

/// Where a decision came from — the data behind "Role → Marketing Manager".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Role that carries the entry.
    pub role_id: Uuid,
    /// Key of that role.
    pub role_key: String,
    /// Name of that role.
    pub role_name: String,
    /// Priority of that role (the tie-breaker).
    pub role_priority: i32,
    /// How the permission reached the principal.
    pub via: Via,
}

/// Why a permission is not held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// A role refuses it explicitly (or through inheritance).
    ExplicitDeny,
    /// No role grants it.
    MissingPermission,
}

/// The outcome of an authorisation question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The principal holds the permission.
    Allowed(Grant),
    /// The principal does not hold it.
    Denied {
        /// Why.
        reason: DenyReason,
        /// The role that refused it, when one did.
        source: Option<Grant>,
    },
}

impl Decision {
    /// `true` for [`Decision::Allowed`].
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed(_))
    }
}

/// The permission set a principal holds, with the provenance of every entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectivePermissions {
    grants: BTreeMap<String, Grant>,
    denials: BTreeMap<String, Grant>,
}

impl EffectivePermissions {
    /// `true` when the key is granted and not refused.
    #[must_use]
    pub fn allows(&self, key: &str) -> bool {
        self.grants.contains_key(key) && !self.denials.contains_key(key)
    }

    /// Decide one key.
    #[must_use]
    pub fn decision(&self, key: &str) -> Decision {
        if let Some(grant) = self.denials.get(key) {
            return Decision::Denied {
                reason: DenyReason::ExplicitDeny,
                source: Some(grant.clone()),
            };
        }
        match self.grants.get(key) {
            Some(grant) => Decision::Allowed(grant.clone()),
            None => Decision::Denied {
                reason: DenyReason::MissingPermission,
                source: None,
            },
        }
    }

    /// Granted permissions with their provenance.
    #[must_use]
    pub fn grants(&self) -> &BTreeMap<String, Grant> {
        &self.grants
    }

    /// Refused permissions with their provenance.
    #[must_use]
    pub fn denials(&self) -> &BTreeMap<String, Grant> {
        &self.denials
    }

    /// Granted keys, sorted.
    #[must_use]
    pub fn granted_keys(&self) -> Vec<String> {
        self.grants
            .iter()
            .filter(|(key, _)| !self.denials.contains_key(*key))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Number of granted permissions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.granted_keys().len()
    }

    /// `true` when nothing is granted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn record(&mut self, key: String, effect: Effect, via: Via, role: &crate::model::Role) {
        let grant = Grant {
            role_id: role.id,
            role_key: role.key.clone(),
            role_name: role.name.clone(),
            role_priority: role.priority,
            via,
        };
        let target = match effect {
            Effect::Allow => &mut self.grants,
            Effect::Deny => &mut self.denials,
        };
        match target.get(&key) {
            Some(existing) if !is_stronger(&grant, existing) => {}
            _ => {
                target.insert(key, grant);
            }
        }
    }
}

/// Pick the provenance that should be reported: strongest route first, then the higher role.
fn is_stronger(candidate: &Grant, existing: &Grant) -> bool {
    match candidate.via.rank().cmp(&existing.via.rank()) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            (candidate.role_priority, &candidate.role_key)
                > (existing.role_priority, &existing.role_key)
        }
    }
}

/// The roles of one scope, indexed for inheritance walks.
#[derive(Debug, Clone, Default)]
pub struct RoleGraph {
    roles: BTreeMap<Uuid, RoleAssignment>,
}

impl RoleGraph {
    /// Index a set of roles.
    #[must_use]
    pub fn new(roles: impl IntoIterator<Item = RoleAssignment>) -> Self {
        Self {
            roles: roles
                .into_iter()
                .map(|assignment| (assignment.role.id, assignment))
                .collect(),
        }
    }

    /// Look a role up.
    #[must_use]
    pub fn role(&self, id: Uuid) -> Option<&RoleAssignment> {
        self.roles.get(&id)
    }

    /// Number of roles in the graph.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roles.len()
    }

    /// `true` when the graph holds no roles.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty()
    }

    /// Resolve the permissions of a principal bound to `bound_role_ids`.
    ///
    /// Every bound role contributes its own entries as explicit decisions and walks its
    /// inheritance chain for the rest. Cycles are cut defensively: a role already visited on a
    /// chain ends the walk instead of looping.
    #[must_use]
    pub fn effective(&self, bound_role_ids: &[Uuid]) -> EffectivePermissions {
        let mut effective = EffectivePermissions::default();

        for role_id in bound_role_ids {
            let Some(bound) = self.roles.get(role_id) else {
                continue;
            };

            let mut visited: BTreeSet<Uuid> = BTreeSet::new();
            let mut cursor = Some(bound);
            let mut inherited = false;

            while let Some(assignment) = cursor {
                if !visited.insert(assignment.role.id) {
                    break;
                }

                for entry in &assignment.permissions {
                    let via = match (entry.effect, inherited) {
                        (Effect::Allow, false) => Via::ExplicitAllow,
                        (Effect::Allow, true) => Via::InheritedAllow,
                        (Effect::Deny, false) => Via::ExplicitDeny,
                        (Effect::Deny, true) => Via::InheritedDeny,
                    };
                    effective.record(entry.key.clone(), entry.effect, via, &assignment.role);
                }

                let parent = assignment
                    .role
                    .inherits_role_id
                    .filter(|_| assignment.role.inherit_permissions);
                inherited = true;
                cursor = parent.and_then(|id| self.roles.get(&id));
            }
        }

        effective
    }
}

/// Load the roles of a scope (platform roles plus the organization's own) with their entries.
pub async fn load_role_graph(pool: &PgPool, organization_id: Option<Uuid>) -> Result<RoleGraph> {
    let roles = roles::list_roles(pool, organization_id).await?;
    let ids: Vec<Uuid> = roles.iter().map(|role| role.id).collect();
    let mut entries = roles::permission_entries(pool, &ids).await?;

    Ok(RoleGraph::new(roles.into_iter().map(|role| {
        let permissions = entries.remove(&role.id).unwrap_or_default();
        RoleAssignment { role, permissions }
    })))
}

/// The permissions a principal holds in one scope.
pub async fn effective_permissions(
    pool: &PgPool,
    user_id: Uuid,
    scope: Scope,
) -> Result<EffectivePermissions> {
    let bindings = bindings::active_for_scope(pool, user_id, scope).await?;
    let graph = load_role_graph(pool, scope.organization_id()).await?;
    let bound: Vec<Uuid> = bindings.iter().map(|binding| binding.role_id).collect();

    Ok(graph.effective(&bound))
}

/// Decide whether a principal holds one permission in one scope.
pub async fn authorize(pool: &PgPool, user_id: Uuid, scope: Scope, key: &str) -> Result<Decision> {
    let effective = effective_permissions(pool, user_id, scope).await?;
    Ok(effective.decision(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, RolePermission};
    use time::OffsetDateTime;

    fn role(key: &str, priority: i32, inherits: Option<Uuid>) -> Role {
        Role {
            id: Uuid::new_v4(),
            organization_id: None,
            key: key.to_owned(),
            name: key.to_owned(),
            description: String::new(),
            priority,
            inherits_role_id: inherits,
            inherit_permissions: true,
            is_system: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn assignment(role: Role, entries: &[(&str, Effect)]) -> RoleAssignment {
        RoleAssignment {
            permissions: entries
                .iter()
                .map(|(key, effect)| RolePermission {
                    key: (*key).to_owned(),
                    effect: *effect,
                })
                .collect(),
            role,
        }
    }

    #[test]
    fn a_bound_role_grants_its_own_entries() {
        let editor = role("editor", 300, None);
        let graph = RoleGraph::new([assignment(
            editor.clone(),
            &[
                ("content.pages.read", Effect::Allow),
                ("media.upload", Effect::Allow),
            ],
        )]);

        let effective = graph.effective(&[editor.id]);
        assert!(effective.allows("content.pages.read"));
        assert!(effective.allows("media.upload"));
        assert!(!effective.allows("content.pages.delete"));
        assert_eq!(effective.len(), 2);

        let allowed = effective.decision("media.upload");
        match allowed {
            Decision::Allowed(grant) => {
                assert_eq!(grant.role_key, "editor");
                assert_eq!(grant.via, Via::ExplicitAllow);
            }
            other => panic!("expected an allow, got {other:?}"),
        }

        let missing = effective.decision("content.pages.delete");
        assert_eq!(
            missing,
            Decision::Denied {
                reason: DenyReason::MissingPermission,
                source: None
            },
            "a permission nobody granted is denied by default"
        );
    }

    #[test]
    fn inherited_allows_reach_the_child_role() {
        // docs/07-IAM.md §4: Marketing Manager inherits Editor.
        let editor = role("editor", 300, None);
        let marketing = role("marketing-manager", 400, Some(editor.id));
        let graph = RoleGraph::new([
            assignment(
                editor.clone(),
                &[
                    ("content.pages.read", Effect::Allow),
                    ("content.pages.update", Effect::Allow),
                ],
            ),
            assignment(
                marketing.clone(),
                &[
                    ("content.pages.publish", Effect::Allow),
                    ("media.manage", Effect::Allow),
                ],
            ),
        ]);

        let effective = graph.effective(&[marketing.id]);
        for key in [
            "content.pages.read",
            "content.pages.update",
            "content.pages.publish",
            "media.manage",
        ] {
            assert!(effective.allows(key), "{key} must be granted");
        }

        match effective.decision("content.pages.update") {
            Decision::Allowed(grant) => {
                assert_eq!(grant.role_key, "editor");
                assert_eq!(grant.via, Via::InheritedAllow);
            }
            other => panic!("expected an inherited allow, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_deny_overrides_an_inherited_allow() {
        // docs/07-IAM.md §5: the reviewer keeps every editor permission except delete.
        let editor = role("editor", 300, None);
        let reviewer = role("content-reviewer", 350, Some(editor.id));
        let graph = RoleGraph::new([
            assignment(
                editor.clone(),
                &[
                    ("content.pages.update", Effect::Allow),
                    ("content.pages.delete", Effect::Allow),
                ],
            ),
            assignment(reviewer.clone(), &[("content.pages.delete", Effect::Deny)]),
        ]);

        let effective = graph.effective(&[reviewer.id]);
        assert!(effective.allows("content.pages.update"), "still inherited");

        let denied = effective.decision("content.pages.delete");
        match denied {
            Decision::Denied { reason, source } => {
                assert_eq!(reason, DenyReason::ExplicitDeny);
                let source = source.expect("a deny carries its source");
                assert_eq!(source.role_key, "content-reviewer");
                assert_eq!(source.via, Via::ExplicitDeny);
            }
            other => panic!("expected a denial, got {other:?}"),
        }
        assert!(
            !effective
                .granted_keys()
                .contains(&"content.pages.delete".to_owned()),
            "a denied key is never reported as granted"
        );
    }

    #[test]
    fn multiple_roles_merge_and_the_highest_priority_explains_the_decision() {
        let admin = role("administrator", 900, None);
        let editor = role("editor", 300, None);
        let graph = RoleGraph::new([
            assignment(admin.clone(), &[("users.read", Effect::Allow)]),
            assignment(editor.clone(), &[("users.read", Effect::Allow)]),
        ]);

        let effective = graph.effective(&[admin.id, editor.id]);
        match effective.decision("users.read") {
            Decision::Allowed(grant) => assert_eq!(grant.role_key, "administrator"),
            other => panic!("expected an allow, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_deny_from_another_binding_still_wins() {
        let admin = role("administrator", 900, None);
        let reviewer = role("content-reviewer", 350, None);
        let graph = RoleGraph::new([
            assignment(admin.clone(), &[("content.pages.publish", Effect::Allow)]),
            assignment(reviewer.clone(), &[("content.pages.publish", Effect::Deny)]),
        ]);

        let effective = graph.effective(&[admin.id, reviewer.id]);
        assert!(
            !effective.allows("content.pages.publish"),
            "an explicit deny beats an explicit allow"
        );
    }

    #[test]
    fn inheritance_can_be_switched_off_and_cycles_are_cut() {
        let editor = role("editor", 300, None);
        let mut detached = role("detached", 400, Some(editor.id));
        detached.inherit_permissions = false;
        let graph = RoleGraph::new([
            assignment(editor.clone(), &[("content.pages.read", Effect::Allow)]),
            assignment(detached.clone(), &[]),
        ]);

        let effective = graph.effective(&[detached.id]);
        assert!(
            !effective.allows("content.pages.read"),
            "inherit_permissions = false stops the walk"
        );

        // A defensive cycle: two roles pointing at each other must terminate.
        let mut first = role("first", 400, None);
        let second = role("second", 400, Some(first.id));
        first.inherits_role_id = Some(second.id);
        let cyclic = RoleGraph::new([
            assignment(first.clone(), &[("media.read", Effect::Allow)]),
            assignment(second.clone(), &[("media.upload", Effect::Allow)]),
        ]);

        let effective = cyclic.effective(&[first.id]);
        assert!(effective.allows("media.read"));
        assert!(effective.allows("media.upload"), "one hop is enough");
    }

    #[test]
    fn unknown_bindings_contribute_nothing() {
        let graph = RoleGraph::new([]);
        let effective = graph.effective(&[Uuid::new_v4()]);
        assert!(effective.is_empty());
        assert_eq!(
            effective.decision("content.pages.read"),
            Decision::Denied {
                reason: DenyReason::MissingPermission,
                source: None
            }
        );
    }
}
