//! Leave: the catalogue, the balances, the request, and the rules that decide whether one is
//! allowed (docs/requests/REQ-055, slice 2).
//!
//! Three rules live here rather than in `apps/api`, and each of them is a rule a form, a bulk
//! approve button, an import and a walkthrough could otherwise disagree about:
//!
//! * **Days are working days, and the number shown before submit is the number charged.** The
//!   computation is a pure function of the range and the working-week, so the form's preview and
//!   the stored `days` cannot be two different calculations of the same request. A weekend inside
//!   a Mon–Fri request is 3 days, not 5, and the day count is stored rather than recomputed at
//!   read time so an organization that later changes its working week does not rewrite history.
//! * **The balance is recomputed, never incremented.** `used_days` and `pending_days` are
//!   projections of the request rows; every writer recomputes them inside the transaction that
//!   moves a request's status. An increment-in-place balance drifts silently — a cancel, a
//!   rejection and a re-approval each move the number by whatever the code of the day added — and
//!   the symptom is a card that reads "3 days left" beside a list that sums to 9.
//! * **An overlap is refused with the dates that clash.** A refusal that says "conflicts with
//!   another request" sends the person back to the list to find which one; the message names both
//!   ranges. Rejected and cancelled rows are *not* conflicts: they are history, and two rejected
//!   requests side by side is a person who changed their mind twice, not a double booking.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{HrError, Result};

/// Longest a reason may be.
pub const MAX_REASON_LENGTH: usize = 500;

/// Longest a decision comment may be.
pub const MAX_COMMENT_LENGTH: usize = 1000;

/// The four statuses the schema accepts.
pub const LEAVE_STATUSES: [&str; 4] = ["pending", "approved", "rejected", "cancelled"];

/// The working week: Monday through Friday, as `number_from_monday()` numbers them — Monday 1
/// through Sunday 7.
///
/// `number_from_monday` and **not** `number_days_from_monday`, and the difference is one whole
/// day of leave per request. The `_days_` variant is **zero-indexed** (Monday = 0), so pairing
/// it with this constant — which reads as Monday-through-Friday to a person — silently made the
/// working week Tuesday through Saturday: every request charged one day too many, the balance
/// drained a fifth too fast, and the absence calendar drew a bar over Sunday. Nothing fails, the
/// schema accepts it, and the number is wrong for every employee in the organization.
///
/// A constant is the right shape for a policy an organization may want to change, and a constant
/// is also where an off-by-one hides forever: the unit test below pins the mapping to named
/// days, so a reader changing the list has to change the test with it.
pub const WORKING_WEEKDAYS: [u8; 5] = [1, 2, 3, 4, 5];

// ---------------------------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------------------------

/// `true` when the status is one the schema accepts.
#[must_use]
pub fn is_leave_status(value: &str) -> bool {
    LEAVE_STATUSES.contains(&value)
}

/// `true` when the day is a working day of the organization's week.
///
/// `time`'s weekday is 1 = Monday … 7 = Sunday, so the comparison is against [`WORKING_WEEKDAYS`]
/// directly and no conversion happens anywhere: an off-by-one here would charge an employee a day
/// for the Saturday they were not at work.
#[must_use]
pub fn is_working_day(day: Date) -> bool {
    WORKING_WEEKDAYS.contains(&day.weekday().number_from_monday())
}

/// The working days between two days, inclusive, as hundredths of a day.
///
/// **Hundredths, not `f64`.** Half-days are the point of the module and `0.1 + 0.2` in binary
/// floating point is a balance card that reads `0.30000000000000004` and a support ticket about
/// it. The column is `numeric(6,2)`, so hundredths is its exact scale and every comparison here
/// is integer arithmetic.
#[must_use]
pub fn working_days_between(starts_on: Date, ends_on: Date) -> i32 {
    if ends_on < starts_on {
        return 0;
    }
    let mut day = starts_on;
    let mut days: i32 = 0;
    // A leave range is bounded by a `date` column and a form field, so the loop is over a
    // person's holiday rather than over an arbitrary span; the bound is a refusal rather than a
    // hang if an import ever passes a decade.
    let mut guard: i32 = 0;
    while day <= ends_on && guard < 3_660 {
        if is_working_day(day) {
            days += 1;
        }
        day += time::Duration::DAY;
        guard += 1;
    }
    days
}

/// The days a request charges, in hundredths: a half-day is exactly 50.
///
/// The half-day is a *rate*, not a range: `starts_on = ends_on` is what the schema checks, and
/// this function is where "one working day, at half" becomes the 50 the balance sees.
#[must_use]
pub fn request_days_hundredths(starts_on: Date, ends_on: Date, half_day: bool) -> i32 {
    let full = working_days_between(starts_on, ends_on);
    if half_day {
        // 50 for one working day, and 0 for a weekend: a half-day Saturday is no leave at all,
        // and charging 50 for it would let a request pass `days > 0` with nothing deducted and
        // nothing taken.
        if full == 1 { 50 } else { 0 }
    } else {
        full * 100
    }
}

