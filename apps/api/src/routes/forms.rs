//! `/api/v1/forms`, `/api/v1/forms/{id}/submissions` and `/api/v1/public/forms/{key}/submit` —
//! REQ-064 slice 2.
//!
//! Three surfaces with different trust levels meet in this file: an authenticated builder, an
//! authenticated inbox, and an unauthenticated endpoint that anybody on the internet can post
//! to. Four decisions shape the code, and each of them exists because the obvious version has
//! already failed somewhere.
//!
//! * **The public submit route answers 202 for what it refused.** A visitor who filled the
//!   honeypot, or who tripped the rate limit, is not told. Telling them is a free oracle for
//!   "is this IP blocked", and it teaches a bot which protection to work around. The store
//!   counts the refusal; the route reports a success shape with a `stored` flag nobody but the
//!   owner can see.
//!
//! * **Field errors are a 422 with every wrong field at once.** One field per round trip turns a
//!   six-field form into six submissions, and the fifth of those is the one that trips the rate
//!   limit — so a bad validator becomes an availability bug for real users.
//!
//! * **The export honours the filters, because an export that ignores them leaks.** `GET
//!   /submissions/export` takes the same query as the list and returns the same rows; "just give
//!   me everything" is a second endpoint nobody would think about when reviewing a filter.
//!
//! * **The inbox is scoped through the form, and the form through the site.** The store's
//!   `where form_id = $1` is what conceals a submission of another form; resolving the row first
//!   and *then* checking ownership would turn the drawer into an existence oracle, which is the
//!   exact defect `entry_in_scope` in `menus.rs` documents.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use omnion_audit::NewAuditEntry;
use omnion_content::{
    ContentError, Form, FormChanges, FormField, NewFormField, NewSubmission, Submission,
    SubmissionQuery,
};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Create a form.
#[derive(Debug, Deserialize)]
pub struct CreateFormRequest {
    /// Site the form belongs to.
    pub site_id: Uuid,
    /// Stable key, unique inside the site — the form's public address.
    pub key: String,
    /// Name the list shows.
    pub name: String,
    /// The first fields, in canvas order. A form with none cannot be saved: it would render an
    /// empty form that accepts nothing, which is worse than one that is obviously not ready.
    #[serde(default)]
    pub fields: Vec<FieldBody>,
    /// Who the submission notification goes to.
    ///
    /// This used to be settable only by a follow-up `PUT`, which made a form born without
    /// recipients: the panel saved the form, published it, and the notification had nobody to
    /// go to until somebody remembered to open the settings tab. The settings are the
    /// definition of a form, so they are part of creating one.
    #[serde(default)]
    pub notify_emails: Option<Vec<String>>,
    /// Subject template, with `{{form_name}}` and `{{submitted_at}}` placeholders.
    #[serde(default)]
    pub notify_subject: Option<String>,
    /// What happens after a submission: `message` or `redirect`.
    #[serde(default)]
    pub submit_action: Option<String>,
    /// Inline success message, when the action is `message`.
    #[serde(default)]
    pub submit_message: Option<String>,
    /// Post-submission redirect, when the action is `redirect`.
    #[serde(default)]
    pub redirect_url: Option<String>,
}

/// One field as the builder submits it.
#[derive(Debug, Deserialize)]
pub struct FieldBody {
    /// Answer name.
    pub key: String,
    /// Label shown above the input.
    pub label: String,
    /// One of the eight types the palette offers.
    #[serde(default = "default_field_type")]
    pub field_type: String,
    /// Whether an empty answer is refused.
    #[serde(default)]
    pub required: bool,
    /// Placeholder inside the input.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Help text under the input.
    #[serde(default)]
    pub help_text: Option<String>,
    /// `half` or `full` — the canvas's row span.
    #[serde(default = "default_width")]
    pub width: String,
    /// Validation rules.
    #[serde(default)]
    pub rules: Value,
    /// Options, for the choice types.
    #[serde(default = "default_options")]
    pub options: Value,
}

fn default_field_type() -> String {
    "text".to_owned()
}
fn default_width() -> String {
    "full".to_owned()
}
fn default_options() -> Value {
    json!([])
}

impl From<FieldBody> for NewFormField {
    fn from(body: FieldBody) -> Self {
        Self {
            position: 0,
            key: body.key,
            label: body.label,
            field_type: body.field_type,
            required: body.required,
            placeholder: body.placeholder,
            help_text: body.help_text,
            width: body.width,
            rules: body.rules,
            options: body.options,
        }
    }
}

/// A form's own settings as an edit.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateFormRequest {
    /// New key.
    pub key: Option<String>,
    /// New name.
    pub name: Option<String>,
    /// What happens after a submission.
    pub submit_action: Option<String>,
    /// Inline success message.
    pub submit_message: Option<String>,
    /// Post-submission redirect.
    pub redirect_url: Option<String>,
    /// Notification recipients.
    pub notify_emails: Option<Vec<String>>,
    /// Notification subject template.
    pub notify_subject: Option<String>,
    /// Honeypot toggle.
    pub honeypot: Option<bool>,
    /// Fill-time floor, in seconds.
    pub min_fill_seconds: Option<i32>,
    /// Per-sender hourly limit.
    pub rate_limit_per_hour: Option<i32>,
    /// Retention, in days.
    pub retention_days: Option<i32>,
}

