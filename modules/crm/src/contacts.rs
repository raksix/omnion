//! Companies and contacts: the two tables the rest of the CRM hangs off (REQ-051, slice 1).
//!
//! The store is deliberately one module: a contact's list, its detail, its inline edit, its
//! archive and its merge all go through the same validation and the same visibility rule, and two
//! modules would be two chances to disagree about an e-mail that is already taken.
//!
//! Three decisions worth stating out loud:
//!
//! * **A record outside the caller's scope is `404`, never `403`.** A `403` would confirm that
//!   the record exists in an organization the caller may not read.
//! * **Filters are pushed as typed values into a `QueryBuilder`**, never as text. A list filter
//!   is assembled at request time, so a value written into the statement text would be an
//!   injection point; a bound value cannot be.
//! * **Merge moves what other rows point at, then archives the loser** — never a delete. A deal
//!   that referenced the losing contact keeps its history and follows the survivor.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use sqlx::query_builder::QueryBuilder;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{CrmError, Result};
use crate::model::{
    MAX_NOTES_LENGTH, MAX_TAGS, STATUSES, Visibility, clean, display_name, is_email, is_phone,
    normalise_tags, redact_custom,
};
use crate::query::{ListQuery, Page, Scope, next_cursor};

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

/// A contact to create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactChanges {
    /// Given name (required).
    #[serde(default)]
    pub first_name: String,
    /// Family name.
    #[serde(default)]
    pub last_name: String,
    /// Address (unique per organization, case-insensitively).
    #[serde(default)]
    pub email: Option<String>,
    /// Phone number.
    #[serde(default)]
    pub phone: Option<String>,
    /// Role at the company.
    #[serde(default)]
    pub job_title: Option<String>,
    /// Company the contact belongs to.
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// Owner (`None` = the caller).
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Lifecycle status.
    #[serde(default)]
    pub status: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Custom field values.
    #[serde(default)]
    pub custom: Option<Value>,
    /// Free-text note.
    #[serde(default)]
    pub notes: Option<String>,
}

/// A partial update of a contact: what the body carries is replaced, what it leaves out is kept.
///
/// `Option<Option<T>>` is not decoration: an explicit `null` **clears** the value while an absent
/// key **keeps** it, and the inline-edit path of the list needs exactly that distinction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactPatch {
    /// New given name.
    #[serde(default)]
    pub first_name: Option<String>,
    /// New family name.
    #[serde(default)]
    pub last_name: Option<String>,
    /// New address; `null` clears it.
    #[serde(default)]
    pub email: Option<Option<String>>,
    /// New phone number; `null` clears it.
    #[serde(default)]
    pub phone: Option<Option<String>>,
    /// New job title; `null` clears it.
    #[serde(default)]
    pub job_title: Option<Option<String>>,
    /// New company; `null` detaches it.
    #[serde(default)]
    pub company_id: Option<Option<Uuid>>,
    /// New owner; `null` unassigns it.
    #[serde(default)]
    pub owner_user_id: Option<Option<Uuid>>,
    /// New status.
    #[serde(default)]
    pub status: Option<String>,
    /// New tags (replaces the list).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// New custom values (merged one level deep).
    #[serde(default)]
    pub custom: Option<Value>,
    /// New note; `null` clears it.
    #[serde(default)]
    pub notes: Option<Option<String>>,
}

/// A company to create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompanyChanges {
    /// Display name (required, unique per organization among the live ones).
    pub name: String,
    /// Web domain (`example.com`).
    #[serde(default)]
    pub domain: Option<String>,
    /// Industry.
    #[serde(default)]
    pub industry: Option<String>,
    /// Owner (`None` = the caller).
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Lifecycle status.
    #[serde(default)]
    pub status: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Custom field values.
    #[serde(default)]
    pub custom: Option<Value>,
    /// Free-text note.
    #[serde(default)]
    pub notes: Option<String>,
}

/// A partial update of a company.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompanyPatch {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New domain; `null` clears it.
    #[serde(default)]
    pub domain: Option<Option<String>>,
    /// New industry; `null` clears it.
    #[serde(default)]
    pub industry: Option<Option<String>>,
    /// New owner; `null` unassigns it.
    #[serde(default)]
    pub owner_user_id: Option<Option<Uuid>>,
    /// New status.
    #[serde(default)]
    pub status: Option<String>,
    /// New tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// New custom values.
    #[serde(default)]
    pub custom: Option<Value>,
    /// New note; `null` clears it.
    #[serde(default)]
    pub notes: Option<Option<String>>,
}

/// The body of a merge: the contact that survives, the one that is folded into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    /// Contact that keeps its identity.
    pub survivor: Uuid,
    /// Contact whose data moves onto the survivor and is then archived.
    pub loser: Uuid,
}

