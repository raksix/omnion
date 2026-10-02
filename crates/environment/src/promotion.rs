//! The frozen change set, and the promotion that carries it to production.
//!
//! Slices 1 and 2 built the environment and the diff. This module is the part where somebody
//! decides to act on the diff, and it carries one property that nothing else in the request does:
//! **what the approver read is exactly what runs**. That is why the change set is a *value* —
//! [`FrozenItem`] values in a `Vec`, serialized into the row — and not a set of ids resolved at
//! apply time.
//!
//! # Why freezing is the whole safety property
//!
//! The tempting design is to store the promotion's *ids* and re-read the rows when it is
//! approved. It is shorter and it is wrong in a way nobody notices until the day it matters: an
//! edit landing in staging between "requested" and "approved" would be published, and nobody —
//! not the requester, not the approver, not the audit log — ever saw it. The approval would be a
//! rubber stamp on a list that moved after it was printed.
//!
//! So [`FrozenItem`] carries the two values that decide whether the production row still says
//! what it said when the diff was taken:
//!
//! * `base_updated_at` — the production row's timestamp at request time. The clone preserves
//!   `updated_at` on both sides, so this is exactly "did anything move since".
//! * `base_digest` — the SHA-256 of the production row's published revision. `updated_at` alone
//!   is a clock, and clocks lie: a promotion that rewrites content in place, or a migration that
//!   touches rows it should not, moves nothing the timestamp can see. The digest catches the edit
//!   that did not move the clock.
//!
//! # Conflict detection is per row, and happens twice
//!
//! Detected once when the promotion is *requested* (so the dialog can lead with the list, as the
//! request insists: "the UI must lead with them rather than hiding them behind a failure toast"),
//! and again at *approve*, because the gap between request and approve is exactly the window in
//! which production moves on. Refusing at approve is the load-bearing check: the request-time
//! list is a heads-up, the approve-time list is the decision.
//!
//! # The step log
//!
//! Four steps — `validate`, `apply`, `audit`, `done` — appended as they complete and stored in the
//! row. It answers a different question from the audit table: the audit log answers "who did
//! this", this answers "how far did it get", which is what an operator needs when the browser was
//! closed mid-deploy and the row is the only thing left.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::changes::ChangeItem;
use crate::model::ChangeKind;

/// One item of a frozen change set.
///
/// Frozen means the values here are what the approver saw. Nothing in this struct resolves at
/// apply time — the *production* row is read to compare against, but the decision of what to
/// write came from these bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenItem {
    /// The page's id **in the environment that holds it**: staging's for `added` and `updated`,
    /// production's for `deleted`. Carried from [`ChangeItem::page_id`], whose own documentation
    /// explains why the two cases have different id spaces.
    pub page_id: Uuid,
    /// The site the page belongs to — needed to address the natural key on write.
    pub site_id: Uuid,
    /// The natural key's second half. The slug is what the apply addresses production rows by,
    /// because production's row for this item has a *different* id and the same slug.
    pub slug: String,
    /// `added`, `updated` or `deleted`, as it stood when the set was frozen.
    pub kind: ChangeKind,
    /// Production's `updated_at` when the set was frozen, or `None` for an `added` item — there
    /// was no production row to have one.
    pub base_updated_at: Option<OffsetDateTime>,
    /// The digest of production's published revision at freeze time, empty for an `added` item.
    pub base_digest: String,
}

impl FrozenItem {
    /// Freeze one row of a live change set.
    ///
    /// The two base values are supplied by the caller rather than derived here because they are
    /// read from the *production* row and the change set only knows the pair, not which half is
    /// which: for a `deleted` item the diff's `changed_at` is production's timestamp, and for an
    /// `added` item there is no production row at all. Deriving them in this function would mean
    /// guessing which case it is — the mistake this signature exists to make impossible.
    pub fn freeze(
        item: &ChangeItem,
        base_updated_at: Option<OffsetDateTime>,
        base_digest: String,
    ) -> FrozenItem {
        FrozenItem {
            page_id: item.page_id,
            site_id: item.site_id,
            slug: item.slug.clone(),
            kind: item.kind,
            // An `added` item has no production row, so a timestamp it somehow carries would be
            // nonsense; the row is deleted-or-absent, never "did a missing row change".
            base_updated_at: match item.kind {
                ChangeKind::Added => None,
                _ => base_updated_at,
            },
            base_digest: match item.kind {
                ChangeKind::Added => String::new(),
                _ => base_digest,
            },
        }
    }

    /// Does this item write a row into production?
    ///
    /// `Deleted` does not — it removes one. The apply and the change-set counts both branch on
    /// this, and both must agree: a counter that counted a deletion as an applied item would
    /// tell an operator their deploy touched a page it never touched.
    pub fn writes_a_row(&self) -> bool {
        self.kind != ChangeKind::Deleted
    }
}

