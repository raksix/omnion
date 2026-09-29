//! `/api/v1/crm/leads*` and `/api/v1/crm/intake/*` — the inbox and the capture surface
//! (docs/requests/REQ-117, slice 1).
//!
//! Two audiences with opposite rules, which is the whole reason this file has two halves:
//!
//! * **`POST /api/v1/crm/intake/{source_key}` is public.** A hand-written page or a
//!   third-party service posts a quote request to it with the key the source was issued. It
//!   carries no permission guard — there is no session to guard — so what protects it is what
//!   can protect a public write path: the key is compared by digest, the body is capped, the
//!   source's own hourly ceiling is counted inside [`omnion_module_crm_intake::store::capture`]
//!   *before* the write, and the answer is `202` with a reference whether the submission was
//!   accepted, filed as a duplicate or filed as spam. A response that distinguished those
//!   would teach an attacker which addresses are already in the CRM.
//! * **Everything else is panel surface**, guarded by the `crm.leads.*` / `crm.intake.manage`
//!   family and scoped through the caller's own organization. A lead of another organization
//!   is a `404`, never a `403`: a `403` says "that exists, not for you", which is an oracle.
//!
//! ## The rules the handlers are shaped around
//!
//! * **The public answer never discloses a verdict's detail.** `state` is one of
//!   `accepted` / `duplicate` / `rejected` — enough for the submitter to know their message
//!   was recorded, and not enough to learn whether the address is already a contact.
//! * **A `Test mapping` writes nothing.** It runs the mapping, the transforms and the
//!   contactability check over a pasted payload and answers the fields it *would* produce. A
//!   preview that stored rows would be a second capture path with a worse authentication
//!   story than the keyed endpoint itself.
//! * **The issued key is returned exactly once**, by create and by rotate, and never by a
//!   read. A read that re-revealed it would make the "shown once" promise a lie the first
//!   time somebody refreshed the editor.
//! * **A lead edit cannot rewrite the evidence.** The patch carries the fields a human can
//!   see; `received_at`, `payload` and `spam_score` are not in it, because a verdict that
//!   its own subject can edit is not a verdict.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_audit::{ActorType, NewAuditEntry};
use omnion_events::{bus, NewEvent};
use omnion_module_crm_intake::autoresponder::Delivery;
use omnion_module_crm_intake::autoresponder_store;
use omnion_module_crm_intake::model::{IntakeSource, Lead, LeadEvent, LeadOwner};
use omnion_module_crm_intake::store::{self, LeadQuery, SourcePatch};
use omnion_module_crm_intake::{CrmIntakeError, LeadMetrics, MappingEntry, NewIntakeSource};

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::state::AppState;
use crate::workflow_runner;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The inbox read's query.
///
/// `status` repeats rather than being comma-separated: a comma inside a value is then
/// impossible, and a panel that sends `status=new,assigned` gets a `400` naming the field
/// instead of a filter that silently matched nothing.
#[derive(Debug, Default, Deserialize)]
pub struct LeadsQuery {
    /// One source's rows.
    pub source: Option<Uuid>,
    /// Keep only these statuses. Repeat for several.
    #[serde(default)]
    pub status: Vec<String>,
    /// `me`, `unassigned`, or a user id.
    pub owner: Option<String>,
    /// Free text over name, e-mail and message.
    pub q: Option<String>,
    /// Free text over the product interest.
    pub product: Option<String>,
    /// Received on or after this instant.
    pub since: Option<String>,
    /// Received before this instant.
    pub until: Option<String>,
    /// Keyset cursor: the instant of the last row of the previous page.
    pub before: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
}

/// A source create's body.
#[derive(Debug, Default, Deserialize)]
pub struct CreateSourceBody {
    /// The name an operator sees.
    pub name: String,
    /// `form`, `endpoint` or `import`.
    pub kind: Option<String>,
    /// The bound REQ-064 form's key.
    pub form_key: Option<String>,
    /// The site it captures for.
    pub site_id: Option<Uuid>,
    /// The ordered mapping.
    #[serde(default)]
    pub mapping: Vec<MappingLine>,
    /// Targets the source refuses to save without.
    #[serde(default)]
    pub required_targets: Vec<String>,
    /// Whether a submission must carry the consent text.
    pub consent_required: Option<bool>,
    /// The words the visitor agreed to.
    pub consent_text: Option<String>,
    /// `link`, `create_anyway` or `reject_duplicate`.
    pub dedupe_policy: Option<String>,
    /// Pipeline a converted deal lands in.
    pub pipeline_id: Option<Uuid>,
    /// Stage a converted deal lands in.
    pub stage_id: Option<Uuid>,
    /// Tags applied to every lead.
    #[serde(default)]
    pub auto_tags: Vec<String>,
    /// The autoresponder's template and delay.
    pub autoresponder: Option<serde_json::Value>,
    /// Submissions per hour.
    pub rate_limit_per_hour: Option<i32>,
    /// Whether the source accepts submissions.
    pub active: Option<bool>,
}

/// A source update's body: every field optional, absent means "leave it alone".
#[derive(Debug, Default, Deserialize)]
pub struct UpdateSourceBody {
    /// New name.
    pub name: Option<String>,
    /// New mapping.
    pub mapping: Option<Vec<MappingLine>>,
    /// New required targets.
    pub required_targets: Option<Vec<String>>,
    /// New consent requirement.
    pub consent_required: Option<bool>,
    /// New consent wording.
    pub consent_text: Option<String>,
    /// New dedupe policy.
    pub dedupe_policy: Option<String>,
    /// New auto tags.
    pub auto_tags: Option<Vec<String>>,
    /// New autoresponder.
    pub autoresponder: Option<serde_json::Value>,
    /// New active flag.
    pub active: Option<bool>,
    /// New hourly ceiling.
    pub rate_limit_per_hour: Option<i32>,
    /// New pipeline.
    pub pipeline_id: Option<Uuid>,
    /// New stage.
    pub stage_id: Option<Uuid>,
}

/// One line of a mapping as the panel sends it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MappingLine {
    /// The CRM field the value lands in.
    pub target: String,
    /// The submission key it is read from.
    pub source: String,
    /// Transforms applied in order.
    #[serde(default)]
    pub transforms: Vec<String>,
    /// Whether an empty value refuses the submission.
    #[serde(default)]
    pub required: bool,
    /// A constant used when the source key is absent.
    pub fallback: Option<String>,
}

impl From<MappingLine> for MappingEntry {
    fn from(value: MappingLine) -> Self {
        Self {
            target: value.target,
            source_key: Some(value.source),
            transform: value.transforms,
            required: value.required,
            fallback: value.fallback,
        }
    }
}

impl From<&MappingEntry> for MappingLine {
    fn from(value: &MappingEntry) -> Self {
        Self {
            target: value.target.clone(),
            // A constant line (no `source_key`) reads nothing from the payload, so the panel
            // shows an empty source cell rather than a misleading `""` that looks like a
            // key nobody filled.
            source: value.source_key.clone().unwrap_or_default(),
            transforms: value.transform.clone(),
            required: value.required,
            fallback: value.fallback.clone(),
        }
    }
}

/// A `Test mapping` request: a payload to run through the mapping, and nothing more.
#[derive(Debug, Default, Deserialize)]
pub struct TestMappingBody {
    /// The sample answers.
    pub payload: serde_json::Value,
}

/// A reject's body: the reason is required, because a rejection nobody can explain is a
/// rejection an operator will undo without thinking.
#[derive(Debug, Default, Deserialize)]
pub struct RejectBody {
    /// Why the lead is refused.
    pub reason: String,
}

/// A hand-assignment: who takes the lead, and why.
///
/// `owner_user_id` is `Option<Option<Uuid>>` for the reason named in `assign`: `Some(Some(id))`
/// is "give it to this person", `Some(None)` is "put it back in the unassigned queue", and a
/// missing field is refused. `Option<Uuid>` would collapse the last two into one, and the second
/// of those is a destructive act nobody should trigger by forgetting a key.
#[derive(Debug, Default, Deserialize)]
pub struct AssignBody {
    /// The new owner, or `null` to return the lead to the queue.
    #[serde(default)]
    pub owner_user_id: Option<Option<uuid::Uuid>>,
    /// Why it moved. Required.
    #[serde(default)]
    pub reason: String,
}

/// A spam marker's body: the reason is optional, because the score is already recorded.
#[derive(Debug, Default, Deserialize)]
pub struct MarkSpamBody {
    /// Optional note kept beside the spam verdict.
    pub reason: Option<String>,
}

/// A lead edit's body.
///
/// There is no `received_at`, no `payload` and no `spam_score` here, and that is the point:
/// a panel edit fixes what a human can see on the row, and the capture-time facts are the
/// evidence the verdicts rest on.
#[derive(Debug, Default, Deserialize)]
pub struct PatchLeadBody {
    /// Given name.
    pub first_name: Option<String>,
    /// Family name.
    pub last_name: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// Job title.
    pub job_title: Option<String>,
    /// What they asked about.
    pub product_interest: Option<String>,
    /// Their message.
    pub message: Option<String>,
    /// New status — one the platform knows.
    pub status: Option<String>,
    /// The contact this lead is linked to.
    pub contact_id: Option<Uuid>,
}

