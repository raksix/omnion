//! Omnion AI App Builder.
//!
//! The request this module answers is one sentence: *"Create an app to manage employees'
//! leave requests"* — followed by the part that decides whether the answer is worth
//! anything: *"...and the app is actually created"* (docs/requests/REQ-045).
//!
//! Those are **two acts**, and the split is the whole design:
//!
//! * **Generation** turns a sentence into typed artifacts — entities, fields, screens,
//!   permissions, a role, a workflow, notification templates and a report. Model output is
//!   untrusted input: every key is validated against platform naming rules before it is
//!   stored as anything but a draft, and a finding is recorded beside the artifact it
//!   belongs to rather than used to rewrite it.
//! * **Apply** (a later slice) is the only writer of live tables, and it needs every required
//!   artifact resolved by a person first.
//!
//! So a plan is inert by construction. There is no code path in which a generation becomes
//! an entity without a review in between, because nothing here can create an entity at all.

#![forbid(unsafe_code)]

pub mod error;
pub mod export;
pub mod generate;
pub mod model;
pub mod store;
pub mod validate;

pub use error::{AppBuilderError, BlockedArtifact, Result, describe_blocker};
pub use export::{
    EXPORT_SCHEMA, ExportArtifact, ExportBlocker, PlanExport, build_plan_export, empty_plan_export,
    export_filename, render_plan_export,
};
pub use generate::{Generated, kind_rank, missing_required_kinds, normalize, schema_prompt};
pub use model::{
    ARTIFACT_STATUSES, AppBuilderApplication, AppBuilderApplicationStep, AppBuilderArtifact,
    AppBuilderPlan, EditedArtifact, KINDS, MAX_PROMPT_LEN, MIN_PROMPT_LEN, NEW_PLAN_STATUS,
    NewArtifact, NewPlan, OPEN_STATUSES, PlanCounts, PlanFilter, PlanPage, PlanUsage,
    REQUIRED_KINDS, STATUSES, artifact_is_resolved, artifact_status_is_known, kind_is_known,
    plan_status_is_known,
};
pub use store::{
    PlanStore, accept_artifact, apply_answer, apply_failure, artifact_counts, blockers,
    delete_plan, find_artifact, find_plan, find_plan_in, insert_artifact, insert_plan,
    list_artifacts, list_plans, reject_artifact, reject_plan, required_kinds_present,
    set_artifact_status, supersede_artifact, supersede_plan, update_artifact_spec,
};
pub use validate::{
    FIELD_TYPES, Finding, MAX_KEY_LEN, MAX_RATIONALE_LEN, MIN_KEY_LEN, PlanValidation,
    RESERVED_KEYS, artifact_id, validate_artifact, validate_key, validate_plan,
};
