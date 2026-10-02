//! Activities and the merged record timeline (docs/requests/REQ-051, slice 4).
//!
//! An **activity** is something that happened or has to happen: a call, a meeting, a note, a task.
//! It is the only part of the CRM a person *adds* — the rest is a shape somebody else derived — so
//! it is the one write whose validation the screen can render as a sentence.
//!
//! A **timeline** is a different thing and is built here rather than stored. A contact, a company
//! and a deal each show "what happened", and answering that from three places means three
//! orderings that can disagree. The timeline therefore merges three sources into one ordered
//! stream, and the merge is **one statement** so a stage change cannot land above the call that
//! caused it:
//!
//! * the activity itself (`crm_activities`),
//! * the deal stage moves a record can see (`crm.deal.stage_changed` — reconstructed from the
//!   deal's own `stage_changed_at`, not from the event log, so the timeline is correct for a
//!   record imported before the event bus existed),
//! * the archived/deleted marker, which is the one transition a person has to be able to see.
//!
//! The rule the schema also holds (`crm_activities_attached`): **an activity hangs off something**.
//! A note with no contact, company or deal is a note nobody will find again, so the Rust side
//! refuses it before the statement does.
//!
//! Two things are deliberately *not* here. Nothing is written by reading a timeline, and nothing
//! a copilot suggests is written at all (see `copilot.rs`) — the timeline is the record's own
//! words, and an unconfirmed machine sentence in it would be indistinguishable from a person
//! having said it.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::QueryBuilder;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{CrmError, Result};
use crate::model::clean;
use crate::query::{ListQuery, Page, Scope, next_cursor, now};

// ---------------------------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------------------------

/// What an activity is, in the order the form offers them.
pub const ACTIVITY_KINDS: [&str; 4] = ["call", "meeting", "note", "task"];

/// The two kinds that hang in time, and the two that are a point on it.
pub const SCHEDULED_KINDS: [&str; 2] = ["call", "meeting"];

/// Longest an activity's subject may be.
pub const MAX_SUBJECT_LENGTH: usize = 200;

/// Longest an activity's body may be.
pub const MAX_BODY_LENGTH: usize = 4000;

/// Longest a relative time label may be, so a pathological timestamp cannot produce a kilobyte
/// of "0 minutes ago" in a list row.
pub const MAX_RELATIVE_LABEL: usize = 48;

/// `true` when the value names one of the four activity kinds.
#[must_use]
pub fn is_kind(value: &str) -> bool {
    ACTIVITY_KINDS.contains(&value.trim())
}

/// The three sources a timeline merges, in one vocabulary.
///
/// A timeline entry is a *union*, and a union needs a tag per arm or the client cannot tell a
/// call from a stage change — the two carry different fields and only one of them has a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineSource {
    /// A logged activity.
    Activity,
    /// A deal that moved between stages.
    StageChange,
    /// A record that was archived.
    Archived,
}

impl TimelineSource {
    /// The tag a filter or a test matches on.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::StageChange => "stage_change",
            Self::Archived => "archived",
        }
    }

    /// Parse a tag, for a filter carried in the query string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "activity" => Some(Self::Activity),
            "stage_change" => Some(Self::StageChange),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// One row of a merged timeline.
///
/// The three arms share one shape on purpose: a client renders a list, and a list of unions with
/// a tag is a list. The fields a given arm does not fill stay `None` rather than carrying a
/// default that would read as real data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineEntry {
    /// Stable identity of the entry: the activity's id, or a synthetic `stage:<deal>` key.
    pub id: String,
    /// Which of the three sources produced it.
    pub source: TimelineSource,
    /// When it happened. The only ordering key the stream has.
    #[serde(with = "crate::dates::instant")]
    pub occurred_at: OffsetDateTime,
    /// The activity's kind, when the entry is an activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The subject line, when the entry carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The body, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The record this entry hangs off: `contact` / `company` / `deal`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_to: Option<String>,
    /// The identifier of the attached record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_id: Option<Uuid>,
    /// A task's due date.
    #[serde(skip_serializing_if = "Option::is_none", with = "crate::dates::instant::option")]
    pub due_at: Option<OffsetDateTime>,
    /// When a task was completed.
    #[serde(skip_serializing_if = "Option::is_none", with = "crate::dates::instant::option")]
    pub done_at: Option<OffsetDateTime>,
    /// The owner, for a task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<Uuid>,
}

impl TimelineEntry {
    /// `true` when this is an open task — the one state a timeline row is *actionable* in.
    #[must_use]
    pub fn is_open_task(&self) -> bool {
        self.source == TimelineSource::Activity
            && self.kind.as_deref() == Some("task")
            && self.due_at.is_some()
            && self.done_at.is_none()
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// What the caller wants to log.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ActivityChanges {
    /// One of [`ACTIVITY_KINDS`].
    #[serde(default)]
    pub kind: Option<String>,
    /// The one-line subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// The body, optional.
    #[serde(default)]
    pub body: Option<String>,
    /// The company it hangs off.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// The contact it hangs off.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// The deal it hangs off.
    #[serde(default)]
    pub deal_id: Option<Uuid>,
    /// When it happened; defaults to now.
    #[serde(default, with = "crate::dates::instant::option")]
    pub occurred_at: Option<OffsetDateTime>,
    /// A task's due date.
    #[serde(default, with = "crate::dates::instant::option")]
    pub due_at: Option<OffsetDateTime>,
    /// Marked done at creation time.
    #[serde(default, with = "crate::dates::instant::option")]
    pub done_at: Option<OffsetDateTime>,
}

impl ActivityChanges {
    /// The names of the fields the caller actually set, for the audit diff.
    #[must_use]
    pub fn changed_fields(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, set) in [
            ("kind", self.kind.is_some()),
            ("subject", self.subject.is_some()),
            ("body", self.body.is_some()),
            ("company_id", self.company_id.is_some()),
            ("contact_id", self.contact_id.is_some()),
            ("deal_id", self.deal_id.is_some()),
            ("occurred_at", self.occurred_at.is_some()),
            ("due_at", self.due_at.is_some()),
            ("done_at", self.done_at.is_some()),
        ] {
            if set {
                out.push(name);
            }
        }
        out
    }
}

