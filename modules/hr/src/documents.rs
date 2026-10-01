//! Employee documents — the cross-employee list, the attach, the remove and the expiry sweep
//! (REQ-055, slice 4's second half).
//!
//! # The bytes are not here, and the row is the whole point
//!
//! `hr_documents` holds a **media id**, never bytes: the file belongs to the media pipeline
//! (REQ-010) and the HR row is the reference, the kind, the title and the validity date. Attaching
//! is therefore not a `multipart` upload from this module — it takes a media id that already
//! exists, and the two steps are deliberately separate: the upload is retried and the metadata is
//! not, so a failed metadata write leaves an orphaned file rather than a file nobody can identify.
//!
//! # "Once" for the expiry event is a fact in the table, not a property of the cron
//!
//! A reminder automation wants one `hr.document.expiring` per document per run window. The sweep
//! therefore **claims** the rows it announces, and the claim is the `acknowledged_at` stamp in the
//! same statement that reads them: a second sweep — another cron, another writer, the same sweep
//! run twice — finds nothing to do. Without the stamp, "once" is a property of a schedule, and a
//! schedule is not a fact the database knows about.
//!
//! # This list is cross-employee on purpose
//!
//! Every other HR list is scoped to one person or one period. This one is the organization's
//! paperwork, opened when somebody asks "whose contract expires next week", and it is served by
//! the `(organization_id, kind, created_at)` and `(organization_id, expires_on)` indexes that
//! 0213 added — 0196's `(employee_id, created_at)` cannot serve it.
//!
//! # One predicate, three places
//!
//! "Expiring" is defined once ([`push_expiry_predicate`]) and read by the `?expiring=` filter, the
//! header's totals and the sweep. Three hand-written versions of the same date arithmetic is three
//! chances for a reminder to fire for a document the list calls valid.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

use crate::dates;
use crate::error::{HrError, Result};
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, MAX_SEARCH_LENGTH, Page};

/// How far ahead the sweep looks, in days. The request's own number.
pub const EXPIRY_WINDOW_DAYS: i64 = 30;

/// The kinds the schema accepts, in the order the picker shows them.
///
/// A list rather than free text because the `check` in 0196 refuses anything else anyway, and a
/// picker offering a kind the database rejects is a form that 400s on a person who picked from it.
pub const KINDS: [&str; 4] = ["contract", "id", "certificate", "other"];

/// Whether a document's kind is one the schema stores.
///
/// Exposed so a test can assert "every kind the picker offers is a kind the table accepts" — the
/// check that stops the list drifting away from a `check` constraint nobody remembers when they
/// add a row.
#[must_use]
pub fn is_known_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// One document, as the screen lists it and the detail shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// The row's id.
    pub id: Uuid,
    /// Whose document it is.
    pub employee_id: Uuid,
    /// The person's name, for the list's column.
    pub employee_name: String,
    /// Their department, for grouping.
    pub department_name: Option<String>,
    /// `contract`, `id`, `certificate` or `other`.
    pub kind: String,
    /// What the person reads on the row.
    pub title: String,
    /// The media pipeline's id — the bytes' address, never the bytes.
    pub media_id: Uuid,
    /// The validity date, absent when the document has none.
    #[serde(with = "dates::option")]
    pub expires_on: Option<Date>,
    /// Whether the expiry sweep has already claimed this row.
    pub acknowledged: bool,
    /// When it claimed it, if it did.
    #[serde(with = "dates::instant::option")]
    pub acknowledged_at: Option<OffsetDateTime>,
    /// Who attached it.
    pub uploaded_by: Option<Uuid>,
    /// When it was attached.
    #[serde(with = "dates::instant")]
    pub created_at: OffsetDateTime,
    /// Days until it expires: negative when it already has, absent when it never will.
    ///
    /// Derived on read because it is a function of **today**; stored, it would be a number that is
    /// wrong every day after the day somebody wrote it.
    pub days_until_expiry: Option<i64>,
    /// `expired`, `expiring` or `valid`, as text.
    ///
    /// The status travels in the payload rather than being derived in the browser because the three
    /// states are three different rows with three different colours, and a screen that gets the
    /// order wrong shows an expired passport in green.
    pub status: String,
}