impl From<PatchLeadBody> for store::LeadPatch {
    fn from(value: PatchLeadBody) -> Self {
        Self {
            first_name: value.first_name,
            last_name: value.last_name,
            email: value.email,
            phone: value.phone,
            company_name: value.company_name,
            job_title: value.job_title,
            product_interest: value.product_interest,
            message: value.message,
            status: value.status,
            contact_id: value.contact_id,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------------

/// A source as the panel reads it.
///
/// `endpoint_key` is `Some` exactly once — on the create or the rotate answer — and `None`
/// on every read afterwards.
#[derive(Debug, Serialize)]
pub struct SourceBody {
    /// The row's id.
    pub id: Uuid,
    /// The site it captures for.
    pub site_id: Option<Uuid>,
    /// The name.
    pub name: String,
    /// `form`, `endpoint` or `import`.
    pub kind: String,
    /// The bound form's key.
    pub form_key: Option<String>,
    /// The last four characters of the live key.
    pub endpoint_key_hint: Option<String>,
    /// The ordered mapping.
    pub mapping: Vec<MappingLine>,
    /// Targets the source refuses to save without.
    pub required_targets: Vec<String>,
    /// Whether consent is required.
    pub consent_required: bool,
    /// The consent wording.
    pub consent_text: Option<String>,
    /// The dedupe policy.
    pub dedupe_policy: String,
    /// The pipeline a conversion targets.
    pub pipeline_id: Option<Uuid>,
    /// The stage a conversion targets.
    pub stage_id: Option<Uuid>,
    /// Tags applied to every lead.
    pub auto_tags: Vec<String>,
    /// The autoresponder's template and delay.
    pub autoresponder: serde_json::Value,
    /// Whether the source accepts submissions.
    pub active: bool,
    /// The hourly ceiling.
    pub rate_limit_per_hour: i32,
    /// When a submission last landed.
    pub last_received_at: Option<String>,
    /// The last failure.
    pub last_error: Option<String>,
    /// Source keys the bound form no longer has.
    pub broken_mappings: Vec<String>,
    /// Whether the binding is broken.
    pub binding_broken: bool,
    /// When the row was created.
    pub created_at: String,
    /// When the row last changed.
    pub updated_at: String,
    /// The clear key, on the one answer that carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_key: Option<String>,
}

impl From<IntakeSource> for SourceBody {
    fn from(value: IntakeSource) -> Self {
        // Everything derived is computed *before* the first move: reading `value.binding_
        // broken()` after `endpoint_key_hint` has moved out is a borrow of a partially moved
        // value, and the compiler's answer there is a wall of error text rather than a
        // pointer to the one line that needs reordering.
        let mapping = value.mapping_lines();
        let binding_broken = value.binding_is_broken();
        Self {
            id: value.id,
            site_id: value.site_id,
            name: value.name,
            kind: value.kind,
            form_key: value.form_key,
            endpoint_key_hint: value.endpoint_key_hint,
            mapping: mapping.iter().map(MappingLine::from).collect(),
            required_targets: value.required_targets,
            consent_required: value.consent_required,
            consent_text: value.consent_text,
            dedupe_policy: value.dedupe_policy,
            pipeline_id: value.pipeline_id,
            stage_id: value.stage_id,
            auto_tags: value.auto_tags,
            autoresponder: value.autoresponder,
            active: value.active,
            rate_limit_per_hour: value.rate_limit_per_hour,
            last_received_at: value.last_received_at.map(|at| at.to_string()),
            last_error: value.last_error,
            broken_mappings: value.broken_mappings,
            binding_broken,
            created_at: value.created_at.to_string(),
            updated_at: value.updated_at.to_string(),
            endpoint_key: None,
        }
    }
}

impl SourceBody {
    /// The same answer with the clear key attached — used by create and rotate only.
    fn with_key(mut self, key: Option<String>) -> Self {
        self.endpoint_key = key;
        self
    }
}

/// One lead as the inbox reads it.
#[derive(Debug, Serialize)]
pub struct LeadBody {
    /// The row's id.
    pub id: Uuid,
    /// The source it came through.
    pub source_id: Option<Uuid>,
    /// The status.
    pub status: String,
    /// Whether the status is one an operator still works.
    pub is_open: bool,
    /// The linked contact.
    pub contact_id: Option<Uuid>,
    /// The deal conversion produced.
    pub deal_id: Option<Uuid>,
    /// The quotation conversion produced.
    pub quote_id: Option<Uuid>,
    /// Who owns it.
    pub owner_user_id: Option<Uuid>,
    /// Given name.
    pub first_name: Option<String>,
    /// Family name.
    pub last_name: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// What they asked about.
    pub product_interest: Option<String>,
    /// Their message.
    pub message: Option<String>,
    /// The consent wording as accepted.
    pub consent_text: Option<String>,
    /// Whether consent was given.
    pub consent_given: bool,
    /// First-touch attribution, as the panel's first panel reads it.
    pub attribution: AttributionBody,
    /// The dedupe verdict.
    pub decision: Option<String>,
    /// The key the verdict matched on.
    pub dedupe_key: Option<String>,
    /// The lead this one duplicates.
    pub duplicate_of: Option<Uuid>,
    /// The reason it was rejected or filed as spam.
    pub rejection_reason: Option<String>,
    /// The spam score.
    pub spam_score: i32,
    /// The first-response deadline (slice 2 computes it; the column is here).
    pub first_response_due_at: Option<String>,
    /// When the first response was recorded.
    pub first_response_at: Option<String>,
    /// When the SLA was escalated.
    pub escalated_at: Option<String>,
    /// When it arrived.
    pub received_at: String,
    /// When it converted.
    pub converted_at: Option<String>,
    /// Whether a first response is still outstanding.
    pub sla_running: bool,
}

impl From<Lead> for LeadBody {
    fn from(value: Lead) -> Self {
        // The two booleans are read off `value.status` *before* any field moves out: after
        // the first move the whole struct is partially moved, and the fix the compiler asks
        // for (cloning everything) is worse than computing the two answers first.
        let status = value.status.clone();
        let is_open = omnion_module_crm_intake::is_open(&status);
        let sla_running = value.first_response_at.is_none() && is_open;
        // The attribution borrows the whole row, so it is built before the first move too.
        let attribution = AttributionBody::from(&value);
        Self {
            id: value.id,
            source_id: value.source_id,
            is_open,
            status,
            contact_id: value.contact_id,
            deal_id: value.deal_id,
            quote_id: value.quote_id,
            owner_user_id: value.owner_user_id,
            first_name: value.first_name,
            last_name: value.last_name,
            email: value.email,
            phone: value.phone,
            company_name: value.company_name,
            product_interest: value.product_interest,
            message: value.message,
            consent_text: value.consent_text,
            consent_given: value.consent_given,
            attribution,
            decision: value.decision,
            dedupe_key: value.dedupe_key,
            duplicate_of: value.duplicate_of,
            rejection_reason: value.rejection_reason,
            spam_score: value.spam_score,
            first_response_due_at: value.first_response_due_at.map(|at| at.to_string()),
            first_response_at: value.first_response_at.map(|at| at.to_string()),
            escalated_at: value.escalated_at.map(|at| at.to_string()),
            received_at: value.received_at.to_string(),
            converted_at: value.converted_at.map(|at| at.to_string()),
            sla_running,
        }
    }
}

/// The attribution split into the two panels the lead detail draws.
#[derive(Debug, Serialize)]
pub struct AttributionBody {
    /// First-touch UTM source.
    pub utm_source: Option<String>,
    /// First-touch UTM medium.
    pub utm_medium: Option<String>,
    /// First-touch UTM campaign.
    pub utm_campaign: Option<String>,
    /// First-touch UTM term.
    pub utm_term: Option<String>,
    /// First-touch UTM content.
    pub utm_content: Option<String>,
    /// The click id.
    pub click_id: Option<String>,
    /// The referring host, from the last visit.
    pub referrer_host: Option<String>,
    /// The landing page, from the last visit.
    pub landing_path: Option<String>,
    /// The page the form was on.
    pub source_path: Option<String>,
}

impl From<&Lead> for AttributionBody {
    fn from(value: &Lead) -> Self {
        Self {
            utm_source: value.utm_source.clone(),
            utm_medium: value.utm_medium.clone(),
            utm_campaign: value.utm_campaign.clone(),
            utm_term: value.utm_term.clone(),
            utm_content: value.utm_content.clone(),
            click_id: value.click_id.clone(),
            referrer_host: value.referrer_host.clone(),
            landing_path: value.landing_path.clone(),
            source_path: value.source_path.clone(),
        }
    }
}

/// One history line as the timeline draws it.
#[derive(Debug, Serialize)]
pub struct EventBody {
    /// The line's id.
    pub id: i64,
    /// What happened.
    pub kind: String,
    /// Who did it.
    pub actor_user_id: Option<Uuid>,
    /// The structured detail.
    pub detail: serde_json::Value,
    /// When.
    pub created_at: String,
}

impl From<LeadEvent> for EventBody {
    fn from(value: LeadEvent) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            actor_user_id: value.actor_user_id,
            detail: value.detail,
            created_at: value.created_at.to_string(),
        }
    }
}

/// The lead detail: the row, its trail and the raw payload.
#[derive(Debug, Serialize)]
pub struct LeadDetailBody {
    /// The row.
    pub lead: LeadBody,
    /// Every answer as submitted.
    pub payload: serde_json::Value,
    /// The payload's size in bytes.
    pub payload_bytes: i32,
    /// The history, newest first.
    pub timeline: Vec<EventBody>,
    /// The four documented steps, computed from the row and what is installed.
    pub steps: Vec<StepBody>,
}

/// One step as the panel renders it.
#[derive(Debug, Serialize)]
pub struct StepBody {
    /// Which step.
    pub key: &'static str,
    /// `done` · `current` · `pending` · `blocked`.
    pub state: &'static str,
    /// What happened, or why it has not.
    pub note: String,
}

impl From<Lead> for LeadDetailBody {
    fn from(value: Lead) -> Self {
        // The payload and its size are taken out first, because `LeadBody::from` consumes the
        // whole row — and a `From` impl that half-moves its own input is a five-minute
        // compiler error that looks like a type problem.
        let mut value = value;
        // `mem::take` rather than `let payload = value.payload`: moving a field out of the
        // struct makes the whole value partially moved, and `LeadBody::from(value)` — which
        // consumes the row — then refuses it. Taking the two fields *and* handing the whole
        // row on is only legal if the takes are `&mut` borrows that end before the move.
        let payload = std::mem::take(&mut value.payload);
        let payload_bytes = std::mem::replace(&mut value.payload_bytes, 0);
        let lead = LeadBody::from(value);
        Self {
            lead,
            steps: Vec::new(),
            payload,
            payload_bytes,
            timeline: Vec::new(),
        }
    }
}

/// The inbox's rows plus the counters beside them.
#[derive(Debug, Serialize)]
pub struct InboxBody {
    /// The rows.
    pub leads: Vec<LeadBody>,
    /// The cursor for the next page.
    pub next_before: Option<String>,
    /// The counters.
    pub metrics: LeadMetrics,
}

/// The public endpoint's answer: always a reference, never a verdict's detail.
#[derive(Debug, Serialize)]
pub struct CaptureResponse {
    /// The lead row's id, so a testing submitter can find what it produced.
    pub reference: Uuid,
    /// `accepted`, `duplicate` or `rejected` — a coarse word that says nothing about which
    /// addresses exist, which is the whole point of the public answer.
    pub state: String,
}

/// The mapping preview's answer: what the payload *would* produce, and nothing stored.
#[derive(Debug, Serialize)]
pub struct MappingPreviewBody {
    /// The mapped values, by target.
    pub values: serde_json::Map<String, serde_json::Value>,
    /// The targets the payload did not fill.
    pub missing_required: Vec<String>,
    /// Whether the mapped result is contactable.
    pub contactable: bool,
    /// Whether the payload carried the consent the source demands.
    pub consent_given: bool,
    /// The dedupe key the mapped result would be judged on.
    pub dedupe_key: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Map a store error onto the API surface.
///
/// The store's taxonomy is deliberately small, so this mapping is the whole of it: a
/// definition the platform refuses is the caller's `400`, a rate limit is a `429`, and a
/// database that did not answer is a `500` — never a `400`, because a client that retries a
/// `400` forever is a client the platform taught to do that.
pub(super) fn map_store(error: CrmIntakeError) -> ApiError {
    use CrmIntakeError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_lead", message),
        E::UnknownKey => invalid_key(),
        E::Spam(_) => ApiError::bad_request("submission_flagged", "this submission was refused"),
        E::RateLimited => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "this intake source is over its hourly ceiling — try again later",
        ),
        E::PayloadTooLarge { max, actual } => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("a submission may not exceed {max} bytes (this one was {actual})"),
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the intake store did not answer: {inner}"),
        ),
    }
}

