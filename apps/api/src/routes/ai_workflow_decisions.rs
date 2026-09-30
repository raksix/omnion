//! `/api/v1/ai/workflows/drafts/{id}/…` — the decision half of the draft console (REQ-046, slice 4).
//!
//! Slice 3 put the console on the wire and left the review screen honestly incomplete: the
//! approval bar was absent rather than present and `404`. This module is that bar.
//!
//! Four decisions live here and they are not four handlers over one table — each is a
//! different promise, and three of the four can be broken in a way a test that only checks
//! "the status moved" would not notice:
//!
//! * **`approve` materialises a DISABLED workflow, explicitly.** The workflow API arms a new
//!   rule by default (`enabled_by_default`), and inheriting that default here would create an
//!   *armed schedule* out of a sentence an operator has not read yet. The spec's risk section
//!   says the definition is reviewed by a human *and* by a test-run before it can fire, so
//!   `enabled` is passed as `false` at the call site rather than relying on a default nobody
//!   in this file owns. Activation is `PATCH /workflows/{id}` with `enabled: true` — a
//!   separate action, on a separate screen, behind the same permission.
//! * **Approval is refused when the approver lacks a permission the steps need.** This is
//!   the spec's explicit "out of scope" clause, and it is checked against the **approver's**
//!   effective permissions at approval time, not the author's: the rule will run as whoever
//!   the workflow follows (`created_by` — the approver, since approval is what creates it),
//!   so the approver is exactly the person whose rights decide whether the steps may fire.
//!   `permission_for` is borrowed from the automation crate, the same map the engine uses at
//!   run time, so a step the engine would refuse at run time is refused here at approval time
//!   with the same key name.
//! * **`revise` spends a generation and answers a stream**, exactly like `generate`: the
//!   revision prompt is a prompt, and the review screen's "ask for changes" needs the same
//!   `plan → validate → repair?` panel.
//! * **`test-run` starts a real execution row and dispatches it, and proves no events
//!   escape.** A dry run that quietly skipped the real dispatch would be a "test" that passes
//!   for a definition that cannot run. So the test run **does** start the run — the *steps*
//!   are what "no external side effects" bounds, and the run row is the only evidence there
//!   was one. The spec's own criterion is "no events emitted during the test run", and the
//!   walk asserts it on the organization's event feed.
//!
//! One store rule is shared by three of the four: the decision writes are conditional on
//! `status in ('draft', 'failed')` (in the store), so a second approve on an already-approved
//! draft finds no row, and the handler answers `409` **naming the workflow it already
//! created** — the message is the whole reason the operator can act on it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use omnion_ai_hub::resolve;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::ai_workflows::{STREAM_BUFFER, store_error};
use crate::scope::ensure_same_organization;
use crate::state::AppState;

use omnion_module_ai as ai;

/// How many stage frames may queue in front of a client before the stream waits for it.
const REVISION_BUFFER: usize = STREAM_BUFFER;

// ---------------------------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------------------------

/// The revision form's body.
#[derive(Debug, Deserialize)]
pub struct ReviseBody {
    /// What the operator wants changed. A revision without a note is a request to guess.
    pub note: String,
    /// `provider/model`, or absent to keep the draft's frozen model.
    pub model: Option<String>,
}

/// The reject form's body.
#[derive(Debug, Deserialize)]
pub struct RejectBody {
    /// Why. Required by the store, and by the spec: a bare rejection is a decision nobody
    /// can learn from.
    pub reason: String,
    /// Organization, for a platform account.
    pub organization_id: Option<Uuid>,
}

/// What a decision answers.
#[derive(Debug, Serialize)]
pub struct DecisionBody {
    /// The draft as it now reads, so a client never has to re-fetch to draw the new state.
    pub draft: crate::routes::ai_workflows::DraftBody,
    /// The workflow approval materialised, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<Uuid>,
    /// Whether the workflow was created **disabled**; `false` on reject.
    pub enabled: bool,
}

/// One planned step of a test run.
#[derive(Debug, Serialize)]
pub struct TestRunStep {
    /// Position, counting from 1.
    pub position: i32,
    /// The step's own name.
    pub name: String,
    /// Step kind.
    pub kind: String,
    /// The action it would run, when it is a task.
    pub action: Option<String>,
    /// Whether the **host** runs this action — the line a reviewer reads first, because it is
    /// the one that says this step leaves the process.
    pub host: bool,
    /// The permission this action needs at run time, borrowed from the automation crate's
    /// map. `null` for the engine's own synthetic actions, which need no account.
    pub permission: Option<&'static str>,
    /// The step's parameters, as written.
    pub params: Value,
}