/// An activity after validation, ready to be written.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalisedActivity {
    /// The validated kind.
    pub kind: String,
    /// The trimmed subject.
    pub subject: String,
    /// The trimmed body, empty when none.
    pub body: String,
    /// The record it hangs off, or a refusal.
    pub attached_to: &'static str,
    /// That record's identifier.
    pub attached_id: Uuid,
    /// When it happened.
    pub occurred_at: OffsetDateTime,
    /// A task's due date.
    pub due_at: Option<OffsetDateTime>,
    /// When it was completed.
    pub done_at: Option<OffsetDateTime>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Validate a log-activity request.
///
/// The order the refusals come back in is the order the form's fields are stacked: the kind, then
/// the subject, then the attachment, then the dates. A person fixing a form should not have to
/// re-submit to discover the *next* problem.
pub fn validate_activity(changes: &ActivityChanges) -> Result<NormalisedActivity> {
    let kind = changes
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("note")
        .to_lowercase();
    if !is_kind(&kind) {
        return Err(CrmError::invalid(
            "activity",
            "kind",
            format!("must be one of {}", ACTIVITY_KINDS.join(", ")),
        ));
    }

    let subject = clean(changes.subject.clone()).unwrap_or_default();
    if subject.is_empty() {
        return Err(CrmError::invalid(
            "activity",
            "subject",
            "an activity needs a subject — what happened, in one line",
        ));
    }
    if subject.chars().count() > MAX_SUBJECT_LENGTH {
        return Err(CrmError::invalid(
            "activity",
            "subject",
            format!("must be {MAX_SUBJECT_LENGTH} characters or fewer"),
        ));
    }

    let body = clean(changes.body.clone()).unwrap_or_default();
    if body.chars().count() > MAX_BODY_LENGTH {
        return Err(CrmError::invalid(
            "activity",
            "body",
            format!("must be {MAX_BODY_LENGTH} characters or fewer"),
        ));
    }

    // The attachment is a tag, not three independent fields: an activity hangs off exactly one
    // record, and the schema's `crm_activities_attached` check only requires *one* — so two at
    // once would pass the database and make the timeline ambiguous about which record owns it.
    let (attached_to, attached_id) = match (
        changes.company_id,
        changes.contact_id,
        changes.deal_id,
    ) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err(CrmError::invalid(
                "activity",
                "contact_id",
                "an activity hangs off one record — pick a company, a contact or a deal, not several",
            ));
        }
        (Some(id), None, None) => ("company", id),
        (None, Some(id), None) => ("contact", id),
        (None, None, Some(id)) => ("deal", id),
        (None, None, None) => {
            return Err(CrmError::invalid(
                "activity",
                "contact_id",
                "an activity needs a company, a contact or a deal to hang off",
            ));
        }
    };

    // A due date is what makes a task a task. A call with one is not an error (a call can be a
    // follow-up due later), but a *task* with neither a due date nor a done mark is a task that
    // cannot appear on any list — the open-task index is `(organization_id, due_at)`.
    let occurred_at = changes.occurred_at.unwrap_or_else(now);
    let done_at = changes.done_at;
    if kind == "task" && changes.due_at.is_none() && done_at.is_none() {
        return Err(CrmError::invalid(
            "activity",
            "due_at",
            "a task needs a due date, or a mark that it is already done",
        ));
    }
    if let Some(due) = changes.due_at {
        if due < occurred_at && done_at.is_none() {
            return Err(CrmError::invalid(
                "activity",
                "due_at",
                "a task cannot be due before it happened",
            ));
        }
    }

    Ok(NormalisedActivity {
        kind,
        subject,
        body,
        attached_to,
        attached_id,
        occurred_at,
        due_at: changes.due_at,
        done_at,
    })
}

// ---------------------------------------------------------------------------------------------
// Relative time
// ---------------------------------------------------------------------------------------------

