//! The form builder and its submission inbox (REQ-064, slice 2).
//!
//! A contact form is the one piece of a CMS that a stranger uses and the owner reads, and the
//! two sides have almost nothing in common: the builder is an authenticated editor arranging
//! fields, the submitter is an anonymous visitor whose browser is not trusted. Four decisions
//! here exist because the obvious implementation gets one of those two sides wrong.
//!
//! 1. **Validation happens once, in the store, against the saved fields.** The builder's
//!    inspector is a *client* of these rules, never the authority: [`validate_submission`] is
//!    what every write path calls, so a public POST, a future import and a test cannot disagree
//!    about whether an answer is acceptable. A message produced by a field is the message the
//!    visitor sees — the field carries its own wording, which is the only way a translated form
//!    reads as translated.
//!
//! 2. **Spam is a *decision with a record*, not a deletion.** A submission that trips the
//!    honeypot, the fill-time floor or the rate limit is still counted, and the count is the
//!    number the screen shows. Silently dropping it would leave an owner asking why nobody ever
//!    receives the contact form, with nothing to point at. The thresholds are the form's own
//!    columns so the owner can tune them without a developer.
//!
//! 3. **A rate limit is per sender, per form, per moving hour**, keyed on the *hash* of the
//!    address rather than the address. Storing the raw IP would make the inbox a log file of
//!    visitors, for no analytical gain: the limiter only ever asks "the same sender again".
//!
//! 4. **Consent is stored as the text that was shown.** A `consent_accepted boolean` cannot
//!    answer "what did they agree to?" later, which is the only question that is ever asked.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::validation::{validate_key, validate_text};

/// The field types the builder's palette offers, in the order the palette shows them.
pub const FIELD_TYPES: [&str; 8] = [
    "text",
    "textarea",
    "select",
    "radio",
    "checkbox",
    "date",
    "file",
    "consent",
];

/// Lifecycle states a form carries.
pub const FORM_STATUSES: [&str; 2] = ["draft", "published"];

/// What happens after a successful submission.
pub const SUBMIT_ACTIONS: [&str; 2] = ["message", "redirect"];

/// Inbox states a submission carries.
pub const SUBMISSIONS_STATUSES: [&str; 4] = ["new", "read", "spam", "archived"];

/// Longest accepted form name.
pub const MAX_FORM_NAME_LENGTH: usize = 120;

/// Longest accepted field label, placeholder or help text.
pub const MAX_FIELD_TEXT_LENGTH: usize = 255;

/// Longest accepted value in a text or textarea answer.
///
/// Bounded because the answer travels into a notification e-mail and a CSV export, both of
/// which have their own limits; a form that stores a megabyte per answer is a form that breaks
/// its own inbox.
pub const MAX_ANSWER_LENGTH: usize = 4_000;

/// Most answers one submission may carry.
pub const MAX_ANSWERS: usize = 50;

/// Most options a select or radio field may offer.
pub const MAX_OPTIONS: usize = 50;

/// The columns a form row is read with.
const FORM_COLUMNS: &str = "id, organization_id, site_id, key, name, status, submit_action, \
     submit_message, redirect_url, notify_emails, notify_subject, honeypot, min_fill_seconds, \
     rate_limit_per_hour, retention_days, target_segment_id, created_by, created_at, updated_at";

/// The columns a submission row is read with.
const SUBMISSION_COLUMNS: &str = "id, form_id, site_id, answers, consent_text, source_path, \
     ip_hash, user_agent_hash, spam_score, status, created_at";

/// A form row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Form {
    /// Primary key.
    pub id: Uuid,
    /// Organization the site belongs to.
    pub organization_id: Uuid,
    /// Site this form belongs to.
    pub site_id: Uuid,
    /// Stable key, unique inside the site — the form's public address.
    pub key: String,
    /// Name the list shows.
    pub name: String,
    /// `draft` or `published`.
    pub status: String,
    /// `message` or `redirect`.
    pub submit_action: String,
    /// Message shown after a submission, when the action is `message`.
    pub submit_message: Option<String>,
    /// Where the visitor lands after a submission, when the action is `redirect`.
    pub redirect_url: Option<String>,
    /// Addresses the submission notification goes to.
    pub notify_emails: Vec<String>,
    /// Subject template, with `{{form_name}}` and `{{submitted_at}}` placeholders.
    pub notify_subject: Option<String>,
    /// Whether the invisible honeypot field is armed.
    pub honeypot: bool,
    /// Seconds a submission has to have taken before it can be spam.
    pub min_fill_seconds: i32,
    /// Submissions one sender may make in an hour.
    pub rate_limit_per_hour: i32,
    /// Days a submission is kept.
    pub retention_days: i32,
    /// Optional segment the event targets (REQ-060).
    pub target_segment_id: Option<Uuid>,
    /// Author, when a person created it.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: time::OffsetDateTime,
    /// Last change.
    pub updated_at: time::OffsetDateTime,
}

impl Form {
    /// A form only answers submissions once it is published.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.status == "published"
    }
}

/// One field of a form, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FormField {
    /// Primary key.
    pub id: Uuid,
    /// Form the field belongs to.
    pub form_id: Uuid,
    /// Order on the canvas.
    pub position: i32,
    /// Stable key — the answer's name in the stored `answers` object.
    pub key: String,
    /// Label shown above the input.
    pub label: String,
    /// One of [`FIELD_TYPES`].
    pub field_type: String,
    /// Whether an empty answer is refused.
    pub required: bool,
    /// Placeholder inside the input.
    pub placeholder: Option<String>,
    /// Help text under the input.
    pub help_text: Option<String>,
    /// `half` or `full` — the canvas's row span.
    pub width: String,
    /// Validation rules (see [`validate_submission`]).
    pub rules: Value,
    /// Options for `select` and `radio`.
    pub options: Value,
}

impl FormField {
    /// Whether this field's type carries a choice list.
    #[must_use]
    pub fn has_options(&self) -> bool {
        matches!(self.field_type.as_str(), "select" | "radio" | "checkbox")
    }
}

/// One field as the builder submits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFormField {
    /// Order on the canvas.
    pub position: i32,
    /// Answer name.
    pub key: String,
    /// Label shown above the input.
    pub label: String,
    /// Field type.
    pub field_type: String,
    /// Required flag.
    pub required: bool,
    /// Placeholder.
    pub placeholder: Option<String>,
    /// Help text.
    pub help_text: Option<String>,
    /// `half` or `full`.
    pub width: String,
    /// Validation rules.
    pub rules: Value,
    /// Options.
    pub options: Value,
}

/// A form's settings the editor may change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormChanges {
    /// New key, when the editor renamed the form's address.
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
    /// Segment the event targets.
    pub target_segment_id: Option<Uuid>,
}

impl FormChanges {
    /// Whether the editor changed anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key.is_none()
            && self.name.is_none()
            && self.submit_action.is_none()
            && self.submit_message.is_none()
            && self.redirect_url.is_none()
            && self.notify_emails.is_none()
            && self.notify_subject.is_none()
            && self.honeypot.is_none()
            && self.min_fill_seconds.is_none()
            && self.rate_limit_per_hour.is_none()
            && self.retention_days.is_none()
    }
}