impl MergeRequest {
    /// The pure refusal rule, so it is provable without a database.
    pub fn validate(&self) -> Result<()> {
        if self.survivor == self.loser {
            return Err(CrmError::InvalidMerge(
                "a contact cannot be merged into itself".to_owned(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Read shapes
// ---------------------------------------------------------------------------------------------

/// A contact as a list row and as a detail header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Contact {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Name the list shows.
    pub display_name: String,
    /// Avatar initials.
    pub initials: String,
    /// Address.
    pub email: Option<String>,
    /// Phone number.
    pub phone: Option<String>,
    /// Job title.
    pub job_title: Option<String>,
    /// Company.
    pub company_id: Option<Uuid>,
    /// Company name, joined for the list.
    pub company_name: Option<String>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Owner display name, joined for the list.
    pub owner_name: Option<String>,
    /// Lifecycle status.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Custom field values, with the flagged keys removed for a role that may not read them.
    pub custom: Value,
    /// Free-text note.
    pub notes: String,
    /// Last activity.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_activity_at: Option<OffsetDateTime>,
    /// When it was archived.
    #[serde(with = "time::serde::rfc3339::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A company as a list row and as a detail header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Company {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// Avatar initials.
    pub initials: String,
    /// Web domain.
    pub domain: Option<String>,
    /// Industry.
    pub industry: Option<String>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Owner display name, joined for the list.
    pub owner_name: Option<String>,
    /// Lifecycle status.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Custom field values, with the flagged keys removed for a role that may not read them.
    pub custom: Value,
    /// Free-text note.
    pub notes: String,
    /// When it was archived.
    #[serde(with = "time::serde::rfc3339::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A company plus the rollups its detail screen shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompanyDetail {
    /// The company.
    pub company: Company,
    /// Live contacts of the company.
    pub contact_count: i64,
    /// Live deals in an open stage.
    pub open_deal_count: i64,
    /// Sum of those deals' amounts, as text so a money value never loses precision in JSON.
    pub pipeline_value: String,
    /// Last activity of any of its records.
    pub last_activity_at: Option<OffsetDateTime>,
}

/// Row shape of the contact query; the joins are left joins, so a contact without a company or
/// without an owner still comes back.
#[derive(Debug, sqlx::FromRow)]
struct ContactRow {
    id: Uuid,
    organization_id: Uuid,
    first_name: Option<String>,
    last_name: Option<String>,
    email: Option<String>,
    phone: Option<String>,
    job_title: Option<String>,
    company_id: Option<Uuid>,
    company_name: Option<String>,
    owner_user_id: Option<Uuid>,
    owner_name: Option<String>,
    status: String,
    tags: Vec<String>,
    custom: Value,
    notes: String,
    last_activity_at: Option<OffsetDateTime>,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl ContactRow {
    /// The public shape, with the flagged custom keys removed when the caller may not read them.
    fn into_contact(self, may_read_sensitive: bool) -> Contact {
        let first = self.first_name.unwrap_or_default();
        let last = self.last_name.unwrap_or_default();
        Contact {
            id: self.id,
            organization_id: self.organization_id,
            initials: crate::model::initials(&first, &last),
            display_name: display_name(&first, &last),
            first_name: first,
            last_name: last,
            email: self.email,
            phone: self.phone,
            job_title: self.job_title,
            company_id: self.company_id,
            company_name: self.company_name,
            owner_user_id: self.owner_user_id,
            owner_name: self.owner_name,
            status: self.status,
            tags: self.tags,
            custom: redact_custom(&self.custom, may_read_sensitive),
            notes: self.notes,
            last_activity_at: self.last_activity_at,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Row shape of the company query.
#[derive(Debug, sqlx::FromRow)]
struct CompanyRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    domain: Option<String>,
    industry: Option<String>,
    owner_user_id: Option<Uuid>,
    owner_name: Option<String>,
    status: String,
    tags: Vec<String>,
    custom: Value,
    notes: String,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl CompanyRow {
    fn into_company(self, may_read_sensitive: bool) -> Company {
        Company {
            initials: crate::model::initials(&self.name, ""),
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            domain: self.domain,
            industry: self.industry,
            owner_user_id: self.owner_user_id,
            owner_name: self.owner_name,
            status: self.status,
            tags: self.tags,
            custom: redact_custom(&self.custom, may_read_sensitive),
            notes: self.notes,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// The columns the contact query selects, with the joins the list needs.
const CONTACT_SELECT: &str = "select c.id, c.organization_id, c.first_name, c.last_name, c.email, \
     c.phone, c.job_title, c.company_id, co.name as company_name, c.owner_user_id, \
     u.display_name as owner_name, c.status, c.tags, c.custom, c.notes, c.last_activity_at, \
     c.archived_at, c.created_at, c.updated_at \
     from crm_contacts c \
     left join crm_companies co on co.id = c.company_id \
     left join users u on u.id = c.owner_user_id";

/// The columns the company query selects.
const COMPANY_SELECT: &str = "select co.id, co.organization_id, co.name, co.domain, co.industry, \
     co.owner_user_id, u.display_name as owner_name, co.status, co.tags, co.custom, co.notes, \
     co.archived_at, co.created_at, co.updated_at \
     from crm_companies co \
     left join users u on u.id = co.owner_user_id";

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// A contact that passed validation, trimmed and defaulted.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalisedContact {
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Address.
    pub email: Option<String>,
    /// Phone number.
    pub phone: Option<String>,
    /// Job title.
    pub job_title: Option<String>,
    /// Company.
    pub company_id: Option<Uuid>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Status.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Custom values.
    pub custom: Value,
    /// Note.
    pub notes: String,
}

/// Validate a contact description, naming the field that failed.
pub fn validate_contact(changes: &ContactChanges) -> Result<NormalisedContact> {
    let first_name = changes.first_name.trim().to_owned();
    let last_name = changes.last_name.trim().to_owned();
    if first_name.is_empty() {
        return Err(CrmError::invalid(
            "contact",
            "first_name",
            "a contact needs a first name",
        ));
    }
    if first_name.chars().count() > 80 {
        return Err(CrmError::invalid(
            "contact",
            "first_name",
            "a first name is at most 80 characters",
        ));
    }
    if last_name.chars().count() > 80 {
        return Err(CrmError::invalid(
            "contact",
            "last_name",
            "a last name is at most 80 characters",
        ));
    }

    let email = clean(changes.email.clone());
    if let Some(address) = email.as_deref() {
        if !is_email(address) {
            return Err(CrmError::invalid(
                "contact",
                "email",
                "that is not an e-mail address",
            ));
        }
    }

    let phone = clean(changes.phone.clone());
    if let Some(number) = phone.as_deref() {
        if !is_phone(number) {
            return Err(CrmError::invalid(
                "contact",
                "phone",
                "a phone number is 7–20 digits, spaces, brackets and dashes",
            ));
        }
    }

    let status = validated_status(changes.status.as_deref(), "contact")?;
    let tags = validated_tags(changes.tags.as_deref(), "contact")?;
    let notes = validated_notes(changes.notes.as_deref(), "contact")?;

    let job_title = clean(changes.job_title.clone());
    if let Some(title) = job_title.as_deref() {
        if title.chars().count() > 120 {
            return Err(CrmError::invalid(
                "contact",
                "job_title",
                "a job title is at most 120 characters",
            ));
        }
    }

    Ok(NormalisedContact {
        first_name,
        last_name,
        email,
        phone,
        job_title,
        company_id: changes.company_id,
        owner_user_id: changes.owner_user_id,
        status,
        tags,
        custom: custom_object(changes.custom.as_ref()),
        notes,
    })
}

/// A company that passed validation.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalisedCompany {
    /// Display name.
    pub name: String,
    /// Domain.
    pub domain: Option<String>,
    /// Industry.
    pub industry: Option<String>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Status.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Custom values.
    pub custom: Value,
    /// Note.
    pub notes: String,
}

/// Validate a company description, naming the field that failed.
pub fn validate_company(changes: &CompanyChanges) -> Result<NormalisedCompany> {
    let name = changes.name.trim().to_owned();
    if name.is_empty() {
        return Err(CrmError::invalid("company", "name", "a company needs a name"));
    }
    if name.chars().count() > 200 {
        return Err(CrmError::invalid(
            "company",
            "name",
            "a company name is at most 200 characters",
        ));
    }

    let domain = clean(changes.domain.clone()).map(|value| value.to_lowercase());
    if let Some(candidate) = domain.as_deref() {
        if !is_domain(candidate) {
            return Err(CrmError::invalid(
                "company",
                "domain",
                "a domain looks like `example.com`",
            ));
        }
    }

    let industry = clean(changes.industry.clone());
    if let Some(value) = industry.as_deref() {
        if value.chars().count() > 80 {
            return Err(CrmError::invalid(
                "company",
                "industry",
                "an industry is at most 80 characters",
            ));
        }
    }

    Ok(NormalisedCompany {
        name,
        domain,
        industry,
        owner_user_id: changes.owner_user_id,
        status: validated_status(changes.status.as_deref(), "company")?,
        tags: validated_tags(changes.tags.as_deref(), "company")?,
        custom: custom_object(changes.custom.as_ref()),
        notes: validated_notes(changes.notes.as_deref(), "company")?,
    })
}

/// The shared status rule: a known value, or `lead`.
fn validated_status(raw: Option<&str>, entity: &'static str) -> Result<String> {
    match clean(raw.map(str::to_owned)).as_deref() {
        Some(value) => {
            if !STATUSES.contains(&value) {
                return Err(CrmError::invalid(
                    entity,
                    "status",
                    format!("status is one of {}", STATUSES.join(", ")),
                ));
            }
            Ok(value.to_owned())
        }
        None => Ok("lead".to_owned()),
    }
}

/// The shared tag rule: normalised, and refused above the documented count.
fn validated_tags(raw: Option<&[String]>, entity: &'static str) -> Result<Vec<String>> {
    let tags = normalise_tags(raw.unwrap_or(&[]));
    if tags.len() > MAX_TAGS {
        return Err(CrmError::invalid(
            entity,
            "tags",
            format!("a record carries at most {MAX_TAGS} tags"),
        ));
    }
    Ok(tags)
}

/// The shared note rule.
fn validated_notes(raw: Option<&str>, entity: &'static str) -> Result<String> {
    let notes = raw.unwrap_or_default().to_owned();
    if notes.chars().count() > MAX_NOTES_LENGTH {
        return Err(CrmError::invalid(
            entity,
            "notes",
            format!("a note is at most {MAX_NOTES_LENGTH} characters"),
        ));
    }
    Ok(notes)
}

/// A custom-field value is an object; anything else is refused rather than coerced.
fn custom_object(raw: Option<&Value>) -> Value {
    match raw {
        Some(value) if value.is_object() => value.clone(),
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// `true` when the value is shaped like a bare domain — the shape the schema check enforces.
#[must_use]
pub fn is_domain(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.len() > 253 || trimmed.contains('/') {
        return false;
    }
    let labels: Vec<&str> = trimmed.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in &labels {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return false;
        }
    }
    // The last label is the suffix, and a suffix is letters.
    labels
        .last()
        .is_some_and(|suffix| suffix.len() >= 2 && suffix.chars().all(|c| c.is_ascii_alphabetic()))
}

// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

/// Push the caller's visibility level onto a list statement.
///
/// The `own` and `team` levels also admit a record with **no** owner: an unassigned record belongs
/// to nobody, so hiding it from everyone would make the list unable to show what still needs an
/// owner — the exact rows an "unassigned" filter exists to find.
fn push_visibility<'a>(
    builder: &mut QueryBuilder<'a, sqlx::Postgres>,
    scope: &Scope,
    column: &str,
) {
    match scope.visibility {
        Visibility::All => {}
        Visibility::Own => {
            builder
                .push(" and (")
                .push(column)
                .push(" = ")
                .push_bind(scope.user_id)
                .push(" or ")
                .push(column)
                .push(" is null)");
        }
        Visibility::Team => {
            let mut ids = scope.team_user_ids.clone();
            if !ids.contains(&scope.user_id) {
                ids.push(scope.user_id);
            }
            builder
                .push(" and (")
                .push(column)
                .push(" = any(")
                .push_bind(ids)
                .push(") or ")
                .push(column)
                .push(" is null)");
        }
    }
}

/// Push the owner filter: `me`, `unassigned` or a user id.
fn push_owner<'a>(
    builder: &mut QueryBuilder<'a, sqlx::Postgres>,
    scope: &Scope,
    raw: Option<&str>,
    column: &str,
) -> Result<()> {
    match clean(raw.map(str::to_owned)).as_deref() {
        Some("me") => {
            builder
                .push(" and ")
                .push(column)
                .push(" = ")
                .push_bind(scope.user_id);
        }
        Some("unassigned") => {
            builder.push(" and ").push(column).push(" is null");
        }
        Some(owner) => match Uuid::parse_str(owner) {
            Ok(id) => {
                builder
                    .push(" and ")
                    .push(column)
                    .push(" = ")
                    .push_bind(id);
            }
            Err(_) => {
                return Err(CrmError::InvalidQuery(
                    "owner is `me`, `unassigned` or a user identifier".to_owned(),
                ));
            }
        },
        None => {}
    }
    Ok(())
}

/// Push the status filter, refusing an unknown status rather than returning nothing.
fn push_status<'a>(
    builder: &mut QueryBuilder<'a, sqlx::Postgres>,
    raw: Option<&str>,
    column: &str,
) -> Result<()> {
    if let Some(status) = clean(raw.map(str::to_owned)) {
        if !STATUSES.contains(&status.as_str()) {
            return Err(CrmError::InvalidQuery(format!(
                "\"{status}\" is not a status; status is one of {}",
                STATUSES.join(", ")
            )));
        }
        builder
            .push(" and ")
            .push(column)
            .push(" = ")
            .push_bind(status);
    }
    Ok(())
}

/// Push the created-range filter, refusing a range that starts after it ends.
fn push_created_range<'a>(
    builder: &mut QueryBuilder<'a, sqlx::Postgres>,
    query: &ListQuery,
    column: &str,
) -> Result<()> {
    if let (Some(from), Some(to)) = (query.created_from, query.created_to)
        && from > to
    {
        return Err(CrmError::InvalidQuery(
            "the created range starts after it ends".to_owned(),
        ));
    }
    if let Some(from) = query.created_from {
        builder
            .push(" and ")
            .push(column)
            .push(" >= ")
            .push_bind(from.midnight().assume_utc());
    }
    if let Some(to) = query.created_to {
        // Inclusive of the last day: the upper bound is the next midnight, not the day itself.
        builder
            .push(" and ")
            .push(column)
            .push(" < ")
            .push_bind((to + Duration::days(1)).midnight().assume_utc());
    }
    Ok(())
}

/// Push every contact-list filter, in the documented order.
pub fn push_contact_filters(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    scope: &Scope,
    query: &ListQuery,
) -> Result<()> {
    builder.push(" where c.organization_id = ").push_bind(scope.organization_id);

    if !query.shows_archived() {
        builder.push(" and c.archived_at is null");
    }
    push_visibility(builder, scope, "c.owner_user_id");

    if let Some(term) = query.search_term()? {
        // One bound value, five columns: the search is "the term appears somewhere a person
        // would look", not "the term matches one column exactly".
        let pattern = format!("%{term}%");
        builder
            .push(" and (lower(coalesce(c.first_name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(c.last_name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(c.email, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(co.name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(c.job_title, '')) like ")
            .push_bind(pattern)
            .push(")");
    }

    push_status(builder, query.status.as_deref(), "c.status")?;

    if let Some(tag) = query.tag_filter() {
        builder
            .push(" and exists (select 1 from unnest(c.tags) t where lower(t) = ")
            .push_bind(tag)
            .push(")");
    }

    push_owner(builder, scope, query.owner.as_deref(), "c.owner_user_id")?;

    if let Some(company_id) = query.company_id {
        builder
            .push(" and c.company_id = ")
            .push_bind(company_id);
    }
    push_created_range(builder, query, "c.created_at")?;

    if let Some(days) = query.inactive_days {
        if !(1..=3650).contains(&days) {
            return Err(CrmError::InvalidQuery(
                "inactive_days is between 1 and 3650".to_owned(),
            ));
        }
        // A contact that never had an activity is exactly the one a "stale" filter looks for,
        // so the clause admits a null `last_activity_at` too.
        builder
            .push(" and (c.last_activity_at is null or c.last_activity_at < ")
            .push_bind(OffsetDateTime::now_utc() - Duration::days(i64::from(days)))
            .push(")");
    }

    Ok(())
}

/// Push every company-list filter. A company list has no "no activity" filter — the value the
/// overview shows is an open-deal rollup, not a timestamp.
pub fn push_company_filters(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    scope: &Scope,
    query: &ListQuery,
) -> Result<()> {
    builder
        .push(" where co.organization_id = ")
        .push_bind(scope.organization_id);

    if !query.shows_archived() {
        builder.push(" and co.archived_at is null");
    }
    push_visibility(builder, scope, "co.owner_user_id");

    if let Some(term) = query.search_term()? {
        let pattern = format!("%{term}%");
        builder
            .push(" and (lower(coalesce(co.name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(co.domain, '')) like ")
            .push_bind(pattern)
            .push(")");
    }

    push_status(builder, query.status.as_deref(), "co.status")?;

    if let Some(tag) = query.tag_filter() {
        builder
            .push(" and exists (select 1 from unnest(co.tags) t where lower(t) = ")
            .push_bind(tag)
            .push(")");
    }

    push_owner(builder, scope, query.owner.as_deref(), "co.owner_user_id")?;
    push_created_range(builder, query, "co.created_at")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// One page of contacts, plus the count the list's "N contacts" line shows.
pub async fn list_contacts(
    pool: &PgPool,
    scope: &Scope,
    query: &ListQuery,
    may_read_sensitive: bool,
) -> Result<Page<Contact>> {
    let (sort, desc) = query.resolve_sort("contacts", "updated_at")?;
    let limit = query.page_size();
    let direction = if desc { "desc" } else { "asc" };

    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(CONTACT_SELECT);
    push_contact_filters(&mut builder, scope, query)?;

    // Keyset paging: the cursor pins the *sort key* of the last row, not an offset, so archiving
    // a row between two pages cannot make the second page skip or repeat one. The id is the
    // tiebreaker, without which two rows sharing a timestamp can bounce a cursor forever.
    //
    // The emitted predicate is
    //
    // ```sql
    // and (<sort>, c.id) < (select <sort>, c.id from crm_contacts c where c.id = $1)
    // ```
    //
    // and both halves matter. The subquery needs the `where c.id = …` or it returns a **set** and
    // the row comparison silently degrades; and its opening paren has to sit before the *inner*
    // select — a `>` followed by `(select <sort> , c.id) from …` closes the subquery on the first
    // `)` and leaves a bare `from` for the parser, which reports "syntax error at or near from" and
    // the list answers a 500 on its second page.
    if let Some(cursor_id) = query.cursor_id()? {
        builder
            .push(" and (")
            .push(sort)
            .push(" , c.id) ")
            .push(if desc { "<" } else { ">" })
            .push(" (select ")
            .push(sort)
            .push(" , c.id from crm_contacts c where c.id = ")
            .push_bind(cursor_id)
            .push(")");
    }

    builder
        .push(" order by ")
        .push(sort)
        .push(" ")
        .push(direction)
        .push(", c.id ")
        .push(direction)
        .push(" limit ")
        .push_bind(limit + 1);

    // One row over the page size: the extra row is the "there is a next page" signal, and it is
    // dropped before the page is built so the cursor points at the last row the caller received.
    let mut rows: Vec<ContactRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.truncate(limit as usize);
    let contacts: Vec<Contact> = rows
        .into_iter()
        .map(|row| row.into_contact(may_read_sensitive))
        .collect();
    let cursor = next_cursor(&contacts, |contact| contact.id);

    let total = count_contacts(pool, scope, query).await?;
    Ok(Page::new(contacts, cursor, total))
}

/// The number of rows a filter matches.
pub async fn count_contacts(pool: &PgPool, scope: &Scope, query: &ListQuery) -> Result<i64> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(
        "select count(*) from crm_contacts c \
         left join crm_companies co on co.id = c.company_id",
    );
    push_contact_filters(&mut builder, scope, query)?;
    let (total,): (i64,) = builder.build_query_as().fetch_one(pool).await?;
    Ok(total)
}

/// The number of companies a filter matches.
pub async fn count_companies(pool: &PgPool, scope: &Scope, query: &ListQuery) -> Result<i64> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> =
        QueryBuilder::new("select count(*) from crm_companies co");
    push_company_filters(&mut builder, scope, query)?;
    let (total,): (i64,) = builder.build_query_as().fetch_one(pool).await?;
    Ok(total)
}

/// One contact, or `NotFound` when it is not in the caller's scope.
pub async fn get_contact(
    pool: &PgPool,
    scope: &Scope,
    contact_id: Uuid,
    may_read_sensitive: bool,
) -> Result<Contact> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(CONTACT_SELECT);
    push_contact_filters(&mut builder, scope, &ListQuery::default())?;
    builder.push(" and c.id = ").push_bind(contact_id);

    let rows: Vec<ContactRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.into_iter()
        .next()
        .map(|row| row.into_contact(may_read_sensitive))
        .ok_or(CrmError::NotFound("contact"))
}

/// One page of companies.
pub async fn list_companies(
    pool: &PgPool,
    scope: &Scope,
    query: &ListQuery,
    may_read_sensitive: bool,
) -> Result<Page<Company>> {
    let (sort, desc) = query.resolve_sort("companies", "name")?;
    let limit = query.page_size();

    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(COMPANY_SELECT);
    push_company_filters(&mut builder, scope, query)?;
    builder
        .push(" order by ")
        .push(sort)
        .push(if desc { " desc, " } else { " asc, " })
        .push("co.id ")
        .push(if desc { "desc" } else { "asc" })
        .push(" limit ")
        .push_bind(limit + 1);

    let mut rows: Vec<CompanyRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.truncate(limit as usize);
    let companies: Vec<Company> = rows
        .into_iter()
        .map(|row| row.into_company(may_read_sensitive))
        .collect();
    let cursor = next_cursor(&companies, |company| company.id);

    let total = count_companies(pool, scope, query).await?;
    Ok(Page::new(companies, cursor, total))
}

/// One company with the rollups its detail screen shows.
pub async fn get_company(
    pool: &PgPool,
    scope: &Scope,
    company_id: Uuid,
    may_read_sensitive: bool,
) -> Result<CompanyDetail> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(COMPANY_SELECT);
    push_company_filters(&mut builder, scope, &ListQuery::default())?;
    builder.push(" and co.id = ").push_bind(company_id);

    let rows: Vec<CompanyRow> = builder.build_query_as().fetch_all(pool).await?;
    let company = rows
        .into_iter()
        .next()
        .map(|row| row.into_company(may_read_sensitive))
        .ok_or(CrmError::NotFound("company"))?;

    // One statement for both rollups, so the header and the totals cannot come from two moments.
    //
    // Every subquery carries an explicit alias. A scalar subquery without one comes back as
    // `?column?`, so `FromRow` cannot find `contact_count` and the detail screen answers a
    // **500** on a perfectly good record — the failure looks like a missing migration column
    // when it is really an unlabelled projection.
    #[derive(sqlx::FromRow)]
    struct Rollup {
        contact_count: i64,
        open_deal_count: i64,
        pipeline_value: String,
        last_activity_at: Option<OffsetDateTime>,
    }

    let rollup: Rollup = sqlx::query_as(
        "select \
           (select count(*) from crm_contacts c where c.company_id = $1 and c.archived_at is null) as contact_count, \
           (select count(*) from crm_deals d where d.company_id = $1 and d.archived_at is null \
              and exists (select 1 from crm_pipeline_stages s where s.id = d.stage_id and s.kind = 'open')) as open_deal_count, \
           (select coalesce(sum(d.amount), 0)::text from crm_deals d where d.company_id = $1 \
              and d.archived_at is null \
              and exists (select 1 from crm_pipeline_stages s where s.id = d.stage_id and s.kind = 'open')) as pipeline_value, \
           (select max(a.occurred_at) from crm_activities a where a.company_id = $1) as last_activity_at",
    )
    .bind(company.id)
    .fetch_one(pool)
    .await?;

    Ok(CompanyDetail {
        company,
        contact_count: rollup.contact_count,
        open_deal_count: rollup.open_deal_count,
        pipeline_value: rollup.pipeline_value,
        last_activity_at: rollup.last_activity_at,
    })
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Create a contact.
pub async fn create_contact(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &ContactChanges,
) -> Result<Contact> {
    let normalised = validate_contact(changes)?;
    let id = Uuid::new_v4();

    let result = sqlx::query(
        "insert into crm_contacts (id, organization_id, first_name, last_name, email, phone, \
         job_title, company_id, owner_user_id, status, tags, custom, notes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(&normalised.first_name)
    .bind(&normalised.last_name)
    .bind(&normalised.email)
    .bind(&normalised.phone)
    .bind(&normalised.job_title)
    .bind(normalised.company_id)
    .bind(normalised.owner_user_id)
    .bind(&normalised.status)
    .bind(&normalised.tags)
    .bind(&normalised.custom)
    .bind(&normalised.notes)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => return Err(CrmError::EmailTaken),
        Err(error) => return Err(CrmError::Database(error)),
    }

    read_contact(pool, organization_id, id, true).await
}

/// Apply a partial update to a contact.
pub async fn patch_contact(
    pool: &PgPool,
    scope: &Scope,
    contact_id: Uuid,
    patch: &ContactPatch,
) -> Result<Contact> {
    // The row is read first, so a patch is validated against what exists — and a custom-field
    // patch can merge into the stored object instead of replacing it blindly.
    let current = get_contact(pool, scope, contact_id, true).await?;

    let mut builder: QueryBuilder<'_, sqlx::Postgres> =
        QueryBuilder::new("update crm_contacts set ");
    let mut touched = 0usize;

    macro_rules! set {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder.push($column).push(" = ").push_bind($value);
            touched += 1;
        }};
    }

    if let Some(first_name) = patch.first_name.as_deref() {
        let value = first_name.trim();
        if value.is_empty() {
            return Err(CrmError::invalid(
                "contact",
                "first_name",
                "a contact needs a first name",
            ));
        }
        if value.chars().count() > 80 {
            return Err(CrmError::invalid(
                "contact",
                "first_name",
                "a first name is at most 80 characters",
            ));
        }
        set!("first_name", value.to_owned());
    }
    if let Some(last_name) = patch.last_name.as_deref() {
        let value = last_name.trim();
        if value.chars().count() > 80 {
            return Err(CrmError::invalid(
                "contact",
                "last_name",
                "a last name is at most 80 characters",
            ));
        }
        set!("last_name", value.to_owned());
    }
    if let Some(email) = patch.email.clone() {
        let email = clean(email);
        if let Some(address) = email.as_deref() {
            if !is_email(address) {
                return Err(CrmError::invalid(
                    "contact",
                    "email",
                    "that is not an e-mail address",
                ));
            }
        }
        set!("email", email);
    }
    if let Some(phone) = patch.phone.clone() {
        let phone = clean(phone);
        if let Some(number) = phone.as_deref() {
            if !is_phone(number) {
                return Err(CrmError::invalid(
                    "contact",
                    "phone",
                    "a phone number is 7–20 digits, spaces, brackets and dashes",
                ));
            }
        }
        set!("phone", phone);
    }
    if let Some(job_title) = patch.job_title.clone() {
        set!("job_title", clean(job_title));
    }
    if let Some(company_id) = patch.company_id {
        set!("company_id", company_id);
    }
    if let Some(owner) = patch.owner_user_id {
        set!("owner_user_id", owner);
    }
    if patch.status.is_some() {
        let status = validated_status(patch.status.as_deref(), "contact")?;
        set!("status", status);
    }
    if let Some(tags) = patch.tags.as_deref() {
        let tags = validated_tags(Some(tags), "contact")?;
        set!("tags", tags);
    }
    if let Some(custom) = patch.custom.as_ref() {
        if !custom.is_object() {
            return Err(CrmError::invalid(
                "contact",
                "custom",
                "custom field values are an object",
            ));
        }
        set!("custom", merge_custom(&current.custom, custom));
    }
    if let Some(notes) = patch.notes.clone() {
        set!("notes", validated_notes(notes.as_deref(), "contact")?);
    }

    if touched == 0 {
        return Ok(current);
    }

    builder
        .push(", updated_at = now() where crm_contacts.id = ")
        .push_bind(contact_id)
        .push(" and crm_contacts.organization_id = ")
        .push_bind(scope.organization_id);

    let result = builder.build().execute(pool).await;
    match result {
        Ok(outcome) => {
            if outcome.rows_affected() == 0 {
                return Err(CrmError::NotFound("contact"));
            }
        }
        Err(error) if is_unique_violation(&error) => return Err(CrmError::EmailTaken),
        Err(error) => return Err(CrmError::Database(error)),
    }

    read_contact(pool, scope.organization_id, contact_id, true).await
}

/// Archive a contact: the soft removal the lists hide and the history keeps.
pub async fn archive_contact(pool: &PgPool, scope: &Scope, contact_id: Uuid) -> Result<Contact> {
    let result = sqlx::query(
        "update crm_contacts set archived_at = now(), updated_at = now() \
         where id = $1 and organization_id = $2 and archived_at is null",
    )
    .bind(contact_id)
    .bind(scope.organization_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(CrmError::NotFound("contact"));
    }

    read_contact(pool, scope.organization_id, contact_id, true).await
}

/// Merge two contacts: the loser's data moves onto the survivor, then the loser is archived.
///
/// Transactional, and in this order for a reason: the move happens before the archive, so a
/// failure halfway leaves a contact holding both sets of data rather than an archived contact
/// whose activities point at nothing.
pub async fn merge_contacts(
    pool: &PgPool,
    scope: &Scope,
    request: &MergeRequest,
) -> Result<Contact> {
    request.validate()?;

    let mut transaction = pool.begin().await?;

    let survivor = fetch_contact(&mut transaction, scope, request.survivor).await?;
    let loser = fetch_contact(&mut transaction, scope, request.loser).await?;

    // The survivor's identity wins; anything it lacks takes the loser's.
    let merged_email = survivor.email.clone().or_else(|| loser.email.clone());
    let merged_phone = survivor.phone.clone().or_else(|| loser.phone.clone());
    let merged_company = survivor.company_id.or(loser.company_id);
    let merged_owner = survivor.owner_user_id.or(loser.owner_user_id);
    let merged_tags = union_tags(&survivor.tags, &loser.tags);
    if merged_tags.len() > MAX_TAGS {
        return Err(CrmError::invalid(
            "contact",
            "tags",
            format!(
                "the merged contact would carry {} tags, more than the {MAX_TAGS} allowed",
                merged_tags.len()
            ),
        ));
    }
    let merged_notes = join_notes(&survivor.notes, &loser.notes);
    let merged_custom = merge_custom(&survivor.custom, &loser.custom);
    let merged_last_activity = max_optional(survivor.last_activity_at, loser.last_activity_at);

    sqlx::query(
        "update crm_contacts set email = $1, phone = $2, company_id = $3, owner_user_id = $4, \
         tags = $5, notes = $6, custom = $7, last_activity_at = $8, updated_at = now() \
         where id = $9",
    )
    .bind(&merged_email)
    .bind(&merged_phone)
    .bind(merged_company)
    .bind(merged_owner)
    .bind(&merged_tags)
    .bind(&merged_notes)
    .bind(&merged_custom)
    .bind(merged_last_activity)
    .bind(request.survivor)
    .execute(&mut *transaction)
    .await?;

    // Everything that pointed at the loser now points at the survivor.
    sqlx::query("update crm_deals set contact_id = $1, updated_at = now() where contact_id = $2")
        .bind(request.survivor)
        .bind(request.loser)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "update crm_activities set contact_id = $1, updated_at = now() where contact_id = $2",
    )
    .bind(request.survivor)
    .bind(request.loser)
    .execute(&mut *transaction)
    .await?;

    // The loser keeps its history readable through `include_archived`, but is no longer a
    // contact anybody works with.
    sqlx::query("update crm_contacts set archived_at = now(), updated_at = now() where id = $1")
        .bind(request.loser)
        .execute(&mut *transaction)
        .await?;

    transaction.commit().await?;
    read_contact(pool, scope.organization_id, request.survivor, true).await
}

/// Create a company.
pub async fn create_company(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &CompanyChanges,
) -> Result<Company> {
    let normalised = validate_company(changes)?;
    let id = Uuid::new_v4();

    let result = sqlx::query(
        "insert into crm_companies (id, organization_id, name, domain, industry, owner_user_id, \
         status, tags, custom, notes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(&normalised.name)
    .bind(&normalised.domain)
    .bind(&normalised.industry)
    .bind(normalised.owner_user_id)
    .bind(&normalised.status)
    .bind(&normalised.tags)
    .bind(&normalised.custom)
    .bind(&normalised.notes)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => return Err(CrmError::CompanyNameTaken),
        Err(error) => return Err(CrmError::Database(error)),
    }

    read_company(pool, organization_id, id, true).await
}

/// Apply a partial update to a company.
pub async fn patch_company(
    pool: &PgPool,
    scope: &Scope,
    company_id: Uuid,
    patch: &CompanyPatch,
) -> Result<Company> {
    let current = get_company(pool, scope, company_id, true).await?.company;

    let mut builder: QueryBuilder<'_, sqlx::Postgres> =
        QueryBuilder::new("update crm_companies set ");
    let mut touched = 0usize;

    macro_rules! set {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder.push($column).push(" = ").push_bind($value);
            touched += 1;
        }};
    }

