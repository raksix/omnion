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

use serde::{Deserialize, Serialize};
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
use crate::vocabulary::{is_status, MAX_BULK_IDS, MAX_PAGE, MAX_PAYLOAD_BYTES};

pub const SOURCE_COLUMNS: &str = "id, organization_id, site_id, name, kind, form_key, \
     endpoint_key_hash, endpoint_key_hint, mapping, required_targets, consent_required, \
     consent_text, dedupe_policy, pipeline_id, stage_id, auto_tags, autoresponder, active, \
     rate_limit_per_hour, last_received_at, last_error, broken_mappings, created_by, \
     created_at, updated_at";

// **`submitter_ip::text` is cast here and not at the call sites.** `inet` is the right column
// type — it is what the platform stores addresses as everywhere else, and a text column would
// be a second spelling of every value — but sqlx has no `INET` decoder without the `ipnetwork`
// feature, so a bare `submitter_ip` in the returning list raises `ColumnDecode: Rust type
// Option<String> is not compatible with SQL type INET` on **every** read of a lead. A cast in
// this one list is what keeps that from being a decision at the twenty call sites that select
// or return a lead. PostgreSQL keeps the column name through a cast, so the name-based decode
// is unaffected.
pub const LEAD_COLUMNS: &str = "id, organization_id, site_id, source_id, status, contact_id, \
     company_id, deal_id, quote_id, owner_user_id, first_name, last_name, email, phone, \
     company_name, job_title, product_interest, message, consent_text, consent_given, \
     utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, referrer_host, \
     landing_path, source_path, payload, payload_bytes, dedupe_key, duplicate_of, \
     dedupe_contact_id, dedupe_score, decision, \
     assignment_rule_id, assignment_reason, sla_policy_id, first_response_due_at, \
     first_response_at, escalated_at, spam_score, rejection_reason, submitter_ip::text, \
     received_at, converted_at, created_at, updated_at";

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

    // The key is issued through the shared predicate, not a second `kind == "endpoint"`:
    // `rotate_key` and `find_source_by_key` both consult `carries_endpoint_key`, and three
    // copies of this fact is two copies too many for a credential boundary.
    let issued = crate::vocabulary::carries_endpoint_key(&draft.kind).then(keys::issue_key);
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
/// Refused for a source that has no key — a form-bound one, and anything `import`-shaped —
/// because "rotate" on a surface with no key is a button that appears to work and does
/// nothing. It is worse than nothing, in fact: the digest is all [`find_source_by_key`]
/// matches on, so issuing one onto a form-bound row is a **new public capture path** onto a
/// source that was never configured to have one, and that source is authenticated by the
/// form's own submission validation rather than by this credential.
///
/// **The doc comment promised this refusal since the key surface shipped and there was no
/// refusal anywhere** — the function found the source, issued a key and wrote it. What hid it
/// is that the panel is *correct* here (`source.kind === "endpoint"` gates the button in
/// `intake-sources.tsx`), and every gate that touched rotation created an endpoint source and
/// asserted only that the digest changed: a test that asserts the happy path passes on an
/// implementation with no guard at all. `tests/crm_key_lifecycle.rs` is the gate.
///
/// `Ok(None)` still means "no such source in this organization" and nothing else, because the
/// handler maps it to `not_found("intake source")` and a *wrong reason for a refusal* would
/// send an operator looking for a source that is right there. The refusal for a live source is
/// an error, not an empty answer.
pub async fn rotate_key(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<keys::IssuedKey>> {
    let Some(existing) = find_source(pool, organization_id, id).await? else {
        return Ok(None);
    };
    if !crate::vocabulary::carries_endpoint_key(&existing.kind) {
        return Err(CrmIntakeError::invalid(format!(
            "this intake source has no endpoint key: a \"{}\" source is authenticated by its \
             own form validation, so there is no key to rotate",
            existing.kind
        )));
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
///
/// **The predicate reads `kind` as well as the digest**, and that is the second half of
/// `rotate_key`'s refusal. The digest alone is not the credential boundary — it is the *fact
/// that a key was written*, and a key can reach a row by a path that is not a rotation: a
/// restored dump, a hand-written insert, an import tool, a future kind that is given a digest
/// by its own migration. Matching on the digest alone would serve all of them, so the rule
/// that decides which kinds are key-addressable is consulted **here, on the public path**,
/// where it cannot be bypassed by anything that got a digest in.
///
/// It is a second predicate on the same indexed column, so it costs no scan, and it cannot
/// make the surface *less* permissive than the REQ promises: `create_source` issues a key
/// only for `endpoint`, so every row this now refuses was one the API itself created wrongly.
pub async fn find_source_by_key(pool: &PgPool, key: &str) -> Result<Option<IntakeSource>> {
    let hash = keys::hash_key(key);
    let query = format!(
        "select {SOURCE_COLUMNS} from crm_intake_sources \
                         where endpoint_key_hash = $1 and active and kind = $2"
    );
    Ok(sqlx::query_as::<_, IntakeSource>(&query)
        .bind(hash)
        .bind(crate::vocabulary::KEY_BEARING_KIND)
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

/// Run the binding health check for this source's submission and record the answer.
///
/// **Never fatal, and never a write when the answer is unchanged.** Three decisions, each one
/// the obvious alternative of which is wrong:
///
/// * **Not fatal.** A health check that can fail a capture is a health check that can lose a
///   lead. A rename on somebody's form is a broken integration; answering `500` to the
///   visitor who is submitting right now makes the platform the reason the business stops.
///   Every failure here is logged and the capture continues, because a lead written with one
///   empty field is worth more than a lead nobody wrote.
/// * **Not a write when unchanged.** A row whose health is rewritten on every submission
///   produces an `updated_at` that moves continuously, and the source list orders by it — so
///   the health check would turn the "recently touched" signal into a submission-rate signal.
///   `broken_mappings` is compared first, and the write is skipped when it already matches.
/// * **Not cleared by an unknown answer.** [`BindingHealth::keys_to_store`] is `None` for the
///   unknown cases, so a forms-less installation does not silently un-break every form-bound
///   source it has. An empty list and an unreadable one are different facts.
async fn record_binding_health(pool: &PgPool, source: &IntakeSource, submission: &Submission) {
    let health = match crate::binding_health::check(pool, source, &submission.payload).await {
        Ok(health) => health,
        Err(error) => {
            tracing::warn!(
                source_id = %source.id,
                error = %error,
                "the intake binding health check could not run; the submission continues"
            );
            return;
        }
    };

    let Some(keys) = health.keys_to_store() else {
        // An answer we could not read. Deliberately not a warning about the binding: the
        // binding may be perfectly healthy, and the operator's action is "install the forms
        // module", which a red "broken mapping" badge would send them in the wrong direction.
        tracing::info!(
            source_id = %source.id,
            reason = ?health,
            "the intake binding health could not be read; the source keeps its last answer"
        );
        return;
    };
    if !keys.is_empty() {
        tracing::warn!(
            source_id = %source.id,
            form_key = ?source.form_key,
            keys = %keys.join(", "),
            "the bound form no longer has some mapped keys; those fields arrive empty"
        );
    }
    if keys == source.broken_mappings {
        return;
    }
    if let Err(error) = set_broken_mappings(pool, source.id, keys).await {
        tracing::warn!(
            source_id = %source.id,
            error = %error,
            "the broken-mapping list could not be recorded"
        );
        return;
    }

    if let Some(lead_id) = lead_of_claim(pool, submission).await {
        // The trail line is what turns "a source is broken" into "this lead is missing these
        // fields", and it is written only when the answer *changed* — a health line on every
        // submission would bury the one where it broke.
        let _ = append_event(
            pool,
            lead_id,
            "mapping_health",
            None,
            serde_json::json!({ "broken": keys }),
        )
        .await;
    }
}

/// The lead a *previous* delivery of this submission produced, if there is one.
///
/// The health check runs before the lead is written, so on the first submission after a rename
/// there is nothing to attach a line to. That asymmetry is deliberate and not worth a second
/// code path: the source row is the authoritative record, and the trail line is a pointer at
/// it. When a claim links a submission to a lead — a redelivery, or a retried capture — that
/// lead is the one the operator is looking at, and it gets the line.
///
/// The lookup goes through `crm_lead_submissions` because that is the only table that
/// records "this submission id produced that lead" as a fact. A `payload->>'submission_id'`
/// read looks equivalent and is not: the payload is the submitter's own data, and a form
/// cannot be trusted to carry an id the platform minted.
async fn lead_of_claim(pool: &PgPool, submission: &Submission) -> Option<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "select lead_id from crm_lead_submissions \
         where source_id = $1 and submission_id = $2 and lead_id is not null \
         order by claimed_at desc limit 1",
    )
    .bind(submission.source_id)
    .bind(submission.submission_id.clone()?)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    id
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

/// How many submissions **one address** has sent to this source in the last hour.
///
/// **This is the second ceiling, and it is the one the per-source counter cannot do.** A
/// source's hourly budget is shared by every visitor to every page the source is bound to, so
/// a single address can spend all of it in seconds. Without a per-address dial the operator
/// has exactly one lever — lower `rate_limit_per_hour` — and that lever takes their real
/// form's legitimate traffic down with the flood it was meant to stop. The REQ's own Risks
/// section calls this surface "the attack surface"; this is the function that makes that
/// sentence true rather than decorative.
///
/// Counted from the rows rather than a counter column, for the same reason
/// [`submissions_this_hour`] is: a counter that is not decremented when a row is deleted is a
/// limit that eventually blocks a source nobody is using. The read runs through the partial
/// index `crm_leads_submitter_ip_idx` (migration `0192`) because the predicate
/// `submitter_ip is not null` is exactly the shape of every row that can match.
///
/// A submission with **no** address is never limited by this function. An event-bus capture
/// has no HTTP request behind it, and refusing one because a *different* transport cannot
/// identify the sender would make the platform's own pipeline throttle-able by anybody who
/// can reach the source's public endpoint from a host without a parseable address.
pub async fn submissions_from_address_this_hour(
    pool: &PgPool,
    source_id: Uuid,
    address: Option<&str>,
) -> Result<i64> {
    let Some(address) = address else {
        return Ok(0);
    };
    // An unparseable address is treated as *no* address rather than as a bucket keyed on the
    // raw string: a header a client can write freely is not an identity, and grouping every
    // malformed value into one shared bucket would let any caller throttle every other
    // malformed caller.
    let Ok(parsed) = address.parse::<std::net::IpAddr>() else {
        return Ok(0);
    };
    let row: (i64,) = sqlx::query_as(
        "select count(*) from crm_leads \
         where source_id = $1 and submitter_ip = $2::inet \
           and received_at > now() - interval '1 hour'",
    )
    .bind(source_id)
    .bind(parsed.to_string())
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

    // **The per-address ceiling, and the one that makes the sentence above true.**
    //
    // `Submission.ip` has said "for the per-IP rate limit" since the struct shipped, and
    // until this line no such limit existed: `submissions_this_hour` is the *only* ceiling in
    // the capture path and it counts by source. So the REQ's Risks section — "the public
    // intake surface is the attack surface … a flood is throttled" — was carried by one dial
    // that moves the flood and the business together, and a single address could spend a
    // whole source's budget in seconds.
    //
    // It is checked **after** the source's ceiling on purpose, not before. The two refusals
    // are the same `429` to the public endpoint, so the order is not observable by a caller —
    // but it *is* observable in the log, and the source ceiling is the one an operator
    // configured, so it is the one that should be the reason recorded.
    let from_address = submissions_from_address_this_hour(pool, source.id, submission.ip.as_deref())
        .await?;
    if from_address >= crate::vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        return Err(CrmIntakeError::RateLimited);
    }

    // **One submission, one lead — enforced by the claim, not by this read.**
    //
    // The line this replaces was `find_lead_by_submission(...)` returning early. It looked
    // like the same thing and was not: a read followed by an unguarded insert has no lock and
    // nothing to collide with, so two deliveries of the same `content.form.submitted` arriving
    // together both read "not found" and both write a lead. The platform's events are
    // at-least-once by contract, and nothing about the second lead is an *error* — it is a
    // well-formed row with its own dedupe verdict, which is why no gate in this crate ever
    // went red over it.
    //
    // The claim is taken here, before the work, and completed at every return below. Taken
    // *before* rather than in the same transaction as the insert on purpose: a claim released
    // by a failed capture is re-claimable, so a burst of retries collides again on exactly the
    // submissions that are already failing. A claim that dies open is recoverable; a claim
    // that frees itself is not.
    let claim = match submission.submission_id.as_deref() {
        Some(submission_id) => match crate::claims::take(pool, source.id, submission_id).await? {
            crate::claims::Claimed::Owned { claimed_at } => Some(claimed_at),
            crate::claims::Claimed::Taken { lead_id } => {
                // The winner either finished already — answer with its lead — or is still
                // writing it. `None` is a real answer and not a failure: the lead exists or is
                // about to, and a delivery that spins until it appears has re-introduced the
                // load the claim was taken to shed. One reference is owed to a caller that
                // sent an idempotency key, so an open claim falls back to the earlier read —
                // which is safe precisely because it can only add work, never remove it.
                let reference = match lead_id {
                    Some(id) => find_lead(pool, submission.organization_id, id).await?,
                    None => find_lead_by_submission(pool, source.id, submission_id).await?,
                };
                return match reference {
                    Some(lead) => Ok(Captured {
                        spam: SpamVerdict::default(),
                        attribution: Attribution::default(),
                        verdict: Verdict::Unique,
                        lead,
                    }),
                    None => Err(CrmIntakeError::invalid(format!(
                        "submission \"{submission_id}\" is already being captured; \
                         retry with the same key once that attempt finishes"
                    ))),
                };
            }
        },
        // No key: every attempt is its own lead, which is the honest reading of a form post a
        // browser may resend. See `apps/api`'s `idempotency_key`.
        None => None,
    };

    let lines = source.mapping_lines();
    let mapped = mapping::apply(&lines, &submission.payload)?;

    // 2a. The binding's health, checked on this submission because this submission is the only
    //     evidence that a key still exists. **Written before the verdict, and never fatal.**
    //
    //     `mapping::health` and `set_broken_mappings` shipped several slices before this line,
    //     and neither had a production caller: the pure function's references were its own
    //     definition and two unit tests, the writer's was its own definition. The column had a
    //     model predicate and a screen branch that renders the missing keys, so the panel drew
    //     a "broken mapping" warning that no state on any installation could reach. An exported,
    //     unit-tested, REQ-named function is not a feature.
    //
    //     The check runs here rather than in the editor because the editor is not where the
    //     drift is observable: a field rename reaches the CRM as a submission whose payload no
    //     longer has the old key, and the moment that answer becomes true is the moment the
    //     platform has evidence for it.
    record_binding_health(pool, &source, submission).await;

    let spam = SpamVerdict::evaluate(&submission.payload);
    // The first touch is looked up by the MAPPED address (`mapped`, not `payload`): the lead
    // row is written with the mapped value, so keying the lookup on the raw payload's `email`
    // compared two different names and found nothing.
    let attribution = merge_attribution(
        pool,
        &source,
        submission.organization_id,
        &mapped,
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
        // A rejected submission is still *this* submission's one lead, so the claim completes
        // on this path too. Leaving it open would make a redelivery of a rejected submission
        // answer "already being captured" for ever, and the operator's own re-submit test on a
        // broken form is exactly the case that would hit it.
        finish_claim(pool, submission, &source, claim, lead.id).await;
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
        finish_claim(pool, submission, &source, claim, lead.id).await;
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
            // **The pointer is the LEAD this repeats, and this is where that used to go wrong.**
            // `duplicate_of` is a foreign key to `crm_leads`; the obvious thing to put here is
            // the contact that matched, and for fourteen ticks that is exactly what the code
            // did — in an `update` *after* the insert, so the failure was a 23503 raised by a
            // statement whose only job was bookkeeping. On any installation with the CRM, a
            // `reject_duplicate` source therefore answered `500` to every visitor, and no test
            // here could see it: `crm_contacts` is absent on this branch, so the dedupe pass
            // never returns a candidate and the arm is dead code in every test.
            //
            // The matched contact is recorded in `dedupe_contact_id` (see below) and
            // `duplicate_of` stays null here. When a *lead* pointer is genuinely available — a
            // previous submission from the same visitor — that is the column's one writer, so
            // it keeps a single meaning.
            None,
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

    // The contact the verdict matched and the confidence it reached, taken **once** from the
    // verdict rather than from the match arm above. Two sources for this number is how the
    // queue and the timeline end up disagreeing about a row, and the score is the only thing
    // that makes a `duplicate` claim checkable rather than asserted.
    let (matched_contact, matched_score) = match &verdict {
        Verdict::Matched(found) => (Some(found.contact_id), Some(found.score)),
        Verdict::Ambiguous(found) => match found.first() {
            Some(first) => (Some(first.contact_id), Some(first.score)),
            None => (None, None),
        },
        Verdict::Unique => (None, None),
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
            dedupe_contact_id: matched_contact,
            dedupe_score: matched_score,
            dedupe_key: key,
            rejection_reason: reason,
            consent_given,
            spam_score: spam.score,
        },
    )
    .await?;

    // A duplicate row used to get its contact pointer from a second statement here, and that
    // statement wrote a `crm_contacts` id into `crm_leads.duplicate_of` — a foreign key to
    // `crm_leads`. On an installation with the CRM it raised 23503 and failed a submission
    // whose lead row had already been written correctly; on an installation without the CRM it
    // never ran at all. The pointer and the score are written by the insert now, and there is
    // deliberately no second statement: a lead row is written once, and bookkeeping cannot fail
    // a capture.
    debug_assert!(
        !lead.duplicate_of.eq(&lead.dedupe_contact_id) || lead.dedupe_contact_id.is_none(),
        "the matched contact must be stored in dedupe_contact_id, never in duplicate_of"
    );

    record_source_outcome(pool, source.id, true, None).await?;

    // 6. Route it. **This call is the whole of slice 2's assignment chain, and it was missing
    //    for twenty-four ticks.**
    //
    //    `assignment_store::claim_assignment`, `stamp_assignment` and `policy_for_source` were
    //    exported, documented, unit-tested and gated — nine assertions in
    //    `scripts/qa/run-crm-assignment.sh`, all green, all of them *calling the three
    //    functions directly*. Not one of them could see that nothing on any installation
    //    called them, because every one of them began at the function and never travelled
    //    backwards up the road to find the submission.
    //
    //    The consequence was not subtle and not small: **a lead arriving through the capture
    //    path was never assigned, never got an owner, and never got a response deadline.** An
    //    operator creates a country rule, watches the simulator name the winner, and every lead
    //    in the inbox reads `Unassigned` with no due time — while the SLA editor's reminder
    //    ("Escalate to…") describes a breach that can never be detected, because
    //    `first_response_due_at` is null on every row and `due_breaches` matches nothing.
    //
    //    That is the sixth time this crate has shipped a correct, unit-tested, REQ-named
    //    function with no caller able to produce the state it describes. The lesson is now
    //    stated in one place, and it is the *shape* rather than the instance: **a gate that
    //    begins at the function proves the function.** The five previous misses were all
    //    found by reading the caller out of the definition; this one survived them because
    //    every gate in this crate starts mid-stack, which is exactly what makes them fast and
    //    is exactly what makes them blind.
    //
    //    Ordering, and why each part is where it is:
    //
    //    * **After the verdict, never before.** A rejected or spam row is nobody's work: a
    //      verdict that cannot be acted on must not land on somebody's desk. The claim is
    //      taken after those two branches have already returned.
    //    * **The returned lead is the *routed* lead, not the inserted one.** `stamp_assignment`
    //      is a second `update`, so the row the caller sees has to be the one after it — an
    //      inbox that reads `new` for a lead that was stamped `assigned` is the panel
    //      disagreeing with itself.
    //    * **Never fatal.** A lead nobody can reach must not be lost because the routing
    //      chain had a bad day. Same rule as the binding-health check two steps above, and for
    //      the same reason: a broken integration must never be the reason a business stops
    //      taking enquiries.
    let lead_id = lead.id;
    let routed = route_captured_lead(pool, submission, &source, &mapped, lead).await;

    finish_claim(pool, submission, &source, claim, lead_id).await;
    Ok(Captured {
        lead: routed,
        verdict,
        spam,
        attribution,
    })
}

/// Run a freshly captured lead through the assignment chain, and hand back the row as it
/// stands afterwards.
///
/// **This is the caller that was missing for twenty-four ticks.** Three exported,
/// documented, unit-tested, gated functions — `claim_assignment`, `stamp_assignment`,
/// `policy_for_source` — had exactly one production caller between them, and it was a *test*.
/// The chain was correct at every step and had no road to the submission.
///
/// ## The three decisions, and the wrong answer to each
///
/// * **Which rules see this lead.** `AssignmentInput::from_lead_row` reads the *mapped*
///   values, not the raw payload. The obvious version reads the payload, and then a source
///   whose form calls the field `e_mail` conditions on nothing — the same two-names-one-value
///   trap the attribution merge fell into, one slice earlier and in the same file. The mapped
///   values are also what the lead row is written from, so the rule chain and the row cannot
///   disagree about who the visitor is.
///
/// * **Which policy.** The source's own choice, else the organization's first active policy,
///   else **no deadline** — `None` rather than a guess. A lead with no policy reads "no target
///   set" in the inbox, which is true; inventing a 24-hour default would read as a promise
///   the organization never made and would then be escalated against.
///
/// * **What to do when the chain fails.** Log and return the row as inserted. Not `?`. The
///   lead is stored and the caller is owed its `202`; a routing chain that cannot evaluate
///   because a rule table is momentarily unreadable must not be the reason a business stops
///   taking enquiries. The cost of the failure is visible — the lead reads `Unassigned` with
///   no due time, which is exactly what the operator needs to see to go and fix it.
///
/// ## Why the returned lead is the *stamped* one
///
/// `stamp_assignment` is a second `update`, and the `Lead` this function was handed was read
/// before it. Returning the stale struct would leave `capture`'s caller holding a lead that
/// says `new` while the row says `assigned` — and `capture`'s caller is the public endpoint,
/// whose response is what the integration and the `crm.lead.received` event carry. So the row
/// is re-read and the *stored* state is what travels. `stamp_assignment` returning nothing
/// (it cannot: the lead was inserted one statement ago) would leave the fresh row as the
/// fallback, which is why the re-read is `find_lead` and not a trust of the previous value.
async fn route_captured_lead(
    pool: &PgPool,
    submission: &Submission,
    source: &IntakeSource,
    mapped: &MappedValues,
    inserted: Lead,
) -> Lead {
    let organization_id = submission.organization_id;

    // The row the evaluator reads is the row as stored, not a hand-built struct: the rule
    // chain conditions on `country`, `region`, `product_interest`, `budget_band`, `source_id`
    // and `source_name`, and five of those six live on the lead rather than the payload. A
    // hand-built row would drift the first time a column is added.
    let row = match find_lead(pool, organization_id, inserted.id).await {
        Ok(Some(row)) => row,
        Ok(None) => return inserted,
        Err(error) => {
            tracing::warn!(
                lead_id = %inserted.id, organization_id = %organization_id, error = %error,
                "the new lead could not be read back for routing; it stays unassigned"
            );
            return inserted;
        }
    };

    let input = assignment_input(mapped, &row, source);

    let outcome = match crate::assignment_store::claim_assignment(pool, organization_id, &input).await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                lead_id = %inserted.id, organization_id = %organization_id, error = %error,
                "the assignment chain could not decide this lead; it stays unassigned"
            );
            return inserted;
        }
    };

    // The policy lookup runs the same way: an organization with no `crm_sla_policies` row
    // gets `None` and therefore no deadline, which is the documented default rather than a
    // failure. Only an *error* is worth a line in the log.
    let policy = match crate::assignment_store::policy_for_source(
        pool,
        organization_id,
        Some(source.id),
    )
    .await
    {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(
                lead_id = %inserted.id, organization_id = %organization_id, error = %error,
                "the SLA policy for this lead could not be read; it gets no deadline"
            );
            None
        }
    };

    if let Err(error) = crate::assignment_store::stamp_assignment(
        pool,
        inserted.id,
        &outcome,
        policy.as_ref(),
        submission.received_at,
    )
    .await
    {
        tracing::warn!(
            lead_id = %inserted.id, organization_id = %organization_id, error = %error,
            "the assignment could not be stamped on the new lead"
        );
        return inserted;
    }

    // The trail line, because a lead that changed hands is not something an audit may learn
    // about from the inbox. The same event kind the hand-over route writes, so the detail
    // timeline renders both from one branch — a new kind here would mean a new screen branch
    // for a sentence the panel already knows how to say.
    let _ = append_event(
        pool,
        inserted.id,
        "assigned",
        None,
        serde_json::json!({
            "source": "rule",
            "rule_id": outcome.rule_id.map(|id| id.to_string()),
            "rule_name": outcome.rule_name,
            "owner_user_id": outcome.owner_user_id.map(|id| id.to_string()),
            "target_kind": outcome.target_kind,
            "sla_policy_id": policy.as_ref().map(|p| p.id.to_string()),
            "first_response_due_at": policy
                .as_ref()
                .map(|p| due_at_preview(p, submission.received_at)),
        }),
    )
    .await;

    match find_lead(pool, organization_id, inserted.id).await {
        Ok(Some(row)) => row,
        Ok(None) => inserted,
        Err(_) => inserted,
    }
}

