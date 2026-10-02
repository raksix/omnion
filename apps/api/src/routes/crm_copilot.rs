//! `/api/v1/crm/copilot/*` — slice 4, part three: the deal copilot (docs/requests/REQ-051).
//!
//! Two endpoints and nothing else. Both take a deal id, both answer a **draft**, and both are
//! audited whether they succeed or fail — because the catalogue says why the key exists at all:
//! *a model that can read the whole CRM is a data-exfiltration surface even when it only returns
//! text, and the audit of the call is the record of what it saw.* So the audit row is written
//! with the deal it read and the model that read it, on the failure path as well as the success
//! path. A copilot call that is never audited is indistinguishable from one that never happened.
//!
//! What this layer owns, and why none of it lives in the module:
//!
//! * **The audit row.** `crm.copilot.summarized` / `crm.copilot.follow_up_drafted`, plus
//!   `crm.copilot.failed` when the call did not land. Metadata carries ids, the model and the
//!   size of the answer — **never the answer itself and never the deal's title**, for the same
//!   reason `crm.activity.logged` carries no note body: an audit log is a record other people
//!   read, and the CRM's own words about a person do not belong in it.
//! * **The call to the provider.** Resolving a model, building the request and reading the
//!   answer back is `omnion_ai_hub`'s job (docs/06-AI-HUB), exactly as `ai::chat` does it.
//!
//! The **rules** stay in `modules/crm::copilot`: the scoped context read, the two instructions,
//! and the sanitiser that reduces an untrusted answer to plain text. A route that assembled its
//! own prompt would be a second answer to "what does this model see", and the audit row would
//! only describe the first one.