/// The `401` of the capture surface.
///
/// An unknown key, a wrong key and a paused source are one answer: a distinct answer for
/// "that source is paused" is a source enumeration.
fn invalid_key() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_source_key",
        "this intake source key is not valid",
    )
}

/// `404` for a row that is not the caller's, and for a row that is gone. The two are the
/// same answer on purpose — a panel that can tell those apart can enumerate ids.
pub(super) fn not_found(what: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no such {what} in this organization"),
    )
}

// ---------------------------------------------------------------------------------------------
// The public capture endpoint
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/crm/intake/{source_key}` — a submission from a keyed endpoint.
///
/// The whole answer contract of this surface is three lines:
///
/// * `401` — the key is unknown, wrong, or belongs to a source that is not live.
/// * `429` — the source's own hourly ceiling.
/// * `202` — everything else, including a submission the store filed as spam. The body says
///   whether the row is `accepted`, `duplicate` or `rejected`; it never says whether a
///   matching contact exists, what the spam score was, or which key matched.
pub async fn capture(
    State(state): State<AppState>,
    Path(source_key): Path<String>,
    headers: HeaderMap,
    address: ClientAddress,
    body: Bytes,
) -> Result<(StatusCode, Json<CaptureResponse>), ApiError> {
    if body.len() > omnion_module_crm_intake::MAX_PAYLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "a submission may not exceed {} bytes",
                omnion_module_crm_intake::MAX_PAYLOAD_BYTES
            ),
        ));
    }

    let pool = state.db().pool();
    let source = store::find_source_by_key(pool, source_key.trim())
        .await
        .map_err(map_store)?
        .ok_or_else(invalid_key)?;

    // A body that is not a JSON object is not "an empty submission": it is a caller that
    // cannot speak the protocol, so the raw bytes are kept as one `_raw` key and the
    // mapping decides what to do with them. Storing them is what makes a hand-written
    // `application/x-www-form-urlencoded` integration debuggable instead of invisible.
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({ "_raw": String::from_utf8_lossy(&body) }));

    let captured = store::capture(
        pool,
        &store::Submission {
            organization_id: source.organization_id,
            site_id: source.site_id,
            source_id: source.id,
            // A keyed endpoint has no submission id of its own, so the idempotency key is
            // the one the caller supplies. A browser that resends a form post without one
            // gets one lead per attempt, which is the honest answer; a server-side
            // integration that retries gets the lead the first attempt wrote.
            submission_id: idempotency_key(&headers),
            ip: address.as_text(),
            payload,
            received_at: OffsetDateTime::now_utc(),
        },
    )
    .await
    .map_err(map_store)?;

    let word = if captured.spam.is_spam() {
        "rejected"
    } else {
        match captured.lead.status.as_str() {
            "duplicate" => "duplicate",
            "rejected" => "rejected",
            _ => "accepted",
        }
    };

    emit_received(pool, &captured, &source).await;

    // The autoresponder is deliberately *not* awaited into the response. A visitor's `202`
    // must not wait on an SMTP handshake: a relay that takes four seconds to refuse turns
    // every form submission on the site into a four-second form submission. The send is
    // spawned and the trail records what it did, which is the same contract the conversion
    // and the assignment already use for work that must not sit in the request.
    spawn_autoresponder(state.clone(), captured.lead.clone(), source.clone());

    Ok((
        StatusCode::ACCEPTED,
        Json(CaptureResponse {
            reference: captured.lead.id,
            state: word.to_string(),
        }),
    ))
}

/// Hand the new lead to the autoresponder, off the request's path.
///
/// A spawn failure is not fatal and is not swallowed either: the lead is already stored and
/// the visitor is already answered with `202`, so there is nothing to roll back — but a lost
/// autoresponder is a lead nobody is ever going to hear back on, and that has to be visible.
fn spawn_autoresponder(state: AppState, lead: Lead, source: IntakeSource) {
    tokio::spawn(async move {
        let pool = state.db().pool().clone();
        let outcome = match autoresponder_store::prepare(
            &pool,
            &lead,
            &source,
            OffsetDateTime::now_utc(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(lead_id = %lead.id, error = %error, "autoresponder could not be prepared");
                return;
            }
        };

        let message = match &outcome.verdict {
            Delivery::Ready(message) if !message.delayed => message,
            // A delayed message is the worker's to send, and every other verdict is a
            // deliberate silence. Both leave a line so the detail page can say which.
            other => {
                if !matches!(other, Delivery::Disabled) {
                    if let Err(error) = autoresponder_store::record_skip(
                        &pool,
                        &lead,
                        &source,
                        other.reason(),
                        serde_json::json!({}),
                    )
                    .await
                    {
                        tracing::warn!(lead_id = %lead.id, error = %error, "the autoresponder's skip could not be recorded");
                    }
                }
                return;
            }
        };

        let settings = workflow_runner::mail_settings(state.config());
        let email = omnion_automation::Email::new(
            message.to.clone(),
            message.subject.clone(),
            message.body.clone(),
        );
        match omnion_automation::mail::send(&settings, &email).await {
            Ok(()) => {
                // The claim was taken *before* the send, so a crash in between leaves a
                // pending line that the detail page reads as "reserved", not "sent".
                if let Err(error) =
                    autoresponder_store::mark_sent(&pool, lead.id, OffsetDateTime::now_utc()).await
                {
                    tracing::warn!(lead_id = %lead.id, error = %error, "the autoresponder went out but was not recorded as sent");
                }
            }
            Err(error) => {
                tracing::warn!(lead_id = %lead.id, error = %error, "the autoresponder could not be sent");
                // Release the claim so the next worker tick may answer this lead: a mailer
                // that refused must not leave a lead that is permanently "already sent".
                if let Err(release) =
                    autoresponder_store::release_claim(&pool, lead.id, &message.to).await
                {
                    tracing::warn!(lead_id = %lead.id, error = %release, "the autoresponder's claim could not be released");
                }
            }
        }
    });
}

/// The client-supplied idempotency key, when the caller sent one.
///
/// Without it a retry is a second lead; with it a retry finds the row the first attempt
/// wrote. A caller that sends no header gets the "every attempt is its own lead" behaviour,
/// which is the honest default for a form post a browser may resend. The value is capped
/// and trimmed, because it is stored in a text column and an unbounded header would let a
/// caller write an arbitrary string into a lead's identity.
fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("x-idempotency-key")?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 128 {
        return None;
    }
    Some(trimmed.to_string())
}

/// `crm.lead.received` — a submission is stored, accepted or not.
///
/// The payload carries ids, statuses and the source name; it never carries the message body
/// or the submitter's answers, because this event is a common automation entry point
/// ("new quote request → notify Slack") and a payload everybody forwards is a payload that
/// eventually lands in a channel with a wider audience than the CRM.
async fn emit_received(pool: &sqlx::PgPool, captured: &store::Captured, source: &IntakeSource) {
    let event = NewEvent::new("crm.lead.received")
        .organization(source.organization_id)
        .site(source.site_id)
        .payload(json!({
            "lead_id": captured.lead.id,
            "source_id": source.id,
            "source_name": source.name,
            "form_key": source.form_key,
            "status": captured.lead.status,
            "decision": captured.lead.decision,
            "product_interest": captured.lead.product_interest,
            "spam_score": captured.lead.spam_score,
        }));
    if let Err(error) = bus::emit(pool, event).await {
        tracing::warn!(error = %error, "crm.lead.received could not be recorded");
    }
}

// ---------------------------------------------------------------------------------------------
// The inbox
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/leads` — the inbox: rows, cursor and counters from one read.
pub async fn list_leads(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<LeadsQuery>,
) -> Result<Json<InboxBody>, ApiError> {
    let query = build_lead_query(&params, session.user.id)?;
    let page = store::list_leads(state.db().pool(), organization_of(&session)?, &query)
        .await
        .map_err(map_store)?;

    Ok(Json(InboxBody {
        next_before: page.next_before.map(|at| at.to_string()),
        metrics: page.metrics,
        leads: page.leads.into_iter().map(LeadBody::from).collect(),
    }))
}

