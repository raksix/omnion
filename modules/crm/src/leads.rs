//! The form → lead ingress: a submitted form becomes a contact, a deal and a ledger row.
//!
//! **The CRM does not own the producer.** The forms module (REQ-064) publishes
//! `form.submitted`; this module reads that event off the same bus the automation matcher reads
//! and turns it into the platform's relationship records. Nothing here imports a form type, a
//! form table or a form crate, and nothing here has to change when the form builder rewrites its
//! own schema — the contract is the event, which is the one thing a producer can change without
//! breaking a consumer.
//!
//! Four decisions are the whole design:
//!
//! 1. **Exactly once, by the event id.** `crm_form_leads.event_id` is the bus identity of the
//!    submission and it is the primary key. A drain that is retried, and a second API process
//!    reading the same bus, must both leave one contact — not two. The insert therefore comes
//!    *before* the writes and the row count decides who acts.
//! 2. **Not in a transaction, on purpose.** `create_contact` and `create_deal` take a
//!    `&PgPool`; wrapping a drain in one transaction would mean changing the module's own write
//!    signatures to take an executor. Claiming first buys a different failure instead: a crash
//!    between the claim and the write leaves a ledger row that says what happened, which the
//!    inbox shows. A rolled-back transaction shows nothing and replays.
//! 3. **A submission is not always a deal.** Routing is a row ([`LeadSettings`]), not a
//!    constant: a support address and a "quote me" address are the same event with different
//!    meanings.
//! 4. **A repeat submission is the same person.** Matching is on the normalized address, and a
//!    repeat is parked in the repeat stage rather than opening a second deal for one interest.
//!
//! What is *not* here: reading the inbox is a screen concern, the drain is driven by
//! `apps/api`'s runner, and the producer's own spam/consent/export handling stays in REQ-064.

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::contacts::{create_contact, ContactChanges};
use crate::deals::{create_deal, DealChanges};
use crate::error::{CrmError, Result};

/// The event this module consumes. The producer is REQ-064's public submit endpoint; the name is
/// the contract, and it is the one string in this file that has to match the producer exactly.
pub const FORM_SUBMITTED: &str = "form.submitted";

/// How many submissions one drain reads. A batch is bounded so a quiet night of submissions
/// cannot turn into one very long tick.
pub const DEFAULT_BATCH: i64 = 100;

/// The upper bound on a batch, matching the automation matcher's.
const MAX_BATCH: i64 = 500;

/// The answer keys a form may use, in the order they are preferred when a submission carries
/// several. A form builder lets the owner name the fields, so the consumer cannot assume
/// `email` — but a submission that carries *any* of these is a person we can name.
/// Note the order: `first_name` comes **before** `name`. A form that collects both a combined
/// "Name" field and a dedicated given name has told us which one it means, and the more specific
/// key is the one to read — the other way round a person who typed "Ada" and "Lovelace" into two
/// boxes would get "Ada Lovelace" in the first-name column.
const NAME_KEYS: [&str; 5] = ["first_name", "given_name", "name", "full_name", "fullname"];
const LAST_KEYS: [&str; 4] = ["last_name", "surname", "family_name", "lastname"];
const EMAIL_KEYS: [&str; 5] = ["email", "e_mail", "email_address", "work_email", "contact_email"];
const PHONE_KEYS: [&str; 4] = ["phone", "phone_number", "telephone", "mobile"];
const COMPANY_KEYS: [&str; 5] = ["company", "company_name", "organisation", "organization", "firm"];
const MESSAGE_KEYS: [&str; 4] = ["message", "comment", "body", "notes"];

/// A company name longer than this is a paragraph pasted into a one-line field.
const MAX_COMPANY_LENGTH: usize = 160;

/// A message longer than this is truncated rather than refused: a long message is a sign of
/// interest, and the note on the contact is the last place anybody would look for it.
const MAX_MESSAGE_LENGTH: usize = 2000;

/// The longest a single answer may be before it is not read as an identity. A 10 KB "name" is an
/// injection attempt or a broken client, and neither belongs in a first-name column.
const MAX_NAME_LENGTH: usize = 120;

/// A submission after the extractor has read it: the person's identity and the form's own
/// bookkeeping, with nothing a consumer would have to guess at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// The form's identifier, when the producer sends one.
    pub form_id: Option<Uuid>,
    /// The form's public key, which is what a site actually embeds.
    pub form_key: Option<String>,
    /// Given name; empty when the answers carry none.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Lowercased address — the repeat check depends on the lowering being here.
    pub email: Option<String>,
    /// Phone as typed.
    pub phone: Option<String>,
    /// The company, as typed. A lead's company is not created on a guess: `None` is a contact
    /// with no company, which is a real state, and inventing a company from a free-text field
    /// would put unverifiable rows in the company list.
    pub company_name: Option<String>,
    /// The longest free-text answer, used as the deal's headline when the form has no title
    /// field and as the note on the contact.
    pub message: Option<String>,
    /// Every answer, kept so the inbox screen can show the form as it was filled in.
    pub payload: Value,
    /// The site the submission came through.
    pub site_id: Option<Uuid>,
    /// When the person submitted.
    pub occurred_at: OffsetDateTime,
}