/// The test run's answer.
#[derive(Debug, Serialize)]
pub struct TestRunBody {
    /// The draft that was tested.
    pub draft_id: Uuid,
    /// The workflow it became, when it has one. `null` for a draft under review — which is
    /// why nothing could be dispatched even in principle.
    pub workflow_id: Option<Uuid>,
    /// The definition as it would be run, with the engine's own defaults filled in.
    pub definition: Value,
    /// The plan, step by step.
    pub steps: Vec<TestRunStep>,
    /// The verdict. `ready` when every step validated; the API refuses otherwise, so this is
    /// not a field a client has to branch on — it is what the walkthrough asserts.
    pub verdict: &'static str,
    /// What this run did **not** do, in the platform's own words, so the screen can say it
    /// rather than leave the operator to assume a rule was exercised.
    pub note: &'static str,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `PATCH /api/v1/ai/workflows/drafts/{id}` — save an operator-edited definition.
///
/// Revalidated by the **module's** validator, not only the engine's: an operator editing the
/// JSON may type a credential-shaped parameter, and the spec's criterion is that a generated
/// definition never carries a secret. Slice 1's `definition::validate` is the function that
/// knows about `SECRET_KEYS`, so the save path uses the same one the generation path does —
/// otherwise the check would exist only on the way in through the model.
///
/// An invalid save changes **nothing**: validation runs before any write, and the store's
/// update is additionally conditional on `status = 'draft'`, so a definition cannot be
/// replaced behind an approval.
pub async fn save_definition(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
    Json(body): Json<Value>,
) -> Result<Json<crate::routes::ai_workflows::DraftBody>, ApiError> {
    let draft = draft_for_decision(&state, &current, draft_id).await?;
    let definition = body.get("definition").cloned().unwrap_or(body);
    if !definition.is_object() {
        return Err(ApiError::bad_request(
            "invalid_draft_definition",
            "a definition is an object with `trigger` and `steps` — send the object itself",
        ));
    }

    // The engine first, so the message names the actions that exist, then the module's own
    // secret rule. Both codes are the ones the generation path reports, so an operator who
    // fixed one error and hit the next sees a consistent vocabulary.
    ai::definition::validate(&definition).map_err(module_error)?;

    let saved = ai::replace_definition(state.db().pool(), draft_id, &definition)
        .await
        .map_err(store_error)?;
    let saved = saved.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "ai_workflow_draft_not_editable",
            format!(
                "a draft can only be edited while it is `draft` — this one is `{}`",
                draft.status
            ),
        )
    })?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.updated")
            .organization(saved.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "steps": saved.definition().and_then(|d| d.get("steps")).and_then(Value::as_array).map(Vec::len).unwrap_or_default() }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(crate::routes::ai_workflows::DraftBody::build(&saved)))
}

/// `POST /api/v1/ai/workflows/drafts/{id}/revise` — ask the model for a change.
///
/// Streamed for the same reason `generate` is: the console's progress panel is not a
/// decoration, and a revision that answers after one round-trip leaves the operator staring
/// at a button that could be pressed twice.
pub async fn revise(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
    Json(body): Json<ReviseBody>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    let draft = draft_for_decision(&state, &current, draft_id).await?;
    if draft.workflow_id.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "draft_already_decided",
            format!(
                "this draft already became workflow {} — change that rule instead",
                draft.workflow_id.expect("checked above")
            ),
        ));
    }
    let note = body.note.trim();
    if note.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_revision_note",
            "asking for changes needs a note saying what to change",
        ));
    }

    // The model is resolved **before** the row is reset, so a revision that cannot run at
    // all leaves the draft exactly as it was: a `409` the console renders, not a `failed`
    // row describing a generation that never started.
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| draft.model_key.clone().unwrap_or_default());
    let resolved = resolve(state.db().pool(), (!model.is_empty()).then_some(model.as_str()))
        .await
        .map_err(provider_error)?;
    let model_id = resolved.id();

    let reset = ai::apply_revision(state.db().pool(), draft_id, note, &model_id)
        .await
        .map_err(store_error)?;
    if reset.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "draft_not_revisable",
            format!(
                "a draft can only be revised while it is `draft` or `failed` — this one is \
                 `{}`",
                draft.status
            ),
        ));
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.revise_requested")
            .organization(draft.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "revision": draft.revision_count + 1, "model": model_id }))
            .ip_address(address.as_text()),
    )
    .await?;

    let pool = state.db().pool().clone();
    let request = ai::GenerationRequest {
        model: Some(model_id.clone()),
        prompt: draft.prompt.clone(),
        revision: Some(note.to_owned()),
        previous: draft
            .definition()
            .map(|definition| definition.to_string()),
    };
    let organization_for_event = draft.organization_id;
    let user_id = current.user.id;
    let ip_address = address.as_text();

    let (frames, receiver) = mpsc::channel::<ReviseFrame>(REVISION_BUFFER);
    tokio::spawn(async move {
        if frames.send(ReviseFrame::Stage("plan".into())).await.is_err() {
            return;
        }
        let outcome = ai::generate(&resolved, &request).await;
        match outcome {
            Ok(answer) => {
                if frames.send(ReviseFrame::Stage("validate".into())).await.is_err() {
                    return;
                }
                if answer.repaired
                    && frames.send(ReviseFrame::Stage("repair".into())).await.is_err()
                {
                    return;
                }
                let stored = ai::apply_answer(
                    &pool,
                    draft_id,
                    &answer.title,
                    answer.rationale.as_deref(),
                    &answer.definition,
                    &answer.model_key,
                    Some(clamp_tokens(answer.tokens.input)),
                    Some(clamp_tokens(answer.tokens.output)),
                )
                .await;

                match stored {
                    Ok(Some(row)) => {
                        emit(
                            &pool,
                            NewEvent::new("ai.workflow_draft.generated")
                                .organization(organization_for_event)
                                .actor(user_id)
                                .payload(json!({
                                    "draft_id": row.id,
                                    "title": row.title,
                                    "model_key": row.model_key,
                                    "repaired": answer.repaired,
                                    "attempts": answer.attempts,
                                })),
                        )
                        .await;
                        let _ = frames
                            .send(ReviseFrame::Done {
                                draft_id: row.id,
                                status: row.status,
                                repaired: answer.repaired,
                                attempts: answer.attempts,
                                tokens: answer.tokens.total(),
                            })
                            .await;
                    }
                    Ok(None) => {
                        let _ = frames
                            .send(ReviseFrame::Failed {
                                code: "draft_not_generating",
                                message: "another generation finished first — reload the draft"
                                    .into(),
                            })
                            .await;
                    }
                    Err(error) => {
                        let _ = frames
                            .send(ReviseFrame::Failed {
                                code: "ai_workflow_store_error",
                                message: error.to_string(),
                            })
                            .await;
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                let code = if error.is_provider_failure() {
                    "ai_provider_error"
                } else {
                    error.code()
                };
                if let Ok(Some(row)) = ai::apply_failure(&pool, draft_id, &message).await {
                    emit(
                        &pool,
                        NewEvent::new("ai.workflow_draft.failed")
                            .organization(organization_for_event)
                            .actor(user_id)
                            .payload(json!({ "draft_id": row.id, "reason": message })),
                    )
                    .await;
                }
                let _ = frames
                    .send(ReviseFrame::Failed { code, message })
                    .await;
            }
        }

        let entry = NewAuditEntry::by_user(user_id, "ai.workflow_draft.generated")
            .organization(organization_for_event)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "model": model_id, "revised": true }))
            .ip_address(ip_address);
        if let Err(error) = omnion_audit::record(&pool, entry).await {
            tracing::warn!(%error, "the AI workflow revision audit row could not be written");
        }
    });

    Ok(Sse::new(ReceiverStream::new(receiver).map(ReviseFrame::event))
        .keep_alive(KeepAlive::default()))
}

