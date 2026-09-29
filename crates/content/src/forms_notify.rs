//! Rendering the notification a form sends when a submission lands — REQ-064 slice 2.
//!
//! The builder stores WHO to notify and a subject template, and the submit route owes the
//! owner a message. Everything that decides *what the words are* lives here, away from both
//! the SMTP client and the route, because those are the two places a mistake is invisible: a
//! template bug shows up as an empty inbox and a transport bug shows up as an empty template.
//!
//! Three rules, each of them a decision rather than a default:
//!
//! * **The answers are rendered as TEXT, never as HTML.** A visitor types `<script>` into a
//!   message field and the recipient opens it in a mail client that renders HTML. The whole
//!   body is therefore built by quoting every value, and a value that contains a newline is
//!   quoted visibly so one answer cannot masquerade as a second field.
//!
//! * **A form with no recipients sends nothing, and says so.** A send that silently does
//!   nothing is indistinguishable from a send that failed, and the owner's conclusion in both
//!   cases is "the form is broken".
//!
//! * **The placeholders are the ones the builder documents** — `{{form_name}}` and
//!   `{{submitted_at}}` — and an unknown placeholder is left alone rather than emptied,
//!   because a subject reading `{{auther}}` is visible and fixable while a subject reading
//!   nothing at all is not.

use serde_json::Value;

use crate::forms::{Form, FormField, Submission};

/// The notification a submission produced, ready to hand to a mail transport.
///
/// `to` is empty when the form names no recipients, and that is the caller's cue: it means
/// "there is nothing to send", not "send it to nobody".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormNotification {
    /// Recipients, in the order the builder listed them.
    pub to: Vec<String>,
    /// Rendered subject line.
    pub subject: String,
    /// Rendered plain-text body.
    pub body: String,
}

/// The fallback subject when the builder did not write one.
pub const DEFAULT_SUBJECT: &str = "New submission on {{form_name}}";

/// Build the notification for a stored submission.
///
/// `fields` is the form's own field list in builder order, and it is what turns a bag of
/// answers into a readable message: the answers object is keyed by field key, and a key is
/// not something a person can read. An answer whose field is gone is still printed, under
/// its key — dropping it would silently lose a message somebody took the trouble to type.
#[must_use]
pub fn render(form: &Form, fields: &[FormField], submission: &Submission) -> FormNotification {
    let submitted_at = submission.created_at.date().to_string(); // ISO-8601 date; the exact minute is in the body, not the subject.
    // The default is a TEMPLATE too, so it goes through the renderer like any other one.
    // Choosing it here and rendering below is the order that matters: a fallback picked
    // after the substitution would arrive with its own `{{form_name}}` still in it, and the
    // first message an owner ever receives from their own contact form would greet them in
    // template syntax. The unit test that caught it is
    // `an_empty_subject_template_falls_back_to_the_default`.
    let subject = render_subject(
        form.notify_subject
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_SUBJECT),
        &form.name,
        &submitted_at,
    );

    let mut body = String::new();
    push_line(&mut body, &format!("Form: {}", form.name));
    // RFC-2822 is the format a mail client itself will render, so a forwarded copy reads the
    // same here as it does in the recipient's client. A timestamp the formatter cannot print
    // falls back to the date rather than failing the whole notification over one field.
    let received = submission
        .created_at
        .format(&time::format_description::well_known::Rfc2822)
        .unwrap_or_else(|_| submitted_at.clone());
    push_line(&mut body, &format!("Received: {received}"));
    if let Some(path) = submission
        .source_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        push_line(&mut body, &format!("Page: {path}"));
    }
    if let Some(consent) = submission
        .consent_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        // The consent TEXT, not a "yes": a row that says "agreed" cannot answer "agreed to
        // what" a year later, and this is the copy the site actually showed.
        push_line(&mut body, "Consent:");
        push_line(&mut body, consent);
    }
    if submission.spam_score > 0 {
        // The heuristic's own score travels with the message. An owner who reads the mail
        // first and the inbox second must still learn that this one looked like spam.
        push_line(&mut body, &format!("Spam score: {}", submission.spam_score));
    }
    body.push_str("\nAnswers:\n");

    let rendered = render_answers(fields, &submission.answers);
    if rendered.is_empty() {
        body.push_str("  (none recorded)\n");
    } else {
        body.push_str(&rendered);
    }

    FormNotification {
        to: form.notify_emails.clone(),
        subject,
        body,
    }
}

