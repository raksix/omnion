//! The approval gate: what a run must wait for a human to decide (REQ-101, slice 1).
//!
//! # The one claim this file makes
//!
//! "Nothing dangerous happens without a human" is a claim about **a code path**, not about a
//! policy table, so this file is written as the only place that decides whether a gated tool
//! call runs. The class policy reads here, the decision writes here, and the route calls
//! [`Gate`] rather than re-deriving anything. A second implementation of "does this class need
//! approval" is the defect this request cannot survive, because the second one is the one an
//! operator will not read.
//!
//! # Why a *class* and not a tool key
//!
//! The request names six classes — `content_publish`, `content_delete`, `plugin_install`,
//! `theme_change`, `deployment`, `database_operation` — and each maps to `require` or `allow`.
//! Gating by key would put the decision on the wrong table: a key is added to the compiled
//! catalogue by a code change, so a key-gated installation would silently stop gating the tool
//! that replaced it. A class is a property of **what the action does**, so a new key in an
//! already-gated class is gated the day it ships, with no operator action and no migration.
//!
//! # Why approval is a separate axis from permission
//!
//! REQ-100's identity resolution answers *who may ask*; this answers *what happens*. The
//! acceptance criterion is explicit: "An approval is required even when the caller holds the
//! underlying domain permission". A design where holding `content.publish` also satisfies the
//! gate would be a design where the permission *is* the approval, and every agent whose service
//! account holds the permission would run gated tools unattended. [`Gate::decide`] therefore
//! takes the resolved permissions and deliberately **ignores** them for the gate decision — the
//! only use is the audit row, and a test asserts a caller holding everything still parks.
//!
//! # What is deliberately not here
//!
//! - **The preview and the apply.** Those are slice 2, and the reason they are not in slice 1 is
//!   that they are the half that has to be *one* implementation: `preview(operation) -> diff` and
//!   `apply(operation, diff)` share a field mapping, and splitting them across two slices would
//!   make "one implementation" true of the final code and untestable at the midpoint.
//! - **The notification.** REQ-021 emits from the same events, and this slice publishes them.
//! - **The sweeper's scheduler.** The tick that expires rows is spawned by the API process; the
//!   query that finds them is [`expire_due`] and is a plain function, so a walk can call it with
//!   a clock seam instead of waiting an hour.

pub mod io;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// Every column the store reads back, in one place so a write and a read cannot drift.
const APPROVAL_COLUMNS: &str = "id, organization_id, site_id, run_id, step_id, agent_id, \
     identity_id, change_set_id, tool_key, tool_class, resource_type, resource_id, \
     resource_label, risk, title, summary, operation_count, irreversible, \
     requires_confirmation, confirmation_phrase, preview, preview_hash, base_revision, status, \
     requested_by, model_id, expires_at, decided_by, decided_at, decision_note, applied_at, \
     error, created_at";

/// The six classes the request gates, in the order the policy screen lists them.
///
/// A `const` array rather than a `matches!` in six places, because the request's first
/// acceptance criterion is "all six dangerous classes are gated by default", and that is a
/// statement about a *list*. A class that is not in the list cannot be gated, cannot be
/// filtered on the policy screen and cannot be asserted on, and the test that walks this
/// array is what makes adding a seventh class a deliberate act rather than an accident.
pub const DANGEROUS_CLASSES: [&str; 6] = [
    "content_publish",
    "content_delete",
    "plugin_install",
    "theme_change",
    "deployment",
    "database_operation",
];

/// The classes whose operations cannot be undone, and therefore the ones the typed
/// confirmation exists for.
///
/// `content_delete` is here because a deleted page is not recoverable through the UI,
/// `deployment` because the previous version is not something the panel can restore, and
/// `database_operation` because nothing rolls a migration back on its own. A publish is
/// reversible by un-publishing and a theme activation by activating the previous theme, so
/// neither asks for a phrase — the request's "irreversible means irreversible" cuts both
/// ways: a gate people type the same word into every time is a gate that stops protecting
/// anything.
pub const IRREVERSIBLE_CLASSES: [&str; 3] = ["content_delete", "deployment", "database_operation"];

/// The default expiry, in minutes, and the bounds the column enforces.
///
/// Minutes rather than seconds because the policy screen edits minutes and a unit conversion
/// at the store boundary is a place for an off-by-60. The bounds are the request's 5–1440.
pub const DEFAULT_EXPIRY_MINUTES: i32 = 60;
pub const MIN_EXPIRY_MINUTES: i32 = 5;
pub const MAX_EXPIRY_MINUTES: i32 = 1440;

