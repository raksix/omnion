//! The autoresponder: the one message the visitor gets back, and the rules that decide it.
//!
//! Three properties make this worth a pure module rather than a few lines in the route:
//!
//! * **Once per lead, whatever the retry count.** A keyed endpoint is called by servers that
//!   retry, a form post is re-sent by browsers, and `capture` is idempotent on the submission
//!   id — so "did we already write a lead for this submission?" is not the same question as
//!   "have we already answered this person?". The send is claimed with a conditional insert
//!   that *loses* the race rather than performing a check and then a write.
//! * **Never an operator's free text.** The REQ's own sentence is the rule: content comes
//!   from a template with the submitter's name and the source's details. An operator typing
//!   the body into the request field is how a lead list turns into a mail merge with no
//!   review, so [`Autoresponder::from_json`] ignores a `body` key that arrives and the
//!   template is the only source of prose.
//! * **A rejected or spam submission answers nothing.** Sending "we got your message" to a
//!   spammer confirms the address works, which is the single most valuable thing a spam
//!   filter can be taught to do for an attacker.
//!
//! The rendering is a small, forgiving `{{name}}` substitution: a template that references a
//! field a mapping never produces renders an empty string rather than refusing to send, since
//! a person waiting for a reply is worse off than a person reading a slightly bare line.

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

/// The delivery verdict, and what it says — never whether a contact was matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The message is ready to hand to the mailer.
    Ready(Message),
    /// No autoresponder is configured on this source.
    Disabled,
    /// The submission was not accepted: spam, rejected, or a duplicate that was not linked.
    NoRecipient(&'static str),
    /// The source has an autoresponder but the lead has no address to send it to.
    NoAddress,
    /// The configured delay has not elapsed yet.
    NotYet(OffsetDateTime),
    /// This lead has already been answered.
    AlreadySent,
    /// The template is unusable (a body that renders to nothing, or no subject).
    InvalidTemplate(String),
}

impl Delivery {
    /// `true` when a message is ready to send. The caller treats every other variant as
    /// "nothing went out", and records the reason — the trail line is the difference between
    /// "we did not mail them" and "we do not know whether we mailed them".
    #[must_use]
    pub fn is_sendable(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The trail line's `reason`, one word per variant so the timeline is scannable.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Ready(_) => "sent",
            Self::Disabled => "not_configured",
            Self::NoRecipient(_) => "not_accepted",
            Self::NoAddress => "no_address",
            Self::NotYet(_) => "delayed",
            Self::AlreadySent => "already_sent",
            Self::InvalidTemplate(_) => "invalid_template",
        }
    }
}

/// The message, fully rendered and ready for the mailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Recipient — the submitter, never an owner.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// Which template the operator chose, for the trail.
    pub template: String,
    /// Whether the send is immediate or waits out the configured delay.
    pub delayed: bool,
    /// When a delayed message becomes due.
    pub due_at: Option<OffsetDateTime>,
}

/// The autoresponder a source carries, parsed out of its `autoresponder` column.
///
/// The column is `jsonb` with no shape of its own — a source written by hand, or by an
/// earlier version of this code, may hold any of it. Every field is therefore optional and
/// every parse failure is a *disabled* autoresponder rather than a rejected request: a
/// malformed column must not make `capture` answer `500` to a visitor's submission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Autoresponder {
    /// Whether the operator switched it on.
    pub enabled: bool,
    /// Template name, echoed onto the trail.
    pub template: String,
    /// Subject template.
    pub subject: String,
    /// Body template.
    pub body: String,
    /// Minutes to wait before sending, so the reply lands after the salesperson's.
    pub delay_minutes: i32,
}

impl Autoresponder {
    /// The longest delay accepted, in minutes (a week).
    pub const MAX_DELAY_MINUTES: i32 = 10_080;