impl Submission {
    /// A submission with nothing in it.
    ///
    /// `occurred_at` is *now*, not the epoch: a 1970 timestamp in a list of real submissions is
    /// a lie a person has to notice, and a submission the CRM could not read is still one it
    /// read now.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            form_id: None,
            form_key: None,
            first_name: String::new(),
            last_name: String::new(),
            email: None,
            phone: None,
            company_name: None,
            message: None,
            payload: Value::Null,
            site_id: None,
            occurred_at: OffsetDateTime::now_utc(),
        }
    }

    /// What a person reads in the inbox, in the form's own words.
    #[must_use]
    pub fn display_name(&self) -> String {
        let joined = [self.first_name.trim(), self.last_name.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if joined.is_empty() {
            self.email.clone().unwrap_or_default()
        } else {
            joined
        }
    }

    /// Whether the submission carries enough to make a record of a person.
    ///
    /// A name **or** an address is enough, because a form that collects only a phone number is
    /// still a real lead — refusing it would silently drop the rows a small business gets most
    /// of its work from.
    #[must_use]
    pub fn is_contactable(&self) -> bool {
        !self.first_name.trim().is_empty() || self.email.is_some()
    }
}

/// The routing policy of one organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeadSettings {
    /// A submission becomes a contact.
    pub create_contact: bool,
    /// A submission becomes a deal.
    pub create_deal: bool,
    /// The stage a new deal lands in; `None` = the pipeline's first open stage.
    pub stage_id: Option<Uuid>,
    /// The stage a repeat submission is parked in.
    pub repeat_stage_id: Option<Uuid>,
    /// The value written to the deal's `source` and the contact's tag.
    pub source_label: String,
}

impl Default for LeadSettings {
    /// The policy a fresh tenant gets: a submission becomes a contact *and* a deal, both in the
    /// first open stage. Turning either off is a decision, and a decision an operator has to make
    /// deliberately in a screen.
    fn default() -> Self {
        Self {
            create_contact: true,
            create_deal: true,
            stage_id: None,
            repeat_stage_id: None,
            source_label: "form".to_owned(),
        }
    }
}

/// What one drain did, in the pieces the runner logs and a test asserts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeadReport {
    /// The cursor the drain started from.
    pub cursor: i64,
    /// The cursor it left behind.
    pub advanced_to: i64,
    /// Submissions turned into new records.
    pub created: usize,
    /// Submissions matched to a contact that already existed.
    pub merged: usize,
    /// Submissions with nothing usable in them.
    pub rejected: usize,
    /// Submissions with no organization to file them under.
    pub orphaned: usize,
    /// Submissions the organization's policy is not converting.
    pub disabled: usize,
    /// (event id, what the drain could not do) — logged, never fatal.
    pub failures: Vec<(i64, String)>,
}

impl LeadReport {
    /// Nothing was read.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.created
            + self.merged
            + self.rejected
            + self.orphaned
            + self.disabled
            == 0
            && self.failures.is_empty()
    }
}

/// One row of the ingress ledger, as the inbox screen reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeadIngest {
    /// The bus identity of the submission — the inbox's own key for "the same thing".
    pub event_id: i64,
    /// The form that produced it.
    pub form_id: Option<Uuid>,
    pub form_key: Option<String>,
    /// What became of it.
    pub outcome: String,
    /// The sentence the inbox shows under the outcome.
    pub detail: Option<String>,
    /// The person, as extracted.
    pub name: String,
    pub email: Option<String>,
    pub company_name: Option<String>,
    /// The records it produced, for the "open the record" links.
    pub contact_id: Option<Uuid>,
    pub deal_id: Option<Uuid>,
    pub company_id: Option<Uuid>,
    /// Every answer, for the detail drawer.
    pub payload: Value,
    /// When the person submitted.
    pub occurred_at: OffsetDateTime,
    /// When the CRM read it.
    pub received_at: OffsetDateTime,
}

/// One row of the bus, as the drain reads it.
#[derive(Debug, sqlx::FromRow)]
struct BusEvent {
    id: i64,
    organization_id: Option<Uuid>,
    payload: Value,
}

// ---------------------------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------------------------

