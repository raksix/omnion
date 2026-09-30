//! The records the intake store reads and writes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::mapping::MappingEntry;

/// One intake source: the binding between a capture surface and the CRM.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct IntakeSource {
    /// The row's id.
    pub id: Uuid,
    /// Organization the source belongs to.
    pub organization_id: Uuid,
    /// Site it captures for, when the source is site-bound.
    pub site_id: Option<Uuid>,
    /// The name an operator sees.
    pub name: String,
    /// `form`, `endpoint` or `import`.
    pub kind: String,
    /// The bound REQ-064 form's key, when the source is form-bound.
    pub form_key: Option<String>,
    /// SHA-256 of the issued key. The clear key is shown once and never stored.
    pub endpoint_key_hash: Option<String>,
    /// The last four characters of the issued key, so an operator can tell which one is live.
    pub endpoint_key_hint: Option<String>,
    /// The ordered mapping.
    pub mapping: Value,
    /// Targets the source refuses to save without.
    pub required_targets: Vec<String>,
    /// Whether a submission must carry the consent text.
    pub consent_required: bool,
    /// The words the visitor agreed to, quoted.
    pub consent_text: Option<String>,
    /// `link`, `create_anyway` or `reject_duplicate`.
    pub dedupe_policy: String,
    /// Pipeline a converted deal lands in.
    pub pipeline_id: Option<Uuid>,
    /// Stage a converted deal lands in.
    pub stage_id: Option<Uuid>,
    /// Tags applied to every lead this source produces.
    pub auto_tags: Vec<String>,
    /// The autoresponder's template and delay.
    pub autoresponder: Value,
    /// Whether the source accepts submissions.
    pub active: bool,
    /// Submissions per hour this source accepts.
    pub rate_limit_per_hour: i32,
    /// When a submission last landed.
    pub last_received_at: Option<OffsetDateTime>,
    /// The last failure, kept so the editor can show it.
    pub last_error: Option<String>,
    /// Source keys the bound form no longer has.
    pub broken_mappings: Vec<String>,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl IntakeSource {
    /// The mapping as typed lines, or an empty list when the column is unreadable.
    ///
    /// An unreadable mapping is `[]` rather than an error: the editor has to *open* a source
    /// whose mapping was hand-edited into nonsense, and a route that answers `500` there
    /// leaves the operator with no way to fix it.
    #[must_use]
    pub fn mapping_lines(&self) -> Vec<MappingEntry> {
        serde_json::from_value(self.mapping.clone()).unwrap_or_default()
    }

    /// The source keys the mapping reads.
    #[must_use]
    pub fn source_keys(&self) -> Vec<String> {
        crate::mapping::source_keys(&self.mapping_lines())
    }

    /// `true` when the last health check found a key the form no longer has.
    #[must_use]
    pub fn binding_is_broken(&self) -> bool {
        !self.broken_mappings.is_empty()
    }
}

/// A source to create.
#[derive(Debug, Clone, PartialEq)]
pub struct NewIntakeSource {
    /// Organization the source belongs to.
    pub organization_id: Uuid,
    /// Site it captures for.
    pub site_id: Option<Uuid>,
    /// The name an operator sees.
    pub name: String,
    /// `form`, `endpoint` or `import`.
    pub kind: String,
    /// The bound form's key.
    pub form_key: Option<String>,
    /// The ordered mapping.
    pub mapping: Vec<MappingEntry>,
    /// Targets the source refuses to save without.
    pub required_targets: Vec<String>,
    /// Whether a submission must carry the consent text.
    pub consent_required: bool,
    /// The consent wording.
    pub consent_text: Option<String>,
    /// The dedupe policy.
    pub dedupe_policy: String,
    /// Pipeline a converted deal lands in.
    pub pipeline_id: Option<Uuid>,
    /// Stage a converted deal lands in.
    pub stage_id: Option<Uuid>,
    /// Tags applied to every lead.
    pub auto_tags: Vec<String>,
    /// The autoresponder's template and delay.
    pub autoresponder: Value,
    /// Whether the source accepts submissions.
    pub active: bool,
    /// Submissions per hour.
    pub rate_limit_per_hour: i32,
    /// Who created it.
    pub created_by: Option<Uuid>,
}