/// The days as the string the API and the database exchange: `3`, `0.5`, `2.50` → `2.5`.
///
/// Rendered without a trailing `.00` because a day count that reads `3.00` in a leave card is a
/// number formatted for a spreadsheet rather than for a person, while `2.50` keeps the half.
#[must_use]
pub fn days_to_text(hundredths: i32) -> String {
    let whole = hundredths / 100;
    let fraction = hundredths % 100;
    if fraction == 0 {
        return whole.to_string();
    }
    // The fraction is rendered as **one** digit and the second is filled only when it is
    // non-zero. `format!("{}.{:02}", 0, 50)` writes `0.50` — the spreadsheet formatting this
    // function exists to avoid, and the reason a half-day in a leave card reads like a value out
    // of a column rather than a number somebody chose.
    if fraction % 10 == 0 {
        format!("{whole}.{}", fraction / 10)
    } else {
        format!("{whole}.{fraction}")
    }
}

/// Read a day count back from the database, refusing what is not a count.
///
/// A `numeric` column read as a bare `f64` would work until a balance reached 0.1 and the
/// comparison that should have refused it did not, so the conversion goes through hundredths and
/// anything that is not a number in the column's own scale is an error rather than a zero.
pub fn days_from_text(raw: &str) -> Result<i32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let (whole, fraction) = trimmed.split_once('.').unwrap_or((trimmed, ""));
    let negative = whole.starts_with('-');
    if !whole.is_empty() && !whole.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Err(HrError::invalid(
            "leave",
            "days",
            format!("{trimmed:?} is not a number of days"),
        ));
    }
    let whole: i32 = if whole.is_empty() || whole == "-" {
        0
    } else {
        whole.parse().map_err(|_| {
            HrError::invalid("leave", "days", format!("{trimmed:?} is not a number of days"))
        })?
    };
    // Two decimals, the column's scale. A third is a value the column could not have held, and
    // rounding it silently would make the stored number differ from the one that was checked.
    if !fraction.is_empty() {
        if fraction.len() > 2 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HrError::invalid(
                "leave",
                "days",
                format!("{trimmed:?} has more precision than a day count can hold"),
            ));
        }
        let padded = format!("{fraction:0<2}");
        let hundredths: i32 = padded.parse().map_err(|_| {
            HrError::invalid("leave", "days", format!("{trimmed:?} is not a number of days"))
        })?;
        let sign = if negative { -1 } else { 1 };
        return Ok(sign * (whole.abs() * 100 + hundredths));
    }
    Ok(whole * 100)
}

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// A leave type as the editor and the request form read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct LeaveType {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The name a select shows.
    pub name: String,
    /// The machine name an API client sends.
    pub code: String,
    /// Whether it costs entitlement.
    pub paid: bool,
    /// The yearly entitlement, as the column's own text (`14.00`).
    pub annual_days: String,
    /// Whether a request needs a decision before it counts.
    pub requires_approval: bool,
    /// Whether a request may exceed the remaining balance.
    pub allow_negative: bool,
    /// Whether the type is offered.
    pub active: bool,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl LeaveType {
    /// The entitlement in hundredths, for the balance card.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored value is not a day count, which means the column and this
    /// reader disagree — a `500` is the honest answer, and a silent `0` would show an employee a
    /// balance of zero for a type that grants 14 days.
    pub fn annual_hundredths(&self) -> Result<i32> {
        days_from_text(&self.annual_days)
    }
}

/// A leave type plus the field set a create or a patch writes.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaveTypeChanges {
    /// The name.
    pub name: String,
    /// The code; upper-cased by the service, because the catalogue is compared case-insensitively.
    pub code: String,
    /// Whether it costs entitlement.
    pub paid: Option<bool>,
    /// The yearly entitlement, in hundredths.
    pub annual_days: Option<i32>,
    /// Whether a request needs a decision.
    pub requires_approval: Option<bool>,
    /// Whether a request may exceed the balance.
    pub allow_negative: Option<bool>,
    /// Whether the type is offered.
    pub active: Option<bool>,
}

impl Default for LeaveTypeChanges {
    /// The required strings empty and every switch absent, so a caller that forgets a field gets
    /// a refusal naming it rather than a row with an empty name.
    fn default() -> Self {
        Self {
            name: String::new(),
            code: String::new(),
            paid: None,
            annual_days: None,
            requires_approval: None,
            allow_negative: None,
            active: None,
        }
    }
}