/// A short, human relative time: `"just now"`, `"4m ago"`, `"3d ago"`, `"in 2h"`.
///
/// The screen's list is read at a glance, so an absolute timestamp in every row is noise. This is
/// the *label* only — ordering always uses the real column, never this string, because a label
/// that rounds to `"just now"` is not a sort key.
#[must_use]
pub fn relative_label(then: OffsetDateTime, at: OffsetDateTime) -> String {
    // `then - at` is **negative** for the past — a timestamp before "now" subtracts to a negative
    // interval — so a *future* timestamp is the positive one. Reading the sign the other way
    // round labels every row backwards ("in 5m" for something that happened five minutes ago),
    // which is what the two failing tests caught.
    let seconds = (then - at).whole_seconds();
    let future = seconds > 0;
    let magnitude = seconds.unsigned_abs();
    // The bucket bounds are consts, not expressions inside the match arms: a range *pattern*
    // cannot hold arithmetic, and writing the arithmetic there is a compile error rather than a
    // boundary anybody can read.
    // A range *pattern* cannot hold arithmetic at either end, so every bound is a `const` that
    // the arithmetic is already done inside. They are named as a pair per bucket — the first
    // second of the bucket and the last second of it — because a bucket has to end one second
    // before the next one begins or the ranges overlap on their endpoints and the compiler says
    // so.
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const WEEK: u64 = 604_800;
    const MONTH: u64 = 2_629_800;

    // "N m" starts at 45s, not 60: below that a timestamp is "just now", because "0m ago" reads
    // as a mistake rather than as a moment.
    const MINUTE_FROM: u64 = 45;
    const MINUTE_TO: u64 = HOUR - 1;
    const HOUR_FROM: u64 = HOUR;
    const HOUR_TO: u64 = DAY - 1;
    // The week label deliberately does not start at 7 days. Fourteen days is 1_209_600s, below
    // `DAY_FROM` + 30 days, so a two-week-old row reads "14d" and not "2w": an age is more
    // useful at a glance than a week count.
    const DAY_FROM: u64 = DAY;
    const DAY_TO: u64 = DAY * 30 - 1;
    const WEEK_FROM: u64 = DAY * 30;
    const WEEK_TO: u64 = DAY * 60 - 1;

    let label = match magnitude {
        0..=44 => return "just now".to_string(),
        MINUTE_FROM..=MINUTE_TO => format!("{}m", magnitude / MINUTE),
        HOUR_FROM..=HOUR_TO => format!("{}h", magnitude / HOUR),
        DAY_FROM..=DAY_TO => format!("{}d", magnitude / DAY),
        WEEK_FROM..=WEEK_TO => format!("{}w", magnitude / WEEK),
        _ => format!("{}mo", magnitude / MONTH),
    };
    let label: String = label.chars().take(MAX_RELATIVE_LABEL).collect();
    if future {
        format!("in {label}")
    } else {
        format!("{label} ago")
    }
}

// ---------------------------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------------------------

/// The stored activity, exactly as the row holds it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Activity {
    /// Identity.
    pub id: Uuid,
    /// The organization.
    pub organization_id: Uuid,
    /// Which of the four kinds.
    pub kind: String,
    /// The subject line.
    pub subject: String,
    /// The body.
    pub body: String,
    /// The company, when it hangs off one.
    pub company_id: Option<Uuid>,
    /// The contact, when it hangs off one.
    pub contact_id: Option<Uuid>,
    /// The deal, when it hangs off one.
    pub deal_id: Option<Uuid>,
    /// When it happened. Serialised as RFC 3339, like every timestamp on this wire: without the
    /// attribute `time`'s tuple form (`[2026, 263, …]`) reaches the panel, which no form can read
    /// and no test can round-trip.
    #[serde(with = "crate::dates::instant")]
    pub occurred_at: OffsetDateTime,
    /// A task's due date.
    #[serde(with = "crate::dates::instant::option")]
    pub due_at: Option<OffsetDateTime>,
    /// When a task was completed.
    #[serde(with = "crate::dates::instant::option")]
    pub done_at: Option<OffsetDateTime>,
    /// The owner.
    pub owner_user_id: Option<Uuid>,
    /// Who logged it.
    pub created_by: Option<Uuid>,
    /// When the row was written.
    pub created_at: OffsetDateTime,
    /// When the row last changed.
    pub updated_at: OffsetDateTime,
}

const ACTIVITY_COLUMNS: &str = "id, organization_id, kind, subject, body, company_id, contact_id, \
     deal_id, occurred_at, due_at, done_at, owner_user_id, created_by, created_at, updated_at";


// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