/// Render the subject template. Unknown placeholders are left verbatim.
fn render_subject(template: &str, form_name: &str, submitted_at: &str) -> String {
    template
        .replace("{{form_name}}", form_name.trim())
        .replace("{{submitted_at}}", submitted_at)
        .trim()
        .to_owned()
}

/// Every answer, in builder order, one per line, values quoted so they cannot fake a field.
fn render_answers(fields: &[FormField], answers: &Value) -> String {
    let mut out = String::new();
    let mut written: Vec<&str> = Vec::new();

    for field in fields {
        let Some(answer) = answers.get(&field.key) else {
            continue;
        };
        let text = answer_text(answer);
        if text.is_empty() {
            continue;
        }
        push_line(&mut out, &format!("  {}: {}", field.label, quote(&text)));
        written.push(field.key.as_str());
    }

    // An answer whose field was deleted is still somebody's message. Print it under its key
    // so nothing a visitor typed disappears from the notification.
    if let Some(map) = answers.as_object() {
        for (key, value) in map {
            if written.contains(&key.as_str()) {
                continue;
            }
            let text = answer_text(value);
            if text.is_empty() {
                continue;
            }
            push_line(&mut out, &format!("  {key}: {}", quote(&text)));
        }
    }

    out
}

/// Flatten one answer into text. A boolean answers as "yes"/"no" rather than as its JSON.
fn answer_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.trim().to_owned(),
        Value::Bool(flag) => if *flag { "yes" } else { "no" }.to_owned(),
        other => other.to_string(),
    }
}

/// Quote a value that claims to be more than one line, so one answer cannot be read as a
/// second field. Newlines inside an answer are legal and common (a message textarea).
fn quote(value: &str) -> String {
    if value.contains('\n') {
        format!("\"{}\"", value.replace('\r', ""))
    } else {
        value.to_owned()
    }
}

