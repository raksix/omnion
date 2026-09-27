//! The permission safety invariants (docs/07-IAM.md §19).
//!
//! Two rules stand between an administrator and an installation nobody can administer:
//!
//! 1. **Nobody locks themselves out.** When the caller revokes their own binding and would be
//!    left without a privileged binding of their own (directly or through a group), the change is
//!    refused. This one is checked first: it is the more specific answer when both rules apply.
//! 2. **A scope keeps at least one Owner or Administrator.** A binding is defended inside its own
//!    class — an organization-scoped binding by the same organization, a global (platform)
//!    binding by the other global ones. Removing the last live privileged binding of that class
//!    is refused.
//!
//! Both refusals name the invariant, and both are checked before the row is touched, so a refused
//! change leaves the store exactly as it was. An expired binding grants nothing, so revoking one
//! is never refused — cleanup stays possible.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;

/// The role keys that carry platform-wide power. A role created by a customer does not count:
/// the invariant is about the built-in Owner and Administrator roles.
pub const PRIVILEGED_ROLE_KEYS: [&str; 2] = ["owner", "administrator"];

/// A change the invariants refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantRefusal {
    /// Stable machine-readable code (`last_owner_binding`, `self_lockout`).
    pub code: &'static str,
    /// The sentence the reader gets — it names the invariant that blocked the change.
    pub message: String,
}

impl InvariantRefusal {
    /// The last privileged binding of the scope class.
    fn last_owner(role_key: &str) -> Self {
        Self {
            code: "last_owner_binding",
            message: format!(
                "an organization must keep at least one live owner or administrator binding — \
                 this is the last one (role {role_key:?}), so grant it to another account first"
            ),
        }
    }

    /// The caller's own last privileged binding.
    fn self_lockout(role_key: &str) -> Self {
        Self {
            code: "self_lockout",
            message: format!(
                "this would remove your own last owner or administrator binding (role \
                 {role_key:?}) and lock you out — keep a privileged binding of your own first, \
                 directly or through a group"
            ),
        }
    }
}

/// Check whether revoking one binding is allowed.
///
/// `Ok(None)` means the change may go ahead; `Ok(Some(refusal))` is the reason it may not.
/// A binding that does not exist, is already revoked or is expired is not defended — the caller
/// handles those cases with its own answers.
pub async fn check_binding_revocation(
    pool: &PgPool,
    binding_id: Uuid,
    caller: Uuid,
) -> Result<Option<InvariantRefusal>> {
    #[derive(sqlx::FromRow)]
    struct BindingRow {
        subject_type: String,
        subject_id: Uuid,
        organization_id: Option<Uuid>,
        role_key: String,
        expired: bool,
    }

    let row: Option<BindingRow> = sqlx::query_as(
        "select b.subject_type, b.subject_id, b.organization_id, r.key as role_key, \
                (b.expires_at is not null and b.expires_at <= now()) as expired \
         from role_bindings b join roles r on r.id = b.role_id \
         where b.id = $1 and b.revoked_at is null",
    )
    .bind(binding_id)
    .fetch_optional(pool)
    .await?;

    let Some(binding) = row else {
        return Ok(None);
    };
    if binding.expired || !PRIVILEGED_ROLE_KEYS.contains(&binding.role_key.as_str()) {
        return Ok(None);
    }

    let sensitive: Vec<String> = PRIVILEGED_ROLE_KEYS
        .iter()
        .map(|key| (*key).to_owned())
        .collect();

    // 1. The caller keeps a privileged binding of their own.
    if binding.subject_type == "user" && binding.subject_id == caller {
        let own: i64 = sqlx::query_scalar(
            "select count(*) from role_bindings b join roles r on r.id = b.role_id \
             where b.id <> $1 and b.revoked_at is null \
               and (b.expires_at is null or b.expires_at > now()) \
               and r.key = any($2) \
               and ( \
                    (b.subject_type = 'user' and b.subject_id = $3) \
                    or (b.subject_type = 'group' and b.subject_id in \
                        (select group_id from group_members where user_id = $3)) \
               )",
        )
        .bind(binding_id)
        .bind(&sensitive)
        .bind(caller)
        .fetch_one(pool)
        .await?;

        if own == 0 {
            return Ok(Some(InvariantRefusal::self_lockout(&binding.role_key)));
        }
    }

    // 2. The scope class keeps at least one live privileged binding: an organization-scoped
    //    binding is defended by its organization, a global binding by the other global ones.
    let remaining: i64 = sqlx::query_scalar(
        "select count(*) from role_bindings b join roles r on r.id = b.role_id \
         where b.id <> $1 and b.revoked_at is null \
           and (b.expires_at is null or b.expires_at > now()) \
           and r.key = any($2) \
           and ( \
                ($3::uuid is not null and b.organization_id = $3) \
                or ($3::uuid is null and b.scope_type = 'global' and b.organization_id is null) \
           )",
    )
    .bind(binding_id)
    .bind(&sensitive)
    .bind(binding.organization_id)
    .fetch_one(pool)
    .await?;

    if remaining == 0 {
        return Ok(Some(InvariantRefusal::last_owner(&binding.role_key)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_name_the_invariant() {
        let last = InvariantRefusal::last_owner("owner");
        assert_eq!(last.code, "last_owner_binding");
        assert!(last.message.contains("last one"), "{}", last.message);
        assert!(last.message.contains("owner"), "{}", last.message);
        assert!(
            last.message.contains("at least one"),
            "the sentence names the invariant: {}",
            last.message
        );

        let lockout = InvariantRefusal::self_lockout("administrator");
        assert_eq!(lockout.code, "self_lockout");
        assert!(
            lockout.message.contains("your own last"),
            "{}",
            lockout.message
        );
        assert!(
            lockout.message.contains("lock you out"),
            "{}",
            lockout.message
        );
    }

    #[test]
    fn only_the_built_in_privileged_roles_are_defended() {
        assert!(PRIVILEGED_ROLE_KEYS.contains(&"owner"));
        assert!(PRIVILEGED_ROLE_KEYS.contains(&"administrator"));
        assert!(!PRIVILEGED_ROLE_KEYS.contains(&"editor"));
    }
}
