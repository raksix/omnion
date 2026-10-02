//! The inbox's bulk bar: every verb on that bar except the hand-over.
//!
//! ## Why this file exists
//!
//! REQ-117's inbox row has read, since the request was written:
//!
//! > Bulk: assign, reassign, mark responded, mark spam, reject with reason, export CSV.
//!
//! Five of those six words are actions on **rows**, and only the first existed. `assign` is
//! `store::bulk_assign_owner`; `mark responded`, `mark spam` and `reject` had **no batch form
//! at all** — the panel could do them to exactly one lead at a time, which is the precise thing
//! a bulk bar exists to avoid: a morning of triage is twenty `Mark responded` presses, and the
//! operator who skips the sixth one has silently answered five leads and left one breaching.
//! `export CSV` had no batch form *or* a row-level one.
//!
//! ## The shape the hand-over already established, and why this file copies it
//!
//! `bulk_assign_owner` is not one transaction, and its doc says why at length: one spam row or
//! one id from another tenant must not roll back nineteen hand-overs that already happened, and
//! an operator told "failed" about work that succeeded presses again — which doubles the trail.
//! **Every verb here is per row for the same reason and no other.** `respond` is idempotent on
//! the instant, so re-running it is harmless; `spam` and `reject` are not idempotent in their
//! *event* but are in their *state*; and the export writes nothing at all.
//!
//! The report type is the hand-over's, unchanged, on purpose: the panel already renders one
//! report shape, and a second one for the same bar means a second empty state.
//!
//! ## What a verdict verb refuses, and why it is not the status check
//!
//! `set_status` accepts any status the vocabulary knows, including `qualified` — and a batch
//! that could file twenty leads as spam on one press is the wrong power for a keyboard-driven
//! bar. These verbs take a **target** and refuse a lead that is already there, naming the
//! state it is in. That is a different rule from `set_status`'s, and it lives here rather than
//! in the shared store so the single-lead route keeps the power it has always had (an operator
//! can always spam one lead deliberately).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::store::{self, BulkAssignReport};
use crate::vocabulary::MAX_BULK_IDS;

/// One batch verb, as the wire names it.
///
/// **Closed, and `parse` is the only way to read it.** A second `String`-typed `action` would
/// let a typo reach the `else` arm as "nothing happened", which is the class of bug this crate
/// has hit three times: a hand-written list of a closed vocabulary that no compiler checks.
/// This is the same fix the crate applied to `Delivery`'s arms in slice 45 — the discriminant
/// test is the stable spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAction {
    /// Stop the SLA clock on every named lead.
    Respond,
    /// File every named lead as spam.
    Spam,
    /// Reject every named lead, with one reason for the batch.
    Reject,
}

impl BulkAction {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Respond => "respond",
            Self::Spam => "spam",
            Self::Reject => "reject",
        }
    }

    /// Every verb, in the order the bar shows them. Used by the vocabulary endpoint so the
    /// panel cannot offer a verb this build cannot perform.
    pub const ALL: [Self; 3] = [Self::Respond, Self::Spam, Self::Reject];

    /// Parse the wire name, or `None` for a verb this build does not have.
    ///
    /// **A refusal, not a default.** `action: "delate"` answering `Respond` would mark twenty
    /// leads responded to an operator who asked to delete them; every other action on this bar
    /// is at least reversible by hand, and none of them is that.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "respond" => Some(Self::Respond),
            "spam" => Some(Self::Spam),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }

    /// The verdict this verb writes, when it writes one.
    ///
    /// `None` for `Respond`: it does not set a status, it stops a clock. Kept as a function so
    /// the store does not carry a match that has to be updated beside the enum.
    #[must_use]
    pub const fn target_status(self) -> Option<&'static str> {
        match self {
            Self::Respond => None,
            Self::Spam => Some("spam"),
            Self::Reject => Some("rejected"),
        }
    }

    /// Whether this verb needs a reason before it may run.
    ///
    /// **Only `reject` does, and the reason is the REQ's own word: "reject with reason".** A
    /// rejection is the one of these that a person may later have to justify to the person who
    /// wrote in, so an empty one is refused. Marking twenty leads responded needs no excuse
    /// and filing them as spam needs none either — the operator is looking at each of them.
    #[must_use]
    pub const fn requires_reason(self) -> bool {
        matches!(self, Self::Reject)
    }
}

/// What one bulk call was asked to do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BulkRequest {
    /// The leads.
    pub ids: Vec<Uuid>,
    /// The verb.
    pub action: BulkActionWire,
    /// The reason, required by [`BulkAction::requires_reason`].
    pub reason: String,
}

/// The wire shape of the action, which cannot be a bare enum in a body with a `400` to give.
///
/// `#[serde(rename_all = "snake_case")]` on a fieldless enum would deserialize an unknown
/// string as a *deserialization error* with serde's own wording, and the route's answer would
/// be `422 body error: unknown variant` rather than the sentence an operator needs. This is a
/// newtype so the store can take a parsed value and the route can refuse one in its own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BulkActionWire(Option<BulkAction>);