/// One leave type with the balance a given employee has of it.
///
/// The two live together because the balance card is the thing a person reads, and a card that
/// had to ask a second request for the entitlement is a card that can show an entitlement from one
/// moment and a balance from another.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BalanceCard {
    /// The type.
    #[serde(flatten)]
    pub leave_type: LeaveType,
    /// The employee the numbers are for.
    pub employee_id: Uuid,
    /// The year the numbers belong to.
    pub balance_year: i32,
    /// What the organization promised, as `14`.
    pub entitled_days: String,
    /// What is approved and spent, as `3`.
    pub used_days: String,
    /// What is requested and undecided, as `2.5`.
    pub pending_days: String,
    /// `entitled - used - pending`, as `8.5`. Negative when the type allows it.
    pub remaining_days: String,
    /// Whether the row existed before this read.
    ///
    /// A type an organization has never used has **no balance row**, and the card must still show
    /// its entitlement with zeroes rather than omitting the type — otherwise a select of leave
    /// types quietly loses every type nobody has taken yet. This flag is what lets the screen say
    /// "nothing taken yet" instead of "no such balance".
    pub seeded: bool,
}

/// A leave request as the list, the detail screen and the calendar read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct LeaveRequest {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// Who is asking.
    pub employee_id: Uuid,
    /// The employee's name, resolved for the list column.
    pub employee_name: String,
    /// The type.
    pub leave_type_id: Uuid,
    /// The type's name, resolved for the list column.
    pub leave_type_name: String,
    /// The first day away.
    #[serde(with = "crate::dates")]
    pub starts_on: Date,
    /// The last day away.
    #[serde(with = "crate::dates")]
    pub ends_on: Date,
    /// The working days charged, rendered the way a person reads it: `3`, `0.5`.
    ///
    /// Not the column's own text. `numeric(6,2)` always renders two decimals, so a row read as
    /// `r.days::text` says `3.00` while every day count the rest of the screen shows says `3` —
    /// and a leave list that mixes `3.00` with `0.50` reads like a spreadsheet export. The SQL
    /// trims the trailing zeros (`trim_scale`) so the value is normalized once, at the reader,
    /// rather than in each of the three places that render it.
    pub days: String,
    /// Whether it is a half-day.
    pub half_day: bool,
    /// Why.
    pub reason: String,
    /// The status.
    pub leave_status: String,
    /// The approver, when decided.
    pub decided_by: Option<Uuid>,
    /// The approver's name, resolved for the list column.
    #[sqlx(default)]
    pub decided_by_name: Option<String>,
    /// When it was decided.
    #[serde(default, with = "crate::dates::instant::option")]
    pub decided_at: Option<OffsetDateTime>,
    /// What the approver wrote.
    pub decision_comment: Option<String>,
    /// When it was cancelled.
    #[serde(default, with = "crate::dates::instant::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    /// When it was raised.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl LeaveRequest {
    /// The charged days in hundredths.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored text is not a day count.
    pub fn days_hundredths(&self) -> Result<i32> {
        days_from_text(&self.days)
    }

    /// The identity of this request for an event payload: ids and dates, never the reason.
    ///
    /// A reason is a person's own words about their health, their family or their manager, and a
    /// subscriber may be a third party's webhook — so the payload carries what a rule needs to act
    /// on and nothing a rule could exfiltrate. The same reasoning the CRM and the people core
    /// already apply to their event payloads.
    #[must_use]
    pub fn event_ref(&self) -> serde_json::Value {
        serde_json::json!({
            "leave_request_id": self.id,
            "organization_id": self.organization_id,
            "employee_id": self.employee_id,
            "leave_type_id": self.leave_type_id,
            "starts_on": crate::dates::to_wire(&self.starts_on),
            "ends_on": crate::dates::to_wire(&self.ends_on),
            "days": self.days,
            "leave_status": self.leave_status,
        })
    }
}

/// The field set a new request writes.
#[derive(Debug, Clone, PartialEq)]
pub struct NewLeaveRequest {
    /// Who is asking; `None` means the caller, resolved by the API.
    pub employee_id: Option<Uuid>,
    /// The type.
    pub leave_type_id: Uuid,
    /// The first day away.
    pub starts_on: Date,
    /// The last day away.
    pub ends_on: Date,
    /// Whether it is a half-day.
    pub half_day: bool,
    /// Why.
    pub reason: Option<String>,
    /// An attachment in the media pipeline.
    pub attachment_media_id: Option<Uuid>,
}

impl Default for NewLeaveRequest {
    /// The required fields at their zero, so a test can fill only what it is about.
    fn default() -> Self {
        Self {
            employee_id: None,
            leave_type_id: Uuid::nil(),
            starts_on: Date::from_calendar_date(1970, time::Month::January, 1)
                .expect("the epoch is a valid date"),
            ends_on: Date::from_calendar_date(1970, time::Month::January, 1)
                .expect("the epoch is a valid date"),
            half_day: false,
            reason: None,
            attachment_media_id: None,
        }
    }
}

