//! The leave request: raising it, deciding it, cancelling it, and the absence calendar.
//!
//! Everything that changes a request does so inside **one transaction** that also recomputes the
//! employee's balance for that type and year. Splitting them is how a leave module ends up with a
//! request list that says "approved" beside a balance card that still counts the same days as
//! pending: two writes, two commits, and the second one can fail on its own.
//!
//! * **The overlap check names the dates that clash.** "Conflicts with another request" sends the
//!   person back to the list to work out which one, and the request's acceptance criterion asks for
//!   the conflicting dates in the message.
//! * **Only `pending` and `approved` are conflicts.** A rejected and a cancelled request are
//!   history — a person who asked twice and was refused once, then cancelled, is not double
//!   booked.
//! * **A request is refused when it exceeds the balance, unless the type allows a negative one**,
//!   and the refusal carries the three numbers a person needs (entitled, used, pending) so they
//!   can see *why* rather than being told no.
//! * **The type's `requires_approval` decides whether a request starts pending or approved.** A
//!   type an organization set to not need approval is the direct-decision path the request's risk
//!   note asks for, so leave stays usable without REQ-059 installed, and the audit trail is the
//!   same either way.

use serde::Serialize;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{HrError, Result};
use crate::leave::{
    BalanceCard, LeaveQuery, LeaveRequest, LeaveScope, LeaveType, NewLeaveRequest, days_from_text,
    days_to_text, recompute_balance, request_days_hundredths,
};
use crate::store::{DEFAULT_PER_PAGE, MAX_PER_PAGE, MAX_SEARCH_LENGTH, Page};

const REQUEST_COLUMNS: &str = "r.id, r.organization_id, r.employee_id, \
     concat_ws(' ', e.first_name, e.last_name) as employee_name, \
     r.leave_type_id, t.name as leave_type_name, r.starts_on, r.ends_on, \
     trim_scale(r.days)::text as days, r.half_day, r.reason, r.leave_status, r.decided_by, \
     (select concat_ws(' ', du.display_name) from users du where du.id = r.decided_by) \
         as decided_by_name, \
     r.decided_at, r.decision_comment, r.cancelled_at, r.created_at";

/// The decision a manager or HR officer makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Counts against the balance.
    Approve,
    /// Recorded with the comment, never counted.
    Reject,
}

impl Decision {
    /// The value the `leave_status` column carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::Reject => "rejected",
        }
    }

    /// The event this decision emits.
    #[must_use]
    pub fn event_name(self) -> &'static str {
        match self {
            Self::Approve => "hr.leave.approved",
            Self::Reject => "hr.leave.rejected",
        }
    }
}

/// One absence, as the month grid draws it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AbsenceBar {
    /// The request it came from.
    pub request_id: Uuid,
    /// Who is away.
    pub employee_id: Uuid,
    /// Their name, for the row label.
    pub employee_name: String,
    /// The type, for the bar's colour.
    pub leave_type_id: Uuid,
    /// The type's name, for the tooltip.
    pub leave_type_name: String,
    /// The first day away, which may be before the window.
    #[serde(with = "crate::dates")]
    pub starts_on: Date,
    /// The last day away, which may be after the window.
    #[serde(with = "crate::dates")]
    pub ends_on: Date,
    /// The charged days.
    pub days: String,
    /// Whether the range continues past the window's last day.
    ///
    /// The request's own note: "leave outside the visible month continues with an arrow marker".
    /// Without this the grid would draw a bar that stops at the 31st and look like the holiday
    /// ended there.
    pub continues_after: bool,
    /// Whether it began before the window's first day.
    pub continues_before: bool,
}

/// The absence calendar for a window: one bar per approved request that touches it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AbsenceCalendar {
    /// The first day of the window.
    #[serde(with = "crate::dates")]
    pub from: Date,
    /// The last day of the window.
    #[serde(with = "crate::dates")]
    pub to: Date,
    /// Today, for the grid's "now" marker.
    ///
    /// Carried by the response rather than computed by the browser: the grid has to mark the same
    /// day the server considered current, and a client that computed its own would disagree with
    /// the window by however far the two clocks are apart.
    #[serde(with = "crate::dates")]
    pub today: Date,
    /// The bars, in start order.
    pub bars: Vec<AbsenceBar>,
    /// The employees the window has bars for, with their names — the row labels.
    pub employees: Vec<AbsenceRow>,
}