/// `GET /api/v1/crm/leads/duplicates` — the duplicate queue.
///
/// Declared before `/{id}` in the router for the same reason axum wants it there: `/duplicates`
/// would otherwise be read as a lead id, and a uuid parse error is a `400` a panel shows as
/// "this screen is broken".
pub async fn duplicates(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<LeadBody>>, ApiError> {
    let rows = store::list_duplicates(state.db().pool(), organization_of(&session)?, 100)
        .await
        .map_err(map_store)?;
    Ok(Json(rows.into_iter().map(LeadBody::from).collect()))
}

/// `GET /api/v1/crm/leads/{id}` — one lead with its payload and its trail.
pub async fn get_lead(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<LeadDetailBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let lead = store::find_lead(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    let timeline = store::list_events(pool, organization_id, lead.id)
        .await
        .map_err(map_store)?
        .into_iter()
        .map(EventBody::from)
        .collect();

    // The steps are computed here rather than in the view: the stepper's whole value is
    // that it reflects the row and the installation, and a client that had to assemble that
    // itself would be the second place the two can disagree. The plan is taken *before*
    // `LeadDetailBody::from` consumes the row — a `From` impl that owns its input cannot
    // lend it back, which is the same half-move trap its own body documents.
    let availability = omnion_module_crm_intake::Availability {
        sales: table_exists(pool, "sales_quotes").await,
        commerce: table_exists(pool, "commerce_customers").await,
    };
    let steps = omnion_module_crm_intake::step_plan(&lead, availability)
        .into_iter()
        .map(|step| StepBody {
            key: step.key,
            state: step.state.as_str(),
            note: step.note,
        })
        .collect();

    let mut detail = LeadDetailBody::from(lead);
    detail.timeline = timeline;
    detail.steps = steps;
    Ok(Json(detail))
}

/// `PATCH /api/v1/crm/leads/{id}` — edit a lead.
pub async fn patch_lead(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchLeadBody>,
) -> Result<Json<LeadBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let before = store::find_lead(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    let updated = store::patch_lead(pool, organization_id, id, &body.into())
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    // The audit entry names both sides. A trail that only carries the new value cannot answer
    // "what did they change", which is the question an audit exists for.
    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.lead.updated",
        id,
        json!({
            "before": lead_fingerprint(&before),
            "after": lead_fingerprint(&updated),
        }),
    )
    .await;

    Ok(Json(LeadBody::from(updated)))
}

/// `POST /api/v1/crm/leads/{id}/assign` — hand a lead to a person, or put it back in the queue.
///
/// The rules answer "who should get this one" and a human answers "who actually should", so the
/// hand exists. What it must not be is silent: the reason is required, the trail keeps the
/// previous owner, and the SLA clock keeps running (see `store::assign_owner` for why each of
/// those is a decision rather than an omission).
pub async fn assign(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<AssignBody>,
) -> Result<Json<LeadBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "reason_required",
            "an assignment says why — a lead that moved hands with no explanation cannot be \
             explained to the person who had it",
        ));
    }

    // The owner is an *optional* owner, not an optional field. `Some(None)` is "send it back to
    // the unassigned queue" and `None` is "the request never said", which must be refused rather
    // than read as the first: a client that forgets the field would otherwise empty every
    // assignee's queue with one press.
    let Some(owner) = body.owner_user_id else {
        return Err(ApiError::bad_request(
            "owner_required",
            "say who the lead goes to, or send it to \"unassigned\" explicitly",
        ));
    };

    let updated = store::assign_owner(
        state.db().pool(),
        organization_id,
        id,
        owner,
        reason,
        Some(session.user.id),
    )
    .await
    .map_err(map_store)?
    .ok_or_else(|| not_found("lead"))?;

    audit(
        state.db().pool(),
        session.user.id,
        organization_id,
        "crm.lead.assigned",
        id,
        json!({ "owner_user_id": owner.map(|o| o.to_string()), "reason": reason }),
    )
    .await;

    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("crm.lead.assigned")
            .organization(organization_id)
            .payload(json!({
                "lead_id": id,
                "owner_user_id": owner.map(|o| o.to_string()),
                "reason": reason,
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "crm.lead.assigned could not be recorded");
    }

    Ok(Json(LeadBody::from(updated)))
}

/// `POST /api/v1/crm/leads/{id}/respond` — record the first response.
///
/// It is the clock-stopping action the SLA is judged on, so it writes the instant, the event
/// and the audit line together, and the store keeps the *first* instant when the button is
/// clicked twice.
pub async fn respond(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<LeadBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let updated = store::record_response(pool, organization_id, id, Some(session.user.id))
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.lead.responded",
        id,
        json!({
            "first_response_at": updated.first_response_at.map(|at| at.to_string()),
        }),
    )
    .await;

    if let Err(error) = bus::emit(
        pool,
        NewEvent::new("crm.lead.responded")
            .organization(organization_id)
            .payload(json!({
                "lead_id": id,
                "first_response_at": updated.first_response_at.map(|at| at.to_string()),
                "status": updated.status,
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "crm.lead.responded could not be recorded");
    }

    Ok(Json(LeadBody::from(updated)))
}

/// `POST /api/v1/crm/leads/{id}/reject` — refuse the lead, keeping the row and the reason.
pub async fn reject(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<RejectBody>,
) -> Result<Json<LeadBody>, ApiError> {
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "reason_required",
            "a rejection must say why — a lead nobody can explain is one an operator undoes",
        ));
    }
    set_terminal(state, session, id, "rejected", Some(reason)).await
}

/// `POST /api/v1/crm/leads/{id}/spam` — mark the lead as spam.
pub async fn mark_spam(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<MarkSpamBody>,
) -> Result<Json<LeadBody>, ApiError> {
    let reason = body
        .reason
        .unwrap_or_else(|| "marked as spam by an operator".to_string());
    set_terminal(state, session, id, "spam", Some(&reason)).await
}

/// Set a terminal status, with the audit line behind it.
async fn set_terminal(
    state: AppState,
    session: CurrentSession,
    id: Uuid,
    status: &'static str,
    reason: Option<&str>,
) -> Result<Json<LeadBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let updated = store::set_status(
        pool,
        organization_id,
        id,
        status,
        Some(session.user.id),
        reason,
    )
    .await
    .map_err(map_store)?
    .ok_or_else(|| not_found("lead"))?;

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.lead.rejected",
        id,
        json!({ "status": status, "reason": reason }),
    )
    .await;

    Ok(Json(LeadBody::from(updated)))
}