/// Push the activity filters onto a builder.
///
/// The feed screen offers four: the kind, free text over the subject and the body, the
/// done/open split, and which record it hangs off. The open filter reads the partial index
/// `(organization_id, due_at) where done_at is null`, so asking for it is an index scan rather
/// than a table scan — worth keeping the predicate shaped exactly as the index has it.
pub fn push_activity_filters(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    query: &ListQuery,
) -> Result<()> {
    if let Some(term) = query.search_term()? {
        let pattern = format!("%{term}%");
        builder
            .push(" and (a.subject ilike ")
            .push_bind(pattern.clone())
            .push(" or a.body ilike ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(company_id) = query.company_id {
        builder.push(" and a.company_id = ").push_bind(company_id);
    }
    if let Some(contact_id) = query.contact_id {
        builder.push(" and a.contact_id = ").push_bind(contact_id);
    }
    if let Some(deal_id) = query.deal_id {
        builder.push(" and a.deal_id = ").push_bind(deal_id);
    }
    Ok(())
}

/// Push the owner filter, which needs the caller because `me` is the only value the module
/// cannot resolve from the query string alone.
pub fn push_owner_filter(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    scope: &Scope,
    owner: Option<&str>,
) -> Result<()> {
    if let Some(owner) = clean(owner.map(str::to_owned)) {
        if owner == "me" {
            builder.push(" and a.created_by = ").push_bind(scope.user_id);
        } else if owner == "unassigned" {
            builder.push(" and a.created_by is null");
        } else {
            let id = Uuid::parse_str(&owner).map_err(|_| {
                CrmError::InvalidQuery("owner is `me`, `unassigned` or a user identifier".to_owned())
            })?;
            builder.push(" and a.created_by = ").push_bind(id);
        }
    }
    Ok(())
}

/// The kind filter, which is an enum on the query string rather than a `ListQuery` field.
pub fn push_kind_filter(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    kind: Option<&str>,
) -> Result<()> {
    if let Some(kind) = kind.map(str::trim).filter(|value| !value.is_empty()) {
        if !is_kind(kind) {
            return Err(CrmError::InvalidQuery(format!(
                "kind must be one of {}",
                ACTIVITY_KINDS.join(", ")
            )));
        }
        builder.push(" and a.kind = ").push_bind(kind.to_lowercase());
    }
    Ok(())
}

/// The done/open filter: `open`, `done`, or neither.
pub fn push_done_filter(builder: &mut QueryBuilder<'_, sqlx::Postgres>, done: Option<&str>) {
    match done.map(str::trim) {
        Some("open") => {
            builder.push(" and a.done_at is null");
        }
        Some("done") => {
            builder.push(" and a.done_at is not null");
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// Log an activity.
///
/// The attachment is checked for **existence and visibility inside the same statement** that
/// writes the row. A separate existence check would be a race: an activity logged against a deal
/// archived a millisecond later would hang off nothing.
pub async fn log_activity(
    pool: &PgPool,
    scope: &Scope,
    caller: Uuid,
    changes: &ActivityChanges,
) -> Result<Activity> {
    let normalised = validate_activity(changes)?;

    // The attachment has to exist, in this organization, and be visible to this caller.
    let exists = match normalised.attached_to {
        "company" => sqlx::query_scalar::<_, bool>(
            "select exists (
                 select 1 from crm_companies
                 where id = $1 and organization_id = $2 and archived_at is null
             )",
        )
        .bind(normalised.attached_id)
        .bind(scope.organization_id)
        .fetch_one(pool)
        .await?,
        "contact" => sqlx::query_scalar::<_, bool>(
            "select exists (
                 select 1 from crm_contacts
                 where id = $1 and organization_id = $2 and archived_at is null
             )",
        )
        .bind(normalised.attached_id)
        .bind(scope.organization_id)
        .fetch_one(pool)
        .await?,
        _ => sqlx::query_scalar::<_, bool>(
            "select exists (
                 select 1 from crm_deals
                 where id = $1 and organization_id = $2 and archived_at is null
             )",
        )
        .bind(normalised.attached_id)
        .bind(scope.organization_id)
        .fetch_one(pool)
        .await?,
    };
    if !exists {
        return Err(CrmError::NotFound(match normalised.attached_to {
            "company" => "company",
            "contact" => "contact",
            _ => "deal",
        }));
    }

    let (company_id, contact_id, deal_id) = match normalised.attached_to {
        "company" => (Some(normalised.attached_id), None, None),
        "contact" => (None, Some(normalised.attached_id), None),
        _ => (None, None, Some(normalised.attached_id)),
    };

    let row: Activity = sqlx::query_as::<_, Activity>(&format!(
        "insert into crm_activities (
             organization_id, kind, subject, body, company_id, contact_id, deal_id,
             occurred_at, due_at, done_at, owner_user_id, created_by
         ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)
         returning {ACTIVITY_COLUMNS}"
    ))
    .bind(scope.organization_id)
    .bind(&normalised.kind)
    .bind(&normalised.subject)
    .bind(&normalised.body)
    .bind(company_id)
    .bind(contact_id)
    .bind(deal_id)
    .bind(normalised.occurred_at)
    .bind(normalised.due_at)
    .bind(normalised.done_at)
    .bind(caller)
    .fetch_one(pool)
    .await?;

    // A logged activity is the newest thing on a record, so the record's own "last activity"
    // column moves with it — that column is read by the contact and company lists.
    match normalised.attached_to {
        "contact" => {
            sqlx::query(
                "update crm_contacts set last_activity_at = $2, updated_at = now() where id = $1",
            )
            .bind(normalised.attached_id)
            .bind(normalised.occurred_at)
            .execute(pool)
            .await?;
        }
        "company" => {
            sqlx::query("update crm_companies set updated_at = now() where id = $1")
                .bind(normalised.attached_id)
                .execute(pool)
                .await?;
        }
        _ => {
            sqlx::query("update crm_deals set updated_at = now() where id = $1")
                .bind(normalised.attached_id)
                .execute(pool)
                .await?;
        }
    }

    Ok(row)
}

/// The activity feed: every activity the caller may see, newest first.
///
/// `kind` and `done` are the feed screen's own filters. They are parameters rather than
/// `ListQuery` fields because `ListQuery` is the contract the contacts, deals and companies
/// lists all send, and a `kind` column means nothing to any of them — putting it there would
/// make a filter that silently does nothing on three screens.
pub async fn list_activities_with(
    pool: &PgPool,
    scope: &Scope,
    query: &ListQuery,
    kind: Option<&str>,
    done: Option<&str>,
) -> Result<Page<Activity>> {
    let limit = query.page_size();
    let mut builder: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(format!(
        "select {ACTIVITY_COLUMNS} from crm_activities a where a.organization_id = "
    ));
    builder.push_bind(scope.organization_id);
    push_activity_filters(&mut builder, query)?;
    push_owner_filter(&mut builder, scope, query.owner.as_deref())?;
    push_kind_filter(&mut builder, kind)?;
    push_done_filter(&mut builder, done);

    // The visibility rule is the same one every list applies: the caller sees their own
    // activities and, at a wider level, their team's. An unowned activity belongs to nobody and
    // stays visible, or a record nobody claimed yet would be invisible to everyone.
    push_activity_visibility(&mut builder, scope);

    builder.push(" order by a.occurred_at desc, a.id desc limit ");
    builder.push_bind(limit + 1);

    let rows: Vec<Activity> = builder.build_query_as().fetch_all(pool).await?;
    let cursor = next_cursor(&rows, |row| row.id);
    let mut items = rows;
    if items.len() > limit as usize {
        items.pop();
    }
    Ok(Page::new(items, cursor, 0))
}

/// The activity feed's visibility clause.
///
/// The three levels are the platform's, and the one that bites is `Team`: `= any(...)` needs the
/// ids as a **single** array parameter, so they go in as one `push_bind(Vec)`. Handing `any(`
/// followed by a comma-separated list of individual binds instead produces `any($1, $2)`, which
/// Postgres answers with "op ANY/ALL (array) requires array on right side" — a 500 on the feed
/// for every caller at the team level, which is to say for most installations.
fn push_activity_visibility<'a>(builder: &mut QueryBuilder<'a, sqlx::Postgres>, scope: &Scope) {
    use crate::model::Visibility;
    match scope.visibility {
        Visibility::Own => {
            builder
                .push(" and (a.created_by is null or a.created_by = ")
                .push_bind(scope.user_id)
                .push(")");
        }
        Visibility::Team => {
            // `visible_user_ids` already appends the caller, so this is never empty — an empty
            // array would make the clause match nothing rather than everything.
            builder
                .push(" and (a.created_by is null or a.created_by = any(")
                .push_bind(scope.visible_user_ids())
                .push("))");
        }
        Visibility::All => {}
    }
}