impl NewIntakeSource {
    /// A minimal endpoint source with an empty mapping and the default dedupe policy.
    ///
    /// Consent is **not** required by default and no wording is attached: a source created
    /// programmatically (a test, a seed, a future import) should not have to invent legal
    /// text to exist, and an operator who *does* want consent turns it on in the editor where
    /// the wording is written. The validator still refuses `consent_required` with no text —
    /// this default simply never trips it.
    #[must_use]
    pub fn endpoint(organization_id: Uuid, name: &str, created_by: Option<Uuid>) -> Self {
        Self {
            organization_id,
            site_id: None,
            name: name.to_string(),
            kind: "endpoint".to_string(),
            form_key: None,
            mapping: Vec::new(),
            required_targets: Vec::new(),
            consent_required: false,
            consent_text: None,
            dedupe_policy: "link".to_string(),
            pipeline_id: None,
            stage_id: None,
            auto_tags: Vec::new(),
            autoresponder: Value::Object(serde_json::Map::new()),
            active: true,
            rate_limit_per_hour: 30,
            created_by,
        }
    }

    /// Attach a mapping and the targets it must satisfy.
    #[must_use]
    pub fn with_mapping(mut self, mapping: Vec<MappingEntry>, required: Vec<String>) -> Self {
        self.mapping = mapping;
        self.required_targets = required;
        self
    }
}

/// One lead, as the inbox reads it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Lead {
    /// The row's id.
    pub id: Uuid,
    /// Organization the lead belongs to.
    pub organization_id: Uuid,
    /// Site it arrived through.
    pub site_id: Option<Uuid>,
    /// The source that produced it.
    pub source_id: Option<Uuid>,
    /// One of the eight statuses.
    pub status: String,
    /// The linked contact, when one was found or made.
    pub contact_id: Option<Uuid>,
    /// The linked company.
    pub company_id: Option<Uuid>,
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
    /// E-mail, as submitted.
    pub email: Option<String>,
    /// Phone, as submitted.
    pub phone: Option<String>,
    /// Company name, as submitted.
    pub company_name: Option<String>,
    /// Job title, as submitted.
    pub job_title: Option<String>,
    /// What they asked about.
    pub product_interest: Option<String>,
    /// Their message.
    pub message: Option<String>,
    /// The consent wording as accepted.
    pub consent_text: Option<String>,
    /// Whether consent was given.
    pub consent_given: bool,
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
    /// The referring host.
    pub referrer_host: Option<String>,
    /// The page the visitor landed on.
    pub landing_path: Option<String>,
    /// The page the form was on.
    pub source_path: Option<String>,
    /// Every answer as submitted.
    pub payload: Value,
    /// The payload's size in bytes.
    pub payload_bytes: i32,
    /// The normalized key the dedupe verdict used.
    pub dedupe_key: Option<String>,
    /// The lead this one duplicates, when the policy kept it separate.
    ///
    /// A **lead**, not a contact: this pointer answers "this submission repeats one we already
    /// have", and the "it matched an existing contact" question is `dedupe_contact_id`. The two
    /// were one column for fourteen ticks and the code wrote a contact id here, which the
    /// foreign key refuses on every installation that has the CRM.
    pub duplicate_of: Option<Uuid>,
    /// The contact the dedupe verdict matched.
    ///
    /// No foreign key to `crm_contacts`, by design: that table is REQ-051's and an installation
    /// without the CRM still has to be able to write a lead row (with this column null).
    pub dedupe_contact_id: Option<Uuid>,
    /// The confidence the verdict was made on, 0.0–1.0.
    pub dedupe_score: Option<f64>,
    /// The verdict.
    pub decision: Option<String>,
    /// The rule that assigned it (slice 2).
    pub assignment_rule_id: Option<Uuid>,
    /// Why it was assigned (slice 2).
    pub assignment_reason: Option<String>,
    /// The policy whose clock is running (slice 2).
    pub sla_policy_id: Option<Uuid>,
    /// When the first response is due (slice 2).
    pub first_response_due_at: Option<OffsetDateTime>,
    /// When somebody first responded.
    pub first_response_at: Option<OffsetDateTime>,
    /// When it escalated (slice 2).
    pub escalated_at: Option<OffsetDateTime>,
    /// The spam heuristics' score.
    pub spam_score: i32,
    /// Why it was rejected, when it was.
    pub rejection_reason: Option<String>,
    /// The submitter's address, as the request carried it.
    ///
    /// Nullable on purpose: a submission that arrived through the events bus has no HTTP
    /// request behind it, and a bus delivery that the platform itself made must not be
    /// attributed to an address. The column exists so the per-address ceiling
    /// ([`crate::store::submissions_from_address_this_hour`]) can count a *durable* fact —
    /// the address can not be recovered after the request is gone, and the trail line's
    /// `detail->>'ip'` is jsonb a `where` clause cannot use.
    pub submitter_ip: Option<String>,
    /// When it arrived.
    pub received_at: OffsetDateTime,
    /// When conversion finished.
    pub converted_at: Option<OffsetDateTime>,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl Lead {
    /// The name to render in the inbox, from whichever half arrived.
    #[must_use]
    pub fn display_name(&self) -> String {
        let full = [self.first_name.as_deref(), self.last_name.as_deref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if full.is_empty() {
            self.company_name.clone().unwrap_or_else(|| "—".to_string())
        } else {
            full
        }
    }
}

/// The first-touch and last-touch attribution of a submission.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    /// UTM source.
    #[serde(default)]
    pub utm_source: Option<String>,
    /// UTM medium.
    #[serde(default)]
    pub utm_medium: Option<String>,
    /// UTM campaign.
    #[serde(default)]
    pub utm_campaign: Option<String>,
    /// UTM term.
    #[serde(default)]
    pub utm_term: Option<String>,
    /// UTM content.
    #[serde(default)]
    pub utm_content: Option<String>,
    /// The click id.
    #[serde(default)]
    pub click_id: Option<String>,
    /// The referring host.
    #[serde(default)]
    pub referrer_host: Option<String>,
    /// The landing path.
    #[serde(default)]
    pub landing_path: Option<String>,
    /// The page the form was on.
    #[serde(default)]
    pub source_path: Option<String>,
}

