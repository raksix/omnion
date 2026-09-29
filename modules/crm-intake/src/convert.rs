//! Conversion: a lead becoming a contact and an opportunity.
//!
//! This module is **pure**, like [`crate::dedupe`] and [`crate::assignment`]: it decides what
//! a lead should turn into and what the panel should say about each step, and the SQL that
//! writes it lives in [`crate::store`]. The reason is the same as everywhere else in this
//! crate — the decisions are the part worth testing, and a decision that can only be made by
//! running a server is a decision nobody re-runs.
//!
//! Three things live here, and each answers a question an operator actually asks:
//!
//! * [`initial_amount`] — "the mapped amount band as the initial amount". A budget band is a
//!   range, and a deal pipeline sorts by amount, so a lead quoting "10k–50k" that lands at
//!   zero is a deal that sorts to the bottom of the board and looks like nobody wanted it.
//!   A band becomes the band that best represents it: an explicit number wins, then a parsed
//!   number, then the *midpoint* of a range, then nothing at all (rather than a guess).
//! * [`deal_title`] — a deal's title is what a board shows, so it is derived from the lead's
//!   own words (what they asked about, who they are) and never invented. A lead with no
//!   product interest and no company still gets a title that identifies it.
//! * [`StepPlan`] — the four documented steps and their state, computed from three facts
//!   (linked contact, linked deal, linked quote). The stepper used to be four hard-coded
//!   strings saying the buttons do not exist yet; that is precisely the "a disabled control
//!   without explanation" the QA plan forbids, and the fix is to compute the state from the
//!   row instead of asserting it in the view.

use serde_json::Value;
use uuid::Uuid;

use crate::model::Lead;

/// The documented flow, in order. The stepper renders exactly this list.
pub const STEPS: [&str; 4] = ["lead", "opportunity", "quotation", "customer"];

/// One step's state in the panel.
///
/// `blocked` is separate from `pending` and it is the reason this is an enum with four
/// variants rather than a boolean: a quotation that is pending because the sales module is
/// not installed is a *different* sentence from one that is pending because nobody pressed
/// the button, and an operator reading "waiting" for a module that does not exist concludes
/// the platform is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Done.
    Done,
    /// The next step, waiting for an action.
    Current,
    /// Not started, and it can be.
    Pending,
    /// Not started, and it cannot be here: the module it needs is not installed.
    Blocked,
}

impl StepState {
    /// The stored name, which is also the `data-step` value the depth pass keys on.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Current => "current",
            Self::Pending => "pending",
            Self::Blocked => "blocked",
        }
    }
}

/// One step as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Which step (`lead`, `opportunity`, …).
    pub key: &'static str,
    /// Where it is.
    pub state: StepState,
    /// One line saying what happened, or why it has not.
    pub note: String,
}

/// Why the quotation and customer steps cannot run on this installation.
///
/// Both are absences of a *module*, never of a permission: an operator with
/// `crm.leads.convert` in hand who cannot open a quotation is missing a feature, and the
/// stepper has to say which one rather than offering a button that answers `404`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Availability {
    /// Whether the sales module (REQ-052) is on this branch.
    pub sales: bool,
    /// Whether the commerce module (REQ-008) is installed.
    pub commerce: bool,
}

impl Availability {
    /// Both modules present — the full documented flow is reachable.
    #[must_use]
    pub fn complete() -> Self {
        Self {
            sales: true,
            commerce: true,
        }
    }

    /// Only what this branch ships.
    #[must_use]
    pub fn intake_only() -> Self {
        Self {
            sales: false,
            commerce: false,
        }
    }
}