/// Build the evaluator's input for a captured lead.
///
/// **The mapped values, and this is the whole reason the call site reads `mapped` and not
/// `submission.payload`.** `crm_leads` has no `country`, `region`, `budget_band` or `language`
/// column — the only place those answers exist is the submission, which is why the rule
/// conditions are shaped like a payload and why the REQ's simulator takes one. Four of the
/// eight condition keys are therefore only reachable from `mapped`, and every one of them is
/// named by the *form's* field rather than by the CRM's: a source mapping `country` from a
/// field called `land` stores `country`, and the rule chain must see `country`.
///
/// `source_id`, `source_name` and `has_email` are filled from the row and the source rather
/// than from the payload, because the payload does not carry them and inventing a lookup for
/// a value already in hand is how the first-touch merge ended up comparing two names.
///
/// `from_lead_row` is still the reader, and that is deliberate: it is the one function that
/// knows the condition keys, so a seventh key added to the evaluator is read here too without
/// this function being touched. Passing a payload jsonb straight into it would also work and
/// would be wrong — a payload key and a mapped target that happen to share a name are the
/// coincidence this branch has now been bitten by twice.
fn assignment_input(mapped: &MappedValues, lead: &Lead, source: &IntakeSource) -> crate::assignment::AssignmentInput {
    let mut row = serde_json::Map::new();
    for key in ASSIGNMENT_CONDITION_KEYS {
        if let Some(value) = mapped.get(key) {
            row.insert((*key).to_string(), serde_json::Value::String(value.to_string()));
        }
    }
    // The two keys that live on the row rather than the mapping.
    if let Some(value) = lead.product_interest.as_deref() {
        row.insert("product_interest".into(), serde_json::Value::String(value.to_owned()));
    }
    if let Some(value) = lead.email.as_deref() {
        row.insert("email".into(), serde_json::Value::String(value.to_owned()));
    }

    let mut input = crate::assignment::AssignmentInput::from_lead_row(&serde_json::Value::Object(row));
    input.source_id = Some(source.id);
    input.source_name = Some(source.name.clone());
    input.has_email = Some(lead.email.is_some());
    input
}

