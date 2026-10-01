//! The plan export document (docs/requests/REQ-045, slice 4).
//!
//! "Export plan JSON" is one line in the console's bulk bar and the only part of slice 4
//! that does not wait on another wave. This file is the document itself, kept **pure** —
//! no pool, no clock, no ids of its own — because an export that reads the clock is an
//! export nobody can assert on, and the acceptance criterion is about the file's shape.
//!
//! **Four decisions, each a shortcut that produces a plausible wrong file.**
//!
//! 1. **The document names its own version.** `schema: "omnion.app-builder.plan/1"` is the
//!    first key. An export whose format can change silently is a file a reader has to guess
//!    at, and the version is what makes the second format a *new* answer rather than a
//!    quietly different one. It is a string rather than an integer so a future `…/2` is
//!    readable without a spec.
//! 2. **Superseded artifacts are exported, not dropped.** A regenerated artifact's
//!    predecessor is still in the table and still explains why the current one looks the way
//!    it does; an export that kept only winners is an export of the answer without the
//!    reasoning. The `superseded_by` edge is drawn rather than inferred, because a reader
//!    cannot tell "replaced" from "never had a parent" out of two unlinked rows.
//! 3. **`spec` is the artifact verbatim and `validation` is the finding list verbatim.** An
//!    export that re-normalises either is a second normaliser, and the one that disagrees
//!    with the validator is the one nobody reads. This file never inspects `spec` at all.
//! 4. **A plan with no artifacts exports as a plan with no artifacts.** An empty
//!    `artifacts` array is the answer for a generation that failed or a draft nobody has
//!    reviewed; a document that refuses to be written is a download button that answers
//!    with an error the operator cannot act on.
//!
//! **The prompt is exported and the cost is not.** The prompt is the request — the thing
//! somebody has to read to know what the plan was *for* — and it is already visible on
//! screen to anybody who can call this endpoint, so withholding it protects nobody. What is
//! withheld is anything the platform cannot honestly state: there is no price source
//! anywhere in the tree (`ai_models` has no price column and REQ-104's `ai_spend_daily` does
//! not exist yet), so a fabricated currency figure would be worse than an absent one. The
//! token counts are exported because the provider reported them; `cost_cents` is exported
//! as the stored number, which is `0` until something can price it, and a reader that
//! multiplies tokens by a rate it invented is the reader's own arithmetic, not a claim this
//! platform made.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{BlockedArtifact, Result};
use crate::model::{AppBuilderArtifact, AppBuilderPlan, PlanCounts};

/// An instant as the file writes it.
///
/// `OffsetDateTime::format` is total — RFC 3339 admits every value `OffsetDateTime` holds —
/// so this returns a `String` rather than an error. A hand-rolled formatter here would be
/// one that silently drops a value on some input nobody tested.
fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

/// The document's format marker. Bump the suffix rather than the number: `…/1` is this
/// shape, and a second shape is `…/2` even if nothing about the first was removed.
pub const EXPORT_SCHEMA: &str = "omnion.app-builder.plan/1";

/// One plan as the export file writes it.
///
/// `Serialize` and `Deserialize` together on purpose: a test that reads the document back
/// out of `serde_json` proves the file round-trips, and a document type that cannot be read
/// back is one whose shape was never checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanExport {
    /// The format marker. Always [`EXPORT_SCHEMA`] on write.
    pub schema: String,
    /// Plan identity.
    pub id: Uuid,
    /// The request as it was typed.
    pub prompt: String,
    /// Human-readable name.
    pub title: String,
    /// Where the plan stands.
    pub status: String,
    /// Which attempt within a chain.
    pub plan_version: i32,
    /// The model that wrote it, frozen at generation.
    pub model_label: String,
    /// Tokens the provider reported, `null` when it reported none — distinct from `0`.
    pub tokens_in: Option<i32>,
    /// Tokens the provider reported, `null` when it reported none.
    pub tokens_out: Option<i32>,
    /// Attributed cost in cents. `0` until a price source exists (REQ-104); never invented.
    pub cost_cents: i32,
    /// Why generation failed or the plan was rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why the whole plan was rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    /// When apply finished, `null` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    /// The attempt this one replaces, `null` for a first attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes_id: Option<Uuid>,
    /// Row creation, RFC 3339.
    pub created_at: String,
    /// Last write, RFC 3339.
    pub updated_at: String,
    /// The counters the review screen's footer renders, so a file read outside the panel
    /// carries the same "4 accepted · 3 pending" the operator saw.
    pub counts: PlanCounts,
    /// What stands between the plan and apply, named. Empty when nothing does.
    #[serde(default)]
    pub blockers: Vec<ExportBlocker>,
    /// Every artifact, **including the superseded ones**, in tree order.
    #[serde(default)]
    pub artifacts: Vec<ExportArtifact>,
}

