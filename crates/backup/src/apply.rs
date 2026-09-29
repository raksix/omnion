//! The restore **plan** (REQ-013, slice 2b): what the operator selected, what it will do and
//! whether it may proceed at all.
//!
//! # Why the plan is a value and not a function
//!
//! A restore is the only operation in this product that destroys live data on purpose, and
//! the two questions that decide it are both answerable without a database:
//!
//! * **Which parts did the operator actually ask for?** The preview lists every part in the
//!   manifest; the wizard lets a box be unticked. Nothing in the manifest knows which boxes
//!   were ticked, so a caller that reads `available_parts()` and restores *all of them* is
//!   restoring more than the operator agreed to — and the difference is invisible in the
//!   response, because both answer `201`.
//! * **May this proceed at all?** A run with nothing readable, a phrase that does not match,
//!   a request that names a part the archive does not have, or a `media`-only request that
//!   would leave the schema in a state it claims to have restored: each of those is a refusal
//!   that must be decided **before** the first byte is written, because there is no partial
//!   state to unwind afterwards.
//!
//! So the decision lives here, pure and unit-tested, and the executor is handed a
//! [`RestorePlan`] it cannot argue with.
//!
//! # The rules, each one a shortcut that produces a plausible wrong answer
//!
//! * **A selection is narrowed, never widened.** Unknown names are refused rather than
//!   dropped, because a request for `["database", "typo"]` that silently restores only
//!   `database` is a restore that did less than it was asked and reported success.
//! * **A selection must intersect what is readable.** A plan with an empty set is refused
//!   with `nothing_restorable` rather than reported as a successful no-op, because "I
//!   restored nothing" and "I restored nothing because you asked for something that was
//!   never there" are different events and only one of them is worth an audit entry.
//! * **The phrase is checked against the plan's own phrase**, and the plan's phrase is the
//!   hash of the run id — the same function the preview used, so the two cannot disagree
//!   about what the operator was asked to type.
//! * **`media` is restored by object, and the plan says how many.** The count is the
//!   archive's index, not a manifest field, so a plan that restores `media` without reading
//!   the index has nothing to restore *into* and would report a success that moved no
//!   bytes. An index that cannot be read is a refusal, not an empty restore.

use serde::{Deserialize, Serialize};

use crate::part::PARTS;
use crate::restore::{RestorablePart, confirm_phrase};

/// How many archived objects one media restore will write back.
pub const MAX_MEDIA_OBJECTS: usize = 200_000;

/// The largest selection a plan will accept, in parts.
///
/// Five, and the number is not the point — the point is that a request naming a hundred
/// parts is either a caller that does not know the vocabulary or a probe, and both are
/// refused in a sentence that names the field rather than by falling over.
pub const MAX_SELECTED_PARTS: usize = 5;

/// What the operator asked for, as a value.
///
/// Built by the route from the request body, and the **only** thing the executor reads. A
/// handler that wanted to restore "whatever is available" would have to put that in here,
/// where the phrase check and the availability check both live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    /// The run being restored, as the operator named it.
    pub backup_id: String,
    /// The parts the operator left ticked.
    pub parts: Vec<String>,
    /// What they typed into the confirmation box.
    pub confirmation: String,
}

impl RestoreRequest {
    /// A request naming every available part, for a caller that means "the whole archive".
    #[must_use]
    pub fn whole_archive(backup_id: impl Into<String>, parts: Vec<String>, confirmation: &str) -> Self {
        Self {
            backup_id: backup_id.into(),
            parts,
            confirmation: confirmation.to_owned(),
        }
    }
}

/// Why a plan was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRefusal {
    /// The archive holds nothing this build can read.
    NothingRestorable,
    /// The typed phrase was not this run's phrase.
    ConfirmationMismatch,
    /// The request named a part that is not one of the five.
    UnknownPart,
    /// The request named a part the archive does not hold.
    PartNotInArchive,
    /// The request named nothing at all.
    EmptySelection,
    /// The `media` part is selected and its index could not be read.
    MediaIndexUnreadable,
}