impl Attribution {
    /// Read the attribution out of a submission payload, keeping only non-empty values.
    ///
    /// Trailing-space and 200-character trimming happen here rather than in the caller: the
    /// UTM values come from a URL somebody typed, and a 4 KiB `utm_campaign` is a payload
    /// size problem wearing an attribution costume.
    #[must_use]
    pub fn from_payload(payload: &Value) -> Self {
        let take = |key: &str| -> Option<String> {
            let raw = payload.get(key)?.as_str()?.trim();
            if raw.is_empty() {
                return None;
            }
            let trimmed: String = raw.chars().take(200).collect();
            Some(trimmed)
        };
        Self {
            utm_source: take("utm_source"),
            utm_medium: take("utm_medium"),
            utm_campaign: take("utm_campaign"),
            utm_term: take("utm_term"),
            utm_content: take("utm_content"),
            click_id: take("click_id")
                .or_else(|| take("gclid"))
                .or_else(|| take("fbclid")),
            referrer_host: payload
                .get("referrer")
                .and_then(Value::as_str)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            landing_path: take("landing_path").or_else(|| take("landing_page")),
            source_path: take("source_path").or_else(|| take("page")),
        }
    }

    /// `true` when the submission carried no attribution at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.utm_source.is_none()
            && self.utm_medium.is_none()
            && self.utm_campaign.is_none()
            && self.click_id.is_none()
            && self.referrer_host.is_none()
            && self.landing_path.is_none()
    }

    /// Merge a later submission's attribution into an existing first touch.
    ///
    /// **First touch wins.** A second submission from the same visitor must not overwrite the
    /// campaign that first brought them in — that is the definition of first touch, and the
    /// reason this function exists rather than a plain assignment. The referrer and the
    /// landing path are the exception: they describe the *most recent* visit and are what an
    /// operator wants when they ask "where did this person come from just now".
    #[must_use]
    pub fn merge_first_touch(self, later: &Attribution) -> Attribution {
        let keep = |existing: &Option<String>, incoming: &Option<String>| -> Option<String> {
            existing.clone().or_else(|| incoming.clone())
        };
        Attribution {
            utm_source: keep(&self.utm_source, &later.utm_source),
            utm_medium: keep(&self.utm_medium, &later.utm_medium),
            utm_campaign: keep(&self.utm_campaign, &later.utm_campaign),
            utm_term: keep(&self.utm_term, &later.utm_term),
            utm_content: keep(&self.utm_content, &later.utm_content),
            click_id: keep(&self.click_id, &later.click_id),
            referrer_host: later.referrer_host.clone().or(self.referrer_host),
            landing_path: later.landing_path.clone().or(self.landing_path),
            source_path: later.source_path.clone().or(self.source_path),
        }
    }
}