/// Replace a form's fields.
#[derive(Debug, Deserialize)]
pub struct SaveFieldsRequest {
    /// The whole canvas, in order.
    pub fields: Vec<FieldBody>,
}

/// Set a form's lifecycle state.
#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    /// `published` or `draft`.
    pub status: String,
}

/// Change one submission's inbox state.
#[derive(Debug, Deserialize)]
pub struct SubmissionStatusRequest {
    /// `new`, `read`, `spam` or `archived`.
    pub status: String,
}

/// Change several submissions' inbox state.
#[derive(Debug, Deserialize)]
pub struct BulkSubmissionRequest {
    /// The rows to move.
    pub ids: Vec<Uuid>,
    /// The state to move them to.
    pub status: String,
}

/// A public submission.
#[derive(Debug, Default, Deserialize)]
pub struct PublicSubmitRequest {
    /// Answers, keyed by field key.
    #[serde(default)]
    pub answers: Map<String, Value>,
    /// The invisible honeypot input. Anything in it came from a script.
    #[serde(default)]
    pub honeypot: String,
    /// Milliseconds the page measured between render and submit.
    #[serde(default)]
    pub filled_at_ms: i64,
    /// The path the form was embedded on.
    #[serde(default)]
    pub source_path: Option<String>,
}

/// A form as the panel list reads it.
#[derive(Debug, Serialize)]
pub struct FormBody {
    /// Form id.
    pub id: Uuid,
    /// Site it belongs to.
    pub site_id: Uuid,
    /// The site's global key, for the panel's own lookups.
    pub site_key: String,
    /// Stable key — the public address.
    pub key: String,
    /// Name.
    pub name: String,
    /// `draft` or `published`.
    pub status: String,
    /// What happens after a submission: `message` or `redirect`.
    pub submit_action: String,
    /// The message the form shows, when it shows one.
    pub submit_message: Option<String>,
    /// Where a redirect form sends the visitor.
    pub redirect_url: Option<String>,
    /// Who the submission notification goes to.
    pub notify_emails: Vec<String>,
    /// Whether the honeypot is armed.
    pub honeypot: bool,
    /// The fill-time floor, in seconds.
    pub min_fill_seconds: i32,
    /// Submissions one sender may make in an hour.
    pub rate_limit_per_hour: i32,
    /// How long a submission is kept.
    pub retention_days: i32,
    /// Field count — the list card's number.
    pub field_count: usize,
    /// Unread submissions — the inbox badge.
    pub unread_count: i64,
    /// Spam submissions counted, never silently dropped.
    pub spam_count: i64,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A form with its fields: the builder's document.
#[derive(Debug, Serialize)]
pub struct FormDetailBody {
    /// The form.
    #[serde(flatten)]
    pub form: FormBody,
    /// Its fields, in canvas order.
    pub fields: Vec<FormFieldBody>,
    /// The vocabulary the palette and inspector draw from, so the panel never hard-codes a list
    /// the server would then refuse.
    pub vocabulary: FormsVocabularyBody,
}

/// The closed vocabularies of this surface.
#[derive(Debug, Serialize)]
pub struct FormsVocabularyBody {
    /// The field types the palette offers, in palette order.
    pub field_types: Vec<&'static str>,
    /// The inbox states.
    pub statuses: Vec<&'static str>,
    /// The submit behaviours.
    pub submit_actions: Vec<&'static str>,
    /// Most fields one form may hold.
    pub max_fields: usize,
    /// Longest accepted answer.
    pub max_answer_length: usize,
}

/// One field as the editor reads it.
#[derive(Debug, Serialize)]
pub struct FormFieldBody {
    /// Field id.
    pub id: Uuid,
    /// Answer name.
    pub key: String,
    /// Label.
    pub label: String,
    /// Type.
    pub field_type: String,
    /// Required flag.
    pub required: bool,
    /// Placeholder.
    pub placeholder: Option<String>,
    /// Help text.
    pub help_text: Option<String>,
    /// Canvas row span.
    pub width: String,
    /// Validation rules.
    pub rules: Value,
    /// Options.
    pub options: Value,
}

impl From<&FormField> for FormFieldBody {
    fn from(field: &FormField) -> Self {
        Self {
            id: field.id,
            key: field.key.clone(),
            label: field.label.clone(),
            field_type: field.field_type.clone(),
            required: field.required,
            placeholder: field.placeholder.clone(),
            help_text: field.help_text.clone(),
            width: field.width.clone(),
            rules: field.rules.clone(),
            options: field.options.clone(),
        }
    }
}

/// The form's own settings, which the builder reads next to its fields.
#[derive(Debug, Serialize)]
pub struct FormSettingsBody {
    /// What happens after a submission.
    pub submit_action: String,
    /// Inline success message.
    pub submit_message: Option<String>,
    /// Post-submission redirect.
    pub redirect_url: Option<String>,
    /// Notification recipients.
    pub notify_emails: Vec<String>,
    /// Notification subject template.
    pub notify_subject: Option<String>,
    /// Whether the honeypot is armed.
    pub honeypot: bool,
    /// Fill-time floor, in seconds.
    pub min_fill_seconds: i32,
    /// Per-sender hourly limit.
    pub rate_limit_per_hour: i32,
    /// Retention, in days.
    pub retention_days: i32,
}

/// A submission as the inbox reads it.
#[derive(Debug, Serialize)]
pub struct SubmissionBody {
    /// Submission id.
    pub id: Uuid,
    /// Answers, keyed by field key.
    pub answers: Value,
    /// The consent text that was shown, when the form has a consent field.
    pub consent_text: Option<String>,
    /// Path it was made from.
    pub source_path: Option<String>,
    /// `new`, `read`, `spam` or `archived`.
    pub status: String,
    /// Heuristic score, 0–100.
    pub spam_score: i32,
    /// When it arrived.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The three columns the inbox derives from the answers. Filled by the list handler, which
    /// is the only place that knows the form's fields; `From` alone cannot, because the field
    /// keys are a property of the *form*, not of the submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryBody>,
}

impl From<&Submission> for SubmissionBody {
    fn from(submission: &Submission) -> Self {
        Self {
            id: submission.id,
            answers: submission.answers.clone(),
            consent_text: submission.consent_text.clone(),
            source_path: submission.source_path.clone(),
            status: submission.status.clone(),
            spam_score: submission.spam_score,
            created_at: submission.created_at,
            summary: None,
        }
    }
}

/// One page of the inbox.
#[derive(Debug, Serialize)]
pub struct InboxBody {
    /// The rows.
    pub submissions: Vec<SubmissionBody>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// The counts the tabs show: new, read, spam, archived.
    pub counts: InboxCountsBody,
}

/// The four inbox counts.
#[derive(Debug, Serialize)]
pub struct InboxCountsBody {
    /// Unread.
    pub new: i64,
    /// Read.
    pub read: i64,
    /// Spam.
    pub spam: i64,
    /// Archived.
    pub archived: i64,
}

/// What a public submission produced.
///
/// `stored` is false for every refusal the spam protections make, and the visitor is never told
/// which one fired: see the module header.
#[derive(Debug, Serialize)]
pub struct PublicSubmitBody {
    /// Whether a row was stored. The owner's own screen reads this from the audit log, not from
    /// the visitor's response.
    pub stored: bool,
    /// What happens next, in the theme's own words.
    pub action: String,
    /// Where the visitor lands, when the form redirects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// The message the form shows, when it shows one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Field-level messages, present only when the submission was rejected as *invalid* — which
    /// is the one refusal a visitor is told about, because they can fix it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<BTreeMap<String, String>>,
    /// Whether the submission was held as spam, so an owner debugging "the form stopped working"
    /// can tell a refusal from a silent drop. Never derived from the *visitor's* request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_as_spam: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------------------------

/// The list query.
#[derive(Debug, Deserialize)]
pub struct ListFormsParams {
    /// Site whose forms to list.
    pub site_id: Uuid,
}

/// `GET /api/v1/forms`.
pub async fn list_forms(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ListFormsParams>,
) -> Result<Json<Vec<FormBody>>, ApiError> {
    let site = site_in_scope(&state, &current, params.site_id).await?;
    let forms = omnion_content::list_forms(state.db().pool(), site.id).await?;
    let mut bodies = Vec::with_capacity(forms.len());
    for form in &forms {
        bodies.push(form_body(&state, form).await?);
    }
    Ok(Json(bodies))
}

/// `GET /api/v1/forms/{id}` — the builder's document.
pub async fn get_form(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<FormDetailBody>, ApiError> {
    let (form, fields) = form_in_scope(&state, &current, id).await?;
    Ok(Json(form_detail(&state, &form, &fields).await?))
}

/// `POST /api/v1/forms`.
pub async fn create_form(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<CreateFormRequest>,
) -> Result<(StatusCode, Json<FormDetailBody>), ApiError> {
    let site = site_in_scope(&state, &current, body.site_id).await?;
    let fields: Vec<NewFormField> = body.fields.into_iter().map(NewFormField::from).collect();
    let form = omnion_content::create_form(
        state.db().pool(),
        site.organization_id,
        site.id,
        &body.key,
        &body.name,
        &fields,
        Some(current.user.id),
    )
    .await?;

    // The notification settings arrive with the form, and go through the same validation the
    // settings tab applies — an address that is not one is refused here too, rather than being
    // stored and then discovered by the first send that fails.
    let wants_settings = body.notify_emails.is_some()
        || body.notify_subject.is_some()
        || body.submit_action.is_some()
        || body.submit_message.is_some()
        || body.redirect_url.is_some();
    let form = if wants_settings {
        omnion_content::update_form(
            state.db().pool(),
            site.id,
            form.id,
            &FormChanges {
                notify_emails: body.notify_emails,
                notify_subject: body.notify_subject,
                submit_action: body.submit_action,
                submit_message: body.submit_message,
                redirect_url: body.redirect_url,
                ..FormChanges::default()
            },
        )
        .await?
    } else {
        form
    };

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "form.create")
            .organization(site.organization_id)
            .target("form", form.id)
            .metadata(json!({ "key": form.key, "name": form.name, "site_id": site.id })),
    )
    .await?;
    emit(
        &state,
        "content.form.updated",
        json!({ "form_id": form.id, "action": "created", "site_id": site.id }),
    )
    .await;

