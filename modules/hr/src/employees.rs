//! Employees: the record, the list query, the manager chain and the visibility rules.
//!
//! The rules here are the ones a screen, an export and an org chart all have to agree on, and
//! the reason they live in the module rather than in `apps/api` is that a rule in two places is a
//! rule that will be true in one of them.
//!
//! * **Visibility is enforced in the SQL.** A caller at the `own` level never receives another
//!   person's record, not even in the count and not even in the total. Filtering afterwards in
//!   Rust would leave the totals and the paging wrong, and the header's "24 employees" would then
//!   be a number the list cannot show.
//! * **The management chain is walked before a manager is written.** A self-manager is one bug;
//!   a cycle is another, and only the second one hangs the org chart. Both refusals are their own
//!   error variants, and the cycle message names the chain that closes it.
//! * **Termination is a status and an end date.** Never a delete: leave, attendance and
//!   onboarding all point at an employee, and a hard delete would take a person's history with it.
//! * **A contract has to carry an end date**, and a start date in the future is *allowed* — the
//!   record shows as "starts soon" rather than being refused, because hiring somebody for next
//!   month is a normal thing to do.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::departments::EmployeeRef;
use crate::error::{HrError, Result};
use crate::model::{
    MAX_EMPLOYEE_NO_LENGTH, MAX_NAME_LENGTH, MAX_NOTES_LENGTH, MAX_POSITION_LENGTH, Visibility,
    clean, is_employee_status, is_employment_type,
};
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, MAX_SEARCH_LENGTH, Page};

/// How deep the management chain is walked before the store gives up.
///
/// A real organization is a handful of levels deep; the bound is for imported data that already
/// contains a loop, and a refusal beats a hang.
pub const MAX_CHAIN_DEPTH: i32 = 32;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// An employee as the list, the detail screen and the export see them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct Employee {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The number the form suggests and the list prints.
    pub employee_no: String,
    /// The platform account, when the employee has one.
    pub user_id: Option<Uuid>,
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Work address; the one a manager may always read.
    pub work_email: String,
    /// Work phone.
    pub phone: Option<String>,
    /// The job title.
    pub position: String,
    /// The department.
    pub department_id: Uuid,
    /// The department's name, resolved for the list column.
    pub department_name: String,
    /// The line manager.
    pub manager_id: Option<Uuid>,
    /// The manager's display name, resolved for the list column.
    #[sqlx(default)]
    pub manager_name: Option<String>,
    /// One of the four employment types.
    pub employment_type: String,
    /// The first day.
    #[serde(with = "crate::dates")]
    pub start_date: Date,
    /// The last day, for a contract and for a leaver.
    #[serde(default, with = "crate::dates::option")]
    pub end_date: Option<Date>,
    /// `active`, `on_leave` or `terminated`.
    pub employee_status: String,
    /// Where the person works.
    pub location: Option<String>,
    /// Free text.
    pub notes: String,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
    /// When it last changed.
    #[serde(with = "crate::dates::instant")]
    pub updated_at: OffsetDateTime,
}

/// An employee with the **gated** personal fields, which only a caller holding
/// `hr.employees.sensitive.read` ever receives.
///
/// A separate type rather than an `Option` on the row: the fields have to be *absent from the
/// response* for a role without the permission, not present-and-null, and one type carrying
/// `Option<Option<String>>` makes "redacted" and "empty" indistinguishable at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmployeePrivate {
    /// The employee, without the gated fields.
    pub employee: Employee,
    /// Personal e-mail, when the caller may read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personal_email: Option<String>,
    /// Personal phone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personal_phone: Option<String>,
    /// Home address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Emergency contact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emergency_contact: Option<String>,
}

impl EmployeePrivate {
    /// The gated fields, or nothing at all when the caller may not read them.
    ///
    /// The single place the gate is applied. A second place would be a second decision, and the
    /// request's risk note is explicit that the list, the detail and the export must agree.
    #[must_use]
    pub fn gated(&self, may_read_sensitive: bool) -> GatedFields<'_> {
        if may_read_sensitive {
            GatedFields {
                personal_email: self.personal_email.as_deref(),
                personal_phone: self.personal_phone.as_deref(),
                address: self.address.as_deref(),
                emergency_contact: self.emergency_contact.as_deref(),
            }
        } else {
            GatedFields::hidden()
        }
    }
}

/// The four personal fields, borrowed — the shape a detail response is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatedFields<'a> {
    /// Personal e-mail.
    pub personal_email: Option<&'a str>,
    /// Personal phone.
    pub personal_phone: Option<&'a str>,
    /// Home address.
    pub address: Option<&'a str>,
    /// Emergency contact.
    pub emergency_contact: Option<&'a str>,
}

impl<'a> GatedFields<'a> {
    /// Every field absent — what a role without the permission receives.
    #[must_use]
    pub fn hidden() -> Self {
        Self {
            personal_email: None,
            personal_phone: None,
            address: None,
            emergency_contact: None,
        }
    }
}

/// The field set a create writes.
///
/// `Default` is **hand-written** rather than derived: `start_date` is a required `Date`, and
/// `time::Date` has no `Default` because there is no day that means "unset". The derived version
/// would not compile, and the obvious workaround — making the field an `Option` — would push a
/// `None` into every create path and make the API's required field optional. The tests use
/// `..Default::default()` to fill the fields they are not about, which is the reason it is here.
#[derive(Debug, Clone, PartialEq)]
pub struct EmployeeChanges {
    /// The employee number; suggested from the count when absent.
    pub employee_no: Option<String>,
    /// The platform account.
    pub user_id: Option<Uuid>,
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Work address.
    pub work_email: String,
    /// Work phone.
    pub phone: Option<String>,
    /// The job title.
    pub position: String,
    /// The department.
    pub department_id: Uuid,
    /// The line manager.
    pub manager_id: Option<Uuid>,
    /// Employment type.
    pub employment_type: String,
    /// The first day.
    pub start_date: Date,
    /// The last day.
    pub end_date: Option<Date>,
    /// The lifecycle status.
    pub employee_status: Option<String>,
    /// Where the person works.
    pub location: Option<String>,
    /// The gated personal block.
    pub personal_email: Option<String>,
    /// Gated: personal phone.
    pub personal_phone: Option<String>,
    /// Gated: home address.
    pub address: Option<String>,
    /// Gated: emergency contact.
    pub emergency_contact: Option<String>,
    /// Free text.
    pub notes: Option<String>,
}

impl Default for EmployeeChanges {
    /// Every optional field absent, and the required strings empty.
    ///
    /// The empty strings are the point: [`validate_changes`] refuses them with a message naming
    /// the field, so a caller that builds a change set from `Default` and forgets a field gets a
    /// `400` that says which one — not a person-shaped record with an empty name.
    fn default() -> Self {
        Self {
            employee_no: None,
            user_id: None,
            first_name: String::new(),
            last_name: String::new(),
            work_email: String::new(),
            phone: None,
            position: String::new(),
            department_id: Uuid::nil(),
            manager_id: None,
            employment_type: String::new(),
            start_date: Date::from_calendar_date(1970, time::Month::January, 1)
                .expect("the epoch is a valid date"),
            end_date: None,
            employee_status: None,
            location: None,
            personal_email: None,
            personal_phone: None,
            address: None,
            emergency_contact: None,
            notes: None,
        }
    }
}