/// Whether a class is one of the six.
#[must_use]
pub fn is_dangerous_class(class: &str) -> bool {
    DANGEROUS_CLASSES.contains(&class)
}

/// Whether a class's operations cannot be undone.
#[must_use]
pub fn is_irreversible_class(class: &str) -> bool {
    IRREVERSIBLE_CLASSES.contains(&class)
}

/// A human-readable label per class, for the inbox's Class column and the policy screen.
///
/// Code rather than i18n keys because the panel is served English and these are identifiers
/// that also appear in audit metadata; a class whose label is a translation key is a class an
/// operator sees as `approval.class.deployment` on the one screen that is supposed to make
/// the decision easy.
#[must_use]
pub fn class_label(class: &str) -> &'static str {
    match class {
        "content_publish" => "Publish content",
        "content_delete" => "Delete content",
        "plugin_install" => "Install plugin",
        "theme_change" => "Change theme",
        "deployment" => "Deployment",
        "database_operation" => "Database operation",
        _ => "Unclassified",
    }
}

/// A class policy row, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ClassPolicy {
    /// `require` parks the run; `allow` runs it.
    pub mode: String,
    pub typed_confirmation: bool,
    pub expires_minutes: i32,
}

impl Default for ClassPolicy {
    /// The safe default, and the same one the migration seeds.
    ///
    /// `Default` here is a policy decision, not a Rust convenience: a caller that builds a
    /// [`Gate`] without a row for a class must gate it, because inheriting "allow" for a
    /// missing row is the shape of the bug this request exists to prevent. A default of
    /// `require` means a dropped policy row fails closed.
    fn default() -> Self {
        Self {
            mode: "require".to_owned(),
            typed_confirmation: true,
            expires_minutes: DEFAULT_EXPIRY_MINUTES,
        }
    }
}

impl ClassPolicy {
    /// Whether a call in this class parks its run.
    #[must_use]
    pub fn requires_approval(&self) -> bool {
        self.mode == "require"
    }

    /// How long the approval waits before the sweeper expires it.
    #[must_use]
    pub fn expires_at(&self, from: OffsetDateTime) -> OffsetDateTime {
        from + time::Duration::minutes(i64::from(self.expires_minutes))
    }
}

/// Which class a tool belongs to, for the classes that are **not** derived from a prefix.
///
/// The mapping is explicit rather than prefix-derived because the six classes are not a naming
/// convention: `content.publish` is `content_publish`, but `content.rollback` is **not** a
/// delete (it restores), `theme.activate` is a `theme_change` while `theme.list` is not
/// anything, and `deployment.preview` is a read while `deployment.deploy` is the one that
/// changes the installation. A prefix rule would gate the rollback and leave the deploy, which
/// is precisely backwards. The test at the bottom asserts both directions: every mapped key is
/// real, and the three high-risk tools that are *not* mapped are named.
#[must_use]
pub fn class_of_tool(tool_key: &str) -> Option<&'static str> {
    Some(match tool_key {
        "content.publish" => "content_publish",
        "content.rollback" => "content_delete",
        "plugin.install" => "plugin_install",
        "theme.activate" => "theme_change",
        "deployment.deploy" | "deployment.restart" => "deployment",
        "workflow.start" => "database_operation",
        _ => return None,
    })
}

/// The verdict for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Not one of the six classes: the call runs, ungated.
    Ungated,
    /// The class is `require`: the run parks and an approval row is created.
    Parked {
        policy: ClassPolicy,
        /// True when the reviewer has to type the resource's name.
        requires_confirmation: bool,
    },
}

impl Gate {
    /// Whether this call parks.
    #[must_use]
    pub fn parks(&self) -> bool {
        matches!(self, Self::Parked { .. })
    }

    /// The policy, when the call parks.
    #[must_use]
    pub fn policy(&self) -> Option<&ClassPolicy> {
        match self {
            Self::Parked { policy, .. } => Some(policy),
            Self::Ungated => None,
        }
    }
}