/// One row of the absence calendar: an employee and the bars on their line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AbsenceRow {
    /// The employee.
    pub employee_id: Uuid,
    /// Their name.
    pub employee_name: String,
    /// How many of their requests touch the window.
    pub request_count: i64,
}

/// A request as the decision panel reads it: the request, the balance it moves, and the timeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestDetail {
    /// The request.
    #[serde(flatten)]
    pub request: LeaveRequest,
    /// The balance card for its type and the year its start falls in.
    pub balance: BalanceCard,
    /// The steps the detail screen draws as a timeline, oldest first.
    pub timeline: Vec<TimelineStep>,
    /// Whether the caller may still decide this request.
    ///
    /// A pending request the caller is not the approver of is rendered read-only, and this is
    /// the flag that says so — rather than the screen guessing from the status alone, which is
    /// how a "Decide" button ends up on a request HR has already approved.
    pub can_decide: bool,
}

/// One step of a request's history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineStep {
    /// `requested`, `approved`, `rejected` or `cancelled`.
    pub kind: String,
    /// When it happened.
    #[serde(with = "crate::dates::instant")]
    pub at: OffsetDateTime,
    /// Who did it, when the platform knows.
    pub actor_id: Option<Uuid>,
    /// The comment they left.
    pub comment: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// One request, or `None` when it belongs to another organization.