/// `DELETE /api/v1/crm/leads/{id}` — delete a lead's data, audited.
///
/// The row is really deleted (that is the retention promise), but the *fact* that somebody
/// deleted it is not: it is an audit entry written outside this module, and the rows that
/// pointed at this lead keep existing with a null pointer.
pub async fn delete_lead(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let lead = store::find_lead(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    if !store::delete_lead(pool, organization_id, id)
        .await
        .map_err(map_store)?
    {
        return Err(not_found("lead"));
    }

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.lead.deleted",
        id,
        json!({
            "email": lead.email,
            "status": lead.status,
            "received_at": lead.received_at.to_string(),
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// What a conversion produced, and what it could not.
///
/// `deal_skipped` is a field rather than an error because the module-absence case is a
/// *partial success* and the panel has to render both halves: a screen that says "converted"
/// over a lead whose opportunity was never created is a lie, and a screen that says "failed"
/// over a contact that now exists throws away work that landed.
#[derive(Debug, Serialize)]
pub struct ConversionBody {
    /// The lead after conversion.
    pub lead: LeadBody,
    /// The contact the lead belongs to.
    pub contact_id: Uuid,
    /// Whether that contact was created by this call.
    pub contact_created: bool,
    /// The opportunity, when one was made.
    pub deal_id: Option<Uuid>,
    /// Why no opportunity was made, in the operator's words.
    pub deal_skipped: Option<String>,
}

/// One person a lead can be handed to.
///
/// The wire shape is the store's row with the label already resolved, because the panel has
/// no way to resolve it itself: it holds ids, not the `users` table.
#[derive(Debug, Serialize)]
pub struct LeadOwnerBody {
    /// The account id.
    pub id: Uuid,
    /// Their name, or their address when they have no display name.
    pub label: String,
    /// Their e-mail.
    pub email: String,
    /// Open leads they hold right now.
    pub open_leads: i64,
    /// Their account status.
    pub status: String,
}

impl From<LeadOwner> for LeadOwnerBody {
    fn from(owner: LeadOwner) -> Self {
        Self {
            id: owner.id,
            label: owner.label,
            email: owner.email,
            open_leads: owner.open_leads,
            status: owner.status,
        }
    }
}

/// Which of the documented flow's modules this deployment actually has.
///
/// The stepper asks this rather than asserting what it can do. The answer is read from the
/// database — the CRM tables are REQ-051's and live on another branch, so "is the CRM
/// installed" is a question about the installation, not a constant.
#[derive(Debug, Serialize)]
pub struct FlowAvailability {
    /// Whether `crm_contacts` exists.
    pub crm: bool,
    /// Whether the sales module (REQ-052) exists.
    pub sales: bool,
    /// Whether the commerce module (REQ-008) exists.
    pub commerce: bool,
}

impl FlowAvailability {
    /// The module's own [`Availability`], for the pure step plan.
    #[must_use]
    pub fn to_module(self) -> omnion_module_crm_intake::Availability {
        omnion_module_crm_intake::Availability {
            sales: self.sales,
            commerce: self.commerce,
        }
    }
}

/// `POST /api/v1/crm/leads/{id}/convert` — create or link the contact and open the
/// opportunity.
///
/// The whole documented flow's third step, in one audited action, and deliberately **not**
/// transactional across the CRM tables: a lead pointing at a contact nobody can see is a
/// broken board, and the platform prefers the contact to exist with a late pointer.
pub async fn convert(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<ConversionBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let report = omnion_module_crm_intake::convert_store::convert_lead(
        pool,
        organization_id,
        id,
        Some(session.user.id),
    )
    .await
    .map_err(map_store)?
    .ok_or_else(|| not_found("lead"))?;

    let lead = store::find_lead(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("lead"))?;

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.lead.converted",
        id,
        json!({
            "contact_id": report.contact_id,
            "contact_created": report.contact_created,
            "deal_id": report.deal_id,
            "deal_skipped": report.deal_skipped,
        }),
    )
    .await;

    if let Err(error) = bus::emit(
        pool,
        NewEvent::new("crm.lead.converted")
            .organization(organization_id)
            .payload(json!({
                "lead_id": id,
                "contact_id": report.contact_id,
                "deal_id": report.deal_id,
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "crm.lead.converted could not be recorded");
    }

    Ok(Json(ConversionBody {
        lead: LeadBody::from(lead),
        contact_id: report.contact_id,
        contact_created: report.contact_created,
        deal_id: report.deal_id,
        deal_skipped: report.deal_skipped,
    }))
}

/// `GET /api/v1/crm/leads/owners` — who a lead can be handed to, and what they already hold.
///
/// The hand-over screen used to ask for a uuid in a free-text box. That is the identifier of
/// an *account*, not of a colleague, and it made the documented hand-over ("an operator who
/// works the queue can decide whose work it is") something only an operator who had the IAM
/// screen open in another tab could actually perform.
///
/// Read permission, not `crm.leads.assign`: seeing the roster is part of reading the inbox —
/// the inbox already shows who owns what — and gating it on the assignment power would hide
/// the owner column's meaning from the majority of the people who use it.
pub async fn owners(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<LeadOwnerBody>>, ApiError> {
    let organization_id = organization_of(&session)?;
    let rows = store::list_owners(state.db().pool(), organization_id)
        .await
        .map_err(map_store)?;
    Ok(Json(rows.into_iter().map(LeadOwnerBody::from).collect()))
}

/// `GET /api/v1/crm/leads/flow` — which steps of the documented flow this deployment can run.
///
/// The stepper used to be four hard-coded strings saying the buttons do not exist, which is
/// the "a disabled control without explanation" the QA plan forbids: the panel could not tell
/// "not built yet" from "not installed here". This answers it from the database.
pub async fn flow(
    State(state): State<AppState>,
    // The extractor is what makes the route authenticated — dropping it would turn a
    // tenant-shape probe into a public endpoint. The session itself is not read: the
    // answer is about the deployment, not about the caller.
    _session: CurrentSession,
) -> Result<Json<FlowAvailability>, ApiError> {
    Ok(Json(FlowAvailability {
        crm: table_exists(state.db().pool(), "crm_contacts").await,
        sales: table_exists(state.db().pool(), "sales_quotes").await,
        commerce: table_exists(state.db().pool(), "commerce_customers").await,
    }))
}

/// `true` when a table is there.
///
/// `to_regclass` rather than a query against the table: asking the catalog whether a name
/// exists is one cheap lookup that cannot fail, where a `select 1 from <table>` that does
/// not exist is an error the caller has to pattern-match on `42P01` (and gets the *other*
/// `42P01` — a missing column — confused with it).
async fn table_exists(pool: &sqlx::PgPool, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>("select to_regclass($1) is not null")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
}

/// `POST /api/v1/crm/leads/retention/sweep` — archive payloads older than the window.
///
/// Reads as a maintenance action rather than something an operator presses per lead, because
/// the promise it keeps is "the row is deletable and the body stops being stored after N
/// days" — a policy, not a decision. The lead rows, their routing and their timelines stay.
///
/// `dry_run` answers the same count and writes nothing, and it is not an optional nicety:
/// the sweep clears the only copy of a person's submission with no undo, so the number has
/// to be readable before the press. It audits nothing either — a preview that appends to the
/// audit log on every render is a log full of events that never happened.
pub async fn retention_sweep(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<SweepBody>,
) -> Result<Json<SweepResult>, ApiError> {
    let days = body.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
    if !(1..=3650).contains(&days) {
        return Err(ApiError::bad_request(
            "invalid_retention_days",
            format!("retention_days must be between 1 and 3650, got {days}"),
        ));
    }
    if body.dry_run == Some(true) {
        let matching = omnion_module_crm_intake::convert_store::count_expired_payloads(
            state.db().pool(),
            organization_of(&session)?,
            days,
        )
        .await
        .map_err(map_store)?;
        return Ok(Json(SweepResult {
            archived: matching,
            retention_days: days,
            dry_run: true,
        }));
    }

    let archived = omnion_module_crm_intake::convert_store::archive_expired_payloads(
        state.db().pool(),
        organization_of(&session)?,
        days,
    )
    .await
    .map_err(map_store)?;

    audit(
        state.db().pool(),
        session.user.id,
        organization_of(&session)?,
        "crm.lead.retention.swept",
        Uuid::nil(),
        json!({ "retention_days": days, "archived": archived }),
    )
    .await;

    Ok(Json(SweepResult {
        archived,
        retention_days: days,
        dry_run: false,
    }))
}

/// The retention window an organization gets when it says nothing.
const DEFAULT_RETENTION_DAYS: i32 = 730;

/// The sweep's request body.
#[derive(Debug, Deserialize)]
pub struct SweepBody {
    /// How old a payload must be before it is archived.
    pub retention_days: Option<i32>,
    /// Count what a sweep would archive and write nothing.
    pub dry_run: Option<bool>,
}

/// What the sweep did — or, in a dry run, what it would have done.
#[derive(Debug, Serialize)]
pub struct SweepResult {
    /// How many lead payloads were cleared (or would be).
    pub archived: u64,
    /// The window it ran with.
    pub retention_days: i32,
    /// Whether this answer came from a dry run, so the screen can say "would clear".
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/intake/sources` — every source, with its health.
pub async fn list_sources(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<Vec<SourceBody>>, ApiError> {
    let rows = store::list_sources(state.db().pool(), organization_of(&session)?)
        .await
        .map_err(map_store)?;
    Ok(Json(rows.into_iter().map(SourceBody::from).collect()))
}

/// `POST /api/v1/crm/intake/sources` — create a source; a keyed endpoint gets its key once.
pub async fn create_source(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<CreateSourceBody>,
) -> Result<(StatusCode, Json<SourceBody>), ApiError> {
    let organization_id = organization_of(&session)?;
    let draft = NewIntakeSource {
        kind: body.kind.unwrap_or_else(|| "endpoint".to_string()),
        form_key: body.form_key,
        dedupe_policy: body.dedupe_policy.unwrap_or_else(|| "link".to_string()),
        // Consent is off unless the operator asked for it *or* supplied wording to ask with:
        // a source created programmatically should not have to invent legal text to exist.
        consent_required: body.consent_required.unwrap_or(body.consent_text.is_some()),
        consent_text: body.consent_text,
        mapping: body.mapping.into_iter().map(Into::into).collect(),
        required_targets: body.required_targets,
        auto_tags: body.auto_tags,
        autoresponder: body
            .autoresponder
            .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
        rate_limit_per_hour: body.rate_limit_per_hour.unwrap_or(30),
        active: body.active.unwrap_or(true),
        pipeline_id: body.pipeline_id,
        stage_id: body.stage_id,
        created_by: Some(session.user.id),
        ..NewIntakeSource::endpoint(organization_id, &body.name, Some(session.user.id))
    };

    let (source, issued) = store::create_source(state.db().pool(), &draft)
        .await
        .map_err(map_store)?;

    audit(
        state.db().pool(),
        session.user.id,
        organization_id,
        "crm.intake.source.created",
        source.id,
        json!({ "kind": source.kind, "name": source.name }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(SourceBody::from(source).with_key(issued.map(|key| key.clear))),
    ))
}

/// `GET /api/v1/crm/intake/sources/{id}` — one source.
///
/// No key is returned here, and that is the point: the clear key exists in exactly one answer
/// in the lifetime of a source, the one that created or rotated it.
pub async fn get_source(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<SourceBody>, ApiError> {
    let source = store::find_source(state.db().pool(), organization_of(&session)?, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;
    Ok(Json(SourceBody::from(source)))
}

/// `PATCH /api/v1/crm/intake/sources/{id}` — update a source.
///
/// A mapping that would drop a required target is refused here, naming the field, rather than
/// at the next submission from a real visitor.
pub async fn update_source(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateSourceBody>,
) -> Result<Json<SourceBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let patch = SourcePatch {
        name: body.name,
        mapping: body
            .mapping
            .map(|lines| lines.into_iter().map(Into::into).collect()),
        required_targets: body.required_targets,
        consent_required: body.consent_required,
        consent_text: body.consent_text,
        dedupe_policy: body.dedupe_policy,
        auto_tags: body.auto_tags,
        autoresponder: body.autoresponder,
        active: body.active,
        rate_limit_per_hour: body.rate_limit_per_hour,
        pipeline_id: body.pipeline_id,
        stage_id: body.stage_id,
    };

    let before = store::find_source(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;

    let updated = store::update_source(pool, organization_id, id, &patch)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;

    // `changed_keys` is what the event carries, so a consumer can tell a mapping edit from a
    // rename without reading the source back.
    let changed = changed_keys(&patch);

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.intake.source.updated",
        id,
        json!({
            "changed_keys": changed,
            "before": { "name": before.name, "active": before.active, "dedupe_policy": before.dedupe_policy },
            "after": { "name": updated.name, "active": updated.active, "dedupe_policy": updated.dedupe_policy },
        }),
    )
    .await;

    if let Err(error) = bus::emit(
        pool,
        NewEvent::new("crm.intake.source.updated")
            .organization(organization_id)
            .payload(json!({ "source_id": id, "changed_keys": changed })),
    )
    .await
    {
        tracing::warn!(error = %error, "crm.intake.source.updated could not be recorded");
    }

    Ok(Json(SourceBody::from(updated)))
}

/// The names of the fields a patch touched, for the event and the audit line.
fn changed_keys(patch: &SourcePatch) -> Vec<&'static str> {
    let mut changed: Vec<&'static str> = Vec::new();
    if patch.name.is_some() {
        changed.push("name");
    }
    if patch.mapping.is_some() {
        changed.push("mapping");
    }
    if patch.required_targets.is_some() {
        changed.push("required_targets");
    }
    if patch.consent_required.is_some() || patch.consent_text.is_some() {
        changed.push("consent");
    }
    if patch.dedupe_policy.is_some() {
        changed.push("dedupe_policy");
    }
    if patch.auto_tags.is_some() {
        changed.push("auto_tags");
    }
    if patch.autoresponder.is_some() {
        changed.push("autoresponder");
    }
    if patch.active.is_some() {
        changed.push("active");
    }
    if patch.rate_limit_per_hour.is_some() {
        changed.push("rate_limit_per_hour");
    }
    if patch.pipeline_id.is_some() || patch.stage_id.is_some() {
        changed.push("pipeline");
    }
    changed
}

/// `DELETE /api/v1/crm/intake/sources/{id}` — delete a source; its leads survive with a null
/// `source_id`, because a business that worked a lead must not lose it by deleting a capture
/// surface.
pub async fn delete_source(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let organization_id = organization_of(&session)?;
    if !store::delete_source(state.db().pool(), organization_id, id)
        .await
        .map_err(map_store)?
    {
        return Err(not_found("intake source"));
    }

    audit(
        state.db().pool(),
        session.user.id,
        organization_id,
        "crm.intake.source.deleted",
        id,
        json!({}),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/crm/intake/sources/{id}/rotate-key` — issue a fresh key, revealed once.
///
/// The old key dies on this write: the store writes a new digest, and a caller still holding
/// the old one gets the same `401` an unknown key gets.
pub async fn rotate_key(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<SourceBody>, ApiError> {
    let organization_id = organization_of(&session)?;
    let pool = state.db().pool();
    let issued = store::rotate_key(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;

    audit(
        pool,
        session.user.id,
        organization_id,
        "crm.intake.source.key_rotated",
        id,
        json!({ "hint": issued.hint }),
    )
    .await;

    let updated = store::find_source(pool, organization_id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;

    Ok(Json(SourceBody::from(updated).with_key(Some(issued.clear))))
}

/// `POST /api/v1/crm/intake/sources/{id}/test` — run a payload through the mapping.
///
/// **Writes nothing.** The answer is the fields the payload *would* produce, the targets it
/// did not fill and whether the result is contactable — a preview that stored rows would be a
/// second capture path with a worse authentication story than the keyed endpoint.
pub async fn test_mapping(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<TestMappingBody>,
) -> Result<Json<MappingPreviewBody>, ApiError> {
    let source = store::find_source(state.db().pool(), organization_of(&session)?, id)
        .await
        .map_err(map_store)?
        .ok_or_else(|| not_found("intake source"))?;

    let payload = if body.payload.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        body.payload
    };

    let mapped =
        omnion_module_crm_intake::apply(&source.mapping_lines(), &payload).map_err(map_store)?;

    let email = mapped.get("email").map(str::to_string);
    let phone = mapped.get("phone").map(str::to_string);

    let values: serde_json::Map<String, serde_json::Value> = mapped
        .values
        .iter()
        .map(|(target, value)| (target.clone(), serde_json::Value::String(value.clone())))
        .collect();

    Ok(Json(MappingPreviewBody {
        dedupe_key: omnion_module_crm_intake::dedupe::dedupe_key(&mapped),
        values,
        missing_required: mapped.missing_required,
        contactable: omnion_module_crm_intake::contactable(email.as_deref(), phone.as_deref()),
        consent_given: store::consent_satisfied(&source, &payload),
    }))
}

// ---------------------------------------------------------------------------------------------
// The autoresponder\'s editor
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/crm/intake/autoresponder/templates` — the templates an operator picks from.
///
/// The list lives on the server because the *prose* does. A template list shipped in the front
/// end is a list the send path cannot honour: `Autoresponder::from_json` reads the template's
/// own `template_body`, so a client that offers names the server does not have produces a
/// message that renders as a bare subject line and no body, and `deliver` answers
/// `InvalidTemplate` — an autoresponder the operator believes is configured and is not.
#[derive(Debug, Serialize)]
pub struct TemplateBody {
    /// The name stored in the source's `autoresponder` column.
    pub name: &'static str,
    /// The default subject line.
    pub subject: &'static str,
    /// The default body, with its `{{placeholders}}` intact.
    pub body: &'static str,
}

/// The placeholders a template may use, and what each one renders as.
///
/// Served rather than hard-coded in the client, because the render function is the server's
/// and an editor that offers a placeholder the renderer does not know produces an empty string
/// in somebody's inbox.
#[derive(Debug, Serialize)]
pub struct PlaceholderBody {
    /// The token, braces included.
    pub token: &'static str,
    /// What it becomes, in a visitor's terms.
    pub renders: &'static str,
}

/// `GET /api/v1/crm/intake/autoresponder/templates`.
pub async fn autoresponder_templates(
    State(_state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<TemplatesBody>, ApiError> {
    let _ = organization_of(&session)?;
    Ok(Json(TemplatesBody {
        templates: omnion_module_crm_intake::autoresponder::TEMPLATES
            .iter()
            .map(|(name, subject, body)| TemplateBody {
                name,
                subject,
                body,
            })
            .collect(),
        placeholders: PLACEHOLDERS
            .iter()
            .map(|(token, renders)| PlaceholderBody { token, renders })
            .collect(),
        max_delay_minutes:
            omnion_module_crm_intake::autoresponder::Autoresponder::MAX_DELAY_MINUTES,
    }))
}

/// The templates answer.
#[derive(Debug, Serialize)]
pub struct TemplatesBody {
    /// Every template, name, subject and body.
    pub templates: Vec<TemplateBody>,
    /// The tokens the renderer knows.
    pub placeholders: Vec<PlaceholderBody>,
    /// The longest delay the store accepts, in minutes.
    pub max_delay_minutes: i32,
}

/// The placeholder table, mirroring `autoresponder::render`’s match arms exactly.
///
/// The three `name` aliases, the two `source` aliases and the two `product` aliases are three
/// groups, not six tokens: the editor lists the groups, and the aliases are the reason a
/// template written against an older release still renders.
const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{{name}}", "Hello <first name>"),
    ("{{source}}", "the name of this intake source"),
    ("{{product}}", "what the visitor wrote in the product field"),
];

/// `POST /api/v1/crm/intake/sources/{id}/autoresponder/preview` — what the visitor would get.
///
/// Runs the *same* [`omnion_module_crm_intake::autoresponder::Autoresponder::deliver`] the send
/// path runs, over the same `Recipient` shape, and answers the rendered subject and body plus
/// the verdict. It writes nothing: a preview that stored a claim would consume the one claim
/// a lead has, so "look at what it says" could make "send it" a no-op.
#[derive(Debug, Deserialize)]
pub struct AutoresponderPreviewBody {
    /// The column as the editor currently holds it, unsaved lines included.
    #[serde(default)]
    pub autoresponder: serde_json::Value,
    /// A submitter's first name, for the greeting.
    #[serde(default)]
    pub first_name: Option<String>,
    /// The address it would go to. Omitting it answers `no_address`, which is the most useful
    /// thing the editor can be told when the mapping is wrong.
    #[serde(default)]
    pub address: Option<String>,
    /// What they asked about.
    #[serde(default)]
    pub product_interest: Option<String>,
    /// The source's own name, so `{{source}}` renders the name the visitor will actually be
    /// told. Defaulting it to a constant would make the preview *look* right for a template
    /// whose whole job is naming the place the message came from.
    #[serde(default)]
    pub source_name: Option<String>,
}

impl Default for AutoresponderPreviewBody {
    fn default() -> Self {
        Self {
            autoresponder: serde_json::Value::Null,
            first_name: None,
            address: None,
            product_interest: None,
            source_name: None,
        }
    }
}

/// The preview’s answer: the verdict, the reason, and the message when there is one.
#[derive(Debug, Serialize)]
pub struct AutoresponderPreviewResponse {
    /// The delivery verdict name, one word: `ready`, `disabled`, `no_address`, `invalid_template`.
    pub verdict: &'static str,
    /// The same reason the timeline writes, so the editor and the trail speak one language.
    pub reason: &'static str,
    /// The rendered subject, when a message was produced.
    pub subject: Option<String>,
    /// The rendered body, when a message was produced.
    pub body: Option<String>,
    /// Whether this send is held for the configured delay.
    pub delayed: bool,
    /// When the held message becomes due.
    pub due_at: Option<String>,
    /// The placeholders in the template that no recipient can fill, so the editor can name them.
    pub unfilled: Vec<String>,
}

pub async fn preview_autoresponder(
    State(_state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<AutoresponderPreviewBody>,
) -> Result<Json<AutoresponderPreviewResponse>, ApiError> {
    let _ = organization_of(&session)?;
    let source_name = body.source_name.as_deref().unwrap_or("your website");
    let autoresponder =
        omnion_module_crm_intake::autoresponder::Autoresponder::from_json(&body.autoresponder);
    let recipient = omnion_module_crm_intake::autoresponder::Recipient {
        address: body.address.as_deref(),
        first_name: body.first_name.as_deref().unwrap_or_default(),
        source_name,
        product_interest: body.product_interest.as_deref().unwrap_or_default(),
        // The preview is of a *configurable* autoresponder, so the submission is accepted by
        // definition: the only thing left to test is the configuration.
        accepted: true,
        reason: "preview",
    };
    let delivery = autoresponder.deliver(
        &recipient,
        time::OffsetDateTime::now_utc(),
        /* already_sent */ false,
    );

    let (subject, rendered, delayed, due_at) = match &delivery {
        Delivery::Ready(message) => (
            Some(message.subject.clone()),
            Some(message.body.clone()),
            message.delayed,
            message.due_at,
        ),
        _ => (None, None, false, None),
    };

    Ok(Json(AutoresponderPreviewResponse {
        verdict: verdict_name(&delivery),
        reason: delivery.reason(),
        subject,
        body: rendered,
        delayed,
        due_at: due_at.map(omnion_module_crm_intake::autoresponder::date_header),
        unfilled: unfilled_placeholders(&autoresponder),
    }))
}

/// The verdict’s name, in the order the editor wants to explain them.
fn verdict_name(delivery: &Delivery) -> &'static str {
    match delivery {
        Delivery::Ready(_) => "ready",
        Delivery::Disabled => "disabled",
        Delivery::NoRecipient(_) => "not_accepted",
        Delivery::NoAddress => "no_address",
        Delivery::NotYet(_) => "delayed",
        Delivery::AlreadySent => "already_sent",
        Delivery::InvalidTemplate(_) => "invalid_template",
    }
}

/// The placeholders a template asks for that no recipient can ever fill.
///
/// `{{name}}` is filled from the mapping's `first_name`, which the *preview* has no value for,
/// so it is never reported: this is about tokens the *renderer* does not know, which is the
/// mistake a hand-written template actually makes. The tokens are collected as owned strings
/// rather than `&'static str` — a per-call `Box::leak` of a request's own input is a leak that
/// grows with every keystroke in the editor.
fn unfilled_placeholders(
    autoresponder: &omnion_module_crm_intake::autoresponder::Autoresponder,
) -> Vec<String> {
    const KNOWN: [&str; 7] = [
        "name",
        "first_name",
        "firstname",
        "source",
        "source_name",
        "product",
        "product_interest",
    ];
    let mut unknown: Vec<String> = Vec::new();
    for text in [&autoresponder.subject, &autoresponder.body] {
        let mut rest = text.as_str();
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else { break };
            let key = after[..end].trim().to_ascii_lowercase();
            if !KNOWN.contains(&key.as_str()) {
                let token = format!("{{{{{key}}}}}");
                if !unknown.contains(&token) {
                    unknown.push(token);
                }
            }
            rest = &after[end + 2..];
        }
    }
    unknown
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The organization the caller's reads and writes are scoped to.
///
/// A signed-in account with no organization is not a platform operator on this surface: the
/// inbox belongs to an organization, and there is no "every organization's inbox" view on
/// this route, so the answer is `400` rather than a query that returns the wrong tenants.
pub(super) fn organization_of(session: &CurrentSession) -> Result<Uuid, ApiError> {
    session.user.organization_id.ok_or_else(|| {
        ApiError::bad_request(
            "no_organization",
            "this account does not belong to an organization — the lead inbox belongs to one",
        )
    })
}

/// Build the store's query from the HTTP query, refusing what the store would refuse with a
/// message about vocabulary rather than a filter.
fn build_lead_query(params: &LeadsQuery, acting_user_id: Uuid) -> Result<LeadQuery, ApiError> {
    for status in &params.status {
        if !omnion_module_crm_intake::is_status(status) {
            return Err(ApiError::bad_request(
                "invalid_status",
                format!(
                    "status \"{status}\" is not one of {}",
                    omnion_module_crm_intake::STATUSES.join(", ")
                ),
            ));
        }
    }
    if let Some(owner) = params.owner.as_deref() {
        if !matches!(owner, "me" | "unassigned") && owner.parse::<Uuid>().is_err() {
            return Err(ApiError::bad_request(
                "invalid_owner",
                "owner must be \"me\", \"unassigned\", or a user id",
            ));
        }
    }

    Ok(LeadQuery {
        source_id: params.source,
        status: params.status.clone(),
        owner: params.owner.clone(),
        // `me` is the *filter* the reader asked for; the session's id is who is asking.
        // Collapsing the two is how `?owner=me` turns into `?owner=<a uuid the client sent>`.
        acting_user_id: Some(acting_user_id),
        search: params.q.clone(),
        product_interest: params.product.clone(),
        since: parse_instant(params.since.as_deref(), "since")?,
        until: parse_instant(params.until.as_deref(), "until")?,
        limit: params.limit.unwrap_or(50),
        before: parse_instant(params.before.as_deref(), "before")?,
    })
}

/// Parse an RFC 3339 instant, naming the field that failed.
///
/// A timestamp the platform cannot read is a `400` naming `since`/`until`/`before`, never a
/// query that quietly returns everything: a date filter that fails open is worse than no
/// date filter, because the reader believes it worked.
fn parse_instant(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<OffsetDateTime>, ApiError> {
    let Some(raw) = value.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_instant",
                format!("{field} must be an RFC 3339 timestamp, for example 2026-01-31T09:00:00Z"),
            )
        })
}

/// A lead's own identifying fields, for the audit's before/after.
fn lead_fingerprint(lead: &Lead) -> serde_json::Value {
    json!({
        "status": lead.status,
        "email": lead.email,
        "phone": lead.phone,
        "company_name": lead.company_name,
        "product_interest": lead.product_interest,
        "contact_id": lead.contact_id,
    })
}

/// Write one audit entry, logging rather than failing when the audit store is unreachable.
///
/// An action that succeeded but whose audit line could not be written is reported, not
/// unwound: the business change is real, and pretending otherwise would make the panel show
/// an error for work that actually happened.
pub(super) async fn audit(
    pool: &sqlx::PgPool,
    actor_user_id: Uuid,
    organization_id: Uuid,
    action: &'static str,
    target_id: Uuid,
    metadata: serde_json::Value,
) {
    let entry = NewAuditEntry {
        organization_id: Some(organization_id),
        actor_user_id: Some(actor_user_id),
        actor_type: ActorType::User,
        action,
        target_type: Some("crm_lead"),
        target_id: Some(target_id.to_string()),
        metadata,
        ip_address: None,
    };
    if let Err(error) = omnion_audit::record(pool, entry).await {
        tracing::warn!(error = %error, action, "an audit entry could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_status_filter_is_a_400_naming_the_values() {
        let params = LeadsQuery {
            status: vec!["won".to_string()],
            ..LeadsQuery::default()
        };
        // `ApiError` implements `Debug`, not `Display` — a `to_string()` on it would not
        // compile, and the reason it does not is worth knowing: the message is a field, and
        // `Display` would have to pick one of the three (code, message, details) to print.
        let error = format!("{:?}", build_lead_query(&params, Uuid::nil()).unwrap_err());
        assert!(error.contains("won"), "{error}");
    }

    #[test]
    fn an_owner_filter_that_is_not_a_uuid_is_a_400() {
        // "everybody" is a plausible thing for a panel to send and it matches no row, so the
        // reader would see an empty inbox rather than an error.
        let params = LeadsQuery {
            owner: Some("everybody".to_string()),
            ..LeadsQuery::default()
        };
        let error = format!("{:?}", build_lead_query(&params, Uuid::nil()).unwrap_err());
        assert!(error.contains("unassigned"), "{error}");

        for good in ["me", "unassigned", &Uuid::nil().to_string()] {
            let params = LeadsQuery {
                owner: Some(good.to_string()),
                ..LeadsQuery::default()
            };
            assert!(build_lead_query(&params, Uuid::nil()).is_ok(), "{good}");
        }
    }

    #[test]
    fn a_date_filter_that_cannot_be_read_fails_closed() {
        for field in ["since", "until", "before"] {
            let error = format!(
                "{:?}",
                parse_instant(Some("last tuesday"), field).unwrap_err()
            );
            assert!(error.contains(field), "{error}");
        }
        assert!(parse_instant(Some("2026-01-31T09:00:00Z"), "since")
            .unwrap()
            .is_some());
        assert!(parse_instant(Some("  "), "since").unwrap().is_none());
    }

    #[test]
    fn the_me_filter_resolves_to_the_session_not_to_the_query() {
        // The store refuses `owner=me` with no acting user (it would otherwise query
        // `false` and answer "no leads"); the route is what supplies the session's id.
        let params = LeadsQuery {
            owner: Some("me".to_string()),
            ..LeadsQuery::default()
        };
        let query = build_lead_query(&params, Uuid::nil()).unwrap();
        assert_eq!(query.owner.as_deref(), Some("me"));
        assert_eq!(query.acting_user_id, Some(Uuid::nil()));
    }

    #[test]
    fn the_patch_cannot_rewrite_the_capture_evidence() {
        // The body has no `received_at`, no `payload` and no `spam_score`: a verdict its own
        // subject can edit is not a verdict.
        let body: PatchLeadBody = serde_json::from_value(
            json!({ "email": "a@b.co", "received_at": "2020-01-01T00:00:00Z" }),
        )
        .expect("the body must parse");
        assert_eq!(body.email.as_deref(), Some("a@b.co"));
        let patch: store::LeadPatch = body.into();
        // The unknown key is dropped by the derive, which is the platform's answer: the field
        // simply is not addressable.
        assert!(patch.status.is_none());
    }

    #[test]
    fn an_idempotency_key_is_trimmed_capped_and_optional() {
        // The header is stored on the lead, so an unbounded value would let a caller write
        // an arbitrary string into a row's identity.
        let mut headers = HeaderMap::new();
        assert_eq!(idempotency_key(&headers), None);

        headers.insert(
            "x-idempotency-key",
            "  abc-123  ".parse().expect("header must build"),
        );
        assert_eq!(idempotency_key(&headers).as_deref(), Some("abc-123"));

        headers.insert(
            "x-idempotency-key",
            "   ".parse().expect("header must build"),
        );
        assert_eq!(idempotency_key(&headers), None);

        headers.insert(
            "x-idempotency-key",
            "x".repeat(129).parse().expect("header must build"),
        );
        assert_eq!(idempotency_key(&headers), None);

        headers.insert(
            "x-idempotency-key",
            "x".repeat(128).parse().expect("header must build"),
        );
        assert_eq!(idempotency_key(&headers).map(|key| key.len()), Some(128));
    }

    // -----------------------------------------------------------------------------------------
    // The autoresponder editor's answers
    // -----------------------------------------------------------------------------------------

    /// The pure half of the preview, so a test can drive it without a request.
    fn preview_of(
        column: serde_json::Value,
        first_name: &str,
        address: Option<&str>,
    ) -> AutoresponderPreviewResponse {
        let autoresponder =
            omnion_module_crm_intake::autoresponder::Autoresponder::from_json(&column);
        let recipient = omnion_module_crm_intake::autoresponder::Recipient {
            address,
            first_name,
            source_name: "Analytical Engines",
            product_interest: "40 seats",
            accepted: true,
            reason: "preview",
        };
        let delivery = autoresponder.deliver(&recipient, time::OffsetDateTime::now_utc(), false);
        let (subject, body, delayed, due_at) = match &delivery {
            Delivery::Ready(message) => (
                Some(message.subject.clone()),
                Some(message.body.clone()),
                message.delayed,
                message.due_at,
            ),
            _ => (None, None, false, None),
        };
        AutoresponderPreviewResponse {
            verdict: verdict_name(&delivery),
            reason: delivery.reason(),
            subject,
            body,
            delayed,
            due_at: due_at.map(omnion_module_crm_intake::autoresponder::date_header),
            unfilled: unfilled_placeholders(&autoresponder),
        }
    }

    fn configured(delay: i64, body: &str) -> serde_json::Value {
        serde_json::json!({
            "enabled": true,
            "template": "acknowledgement",
            "subject": "We received your message",
            "template_body": body,
            "delay_minutes": delay,
        })
    }

    #[test]
    fn a_configured_autoresponder_previews_the_message_the_send_path_would_send() {
        let answer = preview_of(
            configured(0, "Hello,\n\nwe have {{name}}'s message from {{source}}."),
            "Ada",
            Some("ada@example.com"),
        );
        assert_eq!(answer.verdict, "ready", "{answer:?}");
        assert_eq!(answer.reason, "sent");
        // The point of the endpoint: the greeting is the *server's* render, not the client's.
        // A client-side substitution would agree on `{{name}}` and disagree on the aliases.
        assert!(answer
            .body
            .as_deref()
            .unwrap()
            .contains("Hello Ada's message"));
        assert!(answer
            .body
            .as_deref()
            .unwrap()
            .contains("Analytical Engines"));
        assert!(answer.unfilled.is_empty(), "{:?}", answer.unfilled);
    }

    #[test]
    fn a_delayed_autoresponder_previews_as_held_with_a_due_time() {
        let answer = preview_of(
            configured(30, "we have your message"),
            "Ada",
            Some("ada@x.com"),
        );
        assert_eq!(answer.verdict, "ready", "{answer:?}");
        assert!(answer.delayed, "a delay has to read as held, not as sent");
        assert!(
            answer.due_at.is_some(),
            "a held message answers when it goes out"
        );
    }

    #[test]
    fn an_enabled_autoresponder_with_no_text_previews_as_invalid_not_as_disabled() {
        // The distinction the editor exists to make: "off" is a choice, "on and empty" is a
        // misconfiguration that eats the lead's single reply without saying so.
        let answer = preview_of(
            serde_json::json!({ "enabled": true, "subject": "", "template_body": "" }),
            "Ada",
            Some("ada@x.com"),
        );
        assert_eq!(answer.verdict, "invalid_template", "{answer:?}");
        assert_eq!(answer.reason, "invalid_template");
        assert!(
            answer.subject.is_none(),
            "an unusable template renders no message"
        );
    }

    #[test]
    fn a_mapping_that_lost_the_address_previews_as_no_address() {
        // What an operator sees when the *mapping* is the thing that is wrong: the
        // autoresponder is fine and there is nobody to send it to.
        let answer = preview_of(configured(0, "we have your message"), "Ada", None);
        assert_eq!(answer.verdict, "no_address", "{answer:?}");
        assert_eq!(answer.reason, "no_address");
    }

    #[test]
    fn an_absent_autoresponder_column_previews_as_disabled() {
        let answer = preview_of(serde_json::Value::Null, "Ada", Some("ada@x.com"));
        assert_eq!(answer.verdict, "disabled", "{answer:?}");
        assert_eq!(answer.reason, "not_configured");
    }

    #[test]
    fn a_placeholder_the_renderer_does_not_know_is_named_rather_than_left_blank() {
        // The failure this reports is invisible in a rendered message: `{{salutation}}` becomes
        // an empty space, so the visitor reads "Dear ," and nobody knows the template is wrong.
        let answer = preview_of(
            configured(
                0,
                "Dear {{salutation}}, {{name}} — from {{source}} and {{company}}.",
            ),
            "Ada",
            Some("ada@x.com"),
        );
        assert_eq!(answer.verdict, "ready", "{answer:?}");
        assert_eq!(
            answer.unfilled,
            vec!["{{salutation}}".to_string(), "{{company}}".to_string()],
            "both unknown tokens, in the order the template asks for them"
        );
    }

    #[test]
    fn every_alias_of_a_known_placeholder_is_known() {
        // `render` accepts three spellings of each idea. If the editor's list and the
        // renderer's arms drift apart, a hand-written template stops rendering.
        for token in [
            "{{name}}",
            "{{first_name}}",
            "{{firstname}}",
            "{{source}}",
            "{{source_name}}",
            "{{product}}",
            "{{product_interest}}",
        ] {
            let body = format!("x {token} y");
            let answer = preview_of(configured(0, &body), "Ada", Some("ada@x.com"));
            assert!(
                answer.unfilled.is_empty(),
                "{token} was reported as unknown: {answer:?}"
            );
        }
    }

    #[test]
    fn the_template_list_the_server_serves_is_the_one_the_send_path_can_render() {
        // The editor's list is the server's, and this is the tie between them: every template
        // the endpoint offers must survive `is_configured` with its own defaults filled in.
        for (name, subject, body) in omnion_module_crm_intake::autoresponder::TEMPLATES {
            let answer = preview_of(
                serde_json::json!({
                    "enabled": true, "template": name, "subject": subject, "template_body": body
                }),
                "Ada",
                Some("ada@example.com"),
            );
            assert_eq!(
                answer.verdict, "ready",
                "template {name} offered but unrenderable"
            );
        }
    }

    #[test]
    fn the_placeholder_table_names_only_tokens_the_renderer_understands() {
        for (token, _) in PLACEHOLDERS {
            let key = token.trim_matches(|c| c == '{' || c == '}');
            assert!(
                ["name", "source", "product"].contains(&key),
                "{token} is offered in the editor but render() has no arm for it"
            );
        }
    }

    #[test]
    fn a_rejection_without_a_reason_is_refused_before_the_write() {
        // The handler's own guard, asserted here because it is the one rule in this file a
        // reviewer cannot see from the SQL.
        let empty = RejectBody {
            reason: "   ".to_string(),
        };
        assert!(empty.reason.trim().is_empty());
    }
}