/// The spam heuristics' verdict on one submission.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpamVerdict {
    /// 0–100, higher is more likely spam.
    pub score: i32,
    /// Which heuristics fired, by name.
    pub reasons: Vec<String>,
}

impl SpamVerdict {
    /// The score at or above which a submission is filed as spam.
    pub const THRESHOLD: i32 = 70;

    /// The field name a form hides from the visitor and a bot fills anyway.
    pub const HONEYPOT: &'static str = "website_confirm";

    /// The field carrying how long the visitor took to fill the form, in milliseconds.
    pub const FILL_TIME: &'static str = "fill_time_ms";

    /// `true` when the verdict is spam.
    #[must_use]
    pub fn is_spam(&self) -> bool {
        self.score >= Self::THRESHOLD
    }

    /// Run the heuristics over a submission.
    ///
    /// Three signals, and the weights are not arbitrary: they encode how *decisive* each one
    /// is on its own.
    ///
    /// * **a filled honeypot (100) — decisive on its own.** The field is hidden and empty in
    ///   a real browser, so a submission that fills it was not submitted by a person filling
    ///   a form. There is no legitimate case, which is why it is not 60 and "probably spam":
    ///   a weight that needs a second signal to cross the threshold would file the most
    ///   certain signal this module has as a lead.
    /// * a fill time under two seconds (40) — a real signal, not a certain one. Nobody types
    ///   a company name that fast, but a visitor who used autofill and hit submit in a hurry
    ///   looks the same from here. A payload with *no* fill time is not counted at all,
    ///   because a form that does not collect one (a keyed endpoint, an import) must not be
    ///   discarded for its absence.
    /// * more than forty answers (20) — a weak signal on its own and only ever a tie-breaker.
    ///
    /// The weights are additive and capped at 100 by the column's check constraint, so a
    /// payload that trips all three is stored as 100 rather than refused by the database.
    #[must_use]
    pub fn evaluate(payload: &Value) -> Self {
        let mut score = 0;
        let mut reasons = Vec::new();

        let honeypot_filled = payload
            .get(Self::HONEYPOT)
            .map(|value| match value {
                serde_json::Value::String(text) => !text.trim().is_empty(),
                serde_json::Value::Null => false,
                other => !other.is_null(),
            })
            .unwrap_or(false);
        if honeypot_filled {
            score = 100;
            reasons.push("honeypot".to_string());
        }

        if let Some(filled) = payload.get(Self::FILL_TIME).and_then(Value::as_i64) {
            if (0..2_000).contains(&filled) {
                score += 40;
                reasons.push("filled_too_fast".to_string());
            }
        }

        let answers = payload.as_object().map(|map| map.len()).unwrap_or(0);
        if answers > 40 {
            score += 20;
            reasons.push("too_many_fields".to_string());
        }

        SpamVerdict {
            score: score.min(100),
            reasons,
        }
    }
}

/// One line of a lead's history.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LeadEvent {
    /// The row's identity (a bigint, not a uuid: it is an append-only log).
    pub id: i64,
    /// The lead it belongs to.
    pub lead_id: Uuid,
    /// What happened (`received`, `assigned`, `responded`, `converted`, `rejected`, …).
    pub kind: String,
    /// Who did it, when a person did.
    pub actor_user_id: Option<Uuid>,
    /// The structured detail.
    pub detail: Value,
    /// When it happened.
    pub created_at: OffsetDateTime,
}

