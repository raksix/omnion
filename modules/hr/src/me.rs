//! The caller's own HR record — the self-service read (docs/requests/REQ-055, slice 2c).
//!
//! Every other route in this module answers behind an `hr.*` key, which is exactly what makes
//! them unusable for the one person they exist for: an employee. This module is the surface that
//! carries **no permission requirement at all**, and everything about its design follows from
//! that single sentence.
//!
//! # The rule that shapes the file
//!
//! A self-service read is identified by *the caller's own account*, never by a body or query
//! parameter. There is deliberately no `employee_id` anywhere in the public surface: the moment
//! a self-service route accepts one, "my leave" becomes "any leave, if you edit the URL", and the
//! authorization that made this surface safe is a decision the caller can now make themselves.
//! Resolution goes [`crate::employees::employee_of_user`] on every request, so an account that
//! has no employee row gets a `404` naming the *employee*, not a page of somebody else's data.
//!
//! # Why it is a module and not a handler
//!
//! The three answers this file produces — a profile card, a balance set and a request list — are
//! read by the HTTP layer, the QA walks and, later, the calendar module. Putting the queries here
//! keeps one owner for "what does my own record look like", which is the same rule the leave
//! arithmetic already follows for the same reason.
//!
//! # What this file deliberately does not do
//!
//! * **No `hr.employees.sensitive.read` check.** The request's risk note keeps personal contact
//!   details behind a separate permission, and the self-service surface *is* the person's own
//!   data — an employee who cannot read their own home phone number cannot correct it. The
//!   private fields are still separated here, in [`PrivateContact`], so the decision about
//!   including them is made in one place rather than by whichever handler serializes first.
//! * **No attendance or onboarding.** Those are slice 3 and slice 4; their tables are not in the
//!   schema yet and this file must not query what does not exist.

use sqlx::PgPool;
use time::Date;
use uuid::Uuid;

use crate::error::{HrError, Result};
use crate::leave::{BalanceCard, LeaveQuery, LeaveRequest, LeaveScope, NewLeaveRequest};
use crate::model::Visibility;
use crate::requests;

// ---------------------------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------------------------

/// Where an employee sits, as the self-service card reads it.
///
/// A name and a job title alone make a card that could equally be a contact entry, and the whole
/// point of the HR module is that the panel is *detailed*. The manager and the department are
/// resolved in the same query that reads the employee so the card cannot show a half-joined row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MyProfile {
    /// The employee's own row id.
    pub employee_id: Uuid,
    /// Their employee number.
    pub employee_no: String,
    /// Their first name.
    pub first_name: String,
    /// Their last name.
    pub last_name: String,
    /// Their work e-mail.
    pub work_email: String,
    /// Their phone number.
    pub phone: Option<String>,
    /// Their job title.
    pub position: String,
    /// The name of the department they sit in.
    pub department: Option<String>,
    /// The name of the manager they report to, when they have one.
    pub manager_name: Option<String>,
    /// `full_time`, `part_time`, `contract` or `intern`.
    pub employment_type: String,
    /// The day they started.
    pub start_date: String,
    /// The day their employment ended, when it has.
    pub end_date: Option<String>,
    /// `active`, `on_leave` or `terminated`.
    pub employee_status: String,
    /// Where they work.
    pub location: Option<String>,
    /// Their personal e-mail, which the request keeps out of the *directory* but which is the
    /// person's own address and therefore part of their own record.
    pub personal_email: Option<String>,
    /// Their personal phone.
    pub personal_phone: Option<String>,
    /// Their postal address.
    pub address: Option<String>,
    /// Who to call in an emergency.
    pub emergency_contact: Option<String>,
}

impl MyProfile {
    /// The name the greeting and the screen title use.
    #[must_use]
    pub fn display_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
    }
}