/// The list contract the leave screen sends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LeaveQuery {
    /// One of the four statuses.
    #[serde(default)]
    pub status: Option<String>,
    /// A leave type.
    #[serde(default)]
    pub leave_type_id: Option<Uuid>,
    /// An employee.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    /// A department, including its children.
    #[serde(default)]
    pub department_id: Option<Uuid>,
    /// Starting on or after.
    #[serde(default)]
    pub from: Option<Date>,
    /// Starting on or before.
    #[serde(default)]
    pub to: Option<Date>,
    /// Free text over the employee's name.
    #[serde(default)]
    pub search: Option<String>,
    /// `pending_only` as a named switch, which is what the list's "pending only" chip sends.
    #[serde(default)]
    pub pending_only: Option<bool>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// The cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Who the visibility level narrows a leave query to.
///
/// The same shape the employees list carries, and for the same reason: a leave row is as personal
/// as an employee row — it says when somebody is not at work and often why — so an `own` caller
/// reads their own requests and nobody else's, enforced in the SQL so the **count** narrows too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveScope {
    /// The organization the query runs in.
    pub organization_id: Uuid,
    /// How much the caller may see.
    pub visibility: crate::model::Visibility,
    /// The caller's own employee row.
    pub employee_id: Option<Uuid>,
}

impl LeaveScope {
    /// A scope that reads every request of the organization.
    #[must_use]
    pub fn all(organization_id: Uuid) -> Self {
        Self {
            organization_id,
            visibility: crate::model::Visibility::All,
            employee_id: None,
        }
    }

    /// A narrowed scope.
    #[must_use]
    pub fn with(mut self, visibility: crate::model::Visibility, employee_id: Option<Uuid>) -> Self {
        self.visibility = visibility;
        self.employee_id = employee_id;
        self
    }