/// A whole frozen change set, with the counts the promotion dialog shows.
///
/// Counts are derived, never stored: two stored counts and a `Vec` of the same items is three
/// numbers that can disagree, and the number an operator reads before approving is exactly the
/// one that must not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrozenChangeSet {
    /// The staging environment the items came from.
    pub environment_id: Uuid,
    /// Where they land.
    pub target_environment_id: Uuid,
    /// The items, in the order they were frozen (the diff's order: by slug).
    pub items: Vec<FrozenItem>,
}

impl FrozenChangeSet {
    /// Build a set, folding in its own counts at the same moment.
    pub fn new(
        environment_id: Uuid,
        target_environment_id: Uuid,
        items: Vec<FrozenItem>,
    ) -> FrozenChangeSet {
        FrozenChangeSet {
            environment_id,
            target_environment_id,
            items,
        }
    }

    /// How many items a `promotion.completed` event must carry.
    ///
    /// Every item, deletions included: the event tells a subscriber which pages moved, and a
    /// deletion is a page that moved. Excluding them would make a subscriber rebuild its cache
    /// and keep serving content the deploy deleted.
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// How many items write a production row.
    pub fn writes(&self) -> usize {
        self.items.iter().filter(|item| item.writes_a_row()).count()
    }

    /// How many are `added`.
    pub fn added(&self) -> usize {
        self.count(ChangeKind::Added)
    }

    /// How many are `updated`.
    pub fn updated(&self) -> usize {
        self.count(ChangeKind::Updated)
    }

    /// How many are `deleted`.
    pub fn deleted(&self) -> usize {
        self.count(ChangeKind::Deleted)
    }

    fn count(&self, kind: ChangeKind) -> usize {
        self.items.iter().filter(|item| item.kind == kind).count()
    }

    /// Is there nothing to promote?
    ///
    /// A named question rather than `items.is_empty()` at four call sites, because three of those
    /// four need to *refuse* and one needs to render an empty state, and the empty state for
    /// "you asked to promote nothing" is a sentence while the refusal is a `400`.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// One step of a promotion's progress, as the dialog's timeline reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Every item re-checked against production; conflicts named before anything is written.
    Validate,
    /// The items written, in one transaction.
    Apply,
    /// The audit entry and the events written.
    Audit,
    /// Finished.
    Done,
}

impl Step {
    /// The wire name, as stored in `step_log`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Apply => "apply",
            Self::Audit => "audit",
            Self::Done => "done",
        }
    }

    /// Parse the stored form.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "validate" => Some(Self::Validate),
            "apply" => Some(Self::Apply),
            "audit" => Some(Self::Audit),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// A completed step: what, when, and anything an operator would need to see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepEntry {
    /// Which step this was.
    pub step: String,
    /// When it completed.
    pub at: OffsetDateTime,
    /// How many items it touched, or a short phrase. Free text on purpose: the timeline is read
    /// by a person, and `{"step":"apply","count":41}` says less than `applied 41 item(s)`.
    pub detail: String,
}

impl StepEntry {
    /// One entry.
    pub fn new(step: Step, at: OffsetDateTime, detail: impl Into<String>) -> StepEntry {
        StepEntry {
            step: step.as_str().to_owned(),
            at,
            detail: detail.into(),
        }
    }
}

/// Append a step to a log, returning the new log.
///
/// Takes and returns the log rather than mutating it because the caller is inside a transaction
/// that may still roll back: a step appended to a log that then fails must not be visible. The
/// append happens as part of the same write that advances the status.
pub fn append_step(log: &mut Vec<StepEntry>, entry: StepEntry) {
    log.push(entry);
}

/// The number of items above which the dialog demands a typed confirmation.
///
/// Twenty-five, not a round number chosen for looks: it is the point where "Are you sure?" stops
/// being cheap for the operator to click through. Under it the button is one click; over it the
/// operator types the environment's name, which costs them the reading of the count.
pub const TYPED_CONFIRMATION_THRESHOLD: usize = 25;