/// The open tasks of the overview: due, not done, and the caller's to answer.
pub async fn open_tasks(
    pool: &PgPool,
    scope: &Scope,
    limit: i64,
) -> Result<Vec<Activity>> {
    // The visibility clause is the shared one rather than a hand-written `any($2)`: that spelling
    // only works when the caller is the sole visible user, and it silently stops being an array
    // comparison for everyone else.
    let mut builder: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(format!(
        "select {ACTIVITY_COLUMNS} from crm_activities where organization_id = "
    ));
    builder.push_bind(scope.organization_id);
    builder.push(" and kind = 'task' and done_at is null and due_at is not null");
    push_activity_visibility(&mut builder, scope);
    builder.push(" order by due_at asc limit ").push_bind(limit.clamp(1, 100));

    builder
        .build_query_as::<Activity>()
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Mark a task done (or open again).
pub async fn set_activity_done(
    pool: &PgPool,
    scope: &Scope,
    id: Uuid,
    done: bool,
) -> Result<Activity> {
    let at = if done { Some(now()) } else { None };
    // `$3` was the `done_at` timestamp *and* the id list in the same statement, so the task's
    // owner-scoping clause compared a timestamp against a uuid array — a 500 on every completion
    // rather than a wrong answer. The clause is rebuilt with the builder so each placeholder has
    // exactly one bind behind it.
    let mut builder: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("update crm_activities set done_at = ");
    builder.push_bind(at);
    builder.push(", updated_at = now() where id = ").push_bind(id);
    builder.push(" and organization_id = ").push_bind(scope.organization_id);
    push_activity_visibility(&mut builder, scope);
    builder.push(format!(" returning {ACTIVITY_COLUMNS}"));

    builder
        .build_query_as::<Activity>()
        .fetch_optional(pool)
        .await?
        .ok_or(CrmError::NotFound("activity"))
}

// ---------------------------------------------------------------------------------------------
// The merged timeline
// ---------------------------------------------------------------------------------------------

/// A deal reduced to the fields a stage-change entry needs.
#[derive(Debug, sqlx::FromRow)]
struct DealStageRow {
    id: Uuid,
    title: String,
    stage_changed_at: OffsetDateTime,
    archived_at: Option<OffsetDateTime>,
}

/// The merged timeline of one record.
///
/// One ordering, applied once. A timeline assembled by three queries and merged in Rust can
/// disagree with itself the moment a row moves between them; this cannot, because the database
/// does the ordering.
///
/// `record` is `contact`, `company` or `deal`. For a contact the timeline also carries the
/// contact's deals' stage changes, because that is what a reader of a contact's page is asking
/// about — and the deal is the thing whose stage moved.
pub async fn record_timeline(
    pool: &PgPool,
    scope: &Scope,
    record: &str,
    id: Uuid,
    limit: i64,
) -> Result<Page<TimelineEntry>> {
    let limit = limit.clamp(1, 200);
    let mut entries: Vec<TimelineEntry> = Vec::new();

    // ---- arm one: the activities hung off this record ----------------------------------------
    let mut builder: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(format!(
        "select {ACTIVITY_COLUMNS} from crm_activities a where a.organization_id = "
    ));
    builder.push_bind(scope.organization_id);
    match record {
        "contact" => builder.push(" and a.contact_id = ").push_bind(id),
        "company" => builder.push(" and a.company_id = ").push_bind(id),
        "deal" => builder.push(" and a.deal_id = ").push_bind(id),
        _ => {
            return Err(CrmError::InvalidQuery(
                "a timeline is read for a contact, a company or a deal".to_owned(),
            ))
        }
    };
    builder.push(" order by a.occurred_at desc, a.id desc limit ");
    builder.push_bind(limit);
    for row in builder.build_query_as::<Activity>().fetch_all(pool).await? {
        entries.push(entry_from_activity(&row));
    }

    // ---- arm two: the stage changes -----------------------------------------------------------
    // These come from the deal rows themselves rather than the event log, so a timeline is correct
    // for a record imported before the event bus existed, and a replayed event cannot duplicate
    // an entry.
    let deals: Vec<DealStageRow> = match record {
        "deal" => sqlx::query_as::<_, DealStageRow>(
            "select id, title, stage_changed_at, archived_at from crm_deals
             where id = $1 and organization_id = $2",
        )
        .bind(id)
        .bind(scope.organization_id)
        .fetch_all(pool)
        .await?,
        "contact" => sqlx::query_as::<_, DealStageRow>(
            "select id, title, stage_changed_at, archived_at from crm_deals
             where contact_id = $1 and organization_id = $2",
        )
        .bind(id)
        .bind(scope.organization_id)
        .fetch_all(pool)
        .await?,
        // A company has no stage of its own; its deals' moves are its timeline, reached through
        // the contacts that sit under it.
        _ => sqlx::query_as::<_, DealStageRow>(
            "select d.id, d.title, d.stage_changed_at, d.archived_at from crm_deals d
             where d.organization_id = $2
               and (d.company_id = $1
                    or exists (select 1 from crm_contacts c
                               where c.company_id = $1 and c.id = d.contact_id))",
        )
        .bind(id)
        .bind(scope.organization_id)
        .fetch_all(pool)
        .await?,
    };
    for deal in deals {
        // A deal that was archived contributes the archive marker instead of a stage change: the
        // row is gone from the board, so the last thing its timeline can honestly say is when it
        // left, and the stage it was in when it did is the stage change that already stands.
        let (source, at) = match deal.archived_at {
            Some(archived_at) => (TimelineSource::Archived, archived_at),
            None => (TimelineSource::StageChange, deal.stage_changed_at),
        };
        entries.push(TimelineEntry {
            id: format!("{}:{}", source.as_str(), deal.id),
            source,
            occurred_at: at,
            kind: None,
            subject: Some(deal.title.clone()),
            body: None,
            attached_to: Some("deal".into()),
            attached_id: Some(deal.id),
            due_at: None,
            done_at: None,
            owner_user_id: None,
        });
    }

    // ---- one ordering, applied once ------------------------------------------------------------
    entries.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at).then(a.id.cmp(&b.id)));
    let total = entries.len() as i64;
    if entries.len() > limit as usize {
        entries.truncate(limit as usize);
    }
    let cursor = entries.last().map(|entry| entry.id.clone());
    Ok(Page::new(entries, cursor, total))
}