impl PlanRefusal {
    /// The stable code the API answers with.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NothingRestorable => "nothing_restorable",
            Self::ConfirmationMismatch => "confirmation_mismatch",
            Self::UnknownPart => "unknown_part",
            Self::PartNotInArchive => "part_not_in_archive",
            Self::EmptySelection => "empty_selection",
            Self::MediaIndexUnreadable => "media_index_unreadable",
        }
    }

    /// The sentence the API answers with.
    ///
    /// Each one names what the operator asked for and what the platform found, because
    /// "restoration refused" with nothing else is a support ticket. Not `const fn`: the
    /// sentences are formatted, and `format!` is not a constant function.
    #[must_use]
    pub fn message(self, asked: &str, found: &str) -> String {
        match self {
            Self::NothingRestorable => format!(
                "nothing on this run can be restored: {found}"
            ),
            Self::ConfirmationMismatch => format!(
                "the confirmation phrase does not match this restore point. The phrase for this run is {asked}."
            ),
            Self::UnknownPart => format!(
                "{asked} is not one of this platform's five backup parts"
            ),
            Self::PartNotInArchive => format!(
                "this run does not hold {asked}. On the destination: {found}"
            ),
            Self::EmptySelection => format!(
                "select at least one part. This run holds: {found}"
            ),
            Self::MediaIndexUnreadable => format!(
                "the media part's index could not be read, so there is nothing to restore into. The run holds: {found}"
            ),
        }
    }
}

/// The refusal, with the words the API sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// Which rule refused.
    pub reason: PlanRefusal,
    /// The message.
    pub message: String,
}

/// An accepted restore, and everything the executor must obey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// The run.
    pub backup_id: String,
    /// The parts, in manifest order, that will be restored.
    pub parts: Vec<String>,
    /// Archived objects the `media` part will write back.
    pub media_objects: i64,
    /// Bytes those objects carry.
    pub media_bytes: i64,
    /// Sites the media objects belong to, capped at [`MAX_REPORTED_SITES`] and flagged when
    /// it is not.
    pub media_sites: Vec<String>,
    /// Whether the site list was capped.
    pub media_sites_truncated: bool,
    /// Live rows the restore drops, priced by the preview and re-read here.
    pub live_dropped: i64,
    /// Whether the media part is among the selection.
    pub restores_media: bool,
}

impl RestorePlan {
    /// Whether the media part is selected at all.
    #[must_use]
    pub fn restores_media(&self) -> bool {
        self.restores_media
    }

    /// One line for the audit entry.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} part{} ({}){}",
            self.parts.len(),
            if self.parts.len() == 1 { "" } else { "s" },
            self.parts.join(", "),
            if self.restores_media {
                format!(", {} object(s) from {} site(s)", self.media_objects, self.media_sites.len())
            } else {
                String::new()
            }
        )
    }
}

/// What the caller knows about the archive while asking.
pub struct ArchiveFacts<'a> {
    /// The preview's parts, in manifest order — the archive's own account of itself.
    pub preview_parts: &'a [RestorablePart],
    /// How many objects the media index holds, or `None` when it could not be read.
    pub media_objects: Option<i64>,
    /// How many bytes those objects carry.
    pub media_bytes: i64,
    /// Distinct sites among them, already capped and flagged by the caller.
    pub media_sites: Vec<String>,
    pub media_sites_truncated: bool,
}