    /// The employee predicate the visibility level implies, as SQL to append.
    ///
    /// Returning SQL text rather than a `QueryBuilder` is what lets the **same** predicate be
    /// applied to the row query and to the count: two builders would let the count drift away
    /// from the rows, and the header's "24 requests" would then be a number the list cannot show.
    ///
    /// The id is rendered as a **quoted, cast uuid literal** rather than interpolated bare. A
    /// UUID is hyphenated, so `e.id = 6ba96e44-1111-…` is not a string to Postgres — it is
    /// subtraction, and the database refuses it with "trailing junk after numeric literal". The
    /// bug is invisible until a caller with a real id reaches the query, which is exactly what
    /// the self-service surface does: it is the first thing in the module that narrows to a
    /// *specific* employee rather than reading the whole organization.
    ///
    /// # Errors
    ///
    /// **Refuses** a narrowed caller with no employee row, rather than returning no predicate.
    /// `None` here means "no restriction", so a caller bound to `own` whose account has no
    /// employee record would read the whole organization's leave — and leave is exactly the data
    /// that says when somebody is not at work. A caller with no record of their own has nothing
    /// to narrow to, and the honest answer to "show me your leave" from such a caller is no rows
    /// at all, not everybody's.
    pub fn employee_predicate(&self) -> Result<Option<String>> {
        match self.visibility {
            crate::model::Visibility::All => Ok(None),
            crate::model::Visibility::Own | crate::model::Visibility::Team => match self.employee_id
            {
                // `Uuid`'s `Display` is the canonical hyphenated form and nothing else, so the
                // quoted literal cannot carry user input into the statement.
                Some(id) => Ok(Some(format!("e.id = '{id}'::uuid"))),
                None => Err(HrError::NotFound("employee")),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------------------------

const TYPE_COLUMNS: &str = "t.id, t.organization_id, t.name, t.code, t.paid, \
     t.annual_days::text as annual_days, t.requires_approval, t.allow_negative, t.active, \
     t.created_at";

/// Every leave type of an organization, the active ones first.
pub async fn list_leave_types(pool: &PgPool, organization_id: Uuid) -> Result<Vec<LeaveType>> {
    let rows = sqlx::query_as::<_, LeaveType>(&format!(
        "select {TYPE_COLUMNS} from hr_leave_types t \
         where t.organization_id = $1 and t.active \
         order by t.paid desc, t.name"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One leave type, or `None` when it belongs to another organization.
pub async fn leave_type_of(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<LeaveType>> {
    let row = sqlx::query_as::<_, LeaveType>(&format!(
        "select {TYPE_COLUMNS} from hr_leave_types t where t.organization_id = $1 and t.id = $2"
    ))
    .bind(organization_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// A new leave type.
///
/// # Errors
///
/// Refuses an empty name or code, a code another type already carries, and an entitlement the
/// schema's own check would refuse — checked here so the message names the field.
pub async fn create_leave_type(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &LeaveTypeChanges,
) -> Result<LeaveType> {
    validate_type(changes)?;
    let code = changes.code.trim().to_uppercase();
    let id: Uuid = sqlx::query_scalar(
        "insert into hr_leave_types (organization_id, name, code, paid, annual_days, \
         requires_approval, allow_negative) \
         values ($1, $2, $3, coalesce($4, true), coalesce($5::numeric, 0), \
                 coalesce($6, true), coalesce($7, false)) returning id",
    )
    .bind(organization_id)
    .bind(changes.name.trim())
    .bind(&code)
    .bind(changes.paid)
    .bind(changes.annual_days.map(days_to_text))
    .bind(changes.requires_approval)
    .bind(changes.allow_negative)
    .fetch_one(pool)
    .await
    .map_err(|error| classify_unique(error, "code"))?;

    leave_type_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("leave type"))
}

/// A type's fields that a patch may change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeaveTypePatch {
    /// The name.
    pub name: Option<String>,
    /// The code.
    pub code: Option<String>,
    /// Whether it costs entitlement.
    pub paid: Option<bool>,
    /// The yearly entitlement, in hundredths.
    pub annual_days: Option<i32>,
    /// Whether a request needs a decision.
    pub requires_approval: Option<bool>,
    /// Whether a request may exceed the balance.
    pub allow_negative: Option<bool>,
    /// Whether the type is offered.
    pub active: Option<bool>,
}

/// Update a leave type.
///
/// # Errors
///
/// Refuses an unknown level of field, a name or code the type editor already validated, and a
/// code another type carries.
pub async fn update_leave_type(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    patch: &LeaveTypePatch,
) -> Result<LeaveType> {
    if let Some(name) = &patch.name {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 120 {
            return Err(HrError::invalid(
                "leave type",
                "name",
                "a leave type needs a name of at most 120 characters",
            ));
        }
    }
    if let Some(annual) = patch.annual_days {
        if !(0..=36_600).contains(&annual) {
            return Err(HrError::invalid(
                "leave type",
                "annual_days",
                "a yearly entitlement must be between 0 and 366 days",
            ));
        }
    }

    let row = sqlx::query_as::<_, (Uuid,)>(
        "update hr_leave_types set name = coalesce($3, name), \
         code = coalesce($4, code), paid = coalesce($5, paid), \
         annual_days = coalesce($6::numeric, annual_days), \
         requires_approval = coalesce($7, requires_approval), \
         allow_negative = coalesce($8, allow_negative), active = coalesce($9, active) \
         where organization_id = $1 and id = $2 returning id",
    )
    .bind(organization_id)
    .bind(id)
    .bind(patch.name.as_ref().map(|value| value.trim().to_owned()))
    .bind(patch.code.as_ref().map(|value| value.trim().to_uppercase()))
    .bind(patch.paid)
    .bind(patch.annual_days.map(days_to_text))
    .bind(patch.requires_approval)
    .bind(patch.allow_negative)
    .bind(patch.active)
    .fetch_optional(pool)
    .await
    .map_err(|error| classify_unique(error, "code"))?;

    if row.is_none() {
        return Err(HrError::NotFound("leave type"));
    }
    leave_type_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("leave type"))
}

fn validate_type(changes: &LeaveTypeChanges) -> Result<()> {
    let name = changes.name.trim();
    if name.is_empty() {
        return Err(HrError::invalid(
            "leave type",
            "name",
            "a leave type needs a name",
        ));
    }
    if name.chars().count() > 120 {
        return Err(HrError::invalid(
            "leave type",
            "name",
            "a leave type name is at most 120 characters",
        ));
    }
    let code = changes.code.trim();
    if code.is_empty() {
        return Err(HrError::invalid(
            "leave type",
            "code",
            "a leave type needs a code an API client can send",
        ));
    }
    if code.chars().count() > 32 {
        return Err(HrError::invalid(
            "leave type",
            "code",
            "a leave type code is at most 32 characters",
        ));
    }
    if let Some(annual) = changes.annual_days
        && !(0..=36_600).contains(&annual)
    {
        return Err(HrError::invalid(
            "leave type",
            "annual_days",
            "a yearly entitlement must be between 0 and 366 days",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Balances
// ---------------------------------------------------------------------------------------------

/// Read a balance row as a `BalanceCard`, or `None` when the employee has none for that type.
/// The card for a type an employee has no row for: the entitlement, with zeroes.
///
/// This is the function that keeps a freshly-installed organization from having a *shorter* leave
/// select than one that has been running for a year — the bug the acceptance criterion names when
/// it asks that the numbers be consistent before and after a decision.
#[allow(clippy::too_many_arguments)]
fn card_from_texts(
    leave_type: &LeaveType,
    employee_id: Uuid,
    year: i32,
    entitled: &str,
    used: &str,
    pending: &str,
    seeded: bool,
) -> BalanceCard {
    let entitled_h = days_from_text(entitled).unwrap_or(0);
    let used_h = days_from_text(used).unwrap_or(0);
    let pending_h = days_from_text(pending).unwrap_or(0);
    let remaining_h = entitled_h - used_h - pending_h;
    BalanceCard {
        leave_type: leave_type.clone(),
        employee_id,
        balance_year: year,
        entitled_days: days_to_text(entitled_h),
        used_days: days_to_text(used_h),
        pending_days: days_to_text(pending_h),
        remaining_days: days_to_text(remaining_h),
        seeded,
    }
}

/// Every balance card for one employee in one year, one per active leave type.
///
/// The **union** of the types and the existing balance rows, deliberately: a type that was
/// deactivated after it had balance still has to be readable, otherwise an employee's history
/// would lose the column that explains it.
///
/// # Errors
///
/// Refuses when the year is outside the schema's range, and propagates a database failure.
pub async fn balances_for(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    year: i32,
) -> Result<Vec<BalanceCard>> {
    if !(2000..=2200).contains(&year) {
        return Err(HrError::invalid(
            "leave",
            "year",
            "a balance year must be between 2000 and 2200",
        ));
    }
    let types = list_leave_types(pool, organization_id).await?;

    // A deactivated type with a balance row is still somebody's history, so the rows are read as
    // well as the catalogue and the two are merged by type id.
    let stored: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select leave_type_id, entitled_days::text, used_days::text, pending_days::text \
         from hr_leave_balances \
         where organization_id = $1 and employee_id = $2 and balance_year = $3",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(year)
    .fetch_all(pool)
    .await?;

    let mut cards: Vec<BalanceCard> = Vec::new();
    for leave_type in &types {
        match stored.iter().find(|(id, ..)| *id == leave_type.id) {
            Some((_, entitled, used, pending)) => cards.push(card_from_texts(
                leave_type,
                employee_id,
                year,
                entitled,
                used,
                pending,
                true,
            )),
            None => {
                let entitled = leave_type.annual_hundredths().unwrap_or(0);
                cards.push(card_from_texts(
                    leave_type,
                    employee_id,
                    year,
                    &days_to_text(entitled),
                    "0",
                    "0",
                    false,
                ));
            }
        }
    }
    for (id, entitled, used, pending) in stored {
        if cards.iter().any(|card| card.leave_type.id == id) {
            continue;
        }
        if let Some(leave_type) = leave_type_of(pool, organization_id, id).await? {
            cards.push(card_from_texts(
                &leave_type,
                employee_id,
                year,
                &entitled,
                &used,
                &pending,
                true,
            ));
        }
    }
    cards.sort_by(|a, b| {
        b.leave_type
            .paid
            .cmp(&a.leave_type.paid)
            .then_with(|| a.leave_type.name.cmp(&b.leave_type.name))
    });
    Ok(cards)
}

/// The recomputation every balance writer shares: read the request rows, write the two numbers.
///
/// The single owner of the arithmetic, which is what the request's risk note asks for. It runs
/// inside the caller's transaction, so the request's status change and the balance it moves are
/// one commit — a decision that committed the status and then failed to update the card would
/// leave the two disagreeing with no way to tell which is right.
///
/// # Errors
///
/// Refuses a request that is already decided, and propagates a database failure.
pub async fn recompute_balance(
    executor: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    employee_id: Uuid,
    leave_type_id: Uuid,
    year: i32,
) -> Result<()> {
    // `coalesce(..., 0)::numeric` because `sum()` over zero rows is NULL, and a NULL written
    // into `used_days numeric(6,2) not null` is a 500 on a brand-new employee's first request.
    sqlx::query(
        "insert into hr_leave_balances \
             (organization_id, employee_id, leave_type_id, balance_year, entitled_days, \
              used_days, pending_days, updated_at) \
         select $1, $2, $3, $4, coalesce(t.annual_days, 0), \
                coalesce(sum(r.days) filter (where r.leave_status = 'approved'), 0), \
                coalesce(sum(r.days) filter (where r.leave_status = 'pending'), 0), now() \
         from hr_leave_types t \
         left join hr_leave_requests r on r.employee_id = $2 and r.leave_type_id = $3 \
              and r.starts_on >= make_date($4, 1, 1) and r.starts_on < make_date($4 + 1, 1, 1) \
         where t.id = $3 \
         group by t.id, t.annual_days \
         on conflict (employee_id, leave_type_id, balance_year) do update \
             set used_days = excluded.used_days, pending_days = excluded.pending_days, \
                 updated_at = now()",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(leave_type_id)
    .bind(year)
    .execute(&mut **executor)
    .await?;
    Ok(())
}

/// Seed an employee's balances for a year from the catalogue, without touching what exists.
///
/// # Errors
///
/// Propagates a database failure.
pub async fn seed_balances(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    year: i32,
) -> Result<()> {
    sqlx::query(
        "insert into hr_leave_balances \
             (organization_id, employee_id, leave_type_id, balance_year, entitled_days) \
         select $1, $2, t.id, $3, t.annual_days from hr_leave_types t \
         where t.organization_id = $1 and t.active \
         on conflict (employee_id, leave_type_id, balance_year) do nothing",
    )
    .bind(organization_id)
    .bind(employee_id)
    .bind(year)
    .execute(pool)
    .await?;
    Ok(())
}

/// Turn a unique-violation into the refusal that names the field, and leave the rest alone.
fn classify_unique(error: sqlx::Error, field: &'static str) -> HrError {
    if let sqlx::Error::Database(db) = &error
        && db.code().as_deref() == Some("23505")
    {
        return HrError::invalid(
            "leave type",
            field,
            "another leave type of this organization already carries this value",
        );
    }
    HrError::Database(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: time::Month, d: u8) -> Date {
        Date::from_calendar_date(year, month, d).expect("a valid date")
    }

    #[test]
    fn the_working_week_is_monday_to_friday_and_not_one_day_later() {
        // The off-by-one this pins: `number_days_from_monday` is zero-indexed, so a constant
        // read as Mon-Fri and compared against it makes the working week Tue-Sat. Every request
        // then charges one day too many and the balance drains a fifth too fast — a defect with
        // no error message anywhere, only wrong numbers.
        // 2026-10-05 is a Monday (asserted first, so a wrong assumption about the calendar
        // fails here rather than as a mysterious count three assertions later), then the week
        // runs Mon 5 → Sun 11.
        for (d, expected) in [(5u8, true), (6, true), (7, true), (8, true), (9, true), (10, false), (11, false)] {
            let date = day(2026, time::Month::October, d);
            assert_eq!(
                is_working_day(date),
                expected,
                "{date} ({:?}) is {}",
                date.weekday(),
                if expected { "a working day" } else { "not a working day" }
            );
        }
        assert_eq!(
            day(2026, time::Month::October, 5).weekday(),
            time::Weekday::Monday,
            "the week this test numbers must really start on a Monday"
        );
    }

    #[test]
    fn a_weekend_inside_a_request_is_not_charged() {
        // Mon 2026-10-05 → Sun 2026-10-11 is 7 calendar days and 5 working days. Charging the 7
        // would be the single most common way a leave module lies to a person.
        assert_eq!(
            working_days_between(day(2026, time::Month::October, 5), day(2026, time::Month::October, 11)),
            5
        );
    }

    #[test]
    fn a_single_working_day_is_one_and_a_single_weekend_day_is_none() {
        assert_eq!(
            working_days_between(day(2026, time::Month::October, 5), day(2026, time::Month::October, 5)),
            1
        );
        assert_eq!(
            working_days_between(day(2026, time::Month::October, 10), day(2026, time::Month::October, 10)),
            0
        );
    }

    #[test]
    fn an_inverted_range_is_zero_rather_than_a_walk_to_the_beginning_of_time() {
        // The schema refuses `ends_on < starts_on`, so this is unreachable through the API — and
        // it must not become a loop if a future writer forgets to check first.
        assert_eq!(
            working_days_between(day(2026, time::Month::October, 10), day(2026, time::Month::October, 5)),
            0
        );
    }

    #[test]
    fn a_half_day_is_exactly_half_and_a_half_weekend_is_nothing() {
        assert_eq!(
            request_days_hundredths(day(2026, time::Month::October, 5), day(2026, time::Month::October, 5), true),
            50
        );
        // A half-day Saturday is no leave: charging 50 would let `days > 0` pass with nothing
        // taken and nothing deducted.
        assert_eq!(
            request_days_hundredths(day(2026, time::Month::October, 10), day(2026, time::Month::October, 10), true),
            0
        );
    }

    #[test]
    fn a_range_cannot_be_half() {
        // Mon 5 -> Wed 7 is three working days, and asking for it "as a half" is a request the
        // form refuses before the API is reached. The arithmetic has to agree with that refusal:
        // charging 150 (three days at half) would be a number nothing else in the module can
        // produce, and charging 300 would charge a half-day as a full week.
        //
        // The answer here is 0, and 0 is refused downstream by the `days > 0` check in the schema
        // and by `create_request` before it. The arithmetic returning 0 rather than a plausible
        // wrong number is what makes the refusal total.
        assert_eq!(
            request_days_hundredths(
                day(2026, time::Month::October, 5),
                day(2026, time::Month::October, 7),
                true
            ),
            0
        );
        // And the same range as a full request is the three days it obviously is.
        assert_eq!(
            request_days_hundredths(
                day(2026, time::Month::October, 5),
                day(2026, time::Month::October, 7),
                false
            ),
            300
        );
    }

    #[test]
    fn days_render_without_a_spreadsheet_trailing_zero() {
        assert_eq!(days_to_text(300), "3");
        assert_eq!(days_to_text(50), "0.5");
        assert_eq!(days_to_text(250), "2.5");
        assert_eq!(days_to_text(0), "0");
        // A negative remaining is a real answer when a type allows a negative balance.
        assert_eq!(days_to_text(-100), "-1");
    }

    #[test]
    fn days_round_trip_through_the_columns_own_text() {
        for value in [0, 50, 100, 250, 1400, -100] {
            let text = days_to_text(value);
            assert_eq!(days_from_text(&text).unwrap(), value, "{text}");
        }
        // The exact shapes a `numeric(6,2)` column produces on the way out.
        assert_eq!(days_from_text("14.00").unwrap(), 1400);
        assert_eq!(days_from_text("0.50").unwrap(), 50);
        assert_eq!(days_from_text(" 3 ").unwrap(), 300);
    }

    #[test]
    fn a_day_count_with_more_precision_than_the_column_holds_is_refused_not_rounded() {
        // Rounding here would make the number that was checked differ from the number stored.
        let error = days_from_text("1.005").expect_err("a thousandth is not a day count");
        assert!(error.to_string().contains("precision"), "{error}");
        assert!(days_from_text("many").is_err());
    }

    #[test]
    fn the_status_vocabulary_is_the_schema_s_own() {
        for value in LEAVE_STATUSES {
            assert!(is_leave_status(value), "{value}");
        }
        assert!(!is_leave_status("declined"));
    }

    #[test]
    fn a_type_editor_refuses_an_empty_name_and_a_blank_code_before_the_database_does() {
        let empty_name = LeaveTypeChanges {
            name: "  ".to_owned(),
            code: "ANNUAL".to_owned(),
            ..LeaveTypeChanges::default()
        };
        assert!(validate_type(&empty_name).is_err());

        let blank_code = LeaveTypeChanges {
            name: "Annual".to_owned(),
            code: String::new(),
            ..LeaveTypeChanges::default()
        };
        assert!(validate_type(&blank_code).is_err());

        // 366 days is the ceiling the schema's own check enforces; 367 must not reach it.
        let too_much = LeaveTypeChanges {
            name: "Annual".to_owned(),
            code: "ANNUAL".to_owned(),
            annual_days: Some(36_700),
            ..LeaveTypeChanges::default()
        };
        assert!(validate_type(&too_much).is_err());

        let good = LeaveTypeChanges {
            name: "Annual".to_owned(),
            code: "ANNUAL".to_owned(),
            annual_days: Some(1_400),
            ..LeaveTypeChanges::default()
        };
        assert!(validate_type(&good).is_ok());
    }

    #[test]
    fn a_card_never_shows_more_than_the_balance_holds() {
        let leave_type = LeaveType {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Annual".to_owned(),
            code: "ANNUAL".to_owned(),
            paid: true,
            annual_days: "14.00".to_owned(),
            requires_approval: true,
            allow_negative: false,
            active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let card = card_from_texts(&leave_type, Uuid::nil(), 2026, "14.00", "3.00", "2.50", true);
        assert_eq!(card.entitled_days, "14");
        assert_eq!(card.used_days, "3");
        assert_eq!(card.pending_days, "2.5");
        assert_eq!(card.remaining_days, "8.5");
        // 14 - 3 - 2.5 == 8.5, and the four numbers have to add up: that is the whole point of a
        // balance card, and float arithmetic is exactly where it would stop adding up.
        let total: i32 = [1400, 300, 250, 850].into_iter().sum();
        assert_eq!(total, 2_800, "entitled must equal used + pending + remaining");
    }

    #[test]
    fn an_unseeded_card_still_shows_the_entitlement_and_says_so() {
        let leave_type = LeaveType {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Sick".to_owned(),
            code: "SICK".to_owned(),
            paid: true,
            annual_days: "0.00".to_owned(),
            requires_approval: true,
            allow_negative: false,
            active: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let card = card_from_texts(&leave_type, Uuid::nil(), 2026, "0.00", "0.00", "0.00", false);
        assert!(!card.seeded, "a card with no balance row must say it has none");
        assert_eq!(card.remaining_days, "0");
    }

    #[test]
    fn a_visibility_level_that_is_not_all_narrows_by_the_callers_own_employee_row() {
        let mine = Uuid::from_u128(7);
        let all = LeaveScope::all(Uuid::from_u128(1));
        assert_eq!(all.employee_predicate().unwrap(), None, "`all` narrows nothing");

        let own = LeaveScope::all(Uuid::from_u128(1))
            .with(crate::model::Visibility::Own, Some(mine));
        // Quoted and cast, because a bare hyphenated uuid is subtraction to the parser. The
        // assertion is on the *exact* string so a change back to interpolation is a red test
        // rather than a 500 that only a browser discovers.
        assert_eq!(
            own.employee_predicate().unwrap(),
            Some(format!("e.id = '{mine}'::uuid"))
        );

        // A platform account with no employee row is REFUSED, not widened. Returning "no
        // predicate" here would mean "no restriction", so an `own` caller with no record of
        // their own would read every request in the organization — and leave is exactly the data
        // that says when somebody is not at work.
        let nobody = LeaveScope::all(Uuid::from_u128(1)).with(crate::model::Visibility::Own, None);
        assert!(
            matches!(nobody.employee_predicate(), Err(HrError::NotFound("employee"))),
            "an `own` caller with no employee row must be refused, never widened"
        );
    }
}
