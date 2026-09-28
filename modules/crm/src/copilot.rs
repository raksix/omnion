//! The CRM copilot: summarize a deal, draft a follow-up, suggest the next action
//! (docs/requests/REQ-051, slice 4).
//!
//! The copilot is the one place in the CRM where **text that a person did not write** is shown
//! next to text they did. That makes three rules load-bearing, and all three live here so that
//! the HTTP layer cannot get one of them subtly wrong:
//!
//! 1. **It reads a snapshot, it never writes.** A summary is derived from a record as it stands
//!    at the moment of the call and is returned as a draft. No field of any CRM row is mutated by
//!    this module, and no future call can observe what an earlier one produced — which is also
//!    why there is no "apply this suggestion" button in the spec's sense: what a person does with
//!    a suggestion, they do by hand, on the ordinary forms.
//! 2. **The model's answer is untrusted text.** A provider can return HTML, a script tag, or a
//!    prompt-injection attempt wearing the voice of a system message. [`sanitise`] strips
//!    anything that is not plain text and caps the length, so the panel can render the result
//!    with a text node and be correct by construction.
//! 3. **The context is assembled here, from scoped reads.** The deal, its timeline and its
//!    company are fetched through the caller's [`Scope`], so a copilot call is exactly as
//!    restricted as the screen it sits on: a member with `own` visibility cannot ask about a
//!    colleague's deal even by naming its id, because the read that builds the prompt is the
//!    same read that draws the card.
//!
//! The prompts are constants rather than strings assembled at the call site, because a prompt
//! that is easy to change in one place and not the other is how a follow-up draft starts
//! answering a question nobody asked.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::activities::{self, TimelineEntry};
use crate::deals::{self, Deal};
use crate::error::{CrmError, Result};
use crate::query::Scope;

/// Which of the two suggestions a caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotAction {
    /// A short summary of the deal plus a suggested next action.
    Summarize,
    /// A drafted follow-up message, returned as a draft and never sent.
    FollowUp,
}

impl CopilotAction {
    /// The audit/event suffix this action is recorded under.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Summarize => "summarize",
            Self::FollowUp => "follow-up",
        }
    }
}

/// Longest a model's answer may be before it is cut.
///
/// The cap is not a politeness rule: an answer longer than this is either a provider looping or
/// a model that ignored the instruction, and both are cheaper to detect here than in a panel.
pub const MAX_ANSWER_CHARS: usize = 4000;

/// How many timeline entries are offered to the model.
///
/// A CRM deal that has three hundred activities exists; a summary of the last twelve is what a
/// person would read before a call, and a smaller window is the cheaper, more deterministic call.
pub const TIMELINE_WINDOW: i64 = 12;

/// The instruction behind [`CopilotAction::Summarize`].
pub const SUMMARIZE_SYSTEM: &str = "\
You are a CRM assistant. You are given one deal, the activities logged against it and the \
company it belongs to. Answer in three short sections, in this order and with these exact \
headings:

STATUS: one sentence on where the deal stands right now.
SIGNALS: at most three bullet points, each naming a fact from the activities you were given. \
Do not invent an activity, a date, a price or a person that is not in the input.
NEXT ACTION: one sentence naming a single concrete step for the deal's owner.

Never output HTML, markdown fences, or any tag. Plain text only.";

/// The instruction behind [`CopilotAction::FollowUp`].
pub const FOLLOW_UP_SYSTEM: &str = "\
You are a CRM assistant. You are given one deal and the activities logged against it. Draft a \
short follow-up message the deal's owner could send to the contact.

Rules:
- Plain text only. No HTML, no markdown fences, no subject line, no signature block.
- At most 150 words.
- Do not invent facts: no prices, dates, discounts or promises that are not in the input.
- Refer to the contact by the name given in the input, or say \"there\" if none is given.
- Address the open tasks or the most recent activity that is genuinely unresolved.";

/// The deal, its company and its recent history, rendered as the model's input.
///
/// This is the shape the provider sees. It is deliberately *not* a dump of CRM rows: the fields
/// are chosen so a prompt cannot accidentally carry a permission the panel is hiding anyway
/// (the amount is included because a summary that omits the value is useless, and the visibility
/// scope that let the caller read the deal is the same one that let them see the amount).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopilotContext {
    /// The deal's title.
    pub title: String,
    /// The deal's value, as text.
    pub amount: String,
    /// Its currency.
    pub currency: String,
    /// The stage it sits in.
    pub stage: String,
    /// The stage's kind: `open`, `won` or `lost`.
    pub stage_kind: String,
    /// Win probability, when the deal overrides its stage.
    pub probability: Option<i32>,
    /// The day it is expected to close.
    #[serde(default, with = "crate::dates::option")]
    pub expected_close_on: Option<time::Date>,
    /// How long it has been in its stage.
    pub days_in_stage: i64,
    /// The company it belongs to.
    pub company: Option<String>,
    /// The contact it belongs to.
    pub contact: Option<String>,
    /// The activities logged against it, newest first.
    pub timeline: Vec<CopilotEntry>,
}