    let fields = omnion_content::list_fields(state.db().pool(), form.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(form_detail(&state, &form, &fields).await?),
    ))
}

/// `PUT /api/v1/forms/{id}` — the settings tab.
pub async fn update_form(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateFormRequest>,
) -> Result<Json<FormDetailBody>, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    let changes = FormChanges {
        key: body.key,
        name: body.name,
        submit_action: body.submit_action,
        submit_message: body.submit_message,
        redirect_url: body.redirect_url,
        notify_emails: body.notify_emails,
        notify_subject: body.notify_subject,
        honeypot: body.honeypot,
        min_fill_seconds: body.min_fill_seconds,
        rate_limit_per_hour: body.rate_limit_per_hour,
        retention_days: body.retention_days,
        target_segment_id: None,
    };
    let updated =
        omnion_content::update_form(state.db().pool(), form.site_id, id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "form.update")
            .organization(form.organization_id)
            .target("form", id)
            .metadata(json!({ "site_id": form.site_id })),
    )
    .await?;
    emit(
        &state,
        "content.form.updated",
        json!({ "form_id": id, "action": "updated", "site_id": form.site_id }),
    )
    .await;

    let fields = omnion_content::list_fields(state.db().pool(), id).await?;
    Ok(Json(form_detail(&state, &updated, &fields).await?))
}