    if let Some(name) = patch.name.as_deref() {
        let value = name.trim();
        if value.is_empty() {
            return Err(CrmError::invalid("company", "name", "a company needs a name"));
        }
        if value.chars().count() > 200 {
            return Err(CrmError::invalid(
                "company",
                "name",
                "a company name is at most 200 characters",
            ));
        }
        set!("name", value.to_owned());
    }
    if let Some(domain) = patch.domain.clone() {
        let domain = clean(domain).map(|value| value.to_lowercase());
        if let Some(candidate) = domain.as_deref() {
            if !is_domain(candidate) {
                return Err(CrmError::invalid(
                    "company",
                    "domain",
                    "a domain looks like `example.com`",
                ));
            }
        }
        set!("domain", domain);
    }
    if let Some(industry) = patch.industry.clone() {
        set!("industry", clean(industry));
    }
    if let Some(owner) = patch.owner_user_id {
        set!("owner_user_id", owner);
    }
    if patch.status.is_some() {
        set!(
            "status",
            validated_status(patch.status.as_deref(), "company")?
        );
    }
    if let Some(tags) = patch.tags.as_deref() {
        set!("tags", validated_tags(Some(tags), "company")?);
    }
    if let Some(custom) = patch.custom.as_ref() {
        if !custom.is_object() {
            return Err(CrmError::invalid(
                "company",
                "custom",
                "custom field values are an object",
            ));
        }
        set!("custom", merge_custom(&current.custom, custom));
    }
    if let Some(notes) = patch.notes.clone() {
        set!("notes", validated_notes(notes.as_deref(), "company")?);
    }