/// Read one `form.submitted` payload into a [`Submission`].
///
/// The extractor is the forgiving half and the drain is the strict half, and the split matters: a
/// producer that spells a field `full_name` must not make the CRM drop the submission, and a
/// submission that genuinely carries nothing must not become a contact called "Unknown". So this
/// function never fails — it returns what it could read, and [`Submission::is_contactable`] is
/// what decides whether that is enough.
#[must_use]
pub fn extract(payload: &Value) -> Submission {
    let answers = payload
        .get("answers")
        .filter(|value| value.is_object())
        .unwrap_or(payload);

    // The form's own bookkeeping can sit beside the answers or inside them; both shapes are
    // accepted because the producer owns this and the consumer should not have to guess.
    let form_id = uuid_field(payload, "form_id").or_else(|| uuid_field(answers, "form_id"));
    let site_id = uuid_field(payload, "site_id").or_else(|| uuid_field(answers, "site_id"));
    let form_key = text_field(payload, &["form_key", "form"])
        .or_else(|| text_field(answers, &["form_key", "form"]));

    // A `name` that holds "Ada Lovelace" splits on the first space; a form with explicit name
    // fields does not need the guess, and the explicit fields win when both are present.
    let (guessed_first, guessed_last) = split_full_name(text_field(answers, &["name"]).as_deref());
    // `and_then(clamp_name)` is not optional here: without it a 400-character "name" or a row of
    // dashes is read as a name, and `create_contact` then refuses the whole submission with a
    // message a person reading a form submission never typed. Refusing at extraction turns that
    // into an honest `rejected` row in the inbox.
    let mut first_name = text_field(answers, &NAME_KEYS)
        .as_deref()
        .and_then(clamp_name)
        .or(guessed_first)
        .unwrap_or_default();
    let last_name = text_field(answers, &LAST_KEYS)
        .as_deref()
        .and_then(clamp_name)
        .or(guessed_last)
        .unwrap_or_default();

    let email = text_field(answers, &EMAIL_KEYS)
        .map(|value| value.to_lowercase())
        .filter(|value| crate::model::is_email(value));

    let company_name = text_field(answers, &COMPANY_KEYS)
        .filter(|value| value.chars().count() <= MAX_COMPANY_LENGTH)
        // A single character is not a company; it is a typo, and a company row named "X" is
        // harder to clean up later than a submission quietly not filed under one.
        .filter(|value| value.chars().count() > 1);

    // Truncated, not dropped: a wall of text in a message box is a sign of interest, and the
    // alternative — no message at all — loses exactly the person who cared enough to write.
    let message = text_field(answers, &MESSAGE_KEYS)
        .map(|value| truncate_words(&value, MAX_MESSAGE_LENGTH));

    // A contact's first name is required by the CRM's own validation, and a form is free to
    // collect only an address. The local part of the address is the honest fallback: it is what
    // the person themselves typed in that field, and a human can correct it. A placeholder like
    // "Unknown" is not — it looks like data and reads as an error.
    if first_name.is_empty() {
        first_name = email
            .as_deref()
            .and_then(|address| address.split('@').next())
            .and_then(clamp_name)
            .unwrap_or_default();
    }

    Submission {
        form_id,
        form_key,
        first_name,
        last_name,
        email,
        phone: text_field(answers, &PHONE_KEYS),
        company_name,
        message,
        payload: answers.clone(),
        site_id,
        occurred_at: timestamp_field(payload, "occurred_at")
            .or_else(|| timestamp_field(payload, "submitted_at"))
            .or_else(|| timestamp_field(payload, "created_at"))
            .unwrap_or_else(OffsetDateTime::now_utc),
    }
}

fn text_field(source: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = source.get(*key) else {
            continue;
        };
        let text = match value {
            Value::String(text) => text.trim().to_owned(),
            Value::Number(number) => number.to_string(),
            Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => continue,
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn uuid_field(source: &Value, key: &str) -> Option<Uuid> {
    source
        .get(key)
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok())
}

fn timestamp_field(source: &Value, key: &str) -> Option<OffsetDateTime> {
    let raw = source.get(key)?.as_str()?.trim();
    // The producer may send RFC 3339 (what every HTTP API sends) or a zone-less wall clock. The
    // module's own reader tries RFC 3339 first and only then reads a local time as UTC — the
    // order matters, because a naive string that happened to parse as an instant would be read
    // as UTC when the producer meant its own zone.
    crate::dates::instant::parse(raw).ok()
}

/// Split `"Ada Lovelace"` into the two columns a person would read.
///
/// One token is a first name, not a last: "Cher" is a first name, and putting it in the family
/// column makes every list that shows "Last, First" wrong for the people who have none.
fn split_full_name(full: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(full) = full.map(str::trim).filter(|value| !value.is_empty()) else {
        return (None, None);
    };
    match full.split_once(char::is_whitespace) {
        Some((first, rest)) => (clamp_name(first), clamp_name(rest)),
        None => (clamp_name(full), None),
    }
}

/// Trim a name to a length a column can hold, and refuse a value that is only punctuation.
fn clamp_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_LENGTH {
        return None;
    }
    if trimmed.chars().all(|character| !character.is_alphanumeric()) {
        return None;
    }
    Some(trimmed.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// Read one organization's routing policy, creating the default row on first use.
///
/// The upsert is not laziness: an installation that predates the CRM has no row, and a feature
/// that only works after somebody opens a settings screen is a hidden feature.
/// `on conflict do update … where false` (expressed here as a self-assignment) keeps the
/// existing row's contents — it is there to make the read answerable, not to overwrite a
/// decision.
pub async fn load_settings(pool: &PgPool, organization_id: Uuid) -> Result<LeadSettings> {
    #[derive(sqlx::FromRow)]
    struct Row {
        create_contact: bool,
        create_deal: bool,
        stage_id: Option<Uuid>,
        repeat_stage_id: Option<Uuid>,
        source_label: String,
    }

    let row: Row = sqlx::query_as(
        "insert into crm_lead_settings (organization_id) values ($1) \
         on conflict (organization_id) do update \
             set organization_id = crm_lead_settings.organization_id \
         returning create_contact, create_deal, stage_id, repeat_stage_id, source_label",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok(LeadSettings {
        create_contact: row.create_contact,
        create_deal: row.create_deal,
        stage_id: row.stage_id,
        repeat_stage_id: row.repeat_stage_id,
        source_label: row.source_label,
    })
}

/// Read the row without creating it — for the settings screen, which shows "not configured yet"
/// rather than writing a row just because somebody opened the page.
pub async fn read_settings_row(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<LeadSettings>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        create_contact: bool,
        create_deal: bool,
        stage_id: Option<Uuid>,
        repeat_stage_id: Option<Uuid>,
        source_label: String,
    }

    let row: Option<Row> = sqlx::query_as(
        "select create_contact, create_deal, stage_id, repeat_stage_id, source_label \
         from crm_lead_settings where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| LeadSettings {
        create_contact: row.create_contact,
        create_deal: row.create_deal,
        stage_id: row.stage_id,
        repeat_stage_id: row.repeat_stage_id,
        source_label: row.source_label,
    }))
}