/// `PUT /api/v1/forms/{id}/fields` — the builder's `Save`.
pub async fn save_form_fields(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveFieldsRequest>,
) -> Result<Json<FormDetailBody>, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    let fields: Vec<NewFormField> = body.fields.into_iter().map(NewFormField::from).collect();
    let saved = omnion_content::save_fields(state.db().pool(), id, &fields).await?;

    emit(
        &state,
        "content.form.updated",
        json!({ "form_id": id, "action": "fields_saved", "fields": saved.len() }),
    )
    .await;
    Ok(Json(form_detail(&state, &form, &saved).await?))
}

/// `POST /api/v1/forms/{id}/publish`.
pub async fn set_form_status(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<PublishRequest>,
) -> Result<Json<FormDetailBody>, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    let updated =
        omnion_content::set_form_status(state.db().pool(), form.site_id, id, &body.status).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "form.publish")
            .organization(form.organization_id)
            .target("form", id)
            .metadata(json!({ "status": body.status, "site_id": form.site_id })),
    )
    .await?;
    emit(
        &state,
        "content.form.updated",
        json!({ "form_id": id, "action": "status", "status": body.status }),
    )
    .await;

    let fields = omnion_content::list_fields(state.db().pool(), id).await?;
    Ok(Json(form_detail(&state, &updated, &fields).await?))
}