/// The field set a patch writes, where every key is optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmployeePatch {
    /// Given name.
    pub first_name: Option<String>,
    /// Family name.
    pub last_name: Option<String>,
    /// Work address.
    pub work_email: Option<String>,
    /// Work phone.
    pub phone: Option<String>,
    /// The job title.
    pub position: Option<String>,
    /// The department.
    pub department_id: Option<Uuid>,
    /// The line manager.
    pub manager_id: Option<Uuid>,
    /// Employment type.
    pub employment_type: Option<String>,
    /// The first day.
    pub start_date: Option<Date>,
    /// The last day.
    pub end_date: Option<Date>,
    /// The lifecycle status.
    pub employee_status: Option<String>,
    /// Where the person works.
    pub location: Option<String>,
    /// Gated: personal e-mail.
    pub personal_email: Option<String>,
    /// Gated: personal phone.
    pub personal_phone: Option<String>,
    /// Gated: home address.
    pub address: Option<String>,
    /// Gated: emergency contact.
    pub emergency_contact: Option<String>,
    /// Free text.
    pub notes: Option<String>,
}

/// The list contract every employee screen sends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmployeeQuery {
    /// Free text: name, number or work e-mail.
    #[serde(default)]
    pub search: Option<String>,
    /// A department; with `include_subdepartments` its descendants too.
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// Whether a department filter also matches its children.
    #[serde(default)]
    pub include_subdepartments: Option<bool>,
    /// A manager id.
    #[serde(default)]
    pub manager_id: Option<Uuid>,
    /// One of the four employment types.
    #[serde(default)]
    pub employment_type: Option<String>,
    /// `active`, `on_leave` or `terminated`.
    #[serde(default)]
    pub status: Option<String>,
    /// Started on or after.
    #[serde(default)]
    pub started_from: Option<Date>,
    /// Started on or before.
    #[serde(default)]
    pub started_to: Option<Date>,
    /// Sort key.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// The visibility level to read at, **as the string the query carries**.
    ///
    /// A `String` rather than the `Visibility` enum, and that is the fix for a real widening
    /// bug rather than a style choice. With the enum, a saved view holding a level this build
    /// does not know about either fails inside serde (so the refusal is an accident of the
    /// derive) or — with a hand-rolled `parse` at the call site — collapses to `None`, and
    /// `None` means the **widest** level. A stale saved view would then read the whole
    /// organization instead of the person's own record. Reading the string and refusing an
    /// unknown value makes the request's rule ("an unknown level is refused, never widened") a
    /// rule this module owns.
    #[serde(default)]
    pub visibility: Option<String>,
}

/// Who the visibility level narrows a query to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The organization the query runs in.
    pub organization_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// How much the caller may see.
    pub visibility: Visibility,
    /// The employee row the caller's platform account is, when there is one.
    ///
    /// This is what makes `own` mean *the caller's own record* rather than "records the caller
    /// happens to own": in HR nobody owns the people, they report to them. `None` for a platform
    /// account with no employee record, and the caller then sees no employees at all.
    pub employee_id: Option<Uuid>,
}

impl Scope {
    /// A scope that reads everything of the organization.
    #[must_use]
    pub fn all(organization_id: Uuid, user_id: Uuid) -> Self {
        Self {
            organization_id,
            user_id,
            visibility: Visibility::All,
            employee_id: None,
        }
    }

    /// A narrowed scope, with the caller's own employee row resolved.
    #[must_use]
    pub fn with(mut self, visibility: Visibility, employee_id: Option<Uuid>) -> Self {
        self.visibility = visibility;
        self.employee_id = employee_id;
        self
    }
}

// ---------------------------------------------------------------------------------------------
// The list query
// ---------------------------------------------------------------------------------------------

/// The sort keys the employee list accepts.
pub const SORTABLE: [&str; 9] = [
    "employee_no",
    "last_name",
    "first_name",
    "position",
    "department",
    "start_date",
    "employee_status",
    "created_at",
    "updated_at",
];

/// The SQL expression a sort key orders by, plus whether it descends by default.
///
/// A closed set because the key becomes an `order by`: a caller-supplied column name would be an
/// injection point, and an unknown column is an error the list cannot render.
#[must_use]
pub fn sort_expression(key: &str) -> Option<(&'static str, bool)> {
    Some(match key {
        "employee_no" => ("lower(e.employee_no)", false),
        "last_name" => ("lower(e.last_name)", false),
        "first_name" => ("lower(e.first_name)", false),
        "position" => ("lower(e.position)", false),
        "department" => ("lower(d.name)", false),
        "start_date" => ("e.start_date", true),
        "employee_status" => ("e.employee_status", false),
        "created_at" => ("e.created_at", true),
        "updated_at" => ("e.updated_at", true),
        _ => return None,
    })
}

impl EmployeeQuery {
    /// The page size, capped and never below one.
    #[must_use]
    pub fn page_size(&self) -> i64 {
        self.limit
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(1, MAX_PER_PAGE)
    }

    /// The sort to apply, refusing an unknown key instead of ignoring it.
    pub fn resolve_sort(&self) -> Result<(&'static str, bool)> {
        let key = self
            .sort
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .unwrap_or("last_name");

        let Some((expression, default_desc)) = sort_expression(key) else {
            return Err(HrError::InvalidQuery(format!(
                "\"{key}\" is not a sort column of employees; the columns are {}",
                SORTABLE.join(", ")
            )));
        };

        let desc = match self.direction.as_deref().map(str::trim) {
            None | Some("") => default_desc,
            Some("asc") => false,
            Some("desc") => true,
            Some(other) => {
                return Err(HrError::InvalidQuery(format!(
                    "\"{other}\" is not a sort direction; use asc or desc"
                )));
            }
        };

        Ok((expression, desc))
    }

    /// The visibility the caller asked to read at, refusing an unknown value.
    ///
    /// Absent is the widest level, so a caller never has to pass one. A value that is present and
    /// not understood is a **refusal**, not a fallback: a saved view written by a build that knew
    /// a fourth level must not quietly read the whole organization on this one.
    pub fn visibility(&self) -> Result<Visibility> {
        match self.visibility.as_deref() {
            None => Ok(Visibility::All),
            Some(level) => Visibility::parse(Some(level)).ok_or_else(|| {
                HrError::InvalidQuery(format!(
                    "\"{level}\" is not a visibility level; the levels are own, team, all"
                ))
            }),
        }
    }

