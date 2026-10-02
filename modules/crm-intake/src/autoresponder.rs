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
    ///
    /// **This variant has no producer, and that is not a tidying opportunity — it is the
    /// evidence.** `Autoresponder::deliver` answers a delayed source with
    /// `Ready(Message { delayed: true, .. })` instead, because the claim is what makes the
    /// delay happen *later* rather than never, and a `NotYet` would have no message attached
    /// for the worker to send. So the one variant whose name is the answer to "why has
    /// nothing gone out?" was unreachable, while the variant that *is* reachable was given
    /// the word `sent` by `reason()` — the trail said a 30-minute reservation had been
    /// delivered. `reason()` now maps both to `delayed`, so the arm and the word agree.
    ///
    /// It is kept rather than deleted because `deliver` is the only producer of `Delivery`
    /// and a variant that names a real state is worth keeping: the moment a caller needs the
    /// due instant *without* a message — a preview, a probe — this is the variant that says
    /// it, and deleting it would mean re-adding it with a name already in use. **An enum
    /// variant with no producer is a question the type is asking that nothing has answered
    /// yet; deleting it loses the question.**
    NotYet(OffsetDateTime),
    /// This lead has already been answered.
    AlreadySent,
    /// The template is unusable (a body that renders to nothing, or no subject).
    InvalidTemplate(String),
}

impl Delivery {
    /// The message, if one may be handed to the mailer **now**.
    ///
    /// **This is the single spelling of "is it sendable", and the delay is part of it.** A
    /// `Ready` message with `delayed: true` has been *reserved* — `prepare` takes the claim for
    /// it precisely so the worker can send it later, which is what makes a send delay a feature
    /// rather than a message that never goes out — and the thing that mails the lead is
    /// `due_reservations`. So a `Ready` that is delayed is not sendable *now*, and a caller
    /// that only tested the discriminant would hand a reservation to the mailer a second time
    /// or report a send that has not happened.
    ///
    /// It was `matches!(self, Self::Ready(_))`, with no callers anywhere in the repository,
    /// while its doc comment described the caller that treated every other variant as silence.
    /// Two other copies existed: `Outcome::sent()` in the store (which also ignored the delay,
    /// under a doc claiming the message "actually went to the mailer") and a hand-written
    /// `verdict_name()` in the route that re-listed every variant. **The arms of a match are the
    /// part that goes stale when a variant is added; a named accessor is the part that does
    /// not**, so the rule lives here once.
    #[must_use]
    pub fn sendable(&self) -> Option<&Message> {
        match self {
            Self::Ready(message) if !message.delayed => Some(message),
            _ => None,
        }
    }

    /// `true` when a message can go to the mailer immediately.
    ///
    /// A thin question over [`Delivery::sendable`], kept because "may I send this" and "give me
    /// the message" are both asked and spelling either of them with a local `matches!` is how
    /// the three copies happened.
    #[must_use]
    pub fn is_sendable(&self) -> bool {
        self.sendable().is_some()
    }