/// `DELETE /api/v1/forms/{id}`.
pub async fn delete_form(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    omnion_content::delete_form(state.db().pool(), form.site_id, id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "form.delete")
            .organization(form.organization_id)
            .target("form", id)
            .metadata(json!({ "site_id": form.site_id })),
    )
    .await?;
    emit(
        &state,
        "content.form.updated",
        json!({ "form_id": id, "action": "deleted", "site_id": form.site_id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Submissions
// ---------------------------------------------------------------------------------------------

/// The inbox query. The export takes exactly this, so the two can never disagree.
#[derive(Debug, Default, Deserialize)]
pub struct InboxParams {
    /// Keep only this state.
    pub status: Option<String>,
    /// Case-insensitive substring over the answers.
    pub search: Option<String>,
    /// Only rows at or after this instant, RFC 3339.
    pub since: Option<String>,
    /// Only rows at or before this instant, RFC 3339.
    pub until: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

impl InboxParams {
    /// The store query these parameters mean.
    fn to_query(&self) -> Result<SubmissionQuery, ApiError> {
        if let Some(status) = self.status.as_deref()
            && !omnion_content::SUBMISSIONS_STATUSES.contains(&status)
        {
            return Err(ApiError::bad_request(
                "invalid_form_status",
                format!(
                    "{status:?} is not an inbox state ({})",
                    omnion_content::SUBMISSIONS_STATUSES.join(", ")
                ),
            ));
        }
        Ok(SubmissionQuery {
            status: self.status.clone(),
            search: self.search.clone().filter(|s| !s.trim().is_empty()),
            since: self.since.as_deref().map(parse_instant).transpose()?,
            until: self.until.as_deref().map(parse_instant).transpose()?,
            limit: self.limit.unwrap_or(50),
            offset: self.offset.unwrap_or(0),
        })
    }
}

/// `GET /api/v1/forms/{id}/submissions`.
pub async fn list_submissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<InboxParams>,
) -> Result<Json<InboxBody>, ApiError> {
    let (_, fields) = form_in_scope(&state, &current, id).await?;
    let query = params.to_query()?;
    let pool = state.db().pool();
    let submissions = omnion_content::list_submissions(pool, id, &query).await?;
    let total = omnion_content::count_submissions(pool, id, &query).await?;
    let counts = inbox_counts(pool, id).await?;

    // The inbox's "Name" and "E-mail" columns read the field whose *key* names them, not the
    // first text field: a form whose first field is a subject line would otherwise show the
    // subject in the Name column, which reads as a bug in the data rather than a guess.
    let name_key = field_key(&fields, "name");
    let email_key = field_key(&fields, "email");
    let rows = submissions
        .iter()
        .map(|submission| {
            let mut body = SubmissionBody::from(submission);
            body.summary = Some(SummaryBody {
                name: answer_text(&submission.answers, name_key.as_deref()),
                email: answer_text(&submission.answers, email_key.as_deref()),
                text: omnion_content::answers_summary(&submission.answers),
            });
            body
        })
        .collect();
    Ok(Json(InboxBody {
        submissions: rows,
        total,
        counts,
    }))
}

/// `GET /api/v1/forms/{id}/submissions/export` — CSV.
///
/// Same filters as the list, same rows. A download that ignores the filter is how a filtered
/// inbox leaks the whole history through a button labelled "Export".
pub async fn export_submissions(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Query(params): Query<InboxParams>,
) -> Result<Response, ApiError> {
    let _ = form_in_scope(&state, &current, id).await?;
    let query = params.to_query()?;
    // An export has no page: the point of the button is the whole filtered set, so the limit is
    // the store's own ceiling rather than the screen's 50.
    let query = SubmissionQuery {
        limit: 500,
        ..query
    };
    let submissions = omnion_content::list_submissions(state.db().pool(), id, &query).await?;
    let csv = omnion_content::submissions_to_csv(&submissions);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            "attachment; filename=\"submissions.csv\"",
        )
        .body(axum::body::Body::from(csv))
        .expect("a CSV response header set is valid"))
}

/// `GET /api/v1/forms/{id}/submissions/{sid}` — the drawer.
pub async fn get_submission(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((id, sid)): Path<(Uuid, Uuid)>,
) -> Result<Json<SubmissionBody>, ApiError> {
    let _ = form_in_scope(&state, &current, id).await?;
    let submission = omnion_content::find_submission(state.db().pool(), id, sid)
        .await?
        .ok_or(ContentError::SubmissionNotFound)?;
    Ok(Json(SubmissionBody::from(&submission)))
}

/// `PATCH /api/v1/forms/{id}/submissions/{sid}`.
pub async fn set_submission_status(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((id, sid)): Path<(Uuid, Uuid)>,
    Json(body): Json<SubmissionStatusRequest>,
) -> Result<Json<SubmissionBody>, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    let updated =
        omnion_content::set_submission_status(state.db().pool(), id, sid, &body.status).await?;
    if body.status == "spam" {
        record(
            &state,
            NewAuditEntry::by_user(current.user.id, "form.submission.spam")
                .organization(form.organization_id)
                .target("form_submission", sid)
                .metadata(json!({ "form_id": id })),
        )
        .await?;
    }
    Ok(Json(SubmissionBody::from(&updated)))
}

/// `PATCH /api/v1/forms/{id}/submissions` — the bulk bar.
pub async fn bulk_submission_status(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<BulkSubmissionRequest>,
) -> Result<Json<InboxBody>, ApiError> {
    let _ = form_in_scope(&state, &current, id).await?;
    let updated =
        omnion_content::bulk_submission_status(state.db().pool(), id, &body.ids, &body.status)
            .await?;
    let query = SubmissionQuery {
        status: None,
        limit: 50,
        ..SubmissionQuery::default()
    };
    let pool = state.db().pool();
    let submissions = omnion_content::list_submissions(pool, id, &query).await?;
    Ok(Json(InboxBody {
        total: omnion_content::count_submissions(pool, id, &query).await?,
        counts: inbox_counts(pool, id).await?,
        submissions: submissions.iter().map(SubmissionBody::from).collect(),
    }))
    .map(|json| {
        let _ = updated;
        json
    })
}