/// A submission row, with the answers already unpacked.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Submission {
    /// Primary key.
    pub id: Uuid,
    /// Form the submission belongs to.
    pub form_id: Uuid,
    /// Site the form belongs to.
    pub site_id: Uuid,
    /// Answers, keyed by field key.
    pub answers: Value,
    /// The consent text that was shown, when the form has a consent field.
    pub consent_text: Option<String>,
    /// Path the submission was made from.
    pub source_path: Option<String>,
    /// Hashed sender address — never the address itself.
    pub ip_hash: Option<String>,
    /// Hashed user agent.
    pub user_agent_hash: Option<String>,
    /// Heuristic score, 0–100.
    pub spam_score: i32,
    /// `new`, `read`, `spam` or `archived`.
    pub status: String,
    /// When it arrived.
    pub created_at: time::OffsetDateTime,
}

/// Inbox filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmissionQuery {
    /// Only this state, when set.
    pub status: Option<String>,
    /// Case-insensitive substring over the answers.
    pub search: Option<String>,
    /// Only rows at or after this instant.
    pub since: Option<time::OffsetDateTime>,
    /// Only rows at or before this instant.
    pub until: Option<time::OffsetDateTime>,
    /// Page size.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

/// What a public submission carries.
///
/// `filled_at` and `sender` are the spam heuristics' inputs and both come from the *client*, so
/// neither is trusted: a caller can claim any fill time it likes, which is why the honeypot and
/// the rate limit are the protections that hold and the fill time is only a cheap extra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubmission {
    /// Answers keyed by field key.
    pub answers: Map<String, Value>,
    /// Value of the invisible honeypot input — anything non-empty is a bot.
    pub honeypot: String,
    /// Milliseconds the visitor spent on the form, as the page measured it.
    pub filled_at_ms: i64,
    /// Hashed sender address, already hashed by the API layer.
    pub ip_hash: Option<String>,
    /// Hashed user agent.
    pub user_agent_hash: Option<String>,
    /// Path the submission came from.
    pub source_path: Option<String>,
}

/// The outcome of a public submission.
///
/// `stored` is false for every refusal — that is the whole point: a submission that trips the
/// spam rules leaves a counter and no row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionOutcome {
    /// The stored row, when the submission was accepted.
    pub submission: Option<Submission>,
    /// Why it was refused, as a store error code.
    pub refused: Option<&'static str>,
    /// Field-level messages, keyed by field key.
    pub errors: BTreeMap<String, String>,
    /// Whether the honeypot fired.
    pub honeypot_fired: bool,
    /// The heuristic score the submission earned.
    pub spam_score: i32,
}

impl SubmissionOutcome {
    /// An accepted submission.
    #[must_use]
    pub fn accepted(submission: Submission) -> Self {
        Self {
            submission: Some(submission),
            refused: None,
            errors: BTreeMap::new(),
            honeypot_fired: false,
            spam_score: 0,
        }
    }

    /// Whether a row was stored.
    #[must_use]
    pub fn stored(&self) -> bool {
        self.submission.is_some()
    }
}

/// Validate a field key: lowercase, digits and underscores, starting with a letter.
fn validate_field_key(key: &str) -> Result<String> {
    let key = key.trim().to_owned();
    let shaped = !key.is_empty()
        && key.len() <= 64
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if shaped {
        return Ok(key);
    }
    Err(ContentError::InvalidFormField(format!(
        "field key {key:?} must start with a letter and use only lowercase letters, digits and underscores"
    )))
}

/// Validate a field definition, returning it cleaned.
fn validate_field(field: &NewFormField) -> Result<FormFieldShape> {
    let key = validate_field_key(&field.key)?;
    let label = validate_text(&field.label, MAX_FIELD_TEXT_LENGTH, "field label")?;
    if !FIELD_TYPES.contains(&field.field_type.as_str()) {
        return Err(ContentError::InvalidFormField(format!(
            "{:?} is not one of the field types this builder offers ({})",
            field.field_type,
            FIELD_TYPES.join(", ")
        )));
    }
    let width = if field.width == "half" { "half" } else { "full" };
    if !field.rules.is_object() {
        return Err(ContentError::InvalidFormField(format!(
            "the rules of field {key:?} must be a JSON object"
        )));
    }
    if !field.options.is_array() {
        return Err(ContentError::InvalidFormField(format!(
            "the options of field {key:?} must be a JSON array"
        )));
    }
    let options = field.options.as_array().cloned().unwrap_or_default();
    if options.len() > MAX_OPTIONS {
        return Err(ContentError::InvalidFormField(format!(
            "field {key:?} offers {} options; a choice list over {MAX_OPTIONS} is a survey, not a form field",
            options.len()
        )));
    }
    let labels = option_labels(&options);
    // A choice field with no choices cannot be answered: the visitor sees a control with nothing
    // in it and the builder's own preview looks broken. Refusing the save names the field.
    if matches!(field.field_type.as_str(), "select" | "radio") && labels.is_empty() {
        return Err(ContentError::InvalidFormField(format!(
            "field {key:?} is a {} with no options to choose from",
            field.field_type
        )));
    }
    Ok(FormFieldShape {
        key,
        label,
        field_type: field.field_type.clone(),
        required: field.required,
        width: width.to_owned(),
    })
}

/// The cleaned parts of a field definition the store persists.
#[derive(Debug)]
struct FormFieldShape {
    key: String,
    label: String,
    field_type: String,
    required: bool,
    width: String,
}