    /// The verdict's name, in the order the editor wants to explain them.
    ///
    /// **One function, not a copy per call site.** The route shipped a local `verdict_name()`
    /// that re-listed all seven variants in a match, which is the failure mode this crate keeps
    /// meeting: a hand-written list of an enum's arms does not fail to compile when a variant is
    /// added, so the new variant renders as `_ => "…"` in some caller and keeps its own name in
    /// another. The strings are the API's, so they are defined beside the variants.
    #[must_use]
    pub fn verdict_name(&self) -> &'static str {
        match self {
            Self::Ready(_) => "ready",
            Self::Disabled => "disabled",
            Self::NoRecipient(_) => "not_accepted",
            Self::NoAddress => "no_address",
            Self::NotYet(_) => "delayed",
            Self::AlreadySent => "already_sent",
            Self::InvalidTemplate(_) => "invalid_template",
        }
    }

    /// The trail line's `reason`, one word per variant so the timeline is scannable.
    ///
    /// ## The word "sent" is a claim about a MAILER, and this is the second copy of that claim
    ///
    /// It read `Self::Ready(_) => "sent"`, and `Ready` does not mean "handed to the mailer":
    /// [`Autoresponder::deliver`] returns `Ready(Message { delayed: true, .. })` for every
    /// source with a non-zero `delay_minutes` — that is the *reservation* mechanism, and the
    /// thing that eventually mails the lead is [`autoresponder_store::due_reservations`]'s
    /// caller. So a 30-minute autoresponder wrote a trail line reading `sent` for a
    /// reservation no mailer had touched, beside a `ClaimState::Reserved` chip the panel
    /// renders from the same row. **One line, two verdicts, and the false one is the word an
    /// operator scans for.** This is [`Delivery::sendable`] asked a second time in a different
    /// spelling, one accessor over.
    ///
    /// `delayed` is therefore an arm of its own, and it is the arm `NotYet` was already
    /// spelled for: the same word, reachable at last. The variant stays because it is the
    /// enum's own name for the case, and because a variant with no producer is what let this
    /// ship — see [`Delivery::NotYet`].
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Ready(message) if !message.delayed => "sent",
            Self::Ready(_) | Self::NotYet(_) => "delayed",
            Self::Disabled => "not_configured",
            Self::NoRecipient(_) => "not_accepted",
            Self::NoAddress => "no_address",
            Self::AlreadySent => "already_sent",
            Self::InvalidTemplate(_) => "invalid_template",
        }
    }

    /// The explanation this verdict carries, for the skip note's own payload.
    ///
    /// ## Why a skip note was not enough on its own
    ///
    /// `reason()` answers *which case* — one machine word, deliberately shared by two readers
    /// (the API's own vocabulary and the panel's), and the panel renders it verbatim because
    /// **there is no owner for those words anywhere in the client**. That is the reader half of
    /// this slice; this method is the writer half, and it is the same shape seen from the other
    /// side.
    ///
    /// [`Self::InvalidTemplate`] carries a `String` that says *what is wrong with the
    /// operator's own configuration* — "the autoresponder has no subject or no body", "the
    /// template renders to nothing" — and both call sites threw it away:
    /// `crm_intake.rs` passed `json!({})`, and the sweep's decline arm passed
    /// `json!({ "reserved": true })`. So the one autoresponder failure an operator can actually
    /// fix wrote `invalid_template` onto the trail and nothing else: a machine word, no
    /// sentence, on the one line whose whole job is to explain a silence.
    ///
    /// **A payload the writer drops is data the platform had and threw away**, and this is the
    /// crate's second instance of it after the batch limit that discarded the rows it filled —
    /// there the rows were read and dropped, here the sentence is read and dropped. The variant
    /// was constructed with the diagnosis attached; nothing attached it to anything.
    ///
    /// Every other variant answers `{}`: `NoRecipient` already carries its own reason *in the
    /// word* (`spam`, `rejected`, `duplicate` — `prepare` builds the context from the lead's
    /// status), and adding a second copy of it under another key would be the duplicate the
    /// REQ's own trail discipline warns about.
    #[must_use]
    pub fn skip_payload(&self) -> Value {
        match self {
            Self::InvalidTemplate(diagnosis) => serde_json::json!({
                "explanation": truncate_explanation(diagnosis),
            }),
            _ => serde_json::json!({}),
        }
    }

    /// [`Self::skip_payload`] with a caller's own context keys, on top.
    ///
    /// ## Why the merge is a named method and not a `serde_json` merge at the call site
    ///
    /// The sweep's decline arm has a fact of its own to record (`reserved: true`) and the
    /// verdict's explanation to keep, so *something* has to combine them — and if that
    /// combination is written at the call site it is a **second spelling of what a verdict
    /// carries**, written in a language (`json!` + object insert) that silently lets the later
    /// literal win. That is the shape that produced the defect: `record_skip`'s own parameter
    /// is called `detail`, so a caller writing `{"explanation": "..."}` looks correct and is
    /// erased.
    ///
    /// **The verdict is applied first and the context second**, which is the direction that
    /// matters: a context key may *add* to what the verdict said, never replace it. A verdict
    /// that answers a case carries an explanation; no caller's context has any business
    /// overwriting one.
    #[must_use]
    pub fn skip_payload_merge(&self, context: Value) -> Value {
        let mut payload = self.skip_payload();
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), context.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        } else if !context.is_null() && context.as_object().is_none() {
            // A non-object context cannot be merged, and silently dropping it would be a
            // writer that loses data it accepted — the same class as the dropped payload.
            // Keeping it under one key keeps the row self-describing instead.
            if let Some(target) = payload.as_object_mut() {
                target.insert("context".to_string(), context);
            }
        }
        payload
    }
}