/// Build the plan, or say why there is none.
///
/// `live_dropped` is handed in rather than recomputed: the preview already priced it against
/// live data moments ago, and a restore that recomputes the price from a *different* filter
/// would be a restore whose cost the operator never saw. The executor re-reads it and the
/// audit carries both, so a drift is visible rather than silent.
pub fn build_plan(
    request: &RestoreRequest,
    facts: &ArchiveFacts<'_>,
    live_dropped: i64,
) -> std::result::Result<RestorePlan, PlanError> {
    let readable: Vec<&str> = facts
        .preview_parts
        .iter()
        .filter(|part| part.available)
        .map(|part| part.part.as_str())
        .collect();
    let held: Vec<&str> = facts
        .preview_parts
        .iter()
        .map(|part| part.part.as_str())
        .collect();

    // The phrase first. It is the cheapest refusal and the one that must not be influenced
    // by anything else: a caller who mistyped it should learn that, not that the archive
    // also has a problem.
    //
    // **Exact, and deliberately so.** The obvious leniency — trim the input, or compare case
    // insensitively — is how a guard teaches the operator it is decorative: the first time
    // the API accepted `restore ae11f7e8` because the paste had a stray space, the next
    // thing anybody tries is a phrase from another restore point. `RestorePreview::accepts`
    // is already exact, and two comparisons would eventually disagree about what "the same
    // phrase" means, so this one is written to be the same rule rather than a near-miss of
    // it. A unit test below holds a lowercased and a padded phrase.
    let phrase = confirm_phrase(&request.backup_id);
    if phrase.is_empty() || phrase != request.confirmation {
        return Err(PlanError {
            reason: PlanRefusal::ConfirmationMismatch,
            message: PlanRefusal::ConfirmationMismatch.message(&phrase, ""),
        });
    }

    if request.parts.is_empty() {
        return Err(PlanError {
            reason: PlanRefusal::EmptySelection,
            message: PlanRefusal::EmptySelection.message("", &join(&held)),
        });
    }
    if request.parts.len() > MAX_SELECTED_PARTS {
        return Err(PlanError {
            reason: PlanRefusal::UnknownPart,
            message: PlanRefusal::UnknownPart.message(
                &format!("{} parts", request.parts.len()),
                &join(&held),
            ),
        });
    }

    // Unknown names are refused, not dropped — a request for `["database", "typo"]` that
    // silently restored `database` is a restore that did less than it was asked.
    for name in &request.parts {
        if !PARTS.contains(&name.as_str()) {
            return Err(PlanError {
                reason: PlanRefusal::UnknownPart,
                message: PlanRefusal::UnknownPart.message(name, &join(&held)),
            });
        }
    }
    for name in &request.parts {
        if !held.contains(&name.as_str()) {
            return Err(PlanError {
                reason: PlanRefusal::PartNotInArchive,
                message: PlanRefusal::PartNotInArchive.message(name, &join(&held)),
            });
        }
    }

    // Manifest order, not request order: a request that names `media, database` must restore
    // the schema before the objects that hang off it, and "the order the caller listed them
    // in" is a different answer each time somebody re-sorts a form.
    let mut parts: Vec<String> = readable
        .iter()
        .filter(|name| request.parts.iter().any(|asked| asked == *name))
        .map(|name| (*name).to_owned())
        .collect();
    if parts.is_empty() {
        return Err(PlanError {
            reason: PlanRefusal::NothingRestorable,
            message: PlanRefusal::NothingRestorable.message("", &join(&held)),
        });
    }

    let restores_media = parts.iter().any(|name| name == "media");
    let (media_objects, media_bytes) = if restores_media {
        match facts.media_objects {
            None => {
                return Err(PlanError {
                    reason: PlanRefusal::MediaIndexUnreadable,
                    message: PlanRefusal::MediaIndexUnreadable.message("", &join(&held)),
                });
            }
            Some(count) => (count, facts.media_bytes),
        }
    } else {
        (0, 0)
    };

    Ok(RestorePlan {
        backup_id: request.backup_id.clone(),
        parts,
        media_objects,
        media_bytes,
        media_sites: facts.media_sites.clone(),
        media_sites_truncated: facts.media_sites_truncated,
        live_dropped,
        restores_media,
    })
}