impl Document {
    /// Whether the document is past its validity date.
    #[must_use]
    pub fn is_expired(&self, today: Date) -> bool {
        self.expires_on.is_some_and(|day| day < today)
    }

    /// Whether it expires inside the sweep's window — not yet expired, not beyond it.
    #[must_use]
    pub fn is_expiring(&self, today: Date) -> bool {
        self.expires_on
            .is_some_and(|day| day >= today && day <= today + Duration::days(EXPIRY_WINDOW_DAYS))
    }

    /// The status a badge renders.
    #[must_use]
    pub fn status_of(&self, today: Date) -> &'static str {
        if self.is_expired(today) {
            "expired"
        } else if self.is_expiring(today) {
            "expiring"
        } else {
            "valid"
        }
    }
}

/// The list's query, as the screen sends it.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DocumentQuery {
    /// Free text over the title and the employee's name.
    #[serde(default)]
    pub search: Option<String>,
    /// One of [`KINDS`], or absent for all of them.
    #[serde(default)]
    pub kind: Option<String>,
    /// Whose documents only.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// A department and everything under it.
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// `true` for the window only, `false` for its complement, absent for all.
    #[serde(default)]
    pub expiring: Option<bool>,
    /// How many rows a page holds.
    #[serde(default)]
    pub per_page: Option<i64>,
    /// The last id of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// The counts the screen's header shows, over the **filtered** rows.
///
/// A header counting the whole organization above a filtered table is a screen lying in the one
/// place somebody is looking for a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentTotals {
    /// Rows the filter matched.
    pub total: i64,
    /// Of those, past their date.
    pub expired: i64,
    /// Of those, inside the window.
    pub expiring: i64,
    /// Of those, carrying no expiry badge at all.
    pub permanent: i64,
}

/// One page of documents with the header's numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentPage {
    /// The rows.
    #[serde(flatten)]
    pub page: Page<Document>,
    /// The counts.
    pub totals: DocumentTotals,
}

/// A document as the attach form sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct NewDocument {
    /// `contract`, `id`, `certificate` or `other`.
    pub kind: String,
    /// The name the row carries.
    ///
    /// Optional: it falls back to the kind, which is what somebody who uploaded `cv.pdf` and
    /// never renamed it wants — a row called "cv.pdf" beats a row called "other".
    #[serde(default)]
    pub title: Option<String>,
    /// The media pipeline's id.
    pub media_id: Uuid,
    /// The validity date, if it has one.
    #[serde(default)]
    pub expires_on: Option<Date>,
}

/// The one definition of "expiring", read by the filter, the totals and the sweep.
///
/// The window is `today … today + 30` **inclusive**, and the badge and the sweep must agree on
/// that boundary: if one of them gains a day, a reminder fires for a document the list says is
/// valid, and the first person to notice is the one whose passport was not chased in time.
fn push_expiry_predicate(builder: &mut QueryBuilder<'_, Postgres>, expiring: bool) {
    if expiring {
        builder.push(
            "d.expires_on is not null and d.expires_on >= current_date \
             and d.expires_on <= current_date + make_interval(days => ",
        );
        builder.push_bind(EXPIRY_WINDOW_DAYS);
        builder.push(")");
    } else {
        // `false` is a positive arm, not a negation of the other one: a document with no date at
        // all is not "expiring", so `not (expiring)` written as SQL would drop the entire
        // permanent-document library — diplomas, photos, the certificate from 2004.
        builder.push("(d.expires_on is null or d.expires_on > current_date + ");
        builder.push_bind(EXPIRY_WINDOW_DAYS);
        builder.push(" days)");
    }
}