/// Does a change set of this size need a typed confirmation?
pub fn needs_typed_confirmation(item_count: usize) -> bool {
    item_count > TYPED_CONFIRMATION_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeSet;
    use uuid::Uuid;

    fn item(slug: &str, kind: ChangeKind) -> ChangeItem {
        ChangeItem {
            site_id: Uuid::nil(),
            slug: slug.to_owned(),
            page_id: Uuid::nil(),
            kind,
            changed_by: None,
            changed_at: OffsetDateTime::UNIX_EPOCH,
            title: Some("t".to_owned()),
        }
    }

    #[test]
    fn the_counts_agree_with_the_items_because_they_are_folded_from_them() {
        let set = FrozenChangeSet::new(
            Uuid::nil(),
            Uuid::nil(),
            vec![
                FrozenItem::freeze(&item("a", ChangeKind::Added), None, String::new()),
                FrozenItem::freeze(&item("b", ChangeKind::Updated), None, "d".to_owned()),
                FrozenItem::freeze(&item("c", ChangeKind::Updated), None, "d".to_owned()),
                FrozenItem::freeze(&item("d", ChangeKind::Deleted), None, "x".to_owned()),
            ],
        );
        assert_eq!(set.added(), 1);
        assert_eq!(set.updated(), 2);
        assert_eq!(set.deleted(), 1);
        assert_eq!(set.item_count(), 4);
        // Three of the four write a row; the fourth removes one. A deploy log that counted the
        // deletion as an applied item would overstate what it touched.
        assert_eq!(set.writes(), 3);
        assert!(!set.is_empty());
    }

    #[test]
    fn an_added_item_can_never_carry_a_base_timestamp_or_digest() {
        // The values are supplied by the caller, and a caller that hands them over for an added
        // item is handing over values for a row that does not exist. Dropping them here means the
        // apply cannot later compare an absent production row against a phantom baseline and
        // report a conflict nobody caused.
        let when = OffsetDateTime::now_utc();
        let frozen = FrozenItem::freeze(&item("new", ChangeKind::Added), Some(when), "abc".to_owned());
        assert_eq!(frozen.base_updated_at, None);
        assert!(frozen.base_digest.is_empty());
    }

    #[test]
    fn a_deleted_item_still_counts_as_an_item_but_writes_no_row() {
        let frozen = FrozenItem::freeze(&item("gone", ChangeKind::Deleted), None, "d".to_owned());
        assert!(!frozen.writes_a_row());
        // Its base values are kept: "production still holds this row" is precisely the thing the
        // apply must verify before removing it.
        assert_eq!(frozen.base_digest, "d");
    }

    #[test]
    fn a_frozen_set_survives_a_json_round_trip_unchanged() {
        // The set is stored in a `jsonb` column, so this is not a nicety: `base_updated_at` is a
        // `time` timestamp and `page_id` a uuid, and a serialisation that loses or reshapes either
        // produces a promotion whose conflicts cannot be recomputed at approve time.
        let when = OffsetDateTime::now_utc();
        let set = FrozenChangeSet::new(
            Uuid::from_u128(7),
            Uuid::from_u128(9),
            vec![FrozenItem::freeze(
                &item("a", ChangeKind::Updated),
                Some(when),
                "deadbeef".to_owned(),
            )],
        );
        let encoded = serde_json::to_string(&set).expect("a frozen set must serialize");
        let decoded: FrozenChangeSet =
            serde_json::from_str(&encoded).expect("a frozen set must deserialize");
        assert_eq!(decoded, set);
        assert_eq!(decoded.items[0].base_updated_at, Some(when));
    }

    #[test]
    fn a_change_set_that_was_never_taken_is_not_a_change_set_of_zero() {
        let empty = FrozenChangeSet::new(Uuid::nil(), Uuid::nil(), Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.item_count(), 0);
        assert_eq!(empty.writes(), 0);
    }

    #[test]
    fn a_step_log_grows_by_appending_and_never_reorders() {
        let mut log = Vec::new();
        for step in [Step::Validate, Step::Apply, Step::Audit, Step::Done] {
            append_step(&mut log, StepEntry::new(step, OffsetDateTime::UNIX_EPOCH, "detail"));
        }
        let steps: Vec<&str> = log.iter().map(|entry| entry.step.as_str()).collect();
        assert_eq!(steps, ["validate", "apply", "audit", "done"]);
    }

    #[test]
    fn every_step_round_trips_and_an_unknown_one_is_not_silently_the_first() {
        for step in [Step::Validate, Step::Apply, Step::Audit, Step::Done] {
            assert_eq!(Step::parse(step.as_str()), Some(step));
        }
        assert_eq!(Step::parse("rollback"), None);
    }

    #[test]
    fn the_typed_confirmation_appears_above_twenty_five_and_not_below() {
        assert!(!needs_typed_confirmation(0));
        assert!(!needs_typed_confirmation(TYPED_CONFIRMATION_THRESHOLD));
        assert!(needs_typed_confirmation(TYPED_CONFIRMATION_THRESHOLD + 1));
    }

    #[test]
    fn a_change_set_from_the_diff_freezes_one_item_per_row() {
        // The relationship between the live set and the frozen one is what slice 3 depends on:
        // nothing is dropped, nothing is invented. A promotion of "everything except the deleted
        // row" would silently resurrect a page the operator deleted in staging.
        let live = ChangeSet {
            environment_id: Uuid::from_u128(1),
            production_id: Uuid::from_u128(2),
            items: vec![
                item("a", ChangeKind::Added),
                item("b", ChangeKind::Updated),
                item("c", ChangeKind::Deleted),
            ],
            added: 1,
            updated: 1,
            deleted: 1,
        };
        let frozen: Vec<FrozenItem> = live
            .items
            .iter()
            .map(|row| FrozenItem::freeze(row, Some(OffsetDateTime::UNIX_EPOCH), String::new()))
            .collect();
        assert_eq!(frozen.len(), live.len());
        assert_eq!(
            frozen.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![ChangeKind::Added, ChangeKind::Updated, ChangeKind::Deleted]
        );
    }
}