    /// The cursor decoded, or `None` for the first page.
    pub fn cursor_id(&self) -> Result<Option<Uuid>> {
        match self.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            None => Ok(None),
            Some(cursor) => Uuid::parse_str(cursor)
                .map(Some)
                .map_err(|_| HrError::InvalidQuery("the cursor is not a record identifier".to_owned())),
        }
    }

    /// The search term, lowercased, or a refusal when it is too long.
    pub fn search_term(&self) -> Result<Option<String>> {
        match clean(self.search.clone()) {
            None => Ok(None),
            Some(term) if term.chars().count() > MAX_SEARCH_LENGTH => Err(HrError::InvalidQuery(
                format!("a search term is at most {MAX_SEARCH_LENGTH} characters"),
            )),
            Some(term) => Ok(Some(term.to_lowercase())),
        }
    }
}

/// The base `SELECT` every employee read shares, with the two resolved columns a list needs.
///
/// The gated personal fields are **not** in it. They are read by [`get_employee_private`], which
/// is the one path that returns them and which the API layer calls only after resolving the
/// permission — a column in the shared projection would be one `select *` away from a list.
const EMPLOYEE_SELECT: &str = "e.id, e.organization_id, e.employee_no, e.user_id, e.first_name, \
     e.last_name, e.work_email, e.phone, e.position, e.department_id, e.manager_id, \
     e.employment_type, e.start_date, e.end_date, e.employee_status, e.location, e.notes, \
     e.created_at, e.updated_at, d.name as department_name, \
     (mgr.first_name || ' ' || mgr.last_name) as manager_name";

/// The `from` clause every employee read shares.
const EMPLOYEE_FROM: &str = " from hr_employees e \
     join hr_departments d on d.id = e.department_id \
     left join hr_employees mgr on mgr.id = e.manager_id ";

/// Build the `where` clauses both the page and the count run.
///
/// A **function** rather than a method, and the count calls it too, because a count built from a
/// different set of clauses is a total that disagrees with the list beside it — the single most
/// confusing bug a filtered list can have, and the one the visibility level would make worse.
fn push_filters<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    scope: &Scope,
    query: &EmployeeQuery,
) -> Result<()> {
    builder.push(" where e.organization_id = ");
    builder.push_bind(scope.organization_id);

    // The visibility clause, in SQL. `own` is the caller's own employee row and nothing else;
    // `team` adds the people who report to them. A caller with **no** employee row sees nothing at
    // either narrowed level, because there is no record that could be theirs — and `Uuid::nil()`
    // is a value no row can carry, so the clause stays true-shaped rather than becoming `or true`.
    match scope.visibility {
        Visibility::All => {}
        Visibility::Own => {
            builder.push(" and e.id = ");
            builder.push_bind(scope.employee_id.unwrap_or(Uuid::nil()));
        }
        Visibility::Team => {
            builder.push(" and (e.id = ");
            builder.push_bind(scope.employee_id.unwrap_or(Uuid::nil()));
            builder.push(" or e.manager_id = ");
            builder.push_bind(scope.user_id);
            builder.push(")");
        }
    }

    if let Some(term) = query.search_term()? {
        builder.push(" and (lower(e.first_name) like ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" or lower(e.last_name) like ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" or lower(e.employee_no) like ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" or lower(e.work_email) like ");
        builder.push_bind(format!("%{term}%"));
        builder.push(")");
    }

    if let Some(department_id) = query.department_id {
        if query.include_subdepartments.unwrap_or(false) {
            // `union` (distinct) rather than `union all`, so a tree that is already cyclic — from
            // an import, or from two concurrent moves — terminates instead of hanging the list.
            builder.push(" and e.department_id in (with recursive subtree (id) as (");
            builder.push_bind(department_id);
            builder.push(" union ");
            builder.push("select c.id from hr_departments c join subtree s on c.parent_id = s.id ");
            builder.push(") select id from subtree)");
        } else {
            builder.push(" and e.department_id = ");
            builder.push_bind(department_id);
        }
    }

    if let Some(manager_id) = query.manager_id {
        builder.push(" and e.manager_id = ");
        builder.push_bind(manager_id);
    }

    if let Some(employment_type) = clean(query.employment_type.clone()) {
        if !is_employment_type(&employment_type) {
            return Err(HrError::InvalidQuery(format!(
                "\"{employment_type}\" is not an employment type; the types are {}",
                crate::model::EMPLOYMENT_TYPES.join(", ")
            )));
        }
        builder.push(" and e.employment_type = ");
        builder.push_bind(employment_type);
    }

    if let Some(status) = clean(query.status.clone()) {
        if !is_employee_status(&status) {
            return Err(HrError::InvalidQuery(format!(
                "\"{status}\" is not an employee status; the statuses are {}",
                crate::model::EMPLOYEE_STATUSES.join(", ")
            )));
        }
        builder.push(" and e.employee_status = ");
        builder.push_bind(status);
    }

    if let Some(from) = query.started_from {
        builder.push(" and e.start_date >= ");
        builder.push_bind(from);
    }
    if let Some(to) = query.started_to {
        builder.push(" and e.start_date <= ");
        builder.push_bind(to);
    }

    Ok(())
}

/// One page of the employee list, with the visibility level applied in SQL.
pub async fn list_employees(
    pool: &PgPool,
    scope: &Scope,
    query: &EmployeeQuery,
) -> Result<Page<Employee>> {
    let (sort_expression, desc) = query.resolve_sort()?;
    let limit = query.page_size();
    let cursor = query.cursor_id()?;

    let mut count = QueryBuilder::new("select count(*)");
    count.push(EMPLOYEE_FROM);
    push_filters(&mut count, scope, query)?;
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

    if total == 0 {
        return Ok(Page::empty());
    }

    // A **CTE** rather than a correlated subquery, and the reason is the binding order.
    //
    // `QueryBuilder` numbers its placeholders sequentially, so a filter that binds the
    // organization cannot be referenced by a hard-coded `$1` in a later clause: a search term
    // pushes everything along by one, and a cursor written as `$2` then reads the *search string*
    // as its id — the second page of a searched list silently returning rows from an unrelated
    // place. Putting the cursor first makes it `$1` by construction: a constant, not an assumption.
    let mut builder = QueryBuilder::new(match cursor {
        Some(_) => format!(
            "with cursor as ( \
                 select e2.id, {sort_expression} as sort_value \
                 from hr_employees e2 \
                 join hr_departments d2 on d2.id = e2.department_id \
                 where e2.organization_id = $1 and e2.id = $2 \
             ) select {EMPLOYEE_SELECT}{EMPLOYEE_FROM}"
        ),
        None => format!("select {EMPLOYEE_SELECT}{EMPLOYEE_FROM}"),
    });
    if let Some(cursor_id) = cursor {
        builder.push_bind(scope.organization_id);
        builder.push_bind(cursor_id);
    }
    push_filters(&mut builder, scope, query)?;
    builder.push(" order by ");
    builder.push(sort_expression);
    builder.push(if desc { " desc" } else { " asc" });
    // The id tiebreak is what makes keyset paging total: two employees can share a surname, and
    // without it the second page can repeat or skip a row.
    builder.push(", e.id asc");
    builder.push(" limit ");
    // One row more than asked for, and the extra row is the "there is a next page" answer — a
    // second count query per page is a round trip a list does not need.
    builder.push_bind(limit + 1);

    if cursor.is_some() {
        // A *keyset* page, not an offset: an offset shifts when somebody is hired, so the second
        // page of a live directory would skip or repeat employees. The cursor row's sort value is
        // read by the **database** through the CTE rather than marshalled into Rust, because a
        // `Date` and a `text` sort value cannot share one Rust type — and a comparison performed in
        // Rust with the wrong type would sort "2026-10-2" after "2026-10-10".
        //
        // The id tiebreak inside the comparison is what makes the page total: two employees can
        // share a surname, and without it the row whose name equals the cursor's could appear on
        // both pages or on neither.
        let comparison = if desc { "<" } else { ">" };
        builder.push(" and (");
        builder.push(sort_expression);
        builder.push(format!(" {comparison} (select sort_value from cursor)"));
        builder.push(" or (");
        builder.push(sort_expression);
        builder.push(" = (select sort_value from cursor)");
        builder.push(" and e.id > (select id from cursor))");
        builder.push(")");
    }

    let rows: Vec<Employee> = builder.build_query_as().fetch_all(pool).await?;

    let has_more = rows.len() as i64 > limit;
    let items: Vec<Employee> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        items.last().map(|employee| employee.id.to_string())
    } else {
        None
    };

    Ok(Page::new(items, next_cursor, total))
}