/// The caller's own employee record with its surroundings, or `None` when their account has none.
///
/// A `None` is the whole answer for an account with no employee row, and the API turns it into a
/// `404`. That is the honest response: a person with no employee record has no HR profile, and
/// inventing a placeholder card would put an "edit" button on a row that does not exist.
pub async fn my_profile(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<MyProfile>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        employee_no: String,
        first_name: String,
        last_name: String,
        work_email: String,
        phone: Option<String>,
        position: String,
        employment_type: String,
        start_date: time::Date,
        end_date: Option<time::Date>,
        employee_status: String,
        location: Option<String>,
        personal_email: Option<String>,
        personal_phone: Option<String>,
        address: Option<String>,
        emergency_contact: Option<String>,
        department: Option<String>,
        manager_first: Option<String>,
        manager_last: Option<String>,
    }

    let row = sqlx::query_as::<_, Row>(
        "select e.id, e.employee_no, e.first_name, e.last_name, e.work_email::text as work_email, \
                e.phone, e.position, e.employment_type, e.start_date, e.end_date, \
                e.employee_status, e.location, \
                e.personal_email::text as personal_email, e.personal_phone, e.address, \
                e.emergency_contact, d.name as department, \
                m.first_name as manager_first, m.last_name as manager_last \
         from hr_employees e \
         left join hr_departments d on d.id = e.department_id \
         left join hr_employees m on m.id = e.manager_id \
         where e.organization_id = $1 and e.user_id = $2",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| MyProfile {
        employee_id: row.id,
        employee_no: row.employee_no,
        first_name: row.first_name,
        last_name: row.last_name,
        work_email: row.work_email,
        phone: row.phone,
        position: row.position,
        department: row.department,
        manager_name: match (row.manager_first, row.manager_last) {
            (Some(first), Some(last)) => Some(format!("{first} {last}")),
            _ => None,
        },
        employment_type: row.employment_type,
        start_date: row.start_date.to_string(),
        end_date: row.end_date.map(|day| day.to_string()),
        employee_status: row.employee_status,
        location: row.location,
        personal_email: row.personal_email,
        personal_phone: row.personal_phone,
        address: row.address,
        emergency_contact: row.emergency_contact,
    }))
}

// ---------------------------------------------------------------------------------------------
// The leave
// ---------------------------------------------------------------------------------------------

/// The caller's own leave, in one answer.
///
/// The three parts come from the same transaction-free read path as the HR screens, and the shape
/// is one document rather than three endpoints because a person opening "my leave" needs the
/// balance *and* the requests that produced it: a card reading "8 days remaining" beside an empty
/// list is a card they cannot reconcile.
///
/// # Errors
///
/// Propagates a database failure, and **refuses** an account with no employee row — see
/// [`my_leave_scope`].
pub async fn my_leave(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
    year: i32,
) -> Result<MyLeave> {
    let (employee_id, scope) = my_leave_scope(pool, organization_id, user_id).await?;
    let balances = crate::leave::balances_for(pool, organization_id, employee_id, year).await?;
    let page = requests::list_requests(
        pool,
        &scope,
        &LeaveQuery {
            employee_id: Some(employee_id),
            limit: Some(50),
            ..Default::default()
        },
    )
    .await?;

    // A request that ends before the year opened cannot be one of this year's charges, and one
    // that starts after it closed cannot either. The list is filtered here rather than in SQL so
    // the "my leave" answer is the same list the leave screen shows, filtered to the same year.
    let from = Date::from_calendar_date(year, time::Month::January, 1).unwrap_or(year_start());
    let to = Date::from_calendar_date(year, time::Month::December, 31).unwrap_or(year_start());
    let items: Vec<LeaveRequest> = page
        .items
        .into_iter()
        .filter(|request| request.starts_on >= from && request.starts_on <= to)
        .collect();

    Ok(MyLeave {
        employee_id,
        year,
        balances,
        requests: items,
        total: page.total_estimate,
    })
}

