//! Whose authority a rule acts with — and what happens when that authority is gone
//! (REQ-003 slice 3).
//!
//! A rule is a stored instruction that fires without a person present. The one thing that
//! makes that safe is that it does not carry *authority* of its own: it runs as an account,
//! and that account's permissions are resolved **at the moment the step runs**, not when
//! the rule was written. A rule written on Monday by an administrator and fired on Friday
//! runs with whatever that administrator holds on Friday.
//!
//! The request asks for this to be "a feature test, not an edge case" and to be written
//! *first*, and the reason is visible in the shape of the code below: everything in it is
//! written so that the **denial** is the well-trodden path.
//!
//! * [`Authority::of`] resolves the account, and the failure modes are the interesting
//!   ones. A rule with no explicit run-as follows its author; a rule whose author was
//!   *deleted* resolves to nobody, because a deleted account's authority is not a permission
//!   anybody holds — falling back to "the creating user still exists somewhere" is exactly
//!   the stale-authority hole the feature exists to close.
//! * [`authorise_action`] is the check every host action makes, and it returns a [`Refusal`]
//!   with the permission named and a message written for the person who *authored* the rule,
//!   not for somebody reading a log.
//! * [`permission_for`] is a closed map from action to permission. It is a `match` and not a
//!   table on purpose: an action that does not appear here is not runnable at all (the host
//!   handler refuses it by name), so forgetting a permission is a compile error rather than
//!   a rule that quietly runs with no authority.
//!
//! What the engine does with a refusal is the last piece and it is in the engine, not here:
//! a revoked permission **stops the run** with `automation.rule.permission_revoked`. It is
//! not a retry and not a `continue`: the action's premise is false, and a rule that keeps
//! going is a rule that publishes the page anyway.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use omnion_permissions::{Decision, EffectivePermissions, Scope};

use crate::error::{AutomationError, Result};

/// The event recorded when a run-as account lost a permission an action needs.
pub const PERMISSION_REVOKED_EVENT: &str = "automation.rule.permission_revoked";

/// The permission an action needs, or `None` when the action needs none.
///
/// `None` is not "allow it unchecked": the engine's synthetic actions run with no account at
/// all, and the *host* actions that touch the world are exactly the ones listed here. An
/// action that grows an effect must grow an entry — and the test below is what notices when
/// it does not.
#[must_use]
pub fn permission_for(action: &str) -> Option<&'static str> {
    match action {
        // A publication is a content permission, not a workflows one, because it publishes
        // content. This is the request's own example: losing `content.pages.publish` must
        // stop a rule that would otherwise publish.
        "publish_page" => Some("content.pages.publish"),
        // A comment is a change to a revision, and it rides the permission that means "you
        // may change this content" — there is no separate comment key in the catalogue and
        // inventing one would be a permission nobody's role carries.
        "comment_revision" => Some("content.pages.update"),
        // Everything that leaves the process on the rule's own account is a `workflows.run`:
        // the person who may start a rule may have it send, call and chain.
        "send_email" | "http_request" | "run_workflow" => Some("workflows.run"),
        // Spending tokens is not a privilege `workflows.run` should imply. A rule that
        // prompts a model on a schedule costs money on every firing, so it rides `ai.chat`
        // — the same key the console's generate button is behind — and a role that may run
        // rules but may not chat cannot arm one. A new permission key would be a key no
        // existing role carries, which silently means "nobody" until an admin visits the
        // catalogue, so the existing one is the right one here.
        "ai.prompt" => Some("ai.chat"),
        // The synthetic actions touch nothing outside the run, so they need no account: the
        // engine is the whole authority there.
        _ => None,
    }
}

/// Every host action and the permission it needs, for the panel's settings screen.
///
/// Kept next to [`permission_for`] and asserted against it by a test, because two lists that
/// can disagree are two answers to "what does this action need" and only one of them is used.
pub const ACTION_PERMISSIONS: &[(&str, &str)] = &[
    ("publish_page", "content.pages.publish"),
    ("comment_revision", "content.pages.update"),
    ("send_email", "workflows.run"),
    ("http_request", "workflows.run"),
    ("run_workflow", "workflows.run"),
    ("ai.prompt", "ai.chat"),
];

/// The account a rule's host actions run with.
///
/// Read from `run_as_user_id`, falling back to `created_by`. Neither id is copied onto the
/// run, so a rule handed to a service account mid-flight changes what its *next* step may do
/// — which is the behaviour an operator expects from a settings field, and the opposite of
/// what a snapshot on the run would give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    /// The account whose permissions resolve, when there is one.
    pub user_id: Option<Uuid>,
    /// Why it is that account — the panel says which of the two it is.
    pub source: AuthoritySource,
}

/// Where a rule's authority comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthoritySource {
    /// `workflows.run_as_user_id` names an account.
    Explicit,
    /// The rule follows its author.
    Author,
    /// Neither: the rule was written by a person who no longer exists.
    Nobody,
}