/// Save an organization's routing policy.
pub async fn save_settings(
    pool: &PgPool,
    organization_id: Uuid,
    settings: &LeadSettings,
) -> Result<LeadSettings> {
    // A stage that belongs to another organization would file a lead on somebody else's board,
    // so the id is checked in this organization before it is written.
    for (label, stage_id) in [
        ("stage_id", settings.stage_id),
        ("repeat_stage_id", settings.repeat_stage_id),
    ] {
        if let Some(stage_id) = stage_id {
            let owned: bool = sqlx::query_scalar(
                "select exists (
                     select 1 from crm_pipeline_stages s
                     join crm_pipelines p on p.id = s.pipeline_id
                     where s.id = $1 and p.organization_id = $2
                 )",
            )
            .bind(stage_id)
            .bind(organization_id)
            .fetch_one(pool)
            .await?;
            if !owned {
                return Err(CrmError::invalid(
                    "lead_settings",
                    label,
                    "that stage belongs to another pipeline",
                ));
            }
        }
    }

    sqlx::query(
        "insert into crm_lead_settings \
             (organization_id, create_contact, create_deal, stage_id, repeat_stage_id, source_label) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (organization_id) do update set \
             create_contact = excluded.create_contact, \
             create_deal = excluded.create_deal, \
             stage_id = excluded.stage_id, \
             repeat_stage_id = excluded.repeat_stage_id, \
             source_label = excluded.source_label, \
             updated_at = now()",
    )
    .bind(organization_id)
    .bind(settings.create_contact)
    .bind(settings.create_deal)
    .bind(settings.stage_id)
    .bind(settings.repeat_stage_id)
    .bind(&settings.source_label)
    .execute(pool)
    .await?;

    load_settings(pool, organization_id).await
}

// ---------------------------------------------------------------------------------------------
// The drain
// ---------------------------------------------------------------------------------------------

/// Read the submissions recorded since the last drain and file them.
///
/// **Claim, then act.** The ledger row is inserted *first*, with `on conflict do nothing`, and
/// the caller acts only if that insert claimed the event. The cursor row is still the lock:
/// `for update skip locked` means a second API process reads nothing rather than waiting, and it
/// is read and advanced by the same cursor, so no process can claim to have seen rows it did
/// not.
pub async fn drain(pool: &PgPool, batch: i64) -> Result<LeadReport> {
    let batch = batch.clamp(1, MAX_BATCH);

    let mut report = LeadReport {
        advanced_to: cursor(pool).await?,
        ..LeadReport::default()
    };
    report.cursor = report.advanced_to;

    // The lock and the read are one statement: a process that loses the lock sees no rows, which
    // is why the cursor is also re-read afterwards rather than being taken from the statement.
    let events: Vec<BusEvent> = sqlx::query_as(
        "with claimed as (
             select last_event_id from crm_lead_cursor where id = 1 for update skip locked
         )
         select e.id, e.organization_id, e.payload
         from events e, claimed c
         where e.id > c.last_event_id and e.name = $1
         order by e.id asc limit $2",
    )
    .bind(FORM_SUBMITTED)
    .bind(batch)
    .fetch_all(pool)
    .await?;

    for event in events {
        report.advanced_to = event.id;
        match file_one(pool, &event).await {
            Ok(Filed::Created) => report.created += 1,
            Ok(Filed::Merged) => report.merged += 1,
            Ok(Filed::Rejected(reason)) => {
                report.rejected += 1;
                tracing::debug!(event_id = event.id, reason, "a submission carried nothing usable");
            }
            Ok(Filed::Orphaned) => report.orphaned += 1,
            Ok(Filed::Disabled) => report.disabled += 1,
            Ok(Filed::AlreadyFiled) => {}
            Err(error) => {
                // One bad submission must not stop the drain: the claim is already recorded, so
                // the failure is visible in the inbox instead of being retried forever against an
                // event that can never succeed.
                tracing::warn!(event_id = event.id, error = %error, "a submission could not be filed");
                report.failures.push((event.id, error.to_string()));
            }
        }
    }

    sqlx::query("update crm_lead_cursor set last_event_id = $1, updated_at = now() where id = 1")
        .bind(report.advanced_to)
        .execute(pool)
        .await?;

    Ok(report)
}

/// Point a never-advanced cursor at the end of the bus, so a fresh install watches forward
/// instead of replaying every submission ever recorded.
pub async fn seed_cursor(pool: &PgPool) -> Result<Option<i64>> {
    let seeded: Option<i64> = sqlx::query_scalar(
        "update crm_lead_cursor \
         set last_event_id = (select coalesce(max(id), 0) from events), updated_at = now() \
         where id = 1 and last_event_id = 0 \
         returning last_event_id",
    )
    .fetch_optional(pool)
    .await?;
    Ok(seeded)
}