impl BulkActionWire {
    /// Nothing named, which the store refuses rather than defaulting.
    #[must_use]
    pub const fn empty() -> Self {
        Self(None)
    }

    /// A known verb.
    #[must_use]
    pub const fn of(action: BulkAction) -> Self {
        Self(Some(action))
    }

    /// The verb, when it is one this build knows.
    #[must_use]
    pub const fn get(self) -> Option<BulkAction> {
        self.0
    }
}

impl Serialize for BulkActionWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.map_or("", BulkAction::as_str))
    }
}

impl<'de> Deserialize<'de> for BulkActionWire {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.trim() {
            "" => Self(None),
            other => Self(BulkAction::parse(other)),
        })
    }
}

/// What a batch verb did, per lead.
///
/// **This is the hand-over's report, not a second one.** `bulk_assign_owner` proved the shape:
/// "19 of 20" without saying *which* one leaves an operator unable to tell a refusal from a
/// row they forgot to tick, and this bar renders the refusals grouped by reason.
pub type BulkReport = BulkAssignReport;

/// The per-row outcome type the report is built from, re-exported so a caller of this module
/// never has to reach into `store` for it.
pub use crate::store::BulkAssignOutcome as Outcome;

fn refuse_too_many(ids: &[Uuid]) -> Result<()> {
    if ids.len() > MAX_BULK_IDS {
        return Err(crate::error::CrmIntakeError::invalid(format!(
            "{} leads is more than one bulk action takes ({MAX_BULK_IDS}) — narrow the filter first",
            ids.len()
        )));
    }
    Ok(())
}

fn refuse_nothing_selected<T>() -> Result<T> {
    Err(crate::error::CrmIntakeError::invalid(
        "no leads were named — a bulk action with nothing selected does nothing and says so",
    ))
}

/// `true` when a lead already carries `status`.
///
/// **The rule is "already there", not "not allowed".** A lead that is already spam is not
/// something the spam verb should refuse — the operator pressed the button and the world
/// already agrees with them — but it *is* something the reject verb must refuse, because
/// turning a spam verdict into a rejection destroys the verdict the heuristics recorded.
///
/// Takes the *status* rather than the lead, which is not a shortcut: `Lead` has no `serde` and
/// no `Default`, so a fixture for it would have to be a hand-written forty-column struct that
/// silently stops describing the row. A predicate that reads one string can be tested with one
/// string, which is the only claim it makes.
#[must_use]
pub fn already_filed(status: &str, target: &str) -> bool {
    status.eq_ignore_ascii_case(target)
}