/// `POST /api/v1/ai/workflows/drafts/{id}/approve` — materialise a **disabled** workflow.
///
/// Four steps in an order that matters, and the order is the point of the slice:
///
/// 1. the definition is revalidated (the model wrote it, but an operator may have edited it
///    and a step may have been changed since);
/// 2. the **approver's** effective permissions are resolved and every step's action is
///    checked against [`omnion_automation::authority::permission_for`];
/// 3. the decision is written (`draft` → `approved`) — **before** the workflow, so a lost
///    race cannot leave a workflow nobody recorded;
/// 4. the workflow is inserted with `enabled: false`, and only then is the id attached
///    (`approved` → `activated`).
///
/// Steps 3 and 4 are two writes, and the failure mode of doing them the other way round is
/// a draft that says "approved" with no workflow, which the review screen renders as a
/// decision that produced nothing. The store's `attach_workflow` is conditional on
/// `workflow_id is null`, so a second approve cannot attach a second workflow.
pub async fn approve(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
) -> Result<Json<DecisionBody>, ApiError> {
    let draft = draft_for_decision(&state, &current, draft_id).await?;
    if let Some(workflow_id) = draft.workflow_id {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "draft_already_approved",
            format!("this draft already produced workflow {workflow_id}"),
        ));
    }
    let definition = draft.definition().cloned().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "draft_has_no_definition",
            "there is nothing to approve — generate or revise the draft first",
        )
    })?;
    let parsed = parse_definition(&definition)?;
    let trigger = parsed.trigger.clone();

    // The approver is the account the rule will run as: `NewWorkflow.run_as_user_id` is
    // `None`, so the engine falls back to `created_by` — which is the approver, because
    // approval is what creates the workflow. Checking the *author's* rights instead would
    // approve a rule the approver cannot run.
    require_approver_authority(&state, &current, draft.organization_id, &parsed).await?;

    let next_run_at = trigger
        .validate(OffsetDateTime::now_utc())
        .map_err(workflow_error)?;

    let approved = ai::apply_decision(
        state.db().pool(),
        draft_id,
        "approved",
        current.user.id,
        None,
    )
    .await
    .map_err(store_error)?;
    let approved = approved.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "draft_not_decidable",
            format!(
                "a draft can only be decided while it is `draft` or `failed` — this one is \
                 `{}`",
                draft.status
            ),
        )
    })?;

    // `enabled: false` written here, at the call site, rather than inherited: the workflow
    // API arms a new rule by default and inheriting that would create an armed schedule out
    // of a sentence nobody has read. Activation is a separate action.
    let workflow = omnion_workflows::store::insert_workflow(
        state.db().pool(),
        omnion_workflows::NewWorkflow {
            on_error: omnion_workflows::OnError::Stop,
            organization_id: approved.organization_id,
            site_id: approved.site_id,
            name: approved.title.clone(),
            description: format!(
                "Generated from a workflow draft ({}). Reviewed and approved by a person; \
                 enable it when the schedule is right.",
                approved.id
            ),
            enabled: false,
            trigger: trigger.kind,
            schedule: trigger.cron.clone(),
            trigger_event: trigger.event.clone(),
            // The column has two constraints and they do not agree: the group check accepts an
            // array **or** a `{"all": …}` object, while `workflows_conditions_need_event`
            // evaluates `jsonb_array_length(conditions) = 0` — and that function **raises** on
            // a non-array instead of returning something falsy, so the check only survives an
            // object when the trigger is an event. A generated draft is a *manual* rule more
            // often than not, and a model that sent the group shape (which is what the
            // automation layer stores, so it is the shape a model has seen) therefore made
            // every approval answer `400 workflow_store_error`, naming the constraint that had
            // **passed**. Normalising here is the whole fix, and it is the same normalisation
            // `automation::build_definition` applies — in the opposite direction, because
            // there the trigger is always an event.
            conditions: storable_conditions(&parsed),
            // Approval creates the rule, so the rule follows the person who approved it —
            // the same `None` the manual workflow surface sends, meaning "follow the author".
            run_as_user_id: None,
            rate_limit_per_hour: None,
            concurrency: None,
            next_run_at,
            steps: parsed.steps_json()?,
            created_by: Some(current.user.id),
        },
    )
    .await
    .map_err(workflow_error)?;

    let activated = ai::attach_workflow(state.db().pool(), draft_id, workflow.id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "draft_already_approved",
                "this draft already produced a workflow",
            )
        })?;

    emit(
        state.db().pool(),
        NewEvent::new("ai.workflow_draft.approved")
            .organization(activated.organization_id)
            .actor(current.user.id)
            .payload(json!({ "draft_id": draft_id, "workflow_id": workflow.id })),
    )
    .await;
    emit(
        state.db().pool(),
        NewEvent::new("workflow.created")
            .organization(activated.organization_id)
            .actor(current.user.id)
            .payload(json!({ "workflow_id": workflow.id, "name": workflow.name })),
    )
    .await;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.approved")
            .organization(activated.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({
                "workflow_id": workflow.id,
                "enabled": false,
                "steps": parsed.steps.len(),
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(DecisionBody {
        draft: crate::routes::ai_workflows::DraftBody::build(&activated),
        workflow_id: Some(workflow.id),
        enabled: false,
    }))
}

/// `POST /api/v1/ai/workflows/drafts/{id}/reject` — reject with a reason.
pub async fn reject(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
    Json(body): Json<RejectBody>,
) -> Result<Json<DecisionBody>, ApiError> {
    let organization_id = organization_of(&current, body.organization_id)?;
    // `draft_for_decision`, not `draft_in_scope`: a draft that already became a workflow has
    // to be refused with the message that **names the builder**, and the store's status guard
    // would otherwise answer "a draft can only be decided while it is `draft` or `failed` —
    // this one is `activated`". That is true, and useless: the operator is told the state and
    // not where to go. Two decision routes refusing the same row with two different stories
    // is a defect the walk found by asserting the message, not the status.
    let draft = draft_for_decision(&state, &current, draft_id).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "rejection_reason_required",
            "rejecting a draft needs a reason the person who asked for it can read",
        ));
    }

    let rejected = ai::apply_decision(
        state.db().pool(),
        draft_id,
        "rejected",
        current.user.id,
        Some(reason),
    )
    .await
    .map_err(store_error)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "draft_not_decidable",
            format!(
                "a draft can only be decided while it is `draft` or `failed` — this one is \
                 `{}`",
                draft.status
            ),
        )
    })?;

    emit(
        state.db().pool(),
        NewEvent::new("ai.workflow_draft.rejected")
            .organization(rejected.organization_id)
            .actor(current.user.id)
            // The reason travels: a webhook receiver that is told "a draft was rejected"
            // with nothing else cannot tell a duplicate idea from a dangerous one.
            .payload(json!({ "draft_id": draft_id, "reason": reason })),
    )
    .await;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.rejected")
            .organization(rejected.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({ "title": rejected.title, "reason": reason }))
            .ip_address(address.as_text()),
    )
    .await?;
    let _ = organization_id;

    Ok(Json(DecisionBody {
        draft: crate::routes::ai_workflows::DraftBody::build(&rejected),
        workflow_id: None,
        enabled: false,
    }))
}