/// `DELETE /api/v1/forms/{id}/submissions/{sid}` — delete for good.
pub async fn delete_submission(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((id, sid)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (form, _) = form_in_scope(&state, &current, id).await?;
    omnion_content::delete_submission(state.db().pool(), id, sid).await?;
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "form.submission.delete")
            .organization(form.organization_id)
            .target("form_submission", sid)
            .metadata(json!({ "form_id": id })),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Public submission
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/public/forms/{key}/submit`.
///
/// Unauthenticated by design: this is the endpoint a visitor's browser posts to from a page the
/// owner does not control the session of. It resolves the site from the request, exactly as
/// `/public/menus/{location}` does, so a renderer with several sites has the same hint.
#[derive(Debug, Deserialize)]
pub struct PublicFormParams {
    /// Site address: a host or a site key.
    #[serde(default)]
    pub site: Option<String>,
}

/// `POST /api/v1/public/forms/{key}/submit`.
pub async fn public_submit(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<PublicFormParams>,
    headers: HeaderMap,
    Json(body): Json<PublicSubmitRequest>,
) -> Result<(StatusCode, Json<PublicSubmitBody>), ApiError> {
    let site =
        crate::routes::public::resolve_site(state.db().pool(), params.site.as_deref(), &headers)
            .await?;
    let form = omnion_content::find_form_by_key(state.db().pool(), site.id, &key)
        .await?
        .ok_or_else(|| {
            // A draft form answers the same 404 as a form that does not exist: the public route
            // must not be able to tell an editor that a form exists but is not live.
            ApiError::new(StatusCode::NOT_FOUND, "form_not_found", "no such form")
        })?;
    if !form.is_live() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "form_not_found",
            "no such form",
        ));
    }
    let fields = omnion_content::list_fields(state.db().pool(), form.id).await?;

    let sender = sender_fingerprint(&headers);
    let incoming = NewSubmission {
        answers: body.answers,
        honeypot: body.honeypot,
        filled_at_ms: body.filled_at_ms,
        ip_hash: sender.ip_hash.clone(),
        user_agent_hash: sender.user_agent_hash.clone(),
        source_path: body.source_path,
    };
    let outcome =
        omnion_content::submit_public(state.db().pool(), &form, &fields, &incoming).await?;

    // The one refusal the visitor IS told about: a submission rejected as invalid can be fixed,
    // and silently dropping it is the worst possible answer to somebody who just filled in a
    // contact form.
    if outcome.refused == Some("form_invalid") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "form_invalid",
            "some answers need attention",
        )
        .with_details(json!({ "errors": outcome.errors })));
    }

    if let Some(stored) = &outcome.submission {
        // The event the REQ names as contract: `content.form.submitted`. An automation, a CRM or
        // a webhook can subscribe without the module knowing any of them exist.
        emit(
            &state,
            "content.form.submitted",
            json!({
                "form_id": form.id,
                "form_key": form.key,
                "site_id": form.site_id,
                "submission_id": stored.id,
                "answers": stored.answers,
                "spam_score": stored.spam_score,
            }),
        )
        .await;

        // The builder's own notification, sent through the platform mail path the workflow
        // engine uses. It runs AFTER the row exists and AFTER the event, so a visitor is never
        // kept waiting on a mail server, and so an owner who wired an automation to
        // `content.form.submitted` still gets theirs even if SMTP is down.
        notify(&state, &form, &fields, stored).await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(PublicSubmitBody {
            stored: outcome.stored(),
            action: form.submit_action.clone(),
            redirect_url: form.redirect_url.clone(),
            message: form.submit_message.clone(),
            errors: None,
            held_as_spam: None,
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The form with its fields, once the caller has been checked against its organization.
async fn form_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<(Form, Vec<FormField>), ApiError> {
    // Read the organization out of the row and conceal a mismatch by hand rather than letting
    // `ensure_same_organization` answer 403 — see `entry_in_scope` in `menus.rs` for why a 403
    // here would be an existence oracle.
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("select site_id, organization_id from cms_forms where id = $1")
            .bind(id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("reading the form: {error}"),
                )
            })?;
    let Some((site_id, organization_id)) = row else {
        return Err(ContentError::FormNotFound.into());
    };
    if let Some(own) = current.user.organization_id
        && own != organization_id
    {
        return Err(ContentError::FormNotFound.into());
    }
    ensure_same_organization(current, Some(organization_id))?;
    let form = omnion_content::read_form(state.db().pool(), site_id, id).await?;
    let fields = omnion_content::list_fields(state.db().pool(), id).await?;
    Ok((form, fields))
}

async fn form_detail(
    state: &AppState,
    form: &Form,
    fields: &[FormField],
) -> Result<FormDetailBody, ApiError> {
    Ok(FormDetailBody {
        form: form_body(state, form).await?,
        fields: fields.iter().map(FormFieldBody::from).collect(),
        vocabulary: FormsVocabularyBody {
            field_types: omnion_content::FIELD_TYPES.to_vec(),
            statuses: omnion_content::SUBMISSIONS_STATUSES.to_vec(),
            submit_actions: omnion_content::SUBMIT_ACTIONS.to_vec(),
            max_fields: omnion_content::MAX_ANSWERS,
            max_answer_length: omnion_content::MAX_ANSWER_LENGTH,
        },
    })
}

/// A form as the list reads it, with the counts the card shows.
async fn form_body(state: &AppState, form: &Form) -> Result<FormBody, ApiError> {
    let site_key = site_of(state, form.site_id).await?.key;
    let pool = state.db().pool();
    let field_count =
        i64::try_from(omnion_content::list_fields(pool, form.id).await?.len()).unwrap_or(0);
    let counts = inbox_counts(pool, form.id).await?;
    Ok(FormBody {
        id: form.id,
        site_id: form.site_id,
        site_key,
        key: form.key.clone(),
        name: form.name.clone(),
        status: form.status.clone(),
        // The settings travel with the form rather than in a second endpoint, because the
        // settings drawer and the builder's Publish button are one screen: a drawer that had to
        // fetch before it could show what the form currently does is a drawer that renders the
        // form's *defaults* half the time, and a defaults panel is a settings panel nobody trusts.
        submit_action: form.submit_action.clone(),
        submit_message: form.submit_message.clone(),
        redirect_url: form.redirect_url.clone(),
        notify_emails: form.notify_emails.clone(),
        honeypot: form.honeypot,
        min_fill_seconds: form.min_fill_seconds,
        rate_limit_per_hour: form.rate_limit_per_hour,
        retention_days: form.retention_days,
        field_count: usize::try_from(field_count).unwrap_or(0),
        unread_count: counts.new,
        spam_count: counts.spam,
        updated_at: form.updated_at,
    })
}

async fn inbox_counts(pool: &sqlx::PgPool, form_id: Uuid) -> Result<InboxCountsBody, ApiError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select status, count(*) from cms_form_submissions where form_id = $1 group by status",
    )
    .bind(form_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("counting the inbox: {error}"),
        )
    })?;
    let count = |wanted: &str| -> i64 {
        rows.iter()
            .find(|(status, _)| status == wanted)
            .map_or(0, |(_, count)| *count)
    };
    Ok(InboxCountsBody {
        new: count("new"),
        read: count("read"),
        spam: count("spam"),
        archived: count("archived"),
    })
}

/// The key of the first field that looks like a name / an address.
fn field_key(fields: &[FormField], wanted: &str) -> Option<String> {
    fields
        .iter()
        .find(|field| field.key == wanted)
        .or_else(|| fields.iter().find(|field| field.field_type == wanted))
        .map(|field| field.key.clone())
}

/// One answer as plain text, when the form carries that field.
fn answer_text(answers: &Value, key: Option<&str>) -> Option<String> {
    let key = key?;
    match answers.get(key)? {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::String(_) | Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The three columns the inbox derives from the answers.
#[derive(Debug, Serialize)]
pub struct SummaryBody {
    /// The name field, when the form has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The e-mail field, when the form has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Every answer as one line, which is the Summary column.
    pub text: String,
}

/// The response type the export returns.
use axum::response::Response;

/// What the API can tell about the sender without storing it.
///
/// A raw IP address in a form inbox turns the owner's own database into a log of every visitor
/// who used the contact form, for no analytical gain: the rate limiter only ever asks "the same
/// sender again", and a hash answers that.
#[derive(Debug, Default)]
pub struct SenderFingerprint {
    /// Hashed address.
    pub ip_hash: Option<String>,
    /// Hashed user agent.
    pub user_agent_hash: Option<String>,
}

/// Hash the sender's address and user agent.
fn sender_fingerprint(headers: &HeaderMap) -> SenderFingerprint {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    SenderFingerprint {
        ip_hash: forwarded.map(|address| hash_fingerprint(&format!("ip:{address}"))),
        user_agent_hash: agent.map(|value| hash_fingerprint(&format!("ua:{value}"))),
    }
}

/// A stable, non-reversible fingerprint.
///
/// SHA-256 with a *fixed* salt: not a password, so no work factor is needed, and the value only
/// ever has to be compared with itself. A per-installation salt would be better privacy and is
/// not available here without a config surface that does not exist yet — the honest note is in
/// the doc comment rather than pretended away.
fn hash_fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_instant(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|_| {
        ApiError::bad_request(
            "invalid_instant",
            format!("{value:?} is not an RFC 3339 instant, e.g. 2026-10-01T09:00:00Z"),
        )
    })
}

async fn site_of(state: &AppState, site_id: Uuid) -> Result<omnion_identity::Site, ApiError> {
    omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<omnion_identity::Site, ApiError> {
    let site = site_of(state, site_id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Publish an event without failing the request that caused it — see `menus.rs`.
fn emit(
    state: &AppState,
    event: &'static str,
    payload: serde_json::Value,
) -> impl std::future::Future<Output = ()> {
    let pool = state.db().pool();
    async move {
        if let Err(error) = bus::emit(pool, NewEvent::new(event).payload(payload)).await {
            tracing::warn!(event, %error, "event bus refused an event");
        }
    }
}

/// What became of a submission's notification. The owner of a contact form has to be able to
/// answer "did the mail go out?", and the only way to answer it is for the attempt to leave a
/// record — a send that is silent in both directions is indistinguishable from a broken form.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationOutcome {
    /// Sent to every recipient.
    Sent,
    /// The builder named nobody, so there was nothing to send.
    NoRecipients,
    /// Platform mail is switched off (`OMNION_MAIL_ENABLED`), so nothing was attempted.
    MailDisabled,
    /// A transport error, with the transport's own words. The submission is stored either way.
    ///
    /// The reason is a `String` and not a `&'static str` because the only thing that knows why
    /// a send failed is the client that tried: an operator looking at the inbox wants to see the
    /// server's refusal ("550 no such user", "connection refused"), and replacing it with a
    /// fixed phrase throws away the only part they can act on.
    Failed(String),
}