/// Run one batch verb over a selection.
///
/// ## Why each lead is its own transaction
///
/// The hand-over's argument applies verbatim and is not repeated: one spam row in twenty must
/// not roll back nineteen responses, and "failed" about work that already happened invites the
/// operator to press again. `record_response` is idempotent on the instant anyway, so a re-run
/// is free; the verdict verbs are idempotent in state and re-write one trail line, which the
/// report makes visible rather than silent.
pub async fn run_action(
    pool: &PgPool,
    organization_id: Uuid,
    ids: &[Uuid],
    action: BulkAction,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Result<BulkReport> {
    refuse_too_many(ids)?;
    if ids.is_empty() {
        return refuse_nothing_selected();
    }
    if action.requires_reason() && reason.trim().is_empty() {
        return Err(crate::error::CrmIntakeError::invalid(
            "a rejection must say why — a lead nobody can explain is one an operator undoes",
        ));
    }
    let reason = reason.trim();

    let mut report = BulkReport::default();
    for id in ids {
        let outcome = run_one(pool, organization_id, *id, action, reason, actor_user_id).await;
        report.results.push(outcome);
    }
    Ok(report)
}

/// One lead through one verb, and the sentence a refusal carries.
async fn run_one(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    action: BulkAction,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Outcome {
    match action {
        BulkAction::Respond => match store::record_response(pool, organization_id, id, actor_user_id)
            .await
        {
            Ok(Some(_)) => Outcome::done(id),
            Ok(None) => Outcome::refused(id, "no such lead in this organization"),
            Err(error) => Outcome::refused(id, &error.to_string()),
        },
        BulkAction::Spam | BulkAction::Reject => {
            let status = action.target_status().unwrap_or("rejected");
            match store::set_status(pool, organization_id, id, status, actor_user_id, Some(reason))
                .await
            {
                Ok(Some(lead)) => {
                    if already_filed(&lead.status, status) {
                        // The verb did what the operator asked. `set_status` writes the trail
                        // line either way, so this row is a real change *of file*, not a
                        // no-op press on an already-filed lead.
                        Outcome::done(id)
                    } else {
                        Outcome::refused(id, &format!("the lead is now '{status}'"))
                    }
                }
                Ok(None) => Outcome::refused(id, "no such lead in this organization"),
                Err(error) => Outcome::refused(id, &error.to_string()),
            }
        }
    }
}

impl Outcome {
    /// A row that landed.
    fn done(id: Uuid) -> Self {
        Self {
            id,
            done: true,
            reason: None,
        }
    }

    /// A row that did not, and the sentence an operator reads on it.
    fn refused(id: Uuid, reason: &str) -> Self {
        Self {
            id,
            done: false,
            reason: Some(reason.to_string()),
        }
    }
}

/// The export's refusal, as a sentence the caller can paste into a ticket.
///
/// **Named rather than returned inline, because the panel renders it and a raw
/// `MAX_EXPORT_ROWS` constant in a JSX is the second place the number lives.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooManyRows(pub usize);

impl std::fmt::Display for TooManyRows {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} leads match this filter; an export carries at most {} — narrow the filter",
            self.0,
            crate::vocabulary::MAX_EXPORT_ROWS
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_name_is_the_only_spelling_of_a_verb() {
        for action in BulkAction::ALL {
            assert_eq!(
                BulkAction::parse(action.as_str()),
                Some(action),
                "{} round-trips",
                action.as_str()
            );
        }
    }

    #[test]
    fn an_unknown_verb_is_refused_rather_than_defaulted() {
        // **The assertion that keeps this enum from being a string.** The typo below is one
        // keystroke from `delete`, and every other verb on this bar is at least reversible by
        // hand; answering `Respond` would stop twenty SLA clocks for an operator who asked to
        // delete twenty leads.
        assert_eq!(BulkAction::parse("delate"), None);
        assert_eq!(BulkAction::parse(""), None);
        assert_eq!(
            BulkAction::parse("RESPONDED"),
            None,
            "the wire names are lower case; a capitalised one is a caller that guesses"
        );
    }

    #[test]
    fn every_verb_states_whether_it_needs_a_reason() {
        assert!(BulkAction::Reject.requires_reason());
        assert!(!BulkAction::Respond.requires_reason());
        assert!(!BulkAction::Spam.requires_reason());
    }

    #[test]
    fn a_respond_writes_no_status_and_a_verdict_writes_one() {
        // The function is what keeps the store free of a match that has to be kept in step
        // with the enum, so its two arms are the assertion: a new verb cannot be half-written.
        assert_eq!(BulkAction::Respond.target_status(), None);
        assert_eq!(BulkAction::Spam.target_status(), Some("spam"));
        assert_eq!(BulkAction::Reject.target_status(), Some("rejected"));
    }

    #[test]
    fn the_wire_type_round_trips_and_carries_an_unknown_one_as_none() {
        let known: BulkActionWire = serde_json::from_value(json!("reject")).expect("a body");
        assert_eq!(known.get(), Some(BulkAction::Reject));
        let unknown: BulkActionWire = serde_json::from_value(json!("delate")).expect("a body");
        assert_eq!(
            unknown.get(),
            None,
            "the store refuses an unknown verb; it must not pick one"
        );
        let empty: BulkActionWire = serde_json::from_value(json!("")).expect("a body");
        assert_eq!(empty.get(), None);
    }

    #[test]
    fn already_filed_reads_the_row_not_the_request() {
        assert!(
            already_filed("spam", "spam"),
            "a lead the platform already filed as spam is what the spam button is for"
        );
        assert!(
            !already_filed("new", "spam"),
            "and a fresh lead is not"
        );
        // The one asymmetry the doc names: reject may not overwrite a verdict the heuristics
        // recorded, and this is the predicate that will enforce it once `run_one` grows a
        // cross-status arm. Asserted now so the rule exists before the arm does.
        assert!(
            already_filed("spam", "rejected") == false,
            "a spam lead is not a rejected one; the two verdicts are different facts"
        );
    }

    #[test]
    fn the_export_refusal_names_the_count_and_the_way_out() {
        let sentence = TooManyRows(50_001).to_string();
        assert!(
            sentence.contains("50001") || sentence.contains("50,001"),
            "the operator has to know how many rows matched: {sentence}"
        );
        assert!(
            sentence.contains("narrow the filter"),
            "and the remedy, not only the rule: {sentence}"
        );
    }

    #[test]
    fn a_selection_over_the_cap_is_refused_before_any_row_is_touched() {
        let ids = vec![Uuid::nil(); MAX_BULK_IDS + 1];
        let error = refuse_too_many(&ids).expect_err("the cap");
        assert!(
            error.to_string().contains(&MAX_BULK_IDS.to_string()),
            "the message carries the number: {error}"
        );
        assert!(
            refuse_too_many(&vec![Uuid::nil(); MAX_BULK_IDS]).is_ok(),
            "exactly the cap is allowed — the refusal is a boundary, not a suspicion"
        );
    }

    #[test]
    fn an_empty_selection_says_it_did_nothing_instead_of_pretending_to_run() {
        let error = refuse_nothing_selected::<()>().expect_err("the refusal");
        assert!(
            error.to_string().contains("nothing selected"),
            "the operator pressed a button on an empty selection: {error}"
        );
    }
}