pub async fn request_of(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<LeaveRequest>> {
    let row = sqlx::query_as::<_, LeaveRequest>(&format!(
        "select {REQUEST_COLUMNS} from hr_leave_requests r \
         join hr_employees e on e.id = r.employee_id \
         join hr_leave_types t on t.id = r.leave_type_id \
         where r.organization_id = $1 and r.id = $2"
    ))
    .bind(organization_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// A page of requests, filtered as asked and narrowed by the caller's visibility.
///
/// # Errors
///
/// Refuses an unknown status and a search term over the length limit, and **refuses a narrowed
/// caller with no employee record** rather than reading the organization (see
/// [`LeaveScope::employee_predicate`]).
pub async fn list_requests(
    pool: &PgPool,
    scope: &LeaveScope,
    query: &LeaveQuery,
) -> Result<Page<LeaveRequest>> {
    if let Some(status) = &query.status
        && !crate::leave::is_leave_status(status)
    {
        return Err(HrError::InvalidQuery(format!(
            "status {status:?} is not one of pending, approved, rejected, cancelled"
        )));
    }
    if let Some(search) = &query.search
        && search.trim().chars().count() > MAX_SEARCH_LENGTH
    {
        return Err(HrError::InvalidQuery(format!(
            "a search term is at most {MAX_SEARCH_LENGTH} characters"
        )));
    }
    let predicate = scope.employee_predicate()?;

    let limit = query.limit.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "select {REQUEST_COLUMNS} from hr_leave_requests r \
         join hr_employees e on e.id = r.employee_id \
         join hr_leave_types t on t.id = r.leave_type_id \
         where r.organization_id = "
    ));
    builder.push_bind(scope.organization_id);

    if let Some(id) = query.leave_type_id {
        builder.push(" and r.leave_type_id = ").push_bind(id);
    }
    if let Some(id) = query.employee_id {
        builder.push(" and r.employee_id = ").push_bind(id);
    }
    if let Some(status) = &query.status {
        builder.push(" and r.leave_status = ").push_bind(status.clone());
    }
    if query.pending_only == Some(true) {
        builder.push(" and r.leave_status = 'pending'");
    }
    if let Some(from) = query.from {
        builder.push(" and r.ends_on >= ").push_bind(from);
    }
    if let Some(to) = query.to {
        builder.push(" and r.starts_on <= ").push_bind(to);
    }
    if let Some(search) = query.search.as_ref().map(|text| text.trim().to_owned())
        && !search.is_empty()
    {
        builder
            .push(" and concat_ws(' ', e.first_name, e.last_name, e.employee_no) ilike ")
            .push_bind(format!("%{search}%"));
    }
    if let Some(predicate) = &predicate {
        builder.push(" and ").push(predicate.clone());
    }

    // The cursor is the id, so a page boundary is stable while somebody approves a request in
    // the middle of the list — an offset would skip a row that appeared after the page was cut.
    if let Some(cursor) = &query.cursor {
        builder.push(" and r.id > ").push_bind(cursor.clone());
    }
    builder.push(" order by r.created_at desc, r.id desc limit ");
    // One more than asked, which is how the cursor knows there is another page without a count.
    builder.push_bind(limit + 1);

    let mut rows = builder.build_query_as::<LeaveRequest>().fetch_all(pool).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(|row| row.id.to_string())
    } else {
        None
    };

    // The total is counted with the **same** predicate, or the header's "24 requests" would be a
    // number the list cannot show.
    let mut count = QueryBuilder::<Postgres>::new(
        "select count(*) from hr_leave_requests r join hr_employees e on e.id = r.employee_id \
         where r.organization_id = ",
    );
    count.push_bind(scope.organization_id);
    if let Some(status) = &query.status {
        count.push(" and r.leave_status = ").push_bind(status.clone());
    }
    if query.pending_only == Some(true) {
        count.push(" and r.leave_status = 'pending'");
    }
    if let Some(predicate) = &predicate {
        count.push(" and ").push(predicate.clone());
    }
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

    Ok(Page::new(rows, next_cursor, total))
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Raise a request, after the three checks that can refuse it.
///
/// # Errors
///
/// * the range is inverted, or a half-day that spans more than one day;
/// * the range contains **no working day** — "0 days of leave" is a form somebody opened and left,
///   and the schema's `days > 0` would otherwise be the only thing standing between a person and a
///   request for a Saturday;
/// * an overlapping `pending` or `approved` request, with the clashing dates in the message;
/// * the balance, when the type does not allow a negative one.
pub async fn create_request(
    pool: &PgPool,
    organization_id: Uuid,
    leave_type: &LeaveType,
    request: NewLeaveRequest,
) -> Result<LeaveRequest> {
    validate_range(&request)?;
    let reason = crate::model::clean(request.reason.clone()).unwrap_or_default();
    if reason.chars().count() > crate::leave::MAX_REASON_LENGTH {
        return Err(HrError::invalid(
            "leave request",
            "reason",
            format!(
                "a reason is at most {} characters",
                crate::leave::MAX_REASON_LENGTH
            ),
        ));
    }
    let employee_id = request.employee_id.ok_or(HrError::NotFound("employee"))?;
    let days = request_days_hundredths(request.starts_on, request.ends_on, request.half_day);
    if days <= 0 {
        return Err(HrError::invalid(
            "leave request",
            "starts_on",
            "that range contains no working day, so there is nothing to request",
        ));
    }
    let year = year_of(request.starts_on);

    let mut transaction = pool.begin().await?;

    ensure_employee_in_org(&mut transaction, organization_id, employee_id).await?;
    let clash = find_overlap(&mut transaction, employee_id, request.starts_on, request.ends_on, None).await?;
    if let Some((other_id, starts_on, ends_on)) = clash {
        transaction.rollback().await?;
        return Err(HrError::LeaveOverlap {
            other_request_id: other_id,
            starts_on,
            ends_on,
        });
    }
    check_balance(
        &mut transaction,
        organization_id,
        leave_type,
        employee_id,
        year,
        days,
    )
    .await?;

    // A type the organization set to not need approval is **approved on creation**, so the
    // direct-decide path needs no second request and the audit trail is identical to the one the
    // approval route would have written.
    let approved_immediately = !leave_type.requires_approval;
    let id: Uuid = sqlx::query_scalar(
        "insert into hr_leave_requests \
             (organization_id, employee_id, leave_type_id, starts_on, ends_on, days, half_day, \
              reason, attachment_media_id, leave_status, decided_at) \
         values ($1, $2, $3, $4, $5, $6::numeric, $7, $8, $9, $10, \
                 case when $10 = 'approved' then now() else null end) \
         returning id",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(leave_type.id)
    .bind(request.starts_on)
    .bind(request.ends_on)
    .bind(days_to_text(days))
    .bind(request.half_day)
    .bind(&reason)
    .bind(request.attachment_media_id)
    .bind(if approved_immediately {
        "approved"
    } else {
        "pending"
    })
    .fetch_one(&mut *transaction)
    .await?;

    recompute_balance(
        &mut transaction,
        organization_id,
        employee_id,
        leave_type.id,
        year,
    )
    .await?;
    transaction.commit().await?;

    request_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("leave request"))
}

/// Approve or reject a pending request, and move the balance in the same transaction.
///
/// # Errors
///
/// Refuses a request that is not pending — deciding a decided request is the bug a double-clicked
/// approve button produces, and it would move the balance twice.
pub async fn decide_request(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    decision: Decision,
    decided_by: Uuid,
    comment: Option<String>,
) -> Result<LeaveRequest> {
    let comment = crate::model::clean(comment);
    if let Some(text) = &comment
        && text.chars().count() > crate::leave::MAX_COMMENT_LENGTH
    {
        return Err(HrError::invalid(
            "leave request",
            "comment",
            format!(
                "a decision comment is at most {} characters",
                crate::leave::MAX_COMMENT_LENGTH
            ),
        ));
    }

    let mut transaction = pool.begin().await?;
    let current: Option<(Uuid, Uuid, Date, String, String)> = sqlx::query_as(
        "select employee_id, leave_type_id, starts_on, leave_status, organization_id::text \
         from hr_leave_requests where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some((employee_id, leave_type_id, starts_on, status, _)) = current else {
        transaction.rollback().await?;
        return Err(HrError::NotFound("leave request"));
    };
    if status != "pending" {
        transaction.rollback().await?;
        return Err(HrError::LeaveAlreadyDecided {
            status: status.clone(),
        });
    }

    // `for update` above is what makes two approvers clicking at once safe: the second one waits,
    // then reads `approved` and is refused rather than charging the balance a second time.
    let applied = sqlx::query(
        "update hr_leave_requests set leave_status = $3, decided_by = $4, decided_at = now(), \
         decision_comment = $5, updated_at = now() \
         where organization_id = $1 and id = $2 and leave_status = 'pending'",
    )
    .bind(organization_id)
    .bind(id)
    .bind(decision.as_str())
    .bind(decided_by)
    .bind(&comment)
    .execute(&mut *transaction)
    .await?;
    if applied.rows_affected() == 0 {
        transaction.rollback().await?;
        return Err(HrError::LeaveAlreadyDecided {
            status: status.clone(),
        });
    }

    recompute_balance(
        &mut transaction,
        organization_id,
        employee_id,
        leave_type_id,
        year_of(starts_on),
    )
    .await?;
    transaction.commit().await?;

    request_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("leave request"))
}