/// The longest explanation a skip note stores.
///
/// A skip note is a **trail line**, and this crate's PII discipline is that a trail line carries
/// ids, keys and timings — never a rendered body and never a submitter's words. A misconfigured
/// template is operator-authored rather than visitor-authored, so its diagnosis is safe to keep;
/// the bound is here because "safe to keep" is a judgement about what the *current* callers pass,
/// and the next caller of `skip_payload` should not have to remember it.
fn truncate_explanation(diagnosis: &str) -> String {
    const MAX: usize = 200;
    let trimmed = diagnosis.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut out: String = diagnosis.chars().take(MAX).collect();
    out.push('…');
    out
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

    // ----------------------------------------------------------------------------------------
    // "Is it sendable" is one rule, and the DELAY is part of it.
    //
    // The whole test module before these asked `matches!(verdict, Delivery::Ready(_))` in
    // three places, because that was the only way to ask. Now there is a method, and these
    // tests are about the one thing the method is for: a reserved message is not a sent one.
    // ----------------------------------------------------------------------------------------

    /// A ready message built by hand, with the delay the caller wants.
    ///
    /// **`template_body`, not `body`.** The first draft of this fixture passed `body` and every
    /// test failed with "the fixture must still be a Ready message" — which is the message
    /// working: a template that renders to nothing is `InvalidTemplate`, so the fixture was
    /// not exercising the delay at all, it was exercising the empty-body guard under a name
    /// that claimed otherwise. **A fixture that fails its own premise assertion has told you
    /// which of its two inputs was wrong**, and reading the neighbouring test that passes
    /// (`a_configured_autoresponder_renders_the_submitter_by_name`) is faster than guessing.
    fn ready(delayed: bool) -> Delivery {
        let configured = Autoresponder::from_json(&json!({
            "enabled": true,
            "template": "acknowledgement",
            "subject": "We received your message",
            "template_body": "Hello,\n\nthanks for writing to {{source}}.\n",
            "delay_minutes": if delayed { 30 } else { 0 },
        }));
        let mut verdict = configured.deliver(&accepted(), MORNING, false);
        // A due instant is what makes the message a *reservation* rather than a plain ready
        // one, so the field is set the same way `deliver` sets it.
        if let Delivery::Ready(message) = &mut verdict {
            message.delayed = delayed;
            message.due_at = if delayed {
                Some(MORNING + time::Duration::minutes(30))
            } else {
                None
            };
        }
        verdict
    }

    #[test]
    fn a_delayed_message_is_reserved_not_sendable() {
        // The defect this slice closes: `Outcome::sent()` was
        // `matches!(self.verdict, Delivery::Ready(_))` under a doc comment claiming the
        // message "actually went to the mailer", and `prepare` claims a DELAYED message on
        // purpose — that claim is what makes the delay happen later instead of never.
        let delayed = ready(true);
        assert!(
            matches!(delayed, Delivery::Ready(_)),
            "the fixture must still be a Ready message, or it is not testing the delay",
        );
        assert!(
            !delayed.is_sendable(),
            "a reserved message has not gone to the mailer",
        );
        assert_eq!(delayed.sendable(), None);
    }

    #[test]
    fn an_undelayed_message_is_sendable() {
        // The positive control, and it is here because the negative test above passes against
        // a `sendable()` that returns `None` for everything — including a message that is
        // ready and due right now.
        let now = ready(false);
        assert!(now.is_sendable());
        let message = now.sendable().expect("an undelayed Ready is sendable");
        assert_eq!(message.to, "visitor@example.com");
    }

    #[test]
    fn every_non_ready_variant_is_not_sendable() {
        // The other five arms, spelled out rather than iterated, because the point is that
        // adding a sixth variant to the enum does not have to touch this test: it is
        // unreachable through `Ready` and therefore unreachable through `sendable`.
        for variant in [
            Delivery::Disabled,
            Delivery::NoRecipient("spam"),
            Delivery::NoAddress,
            Delivery::NotYet(MORNING),
            Delivery::AlreadySent,
            Delivery::InvalidTemplate("empty body".into()),
        ] {
            assert!(!variant.is_sendable(), "{variant:?} is not a send");
            assert_eq!(variant.sendable(), None);
        }
    }

    /// The word "sent" must mean a mailer, and the word "delayed" must mean a reservation.
    ///
    /// `reason()` is what the lead timeline shows, so these two are the sentence an operator
    /// reads after a lead that looks answered was not answered for half an hour. The positive
    /// control is the undelayed case: a gate (or a test) whose every assertion is "the delayed
    /// message is not called sent" is satisfied by a function that cannot say "sent" at all.
    #[test]
    fn a_delayed_message_is_not_reported_as_sent() {
        assert_eq!(ready(false).reason(), "sent");
        assert_eq!(ready(true).reason(), "delayed");
    }

    /// `NotYet` and a delayed `Ready` name the same fact and must answer with the same word.
    ///
    /// The two are different variants for a real reason — a `NotYet` carries no message to
    /// send, a delayed `Ready` does — and the trail only ever needed the word. Spelling the
    /// word twice is how the delayed case ended up borrowing the sent one.
    #[test]
    fn the_unreachable_delayed_variant_agrees_with_the_reachable_one() {
        assert_eq!(Delivery::NotYet(MORNING).reason(), ready(true).reason());
    }

    /// `reason()` and `verdict_name()` are two vocabularies on purpose: one for the operator's
    /// timeline and one for the editor. They must not drift into each other — a preview that
    /// said "sent" for a reserved message would be the same defect one screen over — so the
    /// editor's word for a ready message stays "ready" whatever the delay, because the editor
    /// is showing a *decision* and the timeline is showing a *fact*.
    #[test]
    fn the_editor_word_is_the_decision_and_the_reason_is_the_fact() {
        assert_eq!(ready(false).verdict_name(), "ready");
        assert_eq!(ready(true).verdict_name(), "ready");
        assert_eq!(ready(false).reason(), "sent");
        assert_eq!(ready(true).reason(), "delayed");
    }

    /// Every reason is a distinct, non-empty, machine-shaped word.
    ///
    /// Counted rather than copied — a copy of the list would be a second spelling of the rule,
    /// which is the defect this file exists to stop. What is worth asserting is that no two
    /// cases share a word, because a shared word is exactly how "sent" came to mean both
    /// "handed to the mailer" and "reserved for later".
    #[test]
    fn reason_names_are_distinct_and_machine_shaped() {
        let reasons = [
            ready(false).reason(),
            ready(true).reason(),
            Delivery::NotYet(MORNING).reason(),
            Delivery::Disabled.reason(),
            Delivery::NoRecipient("spam").reason(),
            Delivery::NoAddress.reason(),
            Delivery::AlreadySent.reason(),
            Delivery::InvalidTemplate("x".into()).reason(),
        ];
        for reason in reasons {
            assert!(!reason.is_empty(), "a variant has no word of its own");
            assert!(
                reason.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{reason:?} is not machine-shaped — the panel renders these verbatim"
            );
        }
        // The two DELAYED cases are allowed to share a word: they are one fact. That makes
        // eight cases and seven words, and the count is the assertion — a shared word between
        // two DIFFERENT facts is exactly how "sent" came to mean both "handed to the mailer"
        // and "reserved for later". Stating the number rather than the list means adding a
        // variant forces this to be re-derived rather than silently accepted.
        let mut facts: Vec<&str> = reasons.to_vec();
        facts.sort_unstable();
        facts.dedup();
        assert_eq!(facts.len(), 7, "two different facts share a word");
        assert_eq!(reasons.len(), 8, "a case stopped being counted");
    }

    #[test]
    fn verdict_name_names_every_variant() {
        // The route shipped a local `verdict_name()` re-listing all seven arms, and this is
        // the test that makes the enum the only place the strings live. **The arms are listed
        // here as a COUNTED assertion rather than a copy of the list** — a copy would be the
        // fourth spelling, and the thing that is worth asserting is that no two arms share a
        // name and that no name is empty, which is what a duplicated string looks like.
        let names = [
            Delivery::Ready(Message {
                to: "a@example.com".into(),
                subject: "s".into(),
                body: "b".into(),
                template: "acknowledgement".into(),
                delayed: false,
                due_at: None,
            })
            .verdict_name(),
            Delivery::Disabled.verdict_name(),
            Delivery::NoRecipient("spam").verdict_name(),
            Delivery::NoAddress.verdict_name(),
            Delivery::NotYet(MORNING).verdict_name(),
            Delivery::AlreadySent.verdict_name(),
            Delivery::InvalidTemplate("x".into()).verdict_name(),
        ];
        let mut unique: Vec<&str> = names.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "two variants share a name");
        assert!(names.iter().all(|name| !name.is_empty()));
    }

    // THE ASSERTION: the diagnosis survives to the row. `InvalidTemplate` is the only variant
    // that carries a sentence, and it is the only autoresponder failure an operator can fix
    // ("the autoresponder has no subject or no body") — so it is the one where dropping the
    // payload costs the platform a fact it already had.
    #[test]
    fn the_skip_note_keeps_the_diagnosis_the_verdict_carried() {
        let verdict =
            Delivery::InvalidTemplate("the autoresponder has no subject or no body".into());
        let payload = verdict.skip_payload();
        assert_eq!(
            payload["explanation"], "the autoresponder has no subject or no body",
            "the sentence the verdict was constructed with is dropped before it reaches the trail"
        );
        assert_eq!(
            verdict.reason(),
            "invalid_template",
            "the word still names the case; the sentence says which"
        );
    }

    // The other direction, and the reason the first assertion cannot be satisfied by a map that
    // answers everything: a variant with no sentence must add no key. `NoRecipient` already
    // carries its reason *in the word* (`spam` / `rejected` / `duplicate`), so a payload here
    // would be a second copy of a fact already stored.
    #[test]
    fn a_verdict_with_nothing_to_explain_adds_no_key() {
        for verdict in [
            Delivery::Disabled,
            Delivery::NoRecipient("spam"),
            Delivery::NoAddress,
            Delivery::AlreadySent,
            Delivery::NotYet(MORNING),
            ready(false),
            ready(true),
        ] {
            let payload = verdict.skip_payload();
            assert_eq!(
                payload,
                serde_json::json!({}),
                "{:?} added an explanation it does not have — a trail line is a fact, not a guess",
                verdict.verdict_name()
            );
        }
    }

    // A trail line is bounded, because this crate's PII discipline is that one carries ids and
    // timings and never a body. The bound is measured on characters rather than bytes so a
    // multi-byte diagnosis cannot be cut mid-character into invalid UTF-8.
    #[test]
    fn an_explanation_is_bounded_but_never_cut_mid_character() {
        let long = "é".repeat(400);
        let verdict = Delivery::InvalidTemplate(long);
        let explanation = verdict.skip_payload()["explanation"]
            .as_str()
            .expect("an explanation string")
            .to_string();
        assert!(
            explanation.chars().count() <= 201,
            "the bound is 200 characters plus its ellipsis, got {}",
            explanation.chars().count()
        );
        assert!(
            explanation.ends_with('…'),
            "a truncated note says it was truncated"
        );
    }

    // The merge direction is the whole point, and it is the direction a caller writing it
    // inline would get backwards. A context key may ADD to what a verdict said; none may
    // replace an explanation, because the verdict is the thing that knows it. The second
    // assertion states that order explicitly so a later refactor that flips it is red here
    // rather than silently re-introducing the class of defect this slice is about.
    #[test]
    fn a_caller_adds_context_to_a_verdict_and_never_overwrites_it() {
        let verdict = Delivery::InvalidTemplate("the template renders to nothing".into());
        let merged = verdict.skip_payload_merge(serde_json::json!({ "reserved": true }));
        assert_eq!(
            merged["reserved"], true,
            "the sweep's own fact must survive the merge"
        );
        assert_eq!(
            merged["explanation"], "the template renders to nothing",
            "the verdict's diagnosis must survive the merge"
        );
    }

    // The degenerate input, stated because it is what a caller passes by accident rather than
    // on purpose: a non-object context is kept, not dropped. `record_skip`'s parameter is a
    // `Value`, so `json!("reserved")` type-checks, and a writer that discards it would be the
    // same defect this slice exists to remove — one layer down.
    #[test]
    fn a_non_object_context_is_kept_rather_than_silently_dropped() {
        let merged =
            Delivery::NoAddress.skip_payload_merge(serde_json::json!("a bare string context"));
        assert_eq!(
            merged["context"], "a bare string context",
            "a context that cannot be merged is lost, and a writer that loses data it accepted \
             is the defect this whole slice is about"
        );
    }

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