/// `POST /api/v1/ai/workflows/drafts/{id}/test-run` — what this definition *would* do.
///
/// **The design decision of this handler, stated because it is the one that could have gone
/// the other way.** The spec asks for a dry run and then defines it as "performs no external
/// side effects — asserted by *no events emitted during the test run*". The first draft of
/// this handler tried to honour the phrase literally in the other direction: start a real
/// `workflow_executions` row and immediately cancel it. That is impossible, and the reason is
/// worth keeping:
///
/// * `workflow_executions.workflow_id` is `not null references workflows (id) on delete
///   cascade`. A draft under review has **no** workflow, so a run row cannot name it. The
///   only ways through are materialising a workflow as a side effect of a *test* — which is
///   precisely the thing the spec forbids — or inventing a workflow row that is deleted
///   immediately afterwards, which would cascade the run away with it.
/// * Even without the foreign key, a row left `running` is a row the engine's sweeper will
///   later decide what to do with, and a row left `cancelled` is a run that never ran: the
///   history screen would show a test the platform cannot distinguish from a real one.
///
/// So the test run is what it can honestly be: **the definition is parsed, validated by the
/// engine, and projected** — every step with the action, whether the host runs it, and the
/// permission that action needs — and the plan comes back with a verdict. No execution row,
/// no dispatch, no event, no row that outlives the request. The criterion "no events emitted"
/// is then true by construction rather than by assertion, and the review screen says exactly
/// that instead of implying a rule was exercised.
pub async fn test_run(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(draft_id): Path<Uuid>,
) -> Result<Json<TestRunBody>, ApiError> {
    let draft = draft_in_scope(&state, &current, draft_id).await?;
    let definition = draft.definition().cloned().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "draft_has_no_definition",
            "there is nothing to test — generate the draft first",
        )
    })?;
    // The engine's validator, not a copy: a test run that accepted a definition the engine
    // would refuse would be the one screen that says "ready" about a rule that cannot run.
    ai::definition::validate(&definition).map_err(module_error)?;
    let parsed: omnion_workflows::definition::WorkflowDefinition =
        serde_json::from_value(definition).map_err(|err| {
            ApiError::bad_request(
                "invalid_draft_definition",
                format!("the stored definition is not a workflow the engine accepts: {err}"),
            )
        })?;

    let steps = parsed
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let action = step.action.as_deref();
            TestRunStep {
                position: i32::try_from(index + 1).unwrap_or(i32::MAX),
                name: step.name.clone(),
                kind: step.kind.as_str().to_owned(),
                action: action.map(str::to_owned),
                host: action.is_some_and(omnion_workflows::actions::is_host_action),
                permission: action.and_then(omnion_automation::authority::permission_for),
                params: step.params.clone(),
            }
        })
        .collect::<Vec<TestRunStep>>();

    // The audit row is the record that a test happened. A dry run that wrote nothing at all
    // would leave no trace, and "an operator pressed Test run" is a fact somebody will ask
    // about after a bad approval.
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "ai.workflow_draft.test_run")
            .organization(draft.organization_id)
            .target("ai_workflow_draft", draft_id.to_string())
            .metadata(json!({
                "steps": parsed.steps.len(),
                "host_steps": steps.iter().filter(|step| step.host).count(),
                "workflow_id": draft.workflow_id,
            }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(Json(TestRunBody {
        draft_id,
        workflow_id: draft.workflow_id,
        definition: serde_json::to_value(&parsed).unwrap_or(Value::Null),
        steps,
        verdict: "ready",
        note: "Validated against the engine and projected step by step. Nothing was \
               dispatched, no email was sent and no event was emitted — enabling the rule is \
               a separate action on the workflow itself.",
    }))
}

// ---------------------------------------------------------------------------------------------
// Authority at approval time
// ---------------------------------------------------------------------------------------------

/// Refuse an approval whose steps need a permission the approver does not hold.
///
/// The spec's own "out of scope" clause: *a generated step needing a permission the approver
/// lacks is refused at approval time, never silently granted*. The map is
/// [`omnion_automation::authority::permission_for`] — borrowed, not copied, because the same
/// map decides the step at run time and a copy would be a second answer to "what does this
/// action need".
///
/// A **host** action is the only kind that needs a permission: the engine's synthetic
/// actions (`noop`, `echo`, `fail`, `transient`) touch nothing outside the run and are
/// answered by the engine itself, so refusing one would refuse the definitions the engine is
/// proven with.
async fn require_approver_authority(
    state: &AppState,
    current: &CurrentSession,
    organization_id: Uuid,
    definition: &omnion_workflows::definition::WorkflowDefinition,
) -> Result<(), ApiError> {
    // The steps' requirements are collected **before** any query: a definition made only of
    // the engine's own actions needs no permission check at all, and a definition that names
    // the same action twice must not resolve twice. Both are cheap to get wrong and neither
    // shows up in a test that only checks the refusal.
    let required = required_permissions(definition);
    if required.is_empty() {
        return Ok(());
    }

    let effective = omnion_permissions::effective_permissions(
        state.db().pool(),
        current.user.id,
        omnion_permissions::Scope::Organization { organization_id },
    )
    .await?;

    // Every missing key is reported, not just the first: an operator who is one grant short
    // is told one thing, fixes it, presses approve again and is told the next one. The whole
    // list in one refusal is the difference between one round-trip and three.
    let mut missing: Vec<&'static str> = Vec::new();
    for (_, permission) in &required {
        if !effective.allows(permission) && !missing.contains(permission) {
            missing.push(permission);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let named: Vec<String> = required
        .iter()
        .filter(|(_, permission)| missing.contains(permission))
        .map(|(action, permission)| format!("{action} needs {permission}"))
        .collect();
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "ai_workflow_missing_authority",
        format!(
            "this rule cannot be approved by you: {} — grant {} first, or ask someone who \
             holds it to approve. The steps were not run and no workflow was created.",
            named.join("; "),
            missing.join(", "),
        ),
    ))
}

/// Every `(action, permission)` a definition's steps require, deduplicated and in first-seen
/// order.
///
/// Pure, so the rule is testable without a database — and it has to be testable, because
/// "which steps need an account" is a claim about the closed action registry that nothing
/// else in the platform checks for this surface. The two facts the function encodes:
///
/// * **a synthetic action needs nothing** — `noop`, `echo`, `fail` and `transient` are run
///   by the engine itself and touch nothing outside the run, so a definition made only of
///   them must be approvable by anybody with `workflows.manage`;
/// * **a host action with no entry in the map is refused** rather than waved through. The
///   engine's own handler refuses such an action at run time with a permission message; if
///   approval waved it through, the rule would be approved and then stop on its first firing.
fn required_permissions(
    definition: &omnion_workflows::definition::WorkflowDefinition,
) -> Vec<(&'static str, &'static str)> {
    let mut required: Vec<(&'static str, &'static str)> = Vec::new();
    for step in &definition.steps {
        let Some(action) = step.action.as_deref() else {
            continue;
        };
        if !omnion_workflows::actions::is_host_action(action) {
            continue;
        }
        let Some(permission) = omnion_automation::authority::permission_for(action) else {
            continue;
        };
        // The action is taken from the **registry**, not from the definition: the registry
        // holds `&'static str` keys, and a `&str` borrowed from the stored JSON would tie
        // the return type to a lifetime nothing here can name. A definition naming an action
        // is already validated against the registry, so the two strings are equal — and
        // using the registry's own is what lets the refusal message name an action the
        // platform still has, even if the row is older than a rename.
        let Some(known) = omnion_workflows::actions::HOST_ACTIONS
            .iter()
            .find(|def| def.key == action)
        else {
            continue;
        };
        if !required.iter().any(|(_, seen)| *seen == permission) {
            required.push((known.key, permission));
        }
    }
    required
}

// ---------------------------------------------------------------------------------------------
// Stream frames
// ---------------------------------------------------------------------------------------------

/// One frame of a revision, before it becomes an SSE event.
enum ReviseFrame {
    /// What the platform is doing: `plan`, `validate` or `repair`.
    Stage(String),
    /// The revised draft is stored and can be reopened.
    Done {
        /// The draft id.
        draft_id: Uuid,
        /// Its status (`draft`).
        status: String,
        /// Whether the single repair round-trip was spent.
        repaired: bool,
        /// How many provider calls it took.
        attempts: usize,
        /// What it cost.
        tokens: i64,
    },
    /// The revision stopped. The draft carries the same message.
    Failed {
        /// Stable machine-readable code.
        code: &'static str,
        /// Message for a person.
        message: String,
    },
}

impl ReviseFrame {
    /// The SSE event a client reads.
    fn event(self) -> Result<Event, std::convert::Infallible> {
        let event = match self {
            Self::Stage(stage) => Event::default()
                .event("stage")
                .data(json!({ "stage": stage }).to_string()),
            Self::Done {
                draft_id,
                status,
                repaired,
                attempts,
                tokens,
            } => Event::default()
                .event("done")
                .data(
                    json!({
                        "draft_id": draft_id,
                        "status": status,
                        "repaired": repaired,
                        "attempts": attempts,
                        "tokens": tokens,
                    })
                    .to_string(),
                ),
            Self::Failed { code, message } => Event::default()
                .event("error")
                .data(json!({ "code": code, "message": message }).to_string()),
        };
        Ok(event)
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Organization, for a platform account; a scoped account's own.
fn organization_of(current: &CurrentSession, requested: Option<Uuid>) -> Result<Uuid, ApiError> {
    match current.user.organization_id {
        Some(own) => {
            ensure_same_organization(current, requested)?;
            Ok(own)
        }
        None => requested.ok_or_else(|| {
            ApiError::bad_request(
                "organization_required",
                "this account is not attached to an organization — name one to decide its drafts",
            )
        }),
    }
}

/// Load a draft in the caller's organization, for a decision.
async fn draft_in_scope(
    state: &AppState,
    current: &CurrentSession,
    draft_id: Uuid,
) -> Result<ai::AiWorkflowDraft, ApiError> {
    let organization_id = organization_of(current, None)?;
    ai::find_draft_in(state.db().pool(), organization_id, draft_id)
        .await
        .map_err(store_error)?
        .ok_or_else(draft_not_found)
}

/// The same, refusing a draft that has already become a workflow.
///
/// A draft with a `workflow_id` is a **record of a decision**, not something to decide again:
/// revising it would change a definition beside an armed rule that was materialised from the
/// old one, and the review screen would then describe something the workflow does not do.
///
/// **The message names the builder, and that is the point of the assertion beside it.** The
/// first version said *"change that rule instead"* — a refusal that is correct and useless: the
/// caller has an id and a draft screen, and nothing that tells it which screen a rule is
/// changed on. The review screen already links `/workflows/{id}/builder`, so the API names the
/// same path it renders. A refusal that does not name its own exit is a dead end, and the
/// person hitting it is usually an operator who is not going to read the code.
async fn draft_for_decision(
    state: &AppState,
    current: &CurrentSession,
    draft_id: Uuid,
) -> Result<ai::AiWorkflowDraft, ApiError> {
    let draft = draft_in_scope(state, current, draft_id).await?;
    if let Some(workflow_id) = draft.workflow_id {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "draft_already_decided",
            format!(
                "this draft already became workflow {workflow_id} — change the rule itself in \
                 the builder at /workflows/{workflow_id}/builder; the draft stays as the record \
                 of the decision"
            ),
        ));
    }
    Ok(draft)
}

/// The 404 a missing and an out-of-scope draft both answer.
fn draft_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "ai_workflow_draft_not_found",
        "no such workflow draft",
    )
}