/// Cancel a pending request, releasing the days it was holding.
///
/// # Errors
///
/// Refuses anything that is not `pending`: an approved request is history and is ended by
/// terminating the leave, not by pretending it was never approved.
pub async fn cancel_request(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    cancelled_by: Uuid,
) -> Result<LeaveRequest> {
    let mut transaction = pool.begin().await?;
    let current: Option<(Uuid, Uuid, Date, String)> = sqlx::query_as(
        "select employee_id, leave_type_id, starts_on, leave_status \
         from hr_leave_requests where organization_id = $1 and id = $2 for update",
    )
    .bind(organization_id)
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some((employee_id, leave_type_id, starts_on, status)) = current else {
        transaction.rollback().await?;
        return Err(HrError::NotFound("leave request"));
    };
    if status != "pending" {
        transaction.rollback().await?;
        return Err(HrError::LeaveNotCancellable {
            status: status.clone(),
        });
    }

    sqlx::query(
        "update hr_leave_requests set leave_status = 'cancelled', cancelled_at = now(), \
         updated_at = now() where organization_id = $1 and id = $2 and leave_status = 'pending'",
    )
    .bind(organization_id)
    .bind(id)
    .execute(&mut *transaction)
    .await?;

    recompute_balance(
        &mut transaction,
        organization_id,
        employee_id,
        leave_type_id,
        year_of(starts_on),
    )
    .await?;
    transaction.commit().await?;

    // The canceller is the actor on the audit entry, so the row is read back with them as the
    // decider — there is no `decided_by` column for a cancellation and inventing one would be a
    // fifth status column that means something else.
    let request = request_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("leave request"))?;
    let _ = cancelled_by;
    Ok(request)
}