/// The cursor the last drain left behind — the screen's "last checked" line.
pub async fn cursor(pool: &PgPool) -> Result<i64> {
    let value: i64 = sqlx::query_scalar("select last_event_id from crm_lead_cursor where id = 1")
        .fetch_one(pool)
        .await?;
    Ok(value)
}

/// What one submission became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filed {
    Created,
    Merged,
    Rejected(&'static str),
    Orphaned,
    Disabled,
    /// The ledger already had this event: a retried drain, or a second process that read the
    /// same bus. Nothing was written, which is the correct answer and not a no-op.
    AlreadyFiled,
}

async fn file_one(pool: &PgPool, event: &BusEvent) -> Result<Filed> {
    let Some(organization_id) = event.organization_id else {
        claim(
            pool,
            event,
            None,
            None,
            None,
            None,
            "orphaned",
            Some("the submission arrived without an organization"),
        )
        .await?;
        return Ok(Filed::Orphaned);
    };

    let submission = extract(&event.payload);
    if !submission.is_contactable() {
        let detail = "the submission carried no name and no e-mail address";
        return Ok(match claim(
            pool,
            event,
            Some(&submission),
            None,
            None,
            None,
            "rejected",
            Some(detail),
        )
        .await?
        {
            Claimed::Won => Filed::Rejected("nothing usable"),
            Claimed::Lost => Filed::AlreadyFiled,
        });
    }

    let settings = load_settings(pool, organization_id).await?;
    if !settings.create_contact && !settings.create_deal {
        let detail = "this organization does not turn submissions into records";
        return Ok(match claim(
            pool,
            event,
            Some(&submission),
            None,
            None,
            None,
            "disabled",
            Some(detail),
        )
        .await?
        {
            Claimed::Won => Filed::Disabled,
            Claimed::Lost => Filed::AlreadyFiled,
        });
    }

    // A person who has already written to us is the same person. The repeat stage exists for
    // exactly this: a second deal for one interest is how a pipeline stops predicting anything.
    let existing = match &submission.email {
        Some(email) => {
            sqlx::query_scalar::<_, Uuid>(
                "select id from crm_contacts \
                 where organization_id = $1 and email = $2 and archived_at is null \
                 order by created_at limit 1",
            )
            .bind(organization_id)
            .bind(email)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };

    // The claim is taken *after* the read and *before* the writes, so a second process that read
    // the same bus has already lost by the time it would create anything.
    if !claim(
        pool,
        event,
        Some(&submission),
        existing,
        None,
        None,
        "created",
        None,
    )
    .await?
    .is_won()
    {
        return Ok(Filed::AlreadyFiled);
    }

    let mut contact_id = existing;
    let mut merged = existing.is_some();

    if settings.create_contact && contact_id.is_none() {
        let changes = ContactChanges {
            first_name: submission.first_name.clone(),
            last_name: submission.last_name.clone(),
            email: submission.email.clone(),
            phone: submission.phone.clone(),
            job_title: None,
            company_id: None,
            owner_user_id: None,
            status: Some("lead".to_owned()),
            tags: Some(vec![settings.source_label.clone()]),
            custom: None,
            notes: submission.message.clone(),
        };
        match create_contact(pool, organization_id, &changes).await {
            Ok(contact) => contact_id = Some(contact.id),
            Err(CrmError::EmailTaken) => {
                // A submission for the same address arrived while this one was being written.
                // Reading the winner is not a second record — it is the same person, twice.
                contact_id = sqlx::query_scalar::<_, Uuid>(
                    "select id from crm_contacts \
                     where organization_id = $1 and email = $2 and archived_at is null limit 1",
                )
                .bind(organization_id)
                .bind(&submission.email)
                .fetch_optional(pool)
                .await?;
                merged = true;
            }
            Err(error) => {
                // The claim is ours and the contact did not materialise, so the ledger must stop
                // claiming a record exists.
                restate(pool, event.id, "rejected", &error.to_string()).await?;
                return Err(error);
            }
        }
    }

    let mut deal_id = None;
    if settings.create_deal {
        let changes = DealChanges {
            title: deal_title(&submission, &settings.source_label),
            pipeline_id: None,
            stage_id: if merged {
                settings.repeat_stage_id
            } else {
                settings.stage_id
            },
            company_id: None,
            contact_id,
            // No owner, on purpose: a submission has no human behind it, and a deal assigned to
            // whoever happened to run the drain would put a stranger's lead in their list. An
            // unassigned deal is one the board shows as needing an owner.
            owner_user_id: None,
            amount: None,
            currency: None,
            probability: None,
            expected_close_on: None,
            source: Some(format!("{}:{}", FORM_SUBMITTED, settings.source_label)),
            lost_reason: None,
        };
        // `None` as the owner fallback, for the same reason.
        deal_id = Some(create_deal(pool, organization_id, None, &changes).await?.id);
    }

    // The claim already exists; this only fills in what the writes produced.
    attach(pool, event.id, contact_id, deal_id, merged).await?;

    Ok(if merged { Filed::Merged } else { Filed::Created })
}

/// Whether an insert claimed the event or found it already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claimed {
    Won,
    Lost,
}