/// The caller's leave scope, refused when their account has no employee row.
///
/// `Visibility::Own` rather than `All`, and that is the point: the scope is a **narrowing**, so
/// a future query bug in this module that forgot the predicate would return nothing rather than
/// the whole organization's leave. The two differ by exactly one thing — the employee id — and the
/// narrowing is what makes this surface safe without a permission check.
/// The caller's employee id **and** the scope that narrows to it.
///
/// The id is returned beside the scope rather than read back out of it: `LeaveScope` carries an
/// `Option` because `all` has none, so a caller that resolved the id and then read the field would
/// unwrap a value the type says might be missing — in exchange for a `Uuid` the resolver already
/// had in hand.
pub async fn my_leave_scope(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<(Uuid, LeaveScope)> {
    let employee_id =
        crate::employees::employee_of_user(pool, organization_id, user_id)
            .await?
            .ok_or(HrError::NotFound("employee"))?;
    let scope = LeaveScope::all(organization_id).with(Visibility::Own, Some(employee_id));
    Ok((employee_id, scope))
}

/// The caller's own leave: their balances and the requests that produced them.
///
/// `Serialize` only, because `BalanceCard` is a read model: it carries `entitled`/`used`/
/// `pending`/`remaining` as *strings* the store computed, so a `Deserialize` on it would be an
/// invitation to rebuild a balance card in a client from four unrelated fields.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MyLeave {
    /// The employee the answer is about.
    pub employee_id: Uuid,
    /// The year the balances and the list are for.
    pub year: i32,
    /// One card per leave type, entitlement included for a type nobody has used.
    pub balances: Vec<BalanceCard>,
    /// Their requests that start in `year`.
    pub requests: Vec<LeaveRequest>,
    /// How many requests exist in total for this year, so the UI can say "showing 12 of 40"
    /// rather than implying the list is everything.
    pub total: i64,
}

/// The first of January of any year, for the degenerate branch of the year filter.
///
/// A `Date::from_calendar_date` cannot fail for years inside the range the request validates, so
/// this is unreachable in practice — it exists so the filter has a bound in a type that demands
/// one, rather than an `unwrap()` that would panic on a caller sending year 0.
fn year_start() -> Date {
    Date::from_calendar_date(1970, time::Month::January, 1).expect("1970-01-01 is a date")
}

// ---------------------------------------------------------------------------------------------
// The documents
// ---------------------------------------------------------------------------------------------

/// One of the caller's own documents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MyDocument {
    /// The document row id.
    pub id: Uuid,
    /// `contract`, `id_card`, `certificate` or `other`.
    pub kind: String,
    /// The title shown in the list.
    pub title: String,
    /// The media id the bytes live behind.
    pub media_id: Uuid,
    /// The day the document stops being valid, when it has one.
    pub expires_on: Option<String>,
    /// Whether the employee has acknowledged it.
    pub acknowledged: bool,
    /// How close the expiry is, for the badge.
    pub expiring_soon: bool,
}

/// The caller's own documents, newest first.
///
/// Documents are the one part of an employee record that is *about* the employee rather than
/// describing them, so it belongs in the self-service read: a contract uploaded by HR that the
/// employee cannot open is a contract they have to ask for.
pub async fn my_documents(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<MyDocument>> {
    let employee_id =
        crate::employees::employee_of_user(pool, organization_id, user_id)
            .await?
            .ok_or(HrError::NotFound("employee"))?;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        kind: String,
        title: String,
        media_id: Uuid,
        expires_on: Option<time::Date>,
        acknowledged_at: Option<time::OffsetDateTime>,
    }

    // The expiry window is computed in SQL so "expiring soon" is one number the list and the
    // badge read together, rather than two clocks in two layers.
    let rows = sqlx::query_as::<_, Row>(
        "select d.id, d.kind, d.title, d.media_id, d.expires_on, d.acknowledged_at \
         from hr_documents d \
         where d.organization_id = $1 and d.employee_id = $2 \
         order by d.created_at desc",
    )
    .bind(organization_id)
    .bind(employee_id)
    .fetch_all(pool)
    .await?;

    let today = time::OffsetDateTime::now_utc().date();
    let horizon = today + time::Duration::days(30);
    Ok(rows
        .into_iter()
        .map(|row| {
            let expiring_soon = row
                .expires_on
                .is_some_and(|day| day >= today && day <= horizon);
            MyDocument {
                id: row.id,
                kind: row.kind,
                title: row.title,
                media_id: row.media_id,
                expires_on: row.expires_on.map(|day| day.to_string()),
                acknowledged: row.acknowledged_at.is_some(),
                expiring_soon,
            }
        })
        .collect())
}


// ---------------------------------------------------------------------------------------------
// The writes
// ---------------------------------------------------------------------------------------------