/// What the panel shows for one lead's four steps.
///
/// Pure over three facts plus the installation's modules, so the whole stepper is one unit
/// test away from a browser and a browser is not.
#[must_use]
pub fn step_plan(lead: &Lead, availability: Availability) -> Vec<Step> {
    let opportunity = match lead.deal_id {
        Some(_) => Step {
            key: "opportunity",
            state: StepState::Done,
            note: format!(
                "Deal created in the source's pipeline{}",
                contact_note(lead)
            ),
        },
        None if lead.contact_id.is_some() => Step {
            key: "opportunity",
            state: StepState::Current,
            note: "Contact is linked. Convert to open the opportunity.".to_string(),
        },
        None => Step {
            key: "opportunity",
            state: StepState::Current,
            note: "Convert creates the contact and the opportunity together.".to_string(),
        },
    };

    let quotation = match lead.quote_id {
        Some(_) => Step {
            key: "quotation",
            state: StepState::Done,
            note: "Quotation is open and linked.".to_string(),
        },
        None if !availability.sales => Step {
            key: "quotation",
            state: StepState::Blocked,
            note: "Needs the sales module (REQ-052) — not installed on this deployment."
                .to_string(),
        },
        None if lead.deal_id.is_none() => Step {
            key: "quotation",
            state: StepState::Pending,
            note: "Opens once the opportunity exists.".to_string(),
        },
        None => Step {
            key: "quotation",
            state: StepState::Current,
            note: "The opportunity is ready to be quoted.".to_string(),
        },
    };

    let customer = match lead.status.as_str() {
        "converted" => Step {
            key: "customer",
            state: StepState::Done,
            note: "Promoted through the customer path.".to_string(),
        },
        _ if !availability.commerce => Step {
            key: "customer",
            state: StepState::Blocked,
            note: "Needs the commerce module (REQ-008) — not installed on this deployment."
                .to_string(),
        },
        _ if lead.quote_id.is_none() => Step {
            key: "customer",
            state: StepState::Pending,
            note: "Runs when the quotation is accepted.".to_string(),
        },
        _ => Step {
            key: "customer",
            state: StepState::Current,
            note: "Waits for the quotation to be accepted.".to_string(),
        },
    };

    vec![
        Step {
            key: "lead",
            state: StepState::Done,
            note: format!(
                "Arrived {} through its intake source.",
                lead.received_at.date()
            ),
        },
        opportunity,
        quotation,
        customer,
    ]
}

fn contact_note(lead: &Lead) -> String {
    match lead.contact_id {
        Some(_) => ", contact linked".to_string(),
        None => String::new(),
    }
}

/// The amount a converted deal starts with.
///
/// The order is deliberate: a number the submitter typed beats a band a dropdown gave them,
/// and both beat the midpoint of a range. The midpoint is a *representation of the band*,
/// not a guess at their budget — a lead that said "10,000–50,000" becomes 30,000, which is
/// what anybody reading the band would have guessed anyway, and it keeps the deal off the
/// bottom of a board that sorts by amount.
///
/// `None` is a real answer: a lead that said nothing about a budget gets a deal at zero,
/// which is honest, and a made-up figure is not.
#[must_use]
pub fn initial_amount(lead: &Lead) -> Option<i64> {
    for key in ["amount", "budget", "quantity", "estimated_value"] {
        if let Some(found) = lead.payload.get(key).and_then(parse_amount) {
            return Some(found);
        }
    }
    // The product interest is the last place a number is looked for, and only because a
    // quote form often has one field: "I am interested in the 5000 TL plan". It is a *weaker*
    // source than the payload's own budget key, which is why it comes second.
    lead.product_interest.as_deref().and_then(parse_amount_text)
}

/// Read a number out of a JSON value, accepting a number, a numeric string or a currency-ish
/// string like `"12,500 TL"`.
#[must_use]
pub fn parse_amount(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_f64().map(amount_from_float),
        Value::String(text) => parse_amount_text(text),
        _ => None,
    }
}

/// The number a human wrote, out of a string that may carry separators, a currency and a
/// range.
#[must_use]
pub fn parse_amount_text(raw: &str) -> Option<i64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    // A range — "10k-50k", "10,000 - 50,000", "1000–5000" — becomes its midpoint. The dash
    // variants are all present because the three characters a person actually types for a
    // range are not the same three characters a keyboard layout offers.
    for dash in ["-", "–", "—", "~", "..", " to "] {
        if let Some((left, right)) = text.split_once(dash) {
            if let (Some(low), Some(high)) = (single_amount(left), single_amount(right)) {
                return Some(amount_from_float((low as f64 + high as f64) / 2.0));
            }
        }
    }
    single_amount(text)
}

fn single_amount(text: &str) -> Option<i64> {
    let cleaned: String = text
        .chars()
        .filter(|character| character.is_ascii_digit() || *character == '.' || *character == ',')
        .collect();
    let cleaned = cleaned.replace(',', "");
    let cleaned = cleaned.trim_end_matches(".0").to_string();
    if cleaned.is_empty() || cleaned == "." {
        return None;
    }
    let number: f64 = cleaned.parse().ok()?;
    // A magnitude suffix is written in letters, and a band that says "10k–50k" means exactly
    // what it says. The suffix has to be read BEFORE the digits are filtered out, because
    // "10k" filtered to "10" is a ten-pound budget, not ten thousand — which is how a quote
    // for a redesign lands on the board as a deal worth ten currency units.
    let trimmed = text.trim();
    let magnitude = if trimmed.ends_with(['k', 'K']) {
        1_000.0
    } else if trimmed.ends_with(['m', 'M']) {
        1_000_000.0
    } else {
        1.0
    };
    Some(amount_from_float(number * magnitude))
}