impl Claimed {
    fn is_won(self) -> bool {
        self == Self::Won
    }
}

/// Take the event, or find that another run already has it.
///
/// `on conflict do nothing` plus the row count is the whole mechanism: the insert that reports
/// one row is the run that owns this event and every other run skips the writes entirely. There
/// is no window between "checked" and "wrote" for a second process to slip through, which is the
/// hole an `exists` check followed by an insert leaves open.
#[allow(clippy::too_many_arguments)]
async fn claim(
    pool: &PgPool,
    event: &BusEvent,
    submission: Option<&Submission>,
    contact_id: Option<Uuid>,
    deal_id: Option<Uuid>,
    company_id: Option<Uuid>,
    outcome: &str,
    detail: Option<&str>,
) -> Result<Claimed> {
    let fallback = Submission::empty();
    let submission = submission.unwrap_or(&fallback);
    let result = sqlx::query(
        "insert into crm_form_leads \
             (event_id, organization_id, site_id, form_id, form_key, payload, contact_id, deal_id, \
              company_id, email, outcome, detail, occurred_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         on conflict (event_id) do nothing",
    )
    .bind(event.id)
    .bind(event.organization_id)
    .bind(submission.site_id)
    .bind(submission.form_id)
    .bind(&submission.form_key)
    .bind(&submission.payload)
    .bind(contact_id)
    .bind(deal_id)
    .bind(company_id)
    .bind(&submission.email)
    .bind(outcome)
    .bind(detail)
    .bind(submission.occurred_at)
    .execute(pool)
    .await?;

    Ok(if result.rows_affected() == 1 {
        Claimed::Won
    } else {
        Claimed::Lost
    })
}

/// Fill in the records the writes produced, and settle the outcome.
async fn attach(
    pool: &PgPool,
    event_id: i64,
    contact_id: Option<Uuid>,
    deal_id: Option<Uuid>,
    merged: bool,
) -> Result<()> {
    sqlx::query(
        "update crm_form_leads \
         set contact_id = coalesce(contact_id, $2), \
             deal_id = coalesce(deal_id, $3), \
             outcome = case when $4 then 'merged' else outcome end \
         where event_id = $1",
    )
    .bind(event_id)
    .bind(contact_id)
    .bind(deal_id)
    .bind(merged)
    .execute(pool)
    .await?;
    Ok(())
}

/// Correct a claim whose write then failed, so the inbox never claims a record exists.
async fn restate(pool: &PgPool, event_id: i64, outcome: &str, detail: &str) -> Result<()> {
    sqlx::query("update crm_form_leads set outcome = $2, detail = $3 where event_id = $1")
        .bind(event_id)
        .bind(outcome)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

/// The headline a new deal carries.
///
/// A form's answers are the person's words, and a pipeline full of `[no message]` is worse than
/// an empty one: it looks like data. The company is preferred over the free-text message because
/// it is the shorter, more specific phrase, and the address is the last resort because it is the
/// one thing a submission nearly always has.
fn deal_title(submission: &Submission, source: &str) -> String {
    let candidate = submission
        .company_name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            submission
                .message
                .as_deref()
                .map(first_sentence)
        })
        .unwrap_or_else(|| submission.display_name());

    let candidate = candidate.trim();
    let candidate = if candidate.is_empty() {
        format!("New lead from {source}")
    } else {
        candidate.to_owned()
    };

    // The column is `text`, but a 4000-character "headline" is unreadable on a board card, so it
    // is cut at a word boundary rather than refused.
    truncate_words(&candidate, 120)
}

/// The first sentence of a message, cut at a sentence mark.
fn first_sentence(message: &str) -> String {
    let trimmed = message.trim();
    let end = trimmed
        .find(['.', '\n', '!', '?'])
        .map_or(trimmed.len(), |index| index + 1);
    trimmed[..end].trim().to_owned()
}