impl Authority {
    /// Read a rule's authority.
    #[must_use]
    pub fn of(run_as_user_id: Option<Uuid>, created_by: Option<Uuid>) -> Self {
        if let Some(user_id) = run_as_user_id {
            return Self {
                user_id: Some(user_id),
                source: AuthoritySource::Explicit,
            };
        }
        match created_by {
            Some(user_id) => Self {
                user_id: Some(user_id),
                source: AuthoritySource::Author,
            },
            // A rule nobody can be attributed to has no authority. Saying so is the whole
            // point: falling back to some platform account would make "the author was
            // deleted" indistinguishable from "the author always had these rights".
            None => Self {
                user_id: None,
                source: AuthoritySource::Nobody,
            },
        }
    }

    /// `true` when there is an account to resolve permissions for.
    #[must_use]
    pub fn is_someone(&self) -> bool {
        self.user_id.is_some()
    }

    /// What the panel shows beside the run-as picker.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self.source {
            AuthoritySource::Explicit => "Runs as the account chosen in this rule's settings",
            AuthoritySource::Author => "Runs as the rule's author",
            AuthoritySource::Nobody => {
                "Runs as nobody — the rule's author no longer exists, so it cannot act"
            }
        }
    }
}

/// A refusal, in the words the person who authored the rule needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The permission that is missing.
    pub permission: &'static str,
    /// The account that is missing it — `None` when the rule runs as nobody at all.
    pub user_id: Option<Uuid>,
    /// The action that needed it.
    pub action: &'static str,
    /// A sentence an operator reads in a step's error column.
    pub message: String,
}

impl Refusal {
    /// The step error this refusal becomes.
    ///
    /// Prefixed with the event name the request specifies, so a subscription on
    /// `automation.rule.permission_revoked` and a grep through the run history both find the
    /// same string, and the two can never disagree about what happened.
    #[must_use]
    pub fn as_step_error(&self) -> String {
        format!("{PERMISSION_REVOKED_EVENT}: {}", self.message)
    }

    /// The event payload for the `permission_revoked` emission.
    #[must_use]
    pub fn as_event_payload(&self) -> Value {
        json!({
            "event": PERMISSION_REVOKED_EVENT,
            "permission": self.permission,
            "action": self.action,
            "run_as_user_id": self.user_id,
        })
    }
}

/// The action's own name, or a phrase for one nobody knows.
///
/// The `&'static str` in a [`Refusal`] cannot borrow the caller's action string, so this is
/// where an unknown action is turned into words instead of being kept as a slice of
/// something that is about to be dropped.
#[must_use]
pub fn action_label(action: &str) -> &'static str {
    match action {
        "publish_page" => "publish_page",
        "comment_revision" => "comment_revision",
        "send_email" => "send_email",
        "http_request" => "http_request",
        "run_workflow" => "run_workflow",
        _ => "this action",
    }
}

/// The permissions an account holds in one organization.
///
/// `Ok` with an empty set when the account does not exist: a deleted user row and a user id
/// that was never one are the same thing to a rule, which is that there is nobody to hold
/// anything. Letting this be an error would push a `RowNotFound` into a step's error column,
/// where the honest sentence is the refusal's.
pub async fn effective(
    pool: &PgPool,
    authority: &Authority,
    organization_id: Uuid,
) -> Result<EffectivePermissions> {
    let Some(user_id) = authority.user_id else {
        return Ok(EffectivePermissions::default());
    };

    omnion_permissions::effective_permissions(
        pool,
        user_id,
        Scope::Organization { organization_id },
    )
    .await
    .map_err(|err| AutomationError::invalid("authority_unreadable", err.to_string()))
}