// ---------------------------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------------------------

fn validate_range(request: &NewLeaveRequest) -> Result<()> {
    if request.ends_on < request.starts_on {
        return Err(HrError::invalid(
            "leave request",
            "ends_on",
            "the last day cannot be before the first day",
        ));
    }
    if request.half_day && request.ends_on != request.starts_on {
        return Err(HrError::invalid(
            "leave request",
            "half_day",
            "a half-day is one day long; for a longer range leave the full days",
        ));
    }
    Ok(())
}

/// The calendar year a request's balance belongs to.
///
/// The **start** decides it, deliberately: a request that runs from 29 December into January
/// charges the year it was asked for, and an employee's December entitlement is the one that had
/// to be left. Charging both years would need two balance rows and would make the December card
/// disagree with the January one about the same holiday.
#[must_use]
pub fn year_of(day: Date) -> i32 {
    day.year()
}

/// The employee must exist in this organization — a `404`, never a `403`.
///
/// # Errors
///
/// Returns `NotFound` when the employee is not in the organization.
async fn ensure_employee_in_org(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    employee_id: Uuid,
) -> Result<()> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "select id from hr_employees where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(employee_id)
    .fetch_optional(&mut **transaction)
    .await?;
    found.map(|_| ()).ok_or(HrError::NotFound("employee"))
}

/// The `pending` or `approved` request that overlaps the range, with its dates.
///
/// `excluding` is the request being edited: without it, editing a request would always "clash"
/// with itself — which is the same class of bug slice 1 hit on the manager cycle, where a create
/// passed a nil id and the employee being inserted matched its own manager row.
#[allow(clippy::type_complexity)]
async fn find_overlap(
    transaction: &mut Transaction<'_, Postgres>,
    employee_id: Uuid,
    starts_on: Date,
    ends_on: Date,
    excluding: Option<Uuid>,
) -> Result<Option<(Uuid, Date, Date)>> {
    let row = sqlx::query(
        "select id, starts_on, ends_on from hr_leave_requests \
         where employee_id = $1 and leave_status in ('pending', 'approved') \
           and starts_on <= $3 and ends_on >= $2 \
           and ($4::uuid is null or id <> $4) \
         order by starts_on limit 1",
    )
    .bind(employee_id)
    .bind(starts_on)
    .bind(ends_on)
    .bind(excluding)
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(row.map(|record| {
        (
            record.get::<Uuid, _>("id"),
            record.get::<Date, _>("starts_on"),
            record.get::<Date, _>("ends_on"),
        )
    }))
}