/// Send a stored submission's notification to the addresses the builder named.
///
/// Three rules, and each of them is a decision rather than an implementation detail:
///
/// * **The words come from the content crate, the transport from the platform mail path.**
///   `omnion_automation::mail` is the same client the workflow engine's `send_email` step
///   uses, so a site that has configured SMTP for its automations gets its form notifications
///   for free and there is exactly one place where "this platform can send mail" is decided.
///
/// * **A failure never fails the submission.** The visitor's message is already a row, and
///   answering 500 to somebody who just sent you a contact form because your mail server is
///   unreachable makes them send it again — which is how a site gets five copies of the same
///   message and an owner with no idea why. The failure is recorded and logged instead.
///
/// * **The attempt is recorded, not just logged.** A log line rotates away and is invisible to
///   the person who actually needs it; the submission row carries the outcome forever, and the
///   inbox can show it.
#[allow(clippy::too_many_lines)]
async fn notify(
    state: &AppState,
    form: &Form,
    fields: &[FormField],
    submission: &omnion_content::Submission,
) -> NotificationOutcome {
    let note = omnion_content::forms_notify::render(form, fields, submission);
    if note.to.is_empty() {
        // Recording happens on EVERY outcome, including the two that send nothing — and the
        // first version of this returned early without it, which quietly undid the reason the
        // columns exist: "the builder named nobody" is an owner's fix in the form editor and
        // "platform mail is switched off" is an operator's fix in the environment, and the
        // walk caught both rows sitting at NULL, which is indistinguishable from a route that
        // was never reached.
        record_notification(state, form, submission, NotificationOutcome::NoRecipients).await;
        return NotificationOutcome::NoRecipients;
    }
    let mail = &state.config().mail;
    if !mail.is_usable() {
        tracing::warn!(
            form = %form.key,
            recipients = note.to.len(),
            "a submission is waiting to be notified but platform mail is switched off"
        );
        record_notification(state, form, submission, NotificationOutcome::MailDisabled).await;
        return NotificationOutcome::MailDisabled;
    }

    let mut settings =
        omnion_automation::MailSettings::new(mail.host.clone(), mail.port, mail.from.clone())
            .with_sending(true)
            .with_timeout(StdDuration::from_millis(mail.timeout_ms.max(1)));
    if let (Some(username), Some(password)) = (&mail.username, &mail.password) {
        settings = settings.with_credentials(username.clone(), password.clone());
    }

    // One message per recipient rather than one message with twenty `To:` headers: a shared
    // inbox then shows which address was reached, and nobody's address is disclosed to the
    // other nineteen. A recipient that fails does not stop the others — a form that notifies
    // three people and reaches one is still worth having sent, and the failure is reported.
    let mut sent = 0usize;
    let mut last_error: Option<String> = None;
    for address in &note.to {
        let email =
            omnion_automation::Email::new(address.clone(), note.subject.clone(), note.body.clone());
        match omnion_automation::mail::send(&settings, &email).await {
            Ok(()) => sent += 1,
            Err(error) => {
                tracing::warn!(%address, form = %form.key, %error, "a form notification did not send");
                last_error = Some(error.to_string());
            }
        }
    }

    let outcome = if sent == note.to.len() {
        NotificationOutcome::Sent
    } else {
        NotificationOutcome::Failed(
            last_error.unwrap_or_else(|| format!("{sent} of {} recipients reached", note.to.len())),
        )
    };
    record_notification(state, form, submission, outcome.clone()).await;
    outcome
}