/// The `conditions` value the `workflows` column will accept for this definition.
///
/// **The single function that knows the two constraints disagree**, and the reason the
/// approval path did not simply hand `conditions_json()` to the store:
///
/// * `workflows_conditions_is_group` (0020) admits an **array** or a group object;
/// * `workflows_conditions_need_event` (0010) evaluates `jsonb_array_length(conditions) = 0`,
///   and `jsonb_array_length` **raises** `cannot get array length of a non-array` rather than
///   answering something falsy. The `or` reaches it only when the left side — the trigger
///   being an event — is false, so a group object survives for an event rule and a non-event
///   rule is refused.
///
/// The shape a model sends is the group one (it is what the automation layer stores, so it is
/// what a model has seen in a definition), and a generated rule is manual or scheduled more
/// often than not. So: an event trigger keeps whatever was validated; anything else stores
/// the **flat array**, which both constraints admit — and an empty one, because a non-event
/// trigger has no payload to evaluate conditions against and `validate_conditions` has already
/// refused any conditions it carries.
fn storable_conditions(
    definition: &omnion_workflows::definition::WorkflowDefinition,
) -> Value {
    if definition.trigger.kind == omnion_workflows::TriggerKind::Event {
        return definition
            .conditions_json()
            .unwrap_or_else(|_| Value::Array(Vec::new()));
    }
    match &definition.conditions {
        Value::Array(items) => Value::Array(items.clone()),
        // A group on a non-event trigger: the validator refuses a *non-empty* one, so
        // anything that reaches here is empty and the flat form says the same thing in the
        // shape the column can measure.
        _ => Value::Array(Vec::new()),
    }
}