impl LeadEvent {
    /// Append one line to a lead's history.
    #[must_use]
    pub fn new(lead_id: Uuid, kind: &str, actor_user_id: Option<Uuid>, detail: Value) -> Self {
        Self {
            id: 0,
            lead_id,
            kind: kind.to_string(),
            actor_user_id,
            detail,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

/// A person a lead can be handed to, with the load they already carry.
///
/// The hand-over screen used to ask for a UUID. That is the identifier of an *account*, not
/// of a colleague: an operator who works the queue all day cannot be expected to know which
/// of a hundred rows is their own, and a picker that only lists names is the difference
/// between a hand-over and a guess. The count is on the same row for the same reason — a
/// picker showing twelve names with no workload tells the operator nothing about whether they
/// are about to dump the tenth lead on somebody who already has nine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeadOwner {
    /// The account id — what `assign` stores.
    pub id: Uuid,
    /// Their name, or their address when the account has no display name.
    pub label: String,
    /// Their e-mail, the stable thing a colleague is actually recognised by.
    pub email: String,
    /// Open leads they hold right now.
    pub open_leads: i64,
    /// Their account status, so a disabled colleague is not offered as a destination.
    pub status: String,
}

impl LeadOwner {
    /// What the label says, when the display name is blank.
    ///
    /// An account created by an invitation flow with no display name renders as an empty
    /// `<option>`, which is indistinguishable from the unassigned row in the list above it —
    /// so the address is the fallback rather than a blank.
    #[must_use]
    pub fn label_of(display_name: &str, email: &str) -> String {
        let name = display_name.trim();
        if !name.is_empty() {
            name.to_string()
        } else {
            email.trim().to_string()
        }
    }
}

/// What one capture call produced.
///
/// `accepted` and `lead_id` are the only two things a *public* caller is told: the intake
/// endpoint never discloses whether a matching contact exists, so a spammer cannot use the
/// response to test which addresses are already in the CRM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureOutcome {
    /// The lead's reference, as a string.
    pub lead_id: String,
    /// Whether the row was written as a real lead.
    pub accepted: bool,
    /// The reference the submitter may quote.
    pub reference: String,
}

/// The inbox's counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LeadMetrics {
    /// Leads waiting for somebody.
    pub open: i64,
    /// Leads that have breached their first-response target.
    pub breached: i64,
    /// Leads with no owner.
    pub unassigned: i64,
    /// Leads filed as duplicates.
    pub duplicates: i64,
    /// Leads discarded as spam or rejected.
    pub discarded: i64,
    /// Leads converted.
    pub converted: i64,
}

impl LeadMetrics {
    /// The counters that come from one grouped read.
    #[must_use]
    pub fn from_rows(rows: &[(String, i64)]) -> Self {
        let count = |wanted: &[&str]| -> i64 {
            rows.iter()
                .filter(|(status, _)| wanted.contains(&status.as_str()))
                .map(|(_, total)| total)
                .sum()
        };
        let unassigned: i64 = rows
            .iter()
            .filter(|(status, _)| crate::vocabulary::is_open(status))
            .map(|(_, total)| total)
            .sum();
        Self {
            open: unassigned,
            breached: 0,
            unassigned,
            duplicates: count(&["duplicate"]),
            discarded: count(&["spam", "rejected"]),
            converted: count(&["converted"]),
        }
    }
}

/// Sanity check used by the store before it writes: is this row a lead at all?
///
/// A lead with neither e-mail nor phone cannot be contacted, so the migration's check
/// constraint refuses it. The function exists so the *caller* gets a message naming the
/// problem instead of a constraint violation, and so the same rule can be used by the
/// `Test mapping` preview, which must refuse the same payloads the real path refuses.
#[must_use]
pub fn contactable(email: Option<&str>, phone: Option<&str>) -> bool {
    email.map(str::trim).is_some_and(|value| !value.is_empty())
        || phone.map(str::trim).is_some_and(|value| !value.is_empty())
}