/// The filter, shared by the page query and both count queries.
///
/// `QueryBuilder` numbers its own placeholders, which is the reason this is a type rather than a
/// string: a hand-written `$2, $3, …` has to be kept in step with a hand-written binding list, and
/// the day one gains a clause and the other does not, every later placeholder shifts by one and
/// the query answers with the wrong rows and no error anywhere.
struct Filter<'a> {
    organization_id: Uuid,
    query: &'a DocumentQuery,
}

impl<'a> Filter<'a> {
    fn new(organization_id: Uuid, query: &'a DocumentQuery) -> Self {
        Self { organization_id, query }
    }

    /// Push `where …` and every binding its predicates name.
    fn push(&self, builder: &mut QueryBuilder<'_, Postgres>) {
        builder.push(" where ");
        self.push_predicates(builder);
    }

    fn push_predicates(&self, builder: &mut QueryBuilder<'_, Postgres>) {
        builder.push("d.organization_id = ");
        builder.push_bind(self.organization_id);

        if let Some(needle) = self.search_needle() {
            builder.push(" and (d.title ilike ");
            builder.push_bind(needle.clone());
            builder.push(" or e.first_name || ' ' || e.last_name ilike ");
            builder.push_bind(needle);
            builder.push(")");
        }
        if let Some(kind) = self.query.kind.as_deref().filter(|k| !k.is_empty()) {
            // Owned, not `&str`: the builder's argument lifetime is its own, so a binding that
            // borrows from `self` would have to live as long as the query rather than as long as
            // the call. Every other binding here is owned for the same reason.
            builder.push(" and d.kind = ");
            builder.push_bind(kind.to_string());
        }
        if let Some(employee_id) = self.query.employee_id {
            builder.push(" and d.employee_id = ");
            builder.push_bind(employee_id);
        }
        if let Some(department_id) = self.query.department_id {
            // The department and everything under it, as the same recursive walk the department
            // screen's own filter uses — written twice, they stop agreeing, and "Engineering"
            // then means two different sets of people on two screens.
            builder.push(
                " and d.employee_id in (select e2.id from hr_employees e2 \
                 where e2.organization_id = ",
            );
            builder.push_bind(self.organization_id);
            builder.push(" and e2.department_id in (with recursive under_dept as ( \
                 select id from hr_departments where id = ");
            builder.push_bind(department_id);
            builder.push(" union all select d2.id from hr_departments d2 \
                 join under_dept u on d2.parent_id = u.id \
                 where d2.organization_id = ");
            builder.push_bind(self.organization_id);
            builder.push(") select id from under_dept))");
        }
        if let Some(expiring) = self.query.expiring {
            builder.push(" and ");
            push_expiry_predicate(builder, expiring);
        }
    }

    /// The search term as an `ILIKE` pattern, escaped so a `%` a person types is a literal `%`.
    fn search_needle(&self) -> Option<String> {
        self.term().map(|raw| format!("%{}%", escape_like(raw)))
    }