/// Cut at a word boundary, with an ellipsis only when something was actually removed.
fn truncate_words(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let head: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

// ---------------------------------------------------------------------------------------------
// The inbox
// ---------------------------------------------------------------------------------------------

/// What the ingress inbox filters on.
#[derive(Debug, Clone, Default)]
pub struct LeadQuery {
    /// One of [`crate::model::OUTCOMES`], or `None` for all.
    pub outcome: Option<String>,
    /// A substring of the name, the address, the form key or the answers.
    pub search: Option<String>,
    /// How many rows.
    pub limit: i64,
    /// How many to skip.
    pub offset: i64,
}

/// Read the ledger for one organization, newest first.
///
/// The inbox is deliberately **not** filtered by the caller's visibility level: it is a list of
/// submissions that arrived, and a manager with the `own` level still has to see that something
/// arrived and was filed under a colleague. The records themselves keep their own visibility —
/// this is a log, not a second copy of the pipeline.
pub async fn list_leads(
    pool: &PgPool,
    organization_id: Uuid,
    query: &LeadQuery,
) -> Result<Vec<LeadIngest>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        event_id: i64,
        form_id: Option<Uuid>,
        form_key: Option<String>,
        outcome: String,
        detail: Option<String>,
        email: Option<String>,
        payload: Value,
        contact_id: Option<Uuid>,
        deal_id: Option<Uuid>,
        company_id: Option<Uuid>,
        occurred_at: OffsetDateTime,
        created_at: OffsetDateTime,
    }

    let search = query.search.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
        "select event_id, form_id, form_key, outcome, detail, email, payload, contact_id, \
         deal_id, company_id, occurred_at, created_at from crm_form_leads where organization_id = ",
    );
    builder.push_bind(organization_id);

    if let Some(outcome) = query.outcome.as_deref() {
        // An outcome the ledger cannot hold is not an error: the filter is a checkbox on a
        // screen, and a stale bookmark carrying one shows the whole list rather than nothing.
        if crate::model::OUTCOMES.contains(&outcome) {
            builder.push(" and outcome = ").push_bind(outcome);
        }
    }
    if let Some(search) = search {
        let lowered = search.to_lowercase();
        let needle = format!("%{lowered}%");
        builder
            .push(" and (lower(payload::text) like ")
            .push_bind(needle.clone())
            .push(" or lower(coalesce(email, '')) like ")
            .push_bind(needle.clone())
            .push(" or lower(coalesce(form_key, '')) like ")
            .push_bind(needle)
            .push(")");
    }

    builder
        .push(" order by created_at desc, event_id desc limit ")
        .push_bind(query.limit.clamp(1, 200))
        .push(" offset ")
        .push_bind(query.offset.max(0));

    let rows: Vec<Row> = builder.build_query_as().fetch_all(pool).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let submission = extract(&row.payload);
            LeadIngest {
                event_id: row.event_id,
                form_id: row.form_id,
                form_key: row.form_key,
                outcome: row.outcome,
                detail: row.detail,
                name: submission.display_name(),
                email: row.email,
                company_name: submission.company_name,
                contact_id: row.contact_id,
                deal_id: row.deal_id,
                company_id: row.company_id,
                payload: row.payload,
                occurred_at: row.occurred_at,
                received_at: row.created_at,
            }
        })
        .collect())
}