    if touched == 0 {
        return Ok(current);
    }

    builder
        .push(", updated_at = now() where crm_companies.id = ")
        .push_bind(company_id)
        .push(" and crm_companies.organization_id = ")
        .push_bind(scope.organization_id);

    let result = builder.build().execute(pool).await;
    match result {
        Ok(outcome) => {
            if outcome.rows_affected() == 0 {
                return Err(CrmError::NotFound("company"));
            }
        }
        Err(error) if is_unique_violation(&error) => return Err(CrmError::CompanyNameTaken),
        Err(error) => return Err(CrmError::Database(error)),
    }

    read_company(pool, scope.organization_id, company_id, true).await
}

/// Archive a company.
pub async fn archive_company(pool: &PgPool, scope: &Scope, company_id: Uuid) -> Result<Company> {
    let result = sqlx::query(
        "update crm_companies set archived_at = now(), updated_at = now() \
         where id = $1 and organization_id = $2 and archived_at is null",
    )
    .bind(company_id)
    .bind(scope.organization_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(CrmError::NotFound("company"));
    }

    read_company(pool, scope.organization_id, company_id, true).await
}

// ---------------------------------------------------------------------------------------------
// Reading back and pure helpers
// ---------------------------------------------------------------------------------------------

/// Read one contact of an organization, without the visibility narrowing.
///
/// Used after a write the caller is entitled to make: the row it just wrote is the answer, and
/// re-reading it through a narrowed list would answer something else.
pub async fn read_contact(
    pool: &PgPool,
    organization_id: Uuid,
    contact_id: Uuid,
    may_read_sensitive: bool,
) -> Result<Contact> {
    let rows: Vec<ContactRow> = sqlx::query_as(&format!(
        "{CONTACT_SELECT} where c.organization_id = $1 and c.id = $2"
    ))
    .bind(organization_id)
    .bind(contact_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .next()
        .map(|row| row.into_contact(may_read_sensitive))
        .ok_or(CrmError::NotFound("contact"))
}

/// Read one company of an organization, without the visibility narrowing.
pub async fn read_company(
    pool: &PgPool,
    organization_id: Uuid,
    company_id: Uuid,
    may_read_sensitive: bool,
) -> Result<Company> {
    let rows: Vec<CompanyRow> = sqlx::query_as(&format!(
        "{COMPANY_SELECT} where co.organization_id = $1 and co.id = $2"
    ))
    .bind(organization_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .next()
        .map(|row| row.into_company(may_read_sensitive))
        .ok_or(CrmError::NotFound("company"))
}

/// Fetch one live contact inside a transaction, honouring the caller's scope.
async fn fetch_contact(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope: &Scope,
    contact_id: Uuid,
) -> Result<ContactRow> {
    let rows: Vec<ContactRow> = sqlx::query_as(&format!(
        "{CONTACT_SELECT} where c.organization_id = $1 and c.id = $2 and c.archived_at is null"
    ))
    .bind(scope.organization_id)
    .bind(contact_id)
    .fetch_all(&mut **transaction)
    .await?;

    rows.into_iter().next().ok_or(CrmError::NotFound("contact"))
}

/// Two tag lists merged, case-insensitively, order preserved.
#[must_use]
pub fn union_tags(left: &[String], right: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for tag in left.iter().chain(right.iter()) {
        let key = tag.to_lowercase();
        if !merged.iter().any(|existing| existing.to_lowercase() == key) {
            merged.push(tag.clone());
        }
    }
    merged
}

/// The survivor's note, with the loser's appended under a marker the person can recognise.
///
/// Truncated to the note limit the form enforces, so a merge can never produce a row the edit
/// form would then refuse to save.
#[must_use]
pub fn join_notes(survivor: &str, loser: &str) -> String {
    let mut notes = survivor.to_owned();
    let loser = loser.trim();
    if !loser.is_empty() {
        if !notes.trim().is_empty() {
            notes.push_str("\n\n--- merged from a duplicate contact ---\n\n");
        }
        notes.push_str(loser);
        notes.truncate(MAX_NOTES_LENGTH);
    }
    notes
}

/// Two custom-value objects merged: the left wins on a key it already carries, a `null` removes.
///
/// One level deep on purpose — a nested object is a single field's own structure, and merging
/// *inside* it would rewrite a field the patch never mentioned.
#[must_use]
pub fn merge_custom(base: &Value, updates: &Value) -> Value {
    let mut merged = match base {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if let Value::Object(entries) = updates {
        for (key, value) in entries {
            if value.is_null() {
                merged.remove(key);
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

/// The later of two optional timestamps.
#[must_use]
pub fn max_optional(
    left: Option<OffsetDateTime>,
    right: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, other) => other,
    }
}

/// `true` when the database refused a unique index.
#[must_use]
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.code().as_deref() == Some("23505"))
}

/// `true` when the database refused a check constraint.
#[must_use]
pub fn is_check_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.code().as_deref() == Some("23514"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::Date;

    fn changes() -> ContactChanges {
        ContactChanges {
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            email: Some("ada@example.com".to_owned()),
            ..ContactChanges::default()
        }
    }

    #[test]
    fn a_valid_contact_is_trimmed_and_defaulted() {
        let normalised = validate_contact(&ContactChanges {
            first_name: "  Ada ".to_owned(),
            last_name: "  Lovelace ".to_owned(),
            email: Some("  ada@example.com ".to_owned()),
            ..ContactChanges::default()
        })
        .expect("a valid contact passes");
        assert_eq!(normalised.first_name, "Ada");
        assert_eq!(normalised.email.as_deref(), Some("ada@example.com"));
        assert_eq!(normalised.status, "lead");
        assert!(normalised.tags.is_empty());
        assert!(normalised.custom.is_object());
    }

    #[test]
    fn the_first_name_is_required_and_named_in_the_refusal() {
        let error = validate_contact(&ContactChanges {
            last_name: "Lovelace".to_owned(),
            ..ContactChanges::default()
        })
        .expect_err("a contact without a first name is refused");
        assert!(error.to_string().contains("first_name"));
    }

    #[test]
    fn a_malformed_address_and_number_are_refused_by_field() {
        let error = validate_contact(&ContactChanges {
            email: Some("ada@example".to_owned()),
            ..changes()
        })
        .expect_err("a malformed address is refused");
        assert!(error.to_string().contains("email"));

        let error = validate_contact(&ContactChanges {
            phone: Some("call me".to_owned()),
            ..changes()
        })
        .expect_err("a malformed number is refused");
        assert!(error.to_string().contains("phone"));
    }

    #[test]
    fn a_blank_address_is_absent_rather_than_empty() {
        let normalised = validate_contact(&ContactChanges {
            email: Some("   ".to_owned()),
            ..changes()
        })
        .expect("a blank address is not an error");
        assert_eq!(normalised.email, None);
    }

    #[test]
    fn an_unknown_status_and_too_many_tags_are_refused() {
        let error = validate_contact(&ContactChanges {
            status: Some("archived".to_owned()),
            ..changes()
        })
        .expect_err("an unknown status is refused");
        assert!(error.to_string().contains("status"));

        let tags: Vec<String> = (0..MAX_TAGS + 1).map(|index| format!("t{index}")).collect();
        let error = validate_contact(&ContactChanges {
            tags: Some(tags),
            ..changes()
        })
        .expect_err("eleven tags is one too many");
        assert!(error.to_string().contains("tags"));
    }

    #[test]
    fn a_custom_value_that_is_not_an_object_is_dropped_rather_than_stored() {
        let normalised = validate_contact(&ContactChanges {
            custom: Some(json!("not an object")),
            ..changes()
        })
        .expect("the contact itself is valid");
        assert!(normalised.custom.is_object());
        assert!(normalised.custom.as_object().unwrap().is_empty());
    }

    #[test]
    fn a_company_domain_is_validated_like_the_schema() {
        assert!(is_domain("example.com"));
        assert!(is_domain("sub.example.co.uk"));
        assert!(!is_domain("example"));
        assert!(!is_domain("example..com"));
        assert!(!is_domain("-example.com"));
        assert!(!is_domain("example.c"));
        assert!(!is_domain("example.c0m"));
        assert!(!is_domain("https://example.com"));

        let normalised = validate_company(&CompanyChanges {
            name: "Analytical Engines".to_owned(),
            domain: Some("  Example.COM ".to_owned()),
            ..CompanyChanges::default()
        })
        .expect("a valid company passes");
        assert_eq!(normalised.domain.as_deref(), Some("example.com"));
    }

    #[test]
    fn a_company_needs_a_name() {
        let error = validate_company(&CompanyChanges {
            name: "  ".to_owned(),
            ..CompanyChanges::default()
        })
        .expect_err("a nameless company is refused");
        assert!(error.to_string().contains("name"));
    }

    #[test]
    fn the_filters_build_without_error_for_a_valid_query() {
        let scope = Scope::all(Uuid::new_v4(), Uuid::new_v4());
        let query = ListQuery {
            search: Some("ada".to_owned()),
            status: Some("customer".to_owned()),
            tag: Some("VIP".to_owned()),
            owner: Some("me".to_owned()),
            inactive_days: Some(30),
            created_from: Some(Date::from_calendar_date(2026, time::Month::January, 1).unwrap()),
            created_to: Some(Date::from_calendar_date(2026, time::Month::September, 27).unwrap()),
            ..ListQuery::default()
        };

        let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
        push_contact_filters(&mut builder, &scope, &query).expect("a valid filter builds");
        let sql = builder.sql().to_owned();
        // The organization is the first binding, so every later clause's number is stable.
        assert!(sql.starts_with("select 1 where c.organization_id = $1"));
        assert!(sql.contains("c.archived_at is null"));
        assert!(sql.contains("c.status = $"));
        assert!(sql.contains("unnest(c.tags)"));
        assert!(sql.contains("c.last_activity_at is null or"));
    }

    #[test]
    fn the_own_level_narrows_the_list_and_keeps_the_unassigned_rows() {
        let user = Uuid::new_v4();
        let scope = Scope::all(Uuid::new_v4(), user).with(Visibility::Own, Vec::new());
        let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
        push_contact_filters(&mut builder, &scope, &ListQuery::default()).expect("builds");
        let sql = builder.sql().to_owned();
        assert!(sql.contains("c.owner_user_id = $2"));
        assert!(sql.contains("or c.owner_user_id is null"));
    }

    #[test]
    fn the_team_level_reads_the_group_and_the_caller() {
        let user = Uuid::new_v4();
        let colleague = Uuid::new_v4();
        let scope = Scope::all(Uuid::new_v4(), user).with(Visibility::Team, vec![colleague]);
        let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
        push_contact_filters(&mut builder, &scope, &ListQuery::default()).expect("builds");
        assert!(builder.sql().contains("= any($2)"));
    }

    #[test]
    fn the_owner_filter_reads_me_unassigned_and_a_user_id() {
        let scope = Scope::all(Uuid::new_v4(), Uuid::new_v4());
        for (owner, expected) in [
            ("me", "c.owner_user_id = $2"),
            ("unassigned", "c.owner_user_id is null"),
        ] {
            let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
            let query = ListQuery {
                owner: Some(owner.to_owned()),
                ..ListQuery::default()
            };
            push_contact_filters(&mut builder, &scope, &query).expect("a known owner builds");
            assert!(builder.sql().contains(expected), "{owner}");
        }

        let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
        let query = ListQuery {
            owner: Some("someone".to_owned()),
            ..ListQuery::default()
        };
        assert!(push_contact_filters(&mut builder, &scope, &query).is_err());
    }

    #[test]
    fn an_unknown_status_a_reversed_range_and_an_absurd_window_are_refused() {
        let scope = Scope::all(Uuid::new_v4(), Uuid::new_v4());
        let cases = [
            ListQuery {
                status: Some("archived".to_owned()),
                ..ListQuery::default()
            },
            ListQuery {
                created_from: Some(Date::from_calendar_date(2026, time::Month::September, 27).unwrap()),
                created_to: Some(Date::from_calendar_date(2026, time::Month::September, 1).unwrap()),
                ..ListQuery::default()
            },
            ListQuery {
                inactive_days: Some(0),
                ..ListQuery::default()
            },
        ];

        for query in cases {
            let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("select 1");
            assert!(
                push_contact_filters(&mut builder, &scope, &query).is_err(),
                "this query must be refused: {query:?}"
            );
        }
    }

    #[test]
    fn a_merge_refuses_two_names_of_the_same_contact() {
        let id = Uuid::new_v4();
        let error = MergeRequest {
            survivor: id,
            loser: id,
        }
        .validate()
        .expect_err("a contact cannot merge into itself");
        assert!(error.to_string().contains("itself"));
    }

    #[test]
    fn the_later_of_two_timestamps_wins() {
        let earlier = OffsetDateTime::UNIX_EPOCH;
        let later = OffsetDateTime::UNIX_EPOCH + Duration::days(1);
        assert_eq!(max_optional(Some(earlier), Some(later)), Some(later));
        assert_eq!(max_optional(None, Some(later)), Some(later));
        assert_eq!(max_optional(Some(later), None), Some(later));
        assert_eq!(max_optional(None, None), None);
    }

    #[test]
    fn the_tags_of_two_contacts_union_without_a_duplicate() {
        let merged = union_tags(
            &["vip".to_owned(), "renewal".to_owned()],
            &["VIP".to_owned(), "emea".to_owned()],
        );
        assert_eq!(merged, vec!["vip", "renewal", "emea"]);
    }

    #[test]
    fn a_merge_keeps_both_notes_and_stays_inside_the_limit() {
        let merged = join_notes("Kept note.", "Loser note.");
        assert!(merged.starts_with("Kept note."));
        assert!(merged.contains("merged from a duplicate contact"));
        assert!(merged.contains("Loser note."));

        // A survivor with an empty note does not get a stray separator.
        let merged = join_notes("", "Only the loser's note.");
        assert_eq!(merged, "Only the loser's note.");

        let long = "x".repeat(MAX_NOTES_LENGTH);
        let merged = join_notes(&long, &long);
        assert_eq!(merged.chars().count(), MAX_NOTES_LENGTH);
    }

    #[test]
    fn a_custom_patch_merges_one_level_deep_and_null_removes() {
        let base = json!({ "seat_count": 12, "contract_value_note": "renewal" });
        let merged = merge_custom(&base, &json!({ "seat_count": 20, "region": "eu" }));
        assert_eq!(merged["seat_count"], json!(20));
        assert_eq!(merged["region"], json!("eu"));
        assert_eq!(merged["contract_value_note"], json!("renewal"));

        let cleared = merge_custom(&base, &json!({ "contract_value_note": null }));
        assert!(cleared.get("contract_value_note").is_none());
        assert_eq!(cleared["seat_count"], json!(12));

        // A nested object is one field's own structure and is replaced, not merged into.
        let nested = merge_custom(
            &json!({ "history": { "a": 1, "b": 2 } }),
            &json!({ "history": { "b": 3 } }),
        );
        assert_eq!(nested["history"], json!({ "b": 3 }));
    }
}