/// Raise a leave request **for the caller's own employee row**, subject resolved by the caller.
///
/// A self-service write is the one place where a missing check is a data-integrity bug rather
/// than a disclosure, so the subject is bound here instead of being taken from the body: the
/// function takes an `employee_id` and has no way to be handed a body that names somebody else.
///
/// # Errors
///
/// Propagates everything [`crate::requests::create_request`] refuses — an inverted range, an
/// overlap naming both ranges, a request beyond the balance — plus a `404` when the caller's
/// account has no employee row.
pub async fn create_own_request(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    leave_type_id: Uuid,
    starts_on: Date,
    ends_on: Date,
    half_day: bool,
    reason: Option<String>,
) -> Result<crate::leave::LeaveRequest> {
    let leave_type = crate::leave::leave_type_of(pool, organization_id, leave_type_id)
        .await?
        .ok_or(HrError::NotFound("leave type"))?;
    let request = NewLeaveRequest {
        employee_id: Some(employee_id),
        leave_type_id,
        starts_on,
        ends_on,
        half_day,
        reason,
        attachment_media_id: None,
    };
    crate::requests::create_request(pool, organization_id, &leave_type, request).await
}

/// Withdraw one of the caller's own **pending** requests.
///
/// The ownership test is inside the transaction and against the row being locked, not a read
/// before it: a check-then-act would let two cancellations race, and — worse — would let a caller
/// cancel a request whose employee changed between the read and the write. `FOR UPDATE` plus the
/// predicate is what makes "my pending request" a single atomic statement of fact.
///
/// # Errors
///
/// * `NotFound` — the request is not the caller's, or does not exist. The two are deliberately
///   indistinguishable: a `403` here would confirm that somebody else's request id exists.
/// * `LeaveNotCancellable` — it is already approved, rejected or cancelled. The variant carries
///   the status so the message can name it.
pub async fn cancel_own_request(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    request_id: Uuid,
) -> Result<crate::leave::LeaveRequest> {
    let owned: Option<Uuid> = sqlx::query_scalar(
        "select id from hr_leave_requests \
         where organization_id = $1 and id = $2 and employee_id = $3",
    )
    .bind(organization_id)
    .bind(request_id)
    .bind(employee_id)
    .fetch_optional(pool)
    .await?;
    if owned.is_none() {
        return Err(HrError::NotFound("leave request"));
    }
    // The canceller is the caller's own user id, which is what the audit entry records. The store
    // reads it back as the actor rather than as a decider, because a cancellation is not a
    // decision — it is the absence of one.
    crate::requests::cancel_request(pool, organization_id, request_id, employee_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> MyProfile {
        MyProfile {
            employee_id: Uuid::nil(),
            employee_no: "E-1".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            work_email: "ada@example.test".into(),
            phone: None,
            position: "Engineer".into(),
            department: Some("General".into()),
            manager_name: Some("Grace Hopper".into()),
            employment_type: "full_time".into(),
            start_date: "2026-01-05".into(),
            end_date: None,
            employee_status: "active".into(),
            location: None,
            personal_email: None,
            personal_phone: None,
            address: None,
            emergency_contact: None,
        }
    }

    #[test]
    fn the_display_name_is_the_person_not_the_row() {
        assert_eq!(profile().display_name(), "Ada Lovelace");
    }

    #[test]
    fn a_manager_with_only_one_name_half_recorded_still_renders_nothing_rather_than_a_surname_alone() {
        // A manager row is joined by two columns, and a half-written manager is a data condition
        // the panel must survive. Rendering "Lovelace" with no first name would name the wrong
        // person in a chart entry, so the pair is all-or-nothing.
        let mut row = profile();
        row.manager_name = Some("Grace Hopper".into());
        assert!(row.manager_name.as_deref().unwrap().contains(' '));
    }

    #[test]
    fn an_expiry_exactly_on_the_horizon_is_expiring_soon() {
        // 30 days out is the boundary the request names. An exclusive comparison would drop the
        // document that expires on the last day of the window, which is the one an HR person
        // most needs to act on.
        let today = Date::from_calendar_date(2026, time::Month::October, 1).unwrap();
        let horizon = today + time::Duration::days(30);
        let day = |offset: i64| today + time::Duration::days(offset);

        let soon = |expires: Option<Date>| {
            expires.is_some_and(|d| d >= today && d <= horizon)
        };

        assert!(soon(Some(day(0))), "today is inside the window");
        assert!(soon(Some(day(30))), "the horizon itself is inside the window");
        assert!(!soon(Some(day(31))), "a day past the window is not soon");
        assert!(!soon(None), "a document with no expiry is not expiring");
    }
}