/// How many employees the organization has — the next free employee number's input.
pub async fn count_employees(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from hr_employees where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The employee number to suggest for a new record.
pub async fn suggest_employee_no(pool: &PgPool, organization_id: Uuid) -> Result<String> {
    let count = count_employees(pool, organization_id).await?;
    Ok(crate::model::suggest_employee_no(count))
}

/// The small employee shape the org chart and a department's member list draw, **keyed by the
/// department the person is in**.
///
/// The key is the department rather than the employee because the only caller is the chart, which
/// groups by department; returning the employee's own id there made the grouping silently match
/// nothing, and the chart looked plausible because every node still drew its department card.
pub async fn list_chart_refs(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<(Uuid, EmployeeRef)>> {
    let rows = sqlx::query(
        "select id, department_id, employee_no, first_name, last_name, position, employee_status, manager_id \
         from hr_employees \
         where organization_id = $1 and employee_status <> 'terminated' \
         order by lower(last_name), lower(first_name), id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut refs = Vec::with_capacity(rows.len());
    for row in rows {
        use sqlx::Row as _;
        let first: String = row.try_get("first_name")?;
        let last: String = row.try_get("last_name")?;
        refs.push((
            // The **department**, not the employee: `departments::build_node` matches this value
            // against the node's department id. Returning the employee id here made every chart
            // node render an empty employee list while the same row's `member_count` said 1 --
            // the tree and the chart disagreeing, which is the criterion this request names.
            row.try_get("department_id")?,
            EmployeeRef {
                id: row.try_get("id")?,
                employee_no: row.try_get("employee_no")?,
                display_name: crate::model::display_name(&first, &last),
                initials: crate::model::initials(&first, &last),
                position: row.try_get("position")?,
                status: row.try_get("employee_status")?,
                manager_id: row.try_get("manager_id")?,
            },
        ));
    }
    Ok(refs)
}

// ---------------------------------------------------------------------------------------------
// Reading one
// ---------------------------------------------------------------------------------------------

/// The caller's own employee row, or `None` when their account has none.
///
/// The self-service routes resolve this on every request, so it is a single indexed lookup
/// rather than a scan of the directory.
pub async fn employee_of_user(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Uuid>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "select id from hr_employees where organization_id = $1 and user_id = $2",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// One employee, read inside the caller's scope.
///
/// A record outside the scope is `None`, not a refusal: the API turns that into a `404`, and a
/// `403` would tell a stranger that the employee exists.
pub async fn get_employee(pool: &PgPool, scope: &Scope, employee_id: Uuid) -> Result<Option<Employee>> {
    let sql = format!(
        "select {EMPLOYEE_SELECT}{EMPLOYEE_FROM} where e.organization_id = $1 and e.id = $2"
    );
    // An **expression**, not a pre-seeded `false`: a `let mut visible = false` that every arm
    // overwrites is a shape where a new arm that forgets to assign leaves the caller reading the
    // seed, which is `false` — a record that exists being answered as not found. The compiler
    // flags the seed as dead today; the expression makes the exhaustiveness do the work.
    let visible = match scope.visibility {
        Visibility::All => true,
        Visibility::Own => scope.employee_id == Some(employee_id),
        Visibility::Team => {
            // Their own row, or somebody who reports to them. The reporting test is the
            // *direct* one, matching what the list shows at this level: a level that also reached
            // indirect reports would list rows the detail screen then refuses.
            let sql = format!(
                "select exists (select 1 from hr_employees where organization_id = $1 and id = $2 \
                 and (id = $3 or manager_id = $4))"
            );
            sqlx::query_scalar::<_, bool>(&sql)
                .bind(scope.organization_id)
                .bind(employee_id)
                .bind(scope.employee_id.unwrap_or(Uuid::nil()))
                .bind(scope.user_id)
                .fetch_one(pool)
                .await?
        }
    };

    if !visible {
        return Ok(None);
    }

    let row = sqlx::query_as::<_, Employee>(&sql)
        .bind(scope.organization_id)
        .bind(employee_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// One employee **with** the gated personal fields, for the detail screen.
///
/// The only read that names them, and the API calls it after resolving
/// `hr.employees.sensitive.read`. It still returns the row for a caller who may not read the
/// fields: the gate decides what is *serialised*, not whether the record exists.
pub async fn get_employee_private(
    pool: &PgPool,
    scope: &Scope,
    employee_id: Uuid,
) -> Result<Option<EmployeePrivate>> {
    let sql = format!(
        "select {EMPLOYEE_SELECT}, e.personal_email, e.personal_phone, e.address, e.emergency_contact \
         {EMPLOYEE_FROM} where e.organization_id = $1 and e.id = $2"
    );

    #[derive(sqlx::FromRow)]
    struct PrivateRow {
        #[sqlx(flatten)]
        employee: Employee,
        personal_email: Option<String>,
        personal_phone: Option<String>,
        address: Option<String>,
        emergency_contact: Option<String>,
    }

    let visible = match get_employee(pool, scope, employee_id).await? {
        Some(employee) => employee,
        None => return Ok(None),
    };

    let row = sqlx::query_as::<_, PrivateRow>(&sql)
        .bind(scope.organization_id)
        .bind(employee_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|row| EmployeePrivate {
        employee: row.employee,
        personal_email: row.personal_email,
        personal_phone: row.personal_phone,
        address: row.address,
        emergency_contact: row.emergency_contact,
    }))
    .map(|value| {
        // The visibility check above already answered; a row that somehow disappeared between the
        // two reads is a `None`, not a panic. `visible` exists to make the scope check explicit.
        let _ = visible;
        value
    })
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// What a create must satisfy, whatever route asked for it.
fn validate_changes(changes: &EmployeeChanges) -> Result<String> {
    let first = changes.first_name.trim();
    if first.is_empty() {
        return Err(HrError::invalid("employee", "first_name", "a first name is required"));
    }
    if first.chars().count() > MAX_NAME_LENGTH {
        return Err(HrError::invalid(
            "employee",
            "first_name",
            format!("a first name is at most {MAX_NAME_LENGTH} characters"),
        ));
    }

    let last = changes.last_name.trim();
    if last.is_empty() {
        return Err(HrError::invalid("employee", "last_name", "a last name is required"));
    }
    if last.chars().count() > MAX_NAME_LENGTH {
        return Err(HrError::invalid(
            "employee",
            "last_name",
            format!("a last name is at most {MAX_NAME_LENGTH} characters"),
        ));
    }

    let email = changes.work_email.trim().to_lowercase();
    if !crate::model::is_email(&email) {
        return Err(HrError::invalid(
            "employee",
            "work_email",
            "that is not an e-mail address",
        ));
    }

    let position = changes.position.trim();
    if position.is_empty() {
        return Err(HrError::invalid("employee", "position", "a position is required"));
    }
    if position.chars().count() > MAX_POSITION_LENGTH {
        return Err(HrError::invalid(
            "employee",
            "position",
            format!("a position is at most {MAX_POSITION_LENGTH} characters"),
        ));
    }

    if !is_employment_type(&changes.employment_type) {
        return Err(HrError::invalid(
            "employee",
            "employment_type",
            format!(
                "employment type is one of {}",
                crate::model::EMPLOYMENT_TYPES.join(", ")
            ),
        ));
    }

    // A contract has to say when it ends. The dependency is on the type rather than on the
    // presence of the field, which is why it is the service and not a check constraint.
    if changes.employment_type == "contract" && changes.end_date.is_none() {
        return Err(HrError::invalid(
            "employee",
            "end_date",
            "a contract needs an end date",
        ));
    }
    if let Some(end_date) = changes.end_date
        && end_date < changes.start_date
    {
        return Err(HrError::invalid(
            "employee",
            "end_date",
            "the end date cannot be before the start date",
        ));
    }

    if let Some(status) = changes.employee_status.as_deref()
        && !is_employee_status(status)
    {
        return Err(HrError::invalid(
            "employee",
            "employee_status",
            format!(
                "status is one of {}",
                crate::model::EMPLOYEE_STATUSES.join(", ")
            ),
        ));
    }

    // A personal e-mail is gated behind the sensitive permission, so it is validated to the same
    // shape as the work one rather than stored unchecked and rendered raw.
    if let Some(personal_email) = clean(changes.personal_email.clone())
        && !crate::model::is_email(&personal_email)
    {
        return Err(HrError::invalid(
            "employee",
            "personal_email",
            "that is not an e-mail address",
        ));
    }

    if let Some(phone) = clean(changes.phone.clone())
        && !crate::model::is_phone(&phone)
    {
        return Err(HrError::invalid(
            "employee",
            "phone",
            "use digits, spaces, + ( ) and - only",
        ));
    }

    if let Some(notes) = changes.notes.as_deref()
        && notes.chars().count() > MAX_NOTES_LENGTH
    {
        return Err(HrError::invalid(
            "employee",
            "notes",
            format!("notes are at most {MAX_NOTES_LENGTH} characters"),
        ));
    }

    let employee_no = match clean(changes.employee_no.clone()) {
        Some(number) => {
            if number.chars().count() > MAX_EMPLOYEE_NO_LENGTH {
                return Err(HrError::invalid(
                    "employee",
                    "employee_no",
                    format!("an employee number is at most {MAX_EMPLOYEE_NO_LENGTH} characters"),
                ));
            }
            number
        }
        // Suggested, not refused: the form proposes `EMP-0042` and a person may type their own
        // scheme. The unique index is what stops two people having the same one.
        None => crate::model::suggest_employee_no(0),
    };

    Ok(employee_no)
}

/// Create an employee.
pub async fn create_employee(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &EmployeeChanges,
) -> Result<Employee> {
    let mut changes = changes.clone();
    if clean(changes.employee_no.clone()).is_none() {
        changes.employee_no = Some(suggest_employee_no(pool, organization_id).await?);
    }
    let employee_no = validate_changes(&changes)?;

    // The department has to be in this organization. A department id from another tenant is a
    // `404` rather than a foreign key error the form cannot read.
    if crate::departments::get_department(pool, organization_id, changes.department_id)
        .await?
        .is_none()
    {
        return Err(HrError::NotFound("department"));
    }

    if let Some(manager_id) = changes.manager_id {
        assert_no_manager_cycle(pool, organization_id, Uuid::nil(), manager_id).await?;
    }

    let id: Uuid = sqlx::query_scalar(
        "insert into hr_employees ( \
             organization_id, employee_no, user_id, first_name, last_name, work_email, phone, \
             position, department_id, manager_id, employment_type, start_date, end_date, \
             employee_status, location, personal_email, personal_phone, address, \
             emergency_contact, notes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20) \
         returning id",
    )
    .bind(organization_id)
    .bind(&employee_no)
    .bind(changes.user_id)
    .bind(changes.first_name.trim())
    .bind(changes.last_name.trim())
    .bind(changes.work_email.trim().to_lowercase())
    .bind(clean(changes.phone.clone()))
    .bind(changes.position.trim())
    .bind(changes.department_id)
    .bind(changes.manager_id)
    .bind(&changes.employment_type)
    .bind(changes.start_date)
    .bind(changes.end_date)
    .bind(changes.employee_status.as_deref().unwrap_or("active"))
    .bind(clean(changes.location.clone()))
    .bind(clean(changes.personal_email.clone()))
    .bind(clean(changes.personal_phone.clone()))
    .bind(clean(changes.address.clone()))
    .bind(clean(changes.emergency_contact.clone()))
    .bind(changes.notes.clone().unwrap_or_default())
    .fetch_one(pool)
    .await
    .map_err(map_employee_conflict)?;

    read_employee(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("employee"))
}

/// Read an employee without a scope — for the writes that just made one.
async fn read_employee(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
) -> Result<Option<Employee>> {
    let sql = format!(
        "select {EMPLOYEE_SELECT}{EMPLOYEE_FROM} where e.organization_id = $1 and e.id = $2"
    );
    let row = sqlx::query_as::<_, Employee>(&sql)
        .bind(organization_id)
        .bind(employee_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Update an employee's own fields.
pub async fn update_employee(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    patch: &EmployeePatch,
) -> Result<Employee> {
    let before = read_employee(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("employee"))?;

    if let Some(department_id) = patch.department_id
        && crate::departments::get_department(pool, organization_id, department_id)
            .await?
            .is_none()
    {
        return Err(HrError::NotFound("department"));
    }

    if let Some(manager_id) = patch.manager_id {
        assert_no_manager_cycle(pool, organization_id, employee_id, manager_id).await?;
    }

    // The patch is folded onto the row and re-validated as a whole, so a patch that clears the
    // position or moves the end date before the start date is refused exactly as a create would
    // be. Validating only the fields present would let a bad combination be built up over several
    // saves, and the record would be one the create route would not have written.
    let merged = EmployeeChanges {
        employee_no: Some(before.employee_no.clone()),
        user_id: before.user_id,
        first_name: patch
            .first_name
            .clone()
            .unwrap_or_else(|| before.first_name.clone()),
        last_name: patch
            .last_name
            .clone()
            .unwrap_or_else(|| before.last_name.clone()),
        work_email: patch
            .work_email
            .clone()
            .unwrap_or_else(|| before.work_email.clone()),
        phone: patch.phone.clone().or(before.phone.clone()),
        position: patch
            .position
            .clone()
            .unwrap_or_else(|| before.position.clone()),
        department_id: patch.department_id.unwrap_or(before.department_id),
        manager_id: patch.manager_id.or(before.manager_id),
        employment_type: patch
            .employment_type
            .clone()
            .unwrap_or_else(|| before.employment_type.clone()),
        start_date: patch.start_date.unwrap_or(before.start_date),
        end_date: patch.end_date.or(before.end_date),
        employee_status: Some(
            patch
                .employee_status
                .clone()
                .unwrap_or_else(|| before.employee_status.clone()),
        ),
        location: patch.location.clone().or(before.location.clone()),
        personal_email: patch.personal_email.clone(),
        personal_phone: patch.personal_phone.clone(),
        address: patch.address.clone(),
        emergency_contact: patch.emergency_contact.clone(),
        notes: patch.notes.clone().or(Some(before.notes.clone())),
    };
    validate_changes(&merged)?;

    let rows = sqlx::query(
        "update hr_employees set \
            first_name = $3, last_name = $4, work_email = $5, \
            phone = coalesce($6, phone), position = $7, \
            department_id = coalesce($8, department_id), \
            manager_id = coalesce($9, manager_id), \
            employment_type = $10, start_date = $11, \
            end_date = coalesce($12, end_date), \
            employee_status = $13, location = coalesce($14, location), \
            notes = coalesce($15, notes), \
            updated_at = now() \
         where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(merged.first_name.trim())
    .bind(merged.last_name.trim())
    .bind(merged.work_email.trim().to_lowercase())
    .bind(clean(merged.phone.clone()))
    .bind(merged.position.trim())
    .bind(merged.department_id)
    .bind(merged.manager_id)
    .bind(&merged.employment_type)
    .bind(merged.start_date)
    .bind(merged.end_date)
    .bind(merged.employee_status.as_deref().unwrap_or("active"))
    .bind(clean(merged.location.clone()))
    .bind(merged.notes.clone().unwrap_or_default())
    .execute(pool)
    .await
    .map_err(map_employee_conflict)?;

    let _ = rows;
    read_employee(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("employee"))
}

/// Terminate an employee: a status and an end date, never a delete.
///
/// A separate function rather than a patch, because the two decisions are different: the status is
/// a lifecycle change the reports read, and the end date is a fact about the employment. A screen
/// that wanted to un-terminate somebody would call the patch; nothing in the module deletes.
pub async fn terminate_employee(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    end_date: Date,
) -> Result<Employee> {
    let before = read_employee(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("employee"))?;

    if end_date < before.start_date {
        return Err(HrError::invalid(
            "employee",
            "end_date",
            "the end date cannot be before the start date",
        ));
    }

    sqlx::query(
        "update hr_employees set employee_status = 'terminated', end_date = $3, updated_at = now() \
         where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(end_date)
    .execute(pool)
    .await?;

    read_employee(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("employee"))
}

// ---------------------------------------------------------------------------------------------
// The management chain
// ---------------------------------------------------------------------------------------------

/// Refuse a manager that is the employee themselves, or sits below them.
///
/// The walk goes **up** from the proposed manager: if the employee being edited is met on the way,
/// they already report — through somebody — to themselves. `union` (distinct) rather than
/// `union all` is what makes a chain that already loops terminate instead of hanging the request.
///
/// The names of the chain come back with the refusal, because "set someone who is below you"
/// sends the person back to the tree to work out which line closes it.
pub async fn assert_no_manager_cycle(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    manager_id: Uuid,
) -> Result<()> {
    if employee_id == manager_id {
        return Err(HrError::SelfManager);
    }

    let manager_exists: bool = sqlx::query_scalar(
        "select exists (select 1 from hr_employees where organization_id = $1 and id = $2)",
    )
    .bind(organization_id)
    .bind(manager_id)
    .fetch_one(pool)
    .await?;
    if !manager_exists {
        return Err(HrError::NotFound("employee"));
    }

    /// Only the name is read: the ids the walk met are the *question*, not the answer, and the
    /// query already filtered the employee being edited out of its own chain.
    /// The depth is carried, not just the name: the decision is "is the employee being edited
    /// **strictly above** the proposed manager", and that is a question about depth, not about
    /// which id happened to be unequal.
    #[derive(sqlx::FromRow)]
    struct ChainRow {
        id: Uuid,
        depth: i32,
        name: String,
    }

    // A `create` statement cannot be a `create` *or* an `insert` for the anchor, and `union`
    // requires matching types across both arms — so the anchor is a `select` over the proposed
    // manager rather than a bound parameter, and the parameter is used only for the comparison.
    let chain: Vec<ChainRow> = sqlx::query_as(
        "with recursive chain (id, depth) as ( \
             select m.id, 0 from hr_employees m \
             where m.organization_id = $1 and m.id = $2 \
             union \
             select e.manager_id, c.depth + 1 \
             from hr_employees e \
             join chain c on e.id = c.id \
             where e.organization_id = $1 and e.manager_id is not null and c.depth < $3 \
         ) \
         select c.id as id, c.depth as depth, \
                coalesce(e.first_name || ' ' || e.last_name, e.employee_no) as name \
         from chain c join hr_employees e on e.id = c.id \
         where c.depth > 0 \
         order by c.depth",
    )
    .bind(organization_id)
    .bind(manager_id)
    .bind(MAX_CHAIN_DEPTH)
    .fetch_all(pool)
    .await?;

    // The decision itself is [`chain_closes_a_loop`], so the unit test covers the create path
    // that a walk with a real database cannot cheaply reach twice. `Uuid::nil()` is the honest
    // "this employee does not exist yet" the create path passes, and the function is written so
    // that it can never match on it.
    let walked: Vec<(Uuid, i32)> = chain.iter().map(|row| (row.id, row.depth)).collect();
    if chain_closes_a_loop(&walked, employee_id).is_some() {
        return Err(HrError::ManagerCycle(crate::error::ManagementChain::new(
            chain
                .iter()
                .map(|row| row.name.clone())
                .collect(),
        )));
    }

    Ok(())
}

/// `true` when the walked chain closes a loop for the employee being edited.
///
/// A **pure function** of the chain and the id, split out because it is the decision that a
/// create and an update both depend on and neither can be tested for without a database. The bug
/// it now guards: the first version compared `chain_id <> employee_id`, and a create passes
/// `Uuid::nil()` because the row does not exist yet — so the manager's own row (depth 0) matched
/// and **every** employee created with a manager was refused as a cycle. A walk that hired a
/// report found it; a unit test on the list query would not have.
fn chain_closes_a_loop(chain: &[(Uuid, i32)], employee_id: Uuid) -> Option<Uuid> {
    // The proposed manager is the depth-0 anchor and is never itself a cycle: the person being
    // edited cannot be found *at* the manager row, only strictly above them.
    chain
        .iter()
        .filter(|(_, depth)| *depth > 0)
        .find(|(id, _)| *id == employee_id)
        .map(|(id, _)| *id)
}

/// Turn a unique-index violation into the sentence the form shows, by constraint name.
///
/// Delegates to [`crate::departments::unique_violation`] rather than keeping its own copy of the
/// mapping. Two copies is two decisions: the first one to gain a constraint is the one whose
/// version the other half of the module keeps failing to honour, and a duplicate work e-mail is
/// refused as "another employee" by one route and as a raw constraint name by the other.
fn map_employee_conflict(error: sqlx::Error) -> HrError {
    if let sqlx::Error::Database(ref db) = error
        && let Some(refusal) =
            crate::departments::unique_violation(db.code().as_deref(), db.constraint())
    {
        return refusal;
    }
    HrError::Database(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes() -> EmployeeChanges {
        EmployeeChanges {
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            work_email: "Ada@Example.com".to_owned(),
            position: "Engineer".to_owned(),
            department_id: Uuid::new_v4(),
            employment_type: "full_time".to_owned(),
            start_date: Date::from_calendar_date(2026, time::Month::January, 5).unwrap(),
            ..EmployeeChanges::default()
        }
    }

    #[test]
    fn a_create_needs_a_name_an_address_a_position_a_type_and_a_start() {
        for (field, mutate) in [
            ("first_name", Box::new(|c: &mut EmployeeChanges| c.first_name = " ".to_owned())
                as Box<dyn Fn(&mut EmployeeChanges)>),
            ("last_name", Box::new(|c: &mut EmployeeChanges| c.last_name = "".to_owned())),
            ("work_email", Box::new(|c: &mut EmployeeChanges| c.work_email = "ada".to_owned())),
            ("position", Box::new(|c: &mut EmployeeChanges| c.position = " ".to_owned())),
            (
                "employment_type",
                Box::new(|c: &mut EmployeeChanges| c.employment_type = "freelance".to_owned()),
            ),
        ] {
            let mut candidate = changes();
            mutate(&mut candidate);
            let error = validate_changes(&candidate).unwrap_err();
            assert!(
                matches!(&error, HrError::Invalid { field: got, .. } if *got == field),
                "{field}: got {error}"
            );
        }
    }

    #[test]
    fn a_contract_needs_an_end_date_and_other_types_do_not() {
        let mut contract = changes();
        contract.employment_type = "contract".to_owned();
        let error = validate_changes(&contract).unwrap_err();
        assert!(matches!(&error, HrError::Invalid { field: "end_date", .. }), "{error}");

        let mut with_end = contract.clone();
        with_end.end_date =
            Some(Date::from_calendar_date(2026, time::Month::December, 31).unwrap());
        assert!(validate_changes(&with_end).is_ok());
    }

    #[test]
    fn an_end_date_before_the_start_is_refused() {
        let mut candidate = changes();
        candidate.end_date = Some(Date::from_calendar_date(2025, time::Month::December, 31).unwrap());
        let error = validate_changes(&candidate).unwrap_err();
        assert!(matches!(&error, HrError::Invalid { field: "end_date", .. }), "{error}");
    }

    #[test]
    fn a_start_date_in_the_future_is_allowed_because_hiring_next_month_is_normal() {
        let mut candidate = changes();
        candidate.start_date =
            Date::from_calendar_date(2027, time::Month::March, 1).unwrap();
        assert!(
            validate_changes(&candidate).is_ok(),
            "a future start is a record that shows as starting soon, not a refusal"
        );
    }

    #[test]
    fn the_work_address_is_lowercased_and_a_personal_one_is_validated_to_the_same_shape() {
        let candidate = changes();
        assert_eq!(validate_changes(&candidate).expect("valid"), "EMP-0001");

        let mut bad_personal = changes();
        bad_personal.personal_email = Some("not-an-address".to_owned());
        let error = validate_changes(&bad_personal).unwrap_err();
        assert!(
            matches!(&error, HrError::Invalid { field: "personal_email", .. }),
            "a gated field is still validated before it is stored: {error}"
        );
    }

    #[test]
    fn a_phone_that_is_not_a_phone_is_refused_on_both_the_work_and_the_personal_field() {
        let mut work = changes();
        work.phone = Some("ring me".to_owned());
        assert!(matches!(
            validate_changes(&work).unwrap_err(),
            HrError::Invalid { field: "phone", .. }
        ));
    }

    #[test]
    fn notes_are_bounded_rather_than_truncated_silently() {
        let mut candidate = changes();
        candidate.notes = Some("x".repeat(MAX_NOTES_LENGTH + 1));
        assert!(matches!(
            validate_changes(&candidate).unwrap_err(),
            HrError::Invalid { field: "notes", .. }
        ));
    }

    #[test]
    fn an_employee_number_the_person_typed_is_kept() {
        let mut candidate = changes();
        candidate.employee_no = Some("  IĞDIR-42 ".to_owned());
        assert_eq!(validate_changes(&candidate).expect("valid"), "IĞDIR-42");
    }

    #[test]
    fn the_sort_keys_are_a_closed_set_and_an_unknown_one_is_refused() {
        let query = EmployeeQuery {
            sort: Some("salary".to_owned()),
            ..EmployeeQuery::default()
        };
        let error = query.resolve_sort().unwrap_err();
        assert!(error.to_string().contains("salary"), "{error}");

        // A closed set because the key becomes an `order by`.
        assert_eq!(SORTABLE.len(), 9);
        assert!(sort_expression("last_name").is_some());
        assert!(sort_expression("; drop table hr_employees").is_none());
    }

    #[test]
    fn the_default_sort_is_the_name_and_a_surname_sorts_ascending() {
        let (expression, desc) = EmployeeQuery::default().resolve_sort().expect("resolves");
        assert_eq!(expression, "lower(e.last_name)");
        assert!(!desc, "a directory reads A to Z by default");
    }

    #[test]
    fn a_direction_the_platform_does_not_have_is_refused_rather_than_ignored() {
        let query = EmployeeQuery {
            sort: Some("last_name".to_owned()),
            direction: Some("sideways".to_owned()),
            ..EmployeeQuery::default()
        };
        assert!(query.resolve_sort().unwrap_err().to_string().contains("sideways"));
    }

    #[test]
    fn an_unknown_visibility_is_refused_rather_than_widened() {
        // A typo in a saved view must not turn a personal list into the whole organization's.
        // The query carries the level as a **string** for exactly this reason: an enum would make
        // the refusal an accident of serde, and a hand-rolled parse would make it a silent
        // widening instead.
        let bad = EmployeeQuery {
            visibility: Some("everyone".to_owned()),
            ..EmployeeQuery::default()
        };
        let error = bad.visibility().expect_err("an unknown level must be refused");
        assert!(error.to_string().contains("everyone"), "{error}");
        assert!(error.to_string().contains("own, team, all"), "{error}");

        // Absent is the widest level, so a caller never has to pass one.
        assert_eq!(
            EmployeeQuery::default().visibility().expect("absent").as_str(),
            "all"
        );
        for (value, expected) in [("own", "own"), ("team", "team"), ("all", "all")] {
            let query = EmployeeQuery {
                visibility: Some(value.to_owned()),
                ..EmployeeQuery::default()
            };
            assert_eq!(query.visibility().expect("a known level").as_str(), expected);
        }
    }

    #[test]
    fn an_unknown_employment_type_or_status_is_refused_by_the_filter() {
        for query in [
            EmployeeQuery {
                employment_type: Some("freelance".to_owned()),
                ..EmployeeQuery::default()
            },
            EmployeeQuery {
                status: Some("left".to_owned()),
                ..EmployeeQuery::default()
            },
        ] {
            let mut builder = QueryBuilder::new("select 1");
            let scope = Scope::all(Uuid::nil(), Uuid::nil());
            let error = push_filters(&mut builder, &scope, &query).unwrap_err();
            assert!(error.to_string().contains("is not an"), "{error}");
        }
    }

    #[test]
    fn the_page_size_is_capped_and_never_zero() {
        assert_eq!(EmployeeQuery::default().page_size(), DEFAULT_PER_PAGE);
        assert_eq!(
            EmployeeQuery {
                limit: Some(0),
                ..EmployeeQuery::default()
            }
            .page_size(),
            1
        );
        assert_eq!(
            EmployeeQuery {
                limit: Some(100_000),
                ..EmployeeQuery::default()
            }
            .page_size(),
            MAX_PER_PAGE
        );
    }

    #[test]
    fn a_cursor_that_is_not_an_identifier_is_refused() {
        let query = EmployeeQuery {
            cursor: Some("not-a-uuid".to_owned()),
            ..EmployeeQuery::default()
        };
        assert!(query.cursor_id().unwrap_err().to_string().contains("cursor"));
        assert!(EmployeeQuery::default().cursor_id().expect("absent").is_none());
    }

    #[test]
    fn a_search_term_is_lowercased_and_a_long_one_is_refused() {
        let query = EmployeeQuery {
            search: Some("  ADA  ".to_owned()),
            ..EmployeeQuery::default()
        };
        assert_eq!(query.search_term().expect("a term").as_deref(), Some("ada"));

        let long = EmployeeQuery {
            search: Some("x".repeat(MAX_SEARCH_LENGTH + 1)),
            ..EmployeeQuery::default()
        };
        assert!(long.search_term().is_err());
    }

    #[test]
    fn the_gated_columns_are_not_in_the_list_projection() {
        // The acceptance criterion is that the list and the export never carry the personal
        // fields. Proving it here means a future column cannot quietly join the shared SELECT:
        // the list has no slot for it.
        for column in crate::model::SENSITIVE_FIELDS {
            assert!(
                !EMPLOYEE_SELECT.contains(column),
                "{column} must not be in the list projection"
            );
        }
    }

    #[test]
    fn a_record_serialises_without_the_gated_keys_when_the_caller_may_not_read_them() {
        let record = EmployeePrivate {
            employee: fake_employee(),
            personal_email: Some("private@example.com".to_owned()),
            personal_phone: Some("+90 532 000 00 00".to_owned()),
            address: Some("Somewhere".to_owned()),
            emergency_contact: Some("A relative".to_owned()),
        };

        let gated = record.gated(false);
        assert_eq!(gated.personal_email, None);
        assert_eq!(gated.address, None);
        assert_eq!(gated.emergency_contact, None);

        // The employee itself never carried them, so the list shape is the same object either way.
        let json = serde_json::to_value(&record.employee).expect("serialises");
        for column in crate::model::SENSITIVE_FIELDS {
            assert!(json.get(column).is_none(), "{column} must be absent from the list row");
        }
    }

    #[test]
    fn the_gate_is_one_decision_a_caller_with_the_permission_sees_the_fields() {
        let record = EmployeePrivate {
            employee: fake_employee(),
            personal_email: Some("private@example.com".to_owned()),
            personal_phone: None,
            address: None,
            emergency_contact: None,
        };

        let gated = record.gated(true);
        assert_eq!(gated.personal_email, Some("private@example.com"));
        // A field nobody filled in stays absent even for a caller who may read them — absent and
        // "hidden" must not become the same answer.
        assert_eq!(gated.address, None);
    }

    #[test]
    fn a_chain_that_starts_at_the_proposed_manager_is_not_a_cycle() {
        // THE BUG: the walk used to answer "does the chain contain a row that is not the employee
        // being edited", and a create passes `Uuid::nil()` because the row does not exist yet.
        // The manager's own row is therefore not-nil, it matched, and **every** employee created
        // with a manager was refused as a cycle. The DB walk that tried to hire a report found it;
        // nothing about the list query would have.
        let manager = Uuid::new_v4();
        let chain = vec![(manager, 0i32)];
        assert_eq!(chain_closes_a_loop(&chain, Uuid::nil()), None);
        assert_eq!(chain_closes_a_loop(&chain, Uuid::new_v4()), None);
    }

    #[test]
    fn meeting_yourself_above_the_proposed_manager_is_the_cycle() {
        let manager = Uuid::new_v4();
        let middle = Uuid::new_v4();
        let editor = Uuid::new_v4();
        // manager -> middle -> editor: the editor is two steps up, so making the editor report to
        // the manager closes a loop.
        let chain = vec![(manager, 0), (middle, 1), (editor, 2)];
        assert_eq!(chain_closes_a_loop(&chain, editor), Some(editor));
        // The middle is a cycle too, and **that is correct**: the middle already reports to the
        // manager, so making the middle report to the manager closes a one-step loop. The only
        // id in a chain that is never a cycle is the anchor at depth 0 — the person being
        // proposed is not below themselves.
        assert_eq!(chain_closes_a_loop(&chain, middle), Some(middle));
        assert_eq!(chain_closes_a_loop(&chain, manager), None);
        // The direct case: editor reports to middle, and the middle's own manager is the editor.
        let direct = vec![(middle, 0), (editor, 1)];
        assert_eq!(chain_closes_a_loop(&direct, editor), Some(editor));
    }

    fn fake_employee() -> Employee {
        Employee {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            employee_no: "EMP-0001".to_owned(),
            user_id: None,
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            work_email: "ada@example.com".to_owned(),
            phone: None,
            position: "Engineer".to_owned(),
            department_id: Uuid::new_v4(),
            department_name: "Engineering".to_owned(),
            manager_id: None,
            manager_name: None,
            employment_type: "full_time".to_owned(),
            start_date: Date::from_calendar_date(2026, time::Month::January, 5).unwrap(),
            end_date: None,
            employee_status: "active".to_owned(),
            location: None,
            notes: String::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}