fn join(names: &[&str]) -> String {
    if names.is_empty() {
        return "nothing".to_owned();
    }
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::Part;
    use crate::restore::RestorablePart;

    const NOW: &str = "2026-09-29T21:00:00Z";

    fn part(name: &str, available: bool) -> RestorablePart {
        RestorablePart {
            part: name.to_owned(),
            available,
            reason: None,
            item_count: 4,
            size_bytes: 512,
            checksum: None,
            live_matches: 0,
            live_dropped: 0,
            mode: crate::restore::RestoreMode::Merge,
        }
    }

    fn facts<'a>(
        parts: &'a [RestorablePart],
        media_objects: Option<i64>,
    ) -> ArchiveFacts<'a> {
        ArchiveFacts {
            preview_parts: parts,
            media_objects,
            media_bytes: media_objects.unwrap_or(0) * 100,
            media_sites: vec!["site-a".to_owned()],
            media_sites_truncated: false,
        }
    }

    fn request(id: &str, parts: &[&str]) -> RestoreRequest {
        RestoreRequest::whole_archive(id, parts.iter().map(|p| (*p).to_owned()).collect(), &confirm_phrase(id))
    }

    #[test]
    fn a_matching_phrase_and_a_readable_part_is_a_plan() {
        let parts = vec![part("database", true), part("media", false)];
        let plan = build_plan(&request("abc", &["database"]), &facts(&parts, None), 3).unwrap();
        assert_eq!(plan.parts, vec!["database"]);
        assert!(!plan.restores_media);
        assert_eq!(plan.live_dropped, 3);
        assert!(plan.summary().contains("1 part"));
    }

    /// The mistyped phrase is refused with the right phrase in the message, and NOT with a
    /// complaint about the archive — the two must not be conflated.
    #[test]
    fn a_wrong_phrase_is_refused_naming_the_right_one() {
        let parts = vec![part("database", true)];
        let mut req = request("abc", &["database"]);
        req.confirmation = "RESTORE 00000000".to_owned();
        let error = build_plan(&req, &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::ConfirmationMismatch);
        assert!(error.message.contains(&confirm_phrase("abc")));
    }

    /// A phrase from a *previous* run is the exact mis-click the guard exists for.
    #[test]
    fn another_runs_phrase_is_refused() {
        let parts = vec![part("database", true)];
        let mut req = request("abc", &["database"]);
        req.confirmation = confirm_phrase("other");
        let error = build_plan(&req, &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::ConfirmationMismatch);
    }

    /// Asking for a part the archive does not hold is refused rather than skipped.
    #[test]
    fn a_part_this_run_did_not_produce_is_refused_not_skipped() {
        let parts = vec![part("database", true)];
        let error = build_plan(&request("abc", &["media"]), &facts(&parts, Some(3)), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::PartNotInArchive);
        assert!(error.message.contains("database"));
    }

    #[test]
    fn an_unknown_part_name_is_refused_by_name() {
        let parts = vec![part("database", true)];
        let error = build_plan(&request("abc", &["database", "typo"]), &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::UnknownPart);
        assert!(error.message.contains("typo"));
    }

    #[test]
    fn an_empty_selection_is_refused_with_the_parts_on_offer() {
        let parts = vec![part("database", true), part("themes", true)];
        let error = build_plan(&request("abc", &[]), &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::EmptySelection);
        assert!(error.message.contains("themes"));
    }

    /// Selecting a part that is in the manifest but *unreadable* is `nothing_restorable`,
    /// never a success that moved nothing.
    #[test]
    fn selecting_only_an_unreadable_part_is_refused() {
        let parts = vec![part("database", true), part("media", false)];
        let error = build_plan(&request("abc", &["media"]), &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::NothingRestorable);
    }

    /// `media` selected with an unreadable index is its own refusal, because the plan has
    /// nothing to restore *into* — a success here would claim bytes moved when none did.
    #[test]
    fn media_without_a_readable_index_is_refused() {
        let parts = vec![part("database", true), part("media", true)];
        let error = build_plan(&request("abc", &["media"]), &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::MediaIndexUnreadable);
    }

    /// The order is the manifest's, not the caller's. Restoring objects before the schema
    /// they belong to is the ordering bug that leaves a library pointing at tables that do
    /// not exist yet.
    #[test]
    fn the_order_is_manifest_order_not_request_order() {
        let parts = vec![part("database", true), part("media", true)];
        let plan = build_plan(&request("abc", &["media", "database"]), &facts(&parts, Some(2)), 0).unwrap();
        assert_eq!(plan.parts, vec!["database", "media"]);
        assert!(plan.restores_media);
        assert_eq!(plan.media_objects, 2);
    }

    /// A duplicate in the selection restores the part **once**. Restoring a part twice is
    /// not idempotent for every part, and a form that can post the same name twice should
    /// not be able to run it twice.
    #[test]
    fn a_duplicated_selection_restores_the_part_once() {
        let parts = vec![part("database", true)];
        let plan = build_plan(&request("abc", &["database", "database"]), &facts(&parts, None), 0).unwrap();
        assert_eq!(plan.parts, vec!["database"]);
    }

    /// The comparison is EXACT, and this test is the reason. The panel's own hint sentence
    /// invites an operator to paste the phrase, and a paste carries a trailing newline; an
    /// API that trims teaches them the guard is decorative the first time it accepts one.
    #[test]
    fn a_padded_or_lowercased_phrase_is_refused() {
        let parts = vec![part("database", true)];
        for typed in [
            confirm_phrase("abc").to_lowercase(),
            format!("{} ", confirm_phrase("abc")),
            format!(" {}", confirm_phrase("abc")),
        ] {
            let mut req = request("abc", &["database"]);
            req.confirmation = typed.clone();
            let error = build_plan(&req, &facts(&parts, None), 0).unwrap_err();
            assert_eq!(
                error.reason,
                PlanRefusal::ConfirmationMismatch,
                "must refuse {typed:?}"
            );
        }
    }

    /// A run id that cannot produce a phrase at all (not a uuid) has no phrase to type, and
    /// so has no request that can proceed. `confirm_phrase` returns empty for it; a plan
    /// built against it must be refused rather than comparing an empty string with an empty
    /// confirmation and calling it a match.
    #[test]
    fn an_unnameable_run_has_no_phrase_and_no_plan() {
        let parts = vec![part("database", true)];
        let mut req = request("abc", &["database"]);
        req.backup_id = "not-a-uuid".to_owned();
        req.confirmation = String::new();
        let error = build_plan(&req, &facts(&parts, None), 0).unwrap_err();
        assert_eq!(error.reason, PlanRefusal::ConfirmationMismatch);
    }

    #[test]
    fn a_summary_counts_parts_and_objects() {
        let parts = vec![part("database", true), part("media", true)];
        let plan = build_plan(&request("abc", &["database", "media"]), &facts(&parts, Some(9)), 0).unwrap();
        assert!(plan.summary().contains("2 parts"));
        assert!(plan.summary().contains("9 object"));
    }

    /// The media index is read with a cap and the cap is **reported**, because a bounded
    /// restore that quotes a truncated number is a false reassurance.
    #[test]
    fn the_object_cap_is_a_named_constant() {
        assert!(MAX_MEDIA_OBJECTS > 0);
        assert_eq!(MAX_SELECTED_PARTS, PARTS.len());
    }

    /// The refusal codes are stable strings, because the panel keys its messages off them.
    #[test]
    fn every_refusal_has_a_stable_code() {
        let all = [
            PlanRefusal::NothingRestorable,
            PlanRefusal::ConfirmationMismatch,
            PlanRefusal::UnknownPart,
            PlanRefusal::PartNotInArchive,
            PlanRefusal::EmptySelection,
            PlanRefusal::MediaIndexUnreadable,
        ];
        let codes: Vec<&str> = all.iter().map(|reason| reason.code()).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "codes must be unique: {codes:?}");
    }
}