    /// The trimmed term — empty is "no filter", because a search box sends `?search=` on every
    /// keystroke and a filter that narrows to nothing on an empty box finds nothing at all.
    fn term(&self) -> Option<&str> {
        self.query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|term| !term.is_empty())
    }

    /// Refuse a query the HTTP layer would answer better with a named error.
    fn validate(&self) -> Result<()> {
        if let Some(raw) = self.term() {
            if raw.len() > MAX_SEARCH_LENGTH {
                return Err(HrError::InvalidQuery(format!(
                    "the search term is longer than {MAX_SEARCH_LENGTH} characters"
                )));
            }
        }
        if let Some(kind) = self.query.kind.as_deref().filter(|k| !k.is_empty()) {
            if !is_known_kind(kind) {
                // The message names the alternatives: "invalid kind" alone sends the operator to
                // the schema to find out what was allowed.
                return Err(HrError::InvalidQuery(format!(
                    "'{kind}' is not a document kind — use one of {}",
                    KINDS.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// `\` `%` and `_` are the pattern metacharacters; everything else is literal.
///
/// A person searching for "50%" or "a_b" is searching for those characters. Unescaped, the first
/// matches every row and the second matches every `a` followed by any character — which reads as
/// "the search is broken" rather than "you used a wildcard".
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One page of documents, with the header's totals.
pub async fn list(
    pool: &PgPool,
    organization_id: Uuid,
    query: &DocumentQuery,
) -> Result<DocumentPage> {
    let filter = Filter::new(organization_id, query);
    filter.validate()?;

    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let page_size = usize::try_from(per_page).unwrap_or(usize::MAX);
    let today = OffsetDateTime::now_utc().date();

    let mut builder = QueryBuilder::<Postgres>::new(
        "select d.id, d.employee_id, e.first_name || ' ' || e.last_name as employee_name, \
                dep.name as department_name, d.kind, d.title, d.media_id, d.expires_on, \
                d.acknowledged_at, d.uploaded_by, d.created_at \
         from hr_documents d \
         join hr_employees e on e.id = d.employee_id \
         left join hr_departments dep on dep.id = e.department_id",
    );
    filter.push(&mut builder);
    // Soonest expiry first, undated last: the screen opens on the paperwork that is about to
    // matter. Ties break on `created_at` then `id` so the order is total and two pages cannot
    // show the same row twice or skip one.
    builder.push(" order by d.expires_on asc nulls last, d.created_at desc, d.id desc limit ");
    builder.push_bind(per_page + 1);

    let fetched: Vec<DocumentRow> = builder.build_query_as().fetch_all(pool).await?;

    // `per_page + 1` rows were asked for: the extra one is the "there is more" fact. A cursor
    // derived from a page that happened to be full, without checking, sends the caller one empty
    // page before the end — which a screen reads as "the list finished" and drops the rest.
    let has_more = fetched.len() > page_size;
    let next_cursor = if has_more {
        fetched.get(page_size.saturating_sub(1)).map(|row| row.id.to_string())
    } else {
        None
    };
    let items: Vec<Document> = fetched
        .into_iter()
        .take(page_size)
        .map(|row| row.into_document(today))
        .collect();

    let total = count(&filter, pool).await?;

    // Two counts, one statement, over the filtered rows. `permanent` is the complement rather than
    // a third filter arm, because "no date" and "a date past the window" are the same answer to
    // the screen's question: does this row carry an expiry badge?
    let (expired, expiring): (i64, i64) = {
        let mut counts = QueryBuilder::<Postgres>::new(
            "select count(*) filter (where d.expires_on is not null \
                    and d.expires_on < current_date), count(*) filter (where ",
        );
        push_expiry_predicate(&mut counts, true);
        counts.push(") from hr_documents d join hr_employees e on e.id = d.employee_id");
        filter.push(&mut counts);
        counts.build_query_as::<(i64, i64)>().fetch_one(pool).await?
    };

    Ok(DocumentPage {
        page: Page::new(items, next_cursor, total),
        totals: DocumentTotals {
            total,
            expired,
            expiring,
            permanent: total.saturating_sub(expired).saturating_sub(expiring),
        },
    })
}

/// `select count(*)` over the same predicate set the rows used.
async fn count(filter: &Filter<'_>, pool: &PgPool) -> Result<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "select count(*) from hr_documents d join hr_employees e on e.id = d.employee_id",
    );
    filter.push(&mut builder);
    Ok(builder.build_query_scalar::<i64>().fetch_one(pool).await?)
}

/// One document, by id, scoped to the organization.
pub async fn of(pool: &PgPool, organization_id: Uuid, document_id: Uuid) -> Result<Option<Document>> {
    let row = sqlx::query_as::<_, DocumentRow>(
        "select d.id, d.employee_id, e.first_name || ' ' || e.last_name as employee_name, \
                dep.name as department_name, d.kind, d.title, d.media_id, d.expires_on, \
                d.acknowledged_at, d.uploaded_by, d.created_at \
         from hr_documents d \
         join hr_employees e on e.id = d.employee_id \
         left join hr_departments dep on dep.id = e.department_id \
         where d.organization_id = $1 and d.id = $2",
    )
    .bind(organization_id)
    .bind(document_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| row.into_document(OffsetDateTime::now_utc().date())))
}

/// A row as the query returns it, before the read-time derivations.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DocumentRow {
    id: Uuid,
    employee_id: Uuid,
    employee_name: String,
    department_name: Option<String>,
    kind: String,
    title: String,
    media_id: Uuid,
    expires_on: Option<Date>,
    acknowledged_at: Option<OffsetDateTime>,
    uploaded_by: Option<Uuid>,
    created_at: OffsetDateTime,
}

impl DocumentRow {
    fn into_document(self, today: Date) -> Document {
        let days_until_expiry =
            self.expires_on
                .map(|day| i64::try_from((day - today).whole_days()).unwrap_or(i64::MAX));
        let status = match self.expires_on {
            Some(day) if day < today => "expired",
            Some(day) if day <= today + Duration::days(EXPIRY_WINDOW_DAYS) => "expiring",
            Some(_) => "valid",
            None => "valid",
        };
        Document {
            id: self.id,
            employee_id: self.employee_id,
            employee_name: self.employee_name,
            department_name: self.department_name,
            kind: self.kind,
            title: self.title,
            media_id: self.media_id,
            expires_on: self.expires_on,
            acknowledged: self.acknowledged_at.is_some(),
            acknowledged_at: self.acknowledged_at,
            uploaded_by: self.uploaded_by,
            created_at: self.created_at,
            days_until_expiry,
            status: status.to_string(),
        }
    }
}

/// Attach a document to an employee.
///
/// The employee is looked up **inside** the organization rather than trusted from the path, so a
/// uuid guessed from another tenant is a 404 — and the same 404 a genuinely missing employee
/// gives, because "that employee is not yours" and "that employee does not exist" must not be
/// distinguishable from each other.
pub async fn attach(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    actor: Uuid,
    new: &NewDocument,
) -> Result<Document> {
    if !is_known_kind(&new.kind) {
        return Err(HrError::invalid(
            "document",
            "kind",
            format!("'{}' is not a document kind", new.kind),
        ));
    }
    let title = new
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(&new.kind);
    if title.len() > 200 {
        return Err(HrError::invalid(
            "document",
            "title",
            "the title is longer than 200 characters",
        ));
    }

    let id: Uuid = sqlx::query_scalar(
        "insert into hr_documents \
           (organization_id, employee_id, kind, title, media_id, expires_on, uploaded_by) \
         select $1, $2, $3, $4, $5, $6, $7 \
         where exists (select 1 from hr_employees \
                       where id = $2 and organization_id = $1) \
         returning id",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(&new.kind)
    .bind(title)
    .bind(new.media_id)
    .bind(new.expires_on)
    .bind(actor)
    .fetch_optional(pool)
    .await?
    .ok_or(HrError::NotFound("employee"))?;

    of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("document"))
}

/// Remove a document.
///
/// The row only. The file stays in the media pipeline, because the row is a reference and deleting
/// a reference must not delete bytes another module may be pointing at — whether the file is then
/// garbage is the media pipeline's business (REQ-010), not this module's.
pub async fn remove(pool: &PgPool, organization_id: Uuid, document_id: Uuid) -> Result<()> {
    let removed =
        sqlx::query_scalar::<_, Uuid>("delete from hr_documents where organization_id = $1 and id = $2 returning id")
            .bind(organization_id)
            .bind(document_id)
            .fetch_optional(pool)
            .await?;
    if removed.is_none() {
        return Err(HrError::NotFound("document"));
    }
    Ok(())
}

/// What a sweep found and is announcing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepResult {
    /// The documents inside the window that had never been claimed.
    pub expiring: Vec<ExpiringNotice>,
    /// How many rows the window held in total, claimed or not.
    ///
    /// Carried so a run that announces nothing **because everything was already announced** is
    /// visibly a different thing from a run in an empty window — the two look identical otherwise,
    /// and the second one is a broken sweep.
    pub considered: i64,
}

/// One document the sweep is announcing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiringNotice {
    /// The document's id.
    pub document_id: Uuid,
    /// Whose it is — an id, not a name: the bus may be a third party's webhook and a name is
    /// personal data.
    pub employee_id: Uuid,
    /// Its validity date.
    #[serde(with = "dates")]
    pub expires_on: Date,
    /// Days from today.
    pub days_left: i64,
}

/// Claim and return the documents expiring inside the window, once each.
///
/// The claim **is** the stamp: `acknowledged_at is null` is the predicate and the same statement
/// sets it, so a second sweep finds nothing to do. `for update skip locked` rather than a plain
/// `update … returning` because two sweeps racing under a plain update make the second one *block*
/// and then announce the same row the first one just claimed — the busy-wait turns "once" into
/// "twice, slower".
pub async fn sweep_expiring(
    pool: &PgPool,
    organization_id: Uuid,
    today: Date,
    window_days: i64,
) -> Result<SweepResult> {
    let until = today + Duration::days(window_days);

    let rows: Vec<(Uuid, Uuid, Date)> = sqlx::query_as(
        "with due as ( \
           select id, employee_id, expires_on from hr_documents \
            where organization_id = $1 \
              and acknowledged_at is null \
              and expires_on is not null \
              and expires_on between $2 and $3 \
            for update skip locked \
         ) \
         update hr_documents d set acknowledged_at = now() \
           from due where d.id = due.id \
         returning d.id, d.employee_id, due.expires_on",
    )
    .bind(organization_id)
    .bind(today)
    .bind(until)
    .fetch_all(pool)
    .await?;

    let considered: i64 = sqlx::query_scalar(
        "select count(*) from hr_documents \
         where organization_id = $1 and expires_on is not null and expires_on between $2 and $3",
    )
    .bind(organization_id)
    .bind(today)
    .bind(until)
    .fetch_one(pool)
    .await?;

    Ok(SweepResult {
        expiring: rows
            .into_iter()
            .map(|(document_id, employee_id, expires_on)| ExpiringNotice {
                document_id,
                employee_id,
                expires_on,
                days_left: i64::try_from((expires_on - today).whole_days()).unwrap_or(i64::MAX),
            })
            .collect(),
        considered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: time::Month, day: u8) -> Date {
        Date::from_calendar_date(year, month, day).expect("a real date")
    }

    fn today() -> Date {
        day(2026, time::Month::October, 1)
    }

    fn document(expires_on: Option<Date>) -> Document {
        Document {
            id: Uuid::nil(),
            employee_id: Uuid::nil(),
            employee_name: "Ada Lovelace".into(),
            department_name: None,
            kind: "contract".into(),
            title: "Contract".into(),
            media_id: Uuid::nil(),
            expires_on,
            acknowledged: false,
            acknowledged_at: None,
            uploaded_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            days_until_expiry: None,
            status: "valid".into(),
        }
    }

    #[test]
    fn a_document_with_no_date_is_valid_and_never_expiring() {
        // "No validity date" is a real answer for a diploma or a photograph, and rendering it as
        // "expiring" would put a permanent document on a reminder list nobody reads any more.
        let doc = document(None);
        assert_eq!(doc.status_of(today()), "valid");
        assert!(!doc.is_expiring(today()));
        assert!(!doc.is_expired(today()));
        assert_eq!(doc.days_until_expiry, None);
    }

    #[test]
    fn yesterday_is_expired_and_today_is_not() {
        let now = today();
        let yesterday = document(Some(now - Duration::days(1)));
        let today_doc = document(Some(now));
        // The boundary is the interesting one: a document whose last day of validity IS today is
        // still valid today and expires tomorrow. Testing "on or before" as expired makes every
        // document look dead for a day early.
        assert_eq!(yesterday.status_of(now), "expired");
        assert_eq!(today_doc.status_of(now), "expiring");
    }

    #[test]
    fn the_window_is_thirty_days_and_closes_on_its_last_day() {
        let now = today();
        let last_day = document(Some(now + Duration::days(EXPIRY_WINDOW_DAYS)));
        let day_after = document(Some(now + Duration::days(EXPIRY_WINDOW_DAYS + 1)));
        assert_eq!(last_day.status_of(now), "expiring");
        // A 31-day-out document is valid, not expiring. A badge that says otherwise trains people
        // to ignore the badge, which is how a real expiry gets missed.
        assert_eq!(day_after.status_of(now), "valid");
    }

    #[test]
    fn a_past_date_and_a_far_future_date_are_different_answers() {
        let now = today();
        assert!(document(Some(now - Duration::days(400))).is_expired(now));
        assert!(!document(Some(now + Duration::days(400))).is_expired(now));
        assert_eq!(document(Some(now - Duration::days(400))).status_of(now), "expired");
        assert_eq!(document(Some(now + Duration::days(400))).status_of(now), "valid");
    }

    #[test]
    fn the_picker_offers_exactly_the_kinds_the_schema_accepts() {
        // 0196's `check (kind in ('contract','id','certificate','other'))` is the authority. If
        // this fails, the picker offers a value the database refuses — a form that 400s on
        // somebody for choosing from a list.
        assert!(KINDS.iter().all(|kind| is_known_kind(kind)));
        assert!(!is_known_kind("passport"));
        assert!(!is_known_kind("Contract"));
        assert!(!is_known_kind(""));
    }

    #[test]
    fn a_search_term_is_escaped_so_wildcards_are_literal() {
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like(""), "");
    }

    #[test]
    fn an_over_long_search_is_refused_before_it_reaches_the_database() {
        let query = DocumentQuery {
            search: Some("x".repeat(MAX_SEARCH_LENGTH + 1)),
            ..DocumentQuery::default()
        };
        let filter = Filter::new(Uuid::nil(), &query);
        let error = filter.validate().expect_err("the filter must refuse it");
        assert!(matches!(error, HrError::InvalidQuery(_)));
    }

    #[test]
    fn a_search_term_at_the_limit_is_accepted() {
        // The boundary belongs in a test as much as the overrun: a limit that refuses the last
        // legal character is a limit nobody can type up to.
        let query = DocumentQuery {
            search: Some("x".repeat(MAX_SEARCH_LENGTH)),
            ..DocumentQuery::default()
        };
        assert!(Filter::new(Uuid::nil(), &query).validate().is_ok());
    }

    #[test]
    fn an_unknown_kind_is_refused_naming_the_ones_that_work() {
        let query = DocumentQuery {
            kind: Some("passport".into()),
            ..DocumentQuery::default()
        };
        let error = Filter::new(Uuid::nil(), &query)
            .validate()
            .expect_err("an unknown kind must refuse");
        let message = error.to_string();
        assert!(message.contains("passport"), "{message}");
        assert!(message.contains("contract"), "{message}");
    }

    #[test]
    fn every_offered_kind_survives_the_filter_that_the_list_runs() {
        // The picker's values and the `?kind=` filter are the same list, and they drift apart the
        // moment somebody adds one in one place. This is the check: build the filter for each
        // offered kind and refuse none of them.
        for kind in KINDS {
            let query = DocumentQuery {
                kind: Some(kind.into()),
                ..DocumentQuery::default()
            };
            assert!(
                Filter::new(Uuid::nil(), &query).validate().is_ok(),
                "{kind} was offered by the picker and refused by the filter"
            );
        }
    }

    #[test]
    fn an_empty_search_is_not_a_filter() {
        // A search box that sends `?search=` on every keystroke must not narrow to nothing: the
        // list would empty itself the moment the field is cleared.
        for blank in ["", "   ", "\t"] {
            let query = DocumentQuery {
                search: Some(blank.into()),
                ..DocumentQuery::default()
            };
            let filter = Filter::new(Uuid::nil(), &query);
            assert!(filter.validate().is_ok());
            assert_eq!(filter.search_needle(), None, "{blank:?} became a filter");
        }
    }

    #[test]
    fn the_surrounding_search_is_a_wildcarded_pattern() {
        let query = DocumentQuery {
            search: Some("  ada  ".into()),
            ..DocumentQuery::default()
        };
        // Trimmed on the way in, then wrapped — and the pattern is what reaches the database, so a
        // term with a space in it still matches "Ada Lovelace".
        assert_eq!(
            Filter::new(Uuid::nil(), &query).search_needle(),
            Some("%ada%".to_string())
        );
    }

    #[test]
    fn the_window_and_the_badge_agree_day_by_day() {
        // The sweep uses `between today and today + 30`; the badge uses `>= today and <= today+30`.
        // Walk both across the whole span: the day they disagree on is the day a reminder fires
        // for a document the list calls valid.
        let now = today();
        let until = now + Duration::days(EXPIRY_WINDOW_DAYS);
        for offset in -5..=40_i64 {
            let candidate = now + Duration::days(offset);
            let in_window = candidate >= now && candidate <= until;
            assert_eq!(
                in_window,
                document(Some(candidate)).is_expiring(now),
                "the badge and the sweep disagree at {offset:+}"
            );
        }
    }

    #[test]
    fn the_row_read_and_the_status_method_agree() {
        // The payload carries `status` and the type carries `status_of`; if they ever disagree the
        // list shows one colour and the badge another. Same function, two callers.
        for offset in [-400_i64, -1, 0, 1, 29, 30, 31, 400] {
            let candidate = Some(today() + Duration::days(offset));
            let row = DocumentRow {
                id: Uuid::nil(),
                employee_id: Uuid::nil(),
                employee_name: "Ada".into(),
                department_name: None,
                kind: "contract".into(),
                title: "Contract".into(),
                media_id: Uuid::nil(),
                expires_on: candidate,
                acknowledged_at: None,
                uploaded_by: None,
                created_at: OffsetDateTime::UNIX_EPOCH,
            };
            assert_eq!(
                row.clone().into_document(today()).status,
                document(candidate).status_of(today()),
                "disagreement at {offset:+}"
            );
        }
    }

    #[test]
    fn an_undated_row_read_as_valid() {
        let row = DocumentRow {
            id: Uuid::nil(),
            employee_id: Uuid::nil(),
            employee_name: "Ada".into(),
            department_name: None,
            kind: "certificate".into(),
            title: "Diploma".into(),
            media_id: Uuid::nil(),
            expires_on: None,
            acknowledged_at: None,
            uploaded_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let read = row.into_document(today());
        assert_eq!(read.status, "valid");
        assert_eq!(read.days_until_expiry, None);
        assert!(!read.acknowledged);
    }

    #[test]
    fn an_acknowledged_row_reports_it_without_hiding_the_date() {
        // The stamp is the "we already told somebody" fact, and it is not the same fact as the
        // validity date: a sweep may claim a document that is still perfectly valid for a month.
        let stamp = OffsetDateTime::now_utc();
        let row = DocumentRow {
            id: Uuid::nil(),
            employee_id: Uuid::nil(),
            employee_name: "Ada".into(),
            department_name: None,
            kind: "id".into(),
            title: "Passport".into(),
            media_id: Uuid::nil(),
            expires_on: Some(today() + Duration::days(10)),
            acknowledged_at: Some(stamp),
            uploaded_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let read = row.into_document(today());
        assert!(read.acknowledged);
        assert_eq!(read.expires_on, Some(today() + Duration::days(10)));
        assert_eq!(read.status, "expiring");
    }

    #[test]
    fn days_until_expiry_is_negative_for_a_past_date() {
        // The count is what the badge's tooltip shows, and a screen that renders a negative number
        // as "in -3 days" instead of "expired 3 days ago" reads as a bug in the platform.
        let now = today();
        let row = DocumentRow {
            id: Uuid::nil(),
            employee_id: Uuid::nil(),
            employee_name: "Ada".into(),
            department_name: None,
            kind: "id".into(),
            title: "Passport".into(),
            media_id: Uuid::nil(),
            expires_on: Some(now - Duration::days(3)),
            acknowledged_at: None,
            uploaded_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(row.into_document(now).days_until_expiry, Some(-3));
    }
}