/// Deserialise a stored definition with the engine's own shape.
fn parse_definition(
    definition: &Value,
) -> Result<omnion_workflows::definition::WorkflowDefinition, ApiError> {
    serde_json::from_value(definition.clone()).map_err(|err| {
        ApiError::bad_request(
            "invalid_draft_definition",
            format!("the stored definition is not a workflow the engine accepts: {err}"),
        )
    })
}

/// A token count a `integer` column admits.
fn clamp_tokens(count: i64) -> i32 {
    count.clamp(0, i64::from(i32::MAX)) as i32
}

/// Turn a module error into the `400` the console's inline messages are written for.
fn module_error(error: ai::AiWorkflowError) -> ApiError {
    ApiError::bad_request(error.code(), error.to_string())
}

/// Turn a workflow error into the `400` the engine's own vocabulary describes.
fn workflow_error(error: omnion_workflows::WorkflowError) -> ApiError {
    ApiError::bad_request(error.code(), error.to_string())
}

/// Turn a routing failure into the status the console's states are written for.
fn provider_error(error: omnion_ai_hub::AiHubError) -> ApiError {
    let (status, code) = match error {
        omnion_ai_hub::AiHubError::ProviderNotFound
        | omnion_ai_hub::AiHubError::ModelNotFound
        | omnion_ai_hub::AiHubError::NoDefaultModel
        | omnion_ai_hub::AiHubError::ProviderDisabled(_) => {
            (StatusCode::CONFLICT, "ai_no_model_available")
        }
        _ => (StatusCode::BAD_GATEWAY, "ai_provider_error"),
    };
    ApiError::new(status, code, error.to_string())
}