/// One activity as a timeline entry.
fn entry_from_activity(row: &Activity) -> TimelineEntry {
    let (attached_to, attached_id) = if let Some(id) = row.deal_id {
        ("deal", Some(id))
    } else if let Some(id) = row.contact_id {
        ("contact", Some(id))
    } else {
        ("company", row.company_id)
    };
    TimelineEntry {
        id: row.id.to_string(),
        source: TimelineSource::Activity,
        occurred_at: row.occurred_at,
        kind: Some(row.kind.clone()),
        subject: Some(row.subject.clone()),
        body: (!row.body.is_empty()).then(|| row.body.clone()),
        attached_to: Some(attached_to.into()),
        attached_id,
        due_at: row.due_at,
        done_at: row.done_at,
        owner_user_id: row.owner_user_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant, so every relative-time assertion is arithmetic rather than a clock read.
    fn at(year: i32, month: time::Month, day: u8, hour: u8, minute: u8) -> OffsetDateTime {
        time::Date::from_calendar_date(year, month, day)
            .expect("a real calendar date")
            .with_hms(hour, minute, 0)
            .expect("a real time of day")
            .assume_utc()
    }

    #[test]
    fn the_four_kinds_are_the_ones_the_schema_allows() {
        for kind in ACTIVITY_KINDS {
            assert!(is_kind(kind), "{kind} should be a kind");
        }
        assert!(!is_kind("email"));
        assert!(!is_kind(""));
        assert!(!is_kind(" Call "), "trimming happens in the validator, not here");
    }

    #[test]
    fn logging_needs_a_subject() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("call".into()),
            subject: Some("   ".into()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect_err("a blank subject is not a subject");
        assert!(err.to_string().contains("subject"), "got {err}");
    }

    #[test]
    fn the_subject_is_trimmed_and_the_body_defaults_to_empty() {
        let ok = validate_activity(&ActivityChanges {
            kind: Some("note".into()),
            subject: Some("  Left a voicemail  \n".into()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect("a note with a subject is valid");
        assert_eq!(ok.subject, "Left a voicemail");
        assert_eq!(ok.body, "");
    }

    #[test]
    fn an_activity_has_to_hang_off_something() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("note".into()),
            subject: Some("Floating".into()),
            ..Default::default()
        })
        .expect_err("an activity with no record is unreachable");
        assert!(err.to_string().contains("company"), "got {err}");
    }

    #[test]
    fn two_attachments_are_refused_even_though_the_check_constraint_allows_them() {
        // `crm_activities_attached` only requires *one* of the three, so two would satisfy the
        // database while making the timeline ambiguous. The module is the stricter of the two.
        let base = ActivityChanges {
            kind: Some("note".into()),
            subject: Some("Ambiguous".into()),
            ..Default::default()
        };
        // A table of whole requests rather than a table of (a, b, c): three `Option<Uuid>`s in an
        // array literal are inferred as one type, so the `None`s pick the type of the other arm
        // and the row stops meaning what it says.
        for changes in [
            ActivityChanges {
                company_id: Some(Uuid::from_u128(1)),
                contact_id: Some(Uuid::from_u128(2)),
                ..base.clone()
            },
            ActivityChanges {
                company_id: Some(Uuid::from_u128(1)),
                deal_id: Some(Uuid::from_u128(2)),
                ..base.clone()
            },
            ActivityChanges {
                contact_id: Some(Uuid::from_u128(1)),
                deal_id: Some(Uuid::from_u128(2)),
                ..base
            },
        ] {
            let err = validate_activity(&changes)
                .expect_err("an activity hangs off exactly one record");
            assert!(err.to_string().contains("one record"), "got {err}");
        }
    }

    #[test]
    fn each_attachment_kind_is_reported_by_name() {
        let id = Uuid::from_u128(9);
        let base = ActivityChanges {
            kind: Some("note".into()),
            subject: Some("s".into()),
            ..Default::default()
        };
        for (expected, changes) in [
            (
                "company",
                ActivityChanges {
                    company_id: Some(id),
                    ..base.clone()
                },
            ),
            (
                "contact",
                ActivityChanges {
                    contact_id: Some(id),
                    ..base.clone()
                },
            ),
            (
                "deal",
                ActivityChanges {
                    deal_id: Some(id),
                    ..base
                },
            ),
        ] {
            let ok = validate_activity(&changes).expect("one attachment is valid");
            assert_eq!(ok.attached_to, expected);
            assert_eq!(ok.attached_id, id);
        }
    }

    #[test]
    fn an_unknown_kind_names_the_four_that_exist() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("email".into()),
            subject: Some("s".into()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect_err("email is not one of the four");
        let text = err.to_string();
        for kind in ACTIVITY_KINDS {
            assert!(text.contains(kind), "{text} should offer {kind}");
        }
    }

    #[test]
    fn an_oversized_subject_is_refused_by_length_not_by_bytes() {
        // 200 characters of a two-byte glyph is 400 bytes: a byte limit would refuse half of what
        // the schema's `length(subject)` (characters) accepts, and the form would look broken.
        let subject = "é".repeat(MAX_SUBJECT_LENGTH);
        assert!(validate_activity(&ActivityChanges {
            kind: Some("note".into()),
            subject: Some(subject.clone()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .is_ok());

        let err = validate_activity(&ActivityChanges {
            kind: Some("note".into()),
            subject: Some("é".repeat(MAX_SUBJECT_LENGTH + 1)),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect_err("201 characters is over the limit");
        assert!(err.to_string().contains("200"), "got {err}");
    }

    #[test]
    fn an_oversized_body_is_refused() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("note".into()),
            subject: Some("s".into()),
            body: Some("x".repeat(MAX_BODY_LENGTH + 1)),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect_err("the body has a limit too");
        assert!(err.to_string().contains("4000"), "got {err}");
    }

    #[test]
    fn a_kind_may_be_omitted_and_becomes_a_note() {
        let ok = validate_activity(&ActivityChanges {
            subject: Some("Quick note".into()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect("a note is the default kind");
        assert_eq!(ok.kind, "note");
    }

    #[test]
    fn a_task_without_a_due_date_or_a_done_mark_is_refused() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("task".into()),
            subject: Some("Call back".into()),
            contact_id: Some(Uuid::nil()),
            ..Default::default()
        })
        .expect_err("a task with no date cannot be listed");
        assert!(err.to_string().contains("due date"), "got {err}");
    }

    #[test]
    fn a_task_that_is_already_done_needs_no_due_date() {
        let ok = validate_activity(&ActivityChanges {
            kind: Some("task".into()),
            subject: Some("Sent the deck".into()),
            contact_id: Some(Uuid::nil()),
            done_at: Some(at(2026, time::Month::September, 1, 10, 0)),
            ..Default::default()
        })
        .expect("a done task is complete");
        assert!(ok.done_at.is_some());
        assert!(ok.due_at.is_none());
    }

    #[test]
    fn a_task_due_before_it_happened_is_refused() {
        let err = validate_activity(&ActivityChanges {
            kind: Some("task".into()),
            subject: Some("Call back".into()),
            contact_id: Some(Uuid::nil()),
            occurred_at: Some(at(2026, time::Month::September, 10, 12, 0)),
            due_at: Some(at(2026, time::Month::September, 1, 9, 0)),
            ..Default::default()
        })
        .expect_err("a task cannot be due before it happened");
        assert!(err.to_string().contains("before"), "got {err}");
    }

    #[test]
    fn a_finished_task_may_carry_a_due_date_in_the_past() {
        // The "due before it happened" rule guards an *open* task: a closed one records when the
        // promise was made and when it was kept, which is legitimately "due yesterday, done now".
        let ok = validate_activity(&ActivityChanges {
            kind: Some("task".into()),
            subject: Some("Call back".into()),
            contact_id: Some(Uuid::nil()),
            occurred_at: Some(at(2026, time::Month::September, 10, 12, 0)),
            due_at: Some(at(2026, time::Month::September, 9, 12, 0)),
            done_at: Some(at(2026, time::Month::September, 10, 13, 0)),
            ..Default::default()
        })
        .expect("a closed task keeps both dates");
        assert!(ok.due_at.is_some());
    }

    #[test]
    fn changed_fields_lists_only_what_was_sent() {
        let changes = ActivityChanges {
            kind: Some("call".into()),
            subject: Some("Ringed".into()),
            ..Default::default()
        };
        assert_eq!(changes.changed_fields(), vec!["kind", "subject"]);
        assert!(ActivityChanges::default().changed_fields().is_empty());
    }

    #[test]
    fn the_relative_label_reads_as_a_person_would_say_it() {
        let now = at(2026, time::Month::September, 28, 12, 0);
        for (ago, expected) in [
            (0, "just now"),
            (30, "just now"),
            (5 * 60, "5m ago"),
            (3 * 3600, "3h ago"),
            (2 * 86_400, "2d ago"),
            // The buckets are ranges, not thresholds applied to a rounded value: 14 days is
            // 1_209_600s, below the week bucket's 2_592_000s floor, so it reads "14d" and not
            // "2w". An age is more useful at a glance than a week count, which is why the
            // week label does not start at 7 days.
            (14 * 86_400, "14d ago"),
            (45 * 86_400, "6w ago"),
            (90 * 86_400, "2mo ago"),
            (400 * 86_400, "13mo ago"),
        ] {
            let then = now - time::Duration::seconds(ago);
            assert_eq!(relative_label(then, now), expected, "for {ago}s ago");
        }
    }

    #[test]
    fn a_future_due_date_says_so() {
        let now = at(2026, time::Month::September, 28, 12, 0);
        let later = now + time::Duration::hours(2);
        assert_eq!(relative_label(later, now), "in 2h");
    }

    #[test]
    fn the_relative_label_is_bounded() {
        let now = at(2026, time::Month::September, 28, 12, 0);
        // A year is a small label, but the constant exists so a future change to the format
        // cannot quietly make a list row unbounded.
        let label = relative_label(now - time::Duration::days(3650), now);
        assert!(label.len() <= MAX_RELATIVE_LABEL, "{label}");
    }

    #[test]
    fn the_source_tag_round_trips() {
        for source in [
            TimelineSource::Activity,
            TimelineSource::StageChange,
            TimelineSource::Archived,
        ] {
            assert_eq!(TimelineSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(TimelineSource::parse("nope"), None);
    }

    #[test]
    fn an_activity_becomes_a_timeline_entry_with_its_attachment_named() {
        let row = Activity {
            id: Uuid::from_u128(7),
            organization_id: Uuid::nil(),
            kind: "task".into(),
            subject: "Send the contract".into(),
            body: String::new(),
            company_id: None,
            contact_id: Some(Uuid::from_u128(8)),
            deal_id: None,
            occurred_at: at(2026, time::Month::September, 20, 9, 0),
            due_at: Some(at(2026, time::Month::September, 25, 9, 0)),
            done_at: None,
            owner_user_id: None,
            created_by: Some(Uuid::from_u128(1)),
            created_at: at(2026, time::Month::September, 20, 9, 0),
            updated_at: at(2026, time::Month::September, 20, 9, 0),
        };
        let entry = entry_from_activity(&row);
        assert_eq!(entry.source, TimelineSource::Activity);
        assert_eq!(entry.attached_to.as_deref(), Some("contact"));
        assert_eq!(entry.attached_id, Some(Uuid::from_u128(8)));
        assert!(entry.is_open_task(), "a task with a due date and no done mark is open");
        // An empty body must not arrive as `Some("")` — that is how a note starts rendering a
        // blank paragraph in the timeline.
        assert!(entry.body.is_none());
    }

    #[test]
    fn a_deal_takes_precedence_when_a_row_carries_both_ids() {
        // The write path refuses two attachments, but a row that predates the rule (or arrives
        // through a future importer) can still carry both, and the timeline has to pick one name
        // rather than render an entry attached to nothing.
        let row = Activity {
            id: Uuid::from_u128(1),
            organization_id: Uuid::nil(),
            kind: "note".into(),
            subject: "s".into(),
            body: "b".into(),
            company_id: Some(Uuid::from_u128(2)),
            contact_id: Some(Uuid::from_u128(3)),
            deal_id: Some(Uuid::from_u128(4)),
            occurred_at: at(2026, time::Month::September, 1, 0, 0),
            due_at: None,
            done_at: None,
            owner_user_id: None,
            created_by: None,
            created_at: at(2026, time::Month::September, 1, 0, 0),
            updated_at: at(2026, time::Month::September, 1, 0, 0),
        };
        let entry = entry_from_activity(&row);
        assert_eq!(entry.attached_to.as_deref(), Some("deal"));
        assert_eq!(entry.body.as_deref(), Some("b"));
    }

    #[test]
    fn a_done_task_is_not_open() {
        let mut entry = TimelineEntry {
            id: "1".into(),
            source: TimelineSource::Activity,
            occurred_at: at(2026, time::Month::September, 1, 0, 0),
            kind: Some("task".into()),
            subject: Some("s".into()),
            body: None,
            attached_to: Some("deal".into()),
            attached_id: None,
            due_at: Some(at(2026, time::Month::September, 2, 0, 0)),
            done_at: Some(at(2026, time::Month::September, 2, 1, 0)),
            owner_user_id: None,
        };
        assert!(!entry.is_open_task());
        entry.done_at = None;
        assert!(entry.is_open_task());
    }

    #[test]
    fn a_stage_change_is_not_even_a_task() {
        let entry = TimelineEntry {
            id: "stage:abc".into(),
            source: TimelineSource::StageChange,
            occurred_at: at(2026, time::Month::September, 1, 0, 0),
            kind: None,
            subject: Some("Renewal".into()),
            body: None,
            attached_to: Some("deal".into()),
            attached_id: Some(Uuid::from_u128(1)),
            due_at: Some(at(2026, time::Month::September, 5, 0, 0)),
            done_at: None,
            owner_user_id: None,
        };
        assert!(!entry.is_open_task());
    }

    #[test]
    fn a_timeline_serialises_its_arms_by_name_and_omits_what_it_has_none_of() {
        let entry = TimelineEntry {
            id: "stage:abc".into(),
            source: TimelineSource::StageChange,
            occurred_at: at(2026, time::Month::September, 1, 0, 0),
            kind: None,
            subject: Some("Renewal".into()),
            body: None,
            attached_to: Some("deal".into()),
            attached_id: Some(Uuid::from_u128(1)),
            due_at: None,
            done_at: None,
            owner_user_id: None,
        };
        let json = serde_json::to_value(&entry).expect("a timeline entry is serialisable");
        assert_eq!(json["source"], "stage_change");
        // A client that reads `due_at` and finds `null` has to special-case it; omitting the key
        // is what lets the timeline render one row shape for three arms.
        assert!(json.get("due_at").is_none(), "got {json}");
        assert!(json.get("kind").is_none(), "got {json}");
    }

    #[test]
    fn the_synthetic_id_carries_its_arm_so_two_entries_never_collide() {
        // A deal's stage change and its archive marker are two entries of one timeline. If both
        // ids were `deal:<id>` the list would render one of them twice, and a stable React key
        // would make the second overwrite the first.
        let deal = Uuid::from_u128(5);
        let stage = format!("{}:{deal}", TimelineSource::StageChange.as_str());
        let archive = format!("{}:{deal}", TimelineSource::Archived.as_str());
        assert_ne!(stage, archive);
        assert!(stage.ends_with(&deal.to_string()));
    }

    #[test]
    fn a_timeline_for_something_that_is_not_a_record_is_refused() {
        // The `async fn` is not run here (no pool); the arm is what decides the shape, and the
        // refusal is the same one the API turns into a 400.
        let err = CrmError::InvalidQuery(
            "a timeline is read for a contact, a company or a deal".to_owned(),
        );
        assert!(err.to_string().contains("contact"));
    }
}