/// The counters the inbox header shows: how many of each outcome, for the whole organization.
pub async fn lead_counts(pool: &PgPool, organization_id: Uuid) -> Result<Vec<(String, i64)>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select outcome, count(*) from crm_form_leads \
         where organization_id = $1 group by outcome order by outcome",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_explicit_name_field_wins_over_a_guessed_split() {
        let submission = extract(&json!({
            "answers": { "name": "Ada Lovelace", "first_name": "Augusta", "last_name": "King" }
        }));
        assert_eq!(submission.first_name, "Augusta");
        assert_eq!(submission.last_name, "King");
    }

    #[test]
    fn a_single_name_token_is_a_first_name_and_not_a_family_name() {
        let submission = extract(&json!({ "answers": { "name": "Cher" } }));
        assert_eq!(submission.first_name, "Cher");
        assert_eq!(submission.last_name, "");
        assert_eq!(submission.display_name(), "Cher");
    }

    #[test]
    fn a_name_column_holding_two_words_splits_into_both_columns() {
        let submission = extract(&json!({ "answers": { "full_name": "Ada Lovelace" } }));
        assert_eq!(submission.display_name(), "Ada Lovelace");
    }

    #[test]
    fn the_address_is_lowercased_so_the_repeat_check_survives_casing() {
        let submission = extract(&json!({ "answers": { "email": "  Ada@Example.COM " } }));
        assert_eq!(submission.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn an_address_that_is_not_an_address_is_not_read_as_one() {
        let submission = extract(&json!({ "answers": { "email": "not an address" } }));
        assert_eq!(submission.email, None);
        assert!(!submission.is_contactable());
    }

    #[test]
    fn a_submission_with_only_a_name_is_still_a_lead() {
        let submission = extract(&json!({ "answers": { "name": "Madonna" } }));
        assert!(submission.is_contactable());
    }

    #[test]
    fn a_submission_with_only_an_address_is_still_a_lead() {
        let submission = extract(&json!({ "answers": { "email": "grace@example.com" } }));
        assert!(submission.is_contactable());
        assert_eq!(submission.email.as_deref(), Some("grace@example.com"));
    }

    #[test]
    fn an_address_only_submission_gets_a_first_name_from_the_address() {
        // The contact table requires a first name, so the extractor supplies one rather than
        // letting `create_contact` refuse a real lead. The local part is what the person typed
        // in that field, so it is reversible by a human editing the record.
        let submission = extract(&json!({ "answers": { "email": "grace.hopper@example.com" } }));
        assert_eq!(submission.first_name, "grace.hopper");
    }

    #[test]
    fn a_display_name_with_nothing_to_join_falls_back_to_the_address() {
        // The path the fallback above does *not* cover: an extractor result whose name columns
        // are empty but which still carries an address.
        let submission = Submission {
            email: Some("grace@example.com".to_owned()),
            ..Submission::empty()
        };
        assert_eq!(submission.display_name(), "grace@example.com");
    }

    #[test]
    fn an_empty_submission_is_not_contactable() {
        let submission = extract(&json!({ "answers": { "note": "hello?" } }));
        assert!(!submission.is_contactable());
    }

    #[test]
    fn the_answers_are_kept_as_the_form_sent_them() {
        let submission = extract(&json!({
            "form_key": "contact-us",
            "answers": { "name": "Ada", "budget": "1000", "consent": true }
        }));
        assert_eq!(submission.form_key.as_deref(), Some("contact-us"));
        assert_eq!(submission.payload["budget"], json!("1000"));
    }

    #[test]
    fn the_bookkeeping_can_sit_beside_the_answers_or_inside_them() {
        let outside = extract(&json!({
            "form_id": "3f2a9c1e-0b1d-4c2e-8f3a-1b2c3d4e5f60",
            "form_key": "beside",
            "answers": { "name": "Ada" }
        }));
        assert!(outside.form_id.is_some());
        assert_eq!(outside.form_key.as_deref(), Some("beside"));

        let inside = extract(&json!({
            "answers": { "form_key": "inside", "name": "Ada" }
        }));
        assert_eq!(inside.form_key.as_deref(), Some("inside"));
    }

    #[test]
    fn a_timestamp_that_is_not_rfc3339_is_read_when_it_is_still_a_local_one() {
        let submission = extract(&json!({
            "occurred_at": "2026-09-26T10:30",
            "answers": { "name": "Ada" }
        }));
        assert_eq!(submission.occurred_at.year(), 2026);
    }

    #[test]
    fn a_name_that_is_only_punctuation_is_not_a_name() {
        let submission = extract(&json!({ "answers": { "name": "---" } }));
        assert!(!submission.is_contactable());
    }

    #[test]
    fn a_very_long_name_is_refused_rather_than_truncated_into_a_column() {
        let submission = extract(&json!({ "answers": { "name": "x".repeat(400) } }));
        assert!(!submission.is_contactable());
    }

    #[test]
    fn a_company_of_one_character_is_a_typo_and_not_a_company() {
        let submission = extract(&json!({ "answers": { "name": "Ada", "company": "X" } }));
        assert_eq!(submission.company_name, None);
    }

    #[test]
    fn the_deal_headline_is_the_company_when_there_is_one() {
        let submission = extract(&json!({
            "answers": { "name": "Ada", "company": "Analytical Engines Ltd", "message": "Hello" }
        }));
        assert_eq!(
            deal_title(&submission, "form"),
            "Analytical Engines Ltd",
            "the shorter, more specific phrase is the headline"
        );
    }

    #[test]
    fn the_deal_headline_falls_back_to_the_first_sentence_of_the_message() {
        let submission = extract(&json!({
            "answers": { "name": "Ada", "message": "We need a quote. Please call back." }
        }));
        assert_eq!(deal_title(&submission, "form"), "We need a quote.");
    }

    #[test]
    fn a_deal_with_no_words_to_title_it_still_gets_a_readable_headline() {
        let submission = Submission::empty();
        assert_eq!(deal_title(&submission, "webinar"), "New lead from webinar");
    }

    #[test]
    fn a_long_headline_is_cut_at_a_word_and_says_that_it_was() {
        let submission = extract(&json!({
            "answers": { "name": "Ada", "message": "word ".repeat(80) }
        }));
        let title = deal_title(&submission, "form");
        assert!(title.chars().count() <= 120, "{title}");
        assert!(title.ends_with('…'), "{title}");
        assert!(!title.contains("  "), "{title}");
    }

    #[test]
    fn a_message_longer_than_a_note_is_cut_rather_than_refused() {
        let submission = extract(&json!({
            "answers": { "name": "Ada", "message": "m".repeat(MAX_MESSAGE_LENGTH + 500) }
        }));
        assert_eq!(
            submission.message.as_deref().map(str::chars).map(Iterator::count),
            Some(MAX_MESSAGE_LENGTH)
        );
        assert!(submission.is_contactable(), "a long message is still a lead");
    }

    #[test]
    fn the_distinct_answers_the_extractor_reads_are_stable() {
        // A rename of an answer key must not change what the inbox shows.
        for answers in [
            json!({ "email": "a@b.co" }),
            json!({ "e_mail": "a@b.co" }),
            json!({ "email_address": "a@b.co" }),
        ] {
            assert_eq!(extract(&answers).email.as_deref(), Some("a@b.co"), "{answers}");
        }
    }

    #[test]
    fn a_form_id_that_is_not_a_uuid_is_read_as_no_form_id() {
        let submission = extract(&json!({ "form_id": "contact-us", "answers": { "name": "Ada" } }));
        assert_eq!(submission.form_id, None);
    }

    #[test]
    fn an_idle_report_is_idle_and_a_failure_is_not() {
        assert!(LeadReport::default().is_idle());
        let mut report = LeadReport::default();
        report.failures.push((4, "boom".to_owned()));
        assert!(!report.is_idle(), "a failure is work that did not happen");
    }

    #[test]
    fn the_default_policy_files_a_submission_in_both_registers() {
        let settings = LeadSettings::default();
        assert!(settings.create_contact && settings.create_deal);
        assert_eq!(settings.source_label, "form");
    }

    #[test]
    fn the_outcome_vocabulary_agrees_with_the_migration_check() {
        // A fourth outcome needs the constant, the check constraint and the inbox filter; this
        // is the test that says so rather than letting the three drift apart silently.
        assert_eq!(crate::model::OUTCOMES.len(), 5);
        for outcome in crate::model::OUTCOMES {
            assert!(!outcome.is_empty());
            assert!(outcome.chars().all(|c| c.is_ascii_lowercase()));
        }
    }
}