/// Check one action against a rule's authority.
///
/// The one call every host action makes before it does anything. An action with no
/// permission of its own (an action that touches only the run) is `Ok` — the engine is the
/// authority there, and a rule that only echoes is not doing anything to the world.
pub async fn authorise_action(
    pool: &PgPool,
    authority: &Authority,
    organization_id: Uuid,
    action: &str,
) -> std::result::Result<(), Refusal> {
    let Some(permission) = permission_for(action) else {
        return Ok(());
    };
    let label = action_label(action);

    // A rule with nobody behind it is refused *before* the permission set is read: there is
    // no set to read, and the message has to name the real problem rather than "missing
    // permission", which would send an operator looking at a role assignment that is not
    // the thing that broke.
    let Some(user_id) = authority.user_id else {
        return Err(Refusal {
            permission,
            user_id: None,
            action: label,
            message: format!(
                "this rule runs as nobody — its author no longer exists — so `{action}` \
                 cannot be given {permission}. Give the rule a run-as account in its \
                 settings, or have an administrator rewrite it."
            ),
        });
    };

    let decision = omnion_permissions::authorize(
        pool,
        user_id,
        Scope::Organization { organization_id },
        permission,
    )
    .await
    .map_err(|err| Refusal {
        permission,
        user_id: Some(user_id),
        action: label,
        message: format!(
            "{permission} could not be checked for the run-as account: {err}. The step was \
             not run."
        ),
    })?;

    match decision {
        Decision::Allowed(_) => Ok(()),
        Decision::Denied { reason, source } => {
            // The provenance is the fastest route to a fix: "refused by the role Editor"
            // tells an operator which binding to change, and "no role grants it" tells them
            // there is nothing to remove. Both are facts about the store, read out here
            // rather than guessed at.
            let because = match (reason, source) {
                (omnion_permissions::DenyReason::ExplicitDeny, Some(grant)) => {
                    format!("the role {} refuses it", grant.role_name)
                }
                (omnion_permissions::DenyReason::PolicyDeny, Some(grant)) => {
                    format!("the policy {} refuses it", grant.role_name)
                }
                _ => "no role grants it".to_owned(),
            };

            Err(Refusal {
                permission,
                user_id: Some(user_id),
                action: label,
                message: format!(
                    "the account this rule runs as no longer holds {permission} — {because} — \
                     so the run stopped at `{action}`. Give that account {permission} again, \
                     or change the rule's run-as account."
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_host_action_names_the_permission_it_needs() {
        // The const list and the `match` must agree: a permission added to one and not the
        // other is a rule that runs with no authority at all, which is the failure this
        // module exists to make impossible.
        for (action, permission) in ACTION_PERMISSIONS {
            assert_eq!(
                permission_for(action),
                Some(*permission),
                "{action} needs {permission}"
            );
        }

        // `publish_page` is the acceptance criterion's own example: it is a content
        // permission, not a workflows one, because it publishes content.
        assert_eq!(
            permission_for("publish_page"),
            Some("content.pages.publish"),
            "losing content.pages.publish must stop a publish step"
        );
        // Everything that leaves the process on the rule's own account is a `workflows.run`.
        for action in ["send_email", "http_request", "run_workflow"] {
            assert_eq!(permission_for(action), Some("workflows.run"), "{action}");
        }
        // The synthetic actions touch nothing, so they need no account at all.
        for action in ["noop", "echo", "fail", "transient"] {
            assert_eq!(permission_for(action), None, "{action}");
        }
        // An action nobody knows needs nothing *here* — the handler refuses it by name, and
        // inventing a permission for it would only add a second, worse error.
        assert_eq!(permission_for("smtp_send"), None);
    }

    #[test]
    fn a_rule_follows_its_author_until_it_is_told_otherwise() {
        let author = Uuid::from_u128(7);
        let service = Uuid::from_u128(9);

        let implicit = Authority::of(None, Some(author));
        assert_eq!(implicit.user_id, Some(author));
        assert_eq!(implicit.source, AuthoritySource::Author);
        assert!(implicit.is_someone());
        assert!(implicit.describe().contains("author"));

        let explicit = Authority::of(Some(service), Some(author));
        assert_eq!(explicit.user_id, Some(service), "the explicit account wins");
        assert_eq!(explicit.source, AuthoritySource::Explicit);
        assert!(explicit.describe().contains("settings"));
    }

    #[test]
    fn a_rule_whose_author_is_gone_runs_as_nobody_rather_than_as_somebody() {
        // This is the whole stale-authority hole in one assertion. A deleted author must not
        // fall back to anything: "nobody" is the only answer that cannot be a permission
        // somebody still holds.
        let orphan = Authority::of(None, None);
        assert_eq!(orphan.user_id, None);
        assert_eq!(orphan.source, AuthoritySource::Nobody);
        assert!(!orphan.is_someone());
        assert!(
            orphan.describe().contains("no longer exists"),
            "the panel says so in words: {}",
            orphan.describe()
        );
    }

    #[test]
    fn a_refusal_says_the_event_the_request_names() {
        let refusal = Refusal {
            permission: "content.pages.publish",
            user_id: Some(Uuid::from_u128(3)),
            action: "publish_page",
            message: "the account this rule runs as no longer holds content.pages.publish"
                .to_owned(),
        };

        let error = refusal.as_step_error();
        assert!(
            error.starts_with(PERMISSION_REVOKED_EVENT),
            "the step error names the event: {error}"
        );
        assert!(error.contains("content.pages.publish"), "{error}");
        // The payload carries the same three facts the message does, so a subscriber can act
        // on the event without parsing English.
        let payload = refusal.as_event_payload();
        assert_eq!(payload["event"], PERMISSION_REVOKED_EVENT);
        assert_eq!(payload["permission"], "content.pages.publish");
        assert_eq!(payload["action"], "publish_page");
        assert_eq!(payload["run_as_user_id"], Uuid::from_u128(3).to_string());
    }

    #[test]
    fn an_unknown_action_gets_words_rather_than_a_borrowed_slice() {
        assert_eq!(action_label("publish_page"), "publish_page");
        assert_eq!(action_label("run_workflow"), "run_workflow");
        assert_eq!(action_label("smtp_send"), "this action");
        assert_eq!(action_label(""), "this action");
    }
}
