//! The environment, the clone job and the promotion record — the shapes the API and the panel
//! agree on, and the state rules that are decisions rather than lookups.

use serde::{Deserialize, Serialize};

/// Whether an environment is the live one or a copy of it.
///
/// Two variants, not a boolean: the words appear in host names, event payloads and permission
/// checks, and a boolean called `is_staging` is a question that gets asked in the wrong direction
/// somewhere eventually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentType {
    /// The organization's live content. Exactly one per organization, created with it.
    Production,
    /// A content copy the panel can enter and later promote back.
    Staging,
}

impl EnvironmentType {
    /// The wire name, as stored and as sent to the panel.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Staging => "staging",
        }
    }

    /// Parse the stored form. `None` for anything the column's own check would have refused.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "production" => Some(Self::Production),
            "staging" => Some(Self::Staging),
            _ => None,
        }
    }

    /// Can this environment be the source of a clone?
    ///
    /// Only production can. A staging environment cloned into another staging environment would
    /// make the diff meaningless — there would be no fixed reference to differ from — which is why
    /// the wizard does not offer it and the API refuses it by name.
    pub fn can_be_clone_source(self) -> bool {
        matches!(self, Self::Production)
    }
}

/// Where an environment is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentStatus {
    /// Usable.
    Active,
    /// A clone is running against it. Reads still work; the content is mid-copy.
    Cloning,
    /// The last clone failed. The status names the failing area on the detail screen.
    Error,
    /// Content kept, host released. Takes no writes.
    Archived,
}

impl EnvironmentStatus {
    /// The wire name, as stored and as sent to the panel.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cloning => "cloning",
            Self::Error => "error",
            Self::Archived => "archived",
        }
    }

    /// Parse the stored form.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "cloning" => Some(Self::Cloning),
            "error" => Some(Self::Error),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }

    /// Does this status accept writes?
    ///
    /// `Cloning` does not: a write landing while a copy is in flight belongs to whichever copy
    /// wins, and the operator is told to retry rather than left to guess which content survived.
    pub fn accepts_writes(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// How far a clone job has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneStatus {
    /// Queued, not started.
    Pending,
    /// Copying.
    Running,
    /// Every area finished.
    Done,
    /// An area failed. The error names it.
    Failed,
    /// Stopped on request. Whatever had been copied stays; the environment is not marked active.
    Cancelled,
}

impl CloneStatus {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the stored form.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Is this job still going to change a counter?
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Where a promotion is in its approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionStatus {
    /// Frozen change set recorded, waiting for somebody else.
    PendingApproval,
    /// Approved, not yet applied.
    Approved,
    /// Applying to production.
    Running,
    /// Applied. The affected ids ride on the event.
    Done,
    /// Refused or broken. The error says which.
    Failed,
    /// Withdrawn before applying.
    Cancelled,
}

impl PromotionStatus {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the stored form.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending_approval" => Some(Self::PendingApproval),
            "approved" => Some(Self::Approved),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Does this status still wait for an approval?
    pub fn awaits_approval(self) -> bool {
        matches!(self, Self::PendingApproval)
    }

    /// The statuses a pending-only index should cover.
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::PendingApproval | Self::Running)
    }
}

/// How a row differs from production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Present in staging, absent in production.
    Added,
    /// Present in both, and changed since the clone.
    Updated,
    /// Present in production, gone from staging.
    Deleted,
}

impl ChangeKind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
        }
    }

    /// Parse the stored form.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "added" => Some(Self::Added),
            "updated" => Some(Self::Updated),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_round_trips_through_its_wire_name() {
        // A rename in the database that the parser does not know reads as "unset" everywhere.
        for raw in ["production", "staging"] {
            assert_eq!(EnvironmentType::parse(raw).unwrap().as_str(), raw);
        }
        for raw in ["active", "cloning", "error", "archived"] {
            assert_eq!(EnvironmentStatus::parse(raw).unwrap().as_str(), raw);
        }
        for raw in ["pending", "running", "done", "failed", "cancelled"] {
            assert_eq!(CloneStatus::parse(raw).unwrap().as_str(), raw);
        }
        for raw in [
            "pending_approval",
            "approved",
            "running",
            "done",
            "failed",
            "cancelled",
        ] {
            assert_eq!(PromotionStatus::parse(raw).unwrap().as_str(), raw);
        }
        for raw in ["added", "updated", "deleted"] {
            assert_eq!(ChangeKind::parse(raw).unwrap().as_str(), raw);
        }
    }

    #[test]
    fn an_unknown_wire_name_parses_to_nothing_rather_than_to_a_default() {
        // Defaulting an unknown status to `active` would hand an archived tenant write access.
        assert!(EnvironmentStatus::parse("frozen").is_none());
        assert!(PromotionStatus::parse("half_done").is_none());
        assert!(EnvironmentType::parse("preview").is_none());
    }

    #[test]
    fn only_production_is_a_clone_source() {
        assert!(EnvironmentType::Production.can_be_clone_source());
        assert!(
            !EnvironmentType::Staging.can_be_clone_source(),
            "a staging source would leave the diff with no fixed reference"
        );
    }

    #[test]
    fn only_active_takes_writes() {
        assert!(EnvironmentStatus::Active.accepts_writes());
        for status in [
            EnvironmentStatus::Cloning,
            EnvironmentStatus::Error,
            EnvironmentStatus::Archived,
        ] {
            assert!(!status.accepts_writes(), "{status:?} must refuse writes");
        }
    }

    #[test]
    fn an_open_clone_is_the_two_states_a_cancel_can_still_reach() {
        assert!(CloneStatus::Pending.is_open());
        assert!(CloneStatus::Running.is_open());
        for status in [
            CloneStatus::Done,
            CloneStatus::Failed,
            CloneStatus::Cancelled,
        ] {
            assert!(!status.is_open(), "{status:?} is finished");
        }
    }

    #[test]
    fn the_in_flight_set_is_exactly_what_the_partial_index_covers() {
        // The index in the migration is `where status in ('pending_approval','running')`; a
        // status that drifts out of the pair stops being indexed and the list slows down silently.
        for status in [
            PromotionStatus::PendingApproval,
            PromotionStatus::Approved,
            PromotionStatus::Running,
            PromotionStatus::Done,
            PromotionStatus::Failed,
            PromotionStatus::Cancelled,
        ] {
            assert_eq!(
                status.is_in_flight(),
                matches!(status.as_str(), "pending_approval" | "running"),
                "{status:?}"
            );
        }
    }
}