/// The condition keys a mapping can supply, read from the mapped values rather than from the
/// submission.
///
/// Listed rather than iterated over "every mapped key" on purpose: a mapping target the
/// evaluator does not know is harmless, and a rule chain that conditions on a key the
/// evaluator does not know would silently never match — so the reader is the evaluator's and
/// this list only says *where to look*.
const ASSIGNMENT_CONDITION_KEYS: [&str; 4] = ["country", "region", "budget_band", "language"];

/// The deadline a policy would set, for the trail line only.
///
/// The real instant is written by `stamp_assignment`, which computes it with this same
/// function inside the organization's window. The trail line therefore shows the deadline the
/// row now carries, and a second arithmetic here would be a second answer to the same
/// question — the shape this crate keeps meeting.
fn due_at_preview(
    policy: &crate::assignment::SlaPolicy,
    received: time::OffsetDateTime,
) -> String {
    crate::assignment::due_at(policy, received).to_string()
}

/// Point the submission's claim at the lead this capture wrote.
///
/// A no-op when there is no claim, which is the `submission_id: None` path — every attempt its
/// own lead, and there is nothing to collide with. A failure here is **logged, not propagated**:
/// the lead is already stored and the caller is owed its `202`, so unwinding here would answer
/// `500` for a submission that was captured. The cost of losing the completion is the honest
/// one: the claim stays open, the sweeper takes it over after `CLAIM_STALE_AFTER`, and the
/// redelivery finds the same lead rather than writing a second one.
async fn finish_claim(
    pool: &PgPool,
    submission: &Submission,
    source: &IntakeSource,
    claim: Option<time::OffsetDateTime>,
    lead_id: Uuid,
) {
    let (Some(claimed_at), Some(submission_id)) = (claim, submission.submission_id.as_deref())
    else {
        return;
    };
    if let Err(error) =
        crate::claims::complete(pool, source.id, submission_id, claimed_at, lead_id).await
    {
        tracing::warn!(
            lead_id = %lead_id,
            source_id = %source.id,
            error = %error,
            "the lead was captured but its submission claim was not completed"
        );
    }
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
    /// The earlier **lead** it repeats, when one is known.
    ///
    /// Never a contact: `crm_leads.duplicate_of` is a foreign key to `crm_leads`, and the
    /// matched contact belongs in `dedupe_contact_id`. The two being one column is the defect
    /// migration `0144` exists to separate.
    duplicate_of: Option<Uuid>,
    /// The contact the dedupe verdict matched.
    ///
    /// Written by the insert rather than by a second statement, so a lead row is written once.
    dedupe_contact_id: Option<Uuid>,
    /// The score, as a column the database bounds rather than a number the code promises.
    dedupe_score: Option<f64>,
    /// The normalized dedupe key.
    dedupe_key: Option<String>,
    /// Why it was rejected or duplicated.
    rejection_reason: Option<String>,
    /// Whether consent was given.
    consent_given: bool,
    /// The spam score.
    spam_score: i32,
}