/// Persist what happened to the notification, on the submission's own row.
async fn record_notification(
    state: &AppState,
    form: &Form,
    submission: &omnion_content::Submission,
    outcome: NotificationOutcome,
) {
    let (status, detail): (&str, Option<&str>) = match &outcome {
        NotificationOutcome::Sent => ("sent", None),
        NotificationOutcome::NoRecipients => ("skipped", Some("no recipients configured")),
        NotificationOutcome::MailDisabled => ("skipped", Some("platform email is switched off")),
        NotificationOutcome::Failed(reason) => ("failed", Some(reason.as_str())),
    };
    if let Err(error) = sqlx::query(
        "update cms_form_submissions set notified_at = now(), notify_status = $2, \
         notify_error = $3 where id = $1",
    )
    .bind(submission.id)
    .bind(status)
    .bind(detail)
    .execute(state.db().pool())
    .await
    {
        // The notification itself already went out (or did not); failing to note that down is
        // a bookkeeping problem, not a reason to take the site down.
        tracing::warn!(submission = %submission.id, %error, "could not record the notification outcome");
        return;
    }
    // The owner's audit trail: a form notification is a message leaving the platform with
    // somebody's words in it, and it is the first thing asked about when one goes missing.
    // The actor is the platform itself, not a signed-in user — nobody clicked anything — so
    // the entry is built the way the other system-written entries are.
    let entry = NewAuditEntry {
        organization_id: Some(form.organization_id),
        actor_user_id: None,
        actor_type: omnion_audit::ActorType::System,
        action: "form.notification",
        target_type: Some("form_submission"),
        target_id: Some(submission.id.to_string()),
        metadata: json!({
            "form_id": form.id,
            "form_key": form.key,
            "status": status,
            "detail": detail,
        }),
        ip_address: None,
    };
    if let Err(error) = omnion_audit::record(state.db().pool(), entry).await {
        tracing::warn!(%error, "could not audit the form notification");
    }
}