/// One artifact as the export file writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportArtifact {
    /// Artifact identity.
    pub id: Uuid,
    /// Which kind.
    pub kind: String,
    /// Stable key within (plan, kind).
    pub key: String,
    /// The artifact this one hangs off, `null` at the top of the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_key: Option<String>,
    /// Order inside the tree.
    pub ordinal: i32,
    /// Where the reviewer has got to with it.
    pub status: String,
    /// The artifact body, **verbatim**. Never normalised, never inspected.
    pub spec: Value,
    /// The model's own explanation.
    pub rationale: String,
    /// The validator's findings, verbatim.
    #[serde(default)]
    pub validation: Value,
    /// Why the reviewer refused it — `null` for a machine retirement, which has no reason
    /// and does not pretend to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
    /// The artifact this one replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes_id: Option<Uuid>,
    /// The artifact that replaced this one, derived from the other rows rather than stored:
    /// a reader must not have to hold the whole set in their head to follow a chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<Uuid>,
}

/// One blocker as the export file writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportBlocker {
    /// Which kind is unresolved or missing.
    pub kind: String,
    /// Which artifact, empty for a required kind the plan never proposed.
    pub key: String,
    /// Its status, or `missing`.
    pub status: String,
    /// The first finding, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Build the document from what the store already read.
///
/// A struct rather than four parameters because **the four are read together**: the plan,
/// its artifacts, its counters and its blockers are one consistent moment, and four
/// separately-fetched arguments can describe a plan that never existed — a plan with
/// another plan's counters is an export that is confidently wrong.
#[must_use]
pub fn build_plan_export(
    plan: &AppBuilderPlan,
    artifacts: &[AppBuilderArtifact],
    counts: PlanCounts,
    blockers: &[BlockedArtifact],
) -> PlanExport {
    // The forward edge, derived rather than stored: `supersedes_id` says what a row replaced,
    // and "what replaced *this* row" is the same fact read from the other end.
    let superseded_by: Vec<(Uuid, Uuid)> = artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .supersedes_id
                .map(|previous| (previous, artifact.id))
        })
        .collect();

    PlanExport {
        schema: EXPORT_SCHEMA.to_owned(),
        id: plan.id,
        prompt: plan.prompt.clone(),
        title: plan.title.clone(),
        status: plan.status.clone(),
        plan_version: plan.plan_version,
        model_label: plan.model_label.clone(),
        tokens_in: plan.tokens_in,
        tokens_out: plan.tokens_out,
        cost_cents: plan.cost_cents,
        error: plan.error.clone(),
        decision_reason: plan.decision_reason.clone(),
        applied_at: plan.applied_at.map(rfc3339),
        supersedes_id: plan.supersedes_id,
        created_at: rfc3339(plan.created_at),
        updated_at: rfc3339(plan.updated_at),
        counts,
        blockers: blockers
            .iter()
            .map(|blocker| ExportBlocker {
                kind: blocker.kind.clone(),
                key: blocker.key.clone(),
                status: blocker.status.clone(),
                reason: blocker.reason.clone(),
            })
            .collect(),
        artifacts: artifacts
            .iter()
            .map(|artifact| ExportArtifact {
                id: artifact.id,
                kind: artifact.kind.clone(),
                key: artifact.key.clone(),
                parent_key: artifact.parent_key.clone(),
                ordinal: artifact.ordinal,
                status: artifact.status.clone(),
                spec: artifact.spec.clone(),
                rationale: artifact.rationale.clone(),
                validation: artifact.validation.clone(),
                rejected_reason: artifact.rejected_reason.clone(),
                supersedes_id: artifact.supersedes_id,
                superseded_by: superseded_by
                    .iter()
                    .find(|(previous, _)| *previous == artifact.id)
                    .map(|(_, next)| *next),
            })
            .collect(),
    }
}