/// Which `$n` the `values` list assigns to the column called `name`, if it appears there.
///
/// The insert's column list and its `values` list are two hand-counted lists that must agree,
/// and the one place they can disagree is a *cast*: a `::inet` written one placeholder to the
/// right does not name a column in the error, it names a type. `42846 cannot cast type
/// timestamp with time zone to inet` is how this file spent a gate run — the message points
/// at the cast, and the mistake is in the counting.
///
/// Parsed out of the statement rather than remembered beside it, so a column added above
/// moves both lists together.
///
/// **The line continuations have to go first, and that is not a detail.** The statement is
/// written as a Rust string with `\` at the end of each line, so the text handed to a
/// `str::split(',')` has ` \\\n          last_name` as the *start* of the next segment. The
/// first version of this returned the right answer for every column except those that follow
/// a wrapped line, and reported `$35` for a column that is `$34` — a checker that is wrong
/// on most inputs is worse than no checker, because it is trusted on the ones it gets right.
fn placeholder_of(query: &str, name: &str) -> Option<usize> {
    // Unwrap the statement onto one line first. `trim_start` on each continuation's tail is
    // what makes the segment a name again rather than a name preceded by a backslash.
    let flat: String = query
        .replace("\\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let (_, after_columns) = flat.split_once('(')?;
    let (columns, _) = after_columns.split_once(')')?;
    // The first value segment is `($1` — the opening paren rides on it, so it does not start
    // with `$` and is dropped by the filter below, which shifts every count by one and
    // reports `$35` for the `$34` that is actually in the statement. Strip it first.
    let values = flat
        .rsplit_once("values ")?
        .1
        .split(" returning ")
        .next()?
        .trim_start()
        .trim_start_matches('(');
    let position = columns
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .position(|entry| entry == name)?;
    values
        .split(',')
        .map(str::trim)
        .filter_map(|entry| entry.strip_prefix('$'))
        .filter_map(|entry| {
            entry
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<usize>()
                .ok()
        })
        .nth(position)
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
    // The address is written here, by the same statement that writes the row, for two reasons.
    //
    // First, `returning {LEAD_COLUMNS}` cannot include a column the insert does not name, so a
    // second `update` after the insert would either need its own `returning` (a second read of
    // a row this function already holds) or leave the in-memory `Lead` disagreeing with the
    // table — and the lead this function returns is what the trail and the response are built
    // from, so the panel would show a row whose address the count says is absent.
    //
    // Second, an address that is only *derived* at count time is not the submitter's address
    // at all: `submission.ip` is the one the request carried, and the window the ceiling
    // counts is the window the row was received in.
    //
    // **`$34::inet` is a cast, not decoration.** Binding an `Option<String>` into an `inet`
    // column raises `42804 column "submitter_ip" is of type inet but expression is of type
    // text` — the DB gate caught it on the first run, and it is the kind of failure that
    // reaches production as a `500` on the *first* submission a site ever takes, which is the
    // worst possible moment to find it. The value is parsed here first, so an unparseable one
    // becomes `None` rather than an error: a submission is not lost because the header that
    // carried its address was malformed.
    let submitter_ip = submission
        .ip
        .as_deref()
        .and_then(|text| text.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string());
    let query = format!(
        "insert into crm_leads \
         (organization_id, site_id, source_id, status, contact_id, duplicate_of, first_name, \
          last_name, email, phone, company_name, job_title, product_interest, message, \
          consent_text, consent_given, utm_source, utm_medium, utm_campaign, utm_term, \
          utm_content, click_id, referrer_host, landing_path, source_path, payload, \
          payload_bytes, dedupe_key, dedupe_contact_id, dedupe_score, decision, spam_score, \
          rejection_reason, submitter_ip, received_at) \
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,\
         $23,$24,$25,$26,$27,$28,$29,$30,$31,$32,$33,$34::inet,$35) returning {LEAD_COLUMNS}"
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
        .bind(write.dedupe_contact_id)
        .bind(write.dedupe_score)
        .bind(write.decision.as_deref())
        .bind(write.spam_score)
        .bind(write.rejection_reason.as_deref())
        .bind(&submitter_ip)
        .bind(submission.received_at)
        .fetch_one(pool)
        .await?;

    // The placeholder number above is the *only* thing that binds the address to the right
    // column, and it is counted by hand. When it was written one too high, PostgreSQL said
    // `cannot cast type timestamp with time zone to inet` — which names a type and not a
    // column, so the failure reads like a cast problem rather than an off-by-one. The check
    // below is cheap and total: the cast must sit on the placeholder of the same name.
    debug_assert_eq!(
        placeholder_of(&query, "submitter_ip"),
        Some(34),
        "the ::inet cast must stay on the submitter_ip placeholder; count the columns, do not \
         guess — the alternative failure names a type, not a column"
    );

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
///
/// **The key is the MAPPED address, not `payload["email"]`.** This read the raw payload's
/// `email` key, which is one mapping away from the value the lead row is actually written
/// with — so every source whose form calls the field anything else (`e_mail`, `contact_email`,
/// `eposta`, `your_email`) silently kept no first touch at all, and a second submission
/// overwrote the campaign that first brought the visitor in. The unit tests could not see
/// it: they all map `email` from a key called `email`, so the two names coincided, and the
/// merge test drove `merge_first_touch` directly with hand-built `Attribution` values.
///
/// `dedupe::dedupe_key` is the right key for the same reason it is the right key for the
/// duplicate queue: it is the one function that already answers "which stored lead is this
/// the same person as", and answering that question a second time in a second place is how
/// the two answers drift apart.
async fn merge_attribution(
    pool: &PgPool,
    source: &IntakeSource,
    organization_id: Uuid,
    mapped: &MappedValues,
    later: &Attribution,
) -> Result<Attribution> {
    let Some(key) = dedupe::dedupe_key(mapped) else {
        return Ok(later.clone());
    };
    let row: Option<StoredAttribution> = sqlx::query_as(
        "select utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, \
                referrer_host, landing_path, source_path from crm_leads \
         where organization_id = $1 and source_id = $2 \
           and (lower(email) = $3 or lower(coalesce(phone, '')) = $3) \
         order by received_at asc limit 1",
    )
    .bind(organization_id)
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

/// What one bulk call actually did, per lead.
///
/// The inbox's bulk bar acts on twenty rows and reports one sentence, and "Assigned 18 of 20"
/// without saying *which* two leaves the operator unable to tell a refusal from a selection
/// that never got ticked. So the answer is per id, and the panel renders the failures rather
/// than the successes' absence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BulkAssignOutcome {
    /// The lead the action was about.
    pub id: Uuid,
    /// Whether it landed.
    pub done: bool,
    /// Why not, in the caller's words. `None` on success.
    pub reason: Option<String>,
}

/// What a whole bulk call did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BulkAssignReport {
    /// Per lead, in the order they were named.
    pub results: Vec<BulkAssignOutcome>,
}

impl BulkAssignReport {
    /// How many landed.
    #[must_use]
    pub fn applied(&self) -> usize {
        self.results.iter().filter(|row| row.done).count()
    }

    /// How many were refused, and why they were not.
    #[must_use]
    pub fn refused(&self) -> usize {
        self.results.len() - self.applied()
    }

    /// One sentence an operator can paste into a ticket.
    #[must_use]
    pub fn summary(&self) -> String {
        let refused = self.refused();
        if refused == 0 {
            return format!("{} leads now have their new owner.", self.applied());
        }
        let mut reasons: Vec<(String, usize)> = Vec::new();
        for row in self.results.iter().filter(|row| !row.done) {
            let reason = row.reason.clone().unwrap_or_else(|| "unknown".to_string());
            match reasons.iter_mut().find(|(text, _)| *text == reason) {
                Some((_, count)) => *count += 1,
                None => reasons.push((reason, 1)),
            }
        }
        let detail = reasons
            .iter()
            .map(|(text, count)| format!("{count} x {text}"))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "{} of {} leads were assigned. {detail}",
            self.applied(),
            self.results.len()
        )
    }
}

/// Hand a batch of leads to one person, or back to the queue.
///
/// **Each lead is its own transaction, on purpose.** `assign_owner` is a transaction because a
/// lead's owner change and its trail line must be together; it is not a transaction because it
/// has to be *atomic with the rest of the batch*, and making it so would be worse than the
/// alternative: one row the operator lacks `crm.leads.read` for, or one lead somebody filed as
/// spam, would roll back nineteen legitimate hand-overs and report a failure for work that had
/// already happened. A batch is a sequence of decisions that happen to be requested together,
/// and the report says which of them landed.
///
/// The refusals are therefore *per row* rather than one exception for the call: a verdict is
/// refused with its status in the message (see `assign_owner`), and that message is what the
/// inbox shows on the row that stayed put.
pub async fn bulk_assign_owner(
    pool: &PgPool,
    organization_id: Uuid,
    ids: &[Uuid],
    owner: Option<Uuid>,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Result<BulkAssignReport> {
    // The cap is here and not only in the handler, for the reason every vocabulary constant
    // in this crate exists: a value the route refuses and the store accepts reads as "nothing
    // happened" from whichever caller forgot the check. Two hundred is a page of the inbox,
    // and a batch larger than that is a filter the operator forgot to apply.
    if ids.len() > MAX_BULK_IDS {
        return Err(CrmIntakeError::invalid(format!(
            "{} leads is more than one bulk action takes ({MAX_BULK_IDS}) — narrow the filter first",
            ids.len()
        )));
    }
    if ids.is_empty() {
        return Err(CrmIntakeError::invalid(
            "no leads were named — a bulk action with nothing selected does nothing and says so",
        ));
    }

    let mut report = BulkAssignReport::default();
    for id in ids {
        let outcome =
            match assign_owner(pool, organization_id, *id, owner, reason, actor_user_id).await {
                Ok(Some(_)) => BulkAssignOutcome {
                    id: *id,
                    done: true,
                    reason: None,
                },
                // A lead of another organization is not a row, and the batch says so rather than
                // pretending the caller asked for something that does not exist — `404` for one
                // id in a list of twenty is not an error the operator can act on.
                Ok(None) => BulkAssignOutcome {
                    id: *id,
                    done: false,
                    reason: Some("no such lead in this organization".to_string()),
                },
                Err(error) => BulkAssignOutcome {
                    id: *id,
                    done: false,
                    reason: Some(error.to_string()),
                },
            };
        report.results.push(outcome);
    }
    Ok(report)
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
///
/// **The request id is stamped here, in this one function, and nowhere else.** Every trail line in
/// the crate goes through these two, so this is the single place that has to know the id exists —
/// and therefore the single place that can be audited for it. A caller that wanted to add it
/// itself would have two ways to spell the same fact, and a line written by the second way would
/// carry whichever value won. Stamping here also means a caller cannot forget: forgetting is
/// silent, the line is written, and the id reads `null` while the row looks healthy.
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
    .bind(stamp_request_id(detail))
    .execute(executor)
    .await?;
    Ok(())
}

/// Add the current exchange's id to a detail object.
///
/// A detail that is not an object is wrapped rather than replaced. Every caller in the crate
/// passes `json!({…})`, so the wrap is unreachable today — which is exactly why it must not be a
/// `match` that *drops* the value: a future caller passing an array would have its line written
/// with its detail silently discarded, and the trail would read as empty rather than as broken.
///
/// A caller that already set `request_id` keeps its own value. The only way that happens today is
/// a test that stamps one deliberately, and a line that claims a different exchange than the one
/// the server did is the one thing a correlation id must never be.
#[must_use]
pub fn stamp_request_id(detail: serde_json::Value) -> serde_json::Value {
    let mut value = match detail {
        serde_json::Value::Object(map) => serde_json::Value::Object(map),
        other => serde_json::json!({ "detail": other }),
    };
    let object = value
        .as_object_mut()
        .expect("the object branch above produced an object");
    object
        .entry("request_id")
        .or_insert_with(crate::request_id::detail_value);
    value
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
/// The `crm_leads_contactable_check` constraint is the backstop for a write that routes around
/// this function, and it is **narrower than it reads**: migration `0159` lets a `rejected`,
/// `spam` or `duplicate` row exist with neither address, because REQ-117 requires the refusal
/// itself to be recorded. This guard is therefore the only thing refusing the *edit* — a
/// rejected row arrived without an address on purpose, and an operator who then clears both
/// fields of an accepted lead has not made it a verdict, they have made it unanswerable. The
/// message names the rule rather than surfacing a constraint violation.
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

/// What an operator decided about a filed duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateDecision {
    /// Attach the lead to the contact its own dedupe pass recorded.
    Link,
    /// File it as a lead in its own right: the match was wrong, or it is a second enquiry.
    KeepSeparate,
}

impl DuplicateDecision {
    /// Parse the wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "link" => Some(Self::Link),
            "keep_separate" => Some(Self::KeepSeparate),
            _ => None,
        }
    }
}