/// One timeline line as the model sees it: a date, a kind and a subject, never a body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopilotEntry {
    /// `YYYY-MM-DD` — a day, because a follow-up does not need the minute.
    pub date: String,
    /// `call`, `meeting`, `note`, `task` or `stage`.
    pub kind: String,
    /// The subject, or the stage name for a stage move.
    pub subject: String,
}

impl CopilotEntry {
    /// Render one timeline entry.
    fn from_entry(entry: &TimelineEntry) -> Self {
        let date = entry.occurred_at.date().to_string();
        let kind = entry
            .kind
            .clone()
            .unwrap_or_else(|| entry.source.as_str().to_owned());
        let subject = entry.subject.clone().unwrap_or_else(|| match entry.source {
            activities::TimelineSource::StageChange => "moved to a new stage".to_owned(),
            activities::TimelineSource::Archived => "archived".to_owned(),
            activities::TimelineSource::Activity => String::new(),
        });
        Self {
            date,
            kind,
            subject,
        }
    }
}

impl CopilotContext {
    /// Read a deal and its history as the caller is allowed to see them.
    ///
    /// Both reads go through the same [`Scope`], so this returns `NotFound` for a deal the caller
    /// may not see — the copilot is not a way around the visibility rule it renders behind.
    ///
    /// # Errors
    ///
    /// [`CrmError::NotFound`] when the deal is not in the caller's scope, or the database's own
    /// error.
    pub async fn read(pool: &PgPool, scope: &Scope, deal_id: Uuid) -> Result<Self> {
        let Deal {
            title,
            amount,
            currency,
            stage_name,
            stage_kind,
            probability,
            expected_close_on,
            days_in_stage,
            company_name,
            contact_name,
            ..
        } = deals::get_deal(pool, scope, deal_id).await?;

        let timeline = activities::record_timeline(pool, scope, "deal", deal_id, TIMELINE_WINDOW)
            .await?
            .items
            .iter()
            .map(CopilotEntry::from_entry)
            .collect();

        Ok(Self {
            title,
            amount,
            currency,
            stage: stage_name,
            stage_kind,
            probability,
            expected_close_on,
            days_in_stage,
            company: company_name,
            contact: contact_name,
            timeline,
        })
    }

    /// The instruction this action sends.
    #[must_use]
    pub fn system_prompt(&self, action: CopilotAction) -> &'static str {
        match action {
            CopilotAction::Summarize => SUMMARIZE_SYSTEM,
            CopilotAction::FollowUp => FOLLOW_UP_SYSTEM,
        }
    }

    /// The user turn: the record, rendered as plain text.
    ///
    /// A text rendering rather than a JSON blob, because a model asked to reason about a JSON
    /// document tends to answer *about* the JSON — with braces, keys and quotes. A record that
    /// reads like a note gets a note back.
    #[must_use]
    pub fn user_prompt(&self) -> String {
        let mut prompt = format!(
            "Deal: {title}\nValue: {amount} {currency}\nStage: {stage} ({stage_kind})\n\
             Days in stage: {days_in_stage}\n",
            title = self.title,
            amount = self.amount,
            currency = self.currency,
            stage = self.stage,
            stage_kind = self.stage_kind,
            days_in_stage = self.days_in_stage,
        );
        if let Some(probability) = self.probability {
            prompt.push_str(&format!("Probability: {probability}%\n"));
        }
        if let Some(close_on) = self.expected_close_on {
            prompt.push_str(&format!("Expected close: {close_on}\n"));
        }
        if let Some(company) = &self.company {
            prompt.push_str(&format!("Company: {company}\n"));
        }
        if let Some(contact) = &self.contact {
            prompt.push_str(&format!("Contact: {contact}\n"));
        }
        if self.timeline.is_empty() {
            prompt.push_str("\nNo activity has been logged against this deal yet.\n");
        } else {
            prompt.push_str("\nActivity (newest first):\n");
            for entry in &self.timeline {
                prompt.push_str(&format!(
                    "- {} [{}] {}\n",
                    entry.date, entry.kind, entry.subject
                ));
            }
        }
        prompt
    }
}