/// The file name a plan exports under.
///
/// Derived from the plan's own short id rather than its title: a title is free text and may
/// hold a slash, a quote or a control character, all of which end up inside a
/// `Content-Disposition` header. The short id is eight hex characters and cannot be any of
/// those, so the header is built from a value no request can poison.
#[must_use]
pub fn export_filename(plan: &AppBuilderPlan) -> String {
    let short: String = plan.id.simple().to_string().chars().take(8).collect();
    format!("omnion-app-plan-{short}.json")
}

/// Serialise the document, indented, with a trailing newline.
///
/// The newline is not decoration: a file without one makes `cat` and every diff tool on it
/// unhappy, and an export is a file an operator will commit to a repository.
pub fn render_plan_export(export: &PlanExport) -> Result<String> {
    let mut body = serde_json::to_string_pretty(export)
        .map_err(|error| crate::error::AppBuilderError::Render(error.to_string()))?;
    body.push('\n');
    Ok(body)
}

/// An empty document, for a caller that wants the shape without a plan.
///
/// Not used by the route — the route always has a plan — and kept because it is what a
/// unit test asserts the shape against, and a shape asserted only against a real row is a
/// shape whose `None` branches are never exercised.
#[must_use]
pub fn empty_plan_export(id: Uuid) -> PlanExport {
    PlanExport {
        schema: EXPORT_SCHEMA.to_owned(),
        id,
        prompt: String::new(),
        title: String::new(),
        status: "draft".to_owned(),
        plan_version: 1,
        model_label: String::new(),
        tokens_in: None,
        tokens_out: None,
        cost_cents: 0,
        error: None,
        decision_reason: None,
        applied_at: None,
        supersedes_id: None,
        created_at: String::new(),
        updated_at: String::new(),
        counts: PlanCounts::default(),
        blockers: Vec::new(),
        artifacts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn plan() -> AppBuilderPlan {
        AppBuilderPlan {
            id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            organization_id: None,
            site_id: None,
            prompt: "Create an app to manage employees' leave requests".into(),
            title: "Leave requests".into(),
            status: "draft".into(),
            plan_version: 2,
            model_label: "qa/mock-model".into(),
            tokens_in: Some(120),
            tokens_out: Some(340),
            cost_cents: 0,
            error: None,
            created_by: None,
            applied_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            supersedes_id: Some(Uuid::from_u128(1)),
            decision_reason: None,
        }
    }

    fn artifact(id: u128, kind: &str, key: &str, supersedes: Option<Uuid>) -> AppBuilderArtifact {
        AppBuilderArtifact {
            id: Uuid::from_u128(id),
            plan_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            kind: kind.into(),
            key: key.into(),
            parent_key: None,
            ordinal: 0,
            status: "accepted".into(),
            spec: json!({ "key": key, "label": "Label" }),
            rationale: "Because.".into(),
            validation: json!([]),
            supersedes_id: supersedes,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            rejected_reason: None,
        }
    }

    /// The document is a **file**, so the properties worth asserting are the ones a reader
    /// in a text editor would rely on. Every one of them is a way this could have looked
    /// finished and produced a file that is wrong in a way no compiler notices.
    #[test]
    fn the_document_names_its_own_format_and_round_trips_through_json() {
        let export = empty_plan_export(plan().id);
        let body = render_plan_export(&export).expect("a document must render");
        let value: Value = serde_json::from_str(&body).expect("the file must be JSON");

        assert_eq!(
            value["schema"], EXPORT_SCHEMA,
            "a reader cannot tell two formats apart without this key"
        );
        let read_back: PlanExport =
            serde_json::from_value(value).expect("the file must read back as a plan");
        assert_eq!(read_back, export, "the type and the file disagree");
    }

    #[test]
    fn a_file_ends_with_a_newline_and_is_indented() {
        let body = render_plan_export(&empty_plan_export(plan().id)).expect("rendered");
        assert!(body.ends_with('\n'), "an export is committed to repositories");
        assert!(
            body.contains("\n  \"schema\""),
            "an unindented export is unreadable in a diff: {body}"
        );
    }

    /// A reader cannot distinguish "the provider reported no tokens" from "it reported
    /// zero", and those are different facts: one means the answer is unpriced, the other
    /// means the answer was free.
    #[test]
    fn an_unreported_token_count_stays_null_and_is_not_flattened_to_zero() {
        let mut source = plan();
        source.tokens_in = None;
        source.tokens_out = None;
        let body = render_plan_export(&build_plan_export(&source, &[], PlanCounts::default(), &[]))
            .expect("rendered");
        let value: Value = serde_json::from_str(&body).expect("JSON");

        assert!(
            value["tokens_in"].is_null(),
            "a reported 0 and an unreported count are different facts: {body}"
        );
        assert!(value["tokens_out"].is_null(), "{body}");
    }

    /// The reported counts are exported **beside** the artifacts rather than derived from
    /// them here, because the store computed them in the same query as the page. A reader
    /// that re-counted would have to know the exact `status` vocabulary this file is free of
    /// — and a file that can be mis-read by counting it in the wrong way is a worse export.
    #[test]
    fn the_counts_are_the_stores_and_the_artifacts_are_the_stores() {
        let counts = PlanCounts {
            artifacts: 4,
            accepted: 3,
            rejected: 1,
            pending: 0,
            invalid: 0,
        };
        let artifacts = vec![artifact(10, "entity", "leave_request", None)];
        let export = build_plan_export(&plan(), &artifacts, counts, &[]);

        assert_eq!(export.counts, counts);
        assert_eq!(export.artifacts.len(), 1);
        assert_eq!(export.artifacts[0].key, "leave_request");
    }

    /// The chain is drawn in **both** directions from one stored edge. This is the whole
    /// reason the export builds `superseded_by` rather than leaving a reader to scan.
    #[test]
    fn a_replacement_chain_is_readable_in_both_directions_from_one_stored_edge() {
        let old = Uuid::from_u128(10);
        let new = artifact(11, "report", "leave_summary", Some(old));
        let export = build_plan_export(
            &plan(),
            &[artifact(10, "report", "leave_summary", None), new],
            PlanCounts::default(),
            &[],
        );

        let retired = export
            .artifacts
            .iter()
            .find(|a| a.id == old)
            .expect("the superseded row is exported too");
        let current = export
            .artifacts
            .iter()
            .find(|a| a.id == Uuid::from_u128(11))
            .expect("the replacement row");

        assert_eq!(
            retired.superseded_by,
            Some(Uuid::from_u128(11)),
            "a reader cannot tell 'replaced' from 'never had a successor' without this"
        );
        assert_eq!(current.supersedes_id, Some(old));
    }

    /// A plan with nothing in it is the answer for a failed generation, and a document that
    /// refuses to be written is a download button that errors for no reason a caller can act
    /// on.
    #[test]
    fn a_plan_with_no_artifacts_still_exports() {
        let export = build_plan_export(&plan(), &[], PlanCounts::default(), &[]);
        assert!(export.artifacts.is_empty());
        let body = render_plan_export(&export).expect("an empty plan is still a document");
        assert!(body.contains("\"artifacts\": []"), "{body}");
    }

    /// The blockers are named in the file, so a plan that cannot be applied is an export a
    /// reader can act on rather than a status they have to re-derive from the artifacts.
    #[test]
    fn the_blockers_travel_with_the_file() {
        let blockers = vec![BlockedArtifact {
            kind: "report".into(),
            key: String::new(),
            status: "missing".into(),
            reason: Some("the plan proposes none".into()),
        }];
        let export = build_plan_export(&plan(), &[], PlanCounts::default(), &blockers);
        assert_eq!(export.blockers.len(), 1);
        assert_eq!(export.blockers[0].kind, "report");
        assert_eq!(export.blockers[0].status, "missing");
    }

    /// A machine retirement carries no reason and must not be given one: an export that
    /// invented "superseded" as a rejection would make the reviewer look like they decided
    /// something they did not.
    #[test]
    fn a_machine_retirement_is_exported_without_a_reason() {
        let mut retired = artifact(10, "field", "leave_days", None);
        retired.status = "rejected".into();
        retired.rejected_reason = None;
        let export = build_plan_export(&plan(), &[retired], PlanCounts::default(), &[]);
        let body = render_plan_export(&export).expect("rendered");
        assert!(
            !body.contains("rejected_reason"),
            "an export must not invent a decision: {body}"
        );
    }

    /// The file name comes from the id, because the title is free text and a free-text file
    /// name ends up inside a response header.
    #[test]
    fn the_file_name_is_derived_from_the_id_not_the_title() {
        let mut hostile = plan();
        hostile.title = "../../etc/passwd\"; drop".into();
        let name = export_filename(&hostile);

        assert!(name.starts_with("omnion-app-plan-"), "{name}");
        assert!(name.ends_with(".json"), "{name}");
        assert!(!name.contains('/'), "a title must not reach the header: {name}");
        assert!(!name.contains('"'), "{name}");
        assert_eq!(name, "omnion-app-plan-01234567.json", "{name}");
    }
}