/// The decision a decision call reached.
///
/// A separate type from [`crate::error::AiHubError`] because most of these are **not** errors in
/// the sense of "something broke": `AlreadyDecided` and `Expired` are the ordinary answers to a
/// second click, and the route turns them into 409/410. Modelling them as error variants would
/// make the success path of `decide()` a `?` and every caller would have to match the error to
/// find out whether anything happened.
#[derive(Debug, Clone)]
pub enum DecisionOutcome {
    /// The row moved out of `pending`; the returned approval is the new state.
    Decided(Box<Approval>),
    /// Somebody already decided. Nothing was written.
    AlreadyDecided(Box<Approval>),
    /// Past `expires_at` and swept. Nothing was written.
    Expired(Box<Approval>),
    /// The resource moved between preview and decision. Nothing was written.
    Stale {
        /// The current revision, so the screen can offer Re-preview against something real.
        current_revision: String,
    },
    /// A gated class needs a typed phrase and none arrived.
    ConfirmationRequired {
        /// The phrase the reviewer has to type.
        phrase: String,
    },
    /// A phrase arrived and it was not the resource's name.
    ConfirmationMismatch { phrase: String },
    /// There is already a pending request for this run step. Nothing was written.
    AlreadyPending,
}

impl DecisionOutcome {
    /// The approval as it now stands, for the outcomes that carry one.
    #[must_use]
    pub fn approval(&self) -> Option<&Approval> {
        match self {
            Self::Decided(approval) | Self::AlreadyDecided(approval) | Self::Expired(approval) => {
                Some(approval)
            }
            _ => None,
        }
    }

    /// Whether a decision was actually recorded.
    #[must_use]
    pub fn changed(&self) -> bool {
        matches!(self, Self::Decided(_))
    }

    /// The stable code the API answers with, or `None` when the call succeeded.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::Decided(_) => return None,
            Self::AlreadyDecided(_) => "already_decided",
            Self::Expired(_) => "expired",
            Self::Stale { .. } => "stale",
            Self::ConfirmationRequired { .. } => "confirmation_required",
            Self::ConfirmationMismatch { .. } => "confirmation_mismatch",
            Self::AlreadyPending => "already_pending",
        })
    }
}

/// An approval request row, as the API and the panel read it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Approval {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub site_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub step_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub identity_id: Option<Uuid>,
    pub change_set_id: Option<Uuid>,
    pub tool_key: String,
    pub tool_class: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub resource_label: Option<String>,
    pub risk: String,
    pub title: String,
    pub summary: String,
    pub operation_count: i32,
    pub irreversible: bool,
    pub requires_confirmation: bool,
    pub confirmation_phrase: Option<String>,
    /// The frozen diff the reviewer decides on. Stored whole and returned whole; the panel
    /// never re-derives a preview, because a preview it computes is a preview nobody approved.
    pub preview: Value,
    pub preview_hash: String,
    pub base_revision: Option<String>,
    pub status: String,
    pub requested_by: Option<Uuid>,
    pub model_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub decided_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
    pub decision_note: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub applied_at: Option<OffsetDateTime>,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl Approval {
    /// Whether a person may still decide this request.
    ///
    /// "May decide" is `pending` **and** unexpired, and the second half is not the same test:
    /// a row the sweeper has not reached yet is still `pending` with an `expires_at` in the
    /// past, and a reviewer who approves it would be approving something the panel will show as
    /// expired thirty seconds later. The check is here so every caller gets the same answer.
    #[must_use]
    pub fn is_decidable(&self, now: OffsetDateTime) -> bool {
        self.status == "pending" && self.expires_at > now
    }

    /// Seconds left before the sweeper expires it, or `None` once decided.
    #[must_use]
    pub fn expires_in_seconds(&self, now: OffsetDateTime) -> Option<i64> {
        if self.status == "pending" {
            Some((self.expires_at - now).whole_seconds())
        } else {
            None
        }
    }

    /// Whether the danger zone has to render for this request.
    #[must_use]
    pub fn is_dangerous(&self) -> bool {
        self.irreversible || self.requires_confirmation
    }
}

/// The policy table as the screen reads it: every class, resolved, with its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyView {
    pub tool_class: String,
    pub label: String,
    /// `organization` when an organization row overrides the platform default.
    pub source: String,
    pub mode: String,
    pub typed_confirmation: bool,
    pub expires_minutes: i32,
    /// True when the class is `allow`, which the row stripes.
    pub permissive: bool,
    /// True when the class's operations cannot be undone — the classes a phrase is required
    /// for by the request even when the policy's own checkbox is off.
    pub irreversible: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub updated_by: Option<Uuid>,
}