/// Reduce a model's answer to plain text and a bounded length.
///
/// A provider's output is **untrusted**: it can carry a tag, a fence, or an injected instruction.
/// Rendering it as a text node already neutralises the first, but a leading fence is a thing a
/// person would see, and an unbounded answer is a thing a panel would choke on — so the fence is
/// removed and the tail is cut at [`MAX_ANSWER_CHARS`] with a marker that says so.
///
/// # Errors
///
/// [`CrmError::EmptyAnswer`] when nothing is left after trimming: the panel must not show an
/// empty draft as if the model had said "nothing to add".
pub fn sanitise(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    // A fenced block is the common shape of a model that ignored "no markdown"; unwrap it when
    // the whole answer is one fence, and strip stray tags when it is not.
    let body = match trimmed.strip_prefix("```") {
        Some(rest) => {
            let rest = rest.split_once('\n').map_or(rest, |(_, tail)| tail);
            rest.strip_suffix("```").unwrap_or(rest)
        }
        None => trimmed,
    };
    let stripped = strip_tags(body);
    let mut clean = String::with_capacity(stripped.len().min(MAX_ANSWER_CHARS) + 32);
    let mut truncated = false;
    for ch in stripped.chars() {
        if clean.chars().count() >= MAX_ANSWER_CHARS {
            truncated = true;
            break;
        }
        // A control character other than a newline or a tab is not something a person wrote.
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            clean.push(ch);
        }
    }
    let mut clean = clean.trim().to_owned();
    if clean.is_empty() {
        return Err(CrmError::EmptyAnswer);
    }
    if truncated {
        clean.push_str("\n…(cut short)");
    }
    Ok(clean)
}

/// Remove angle-bracketed runs and anything that looks like a tag.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut inside = false;
    for ch in input.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> CopilotContext {
        CopilotContext {
            title: "Renewal — Northwind".to_owned(),
            amount: "4200.00".to_owned(),
            currency: "EUR".to_owned(),
            stage: "Negotiation".to_owned(),
            stage_kind: "open".to_owned(),
            probability: Some(60),
            expected_close_on: None,
            days_in_stage: 21,
            company: Some("Northwind".to_owned()),
            contact: Some("Ada Lovelace".to_owned()),
            timeline: vec![CopilotEntry {
                date: "2026-09-20".to_owned(),
                kind: "call".to_owned(),
                subject: "Pricing review".to_owned(),
            }],
        }
    }

    #[test]
    fn the_prompt_names_the_deal_and_its_history() {
        let prompt = context().user_prompt();
        assert!(prompt.contains("Renewal — Northwind"), "{prompt}");
        assert!(prompt.contains("4200.00 EUR"), "{prompt}");
        assert!(prompt.contains("Probability: 60%"), "{prompt}");
        assert!(prompt.contains("Ada Lovelace"), "{prompt}");
        assert!(prompt.contains("2026-09-20 [call] Pricing review"), "{prompt}");
        // A deal nobody has touched says so, rather than leaving the model to invent history.
        let mut empty = context();
        empty.timeline.clear();
        assert!(empty.user_prompt().contains("No activity has been logged"), "{}", empty.user_prompt());
    }

    #[test]
    fn each_action_sends_its_own_instruction() {
        let context = context();
        let summarize = context.system_prompt(CopilotAction::Summarize);
        let follow_up = context.system_prompt(CopilotAction::FollowUp);
        assert!(summarize.contains("NEXT ACTION"), "{summarize}");
        assert!(follow_up.contains("follow-up"), "{follow_up}");
        assert_ne!(summarize, follow_up);
        assert_eq!(CopilotAction::Summarize.slug(), "summarize");
        assert_eq!(CopilotAction::FollowUp.slug(), "follow-up");
    }

    #[test]
    fn a_fenced_answer_is_unwrapped_rather_than_shown() {
        let answer = sanitise("```\nSTATUS: negotiating\n```").expect("a usable answer");
        assert_eq!(answer, "STATUS: negotiating");
    }

    #[test]
    fn tags_and_control_characters_never_reach_the_panel() {
        let answer = sanitise("<script>alert(1)</script>STATUS: \u{7}live").expect("text survives");
        assert!(!answer.contains('<'), "{answer}");
        assert!(!answer.contains("script"), "{answer}");
        assert!(!answer.contains('\u{7}'), "{answer}");
        assert!(answer.contains("STATUS:"), "{answer}");
    }

    #[test]
    fn an_answer_that_is_only_markup_is_refused() {
        assert!(matches!(sanitise("<b></b>"), Err(CrmError::EmptyAnswer)));
        assert!(matches!(sanitise("   \n\t "), Err(CrmError::EmptyAnswer)));
    }

    #[test]
    fn a_runaway_answer_is_cut_and_says_so() {
        let raw = "x".repeat(MAX_ANSWER_CHARS + 500);
        let answer = sanitise(&raw).expect("a bounded answer");
        assert!(answer.chars().count() <= MAX_ANSWER_CHARS + 20, "{}", answer.chars().count());
        assert!(answer.ends_with("(cut short)"), "{}", &answer[answer.len().saturating_sub(40)..]);
    }

    #[test]
    fn the_newlines_a_person_wrote_survive() {
        let answer = sanitise("STATUS: live\nNEXT ACTION: call Ada").expect("multi-line answers");
        assert_eq!(answer, "STATUS: live\nNEXT ACTION: call Ada");
    }
}