/// The dedupe-policy names, re-exported so the editor can render its radio group without
/// reaching into [`crate::vocabulary`].
pub const POLICIES: [&str; 3] = crate::vocabulary::DEDUPE_POLICIES;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attribution_is_read_out_of_a_payload_and_trimmed() {
        let attribution = Attribution::from_payload(&json!({
            "utm_source": " google ",
            "utm_campaign": "  spring  ",
            "gclid": "abc123",
            "referrer": "https://news.example/x",
            "landing_page": "/pricing",
            "page": "/contact",
        }));
        assert_eq!(attribution.utm_source.as_deref(), Some("google"));
        assert_eq!(attribution.utm_campaign.as_deref(), Some("spring"));
        // A click id sent under a platform's own name is still a click id.
        assert_eq!(attribution.click_id.as_deref(), Some("abc123"));
        assert_eq!(
            attribution.referrer_host.as_deref(),
            Some("https://news.example/x")
        );
        assert_eq!(attribution.landing_path.as_deref(), Some("/pricing"));
        assert_eq!(attribution.source_path.as_deref(), Some("/contact"));
    }

    #[test]
    fn an_over_long_utm_value_is_truncated_rather_than_stored() {
        let long = "x".repeat(500);
        let attribution = Attribution::from_payload(&json!({ "utm_campaign": long }));
        assert_eq!(
            attribution.utm_campaign.as_ref().map(String::len),
            Some(200)
        );
    }

    #[test]
    fn a_second_submission_does_not_overwrite_the_first_touch() {
        let first = Attribution::from_payload(&json!({
            "utm_campaign": "spring", "utm_source": "newsletter",
            "landing_path": "/pricing", "referrer": "https://a.example"
        }));
        let second = Attribution::from_payload(&json!({
            "utm_campaign": "autumn", "utm_source": "search",
            "landing_path": "/contact", "referrer": "https://b.example"
        }));
        let merged = first.merge_first_touch(&second);
        // The campaign that first brought them in stays the campaign…
        assert_eq!(merged.utm_campaign.as_deref(), Some("spring"));
        assert_eq!(merged.utm_source.as_deref(), Some("newsletter"));
        // …while the last visit is what the operator wants when asking "where did they come
        // from just now", so the referrer and the landing path follow the later submission.
        assert_eq!(merged.referrer_host.as_deref(), Some("https://b.example"));
        assert_eq!(merged.landing_path.as_deref(), Some("/contact"));
    }

    #[test]
    fn an_empty_submission_has_no_attribution() {
        assert!(Attribution::from_payload(&json!({})).is_empty());
        assert!(Attribution::from_payload(&json!({"utm_source": "  "})).is_empty());
    }

    #[test]
    fn a_filled_honeypot_is_spam_on_its_own() {
        // The key is built with `json!({ key: value })` on a *constant* — a non-literal key
        // inside the braces is not a key, so the honeypot silently never arrives and the
        // test would pass against a honeypot check that does not exist.
        let mut payload = json!({ "name": "Ada" });
        payload[SpamVerdict::HONEYPOT] = json!("http://spam.example");
        let verdict = SpamVerdict::evaluate(&payload);
        assert!(verdict.is_spam());
        assert!(verdict.reasons.contains(&"honeypot".to_string()));
        // The submitter's own words are never part of the reason: the detail is shown to an
        // operator, and a reason that quoted the spam would put attacker text in the panel.
        assert!(!verdict
            .reasons
            .iter()
            .any(|reason| reason.contains("spam.example")));
    }

    #[test]
    fn a_human_filling_the_form_is_not_spam() {
        let verdict = SpamVerdict::evaluate(&json!({
            "name": "Ada", "email": "ada@acme.co", SpamVerdict::FILL_TIME: 25_000
        }));
        assert!(!verdict.is_spam());
        assert_eq!(verdict.score, 0);
        assert!(verdict.reasons.is_empty());
    }

    #[test]
    fn a_submission_with_no_fill_time_is_never_spam_for_want_of_one() {
        // A keyed endpoint or an import does not send a fill time, and a form that a bot
        // wrote must not be discarded for the absence of a field the form does not collect.
        let verdict = SpamVerdict::evaluate(&json!({"email": "ada@acme.co"}));
        assert_eq!(verdict.score, 0);
    }

    #[test]
    fn the_three_signals_add_up_and_are_capped_at_the_column_limit() {
        // Honeypot (100) + fast fill (40) + wide payload (20) = 160, stored as 100 because
        // the column's check constraint refuses anything above it — a submission that trips
        // every heuristic must be *recorded*, not rejected by the database.
        let mut payload = json!({ SpamVerdict::FILL_TIME: 100 });
        for index in 0..50 {
            payload[format!("f{index}")] = json!("v");
        }
        payload[SpamVerdict::HONEYPOT] = json!("x");
        let verdict = SpamVerdict::evaluate(&payload);
        assert_eq!(verdict.score, 100);
        assert_eq!(verdict.reasons.len(), 3);
        assert!(verdict.reasons.contains(&"too_many_fields".to_string()));
    }

    #[test]
    fn the_weak_signals_alone_never_reach_the_threshold() {
        // A fast fill and a wide payload are hints, not verdicts. Together they stay under
        // the bar, which is the property that keeps a hurried real visitor out of the spam
        // queue — a form that discards those loses real business.
        let mut payload = json!({ SpamVerdict::FILL_TIME: 100 });
        for index in 0..50 {
            payload[format!("f{index}")] = json!("v");
        }
        let verdict = SpamVerdict::evaluate(&payload);
        assert_eq!(verdict.score, 60);
        assert!(!verdict.is_spam(), "60 is not a verdict");

        let wide: serde_json::Map<String, Value> = (0..50)
            .map(|index| (format!("f{index}"), json!("v")))
            .collect();
        let mut payload = json!({ SpamVerdict::HONEYPOT: "x", SpamVerdict::FILL_TIME: 0 });
        for (key, value) in wide {
            payload[key] = value;
        }
        let verdict = SpamVerdict::evaluate(&payload);
        assert_eq!(
            verdict.score, 100,
            "the check constraint refuses anything above 100"
        );
        assert!(verdict.reasons.contains(&"too_many_fields".to_string()));
    }

    #[test]
    fn a_negative_fill_time_is_ignored_rather_than_credited() {
        let verdict = SpamVerdict::evaluate(&json!({ SpamVerdict::FILL_TIME: -1 }));
        assert_eq!(verdict.score, 0);
    }

    #[test]
    fn a_lead_needs_an_email_or_a_phone() {
        assert!(contactable(Some("ada@acme.co"), None));
        assert!(contactable(None, Some("+905321112233")));
        // Whitespace is not a contact: a form that sends "  " has not sent anything, and
        // a lead whose e-mail is two spaces satisfies a check that means nothing.
        assert!(!contactable(Some("  "), Some("  ")));
        assert!(!contactable(None, None));
    }

    #[test]
    fn the_metrics_split_the_statuses_into_what_a_leader_asks() {
        let metrics = LeadMetrics::from_rows(&[
            ("new".to_string(), 3),
            ("assigned".to_string(), 2),
            ("converted".to_string(), 1),
            ("duplicate".to_string(), 4),
            ("spam".to_string(), 5),
        ]);
        assert_eq!(metrics.open, 5);
        assert_eq!(metrics.converted, 1);
        assert_eq!(metrics.duplicates, 4);
        assert_eq!(metrics.discarded, 5);
    }

    #[test]
    fn a_lead_without_a_name_renders_its_company_or_a_dash() {
        let mut lead = Lead {
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
            first_name: None,
            last_name: None,
            email: None,
            phone: None,
            company_name: None,
            job_title: None,
            product_interest: None,
            message: None,
            consent_text: None,
            consent_given: false,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
            utm_term: None,
            utm_content: None,
            click_id: None,
            referrer_host: None,
            landing_path: None,
            source_path: None,
            payload: Value::Null,
            payload_bytes: 0,
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
            submitter_ip: None,
            received_at: OffsetDateTime::UNIX_EPOCH,
            converted_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(lead.display_name(), "—");
        lead.company_name = Some("Acme".to_string());
        assert_eq!(lead.display_name(), "Acme");
        lead.first_name = Some("Ada".to_string());
        lead.last_name = Some("Lovelace".to_string());
        assert_eq!(lead.display_name(), "Ada Lovelace");
    }
}