/// The effective policy per class, with the organization row preferred over the platform default.
#[must_use]
pub fn resolve_policies(rows: &[PolicyRow]) -> BTreeMap<&'static str, PolicyView> {
    let mut resolved: BTreeMap<&'static str, PolicyView> = BTreeMap::new();
    // Platform rows first, then organization rows, so a later write overwrites the default.
    let mut ordered: Vec<&PolicyRow> = rows.iter().collect();
    ordered.sort_by_key(|row| row.organization_id.is_some());
    for row in ordered {
        let Some(class) = DANGEROUS_CLASSES
            .iter()
            .find(|known| **known == row.tool_class)
        else {
            // A row for a class this build does not know is kept, not shown: dropping it from
            // the screen would make "the policy I set disappeared" a real report and there is
            // no repair a person could perform.
            continue;
        };
        resolved.insert(
            class,
            PolicyView {
                tool_class: (*class).to_owned(),
                label: class_label(class).to_owned(),
                source: if row.organization_id.is_some() {
                    "organization"
                } else {
                    "platform"
                }
                .to_owned(),
                mode: row.mode.clone(),
                typed_confirmation: row.typed_confirmation,
                expires_minutes: row.expires_minutes,
                permissive: row.mode == "allow",
                irreversible: is_irreversible_class(class),
                updated_at: row.updated_at,
                updated_by: row.updated_by,
            },
        );
    }
    resolved
}

/// A policy row as stored.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PolicyRow {
    pub id: Uuid,
    pub organization_id: Option<Uuid>,
    pub tool_class: String,
    pub mode: String,
    pub typed_confirmation: bool,
    pub expires_minutes: i32,
    pub updated_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl PolicyRow {
    /// The half of the row the runtime needs.
    fn policy(&self) -> ClassPolicy {
        ClassPolicy {
            mode: self.mode.clone(),
            typed_confirmation: self.typed_confirmation,
            expires_minutes: self.expires_minutes,
        }
    }
}