/// Write an audit row, and let its failure be a `500` like any other write's.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Publish an event, and never let it break the operation that published it.
async fn emit(pool: &sqlx::PgPool, event: NewEvent) {
    if let Err(error) = bus::emit(pool, event).await {
        tracing::warn!(%error, "an AI workflow draft event could not be published");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_count_a_column_cannot_hold_is_clamped_rather_than_wrapped() {
        assert_eq!(clamp_tokens(4_096), 4_096);
        assert_eq!(clamp_tokens(i64::from(i32::MAX) + 10), i32::MAX);
        assert_eq!(clamp_tokens(-1), 0);
    }

    fn definition_with_trigger(trigger: Value, conditions: Value) -> Value {
        json!({ "trigger": trigger, "conditions": conditions,
                "steps": [{ "name": "greet", "kind": "task", "action": "echo",
                            "params": { "value": "hi" } }] })
    }

    #[test]
    fn a_group_on_a_non_event_rule_is_flattened_because_the_column_asks_for_a_length() {
        // The failure this prevents, stated as a test: a model answers the group shape, the
        // approval is a **manual** rule, and `jsonb_array_length({'all': []})` raises — so the
        // insert answers `workflow_store_error` naming the constraint that had passed. Both
        // sides asserted, because the group case is what automation stores and the flat case
        // is what the manual surface writes.
        let manual_group: omnion_workflows::definition::WorkflowDefinition =
            serde_json::from_value(definition_with_trigger(
                json!({ "kind": "manual" }),
                json!({ "all": [] }),
            ))
            .expect("a definition with an empty group is valid");
        assert_eq!(
            storable_conditions(&manual_group),
            json!([]),
            "a non-event rule must store the flat array"
        );

        let manual_array: omnion_workflows::definition::WorkflowDefinition =
            serde_json::from_value(definition_with_trigger(
                json!({ "kind": "manual" }),
                json!([]),
            ))
            .expect("valid");
        assert_eq!(storable_conditions(&manual_array), json!([]));
    }

    #[test]
    fn an_event_rule_keeps_its_group_because_that_is_the_shape_conditions_have() {
        // The other direction. Flattening an event rule's conditions would throw away the
        // whole point of a condition, so the normaliser has to leave the group alone — and a
        // normaliser that only ever produced `[]` would pass the manual test above.
        let event: omnion_workflows::definition::WorkflowDefinition =
            serde_json::from_value(definition_with_trigger(
                json!({ "kind": "event", "event": "page.published" }),
                json!({ "all": [] }),
            ))
            .expect("an event definition with an empty group is valid");
        assert_eq!(storable_conditions(&event), json!({ "all": [] }));
    }

    #[test]
    fn a_definition_that_is_not_an_object_is_refused_before_it_is_stored() {
        let error = parse_definition(&json!({ "steps": "not a list" }))
            .expect_err("a definition the engine cannot deserialise is refused");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_valid_definition_is_the_engine_s_own_shape_not_a_second_one() {
        let definition = json!({
            "trigger": { "kind": "manual" },
            "steps": [{ "name": "greet", "kind": "task", "action": "echo",
                        "params": { "value": "hi" } }],
        });
        let parsed = parse_definition(&definition).expect("a definition the engine accepts");
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].name, "greet");
        // The stored JSON carries the engine's own defaults, which is why "the definition
        // round-trips unchanged" cannot be object equality — asserted here so a changed
        // default is caught even if no route compares it.
        let steps = parsed.steps_json().expect("steps serialise");
        assert_eq!(steps[0]["on_error"], "inherit");
        assert_eq!(steps[0]["max_attempts"], 1);
    }

    fn definition_with(steps: Value) -> omnion_workflows::definition::WorkflowDefinition {
        serde_json::from_value(json!({ "trigger": { "kind": "manual" }, "steps": steps }))
            .expect("a definition the engine accepts")
    }

    #[test]
    fn a_definition_of_synthetic_actions_needs_no_account_at_all() {
        // The engine runs `noop`, `echo`, `fail` and `transient` itself. A draft built from
        // them has to be approvable by anybody holding `workflows.manage`, or the engine's
        // own test definitions could never be reviewed by a person.
        let definition = definition_with(json!([
            { "name": "one", "kind": "task", "action": "noop", "params": {} },
            { "name": "two", "kind": "task", "action": "echo", "params": { "value": "hi" } },
        ]));
        assert!(required_permissions(&definition).is_empty());
    }

    #[test]
    fn a_host_step_names_the_permission_it_needs_and_the_map_is_the_engine_s() {
        // Asserted against `permission_for` itself rather than against a literal: a copy of
        // this list is a second answer to "what does send_email need", and the copy is the
        // one a reader would trust.
        let definition = definition_with(json!([
            { "name": "mail", "kind": "task", "action": "send_email",
              "params": { "to": "a@b.test", "subject": "s", "body": "b" } },
            { "name": "model", "kind": "task", "action": "ai.prompt",
              "params": { "prompt": "summarise this" } },
        ]));
        let required = required_permissions(&definition);
        assert_eq!(required.len(), 2, "{required:?}");
        assert_eq!(
            required[0].1,
            omnion_automation::authority::permission_for("send_email").expect("mapped")
        );
        assert_eq!(required[1].1, "ai.chat");
    }

    #[test]
    fn the_same_permission_twice_is_reported_once() {
        // A definition that emails and then posts a comment needs two grants; one that
        // emails twice needs one. Collecting per step rather than per permission turns the
        // refusal message into a list with the same line in it twice.
        let definition = definition_with(json!([
            { "name": "first", "kind": "task", "action": "send_email",
              "params": { "to": "a@b.test", "subject": "s", "body": "b" } },
            { "name": "second", "kind": "task", "action": "http_request",
              "params": { "method": "GET", "url": "https://example.test" } },
        ]));
        // Both are `workflows.run`, so one entry, and the first-seen action is the one named.
        let required = required_permissions(&definition);
        assert_eq!(required.len(), 1, "{required:?}");
        assert_eq!(required[0].1, "workflows.run");
    }

    #[test]
    fn a_credential_shaped_parameter_is_refused_on_the_save_path_too() {
        // The save path uses the module's validator, not only the engine's: an operator may
        // type what a model would not. A save that skipped this check would make the
        // "never carries a secret" criterion true of generation and false of editing.
        let with_secret = json!({
            "trigger": { "kind": "manual" },
            "steps": [{ "name": "call", "kind": "task", "action": "echo",
                        "params": { "value": "hi", "api_key": "sk-live-123" } }],
        });
        let error = ai::definition::validate(&with_secret)
            .expect_err("a credential in a definition is refused");
        assert_eq!(error.code(), "secret_in_definition");

        let clean = json!({
            "trigger": { "kind": "manual" },
            "steps": [{ "name": "call", "kind": "task", "action": "echo",
                        "params": { "value": "hi" } }],
        });
        assert!(
            ai::definition::validate(&clean).is_ok(),
            "a definition with no credential is accepted"
        );
    }
}