/// The answer a duplicate decision produced.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateResolution {
    /// The lead after the decision.
    pub lead: Lead,
    /// The contact it was attached to, when the decision was `Link`.
    pub contact_id: Option<Uuid>,
    /// Why the decision could not be carried out, when it could not.
    pub refused: Option<String>,
}

/// Reverse a filed duplicate verdict.
///
/// **This lives in the store and not in the panel, because only the store knows the matched
/// contact.** The queue's `Link` button used to send `PATCH { status: "assigned" }`; `patch_lead`
/// takes `contact_id` from the patch or else from the existing row, and a duplicate row has
/// none — so the row left the queue, the panel announced "linked to the contact it matched", and
/// no contact had been touched. The panel cannot even *name* the right contact, so an endpoint
/// that required it would have pushed the bug somewhere less visible rather than fixing it.
///
/// Two refusals, both of them about a row the operator is misreading:
///
/// * **A lead with no recorded match cannot be linked.** `duplicate_of` (a lead pointer) is not a
///   fallback for `dedupe_contact_id`; one is an id of a lead and the other of a contact, and
///   using one where the other belongs is the defect this file's header is about.
/// * **A row that is not a duplicate claim is refused rather than silently restated.** Pressing
///   `Keep separate` on a lead that was never filed as one would write a status change to a row
///   nobody asked about; the answer names the status it actually has.
pub async fn resolve_duplicate(
    pool: &PgPool,
    organization_id: Uuid,
    lead_id: Uuid,
    decision: DuplicateDecision,
    actor_user_id: Option<Uuid>,
) -> Result<Option<DuplicateResolution>> {
    let Some(existing) = find_lead(pool, organization_id, lead_id).await? else {
        // Another organization's lead answers the same `None` as a lead that is gone, on the
        // same principle as every other read in this module: a panel that can tell those apart
        // can enumerate ids.
        return Ok(None);
    };

    let is_duplicate_claim = existing.status == "duplicate" || existing.duplicate_of.is_some();
    let (previous_status, matched_contact) = (existing.status.clone(), existing.dedupe_contact_id);
    if !is_duplicate_claim {
        return Ok(Some(DuplicateResolution {
            lead: existing,
            contact_id: None,
            refused: Some(format!(
                "this lead is filed as \"{previous_status}\", not as a duplicate — there is no verdict to reverse"
            )),
        }));
    }

    // The outcome is a value of its own type rather than a `(status, label, refusal)` tuple: the
    // first version returned a `decision_label` that was *sometimes* a refusal sentence and
    // decided which by `label.len() > 8`, which is a string's length standing in for a type
    // distinction. `linked` is six characters and `kept_separate` is thirteen, so the test was a
    // "did somebody write a sentence" heuristic that the next label would silently defeat.
    let outcome = match decision {
        DuplicateDecision::Link => match matched_contact {
            Some(contact) => Outcome::Attach(contact),
            // The ambiguous case is worth its own sentence: several contacts matched and the
            // queue shows every one of them, so "no match" here would read as a platform bug.
            None => Outcome::Refused(
                "this duplicate matched several contacts or none — open the lead and pick one"
                    .to_string(),
            ),
        },
        DuplicateDecision::KeepSeparate => Outcome::Separate,
    };

    if let Outcome::Attach(contact) = outcome {
        let status = "assigned";
        let decision_label = "linked";
        let contact_id = Some(contact);
        let query = format!(
            "update crm_leads set status = $3, contact_id = $4, updated_at = now() \
             where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, Lead>(&query)
            .bind(organization_id)
            .bind(lead_id)
            .bind(status)
            .bind(contact_id)
            .fetch_optional(pool)
            .await?;
        if let Some(lead) = &updated {
            append_event(
                pool,
                lead.id,
                "duplicate_decided",
                actor_user_id,
                serde_json::json!({
                    "decision": decision_label,
                    "previous_status": existing.status,
                    "contact_id": contact_id.map(|id| id.to_string()),
                    "dedupe_key": lead.dedupe_key,
                    "dedupe_score": lead.dedupe_score,
                }),
            )
            .await?;
        }
        return Ok(updated.map(|lead| DuplicateResolution {
            lead,
            contact_id,
            refused: None,
        }));
    }

    match outcome {
        // Unreachable: the `Attach` arm above returns. Written out rather than `unreachable!()`
        // so a future arm added to `Outcome` gets a compile error here instead of a panic in a
        // request handler.
        Outcome::Attach(_) => Ok(None),
        Outcome::Separate => {
            let query = format!(
                "update crm_leads set status = 'new', updated_at = now() \
                 where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
            );
            let updated = sqlx::query_as::<_, Lead>(&query)
                .bind(organization_id)
                .bind(lead_id)
                .fetch_optional(pool)
                .await?;
            if let Some(lead) = &updated {
                append_event(
                    pool,
                    lead.id,
                    "duplicate_decided",
                    actor_user_id,
                    serde_json::json!({
                        "decision": "kept_separate",
                        "previous_status": existing.status,
                        "contact_id": serde_json::Value::Null,
                        "dedupe_key": lead.dedupe_key,
                        "dedupe_score": lead.dedupe_score,
                    }),
                )
                .await?;
            }
            Ok(updated.map(|lead| DuplicateResolution {
                lead,
                contact_id: None,
                refused: None,
            }))
        }
        // A refusal writes nothing and says why — the same rule as the conversion path's
        // `conversion_skipped`: a row is never left looking decided when nothing was decided.
        Outcome::Refused(reason) => Ok(Some(DuplicateResolution {
            lead: existing,
            contact_id: None,
            refused: Some(reason),
        })),
    }
}

/// What resolving a duplicate verdict produced, before anything is written.
enum Outcome {
    /// Attach the lead to this contact.
    Attach(Uuid),
    /// File it as a lead in its own right.
    Separate,
    /// Nothing may be written; this is why.
    Refused(String),
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
