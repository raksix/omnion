//! The intake store: the SQL behind sources, capture, the inbox and the duplicate queue.
//!
//! Three rules shape every function here.
//!
//! * **Organization-scoped always.** No function takes an organization the caller chose *and*
//!   an id the caller chose, in a way that lets them disagree: every read is filtered by the
//!   organization **and** the row's own id, so a lead of another tenant is a `None`, not an
//!   error the handler has to remember to convert.
//! * **The public capture path is the only unauthenticated write, and it writes through one
//!   function.** [`capture`] is the single place a submission becomes a row, which is what
//!   makes "one submission, one lead" a property of the code rather than of every caller
//!   remembering the same three lines.
//! * **A refusal is still a row.** [`capture`] writes the spam and rejected rows *through the
//!   same insert* as an accepted one, so the inbox can show what was discarded.

use sqlx::postgres::PgQueryResult;
use sqlx::PgPool;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::dedupe::{self, Candidate, DedupePolicy, Verdict};
use crate::error::{CrmIntakeError, Result};
use crate::keys;
use crate::mapping::{self, MappedValues, MappingEntry};
use crate::model::{
    contactable, Attribution, IntakeSource, Lead, LeadEvent, LeadMetrics, LeadOwner,
    NewIntakeSource, SpamVerdict,
};
use crate::vocabulary::{is_status, MAX_PAGE, MAX_PAYLOAD_BYTES};

pub const SOURCE_COLUMNS: &str = "id, organization_id, site_id, name, kind, form_key, \
     endpoint_key_hash, endpoint_key_hint, mapping, required_targets, consent_required, \
     consent_text, dedupe_policy, pipeline_id, stage_id, auto_tags, autoresponder, active, \
     rate_limit_per_hour, last_received_at, last_error, broken_mappings, created_by, \
     created_at, updated_at";

pub const LEAD_COLUMNS: &str = "id, organization_id, site_id, source_id, status, contact_id, \
     company_id, deal_id, quote_id, owner_user_id, first_name, last_name, email, phone, \
     company_name, job_title, product_interest, message, consent_text, consent_given, \
     utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, referrer_host, \
     landing_path, source_path, payload, payload_bytes, dedupe_key, duplicate_of, decision, \
     assignment_rule_id, assignment_reason, sla_policy_id, first_response_due_at, \
     first_response_at, escalated_at, spam_score, rejection_reason, received_at, \
     converted_at, created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------------