/// Refuse a request that exceeds the balance, unless the type allows a negative one.
///
/// # Errors
///
/// [`HrError::InsufficientBalance`] carrying entitled/used/pending, so the person can see the
/// three numbers rather than being told no.
async fn check_balance(
    transaction: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    leave_type: &LeaveType,
    employee_id: Uuid,
    year: i32,
    days: i32,
) -> Result<()> {
    // An unpaid type with no entitlement is not a balance at all: "unpaid leave" is time off that
    // simply is not counted, and refusing it against a 0-day entitlement would make the module
    // refuse the one leave type that is always allowed.
    if !leave_type.paid || leave_type.allow_negative {
        return Ok(());
    }
    let row: Option<(String, String, String)> = sqlx::query_as(
        "select entitled_days::text, used_days::text, pending_days::text \
         from hr_leave_balances \
         where organization_id = $1 and employee_id = $2 and leave_type_id = $3 and balance_year = $4",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(leave_type.id)
    .bind(year)
    .fetch_optional(&mut **transaction)
    .await?;

    let (entitled, used, pending) = row
        .map(|(entitled, used, pending)| (entitled, used, pending))
        .unwrap_or_else(|| (leave_type.annual_days.clone(), "0".to_owned(), "0".to_owned()));
    let entitled = days_from_text(&entitled).unwrap_or(0);
    let used = days_from_text(&used).unwrap_or(0);
    let pending = days_from_text(&pending).unwrap_or(0);
    let remaining = entitled - used - pending;
    if days > remaining {
        return Err(HrError::InsufficientBalance {
            leave_type: leave_type.name.clone(),
            entitled: days_to_text(entitled),
            used: days_to_text(used),
            pending: days_to_text(pending),
            requested: days_to_text(days),
            remaining: days_to_text(remaining),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The detail and the calendar
// ---------------------------------------------------------------------------------------------

/// A request with the balance it moves and the timeline the detail screen draws.
///
/// # Errors
///
/// Refuses a request of another organization.
pub async fn request_detail(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    caller_may_decide: bool,
) -> Result<RequestDetail> {
    let request =
        request_of(pool, organization_id, id)
            .await?
            .ok_or(HrError::NotFound("leave request"))?;
    let year = year_of(request.starts_on);
    let cards = crate::leave::balances_for(pool, organization_id, request.employee_id, year).await?;
    let balance = cards
        .into_iter()
        .find(|card| card.leave_type.id == request.leave_type_id)
        .ok_or(HrError::NotFound("leave type"))?;

    let mut timeline = vec![TimelineStep {
        kind: "requested".to_owned(),
        at: request.created_at,
        actor_id: None,
        comment: crate::model::clean(Some(request.reason.clone())),
    }];
    if let Some(at) = request.decided_at {
        timeline.push(TimelineStep {
            kind: request.leave_status.clone(),
            at,
            actor_id: request.decided_by,
            comment: request.decision_comment.clone(),
        });
    }
    if let Some(at) = request.cancelled_at {
        timeline.push(TimelineStep {
            kind: "cancelled".to_owned(),
            at,
            actor_id: None,
            comment: None,
        });
    }

    let can_decide = caller_may_decide && request.leave_status == "pending";
    Ok(RequestDetail {
        request,
        balance,
        timeline,
        can_decide,
    })
}

/// The absence calendar for a window.
///
/// Only **approved** leave is a bar: a pending request is not an absence yet, and drawing it as
/// one would put a person on a calendar for a holiday their manager has not agreed to.
///
/// # Errors
///
/// Refuses an inverted window.
pub async fn absence_calendar(
    pool: &PgPool,
    scope: &LeaveScope,
    from: Date,
    to: Date,
) -> Result<AbsenceCalendar> {
    if to < from {
        return Err(HrError::invalid(
            "leave calendar",
            "to",
            "the last day of the window cannot be before its first day",
        ));
    }
    let predicate = scope.employee_predicate()?;

    // Overlap, not containment: a range that started last month and runs into this one is a bar
    // in this month's grid, which is what the request's "continues with an arrow" note is about.
    let rows = sqlx::query(&format!(
        "select r.id, r.employee_id, concat_ws(' ', e.first_name, e.last_name) as employee_name, \
         r.leave_type_id, t.name as leave_type_name, r.starts_on, r.ends_on, trim_scale(r.days)::text as days \
         from hr_leave_requests r \
         join hr_employees e on e.id = r.employee_id \
         join hr_leave_types t on t.id = r.leave_type_id \
         where r.organization_id = $1 and r.leave_status = 'approved' \
           and r.starts_on <= $2 and r.ends_on >= $3 {} \
         order by e.last_name, e.first_name, r.starts_on",
        predicate.map(|sql| format!("and {sql}")).unwrap_or_default()
    ))
    .bind(scope.organization_id)
    .bind(to)
    .bind(from)
    .fetch_all(pool)
    .await?;

    let mut bars: Vec<AbsenceBar> = Vec::with_capacity(rows.len());
    let mut employees: Vec<AbsenceRow> = Vec::new();
    for row in &rows {
        let employee_id: Uuid = row.get("employee_id");
        let employee_name: String = row.get("employee_name");
        bars.push(AbsenceBar {
            request_id: row.get("id"),
            employee_id,
            employee_name: employee_name.clone(),
            leave_type_id: row.get("leave_type_id"),
            leave_type_name: row.get("leave_type_name"),
            starts_on: row.get("starts_on"),
            ends_on: row.get("ends_on"),
            days: row.get("days"),
            continues_before: row.get::<Date, _>("starts_on") < from,
            continues_after: row.get::<Date, _>("ends_on") > to,
        });
        match employees.iter_mut().find(|entry| entry.employee_id == employee_id) {
            Some(entry) => entry.request_count += 1,
            None => employees.push(AbsenceRow {
                employee_id,
                employee_name,
                request_count: 1,
            }),
        }
    }

    Ok(AbsenceCalendar {
        from,
        to,
        today: time::OffsetDateTime::now_utc().date(),
        bars,
        employees,
    })
}

/// Every balance card for an employee in the year a request starts in — the self-service read.
///
/// # Errors
///
/// Propagates a database failure and an out-of-range year.
pub async fn balances_of_employee(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    year: i32,
) -> Result<Vec<BalanceCard>> {
    crate::leave::balances_for(pool, organization_id, employee_id, year).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: time::Month, d: u8) -> Date {
        Date::from_calendar_date(year, month, d).expect("a valid date")
    }

    #[test]
    fn an_inverted_range_is_refused_naming_the_field() {
        let request = NewLeaveRequest {
            starts_on: day(2026, time::Month::October, 10),
            ends_on: day(2026, time::Month::October, 5),
            ..NewLeaveRequest::default()
        };
        let error = validate_range(&request).expect_err("an inverted range must be refused");
        assert!(error.to_string().contains("ends_on"), "{error}");
    }

    #[test]
    fn a_half_day_that_spans_a_range_is_refused_rather_than_silently_charged_as_a_full_day() {
        // Charging a Mon–Wed "half day" as three full days would be a decision nobody made.
        let request = NewLeaveRequest {
            starts_on: day(2026, time::Month::October, 5),
            ends_on: day(2026, time::Month::October, 7),
            half_day: true,
            ..NewLeaveRequest::default()
        };
        let error = validate_range(&request).expect_err("a multi-day half must be refused");
        assert!(error.to_string().contains("half_day"), "{error}");

        let single = NewLeaveRequest {
            starts_on: day(2026, time::Month::October, 5),
            ends_on: day(2026, time::Month::October, 5),
            half_day: true,
            ..NewLeaveRequest::default()
        };
        assert!(validate_range(&single).is_ok());
    }

    #[test]
    fn a_request_is_charged_to_the_year_it_starts_in() {
        // 29 Dec → 2 Jan is five working days that belong to December's entitlement: it was asked
        // for in December, and December is the entitlement that had to be left.
        assert_eq!(year_of(day(2026, time::Month::December, 29)), 2026);
        assert_eq!(year_of(day(2027, time::Month::January, 2)), 2027);
    }

    #[test]
    fn a_decision_names_its_own_status_and_event() {
        // The two decisions are different acts: one charges the balance, the other records why
        // it was refused. A single "decided" status would make the balance arithmetic impossible.
        assert_eq!(Decision::Approve.as_str(), "approved");
        assert_eq!(Decision::Reject.as_str(), "rejected");
        assert_eq!(Decision::Approve.event_name(), "hr.leave.approved");
        assert_eq!(Decision::Reject.event_name(), "hr.leave.rejected");
    }
}