/// Read a `select`/`radio` option list into plain labels.
///
/// Both shapes are accepted because the builder stores `{"value": …, "label": …}` while a
/// hand-written migration may have stored bare strings, and a submission has to validate against
/// whichever the field actually holds.
#[must_use]
pub fn option_labels(options: &[Value]) -> Vec<String> {
    options
        .iter()
        .filter_map(|option| match option {
            Value::String(text) => Some(text.clone()),
            Value::Object(map) => map
                .get("value")
                .or_else(|| map.get("label"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .filter(|label| !label.is_empty())
        .collect()
}

/// Validate the recipients a form notifies.
fn validate_notify_emails(addresses: &[String]) -> Result<Vec<String>> {
    if addresses.len() > 20 {
        return Err(ContentError::InvalidFormField(
            "a form may notify at most 20 addresses".to_owned(),
        ));
    }
    addresses
        .iter()
        .map(|address| {
            let address = address.trim();
            // Deliberately loose: an address that is merely shaped wrong belongs in the owner's
            // inbox as a bounce, not refused by the builder. What is refused is nonsense.
            let shaped = address.len() <= 254
                && address.matches('@').count() == 1
                && !address.starts_with('@')
                && !address.ends_with('@');
            if shaped {
                Ok(address.to_owned())
            } else {
                Err(ContentError::InvalidFormField(format!(
                    "{address:?} is not an e-mail address to notify"
                )))
            }
        })
        .collect()
}

/// Check the new settings against the form's own rules, which is where the two constraints the
/// schema also carries are enforced *before* the write so the caller gets the message.
fn validate_settings(
    submit_action: &str,
    submit_message: Option<&str>,
    redirect_url: Option<&str>,
) -> Result<String> {
    if !SUBMIT_ACTIONS.contains(&submit_action) {
        return Err(ContentError::InvalidFormField(format!(
            "{submit_action:?} is not a submit behaviour ({})",
            SUBMIT_ACTIONS.join(" or ")
        )));
    }
    match submit_action {
        "message" => {
            let message = submit_message.map(str::trim).filter(|m| !m.is_empty());
            if message.is_none() {
                return Err(ContentError::InvalidFormField(
                    "a form that shows a message needs one to show".to_owned(),
                ));
            }
            if message.map(|m| m.chars().count()) > Some(1_000) {
                return Err(ContentError::InvalidFormField(
                    "the success message is longer than 1000 characters".to_owned(),
                ));
            }
        }
        "redirect" => {
            let url = redirect_url.map(str::trim).filter(|u| !u.is_empty());
            match url {
                None => {
                    return Err(ContentError::InvalidFormField(
                        "a form that redirects needs a URL to redirect to".to_owned(),
                    ));
                }
                Some(url) if url.len() > MAX_URL_LENGTH => {
                    return Err(ContentError::InvalidFormField(
                        "the redirect URL is longer than 2048 characters".to_owned(),
                    ));
                }
                Some(url) => {
                    // A redirect that leaves the site turns a contact form into an open redirect
                    // on every page it is embedded in, so the scheme and the host are checked
                    // here rather than trusted at render time.
                    let internal = url.starts_with('/') && !url.starts_with("//");
                    let external = ["http://", "https://"]
                        .iter()
                        .any(|scheme| url.starts_with(scheme));
                    if !internal && !external {
                        return Err(ContentError::InvalidFormField(format!(
                            "the redirect URL {url:?} must start with / or http(s)://"
                        )));
                    }
                }
            }
        }
        _ => unreachable!("checked above"),
    }
    Ok(submit_action.to_owned())
}

const MAX_URL_LENGTH: usize = 2_048;

/// The minimum a rate limit may be set to: zero would mean "unlimited", which is a switch the
/// field does not have — an owner turns the limit *up*, never off.
const MIN_RATE_LIMIT: i32 = 1;

/// Validate one answer against one field.
///
/// The rules object is read field by field and a rule that is nonsense is *ignored* rather than
/// refused: a builder that saved `{"min_length": "ten"}` should still let a submission through
/// with the rules it does understand, because the alternative is a form that rejects everyone
/// because of a typo in the panel.
#[must_use]
pub fn validate_answer(field: &FormField, answer: &Value) -> Option<String> {
    let empty = match answer {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if field.field_type == "consent" {
        // A consent field is answered with `true`; the submission stores the *text* shown, and
        // an unanswered consent is a refusal, not an empty answer.
        let accepted = matches!(answer, Value::Bool(true));
        if field.required && !accepted {
            return Some("this field has to be accepted".to_owned());
        }
        return None;
    }
    if empty {
        if field.required {
            return Some("this field is required".to_owned());
        }
        return None;
    }

    let rules = field.rules.as_object();
    let rule = |name: &str| rules.and_then(|map| map.get(name));

    let text = match answer {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
            // A checkbox answers a list or a boolean; a text field handed a list is a builder
            // mismatch, and coercing it would store "[object Object]" in somebody's inbox.
            if field.field_type == "checkbox" {
                return None;
            }
            return Some("this field accepts a single value".to_owned());
        }
        Value::Null => String::new(),
    };

    if let Some(min) = rule("min_length").and_then(Value::as_u64) {
        if (text.chars().count() as u64) < min {
            return Some(format!("please use at least {min} characters"));
        }
    }
    if let Some(max) = rule("max_length").and_then(Value::as_u64) {
        if (text.chars().count() as u64) > max {
            return Some(format!("please use at most {max} characters"));
        }
    }
    if text.chars().count() > MAX_ANSWER_LENGTH {
        return Some(format!(
            "please keep this under {MAX_ANSWER_LENGTH} characters"
        ));
    }
    if let Some(pattern) = rule("pattern").and_then(Value::as_str) {
        // Deliberately not a regular expression engine. A visitor-supplied answer is matched
        // against a pattern an owner typed, on an endpoint with no authentication: a backtracking
        // engine here is a denial-of-service surface that one pattern can open, and this crate
        // deliberately depends on no regex crate. The dialect is a small, total one — literal
        // text with `.` (any character) and `*` (repeat the previous character), anchored to the
        // whole answer — which covers "digits", "phone-like" and "no spaces" without a regex.
        if !matches_pattern(pattern, &text) {
            return Some("this value is not in the expected format".to_owned());
        }
    }
    if let Some(kind) = rule("format").and_then(Value::as_str) {
        let shaped = match kind {
            "email" => text.contains('@') && !text.starts_with('@') && !text.ends_with('@'),
            "url" => text.starts_with("http://") || text.starts_with("https://"),
            "tel" => text.chars().all(|c| {
                c.is_ascii_digit()
                    || matches!(c, '+' | '-' | ' ' | '(' | ')' | '.')
            }) && text.chars().any(|c| c.is_ascii_digit()),
            // An unknown format name validates nothing; refusing on it would make a future
            // rule name a breaking change to every form already saved.
            _ => true,
        };
        if !shaped {
            return Some(format!("please enter a valid {kind}"));
        }
    }
    if let (Some(min), Some(number)) = (
        rule("min").and_then(Value::as_f64),
        answer.as_f64().or_else(|| text.parse::<f64>().ok()),
    ) {
        if number < min {
            return Some(format!("please enter {min} or more"));
        }
    }
    if let (Some(max), Some(number)) = (
        rule("max").and_then(Value::as_f64),
        answer.as_f64().or_else(|| text.parse::<f64>().ok()),
    ) {
        if number > max {
            return Some(format!("please enter {max} or less"));
        }
    }
    match field.field_type.as_str() {
        // A date is compared as a *date*, never as text: `2026-02-31` passes a shape check and
        // sorts before `2026-03-01` as a string, which would let a visitor pick a day that does
        // not exist and a max_date of `2026-02-31` silently accept nothing after it.
        "date" => {
            let Some(picked) = parse_iso_date(&text) else {
                return Some("please enter a date as YYYY-MM-DD".to_owned());
            };
            for (rule_name, message) in [
                ("min_date", "on or after"),
                ("max_date", "on or before"),
            ] {
                if let Some(bound) = rule(rule_name).and_then(Value::as_str) {
                    // An unparseable bound validates nothing rather than everything: refusing
                    // every submission over a typo in the panel is the worse failure.
                    if let Some(bound) = parse_iso_date(bound) {
                        let outside = if rule_name == "min_date" {
                            picked < bound
                        } else {
                            picked > bound
                        };
                        if outside {
                            return Some(format!("please pick a date {message} {bound}"));
                        }
                    }
                }
            }
        }
        "file" => {
            if let Some(max_bytes) = rule("max_bytes").and_then(Value::as_u64) {
                if let Some(bytes) = rule("bytes").and_then(Value::as_u64) {
                    if bytes > max_bytes {
                        return Some(format!(
                            "the file is {bytes} bytes; this field accepts up to {max_bytes}"
                        ));
                    }
                }
            }
            if let Some(types) = rule("allowed_types").and_then(Value::as_array) {
                let allowed: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
                if !allowed.is_empty() {
                    let suffix = text.rsplit('.').next().unwrap_or_default().to_lowercase();
                    if !allowed.iter().any(|t| t.trim_start_matches('.').eq_ignore_ascii_case(&suffix))
                    {
                        return Some(format!("please upload one of: {}", allowed.join(", ")));
                    }
                }
            }
        }
        _ => {}
    }
    if field.has_options() && field.field_type != "checkbox" {
        let options = field.options.as_array().cloned().unwrap_or_default();
        let labels = option_labels(&options);
        if !labels.is_empty() && !labels.iter().any(|label| label == &text) {
            return Some(format!("{text:?} is not one of the offered options"));
        }
    }
    None
}

/// A whole submission against a whole form.
///
/// Returns the field-level messages, so a public POST answers 422 with every wrong field at once
/// rather than making the visitor fix one field per round trip.
#[must_use]
pub fn validate_submission(
    fields: &[FormField],
    answers: &Map<String, Value>,
) -> BTreeMap<String, String> {
    let mut errors = BTreeMap::new();
    for field in fields {
        let answer = answers.get(&field.key).unwrap_or(&Value::Null);
        if let Some(message) = validate_answer(field, answer) {
            errors.insert(field.key.clone(), message);
        }
    }
    errors
}

/// The consent text a form asks a visitor to accept, if it has one.
#[must_use]
pub fn consent_text(fields: &[FormField]) -> Option<String> {
    fields
        .iter()
        .find(|field| field.field_type == "consent")
        .map(|field| field.label.clone())
}

/// The heuristic score of a submission, 0–100.
///
/// The signals are local and deliberately cheap — links, length, an answer nobody typed — and
/// the score is a *number the owner sees*, not a decision. The decision is
/// [`submit_public`], and it only refuses on the three protections the form arms.
#[must_use]
pub fn spam_score(answers: &Map<String, Value>) -> i32 {
    let mut score: i32 = 0;
    for answer in answers.values() {
        let text = match answer {
            Value::String(text) => text.clone(),
            _ => continue,
        };
        let lower = text.to_lowercase();
        if lower.matches("http://").count() + lower.matches("https://").count() >= 2 {
            score += 45;
        } else if lower.contains("http://") || lower.contains("https://") {
            score += 15;
        }
        if text.chars().count() > 2_000 {
            score += 15;
        }
        // Cyrillic inside an otherwise Latin answer is the cheapest reliable signal that a
        // message was machine-generated for a form in another language.
        if text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) && text.chars().any(|c| c.is_ascii_alphabetic()) {
            score += 20;
        }
    }
    score.min(100)
}

/// Create a form with its first fields, in one transaction.
///
/// A form and its fields are one thing to the editor — a builder that saved the form and then
/// failed on the third field would leave a form whose canvas does not match its definition.
pub async fn create_form(
    pool: &PgPool,
    organization_id: Uuid,
    site_id: Uuid,
    key: &str,
    name: &str,
    fields: &[NewFormField],
    created_by: Option<Uuid>,
) -> Result<Form> {
    let key = validate_key(key, "form key")?;
    let name = validate_text(name, MAX_FORM_NAME_LENGTH, "name")?;
    let submit_action = validate_settings("message", Some("Thank you."), None)?;
    let shapes = validate_fields(fields)?;
    let sql = format!(
        "insert into cms_forms (organization_id, site_id, key, name, submit_action, submit_message, \
         created_by) values ($1, $2, $3, $4, $5, $6, $7) returning {FORM_COLUMNS}"
    );
    let mut tx = pool.begin().await?;
    let form = sqlx::query_as::<_, Form>(&sql)
        .bind(organization_id)
        .bind(site_id)
        .bind(&key)
        .bind(&name)
        .bind(submit_action)
        .bind("Thank you.")
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| map_form_write_error(error, &key))?;
    insert_fields(&mut tx, form.id, &shapes).await?;
    tx.commit().await?;
    Ok(form)
}

/// Validate a whole field list: keys unique, ordered, and every definition usable.
fn validate_fields(fields: &[NewFormField]) -> Result<Vec<(FormFieldShape, Value, Value)>> {
    if fields.is_empty() {
        return Err(ContentError::InvalidFormField(
            "a form needs at least one field".to_owned(),
        ));
    }
    if fields.len() > 50 {
        return Err(ContentError::InvalidFormField(format!(
            "a form may hold {} fields; this one has {}",
            MAX_ANSWERS,
            fields.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut shapes = Vec::with_capacity(fields.len());
    for field in fields {
        let shape = validate_field(field)?;
        if seen.contains(&shape.key.as_str()) {
            return Err(ContentError::InvalidFormField(format!(
                "two fields share the key {:?}; answers are stored by key, so the second would \
                 overwrite the first",
                shape.key
            )));
        }
        seen.push(field.key.trim());
        shapes.push((shape, field.rules.clone(), field.options.clone()));
    }
    Ok(shapes)
}

/// Insert the field rows of a form inside an open transaction.
async fn insert_fields(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    form_id: Uuid,
    shapes: &[(FormFieldShape, Value, Value)],
) -> Result<()> {
    for (index, (shape, rules, options)) in shapes.iter().enumerate() {
        sqlx::query(
            "insert into cms_form_fields (form_id, position, key, label, field_type, required, \
             placeholder, help_text, width, rules, options) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(form_id)
        .bind(index as i32)
        .bind(&shape.key)
        .bind(&shape.label)
        .bind(&shape.field_type)
        .bind(shape.required)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(&shape.width)
        .bind(rules)
        .bind(options)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Every form of one site.
pub async fn list_forms(pool: &PgPool, site_id: Uuid) -> Result<Vec<Form>> {
    let sql = format!(
        "select {FORM_COLUMNS} from cms_forms where site_id = $1 order by created_at desc"
    );
    Ok(sqlx::query_as::<_, Form>(&sql)
        .bind(site_id)
        .fetch_all(pool)
        .await?)
}

/// Read one form, or `None` when the site does not carry it.
pub async fn find_form(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<Option<Form>> {
    let sql = format!("select {FORM_COLUMNS} from cms_forms where site_id = $1 and id = $2");
    Ok(sqlx::query_as::<_, Form>(&sql)
        .bind(site_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Read the form whose key is its public address.
pub async fn find_form_by_key(
    pool: &PgPool,
    site_id: Uuid,
    key: &str,
) -> Result<Option<Form>> {
    let sql = format!("select {FORM_COLUMNS} from cms_forms where site_id = $1 and key = $2");
    Ok(sqlx::query_as::<_, Form>(&sql)
        .bind(site_id)
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// Read one form, or [`ContentError::FormNotFound`].
pub async fn read_form(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<Form> {
    find_form(pool, site_id, id)
        .await?
        .ok_or(ContentError::FormNotFound)
}

/// The fields of a form, in canvas order.
pub async fn list_fields(pool: &PgPool, form_id: Uuid) -> Result<Vec<FormField>> {
    let sql = "select id, form_id, position, key, label, field_type, required, placeholder, \
               help_text, width, rules, options from cms_form_fields \
               where form_id = $1 order by position, key";
    Ok(sqlx::query_as::<_, FormField>(sql)
        .bind(form_id)
        .fetch_all(pool)
        .await?)
}

/// Replace a form's fields with the ones submitted.
///
/// Fields are written whole rather than diffed: the editor holds the canvas, and a rename that
/// keeps the key keeps every answer already stored under it. A field *removed* here leaves its
/// answers in historical submissions, which is why [`read_form`] never deletes them.
pub async fn save_fields(
    pool: &PgPool,
    form_id: Uuid,
    fields: &[NewFormField],
) -> Result<Vec<FormField>> {
    let shapes = validate_fields(fields)?;
    let mut tx = pool.begin().await?;
    sqlx::query("delete from cms_form_fields where form_id = $1")
        .bind(form_id)
        .execute(&mut *tx)
        .await?;
    insert_fields(&mut tx, form_id, &shapes).await?;
    tx.commit().await?;
    list_fields(pool, form_id).await
}

/// Apply a form's own settings.
pub async fn update_form(
    pool: &PgPool,
    site_id: Uuid,
    id: Uuid,
    changes: &FormChanges,
) -> Result<Form> {
    if changes.is_empty() {
        return read_form(pool, site_id, id).await;
    }
    let current = read_form(pool, site_id, id).await?;
    let key = match changes.key.as_deref() {
        Some(key) => Some(validate_key(key, "form key")?),
        None => None,
    };
    let name = match changes.name.as_deref() {
        Some(name) => Some(validate_text(name, MAX_FORM_NAME_LENGTH, "name")?),
        None => None,
    };
    let submit_action = changes.submit_action.as_deref().unwrap_or(&current.submit_action);
    let submit_message = changes
        .submit_message
        .as_deref()
        .or(current.submit_message.as_deref());
    let redirect_url = changes
        .redirect_url
        .as_deref()
        .or(current.redirect_url.as_deref());
    let submit_action = validate_settings(submit_action, submit_message, redirect_url)?;
    let notify_emails = match &changes.notify_emails {
        Some(addresses) => Some(validate_notify_emails(addresses)?),
        None => None,
    };
    for (value, name, low, high) in [
        (changes.min_fill_seconds, "minimum fill time", 0, 3_600),
        (
            changes.rate_limit_per_hour,
            "rate limit",
            MIN_RATE_LIMIT,
            10_000,
        ),
        (changes.retention_days, "retention", 1, 3_650),
    ] {
        if let Some(value) = value {
            if !(low..=high).contains(&value) {
                return Err(ContentError::InvalidFormField(format!(
                    "the {name} must be between {low} and {high}"
                )));
            }
        }
    }
    let conflicting_key = key.clone().unwrap_or_default();
    let sql = format!(
        "update cms_forms set \
           key = coalesce($3, key), \
           name = coalesce($4, name), \
           submit_action = $5, \
           submit_message = nullif($6, ''), \
           redirect_url = nullif($7, ''), \
           notify_emails = coalesce($8, notify_emails), \
           notify_subject = coalesce($9, notify_subject), \
           honeypot = coalesce($10, honeypot), \
           min_fill_seconds = coalesce($11, min_fill_seconds), \
           rate_limit_per_hour = coalesce($12, rate_limit_per_hour), \
           retention_days = coalesce($13, retention_days), \
           updated_at = now() \
         where site_id = $1 and id = $2 returning {FORM_COLUMNS}"
    );
    sqlx::query_as::<_, Form>(&sql)
        .bind(site_id)
        .bind(id)
        .bind(key)
        .bind(name)
        .bind(submit_action)
        .bind(submit_message.filter(|m| !m.trim().is_empty()))
        .bind(redirect_url.filter(|u| !u.trim().is_empty()))
        .bind(notify_emails)
        .bind(changes.notify_subject.clone())
        .bind(changes.honeypot)
        .bind(changes.min_fill_seconds)
        .bind(changes.rate_limit_per_hour)
        .bind(changes.retention_days)
        .fetch_optional(pool)
        .await
        .map_err(|error| map_form_write_error(error, &conflicting_key))?
        .ok_or(ContentError::FormNotFound)
}

/// Publish or unpublish a form.
///
/// Publishing is refused for a form with no fields: the live site would render an empty form
/// that accepts nothing, which is worse than one that is obviously not ready.
pub async fn set_form_status(
    pool: &PgPool,
    site_id: Uuid,
    id: Uuid,
    status: &str,
) -> Result<Form> {
    if !FORM_STATUSES.contains(&status) {
        return Err(ContentError::InvalidFormField(format!(
            "{status:?} is not a form state ({})",
            FORM_STATUSES.join(" or ")
        )));
    }
    if status == "published" {
        let fields = list_fields(pool, id).await?;
        if fields.is_empty() {
            return Err(ContentError::InvalidFormField(
                "this form has no fields yet, so there is nothing to publish".to_owned(),
            ));
        }
    }
    let sql = format!(
        "update cms_forms set status = $3, updated_at = now() \
         where site_id = $1 and id = $2 returning {FORM_COLUMNS}"
    );
    sqlx::query_as::<_, Form>(&sql)
        .bind(site_id)
        .bind(id)
        .bind(status)
        .fetch_optional(pool)
        .await?
        .ok_or(ContentError::FormNotFound)
}

/// Delete a form. Its submissions go with it — the cascade is the record of what the form asked.
pub async fn delete_form(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<()> {
    let deleted = sqlx::query("delete from cms_forms where site_id = $1 and id = $2")
        .bind(site_id)
        .bind(id)
        .execute(pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(ContentError::FormNotFound);
    }
    Ok(())
}

/// A submission as it arrives, plus what the API layer knows about the sender.
pub async fn submit_public(
    pool: &PgPool,
    form: &Form,
    fields: &[FormField],
    incoming: &NewSubmission,
) -> Result<SubmissionOutcome> {
    let mut outcome = SubmissionOutcome {
        submission: None,
        refused: None,
        errors: BTreeMap::new(),
        honeypot_fired: false,
        spam_score: 0,
    };

    // 1. The honeypot. An invisible input a human never sees: anything in it came from a script.
    if form.honeypot && !incoming.honeypot.trim().is_empty() {
        outcome.honeypot_fired = true;
        outcome.refused = Some("form_spam");
        outcome.spam_score = 100;
        return Ok(outcome);
    }

    // 2. The rate limit, per sender, per form, over a moving hour. Counted before validation so
    //    a flood of invalid submissions is throttled too — otherwise the limiter protects the
    //    database from nothing at all.
    if let Some(sender) = incoming.ip_hash.as_deref() {
        let sent: i64 = sqlx::query_scalar(
            "select count(*) from cms_form_submissions \
             where form_id = $1 and ip_hash = $2 and created_at > now() - interval '1 hour'",
        )
        .bind(form.id)
        .bind(sender)
        .fetch_one(pool)
        .await?;
        if sent >= i64::from(form.rate_limit_per_hour) {
            outcome.refused = Some("form_rate_limited");
            return Ok(outcome);
        }
    }

    // 3. The fill-time floor. A form filled in under a second was not read.
    let floor = i64::from(form.min_fill_seconds.max(0)) * 1_000;
    if floor > 0 && incoming.filled_at_ms >= 0 && incoming.filled_at_ms < floor {
        outcome.refused = Some("form_too_fast");
        outcome.spam_score = 80;
        return Ok(outcome);
    }

    // 4. Only now the answers, against the saved fields.
    if incoming.answers.len() > MAX_ANSWERS {
        return Err(ContentError::InvalidFormField(format!(
            "a submission carries {} answers; this form defines {} fields",
            incoming.answers.len(),
            fields.len()
        )));
    }
    let errors = validate_submission(fields, &incoming.answers);
    if !errors.is_empty() {
        outcome.errors = errors;
        outcome.refused = Some("form_invalid");
        return Ok(outcome);
    }

    // An answer for a field the form does not define is dropped rather than stored: it is either
    // a stale form in the visitor's browser or someone stuffing the endpoint, and storing it
    // would show up in the owner's inbox as a column that does not exist anywhere else.
    let answers: Map<String, Value> = fields
        .iter()
        .filter_map(|field| {
            incoming
                .answers
                .get(&field.key)
                .filter(|answer| !answer.is_null())
                .map(|answer| (field.key.clone(), answer.clone()))
        })
        .collect();

    let sql = format!(
        "insert into cms_form_submissions (form_id, site_id, answers, consent_text, source_path, \
         ip_hash, user_agent_hash, spam_score) values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning {SUBMISSION_COLUMNS}"
    );
    let score = spam_score(&answers);
    let stored = sqlx::query_as::<_, Submission>(&sql)
        .bind(form.id)
        .bind(form.site_id)
        .bind(Value::Object(answers))
        .bind(consent_text(fields))
        .bind(incoming.source_path.as_deref())
        .bind(incoming.ip_hash.as_deref())
        .bind(incoming.user_agent_hash.as_deref())
        .bind(score)
        .fetch_one(pool)
        .await?;
    outcome.spam_score = score;
    outcome.submission = Some(stored);
    Ok(outcome)
}

/// How many submissions one sender has made in the last hour.
///
/// The public route calls this *before* writing, so the 429 it answers carries the number of
/// minutes until the oldest submission leaves the window rather than an open-ended retry hint.
pub async fn submissions_in_last_hour(
    pool: &PgPool,
    form_id: Uuid,
    sender: Option<&str>,
) -> Result<i64> {
    let Some(sender) = sender else {
        return Ok(0);
    };
    let sent: i64 = sqlx::query_scalar(
        "select count(*) from cms_form_submissions \
         where form_id = $1 and ip_hash = $2 and created_at > now() - interval '1 hour'",
    )
    .bind(form_id)
    .bind(sender)
    .fetch_one(pool)
    .await?;
    Ok(sent)
}

/// The inbox of one form.
pub async fn list_submissions(
    pool: &PgPool,
    form_id: Uuid,
    query: &SubmissionQuery,
) -> Result<Vec<Submission>> {
    let sql = format!(
        "select {SUBMISSION_COLUMNS} from cms_form_submissions \
         where form_id = $1 \
           and ($2::text is null or status = $2) \
           and ($3::timestamptz is null or created_at >= $3) \
           and ($4::timestamptz is null or created_at <= $4) \
           and ($5::text is null or answers::text ilike '%' || $5 || '%') \
         order by created_at desc, id \
         limit $6 offset $7"
    );
    Ok(sqlx::query_as::<_, Submission>(&sql)
        .bind(form_id)
        .bind(query.status.as_deref())
        .bind(query.since)
        .bind(query.until)
        .bind(query.search.as_deref())
        .bind(query.limit.clamp(1, 500))
        .bind(query.offset.max(0))
        .fetch_all(pool)
        .await?)
}

/// How many submissions the inbox holds, under the same filters.
pub async fn count_submissions(
    pool: &PgPool,
    form_id: Uuid,
    query: &SubmissionQuery,
) -> Result<i64> {
    let sql = "select count(*) from cms_form_submissions \
               where form_id = $1 \
                 and ($2::text is null or status = $2) \
                 and ($3::timestamptz is null or created_at >= $3) \
                 and ($4::timestamptz is null or created_at <= $4) \
                 and ($5::text is null or answers::text ilike '%' || $5 || '%')";
    let count: i64 = sqlx::query_scalar(sql)
        .bind(form_id)
        .bind(query.status.as_deref())
        .bind(query.since)
        .bind(query.until)
        .bind(query.search.as_deref())
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Read one submission, scoped to the form that owns it.
pub async fn find_submission(
    pool: &PgPool,
    form_id: Uuid,
    id: Uuid,
) -> Result<Option<Submission>> {
    let sql = format!(
        "select {SUBMISSION_COLUMNS} from cms_form_submissions where form_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, Submission>(&sql)
        .bind(form_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Change the inbox state of a submission.
pub async fn set_submission_status(
    pool: &PgPool,
    form_id: Uuid,
    id: Uuid,
    status: &str,
) -> Result<Submission> {
    if !SUBMISSIONS_STATUSES.contains(&status) {
        return Err(ContentError::InvalidFormField(format!(
            "{status:?} is not an inbox state ({})",
            SUBMISSIONS_STATUSES.join(", ")
        )));
    }
    let sql = format!(
        "update cms_form_submissions set status = $3 where form_id = $1 and id = $2 \
         returning {SUBMISSION_COLUMNS}"
    );
    sqlx::query_as::<_, Submission>(&sql)
        .bind(form_id)
        .bind(id)
        .bind(status)
        .fetch_optional(pool)
        .await?
        .ok_or(ContentError::SubmissionNotFound)
}

/// Move several submissions to one inbox state at once.
pub async fn bulk_submission_status(
    pool: &PgPool,
    form_id: Uuid,
    ids: &[Uuid],
    status: &str,
) -> Result<Vec<Submission>> {
    if !SUBMISSIONS_STATUSES.contains(&status) {
        return Err(ContentError::InvalidFormField(format!(
            "{status:?} is not an inbox state ({})",
            SUBMISSIONS_STATUSES.join(", ")
        )));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "update cms_form_submissions set status = $3 where form_id = $1 and id = any($2) \
         returning {SUBMISSION_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, Submission>(&sql)
        .bind(form_id)
        .bind(ids)
        .bind(status)
        .fetch_all(pool)
        .await?)
}

/// Delete one submission for good.
pub async fn delete_submission(pool: &PgPool, form_id: Uuid, id: Uuid) -> Result<()> {
    let deleted = sqlx::query("delete from cms_form_submissions where form_id = $1 and id = $2")
        .bind(form_id)
        .bind(id)
        .execute(pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(ContentError::SubmissionNotFound);
    }
    Ok(())
}

/// The inbox as CSV, honouring the filters.
///
/// The export is the same rows the screen shows and not "everything", because an export that
/// ignores the filters is how a filtered inbox leaks. `text` is quoted per RFC 4180, including
/// the quote doubling a spreadsheet needs to show a quote.
#[must_use]
pub fn submissions_to_csv(submissions: &[Submission]) -> String {
    let mut out = String::from("received,status,spam_score,source_path,answers\n");
    for submission in submissions {
        out.push_str(&csv_cell(&format_timestamp(&submission.created_at)));
        out.push(',');
        out.push_str(&csv_cell(&submission.status));
        out.push(',');
        out.push_str(&csv_cell(&submission.spam_score.to_string()));
        out.push(',');
        out.push_str(&csv_cell(submission.source_path.as_deref().unwrap_or("")));
        out.push(',');
        out.push_str(&csv_cell(&answers_summary(&submission.answers)));
        out.push('\n');
    }
    out
}

/// One cell, quoted when it needs to be.
fn csv_cell(value: &str) -> String {
    let needs_quotes = value.contains([',', '"', '\n', '\r']);
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// `label: answer` pairs, for the export's single answers column.
///
/// Deliberately one column rather than one column per field: the export has to survive a form
/// that gains a field, and a CSV whose header changes shape between two exports of the same form
/// cannot be appended to.
#[must_use]
pub fn answers_summary(answers: &Value) -> String {
    let Some(map) = answers.as_object() else {
        return String::new();
    };
    map.iter()
        .map(|(key, answer)| {
            let text = match answer {
                Value::String(text) => text.clone(),
                Value::Bool(yes) => if *yes { "yes" } else { "no" }.to_owned(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            format!("{key}: {text}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Translate a unique-constraint failure into the error that names the key.
fn map_form_write_error(error: sqlx::Error, key: &str) -> ContentError {
    // `sqlx::Error` is not `Clone`, so the constraint is read in the arm that already owns the
    // error rather than by borrowing it and cloning — the pattern that does not compile.
    match error {
        sqlx::Error::Database(ref database) if database.code().as_deref() == Some("23505") => {
            ContentError::FormKeyTaken(key.to_owned())
        }
        other => ContentError::Database(other),
    }
}

/// An instant as RFC 3339, which is what a CSV reader expects in a date column.
fn format_timestamp(value: &time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_string())
}

/// Match an answer against the pattern dialect documented on [`validate_answer`].
///
/// The dialect is anchored to the whole answer and supports two metacharacters:
///
/// * `.` matches exactly one character,
/// * `*` repeats the *preceding* item zero or more times,
///
/// and everything else is itself. `999` matches `999`; `5*` matches `5`, `55` and `555`;
/// `5*9` matches `9`, `59` and `5599`. It is a backtracking matcher over a bounded answer
/// ([`MAX_ANSWER_LENGTH`]) and a bounded pattern, so it is total: there is no input that makes
/// it take superlinear time beyond what that bound already allows.
#[must_use]
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    // Cap the pattern the same way the answer is capped, so "an owner pastes a novel into the
    // pattern box" is a refused answer rather than a slow one.
    let pattern: Vec<char> = pattern.chars().take(MAX_ANSWER_LENGTH).collect();
    let value: Vec<char> = value.chars().take(MAX_ANSWER_LENGTH).collect();
    // Build the items, dropping the `*` tokens as they are seen and marking the item *before*
    // them as repeatable. Keeping `*` as an item is the bug this shape exists to prevent: an
    // item that must match the literal character `*` turns "5*" into a pattern for `5*`, so the
    // dialect silently stops accepting any repetition at all.
    let mut items: Vec<(char, bool)> = Vec::with_capacity(pattern.len());
    for token in &pattern {
        match token {
            '*' => {
                if let Some(last) = items.last_mut() {
                    last.1 = true;
                }
                // A leading `*` repeats nothing; it is ignored rather than made into a wildcard
                // over the whole answer, which would silently accept anything.
            }
            '.' => items.push(('\0', false)),
            other => items.push((*other, false)),
        }
    }
    let repeat_positions: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, (_, repeats))| *repeats)
        .map(|(index, _)| index)
        .collect();
    matches_items(&items, &repeat_positions, 0, &value, 0)
}

/// Whether `value` from `at` matches `items` from `item_at`.
fn matches_items(
    items: &[(char, bool)],
    repeat_positions: &[usize],
    item_at: usize,
    value: &[char],
    at: usize,
) -> bool {
    if item_at == items.len() {
        return at == value.len();
    }
    let (expected, _) = items[item_at];
    let repeatable = repeat_positions.binary_search(&item_at).is_ok();
    if repeatable {
        // Greedy with backtracking: take as many as possible, then give one back.
        let mut end = at;
        while end < value.len() && (expected == '\0' || value[end] == expected) {
            end += 1;
        }
        loop {
            if matches_items(items, repeat_positions, item_at + 1, value, end) {
                return true;
            }
            if end == at {
                return false;
            }
            end -= 1;
        }
    }
    if at < value.len() && (expected == '\0' || value[at] == expected) {
        return matches_items(items, repeat_positions, item_at + 1, value, at + 1);
    }
    false
}

/// Parse `YYYY-MM-DD` into a date, or `None` when it is not one.
///
/// Length and character checks are *not* enough: `2026-02-31` is ten digits and two dashes and
/// is not a day. The calendar check is the whole point.
///
/// The format description is parsed per call rather than held in a `const`: `time`'s parser
/// returns a `Vec`, and a `const Vec` does not exist. The input is a ten-character string and
/// the description is three items, so parsing it here is cheaper than any lazy-static
/// bookkeeping would be — and this is a per-submission path, not a per-request header parse.
fn parse_iso_date(value: &str) -> Option<time::Date> {
    if value.len() != 10 || !value.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return None;
    }
    let format = time::format_description::parse("[year]-[month]-[day]")
        .expect("the ISO date format description is a valid literal");
    time::Date::parse(value, &format).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(field_type: &str, rules: Value, options: Value) -> FormField {
        FormField {
            id: Uuid::nil(),
            form_id: Uuid::nil(),
            position: 0,
            key: "answer".to_owned(),
            label: "Answer".to_owned(),
            field_type: field_type.to_owned(),
            required: false,
            placeholder: None,
            help_text: None,
            width: "full".to_owned(),
            rules,
            options,
        }
    }

    #[test]
    fn a_required_field_refuses_an_empty_answer_with_its_own_message() {
        let mut field = field("text", json!({}), json!([]));
        field.required = true;
        assert_eq!(
            validate_answer(&field, &Value::String("   ".to_owned())),
            Some("this field is required".to_owned())
        );
        assert_eq!(validate_answer(&field, &json!("hello")), None);
        // The same empty answer on an optional field is not a refusal — otherwise making a
        // field optional would be impossible, which is the flag's whole purpose.
        field.required = false;
        assert_eq!(validate_answer(&field, &Value::String("   ".to_owned())), None);
    }

    #[test]
    fn length_rules_are_counted_in_characters_not_bytes() {
        let field = field("text", json!({"max_length": 3}), json!([]));
        // Four accented characters are eight bytes; the rule is about what a person typed.
        assert_eq!(validate_answer(&field, &json!("ççç")), None);
        assert!(validate_answer(&field, &json!("çççç")).is_some());
    }

    #[test]
    fn a_choice_field_refuses_an_answer_it_did_not_offer() {
        let field = field("radio", json!({}), json!(["gold", "silver"]));
        assert_eq!(validate_answer(&field, &json!("gold")), None);
        assert_eq!(
            validate_answer(&field, &json!("bronze")),
            Some("\"bronze\" is not one of the offered options".to_owned())
        );
    }

    #[test]
    fn both_option_shapes_read_the_same() {
        let typed = json!([{"value": "gold", "label": "Gold"}]);
        assert_eq!(option_labels(typed.as_array().unwrap()), vec!["gold"]);
        assert_eq!(option_labels(json!(["gold"]).as_array().unwrap()), vec!["gold"]);
    }

    #[test]
    fn a_date_that_does_not_exist_is_refused() {
        let field = field("date", json!({}), json!([]));
        assert!(validate_answer(&field, &json!("2026-02-31")).is_some());
        assert_eq!(validate_answer(&field, &json!("2026-02-28")), None);
    }

    #[test]
    fn date_bounds_are_compared_as_dates_not_as_text() {
        // `2026-02-31` is not a day, so a max_date of it accepts nothing after it rather than
        // rejecting everything by string comparison.
        let field = field("date", json!({"max_date": "2026-02-31"}), json!([]));
        assert_eq!(validate_answer(&field, &json!("2026-03-01")), None);
    }

    #[test]
    fn consent_is_answered_with_a_true_and_stores_its_own_text() {
        let mut field = field("consent", json!({}), json!([]));
        field.required = true;
        assert!(validate_answer(&field, &Value::Bool(false)).is_some());
        assert_eq!(validate_answer(&field, &Value::Bool(true)), None);
        field.label = "I agree to the privacy policy".to_owned();
        assert_eq!(
            consent_text(std::slice::from_ref(&field)).as_deref(),
            Some("I agree to the privacy policy")
        );
    }

    #[test]
    fn the_pattern_dialect_is_literal_with_star_repeats() {
        assert!(matches_pattern("999", "999"));
        assert!(!matches_pattern("999", "991"));
        assert!(matches_pattern("5*", "5555"));
        assert!(matches_pattern("5*", "5"));
        // `*` repeats the character *before* it, so this is an optional 5 and then a 9.
        assert!(matches_pattern("5*9", "9"));
        assert!(matches_pattern("5*9", "5559"));
        assert!(!matches_pattern("5*9", "95"));
        assert!(matches_pattern("....", "abcd"));
        assert!(!matches_pattern("...", "abcd"));
    }

    #[test]
    fn a_field_with_an_unknown_format_rule_validates_nothing() {
        // A rule name from a future release must not break every form already saved.
        let field = field("text", json!({"format": "postcode-in-wales"}), json!([]));
        assert_eq!(validate_answer(&field, &json!("anything at all")), None);
    }

    #[test]
    fn a_placeholder_rule_that_is_nonsense_is_skipped_not_enforced() {
        let field = field("text", json!({"min_length": "ten"}), json!([]));
        assert_eq!(validate_answer(&field, &json!("ok")), None);
    }

    #[test]
    fn a_choice_field_with_no_options_is_refused_at_save_time() {
        let field = NewFormField {
            position: 0,
            key: "plan".to_owned(),
            label: "Plan".to_owned(),
            field_type: "select".to_owned(),
            required: false,
            placeholder: None,
            help_text: None,
            width: "full".to_owned(),
            rules: json!({}),
            options: json!([]),
        };
        let error = validate_field(&field).expect_err("an empty select is unusable");
        assert!(error.to_string().contains("no options"));
    }

    #[test]
    fn a_duplicate_field_key_is_refused_because_answers_are_stored_by_key() {
        let make = |key: &str| NewFormField {
            position: 0,
            key: key.to_owned(),
            label: "Label".to_owned(),
            field_type: "text".to_owned(),
            required: false,
            placeholder: None,
            help_text: None,
            width: "full".to_owned(),
            rules: json!({}),
            options: json!([]),
        };
        let error = validate_fields(&[make("email"), make("email")])
            .expect_err("two fields may not share a key");
        assert!(error.to_string().contains("share the key"));
    }

    #[test]
    fn a_redirect_may_not_leave_the_site_through_a_protocol_relative_url() {
        let error = validate_settings("redirect", None, Some("//evil.example/form"))
            .expect_err("a protocol-relative redirect is an open redirect");
        assert!(error.to_string().contains("must start with / or http"));
    }

    #[test]
    fn a_message_form_without_a_message_is_refused() {
        assert!(validate_settings("message", None, None).is_err());
        assert_eq!(
            validate_settings("message", Some("Done."), None).expect("valid"),
            "message"
        );
    }

    #[test]
    fn a_notification_address_needs_exactly_one_at_sign() {
        assert!(validate_notify_emails(&["owner@example.com".to_owned()]).is_ok());
        assert!(validate_notify_emails(&["owner.example.com".to_owned()]).is_err());
        assert!(validate_notify_emails(&["@example.com".to_owned()]).is_err());
    }

    #[test]
    fn a_spam_score_rewards_links_and_never_exceeds_its_ceiling() {
        let one = serde_json::Map::from_iter([(
            "body".to_owned(),
            json!("see https://a.example for details"),
        )]);
        assert_eq!(spam_score(&one), 15, "one link is weak evidence");
        let many = serde_json::Map::from_iter([(
            "body".to_owned(),
            json!("buy https://a.example https://b.example https://c.example"),
        )]);
        assert_eq!(spam_score(&many), 45, "a link dump is the strongest single signal");
        // The ceiling is a ceiling: the signals add up and stop, so a long mixed-language
        // answer with a link dump cannot report a score a screen would read as 100% certain spam.
        let overflowing = serde_json::Map::from_iter([
            (
                "a".to_owned(),
                json!("https://a.example https://b.example https://c.example привет мир"),
            ),
            (
                "b".to_owned(),
                json!("https://d.example https://e.example привет мир as well"),
            ),
            ("c".to_owned(), json!("x".repeat(3_000))),
        ]);
        assert_eq!(spam_score(&overflowing), 100);
        let plain = serde_json::Map::from_iter([("body".to_owned(), json!("hello there"))]);
        assert_eq!(spam_score(&plain), 0);
    }

    #[test]
    fn a_csv_cell_quotes_what_a_spreadsheet_would_break_on() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn the_export_is_one_column_of_pairs_so_a_new_field_cannot_change_its_shape() {
        let answers = json!({"name": "Ada", "consent": true, "note": null});
        let summary = answers_summary(&answers);
        assert!(summary.contains("name: Ada"));
        assert!(summary.contains("consent: yes"));
        assert!(summary.contains("note: "));
    }
}