/// Append one line, with the newline the format needs and none of the ones it does not.
fn push_line(buffer: &mut String, line: &str) {
    buffer.push_str(line);
    buffer.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;

    use crate::forms::{Form, FormField, Submission};

    /// The store's own types are not the interesting part here; a fixture is.
    fn form() -> Form {
        Form {
            id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            key: "contact".to_owned(),
            name: "Contact form".to_owned(),
            status: "published".to_owned(),
            submit_action: "message".to_owned(),
            submit_message: Some("Thanks".to_owned()),
            redirect_url: None,
            notify_emails: vec!["owner@example.com".to_owned()],
            notify_subject: None,
            honeypot: true,
            min_fill_seconds: 3,
            rate_limit_per_hour: 5,
            retention_days: 90,
            target_segment_id: None,
            created_by: None,
            created_at: datetime!(2026-09-29 10:00 UTC),
            updated_at: datetime!(2026-09-29 10:00 UTC),
        }
    }

    fn field(key: &str, label: &str) -> FormField {
        FormField {
            id: Uuid::new_v4(),
            form_id: Uuid::new_v4(),
            position: 0,
            key: key.to_owned(),
            label: label.to_owned(),
            field_type: "text".to_owned(),
            required: false,
            placeholder: None,
            help_text: None,
            width: "full".to_owned(),
            rules: Value::Null,
            options: Value::Array(Vec::new()),
        }
    }

    fn submission(answers: Value) -> Submission {
        Submission {
            id: Uuid::new_v4(),
            form_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            answers,
            consent_text: Some("I agree to be contacted.".to_owned()),
            source_path: Some("/contact".to_owned()),
            ip_hash: Some("abc".to_owned()),
            user_agent_hash: None,
            spam_score: 0,
            status: "new".to_owned(),
            created_at: datetime!(2026-09-29 12:30 UTC),
        }
    }

    #[test]
    fn the_default_subject_names_the_form() {
        let note = render(&form(), &[], &submission(json!({})));
        assert_eq!(note.subject, "New submission on Contact form");
        assert_eq!(note.to, vec!["owner@example.com".to_owned()]);
    }

    #[test]
    fn the_subject_template_fills_both_documented_placeholders() {
        let mut f = form();
        f.notify_subject = Some("{{form_name}} — {{submitted_at}}".to_owned());
        let note = render(&f, &[], &submission(json!({})));
        assert_eq!(note.subject, "Contact form — 2026-09-29");
    }

    #[test]
    fn an_unknown_placeholder_is_left_alone_rather_than_emptied() {
        let mut f = form();
        f.notify_subject = Some("{{auther}} asked".to_owned());
        let note = render(&f, &[], &submission(json!({})));
        // A subject reading "{{auther}}" is visible and fixable; one reading "" is not.
        assert_eq!(note.subject, "{{auther}} asked");
    }

    #[test]
    fn an_empty_subject_template_falls_back_to_the_default() {
        let mut f = form();
        f.notify_subject = Some("   ".to_owned());
        // Asserted against the RENDERED default, not against the template constant: the whole
        // point of the fix is that a form with no subject of its own greets the owner with the
        // form's name rather than with the placeholder it was written in.
        assert_eq!(
            render(&f, &[], &submission(json!({}))).subject,
            "New submission on Contact form"
        );
        assert!(
            DEFAULT_SUBJECT.contains("{{form_name}}"),
            "the default is a template too"
        );
    }

    #[test]
    fn answers_are_labelled_in_builder_order_not_in_json_order() {
        let f = form();
        let fields = vec![field("email", "E-mail"), field("message", "Message")];
        let note = render(
            &f,
            &fields,
            &submission(json!({ "message": "Hello", "email": "a@example.com" })),
        );
        let email_at = note
            .body
            .find("E-mail: a@example.com")
            .expect("e-mail line");
        let message_at = note.body.find("Message: Hello").expect("message line");
        assert!(
            email_at < message_at,
            "builder order must win over JSON order"
        );
    }

    #[test]
    fn a_missing_answer_leaves_its_field_out_entirely() {
        let f = form();
        let fields = vec![field("email", "E-mail"), field("phone", "Phone")];
        let note = render(
            &f,
            &fields,
            &submission(json!({ "email": "a@example.com" })),
        );
        assert!(note.body.contains("E-mail: a@example.com"));
        assert!(
            !note.body.contains("Phone:"),
            "an unanswered field is not a blank line"
        );
    }

    #[test]
    fn a_multi_line_answer_is_quoted_so_it_cannot_fake_a_field() {
        let f = form();
        let fields = vec![field("message", "Message")];
        let note = render(
            &f,
            &fields,
            &submission(json!({ "message": "first line\n  Subject: hijacked" })),
        );
        assert!(
            note.body
                .contains("Message: \"first line\n  Subject: hijacked\""),
            "a newline inside an answer must be inside quotes, got:\n{}",
            note.body
        );
    }

    #[test]
    fn an_answer_whose_field_was_deleted_is_still_printed_under_its_key() {
        let f = form();
        let note = render(
            &f,
            &[field("email", "E-mail")],
            &submission(json!({
                "email": "a@example.com",
                "removed_field": "typed before the field was deleted",
            })),
        );
        assert!(
            note.body
                .contains("removed_field: typed before the field was deleted"),
            "nothing a visitor typed may vanish from the notification:\n{}",
            note.body
        );
    }

    #[test]
    fn a_submission_with_no_answers_says_so_rather_than_sending_a_bare_header() {
        let note = render(&form(), &[field("email", "E-mail")], &submission(json!({})));
        assert!(note.body.contains("(none recorded)"));
    }

    #[test]
    fn the_consent_text_travels_with_the_message() {
        let note = render(&form(), &[], &submission(json!({})));
        assert!(note.body.contains("I agree to be contacted."));
    }

    #[test]
    fn a_spam_score_travels_with_the_message() {
        let mut s = submission(json!({}));
        s.spam_score = 40;
        let note = render(&form(), &[], &s);
        assert!(note.body.contains("Spam score: 40"));
    }

    #[test]
    fn a_form_with_no_recipients_produces_an_empty_to_list() {
        let mut f = form();
        f.notify_emails = Vec::new();
        let note = render(&f, &[], &submission(json!({})));
        assert!(
            note.to.is_empty(),
            "empty recipients is the cue for 'do not send'"
        );
        assert!(!note.subject.is_empty());
    }

    #[test]
    fn a_boolean_answer_reads_as_yes_and_no() {
        let f = form();
        let fields = vec![field("consent", "Consent")];
        let note = render(
            &f,
            &fields,
            &submission(json!({ "consent": true, "other": false })),
        );
        assert!(note.body.contains("Consent: yes"));
        assert!(note.body.contains("other: no"));
    }
}