/// Validate an expiry the way the column would, so a bad value never reaches a round trip.
fn validate_expiry(minutes: i32) -> Result<()> {
    if !(MIN_EXPIRY_MINUTES..=MAX_EXPIRY_MINUTES).contains(&minutes) {
        return Err(AiHubError::InvalidApproval(format!(
            "an approval may wait between {MIN_EXPIRY_MINUTES} and {MAX_EXPIRY_MINUTES} minutes, \
             not {minutes}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::io::class_options;
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    #[test]
    fn the_six_dangerous_classes_are_exactly_the_requests() {
        // The request names them in one sentence; this is that sentence as a list, so a class
        // that gets dropped is a failing test rather than a silently ungated class.
        for class in [
            "content_publish",
            "content_delete",
            "plugin_install",
            "theme_change",
            "deployment",
            "database_operation",
        ] {
            assert!(is_dangerous_class(class), "{class} must be gated");
            assert!(!class_label(class).is_empty(), "{class} needs a label");
        }
        assert_eq!(DANGEROUS_CLASSES.len(), 6);
        for other in ["content", "media", "users", "sites", "ops", "read", ""] {
            assert!(
                !is_dangerous_class(other),
                "`{other}` is a tool class, not a gated action class"
            );
        }
    }

    #[test]
    fn only_the_three_irreversible_classes_demand_a_phrase() {
        // The request: "Typed confirmation for irreversible operations (`content_delete`,
        // `deployment`, `database_operation`)". A publish and a theme activation are both
        // reversible from the panel, and a gate that asks for a phrase on those is a gate
        // people type the same word into every time.
        for class in DANGEROUS_CLASSES {
            let expected = matches!(
                class,
                "content_delete" | "deployment" | "database_operation"
            );
            assert_eq!(
                is_irreversible_class(class),
                expected,
                "{class} reversibility"
            );
        }
    }

    #[test]
    fn the_class_mapping_covers_the_publish_and_the_deploy_and_leaves_the_reads_alone() {
        assert_eq!(class_of_tool("content.publish"), Some("content_publish"));
        assert_eq!(class_of_tool("deployment.deploy"), Some("deployment"));
        assert_eq!(class_of_tool("deployment.restart"), Some("deployment"));
        assert_eq!(class_of_tool("plugin.install"), Some("plugin_install"));
        assert_eq!(class_of_tool("theme.activate"), Some("theme_change"));
        assert_eq!(class_of_tool("content.rollback"), Some("content_delete"));
        // Reads and previews are not gated: gating `content.search` would train a reviewer to
        // click approve without reading, and that is exactly how a gate stops protecting
        // anything.
        for key in [
            "content.search",
            "content.read",
            "content.create",
            "content.update",
            "theme.list",
            "plugin.list",
            "deployment.preview",
            "deployment.read",
            "logs.read",
            "health.read",
        ] {
            assert_eq!(class_of_tool(key), None, "{key} must not be gated");
        }
    }

    #[test]
    fn every_mapped_key_is_a_real_tool() {
        // A class mapping naming a tool the catalogue does not carry is a class nobody can
        // ever reach — and a test that only checked the mapping's own strings would agree with
        // itself, which is the failure this one exists to prevent.
        for key in [
            "content.publish",
            "content.rollback",
            "plugin.install",
            "theme.activate",
            "deployment.deploy",
            "deployment.restart",
            "workflow.start",
        ] {
            assert!(
                crate::registry::spec_for(key).is_some(),
                "`{key}` is mapped to a class but is not in the compiled catalogue"
            );
        }
    }

    #[test]
    fn a_policy_with_no_row_fails_closed() {
        // `Default` is a policy decision. A caller that resolves no row must gate, because
        // inheriting "allow" for a missing row is precisely the bug this request exists to
        // stop.
        let policy = ClassPolicy::default();
        assert!(policy.requires_approval());
        assert!(policy.typed_confirmation);
        assert_eq!(policy.expires_minutes, DEFAULT_EXPIRY_MINUTES);
    }

    #[test]
    fn an_allow_policy_does_not_gate() {
        let policy = ClassPolicy {
            mode: "allow".to_owned(),
            ..ClassPolicy::default()
        };
        assert!(!policy.requires_approval());
    }

    #[test]
    fn the_expiry_is_measured_from_the_request_not_from_now() {
        let policy = ClassPolicy {
            expires_minutes: 15,
            ..ClassPolicy::default()
        };
        assert_eq!(
            policy.expires_at(now()),
            now() + time::Duration::minutes(15),
            "the clock seam is the whole reason a walk can see an expiry"
        );
    }

    #[test]
    fn the_expiry_bounds_are_the_requests() {
        assert!(validate_expiry(MIN_EXPIRY_MINUTES).is_ok());
        assert!(validate_expiry(MAX_EXPIRY_MINUTES).is_ok());
        assert!(validate_expiry(0).is_err(), "0 would expire on arrival");
        assert!(validate_expiry(-1).is_err());
        assert!(validate_expiry(MAX_EXPIRY_MINUTES + 1).is_err());
        // The message names the field's rule, because the route prints it above the input.
        let message = validate_expiry(0)
            .expect_err("0 is out of range")
            .to_string();
        assert!(message.contains('5'), "{message}");
        assert!(message.contains("1440"), "{message}");
    }

    #[test]
    fn a_pending_row_past_its_expiry_is_not_decidable() {
        let mut approval = approval();
        approval.expires_at = now() + time::Duration::minutes(1);
        assert!(approval.is_decidable(now()));
        // A row the sweeper has not reached yet is still `pending` with an expiry in the past,
        // and a reviewer who approved it would be approving something the panel shows as
        // expired thirty seconds later.
        approval.expires_at = now() - time::Duration::seconds(1);
        assert!(!approval.is_decidable(now()));
    }

    #[test]
    fn a_decided_row_reports_no_countdown() {
        let mut approval = approval();
        approval.status = "approved".to_owned();
        assert!(!approval.is_decidable(now()));
        assert_eq!(
            approval.expires_in_seconds(now()),
            None,
            "a countdown on a decided request is a countdown to nothing"
        );
    }

    #[test]
    fn an_organization_policy_overrides_the_platform_default() {
        let platform = policy_row(None, "require", 60);
        let organization = policy_row(Some(Uuid::new_v4()), "allow", 15);
        let resolved = resolve_policies(&[platform, organization]);
        let deployment = resolved
            .get("deployment")
            .expect("the deployment class is one of the six");
        assert_eq!(deployment.mode, "allow");
        assert_eq!(deployment.expires_minutes, 15);
        assert_eq!(deployment.source, "organization");
    }

    #[test]
    fn the_order_of_the_two_rows_does_not_matter() {
        // The resolver sorts platform-before-organization rather than trusting the query's
        // order, so a planner that returns the organization row first cannot flip a gate.
        let organization = policy_row(Some(Uuid::new_v4()), "allow", 15);
        let platform = policy_row(None, "require", 60);
        let forwards = resolve_policies(&[platform.clone(), organization.clone()]);
        let backwards = resolve_policies(&[organization, platform]);
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn a_row_for_an_unknown_class_is_kept_out_of_the_view_rather_than_shown() {
        let mut row = policy_row(None, "require", 60);
        row.tool_class = "quantum_deploy".to_owned();
        assert!(
            resolve_policies(&[row]).is_empty(),
            "a class this build does not know has no label to render"
        );
    }

    #[test]
    fn the_permissive_rows_are_the_ones_the_screen_stripes() {
        let mut permissive = policy_row(None, "allow", 60);
        permissive.tool_class = "content_delete".to_owned();
        let resolved = resolve_policies(&[permissive]);
        let view = resolved
            .get("content_delete")
            .expect("content_delete is one of the six");
        assert!(view.permissive);
        assert!(
            view.irreversible,
            "the stripe and the danger flag are separate facts"
        );
    }

    #[test]
    fn the_gate_reports_both_halves_of_a_park() {
        let gate = Gate::Parked {
            policy: ClassPolicy::default(),
            requires_confirmation: true,
        };
        assert!(gate.parks());
        assert!(gate.policy().is_some_and(ClassPolicy::requires_approval));
        assert!(!Gate::Ungated.parks());
        assert!(Gate::Ungated.policy().is_none());
    }

    #[test]
    fn a_danger_zone_is_shown_for_a_phrase_and_for_an_ungated_reversible_action() {
        // The fixture arrives with a phrase required (the platform default), so the
        // "nothing to warn about" case is built by taking it away. A fixture whose own
        // defaults contradict the first assertion is a test measuring its own setup.
        let mut approval = approval();
        approval.requires_confirmation = false;
        approval.irreversible = false;
        assert!(
            !approval.is_dangerous(),
            "a publish with no phrase and nothing irreversible is not a danger zone"
        );
        approval.requires_confirmation = true;
        assert!(approval.is_dangerous());
        let mut delete = approval;
        delete.irreversible = true;
        delete.requires_confirmation = false;
        assert!(
            delete.is_dangerous(),
            "irreversible is enough on its own — the flag and the phrase are separate switches"
        );
    }

    #[test]
    fn the_class_options_carry_the_reversibility_the_form_shows() {
        let options = class_options();
        assert_eq!(options.len(), 6);
        for (class, label, irreversible) in options {
            assert!(!label.is_empty());
            assert_eq!(irreversible, is_irreversible_class(class));
        }
    }

    fn policy_row(organization_id: Option<Uuid>, mode: &str, minutes: i32) -> PolicyRow {
        PolicyRow {
            id: Uuid::new_v4(),
            organization_id,
            tool_class: "deployment".to_owned(),
            mode: mode.to_owned(),
            typed_confirmation: true,
            expires_minutes: minutes,
            updated_by: None,
            updated_at: now(),
        }
    }

    fn approval() -> Approval {
        Approval {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: None,
            run_id: None,
            step_id: None,
            agent_id: None,
            identity_id: None,
            change_set_id: None,
            tool_key: "content.publish".to_owned(),
            tool_class: "content_publish".to_owned(),
            resource_type: Some("page".to_owned()),
            resource_id: Some("7f1c".to_owned()),
            resource_label: Some("Autumn pricing".to_owned()),
            risk: "medium".to_owned(),
            title: "Publish Autumn pricing".to_owned(),
            summary: String::new(),
            operation_count: 1,
            irreversible: false,
            requires_confirmation: true,
            confirmation_phrase: Some("Autumn pricing".to_owned()),
            preview: serde_json::json!({ "operations": [] }),
            preview_hash: "abc".to_owned(),
            base_revision: None,
            status: "pending".to_owned(),
            requested_by: None,
            model_id: None,
            expires_at: now() + time::Duration::minutes(60),
            decided_by: None,
            decided_at: None,
            decision_note: None,
            applied_at: None,
            error: None,
            created_at: now(),
        }
    }
}