    /// Read a source's column.
    ///
    /// `null`, an absent key, or a non-object are all "no autoresponder". A negative or
    /// absurd delay is clamped rather than refused: the send is a courtesy, and a source that
    /// asks for a nine-hour delay should still answer its visitor.
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        let delay = object
            .get("delay_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .clamp(0, i64::from(Self::MAX_DELAY_MINUTES)) as i32;
        Self {
            enabled: object
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            template: text(object.get("template")),
            subject: text(object.get("subject")),
            // A `body` key is read for nothing on purpose: the prose is the template's, and
            // an operator-supplied body would turn this into an unreviewed mail merge.
            body: text(object.get("template_body")),
            delay_minutes: delay,
        }
    }

    /// `true` when there is something to send at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.subject.trim().is_empty() && !self.body.trim().is_empty()
    }

    /// Decide what this lead gets, if anything.
    ///
    /// `already_sent` is the caller's record of a prior send for this lead — the store reads
    /// it from the trail rather than keeping a second truth about it.
    #[must_use]
    pub fn deliver(
        &self,
        context: &Recipient<'_>,
        now: OffsetDateTime,
        already_sent: bool,
    ) -> Delivery {
        // A switched-on autoresponder with nothing to say is a *misconfiguration*, and it is
        // reported as one: folding it into `Disabled` would tell the operator their autoresponder
        // is off when it is on, empty and silently eating every lead's single reply.
        if !self.enabled {
            return Delivery::Disabled;
        }
        if self.subject.trim().is_empty() || self.body.trim().is_empty() {
            return Delivery::InvalidTemplate("the autoresponder has no subject or no body".into());
        }
        // The order matters: a lead that was never accepted answers nothing *and* has no
        // address, and the trail should say why it stayed silent in the visitor's terms.
        if !context.accepted {
            return Delivery::NoRecipient(context.reason);
        }
        let Some(address) = context.address else {
            return Delivery::NoAddress;
        };
        if already_sent {
            return Delivery::AlreadySent;
        }

        let subject = render(&self.subject, context);
        let body = render(&self.body, context);
        if body.trim().is_empty() {
            return Delivery::InvalidTemplate("the template renders to nothing".to_string());
        }
        let due_at = if self.delay_minutes > 0 {
            Some(now + time::Duration::minutes(i64::from(self.delay_minutes)))
        } else {
            None
        };
        Delivery::Ready(Message {
            to: address.to_string(),
            subject: if subject.trim().is_empty() {
                "We received your message".to_string()
            } else {
                subject
            },
            body,
            template: if self.template.trim().is_empty() {
                "custom".to_string()
            } else {
                self.template.clone()
            },
            delayed: due_at.is_some(),
            due_at,
        })
    }
}

/// Everything the template may say, and the address it goes to.
///
/// Passed by reference so a long-lived template render never copies the submitter's message
/// into a struct that outlives the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipient<'a> {
    /// The submitter's address, when the mapping produced one.
    pub address: Option<&'a str>,
    /// The submitter's first name, for the greeting.
    pub first_name: &'a str,
    /// The source's name, so the reply names where it came from.
    pub source_name: &'a str,
    /// What they asked about.
    pub product_interest: &'a str,
    /// Whether the submission was accepted: the gate on sending anything at all.
    pub accepted: bool,
    /// Why it was not, when it was not.
    pub reason: &'static str,
}

impl Default for Recipient<'_> {
    fn default() -> Self {
        Self {
            address: None,
            first_name: "",
            source_name: "",
            product_interest: "",
            accepted: false,
            reason: "rejected",
        }
    }
}

/// Substitute `{{field}}` placeholders.
///
/// Unknown placeholders render empty rather than surviving into the body: a message reading
/// "Hello {{fnam}," is a bug report waiting to happen, and a blank line is not. A template
/// with no placeholders is returned unchanged.
fn render(template: &str, recipient: &Recipient<'_>) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let greeting = if recipient.first_name.trim().is_empty() {
        "Hello".to_string()
    } else {
        format!("Hello {}", recipient.first_name.trim())
    };
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim().to_ascii_lowercase();
        out.push_str(match key.as_str() {
            "name" | "first_name" | "firstname" => &greeting,
            "source" | "source_name" => recipient.source_name.trim(),
            "product" | "product_interest" => recipient.product_interest.trim(),
            _ => "",
        });
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// The default templates an operator picks from.
///
/// They are here rather than in the admin app because the REQ says the autoresponder is
/// "content from a template", and a template that lives in the front end is a template the
/// server cannot honour — the send path would have no way to prove which one was used.
pub const TEMPLATES: &[(&str, &str, &str)] = &[
    (
        "acknowledgement",
        "We received your message",
        "Hello,\n\nthank you for writing to {{source}}. We have your message and somebody will answer it.\n\nIf you asked about {{product}}, it is worth knowing that a person reads every one of these.\n",
    ),
    (
        "quote_received",
        "Your request for a quote",
        "Hello,\n\nyour request for a quote arrived and is with our team. You will hear back shortly.\n\nNothing else is needed from you at this point.\n",
    ),
    (
        "out_of_hours",
        "We have your message",
        "Hello,\n\n{{source}} is closed at the moment. Your message has been recorded and will be answered when we open.\n",
    ),
];