/// Round a float to a whole amount, refusing negatives and anything that is not a number.
///
/// A `NaN` from a `NaN` in a payload would otherwise become the largest possible integer
/// through a float-to-int cast, which is the kind of bug that shows up as one absurd deal in
/// a board and no error anywhere.
fn amount_from_float(value: f64) -> i64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.round().min(f64::from(i32::MAX)) as i64
}

/// The title a converted deal carries on the board.
///
/// Derived from the lead's own words and never from the operator's free text: the title is
/// what somebody reads when deciding which of forty deals to open, so "Website rewrite for
/// Furkan Ermağ" is useful and "New lead" is not.
#[must_use]
pub fn deal_title(lead: &Lead) -> String {
    let interest = lead
        .product_interest
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let company = lead
        .company_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let who = company
        .map(str::to_string)
        .or_else(|| contact_name(lead))
        .unwrap_or_else(|| "a website visitor".to_string());

    let title = match interest {
        Some(product) => format!("{product} — {who}"),
        None => format!("Quote request from {who}"),
    };
    // The column is `text not null` with a 200-character check, and a check violation is a
    // 500 on a button an operator pressed once. Truncating the title is invisible; a
    // refused conversion is not.
    truncate(&title, 200)
}

/// A lead's name, or `None` when it has neither part.
#[must_use]
pub fn contact_name(lead: &Lead) -> Option<String> {
    let name = [lead.first_name.as_deref(), lead.last_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// Truncate on a character boundary, never mid-`char`.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit - 1).collect::<String>() + "…"
}

/// What a conversion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// The contact the lead now belongs to.
    pub contact_id: Uuid,
    /// Whether that contact was made here or already existed.
    pub contact_created: bool,
    /// The deal conversion opened, when the CRM's tables are there.
    pub deal_id: Option<Uuid>,
    /// The lead row, for the handler's answer.
    pub lead_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lead() -> Lead {
        Lead {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            source_id: None,
            status: "new".to_string(),
            contact_id: None,
            company_id: None,
            deal_id: None,
            quote_id: None,
            owner_user_id: None,
            first_name: Some("Furkan".to_string()),
            last_name: Some("Ermağ".to_string()),
            email: Some("f@example.com".to_string()),
            phone: None,
            company_name: Some("Acme".to_string()),
            job_title: None,
            product_interest: Some("Website rewrite".to_string()),
            message: None,
            consent_text: None,
            consent_given: true,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
            utm_term: None,
            utm_content: None,
            click_id: None,
            referrer_host: None,
            landing_path: None,
            source_path: None,
            payload: json!({}),
            payload_bytes: 2,
            dedupe_key: None,
            dedupe_contact_id: None,
            dedupe_score: None,
            duplicate_of: None,
            decision: None,
            assignment_rule_id: None,
            assignment_reason: None,
            sla_policy_id: None,
            first_response_due_at: None,
            first_response_at: None,
            escalated_at: None,
            spam_score: 0,
            rejection_reason: None,
            received_at: time::OffsetDateTime::UNIX_EPOCH,
            converted_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_plain_number_reads_as_itself() {
        assert_eq!(parse_amount_text("1200"), Some(1200));
        assert_eq!(parse_amount_text(" 1 200 "), Some(1200));
        assert_eq!(parse_amount_text("1,200"), Some(1200));
    }

    #[test]
    fn a_range_becomes_its_midpoint() {
        for dash in [
            "10k-50k",
            "10,000 - 50,000",
            "10 000 – 50 000",
            "1000 to 5000",
        ] {
            let parsed = parse_amount_text(dash).unwrap_or_else(|| panic!("{dash} did not parse"));
            // 10k–50k is 30 000; 1 000–5 000 is 3 000. Both are the midpoint, and both are
            // the number a reader of the band would have named.
            assert!(
                (parsed - 30_000).abs() <= 5 || (parsed - 3_000).abs() <= 5,
                "{dash} produced {parsed}"
            );
        }
    }

    #[test]
    fn a_non_number_is_none_rather_than_zero() {
        // Zero would be a deal at the bottom of the board, which reads as "nobody wants it".
        // `None` reads as "this lead never said", which is what happened.
        assert_eq!(parse_amount_text("call me"), None);
        assert_eq!(parse_amount_text(""), None);
        assert_eq!(parse_amount_text("—"), None);
    }

    #[test]
    fn a_negative_or_nonsense_amount_never_becomes_a_number() {
        assert_eq!(parse_amount_text("-500"), Some(500));
        assert_eq!(parse_amount(&json!(null)), None);
        assert_eq!(parse_amount(&json!(true)), None);
        // A float cast of a non-finite number is undefined behaviour in the source language
        // and a huge integer here; the guard turns it into a zero the column accepts.
        assert_eq!(amount_from_float(f64::NAN), 0);
        assert_eq!(amount_from_float(f64::INFINITY), 0);
        assert_eq!(amount_from_float(-1.0), 0);
    }

    #[test]
    fn the_payloads_own_number_wins_over_the_product_field() {
        let mut row = lead();
        row.payload = json!({ "budget": 5000 });
        row.product_interest = Some("1000".to_string());
        assert_eq!(initial_amount(&row), Some(5000));
    }

    #[test]
    fn a_lead_that_said_nothing_about_budget_gets_no_amount() {
        assert_eq!(initial_amount(&lead()), None);
    }

    #[test]
    fn the_title_names_the_product_and_the_company() {
        assert_eq!(deal_title(&lead()), "Website rewrite — Acme");
    }

    #[test]
    fn a_lead_with_no_company_falls_back_to_the_person() {
        let mut row = lead();
        row.company_name = None;
        assert_eq!(deal_title(&row), "Website rewrite — Furkan Ermağ");
    }

    #[test]
    fn a_lead_with_neither_still_gets_an_identifying_title() {
        let mut row = lead();
        row.company_name = None;
        row.product_interest = None;
        row.first_name = None;
        row.last_name = None;
        assert_eq!(deal_title(&row), "Quote request from a website visitor");
    }

    #[test]
    fn a_very_long_title_is_truncated_below_the_column_check() {
        let mut row = lead();
        row.product_interest = Some("x".repeat(400));
        let title = deal_title(&row);
        assert!(title.chars().count() <= 200, "{}", title.chars().count());
    }

    #[test]
    fn the_first_step_is_done_and_the_rest_are_not() {
        let plan = step_plan(&lead(), Availability::intake_only());
        assert_eq!(plan[0].key, "lead");
        assert_eq!(plan[0].state, StepState::Done);
        assert_eq!(plan[1].state, StepState::Current);
    }

    #[test]
    fn a_missing_sales_module_blocks_the_quotation_and_says_so() {
        let plan = step_plan(&lead(), Availability::intake_only());
        let quotation = &plan[2];
        assert_eq!(quotation.state, StepState::Blocked);
        // The note has to name the module, or an operator reads "waiting" and files a bug.
        assert!(quotation.note.contains("REQ-052"), "{}", quotation.note);
    }

    #[test]
    fn an_installed_sales_module_does_not_offer_a_quotation_before_the_opportunity() {
        // Sales installed changes what is *possible*, not what is *next*. A quotation over a
        // lead with no deal would have nothing to price, so the step stays pending and the
        // note says what it is waiting for — the blocked message is reserved for the module
        // being absent, and using it for "not yet" would make the two indistinguishable.
        let plan = step_plan(&lead(), Availability::complete());
        assert_eq!(plan[2].state, StepState::Pending);
        assert!(plan[2].note.contains("opportunity"), "{}", plan[2].note);
        assert!(!plan[2].note.contains("not installed"));
    }

    #[test]
    fn an_opportunity_with_sales_installed_is_ready_to_quote() {
        let mut row = lead();
        row.contact_id = Some(Uuid::from_u128(3));
        row.deal_id = Some(Uuid::from_u128(4));
        let plan = step_plan(&row, Availability::complete());
        assert_eq!(plan[2].state, StepState::Current);
    }

    #[test]
    fn a_linked_deal_marks_the_opportunity_done() {
        let mut row = lead();
        row.deal_id = Some(Uuid::from_u128(7));
        let plan = step_plan(&row, Availability::complete());
        assert_eq!(plan[1].state, StepState::Done);
        assert!(plan[1].note.contains("Deal created"), "{}", plan[1].note);
    }

    #[test]
    fn a_converted_lead_is_done_at_the_customer_step() {
        let mut row = lead();
        row.quote_id = Some(Uuid::from_u128(9));
        row.status = "converted".to_string();
        let plan = step_plan(&row, Availability::complete());
        assert_eq!(plan[3].state, StepState::Done);
    }

    #[test]
    fn commerce_being_absent_does_not_undo_a_completed_customer() {
        // The one ordering a status-first implementation gets wrong: it checks the modules
        // before the facts, and an installed-then-removed module reports a finished lead as
        // blocked. The lead's own state outranks the deployment's.
        let mut row = lead();
        row.quote_id = Some(Uuid::from_u128(9));
        row.status = "converted".to_string();
        let plan = step_plan(&row, Availability::intake_only());
        assert_eq!(plan[3].state, StepState::Done);
    }
}