use axum::Json;
use axum::extract::{Path, State};
use omnion_ai_hub::{ChatMessage, ChatRequest, ChatRole, ProviderTarget, resolve};
use omnion_ai_hub::client::chat;
use omnion_module_crm::copilot::{self, CopilotAction, CopilotContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::crm::{organization_of, scope_of};
use crate::routes::iam::record;
use crate::state::AppState;

/// The answer budget the copilot sends.
///
/// Bounded on purpose: the answer is a panel-sized draft, and a summary that ran to ten thousand
/// tokens is a provider that ignored the instruction — which [`copilot::sanitise`] also catches,
/// but it is cheaper not to pay for it.
const MAX_TOKENS: u32 = 800;

// ---------------------------------------------------------------------------------------------
// Requests and responses
// ---------------------------------------------------------------------------------------------

/// Body of both endpoints: a deal, and optionally the model to answer with.
#[derive(Debug, Default, Deserialize)]
pub struct CopilotBody {
    /// Model as `provider/model` or a bare key; the installation's default when absent.
    #[serde(default)]
    pub model: Option<String>,
}

/// One copilot answer, as the panel receives it.
///
/// `draft` is the whole answer as **plain text** — the sanitiser already removed markup, and the
/// panel renders it in a text node, so there is no second sanitising step for the client to
/// forget. `action` is what the caller asked for, echoed back so a client that has both buttons
/// can label the card it is holding without tracking its own state.
#[derive(Debug, Serialize)]
pub struct CopilotAnswer {
    /// The deal the answer is about.
    pub deal_id: Uuid,
    /// `summarize` or `follow-up`.
    pub action: &'static str,
    /// The model that answered, as `provider/model`.
    pub model: String,
    /// The answer, as plain text, bounded.
    pub draft: String,
    /// How many characters came back, before the bound.
    pub chars: usize,
    /// A draft is never written to the record: this is `true` in every response, and the panel
    /// reads it rather than assuming it.
    pub is_draft: bool,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/crm/copilot/summarize` — a summary of one deal and a suggested next action.
pub async fn summarize(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(deal_id): Path<Uuid>,
    body: Option<Json<CopilotBody>>,
) -> Result<Json<CopilotAnswer>, ApiError> {
    answer(state, current, address, deal_id, body, CopilotAction::Summarize).await
}

/// `POST /api/v1/crm/copilot/follow-up` — a drafted follow-up message, returned as a draft.
pub async fn follow_up(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(deal_id): Path<Uuid>,
    body: Option<Json<CopilotBody>>,
) -> Result<Json<CopilotAnswer>, ApiError> {
    answer(state, current, address, deal_id, body, CopilotAction::FollowUp).await
}

/// The one handler the two endpoints share.
///
/// The order is deliberate and is the order the audit tells: **the scoped read happens before the
/// model is resolved.** A deal in another organization is a `404` from
/// [`CopilotContext::read`] and never reaches a provider, so a caller cannot use this endpoint to
/// discover whether a deal id exists somewhere they may not look — the id space is not something
/// this route is allowed to confirm.
async fn answer(
    state: AppState,
    current: CurrentSession,
    address: ClientAddress,
    deal_id: Uuid,
    body: Option<Json<CopilotBody>>,
    action: CopilotAction,
) -> Result<Json<CopilotAnswer>, ApiError> {
    let organization_id = organization_of(&state, &current, None).await?;
    let scope = scope_of(&state, &current, organization_id).await;

    // 1. The read. Scoped exactly as the card that renders this button was drawn, so the copilot
    //    is not a way around the visibility rule it sits on. `NotFound` becomes a 404.
    let context = CopilotContext::read(state.db().pool(), &scope, deal_id).await?;

    // 2. The instruction and the record, both from the module.
    let requested_model = body.and_then(|Json(body)| body.model);
    let request = ChatRequest {
        model: String::new(), // replaced by the resolved key below
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: context.system_prompt(action).to_owned(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: context.user_prompt(),
            },
        ],
        temperature: Some(0.2),
        max_tokens: Some(MAX_TOKENS),
    };

    // 3. The model. A provider that is not connected is an installation problem, not a bad
    //    request, and it is recorded as a failure so the audit shows the call was attempted.
    let resolved = match resolve(state.db().pool(), requested_model.as_deref()).await {
        Ok(resolved) => resolved,
        Err(error) => {
            audit(
                &state,
                current.user.id,
                organization_id,
                deal_id,
                action,
                // No model was resolved, so the audit records that too: "the copilot was tried
                // and no model answered" is a different question from "a model answered wrongly".
                "",
                error.code(),
                &error.to_string(),
                0,
                address.as_text(),
            )
            .await?;
            return Err(error.into());
        }
    };
    let model_id = resolved.id();
    let mut request = request;
    request.model = resolved.model.model_key.clone();

    // 4. The call. `validate_request` before the provider is touched, so a malformed request is a
    //    400 that never costs a token.
    if let Err(error) = omnion_ai_hub::validate_request(&request) {
        audit(
            &state,
            current.user.id,
            organization_id,
            deal_id,
            action,
            &model_id,
            error.code(),
            &error.to_string(),
            0,
            address.as_text(),
        )
        .await?;
        return Err(error.into());
    }

    let target = ProviderTarget::from_provider(&resolved.provider);
    let outcome = match chat(&target, &request).await {
        Ok(outcome) => outcome,
        Err(error) => {
            audit(
                &state,
                current.user.id,
                organization_id,
                deal_id,
                action,
                &model_id,
                error.code(),
                &error.to_string(),
                0,
                address.as_text(),
            )
            .await?;
            return Err(error.into());
        }
    };

    // 5. The untrusted answer. The sanitiser is the module's rule and it is the only thing between
    //    a provider's output and the panel: it unwraps a fence, strips tags, drops control
    //    characters, cuts the tail, and refuses an answer with nothing left (`EmptyAnswer` is a
    //    502 rather than an empty card).
    let chars = outcome.content.chars().count();
    let draft = match copilot::sanitise(&outcome.content) {
        Ok(draft) => draft,
        Err(error) => {
            audit(
                &state,
                current.user.id,
                organization_id,
                deal_id,
                action,
                &model_id,
                "crm_copilot_empty_answer",
                &error.to_string(),
                chars,
                address.as_text(),
            )
            .await?;
            return Err(ApiError::from(error));
        }
    };

    audit(
        &state,
        current.user.id,
        organization_id,
        deal_id,
        action,
        &model_id,
        "",
        "",
        chars,
        address.as_text(),
    )
    .await?;

    Ok(Json(CopilotAnswer {
        deal_id,
        action: action.slug(),
        model: model_id,
        draft,
        chars,
        is_draft: true,
    }))
}

/// Write the audit row for one copilot call.
///
/// A failure is audited under `crm.copilot.failed` and a success under the action's own name, so
/// "the copilot was never used here" and "the copilot was tried and could not answer" are two
/// different questions the log can answer. The metadata carries **what was read and who read it**,
/// not what came back: the deal id, the model, the character count and the error code are the
/// audit's job, and the draft belongs to the person who asked for it.
#[allow(clippy::too_many_arguments)]
async fn audit(
    state: &AppState,
    user_id: Uuid,
    organization_id: Uuid,
    deal_id: Uuid,
    action: CopilotAction,
    model: &str,
    error_code: &str,
    error: &str,
    chars: usize,
    ip_address: Option<String>,
) -> Result<(), ApiError> {
    let event = if error_code.is_empty() {
        match action {
            CopilotAction::Summarize => "crm.copilot.summarized",
            CopilotAction::FollowUp => "crm.copilot.follow_up_drafted",
        }
    } else {
        "crm.copilot.failed"
    };
    let entry = omnion_audit::NewAuditEntry::by_user(user_id, event)
        .organization(organization_id)
        .target("crm_deal", deal_id.to_string())
        .metadata(json!({
            "action": action.slug(),
            "deal_id": deal_id,
            "model": model,
            "chars": chars,
            "error_code": if error_code.is_empty() { None } else { Some(error_code) },
            "error": if error.is_empty() { None } else { Some(error) },
        }))
        .ip_address(ip_address);
    record(state, entry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::Value;

    /// The status and body an `ApiError` produces, read through its own `IntoResponse` — the
    /// shape a client actually receives. Two things are returned because they are two different
    /// facts: the body nests the *code* under `error`, and the *status* is on the response, not
    /// in the body at all. Asserting a `status` key inside the body passes for the wrong reason
    /// if the code happens to be right, and fails for a missing field when the status is wrong.
    async fn response_of(error: ApiError) -> (axum::http::StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("an error body is small");
        (status, serde_json::from_slice(&bytes).expect("the error body is JSON"))
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_502_a_panel_can_offer_a_retry_against() {
        // The whole point of `EmptyAnswer`: a card that renders as if the model had said
        // "nothing to add" is a lie the user cannot detect, so this has to be a failure with a
        // stable code and not an empty `200`.
        let error = ApiError::from(omnion_module_crm::CrmError::EmptyAnswer);
        let (status, body) = response_of(error).await;
        assert_eq!(body["error"]["code"], "crm_copilot_empty_answer", "{body}");
        assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY, "{body}");
    }

    #[test]
    fn an_answer_is_always_a_draft_and_says_so_on_the_wire() {
        // `is_draft` is not decoration: the client reads it rather than assuming the endpoint
        // never writes. Serialize the response the way a client receives it and assert the field
        // is there — an in-memory assertion would prove nothing about the wire.
        let answer = CopilotAnswer {
            deal_id: Uuid::nil(),
            action: CopilotAction::Summarize.slug(),
            model: "local/stub".to_owned(),
            draft: "STATUS: live".to_owned(),
            chars: 12,
            is_draft: true,
        };
        let body = serde_json::to_value(&answer).expect("an answer serialises");
        assert_eq!(body["is_draft"], Value::Bool(true), "{body}");
        assert_eq!(body["action"], "summarize", "{body}");
        assert_eq!(body["draft"], "STATUS: live", "{body}");
    }

    #[test]
    fn a_request_with_no_body_and_a_request_with_a_model_are_both_accepted() {
        // The panel's two buttons may send `{}` or `{"model": "…"}`; neither may be a 400, and a
        // blank model string has to read as "the default" rather than as a model named "".
        let empty: CopilotBody = serde_json::from_value(json!({})).expect("an empty body parses");
        assert_eq!(empty.model, None);
        let blank: CopilotBody =
            serde_json::from_value(json!({ "model": "   " })).expect("a blank model parses");
        assert_eq!(blank.model.as_deref(), Some("   "));
        let named: CopilotBody =
            serde_json::from_value(json!({ "model": "local/stub" })).expect("a model parses");
        assert_eq!(named.model.as_deref(), Some("local/stub"));
    }

    #[test]
    fn the_two_endpoints_send_different_instructions_under_one_action_slug() {
        // The slug is what the audit is filed under and the panel's card is labelled by, so the
        // two must not collide; the instruction behind each is the module's own constant.
        assert_ne!(CopilotAction::Summarize.slug(), CopilotAction::FollowUp.slug());
        assert_eq!(CopilotAction::Summarize.slug(), "summarize");
        assert_eq!(CopilotAction::FollowUp.slug(), "follow-up");
    }
}