/// The template names a source editor may offer.
#[must_use]
pub fn template_names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(name, _, _)| *name).collect()
}

/// A template by name, or the acknowledgement when the name is unknown.
#[must_use]
pub fn template(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    TEMPLATES.iter().find(|(key, _, _)| *key == name).copied()
}

/// `Date` header, formatted per RFC 2822 — the same shape `automation::mail` writes.
#[must_use]
pub fn date_header(instant: OffsetDateTime) -> String {
    instant
        .format(&Rfc2822)
        .unwrap_or_else(|_| instant.to_string())
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    /// A fixed test instant, so a test never reads the wall clock.
    const MORNING: OffsetDateTime = datetime!(2026-09-29 10:00 UTC);

    fn accepted() -> Recipient<'static> {
        Recipient {
            address: Some("visitor@example.com"),
            first_name: "Ada",
            source_name: "Contact us",
            product_interest: "a warehouse licence",
            accepted: true,
            reason: "rejected",
        }
    }

    #[test]
    fn an_absent_column_is_a_disabled_autoresponder() {
        let none = Autoresponder::from_json(&Value::Null);
        assert!(!none.is_configured());
        assert_eq!(
            none.deliver(&accepted(), MORNING, false),
            Delivery::Disabled
        );

        // A source whose column holds an array rather than an object is not a crash.
        let odd = Autoresponder::from_json(&json!([1, 2, 3]));
        assert!(!odd.is_configured());
    }

    #[test]
    fn a_configured_autoresponder_renders_the_submitter_by_name() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true,
            "template": "acknowledgement",
            "subject": "We received your message",
            "template_body": "Hello,\n\nthanks for writing to {{source}} about {{product}}.\n",
        }));
        assert!(ar.is_configured());

        let Delivery::Ready(message) = ar.deliver(&accepted(), MORNING, false) else {
            panic!("a configured autoresponder on an accepted lead must send");
        };
        assert_eq!(message.to, "visitor@example.com");
        assert_eq!(message.template, "acknowledgement");
        assert!(message.body.contains("Contact us"), "{}", message.body);
        assert!(
            message.body.contains("a warehouse licence"),
            "{}",
            message.body
        );
        assert!(!message.delayed);
    }

    #[test]
    fn a_rejected_submission_is_never_answered() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "thanks",
        }));
        let mut recipient = accepted();
        recipient.accepted = false;
        recipient.reason = "spam";

        assert_eq!(
            ar.deliver(&recipient, MORNING, false),
            Delivery::NoRecipient("spam")
        );
    }

    #[test]
    fn a_lead_with_no_address_is_not_reached_by_the_sender() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "thanks",
        }));
        let mut recipient = accepted();
        recipient.address = None;

        assert_eq!(ar.deliver(&recipient, MORNING, false), Delivery::NoAddress);
    }

    #[test]
    fn a_second_call_for_one_lead_says_already_sent() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "thanks",
        }));
        assert_eq!(
            ar.deliver(&accepted(), MORNING, true),
            Delivery::AlreadySent
        );
    }

    #[test]
    fn a_delay_makes_the_message_due_later_rather_than_lost() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "thanks",
            "delay_minutes": 30,
        }));
        let now = MORNING;
        let Delivery::Ready(message) = ar.deliver(&accepted(), now, false) else {
            panic!("a delayed autoresponder is still a message");
        };
        assert!(message.delayed);
        assert_eq!(message.due_at, Some(now + time::Duration::minutes(30)));
    }

    #[test]
    fn an_absurd_delay_is_clamped_rather_than_refused() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "thanks",
            "delay_minutes": 9_999_999,
        }));
        assert_eq!(ar.delay_minutes, Autoresponder::MAX_DELAY_MINUTES);
        assert!(
            ar.is_configured(),
            "a clamped autoresponder still answers its visitor"
        );
    }

    #[test]
    fn an_operators_own_body_is_never_the_prose() {
        // The column can carry a `body` key — written by a person, or by an import. It is
        // read for nothing, so the send stays a template rather than a mail merge.
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it",
            "body": "Dear {{name}}, you have won a prize. Reply with your bank details.",
            "template_body": "thanks for writing to {{source}}",
        }));
        let Delivery::Ready(message) = ar.deliver(&accepted(), MORNING, false) else {
            panic!("must send");
        };
        assert!(!message.body.contains("prize"), "{}", message.body);
        assert!(!message.body.contains("bank details"), "{}", message.body);
        assert!(message.body.contains("thanks for writing to Contact us"));
    }

    #[test]
    fn a_template_that_renders_to_nothing_is_refused_rather_than_sent_blank() {
        let ar = Autoresponder::from_json(&json!({
            "enabled": true, "subject": "got it", "template_body": "   \n  ",
        }));
        assert!(matches!(
            ar.deliver(&accepted(), MORNING, false),
            Delivery::InvalidTemplate(_)
        ));
    }

    #[test]
    fn an_enabled_but_empty_autoresponder_is_a_misconfiguration_not_an_off_switch() {
        // The failure this guards: an operator turns the autoresponder on, forgets the body,
        // and every lead goes unanswered. Reporting it as "not configured" tells them the
        // switch is off, so the fix they make is to look somewhere they already looked.
        let empty = Autoresponder::from_json(&json!({ "enabled": true }));
        let verdict = empty.deliver(&accepted(), MORNING, false);
        assert!(
            matches!(verdict, Delivery::InvalidTemplate(_)),
            "got {verdict:?}"
        );
        assert_eq!(verdict.reason(), "invalid_template");

        // A genuinely switched-off one still reads as off.
        let off = Autoresponder::from_json(&json!({
            "enabled": false, "subject": "got it", "template_body": "thanks",
        }));
        assert_eq!(off.deliver(&accepted(), MORNING, false), Delivery::Disabled);
    }

    #[test]
    fn an_unknown_placeholder_renders_empty_and_does_not_break_the_body() {
        assert_eq!(render("a {{nope}} b", &accepted()), "a  b");
        assert_eq!(
            render("{{name}}, from {{source}}", &accepted()),
            "Hello Ada, from Contact us"
        );
        // An unclosed placeholder is literal text, not a panic and not a swallowed tail.
        assert_eq!(render("before {{name", &accepted()), "before {{name");
        // A nameless submitter gets a greeting that still reads as one.
        let anonymous = Recipient {
            first_name: "",
            ..accepted()
        };
        assert_eq!(render("{{name}}!", &anonymous), "Hello!");
    }

    #[test]
    fn every_shipped_template_renders_something() {
        for (name, subject, body) in TEMPLATES {
            let ar = Autoresponder::from_json(&json!({
                "enabled": true, "template": name, "subject": subject, "template_body": body,
            }));
            assert!(ar.is_configured(), "{name} must be usable as shipped");
            assert!(
                matches!(ar.deliver(&accepted(), MORNING, false), Delivery::Ready(_)),
                "{name} must produce a message"
            );
        }
    }

    #[test]
    fn a_template_name_resolves_and_an_unknown_one_does_not() {
        assert!(template("quote_received").is_some());
        assert!(template("nope").is_none());
        assert!(template_names().contains(&"acknowledgement"));
    }

    #[test]
    fn the_date_header_is_rfc_2822() {
        let header = date_header(MORNING);
        assert!(header.contains("2026"), "{header}");
    }
}