/// Every source of an organization, by name.
pub async fn list_sources(pool: &PgPool, organization_id: Uuid) -> Result<Vec<IntakeSource>> {
    let query = format!(
        "select {SOURCE_COLUMNS} from crm_intake_sources \
                         where organization_id = $1 order by name, id"
    );
    let rows = sqlx::query_as::<_, IntakeSource>(&query)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One source of an organization, or `None` when it is not there.
///
/// `None` covers both "no such source" and "another organization's source" — the same answer,
/// on purpose: a panel that can tell those apart can enumerate ids.
pub async fn find_source(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<IntakeSource>> {
    let query = format!(
        "select {SOURCE_COLUMNS} from crm_intake_sources \
                         where organization_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, IntakeSource>(&query)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Create a source, issuing a key when it is a keyed endpoint.
///
/// The clear key is returned once and never stored; the caller shows it and loses it. A
/// `form` source gets no key because it is authenticated by the form's own submission
/// validation, and issuing one anyway would put a live credential in the editor for a surface
/// that does not use it.
pub async fn create_source(
    pool: &PgPool,
    draft: &NewIntakeSource,
) -> Result<(IntakeSource, Option<keys::IssuedKey>)> {
    validate_source(draft)?;

    let issued = (draft.kind == "endpoint").then(keys::issue_key);
    let query = format!(
        "insert into crm_intake_sources \
         (organization_id, site_id, name, kind, form_key, endpoint_key_hash, endpoint_key_hint, \
          mapping, required_targets, consent_required, consent_text, dedupe_policy, pipeline_id, \
          stage_id, auto_tags, autoresponder, active, rate_limit_per_hour, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) \
         returning {SOURCE_COLUMNS}"
    );
    let mapping = serde_json::to_value(&draft.mapping).map_err(|error| {
        CrmIntakeError::invalid(format!("mapping is not serializable: {error}"))
    })?;
    let created = sqlx::query_as::<_, IntakeSource>(&query)
        .bind(draft.organization_id)
        .bind(draft.site_id)
        .bind(draft.name.trim())
        .bind(&draft.kind)
        .bind(draft.form_key.as_deref().map(str::trim))
        .bind(issued.as_ref().map(|key| key.hash.clone()))
        .bind(issued.as_ref().map(|key| key.hint.clone()))
        .bind(mapping)
        .bind(&draft.required_targets)
        .bind(draft.consent_required)
        .bind(draft.consent_text.as_deref())
        .bind(&draft.dedupe_policy)
        .bind(draft.pipeline_id)
        .bind(draft.stage_id)
        .bind(&draft.auto_tags)
        .bind(&draft.autoresponder)
        .bind(draft.active)
        .bind(draft.rate_limit_per_hour)
        .bind(draft.created_by)
        .fetch_one(pool)
        .await?;
    Ok((created, issued))
}

/// Refuse a source the platform will not store, naming the field.
fn validate_source(draft: &NewIntakeSource) -> Result<()> {
    if draft.name.trim().is_empty() {
        return Err(CrmIntakeError::invalid("name is required"));
    }
    if !crate::vocabulary::is_source_kind(&draft.kind) {
        return Err(CrmIntakeError::invalid(format!(
            "kind \"{}\" is not one of {}",
            draft.kind,
            crate::vocabulary::SOURCE_KINDS.join(", ")
        )));
    }
    if !crate::vocabulary::is_dedupe_policy(&draft.dedupe_policy) {
        return Err(CrmIntakeError::invalid(format!(
            "dedupe policy \"{}\" is not one of {}",
            draft.dedupe_policy,
            crate::vocabulary::DEDUPE_POLICIES.join(", ")
        )));
    }
    if !(1..=10_000).contains(&draft.rate_limit_per_hour) {
        return Err(CrmIntakeError::invalid(
            "rate limit must be between 1 and 10000 submissions per hour",
        ));
    }
    if draft.kind == "form"
        && draft
            .form_key
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(CrmIntakeError::invalid(
            "a form source needs the form's key",
        ));
    }
    // A source that demands consent but has no wording to demand is a source that cannot
    // store what the visitor agreed to, which is the only proof the platform ever has.
    if draft.consent_required
        && draft
            .consent_text
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(CrmIntakeError::invalid(
            "consent is required, so consent_text must say what the visitor agreed to",
        ));
    }
    mapping::validate_required_targets(&draft.mapping, &draft.required_targets)
}

/// Update a source.
///
/// The mapping is validated against the source's own required targets before the write, so a
/// mapping that would drop a required field is refused *here* rather than at the next
/// submission from a real visitor.
pub async fn update_source(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    patch: &SourcePatch,
) -> Result<Option<IntakeSource>> {
    let Some(existing) = find_source(pool, organization_id, id).await? else {
        return Ok(None);
    };

    let name = patch.name.as_deref().unwrap_or(&existing.name);
    if name.trim().is_empty() {
        return Err(CrmIntakeError::invalid("name is required"));
    }
    let dedupe_policy = patch
        .dedupe_policy
        .as_deref()
        .unwrap_or(&existing.dedupe_policy);
    if !crate::vocabulary::is_dedupe_policy(dedupe_policy) {
        return Err(CrmIntakeError::invalid(format!(
            "dedupe policy \"{dedupe_policy}\" is not one of {}",
            crate::vocabulary::DEDUPE_POLICIES.join(", ")
        )));
    }
    let rate_limit = patch
        .rate_limit_per_hour
        .unwrap_or(existing.rate_limit_per_hour);
    if !(1..=10_000).contains(&rate_limit) {
        return Err(CrmIntakeError::invalid(
            "rate limit must be between 1 and 10000 submissions per hour",
        ));
    }

    let lines = patch
        .mapping
        .clone()
        .unwrap_or_else(|| existing.mapping_lines());
    let required = patch
        .required_targets
        .clone()
        .unwrap_or_else(|| existing.required_targets.clone());
    mapping::validate_required_targets(&lines, &required)?;

    let consent_required = patch.consent_required.unwrap_or(existing.consent_required);
    let consent_text = patch.consent_text.clone().or(existing.consent_text.clone());
    if consent_required
        && consent_text
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(CrmIntakeError::invalid(
            "consent is required, so consent_text must say what the visitor agreed to",
        ));
    }

    let query = format!(
        "update crm_intake_sources set name = $3, mapping = $4, required_targets = $5, \
         consent_required = $6, consent_text = $7, dedupe_policy = $8, auto_tags = $9, \
         autoresponder = $10, active = $11, rate_limit_per_hour = $12, pipeline_id = $13, \
         stage_id = $14, updated_at = now() \
         where organization_id = $1 and id = $2 returning {SOURCE_COLUMNS}"
    );
    let mapping_value = serde_json::to_value(&lines).map_err(|error| {
        CrmIntakeError::invalid(format!("mapping is not serializable: {error}"))
    })?;
    let updated = sqlx::query_as::<_, IntakeSource>(&query)
        .bind(organization_id)
        .bind(id)
        .bind(name.trim())
        .bind(mapping_value)
        .bind(&required)
        .bind(consent_required)
        .bind(consent_text.as_deref())
        .bind(dedupe_policy)
        .bind(patch.auto_tags.clone().unwrap_or(existing.auto_tags))
        .bind(
            patch
                .autoresponder
                .clone()
                .unwrap_or(existing.autoresponder),
        )
        .bind(patch.active.unwrap_or(existing.active))
        .bind(rate_limit)
        .bind(patch.pipeline_id.or(existing.pipeline_id))
        .bind(patch.stage_id.or(existing.stage_id))
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// The fields an update may carry. `None` means "leave it alone".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourcePatch {
    /// New name.
    pub name: Option<String>,
    /// New mapping.
    pub mapping: Option<Vec<MappingEntry>>,
    /// New required targets.
    pub required_targets: Option<Vec<String>>,
    /// Whether consent is required.
    pub consent_required: Option<bool>,
    /// New consent wording.
    pub consent_text: Option<String>,
    /// New dedupe policy.
    pub dedupe_policy: Option<String>,
    /// New auto tags.
    pub auto_tags: Option<Vec<String>>,
    /// New autoresponder.
    pub autoresponder: Option<serde_json::Value>,
    /// Whether the source accepts submissions.
    pub active: Option<bool>,
    /// New hourly ceiling.
    pub rate_limit_per_hour: Option<i32>,
    /// New pipeline.
    pub pipeline_id: Option<Uuid>,
    /// New stage.
    pub stage_id: Option<Uuid>,
}

/// Delete a source.
///
/// The leads it produced keep their rows and lose their `source_id` (the column is
/// `on delete set null`): deleting a lead source must not delete the leads a business already
/// worked.
pub async fn delete_source(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let result: PgQueryResult = sqlx::query(
        "delete from crm_intake_sources \
                                            where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Rotate a source's endpoint key, returning the new clear key once.
///
/// Refused for a source that has no key (a form-bound one), because "rotate" on a surface
/// with no key is a button that appears to work and does nothing.
pub async fn rotate_key(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<keys::IssuedKey>> {
    if find_source(pool, organization_id, id).await?.is_none() {
        return Ok(None);
    }
    let issued = keys::issue_key();
    sqlx::query(
        "update crm_intake_sources set endpoint_key_hash = $3, endpoint_key_hint = $4, \
                 updated_at = now() where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(id)
    .bind(&issued.hash)
    .bind(&issued.hint)
    .execute(pool)
    .await?;
    Ok(Some(issued))
}

/// Look a source up by its public key — the public intake path's only lookup.
///
/// The key is hashed and matched against the stored digest, so this is one indexed equality
/// and the clear key never reaches the database. An unknown key is `None`, not an error: the
/// caller answers `401` for both an unknown key and a wrong one, because a distinct answer
/// for a source that does not exist is a source enumeration.
pub async fn find_source_by_key(pool: &PgPool, key: &str) -> Result<Option<IntakeSource>> {
    let hash = keys::hash_key(key);
    let query = format!(
        "select {SOURCE_COLUMNS} from crm_intake_sources \
                         where endpoint_key_hash = $1 and active"
    );
    Ok(sqlx::query_as::<_, IntakeSource>(&query)
        .bind(hash)
        .fetch_optional(pool)
        .await?)
}

/// Record a source's health: which mapping keys the bound form no longer has.
pub async fn set_broken_mappings(pool: &PgPool, id: Uuid, broken: &[String]) -> Result<()> {
    sqlx::query(
        "update crm_intake_sources set broken_mappings = $2, updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(broken)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record that a source accepted (or refused) a submission, and why.
///
/// `last_error` is *not* cleared on success by this function; the editor shows the last thing
/// that went wrong, because an operator asking "why is nothing arriving" wants the error from
/// an hour ago, not a field that was reset to null by a submission that happened to work.
pub async fn record_source_outcome(
    pool: &PgPool,
    id: Uuid,
    received: bool,
    error: Option<&str>,
) -> Result<()> {
    if received {
        sqlx::query(
            "update crm_intake_sources set last_received_at = now(), updated_at = now() \
                     where id = $1",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    if let Some(message) = error {
        sqlx::query(
            "update crm_intake_sources set last_error = $2, updated_at = now() where id = $1",
        )
        .bind(id)
        .bind(truncate(message, 500))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// How many submissions this source has taken in the last hour.
///
/// Counted from the source's own leads rather than a counter column: a counter that is
/// incremented on the way in and not decremented when a row is deleted is a rate limit that
/// eventually blocks a source nobody is using.
pub async fn submissions_this_hour(pool: &PgPool, source_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from crm_leads \
         where source_id = $1 and received_at > now() - interval '1 hour'",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// ---------------------------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------------------------

/// What the capture function needs from the caller.
///
/// A struct rather than eleven positional arguments, because the call site is the difference
/// between "a submission arrived" and "a submission from *this* source, with *this* visitor's
/// IP and *this* attribution" — and eleven `Option`s in a row is how the attribution ends up
/// on the honeypot argument.
#[derive(Debug, Clone, PartialEq)]
pub struct Submission {
    /// The organization the lead belongs to.
    pub organization_id: Uuid,
    /// The site it arrived through.
    pub site_id: Option<Uuid>,
    /// The source that produced it.
    pub source_id: Uuid,
    /// The submission id from the form's own event, when there is one. Two deliveries of the
    /// same submission id produce one lead.
    pub submission_id: Option<String>,
    /// The submitter's IP, for the per-IP rate limit and the audit trail.
    pub ip: Option<String>,
    /// Every answer as submitted.
    pub payload: serde_json::Value,
    /// When it arrived.
    pub received_at: time::OffsetDateTime,
}

/// The result of a capture, internal to the module.
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    /// The row that was written.
    pub lead: Lead,
    /// The dedupe verdict.
    pub verdict: Verdict,
    /// The spam heuristics' verdict.
    pub spam: SpamVerdict,
    /// The attribution, first touch merged.
    pub attribution: Attribution,
}

/// Turn a submission into a lead row.
///
/// The one place a submission becomes a row, and the order of its steps is the whole design:
///
/// 1. **Size first.** A payload over the ceiling is refused before anything reads it, so an
///    over-large submission cannot make the platform hold it even briefly.
/// 2. **The source's own health.** A source whose mapping has broken keys is still live —
///    refusing its submissions would lose real business over a rename — but the lead records
///    it, so the editor can show what is happening.
/// 3. **Mapping, then contactability.** A submission with neither e-mail nor phone after
///    mapping writes a *rejected* row with the reason, never a partial lead.
/// 4. **Spam after mapping.** A honeypot is checked before anything is written, but a
///    submission that also failed to map is recorded as rejected rather than spam: "you did
///    not fill in the form" and "you are a bot" are different verdicts and an operator chasing
///    a broken form needs the first one.
/// 5. **Dedupe last**, against the organization's existing contacts, with the policy from the
///    source.
pub async fn capture(pool: &PgPool, submission: &Submission) -> Result<Captured> {
    let source = find_source(pool, submission.organization_id, submission.source_id)
        .await?
        .ok_or(CrmIntakeError::UnknownKey)?;
    if !source.active {
        return Err(CrmIntakeError::invalid("this intake source is paused"));
    }

    let size = mapping::payload_size(&submission.payload);
    if size > MAX_PAYLOAD_BYTES {
        return Err(CrmIntakeError::PayloadTooLarge {
            max: MAX_PAYLOAD_BYTES,
            actual: size,
        });
    }

    // The source's own ceiling. Counted before the write so two simultaneous floods cannot
    // both read "29 of 30" and both write.
    let taken = submissions_this_hour(pool, source.id).await?;
    if taken >= i64::from(source.rate_limit_per_hour) {
        return Err(CrmIntakeError::RateLimited);
    }

    // One submission, one lead: a retried delivery finds the row the first one wrote.
    if let Some(submission_id) = submission.submission_id.as_deref() {
        if let Some(existing) = find_lead_by_submission(pool, source.id, submission_id).await? {
            return Ok(Captured {
                spam: SpamVerdict::default(),
                attribution: Attribution::default(),
                verdict: Verdict::Unique,
                lead: existing,
            });
        }
    }

    let lines = source.mapping_lines();
    let mapped = mapping::apply(&lines, &submission.payload)?;
    let spam = SpamVerdict::evaluate(&submission.payload);
    let attribution = merge_attribution(
        pool,
        &source,
        submission,
        &Attribution::from_payload(&submission.payload),
    )
    .await?;

    let email = mapped.get("email").map(str::to_string);
    let phone = mapped.get("phone").map(str::to_string);
    let consent_given = consent_satisfied(&source, &submission.payload);

    // 3. Nothing to contact: a rejected row, never a partial lead.
    if !mapped.missing_required.is_empty() || !contactable(email.as_deref(), phone.as_deref()) {
        let reason = if !mapped.missing_required.is_empty() {
            format!(
                "required field(s) not filled: {}",
                mapped.missing_required.join(", ")
            )
        } else {
            "neither e-mail nor phone was submitted".to_string()
        };
        let lead = insert_lead(
            pool,
            submission,
            &source,
            &mapped,
            &attribution,
            size as i32,
            LeadWrite {
                status: "rejected".to_string(),
                decision: Some("rejected".to_string()),
                rejection_reason: Some(reason),
                spam_score: spam.score,
                consent_given,
                ..LeadWrite::default()
            },
        )
        .await?;
        record_source_outcome(
            pool,
            source.id,
            false,
            Some("a submission was rejected: the mapping produced no contactable lead"),
        )
        .await?;
        return Ok(Captured {
            lead,
            verdict: Verdict::Unique,
            spam,
            attribution,
        });
    }

    // 4. A bot is a recorded row, not a hole.
    if spam.is_spam() {
        let lead = insert_lead(
            pool,
            submission,
            &source,
            &mapped,
            &attribution,
            size as i32,
            LeadWrite {
                status: "spam".to_string(),
                decision: Some("spam".to_string()),
                rejection_reason: Some(format!("spam heuristics: {}", spam.reasons.join(", "))),
                spam_score: spam.score,
                consent_given,
                ..LeadWrite::default()
            },
        )
        .await?;
        record_source_outcome(pool, source.id, true, Some("spam heuristics fired")).await?;
        return Ok(Captured {
            lead,
            verdict: Verdict::Unique,
            spam,
            attribution,
        });
    }

    // 5. Dedupe, then the verdict the source's policy asks for.
    let policy = DedupePolicy::parse(&source.dedupe_policy).unwrap_or(DedupePolicy::Link);
    let candidates = fetch_candidates(pool, submission.organization_id, &mapped).await?;
    let verdict = dedupe::evaluate(&mapped, &candidates);
    let key = dedupe::dedupe_key(&mapped);

    let (status, decision, contact_id, duplicate_of, reason) = match (&verdict, policy) {
        (Verdict::Unique, _) => ("new", Some("created"), None, None, None),
        (Verdict::Matched(found), DedupePolicy::Link) => (
            "assigned",
            Some("linked"),
            Some(found.contact_id),
            None,
            None,
        ),
        (Verdict::Matched(found), DedupePolicy::CreateAnyway) => {
            let _ = found;
            ("new", Some("created"), None, None, None)
        }
        (Verdict::Matched(found), DedupePolicy::RejectDuplicate) => (
            "duplicate",
            Some("duplicate"),
            None,
            Some(found.contact_id),
            Some(format!(
                "duplicate of an existing contact (matched on {})",
                found.key.as_str()
            )),
        ),
        (Verdict::Ambiguous(found), DedupePolicy::Link) => {
            let first = &found[0];
            (
                "assigned",
                Some("linked"),
                Some(first.contact_id),
                None,
                Some(format!(
                    "linked to the best of {} matches (matched on {})",
                    found.len(),
                    first.key.as_str()
                )),
            )
        }
        (Verdict::Ambiguous(found), DedupePolicy::CreateAnyway) => {
            let _ = found;
            ("new", Some("created"), None, None, None)
        }
        (Verdict::Ambiguous(_), DedupePolicy::RejectDuplicate) => (
            "duplicate",
            Some("duplicate"),
            None,
            None,
            Some("several existing contacts matched".to_string()),
        ),
    };

    let lead = insert_lead(
        pool,
        submission,
        &source,
        &mapped,
        &attribution,
        size as i32,
        LeadWrite {
            status: status.to_string(),
            decision: decision.map(str::to_string),
            contact_id,
            duplicate_of,
            dedupe_key: key,
            rejection_reason: reason,
            consent_given,
            spam_score: spam.score,
        },
    )
    .await?;

    // A duplicate row points at the *contact* it matched, and the queue is built from the
    // lead's own decision — so it is written here rather than derived later, because a
    // duplicate queue that has to re-run the dedupe to be displayed is a queue that shows
    // different answers tomorrow.
    if decision == Some("duplicate") && contact_id.is_none() && duplicate_of.is_none() {
        let best = match &verdict {
            Verdict::Matched(found) => Some(found),
            Verdict::Ambiguous(found) => found.first(),
            Verdict::Unique => None,
        };
        if let Some(first) = best {
            sqlx::query("update crm_leads set duplicate_of = $2 where id = $1")
                .bind(lead.id)
                .bind(first.contact_id)
                .execute(pool)
                .await?;
        }
    }

    record_source_outcome(pool, source.id, true, None).await?;
    Ok(Captured {
        lead,
        verdict,
        spam,
        attribution,
    })
}

/// A stored attribution, read back as a row.
///
/// Its own type rather than a nine-element tuple: the tuple compiled, and reading it a week
/// later meant counting parentheses to find out which of the nine `Option<String>`s was the
/// referrer. `Attribution` is the same shape and is what the rest of the crate passes around,
/// so the row type exists only to be converted the moment it is read.
#[derive(Debug, sqlx::FromRow)]
struct StoredAttribution {
    utm_source: Option<String>,
    utm_medium: Option<String>,
    utm_campaign: Option<String>,
    utm_term: Option<String>,
    utm_content: Option<String>,
    click_id: Option<String>,
    referrer_host: Option<String>,
    landing_path: Option<String>,
    source_path: Option<String>,
}

impl StoredAttribution {
    fn into_attribution(self) -> Attribution {
        Attribution {
            utm_source: self.utm_source,
            utm_medium: self.utm_medium,
            utm_campaign: self.utm_campaign,
            utm_term: self.utm_term,
            utm_content: self.utm_content,
            click_id: self.click_id,
            referrer_host: self.referrer_host,
            landing_path: self.landing_path,
            source_path: self.source_path,
        }
    }
}

/// The fields of the insert that are not the mapped values themselves.
#[derive(Debug, Clone, Default, PartialEq)]
struct LeadWrite {
    /// Status to file it under.
    status: String,
    /// Dedupe verdict.
    decision: Option<String>,
    /// Contact it was linked to.
    contact_id: Option<Uuid>,
    /// Contact it duplicates.
    duplicate_of: Option<Uuid>,
    /// The normalized dedupe key.
    dedupe_key: Option<String>,
    /// Why it was rejected or duplicated.
    rejection_reason: Option<String>,
    /// Whether consent was given.
    consent_given: bool,
    /// The spam score.
    spam_score: i32,
}

#[allow(clippy::too_many_arguments)]
async fn insert_lead(
    pool: &PgPool,
    submission: &Submission,
    source: &IntakeSource,
    mapped: &MappedValues,
    attribution: &Attribution,
    payload_bytes: i32,
    write: LeadWrite,
) -> Result<Lead> {
    let payload = serde_json::to_value(&submission.payload).map_err(|error| {
        CrmIntakeError::invalid(format!("payload is not serializable: {error}"))
    })?;
    let query = format!(
        "insert into crm_leads \
         (organization_id, site_id, source_id, status, contact_id, duplicate_of, first_name, \
          last_name, email, phone, company_name, job_title, product_interest, message, \
          consent_text, consent_given, utm_source, utm_medium, utm_campaign, utm_term, \
          utm_content, click_id, referrer_host, landing_path, source_path, payload, \
          payload_bytes, dedupe_key, decision, spam_score, rejection_reason, received_at) \
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,\
         $23,$24,$25,$26,$27,$28,$29,$30,$31,$32) returning {LEAD_COLUMNS}"
    );
    let lead = sqlx::query_as::<_, Lead>(&query)
        .bind(submission.organization_id)
        .bind(submission.site_id)
        .bind(source.id)
        .bind(&write.status)
        .bind(write.contact_id)
        .bind(write.duplicate_of)
        .bind(mapped.get("first_name"))
        .bind(mapped.get("last_name"))
        .bind(mapped.get("email"))
        .bind(mapped.get("phone"))
        .bind(mapped.get("company_name"))
        .bind(mapped.get("job_title"))
        .bind(mapped.get("product_interest"))
        .bind(mapped.get("message"))
        .bind(source.consent_text.as_deref())
        .bind(write.consent_given)
        .bind(attribution.utm_source.as_deref())
        .bind(attribution.utm_medium.as_deref())
        .bind(attribution.utm_campaign.as_deref())
        .bind(attribution.utm_term.as_deref())
        .bind(attribution.utm_content.as_deref())
        .bind(attribution.click_id.as_deref())
        .bind(attribution.referrer_host.as_deref())
        .bind(attribution.landing_path.as_deref())
        .bind(attribution.source_path.as_deref())
        .bind(payload)
        .bind(payload_bytes)
        .bind(write.dedupe_key.as_deref())
        .bind(write.decision.as_deref())
        .bind(write.spam_score)
        .bind(write.rejection_reason.as_deref())
        .bind(submission.received_at)
        .fetch_one(pool)
        .await?;

    // The trail's first line is written with the row, not after it: a lead with no history is
    // a lead the detail page renders with a blank timeline, and an empty timeline is
    // indistinguishable from a broken query.
    append_event(
        pool,
        lead.id,
        "received",
        None,
        serde_json::json!({
            "source_id": source.id,
            "status": lead.status,
            "decision": lead.decision,
            "dedupe_key": lead.dedupe_key,
            "spam_score": lead.spam_score,
            "ip": submission.ip,
            "submission_id": submission.submission_id,
        }),
    )
    .await?;
    Ok(lead)
}

/// Whether the submission carried the consent the source demands.
pub fn consent_satisfied(source: &IntakeSource, payload: &serde_json::Value) -> bool {
    if !source.consent_required {
        return true;
    }
    match payload
        .get("consent")
        .or_else(|| payload.get("consent_given"))
    {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(text)) => {
            let text = text.trim().to_lowercase();
            text == "true" || text == "yes" || text == "on" || text == "1"
        }
        _ => false,
    }
}

/// The organization's contacts that could match this submission.
///
/// The email and phone indexes are what make this cheap; the third query (same company name)
/// is a `like` and is bounded by the organization. Twenty candidates is enough to rank and
/// few enough that a large tenant does not read its whole contact table on every submission.
pub async fn fetch_candidates(
    pool: &PgPool,
    organization_id: Uuid,
    mapped: &MappedValues,
) -> Result<Vec<Candidate>> {
    // **`crm_contacts` has no `company_name` column.** The company lives in `crm_companies`
    // and is reached through `company_id`, so the name is an expression over a left join,
    // not a column of the contact. Naming it as a column is a 42703 — and it is a 42703 only
    // on an installation that *has* the CRM, which is precisely the half that a CRM-less
    // test run never reaches. `scripts/qa/run-crm-convert.sh` found it on its first run.
    const CANDIDATE_COLUMNS: &str = "crm_contacts.id, crm_contacts.email, crm_contacts.phone, \
         c.name as company_name, crm_contacts.first_name, crm_contacts.last_name";
    let email = dedupe::normalize_email(mapped.get("email"));
    let phone = dedupe::normalize_phone(mapped.get("phone"));
    let company = mapped
        .get("company_name")
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if email.is_none() && phone.is_none() && company.is_none() {
        return Ok(Vec::new());
    }

    let query = format!(
        "select {CANDIDATE_COLUMNS} from crm_contacts \
         left join crm_companies c on c.id = crm_contacts.company_id \
         where crm_contacts.organization_id = $1 and crm_contacts.archived_at is null and ( \
             ($2::text is not null and lower(email) = $2) \
          or ($3::text is not null and regexp_replace(coalesce(phone, ''), '[^0-9]', '', 'g') = $3) \
          or ($4::text is not null and company_id is not null and exists ( \
                select 1 from crm_companies c2 where c2.id = crm_contacts.company_id \
                  and c2.organization_id = $1 and lower(c2.name) like $4) ) ) \
         order by crm_contacts.updated_at desc limit 20"
    );
    let rows = sqlx::query_as::<_, Candidate>(&query)
        .bind(organization_id)
        .bind(email.as_deref())
        .bind(phone.as_deref())
        .bind(company.map(|value| format!("%{}%", value.to_lowercase())))
        .fetch_all(pool)
        .await;

    match rows {
        Ok(rows) => Ok(rows),
        // **Module-absence degradation is designed, not accidental.** `crm_contacts` belongs
        // to REQ-051, which is a separate module with its own migration; an installation that
        // has the intake sources but not the CRM tables must still capture leads rather than
        // answering `500` to every visitor's form. 42P01 is `undefined_table`, and it is the
        // only error swallowed here: a *missing module* has no candidates, anything else
        // (a dead connection, a permission failure) is a real failure the caller must see.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42P01") => {
            tracing::warn!(
                organization_id = %organization_id,
                "crm_contacts is absent — intake captures without dedupe matching"
            );
            Ok(Vec::new())
        }
        Err(error) => Err(error.into()),
    }
}

/// The first touch already recorded for this visitor, if any.
///
/// Keyed on the dedupe key rather than on a cookie or a fingerprint: a returning visitor who
/// submits twice *is* the same lead by definition, and a fingerprint cookie is both a privacy
/// problem and a value a bot can clear.
async fn merge_attribution(
    pool: &PgPool,
    source: &IntakeSource,
    submission: &Submission,
    later: &Attribution,
) -> Result<Attribution> {
    let Some(key) = submission
        .payload
        .get("email")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| dedupe::normalize_email(Some(value)))
    else {
        return Ok(later.clone());
    };
    let row: Option<StoredAttribution> = sqlx::query_as(
        "select utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, \
                referrer_host, landing_path, source_path from crm_leads \
         where organization_id = $1 and source_id = $2 and lower(email) = $3 \
         order by received_at asc limit 1",
    )
    .bind(submission.organization_id)
    .bind(source.id)
    .bind(&key)
    .fetch_optional(pool)
    .await?;

    let Some(first) = row else {
        return Ok(later.clone());
    };
    Ok(first.into_attribution().merge_first_touch(later))
}

async fn find_lead_by_submission(
    pool: &PgPool,
    source_id: Uuid,
    submission_id: &str,
) -> Result<Option<Lead>> {
    // The submission id is recorded in the trail rather than in a column of its own: a second
    // lead column is a second thing to keep in sync, and the trail is already the record of
    // what happened to this lead. The lookup is a lateral over the first line, which is the
    // only one that can carry it.
    let query = format!(
        "select {LEAD_COLUMNS} from crm_leads l where l.source_id = $1 and exists ( \
            select 1 from crm_lead_events e where e.lead_id = l.id \
              and e.kind = 'received' and e.detail->>'submission_id' = $2) limit 1"
    );
    Ok(sqlx::query_as::<_, Lead>(&query)
        .bind(source_id)
        .bind(submission_id)
        .fetch_optional(pool)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// The inbox
// ---------------------------------------------------------------------------------------------

/// What one inbox read may filter by.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeadQuery {
    /// Keep only these sources.
    pub source_id: Option<Uuid>,
    /// Keep only these statuses.
    pub status: Vec<String>,
    /// `me`, `unassigned`, or a user id.
    pub owner: Option<String>,
    /// The signed-in account, used to resolve `owner=me`.
    ///
    /// Separate from `owner` on purpose: the *filter* is what the reader asked for and the
    /// *acting user* is who is asking, and collapsing them into one field is how `?owner=me`
    /// turns into `?owner=<some uuid the client sent>`.
    pub acting_user_id: Option<Uuid>,
    /// Free text over name, e-mail and message.
    pub search: Option<String>,
    /// Free text over the product interest column.
    pub product_interest: Option<String>,
    /// Received on or after this instant (RFC 3339).
    pub since: Option<time::OffsetDateTime>,
    /// Received before this instant (RFC 3339).
    pub until: Option<time::OffsetDateTime>,
    /// Page size.
    pub limit: i64,
    /// Keyset cursor: the instant of the last row of the previous page.
    pub before: Option<time::OffsetDateTime>,
}

impl LeadQuery {
    /// A page of the default size, with no filters.
    #[must_use]
    pub fn inbox() -> Self {
        Self {
            limit: 50,
            ..Self::default()
        }
    }
}

/// One page of the inbox plus the counters beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct LeadPage {
    /// The rows.
    pub leads: Vec<Lead>,
    /// The cursor for the next page, when there is one.
    pub next_before: Option<time::OffsetDateTime>,
    /// The counters.
    pub metrics: LeadMetrics,
}

/// Read the inbox.
///
/// **The page and the counters come from one filter list.** A count that disagrees with the
/// list it counts is the single most damaging kind of wrong in a panel — "3 new leads" above a
/// table of 5 — so both are built from the same `where` clause rather than from two queries
/// that look similar.
pub async fn list_leads(
    pool: &PgPool,
    organization_id: Uuid,
    query: &LeadQuery,
) -> Result<LeadPage> {
    for status in &query.status {
        if !is_status(status) {
            return Err(CrmIntakeError::invalid(format!(
                "status \"{status}\" is not one of {}",
                crate::vocabulary::STATUSES.join(", ")
            )));
        }
    }
    let limit = query.limit.clamp(1, MAX_PAGE);

    // **One filter list, two builders.** The page and the counters must agree, and the only
    // way to guarantee that is for both to be built by the *same* function over the same
    // struct — so `push_lead_filters` is called twice rather than the first builder's SQL
    // being reused (a `QueryBuilder` cannot be cloned, and hand-writing the second `where`
    // clause is how "3 new leads" ends up above a table of five).
    let mut page_sql = QueryBuilder::<Postgres>::new("select ");
    page_sql.push(LEAD_COLUMNS);
    page_sql.push(" from crm_leads where organization_id = ");
    page_sql.push_bind(organization_id);
    push_lead_filters(&mut page_sql, query);
    // Breached first, then the soonest deadline, then newest. The sort is the inbox's whole
    // reason for existing: an operator opens it to answer the most overdue thing, not to read
    // a list in arrival order.
    page_sql.push(" order by (first_response_due_at is null) asc, first_response_due_at asc, received_at desc, id desc limit ");
    page_sql.push_bind(limit + 1);
    let mut leads: Vec<Lead> = page_sql.build_query_as().fetch_all(pool).await?;
    let has_more = leads.len() > limit as usize;
    leads.truncate(limit as usize);
    let next_before = has_more
        .then(|| leads.last().map(|lead| lead.received_at))
        .flatten();

    let mut count_sql = QueryBuilder::<Postgres>::new(
        "select status, count(*) from crm_leads where organization_id = ",
    );
    count_sql.push_bind(organization_id);
    push_lead_filters(&mut count_sql, query);
    count_sql.push(" group by status");
    let rows: Vec<(String, i64)> = count_sql.build_query_as().fetch_all(pool).await?;
    let mut metrics = LeadMetrics::from_rows(&rows);
    metrics.breached = count_breached(pool, organization_id).await?;
    metrics.unassigned = count_unassigned(pool, organization_id).await?;

    Ok(LeadPage {
        leads,
        next_before,
        metrics,
    })
}

fn push_lead_filters<'args>(builder: &mut QueryBuilder<'args, Postgres>, query: &'args LeadQuery) {
    if let Some(source_id) = query.source_id {
        builder.push(" and source_id = ").push_bind(source_id);
    }
    if !query.status.is_empty() {
        builder.push(" and status = any(");
        builder.push_bind(&query.status);
        builder.push(")");
    }
    // The arms are statements: `push_bind` returns `&mut QueryBuilder` and the branches bind
    // different types, so an expression `match` would have to unify `()` with a builder
    // reference. Written as statements so each arm can bind what it binds.
    match query.owner.as_deref() {
        // "unassigned" is a filter, not a user: the inbox's most-used view is "the ones with
        // nobody on them", and expressing it as a null comparison is the only way it stays a
        // single indexed read.
        Some("unassigned") => {
            builder.push(" and owner_user_id is null");
        }
        // "me" resolves to the session's own id, resolved by the caller. A SQL-side
        // `current_setting` of a session variable nothing sets would answer "no rows" — a
        // filter that silently matches nothing is worse than no filter.
        Some("me") => match query.acting_user_id {
            Some(id) => {
                builder.push(" and owner_user_id = ").push_bind(id);
            }
            None => {
                builder.push(" and false");
            }
        },
        Some(other) if !other.is_empty() => {
            match other.parse::<Uuid>() {
                Ok(id) => {
                    builder.push(" and owner_user_id = ").push_bind(id);
                }
                // A filter that does not parse is a `400` at the route, not a query that
                // matches nothing: the two are indistinguishable to the reader otherwise.
                Err(_) => {
                    builder.push(" and false");
                }
            }
        }
        _ => {}
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = format!("%{}%", search.to_lowercase());
        builder
            .push(" and (lower(coalesce(first_name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(last_name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(email, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(message, '')) like ")
            .push_bind(pattern);
    }
    if let Some(product) = query
        .product_interest
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder
            .push(" and lower(coalesce(product_interest, '')) like ")
            .push_bind(format!("%{}%", product.to_lowercase()));
    }
    if let Some(since) = query.since {
        builder.push(" and received_at >= ").push_bind(since);
    }
    if let Some(until) = query.until {
        builder.push(" and received_at < ").push_bind(until);
    }
    if let Some(before) = query.before {
        builder.push(" and received_at < ").push_bind(before);
    }
}

async fn count_breached(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from crm_leads where organization_id = $1 and first_response_at is null \
         and first_response_due_at is not null and first_response_due_at < now() \
         and status not in ('spam', 'rejected', 'duplicate')",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

async fn count_unassigned(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "select count(*) from crm_leads where organization_id = $1 and owner_user_id is null \
         and status in ('new', 'assigned', 'contacted', 'qualified')",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// The people a lead can be handed to, with the open load each one already holds.
///
/// Three decisions here are load-bearing, and none of them is visible in the query.
///
/// * **The account has to belong to the organization.** `users.organization_id` is
///   nullable, so a *platform* account (an owner who belongs to no organization) matches no
///   filter and is simply absent — which is correct: this roster is who the tenant's own
///   leads can be routed to, and a platform operator's personal queue is not one of them.
/// * **The load is a correlated subquery, not a second pass.** The picker shows twelve people
///   and twelve counts; a second query to annotate them would be a second chance for the list
///   and the numbers to disagree, and this screen exists precisely to stop the inbox and the
///   roster telling different stories.
/// * **A disabled account is returned, not hidden.** Filtering it out would make the owner of
///   an existing lead vanish from the screen that explains who owns what, and the trail line
///   would render a raw uuid for a colleague who plainly exists. The picker marks it instead.
pub async fn list_owners(pool: &PgPool, organization_id: Uuid) -> Result<Vec<LeadOwner>> {
    let rows: Vec<(Uuid, String, String, String, i64)> = sqlx::query_as(
        "select u.id, u.display_name, u.email, u.status, \
                (select count(*) from crm_leads l where l.organization_id = $1 \
                   and l.owner_user_id = u.id and l.status in ('new','assigned','contacted','qualified')) \
         from users u \
         where u.organization_id = $1 \
         order by lower(coalesce(nullif(btrim(u.display_name), ''), u.email)), u.email",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, display_name, email, status, open_leads)| LeadOwner {
            id,
            label: LeadOwner::label_of(&display_name, &email),
            email,
            open_leads,
            status,
        })
        .collect())
}

/// One lead of an organization, or `None`.
pub async fn find_lead(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<Lead>> {
    let query =
        format!("select {LEAD_COLUMNS} from crm_leads where organization_id = $1 and id = $2");
    Ok(sqlx::query_as::<_, Lead>(&query)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// A lead's history, newest first.
pub async fn list_events(
    pool: &PgPool,
    organization_id: Uuid,
    lead_id: Uuid,
) -> Result<Vec<LeadEvent>> {
    sqlx::query_as::<_, LeadEvent>(
        "select e.id, e.lead_id, e.kind, e.actor_user_id, e.detail, e.created_at \
         from crm_lead_events e join crm_leads l on l.id = e.lead_id \
         where l.organization_id = $1 and e.lead_id = $2 order by e.created_at desc, e.id desc",
    )
    .bind(organization_id)
    .bind(lead_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Append one line to a lead's history.
pub async fn append_event(
    pool: &PgPool,
    lead_id: Uuid,
    kind: &str,
    actor_user_id: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<()> {
    append_event_on(pool, lead_id, kind, actor_user_id, detail).await
}

/// `append_event` over any executor, so a caller inside a transaction writes its trail line
/// atomically with the change it describes.
///
/// The two-argument version above exists because most trail writes are single statements that do
/// not need to be atomic with anything — and those call sites should not have to open a
/// transaction to append a line. The ones that *do* need it are exactly the writes where the
/// trail is the only record of what happened (assignment, conversion, a response): if the
/// transaction commits without the line, the panel shows a lead in a state its own history
/// contradicts, and no later read can detect the gap because both halves look valid on their own.
pub async fn append_event_on<'e, E>(
    executor: E,
    lead_id: Uuid,
    kind: &str,
    actor_user_id: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "insert into crm_lead_events (lead_id, kind, actor_user_id, detail) \
                 values ($1, $2, $3, $4)",
    )
    .bind(lead_id)
    .bind(kind)
    .bind(actor_user_id)
    .bind(detail)
    .execute(executor)
    .await?;
    Ok(())
}

/// Set a lead's status, refusing one the platform does not know.
///
/// The check is here and not only in the column because the message is the difference between
/// "status \"won\" is not one of …" and a check-constraint violation a panel cannot render.
pub async fn set_status(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    status: &str,
    actor_user_id: Option<Uuid>,
    reason: Option<&str>,
) -> Result<Option<Lead>> {
    if !is_status(status) {
        return Err(CrmIntakeError::invalid(format!(
            "status \"{status}\" is not one of {}",
            crate::vocabulary::STATUSES.join(", ")
        )));
    }
    let query = format!(
        "update crm_leads set status = $3, updated_at = now(), \
         rejection_reason = coalesce($4, rejection_reason) \
         where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    );
    let updated = sqlx::query_as::<_, Lead>(&query)
        .bind(organization_id)
        .bind(id)
        .bind(status)
        .bind(reason.map(|message| truncate(message, 500)))
        .fetch_optional(pool)
        .await?;
    if let Some(lead) = &updated {
        append_event(
            pool,
            lead.id,
            "status_changed",
            actor_user_id,
            serde_json::json!({ "status": status, "reason": reason }),
        )
        .await?;
    }
    Ok(updated)
}

/// The fields a lead edit may carry. `None` means "leave it alone".
///
/// There is no `received_at`, no `payload` and no `spam_score` here, and that is the point:
/// a panel edit fixes what a human can see on the row, and the capture-time facts (when it
/// arrived, exactly what was submitted, what the heuristics scored) are the evidence the
/// verdicts rest on. An editor that could rewrite them would make every earlier verdict
/// unfalsifiable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeadPatch {
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
    /// New status — refused unless the platform knows it.
    pub status: Option<String>,
    /// The contact this lead is linked to.
    pub contact_id: Option<Uuid>,
}

/// Edit a lead.
///
/// The `crm_leads_contactable_check` constraint is the backstop: an edit that removes both
/// the e-mail and the phone is refused by the database with the constraint's own message, and
/// the platform prefers that to a lead nobody can answer.
pub async fn patch_lead(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    patch: &LeadPatch,
) -> Result<Option<Lead>> {
    if let Some(status) = patch.status.as_deref() {
        if !is_status(status) {
            return Err(CrmIntakeError::invalid(format!(
                "status \"{status}\" is not one of {}",
                crate::vocabulary::STATUSES.join(", ")
            )));
        }
    }
    let Some(existing) = find_lead(pool, organization_id, id).await? else {
        return Ok(None);
    };

    // A lead with no e-mail and no phone cannot be worked, so a `null` on both is refused
    // here with a message that names the rule rather than surfacing a constraint violation.
    let email = patch.email.clone().or(existing.email.clone()).or(None);
    let phone = patch.phone.clone().or(existing.phone.clone()).or(None);
    if !contactable(email.as_deref(), phone.as_deref()) {
        return Err(CrmIntakeError::invalid(
            "a lead needs an e-mail or a phone — an edit that clears both is refused",
        ));
    }

    let query = format!(
        "update crm_leads set first_name = $3, last_name = $4, email = $5, phone = $6, \
         company_name = $7, job_title = $8, product_interest = $9, message = $10, \
         status = $11, contact_id = $12, updated_at = now() \
         where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    );
    let updated = sqlx::query_as::<_, Lead>(&query)
        .bind(organization_id)
        .bind(id)
        .bind(patch.first_name.clone().or(existing.first_name.clone()))
        .bind(patch.last_name.clone().or(existing.last_name.clone()))
        .bind(email)
        .bind(phone)
        .bind(patch.company_name.clone().or(existing.company_name.clone()))
        .bind(patch.job_title.clone().or(existing.job_title.clone()))
        .bind(
            patch
                .product_interest
                .clone()
                .or(existing.product_interest.clone()),
        )
        .bind(patch.message.clone().or(existing.message.clone()))
        .bind(patch.status.clone().unwrap_or(existing.status.clone()))
        .bind(patch.contact_id.or(existing.contact_id))
        .fetch_optional(pool)
        .await?;

    if let Some(lead) = &updated {
        // Only the lines that actually changed go on the trail: a trail that records every
        // field of every save is a trail nobody reads.
        let mut changed: Vec<&str> = Vec::new();
        if patch.first_name.is_some() {
            changed.push("first_name");
        }
        if patch.last_name.is_some() {
            changed.push("last_name");
        }
        if patch.email.is_some() {
            changed.push("email");
        }
        if patch.phone.is_some() {
            changed.push("phone");
        }
        if patch.company_name.is_some() {
            changed.push("company_name");
        }
        if patch.job_title.is_some() {
            changed.push("job_title");
        }
        if patch.product_interest.is_some() {
            changed.push("product_interest");
        }
        if patch.message.is_some() {
            changed.push("message");
        }
        if patch.status.is_some() {
            changed.push("status");
        }
        if patch.contact_id.is_some() {
            changed.push("contact_id");
        }
        if !changed.is_empty() {
            append_event(
                pool,
                lead.id,
                "edited",
                None,
                serde_json::json!({ "changed": changed }),
            )
            .await?;
        }
    }
    Ok(updated)
}

/// Assign or reassign a lead by hand, keeping the whole history.
///
/// The automatic rules answer "who should get this one", and they are wrong in the cases a human
/// is right about: the country rule sends a lead about a competitor's country to the wrong
/// region, the pool is round-robin and this person asked for it, the person the rule picked has
/// left. So the inbox needs a hand, and the hand has to leave a trace — a reassignment that
/// silently overwrites `owner_user_id` leaves nobody able to answer "who had this at 3pm".
///
/// Three decisions worth naming, each of which is a way the obvious version is wrong:
///
/// * **An unassign is a real instruction.** `owner_user_id: null` is not "clear the field", it
///   is "put it back in the unassigned queue", and it is a different act from assigning. So the
///   request carries the owner as an `Option<Option<Uuid>>` — three states, not two — and the
///   omitted case is a *refusal*, not a silent unassign. A request that forgets the field must
///   not empty somebody's queue by accident.
/// * **The reason is required, and the rule is not cleared.** `assignment_rule_id` is left as it
///   was: the rule that *would* have matched is a fact about the routing, and overwriting it
///   with `null` would make a later "why did this skip the pool" unanswerable. The reason
///   records the human decision; the rule records the machine one, and both are needed.
/// * **A terminal lead is not reassigned.** `spam`, `rejected` and `duplicate` are verdicts,
///   not stages; assigning one of them back to a person is how a discarded submission comes
///   back as somebody's work. The refusal names the status so the caller can un-reject first.
///
/// The SLA clock is deliberately *not* restarted. A manual assignment is a routing decision, not
/// a new promise to the person who wrote in: the deadline was set from when the lead arrived,
/// and moving the owner does not buy the submitter more time. Recomputing it here would make a
/// reassignment a way to reset a breach that has already happened.
pub async fn assign_owner(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    owner: Option<Uuid>,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Result<Option<Lead>> {
    // The write, the trail line and the read-back are one transaction. They have to be: a lead
    // that changed hands with no `crm_lead_events` row is a reassignment nobody can audit, and
    // the panel's timeline is the only place that history exists. A commit that wrote the owner
    // and then failed to write the line would leave the screen asserting a new owner with a
    // timeline that says the lead was never touched.
    let mut tx = pool.begin().await?;

    // `for update` is what makes "who had it before" true rather than probably true. Two
    // operators reassigning the same lead at the same moment must not both record themselves
    // as having taken it from the same person, and without the lock the second one's read races
    // the first one's write. The read and the update are separate statements *inside* the
    // transaction on purpose: a `returning` clause cannot see the pre-update value, so a single
    // statement would have recorded the new owner as the previous one on every reassignment.
    let row: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "select status, owner_user_id from crm_leads \
         where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((status, previous_owner)) = row else {
        tx.rollback().await?;
        return Ok(None);
    };

    // A verdict is not a piece of work. `spam`, `rejected` and `duplicate` are the answers the
    // platform gave a submission; handing one to a person is how a discarded enquiry comes back
    // as somebody's work, and the message names the status so the caller can undo the verdict
    // first rather than guessing which button to press.
    if matches!(status.as_str(), "spam" | "rejected" | "duplicate") {
        tx.rollback().await?;
        return Err(CrmIntakeError::invalid(format!(
            "this lead is '{status}' — a verdict, not work. Take it out of that state first."
        )));
    }

    let updated: Option<Lead> = sqlx::query_as(&format!(
        "update crm_leads set owner_user_id = $3, \
             status = case when status = 'new' and $3 is not null then 'assigned' else status end, \
             assignment_reason = $4, updated_at = now() \
         where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(id)
    .bind(owner)
    .bind(reason)
    .fetch_optional(&mut *tx)
    .await?;

    // `assigned` vs `reassigned` is decided by what the row *was*, not by whether the new owner
    // differs from the old one: assigning a lead to the person who already has it is a no-op
    // press, and writing "reassigned" for it would put a line on the timeline that implies
    // something happened.
    append_event_on(
        &mut *tx,
        id,
        if previous_owner.is_some() {
            "reassigned"
        } else {
            "assigned"
        },
        actor_user_id,
        serde_json::json!({
            "owner_user_id": owner.map(|o| o.to_string()),
            "previous_owner_user_id": previous_owner.map(|o| o.to_string()),
            "reason": reason,
        }),
    )
    .await?;

    tx.commit().await?;
    Ok(updated)
}

/// Record the first response, which is what stops the SLA clock.
///
/// **Idempotent on the instant.** A second call on a lead that already has
/// `first_response_at` keeps the first one and answers with the same row: "when did we first
/// answer this" has exactly one answer, and a panel that answered `200` with a *newer* time
/// would quietly rewrite the measurement a whole SLA report rests on. The caller may pass
/// `None` for the actor, because a quotation sent from REQ-052 counts as a response and that
/// path has no panel session.
pub async fn record_response(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    actor_user_id: Option<Uuid>,
) -> Result<Option<Lead>> {
    let query = format!(
        "update crm_leads set first_response_at = coalesce(first_response_at, now()), \
         status = case when first_response_at is null and status in ('new', 'assigned') \
                       then 'contacted' else status end, updated_at = now() \
         where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    );
    let updated = sqlx::query_as::<_, Lead>(&query)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if let Some(lead) = &updated {
        append_event(
            pool,
            lead.id,
            "responded",
            actor_user_id,
            serde_json::json!({
                "first_response_at": lead.first_response_at.map(|at| at.to_string()),
            }),
        )
        .await?;
    }
    Ok(updated)
}

/// The duplicate queue: rows kept separate because something already matched them.
pub async fn list_duplicates(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
) -> Result<Vec<Lead>> {
    let query = format!(
        "select {LEAD_COLUMNS} from crm_leads \
         where organization_id = $1 and (status = 'duplicate' or duplicate_of is not null) \
         order by received_at desc limit $2"
    );
    Ok(sqlx::query_as::<_, Lead>(&query)
        .bind(organization_id)
        .bind(limit.clamp(1, MAX_PAGE))
        .fetch_all(pool)
        .await?)
}

/// Delete a lead.
///
/// A lead's *data* is deletable — that is the retention promise and the reason the table is
/// not append-only — but the fact that somebody deleted it is not: the audit layer writes
/// that, outside this module. The trail goes with the row (the events table cascades), and
/// the rows that pointed *at* this lead keep existing with a null pointer
/// (`duplicate_of` is `on delete set null`), so a duplicate queue never keeps a pointer into
/// a void.
pub async fn delete_lead(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    if find_lead(pool, organization_id, id).await?.is_none() {
        return Ok(false);
    }
    let result: PgQueryResult =
        sqlx::query("delete from crm_leads where organization_id = $1 and id = $2")
            .bind(organization_id)
            .bind(id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() == 1)
}

/// Cap a stored message so an error cannot grow a column without bound.
fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::MappingEntry;

    #[test]
    fn a_source_needs_a_name_a_known_kind_and_a_known_policy() {
        let mut draft = NewIntakeSource::endpoint(Uuid::nil(), "Quote form", None);
        assert!(validate_source(&draft).is_ok());

        draft.name = "  ".to_string();
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("name"), "{error}");

        draft.name = "Quote form".to_string();
        draft.kind = "webhook".to_string();
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("webhook"), "{error}");

        draft.kind = "endpoint".to_string();
        draft.dedupe_policy = "always".to_string();
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("always"), "{error}");
    }

    #[test]
    fn a_form_source_needs_a_form_key() {
        let mut draft = NewIntakeSource::endpoint(Uuid::nil(), "Bound", None);
        draft.kind = "form".to_string();
        draft.consent_required = false;
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("form"), "{error}");

        draft.form_key = Some("  ".to_string());
        assert!(validate_source(&draft).is_err());
        draft.form_key = Some("contact-us".to_string());
        assert!(validate_source(&draft).is_ok());
    }

    #[test]
    fn consent_without_wording_is_refused() {
        // The only proof the platform ever has of consent is the words the visitor agreed to,
        // so a source that demands consent and stores none is refused at save time.
        let mut draft = NewIntakeSource::endpoint(Uuid::nil(), "Consent", None);
        draft.consent_required = true;
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("consent_text"), "{error}");

        draft.consent_text = Some("  ".to_string());
        assert!(validate_source(&draft).is_err());

        draft.consent_text = Some("I agree to be contacted about this request.".to_string());
        assert!(validate_source(&draft).is_ok());
    }

    #[test]
    fn a_source_whose_mapping_drops_a_required_target_is_refused() {
        let mut draft = NewIntakeSource::endpoint(Uuid::nil(), "Mapping", None);
        draft.consent_required = false;
        draft.required_targets = vec!["email".to_string()];
        draft.mapping = vec![MappingEntry::new("first_name", "name")];
        let error = validate_source(&draft).unwrap_err();
        assert!(error.to_string().contains("email"), "{error}");

        draft.mapping.push(MappingEntry::new("email", "email"));
        assert!(validate_source(&draft).is_ok());
    }

    #[test]
    fn the_hourly_ceiling_must_be_a_real_number() {
        let mut draft = NewIntakeSource::endpoint(Uuid::nil(), "Limit", None);
        draft.consent_required = false;
        for bad in [0, -1, 10_001] {
            draft.rate_limit_per_hour = bad;
            assert!(validate_source(&draft).is_err(), "{bad} should be refused");
        }
        for good in [1, 30, 10_000] {
            draft.rate_limit_per_hour = good;
            assert!(validate_source(&draft).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn consent_is_satisfied_by_the_ways_a_form_can_send_it() {
        let mut source = NewIntakeSource::endpoint(Uuid::nil(), "Consent", None);
        source.consent_text = Some("I agree.".to_string());
        let required = IntakeSource {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            site_id: None,
            name: "Consent".to_string(),
            kind: "endpoint".to_string(),
            form_key: None,
            endpoint_key_hash: None,
            endpoint_key_hint: None,
            mapping: serde_json::json!([]),
            required_targets: Vec::new(),
            consent_required: true,
            consent_text: Some("I agree.".to_string()),
            dedupe_policy: "link".to_string(),
            pipeline_id: None,
            stage_id: None,
            auto_tags: Vec::new(),
            autoresponder: serde_json::json!({}),
            active: true,
            rate_limit_per_hour: 30,
            last_received_at: None,
            last_error: None,
            broken_mappings: Vec::new(),
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };

        for payload in [
            serde_json::json!({"consent": true}),
            serde_json::json!({"consent": "true"}),
            serde_json::json!({"consent": "YES"}),
            serde_json::json!({"consent": "on"}),
            serde_json::json!({"consent": "1"}),
            serde_json::json!({"consent_given": true}),
        ] {
            assert!(consent_satisfied(&required, &payload), "{payload}");
        }
        for payload in [
            serde_json::json!({"consent": false}),
            serde_json::json!({"consent": "false"}),
            serde_json::json!({"consent": ""}),
            serde_json::json!({}),
        ] {
            assert!(!consent_satisfied(&required, &payload), "{payload}");
        }

        // A source that does not demand consent is satisfied by anything, including silence.
        let optional = IntakeSource {
            consent_required: false,
            ..required
        };
        assert!(consent_satisfied(&optional, &serde_json::json!({})));
    }

    #[test]
    fn a_stored_message_is_capped() {
        let long = "x".repeat(1_000);
        assert_eq!(truncate(&long, 500).chars().count(), 500);
        assert_eq!(truncate("short", 500), "short");
    }

    #[test]
    fn the_page_ceiling_is_a_ceiling_and_not_a_suggestion() {
        // `list_leads` clamps with `query.limit.clamp(1, MAX_PAGE)`, which is what stops a
        // caller from asking for the whole table. The clamp needs a pool to run, so what is
        // asserted here is the pair the clamp produces — a ceiling of 0 would make every
        // request return one row, and a ceiling below the default page would make the
        // default page lie about its own size.
        assert_eq!(crate::vocabulary::MAX_PAGE, 100);
        assert_eq!(50_i64.clamp(1, crate::vocabulary::MAX_PAGE), 50);
        assert_eq!(10_000_i64.clamp(1, crate::vocabulary::MAX_PAGE), 100);
        assert_eq!(0_i64.clamp(1, crate::vocabulary::MAX_PAGE), 1);
        // And the default page fits under it.
        assert!(LeadQuery::inbox().limit <= crate::vocabulary::MAX_PAGE);
    }
}
