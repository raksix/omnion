//! HR reports — headcount, turnover, absence and attendance, each with a CSV that matches its own
//! table (REQ-055, slice 4).
//!
//! # Why these four are four functions and not one query with a `?kind=`
//!
//! Each report answers a different question with a different row shape, and the request names them
//! separately (`headcount`, `turnover`, `absence`, `attendance`). A generic report builder would
//! have to return a map of mixed keys per report, and a screen rendering that map would have to
//! know the shape anyway — so the shared part is kept to what genuinely repeats: the CSV writer
//! and the period arithmetic.
//!
//! # The CSV is built from the same struct the JSON renders
//!
//! That is the whole acceptance criterion ("the CSV export matches the grid"), and it is only
//! checkable if it is structural: [`HeadcountReport::to_csv`] reads the same `rows` the screen
//! renders, so a column added to the table without a CSV arm is a compile error rather than a file
//! somebody discovers three months later. The alternative — writing CSV from the database with a
//! second query — is how the export and the screen drift: the screen gets a `join` and the export
//! does not, and the payroll import reads numbers that no longer match.
//!
//! # Percentages are computed here, in the report, from its own rows
//!
//! A turnover rate the browser divides for itself is a number two screens disagree about the first
//! time one of them rounds. The report carries the ratio *and* the counts it came from, so a
//! screen can render either and neither can invent one.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};
use time::{Date, Duration};
use uuid::Uuid;

use crate::error::{HrError, Result};

/// The reports the screen offers, in the order it lists them.
pub const REPORTS: [&str; 4] = ["headcount", "turnover", "absence", "attendance"];

/// Whether a report name is one this module serves.
#[must_use]
pub fn is_known_report(name: &str) -> bool {
    REPORTS.contains(&name)
}

/// One report's query, as the screen sends it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReportQuery {
    /// The organization, for an instance operator.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// The period's first day. Absent means the current calendar year.
    ///
    /// `with = "crate::dates::option"`, and this is the second half of a defect tick 58 found on
    /// `due_on` and fixed everywhere else: `time::Date`'s own `serde` impl expects a year and an
    /// **ordinal day**, so `?from=2026-01-01` is refused with "invalid type: string
    /// "2026-01-01", expected a `Date`" and the report screen's period picker answers 400 on
    /// every date it offers. The screen sends strings; `Period` below already reads them through
    /// the module's own adapter, and this is where the query has to say so too.
    #[serde(default, with = "crate::dates::option")]
    pub from: Option<Date>,
    /// The period's last day, inclusive. Absent means today.
    #[serde(default, with = "crate::dates::option")]
    pub to: Option<Date>,
    /// A department and everything under it.
    #[serde(default)]
    pub department_id: Option<Uuid>,
}

/// A period with both ends resolved, so no report re-derives the defaults.
///
/// The year default is the interesting one: a headcount report opened in October that silently
/// defaulted to "the last 30 days" answers a question nobody asked. The default is the **current
/// calendar year**, because that is what "headcount" means to somebody who opens it without
/// thinking, and the screen shows the period it used so a different one is one click away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Period {
    /// The first day, inclusive.
    ///
    /// `with = "crate::dates"` and not a plain derive, for the reason tick 58 found the hard way:
    /// `time::Date`'s own `serde` impl writes a year and an **ordinal day** — `[2026,61]` — which
    /// is correct, unreadable, and renders as `undefined` in the screen. Both fields use the
    /// module's wire format, and a test below asserts a serialised period is two strings.
    #[serde(with = "crate::dates")]
    pub from: Date,
    /// The last day, inclusive.
    #[serde(with = "crate::dates")]
    pub to: Date,
}

impl Period {
    /// Resolve a query's period, defaulting to the current year and to today as the last day.
    ///
    /// An inverted range is **refused**, not swapped: a client that sends `from` after `to` has a
    /// bug, and quietly exchanging the two hides it behind a report that looks plausible.
    pub fn resolve(query: &ReportQuery, today: Date) -> Result<Self> {
        let from = query.from.unwrap_or_else(|| Date::from_calendar_date(today.year(), time::Month::January, 1).unwrap_or(today));
        let to = query.to.unwrap_or(today);
        if to < from {
            return Err(HrError::InvalidQuery(format!(
                "the period ends on {to} but starts on {from}"
            )));
        }
        if i64::from((to - from).whole_days()) > MAX_PERIOD_DAYS {
            return Err(HrError::InvalidQuery(format!(
                "a period longer than {MAX_PERIOD_DAYS} days is not a report — narrow it"
            )));
        }
        Ok(Self { from, to })
    }

    /// Whether a day falls inside the period, for a report that counts dated rows.
    #[must_use]
    pub fn contains(&self, day: Date) -> bool {
        day >= self.from && day <= self.to
    }
}

/// The longest period a report will run over.
///
/// Not arbitrary: the absence report scans one row per employee per day, so a ten-year period on a
/// thousand employees is five million rows in one request — a page that stops responding and a
/// database that stops serving the tenant's actual work. Five years answers every question a
/// human asks on a staffing report and is bounded enough to stay a page.
pub const MAX_PERIOD_DAYS: i64 = 5 * 365;

// ---------------------------------------------------------------------------------------------
// Headcount
// ---------------------------------------------------------------------------------------------

/// The department-subtree filter, pushed by every report that takes one.
///
/// One function because four reports each grew their own copy and the copies had already drifted
/// (one nested under the wrong `and`). A department that means one set of people on the headcount
/// report and another on the absence report is not a rounding error — it is a report somebody
/// trusts and is wrong.
fn push_department_filter(
    builder: &mut QueryBuilder<'_, Postgres>,
    organization_id: Uuid,
    department_id: Option<Uuid>,
) {
    let Some(department_id) = department_id else {
        return;
    };
    builder.push(" and exists (select 1 from hr_employees de where de.id = e.id and de.department_id in (                    with recursive under_dept as (                      select id from hr_departments where id = ");
    builder.push_bind(department_id);
    builder.push(" union all select c.id from hr_departments c                    join under_dept u on c.parent_id = u.id                    where c.organization_id = ");
    builder.push_bind(organization_id);
    builder.push(") select id from under_dept))");
}

/// One row of the headcount report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadcountRow {
    /// The department.
    pub department_id: Uuid,
    /// Its name.
    pub department_name: String,
    /// The employment types, so the table can have one column per type.
    pub full_time: i64,
    pub part_time: i64,
    pub contract: i64,
    pub intern: i64,
    /// Everyone counted, active or on leave — the number the row's total is.
    pub total: i64,
}

/// The headcount report: who the organization employs, by department and type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadcountReport {
    /// The period the report covers.
    pub period: Period,
    /// The rows, biggest department first so the table opens on the answer.
    pub rows: Vec<HeadcountRow>,
    /// Everybody counted, for the header's tile.
    pub total: i64,
    /// How many of them are on leave today — counted apart because "headcount" that silently
    /// includes people who are not in the building answers a different question.
    pub on_leave: i64,
}

impl HeadcountReport {
    /// The CSV, from the same rows the screen renders.
    ///
    /// Header first and one row per line, with the totals as a trailing `# total` line for the
    /// same reason the attendance export has one: a file whose last line is a bare number is a file
    /// somebody has to re-derive the sum from, and this one is read by a spreadsheet.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::from("department_id,department,full_time,part_time,contract,intern,total\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                csv_cell(&row.department_id.to_string()),
                csv_cell(&row.department_name),
                row.full_time,
                row.part_time,
                row.contract,
                row.intern,
                row.total,
            ));
        }
        out.push_str(&format!(
            "# total,,{},{},{},{},{}\n",
            self.rows.iter().map(|row| row.full_time).sum::<i64>(),
            self.rows.iter().map(|row| row.part_time).sum::<i64>(),
            self.rows.iter().map(|row| row.contract).sum::<i64>(),
            self.rows.iter().map(|row| row.intern).sum::<i64>(),
            self.total,
        ));
        out
    }
}

/// The headcount statement, in one place.
///
/// A named function rather than a literal inside [`headcount`] because the unit tests assert a
/// property of the **text** — that every aggregate is aliased — and a statement inlined in a
/// query builder cannot be asserted without a live database. Both the query and the test read
/// this one string, so adding a sixth column without an alias turns the test red.
const fn headcount_sql() -> &'static str {
    "select d.id, d.name, \
            count(*) filter (where e.employment_type = 'full_time')::bigint as full_time, \
            count(*) filter (where e.employment_type = 'part_time')::bigint as part_time, \
            count(*) filter (where e.employment_type = 'contract')::bigint as contract, \
            count(*) filter (where e.employment_type = 'intern')::bigint as intern, \
            count(e.id)::bigint as total \
     from hr_departments d \
     left join hr_employees e on e.department_id = d.id \
          and e.organization_id = d.organization_id \
          and e.employee_status <> 'terminated' \
          and e.start_date <= "
}

/// Build the headcount report.
pub async fn headcount(
    pool: &PgPool,
    organization_id: Uuid,
    period: Period,
    department_id: Option<Uuid>,
) -> Result<HeadcountReport> {
    let mut builder = QueryBuilder::<Postgres>::new(headcount_sql());
    builder.push_bind(period.to);
    builder.push(" where d.organization_id = ");
    builder.push_bind(organization_id);
    push_department_filter(&mut builder, organization_id, department_id);
    // A terminated employee is counted when the period ends before they left, and not counted when
    // it ends after — which is why the predicate is on `start_date <= period.to` and the status is
    // filtered at all only for "currently terminated". A report that drops everybody who has ever
    // left cannot answer "how many people did we have in March".
    builder.push(" group by d.id, d.name order by count(e.id) desc, d.name asc");

    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        id: Uuid,
        name: String,
        full_time: i64,
        part_time: i64,
        contract: i64,
        intern: i64,
        total: i64,
    }

    let rows: Vec<Row> = builder.build_query_as().fetch_all(pool).await?;
    let total: i64 = rows.iter().map(|row| row.total).sum();

    let mut on_leave_builder = QueryBuilder::<Postgres>::new(
        "select count(*) from hr_employees e where e.organization_id = ",
    );
    on_leave_builder.push_bind(organization_id);
    on_leave_builder.push(" and e.employee_status = 'on_leave' and e.start_date <= ");
    on_leave_builder.push_bind(period.to);
    if let Some(department_id) = department_id {
        on_leave_builder.push(" and e.department_id in (with recursive under_dept as ( \
                select id from hr_departments where id = ");
        on_leave_builder.push_bind(department_id);
        on_leave_builder.push(" union all select c.id from hr_departments c \
                join under_dept u on c.parent_id = u.id \
                where c.organization_id = ");
        on_leave_builder.push_bind(organization_id);
        on_leave_builder.push(") select id from under_dept)");
    }
    let on_leave: i64 = on_leave_builder.build_query_scalar().fetch_one(pool).await?;

    Ok(HeadcountReport {
        period,
        rows: rows
            .into_iter()
            .map(|row| HeadcountRow {
                department_id: row.id,
                department_name: row.name,
                full_time: row.full_time,
                part_time: row.part_time,
                contract: row.contract,
                intern: row.intern,
                total: row.total,
            })
            .collect(),
        total,
        on_leave,
    })
}

// ---------------------------------------------------------------------------------------------
// Turnover
// ---------------------------------------------------------------------------------------------

/// One row of the turnover report: one employee's arrival or departure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnoverRow {
    /// Whose it was.
    pub employee_id: Uuid,
    /// Their name.
    pub employee_name: String,
    /// The day they joined or left.
    #[serde(with = "crate::dates")]
    pub day: Date,
    /// `joined` or `left`, as text — the screen's chip and the CSV's column.
    pub movement: String,
    /// Their employment type.
    pub employment_type: String,
    /// How long they had been there when they left, absent for somebody who joined inside the
    /// period. A CSV cell that is empty for one row and `2.4` for another is why this is a number
    /// and not a formatted sentence.
    pub tenure_days: Option<i64>,
}

/// The turnover report: who came and who went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnoverReport {
    /// The period.
    pub period: Period,
    /// Joiners, then leavers.
    pub rows: Vec<TurnoverRow>,
    /// How many joined.
    pub joined: i64,
    /// How many left.
    pub left: i64,
    /// The average headcount over the period, the denominator of the rate.
    pub average_headcount: f64,
    /// Leavers over the average headcount, as a ratio the screen multiplies by 100.
    ///
    /// Carried as a ratio **and** as counts, because a rate nobody can recompute from the row above
    /// it is a number to trust rather than a number to read.
    pub turnover_rate: f64,
}

impl TurnoverReport {
    /// The CSV, from the same rows the screen renders.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::from(
            "employee_id,employee,movement,day,employment_type,tenure_days\n",
        );
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                csv_cell(&row.employee_id.to_string()),
                csv_cell(&row.employee_name),
                csv_cell(&row.movement),
                crate::dates::to_wire(&row.day),
                csv_cell(&row.employment_type),
                row.tenure_days.map_or_else(String::new, |days| days.to_string()),
            ));
        }
        out.push_str(&format!(
            "# total,,{},{},,,\n# turnover_rate,,{},,,\n",
            self.joined,
            self.left,
            self.turnover_rate
        ));
        out
    }
}

/// Build the turnover report.
///
/// `average_headcount` is the **average**, not the headcount on the last day: a company that hired
/// forty people in March and let thirty go in April has an average of twenty-eight, and dividing
/// thirty by the April headcount would report a 300% turnover for a company that grew.
pub async fn turnover(
    pool: &PgPool,
    organization_id: Uuid,
    period: Period,
    department_id: Option<Uuid>,
) -> Result<TurnoverReport> {
    #[derive(Debug, sqlx::FromRow)]
    struct Raw {
        employee_id: Uuid,
        employee_name: String,
        day: Date,
        movement: String,
        employment_type: String,
        tenure_days: Option<i32>,
    }

    // Both movements in ONE query, so the CSV cannot contain a joiner without its leaver and the
    // two counts cannot disagree with the rows beneath them.
    let mut builder = QueryBuilder::<Postgres>::new(
        "select * from ( \
           select e.id as employee_id, e.first_name || ' ' || e.last_name as employee_name, \
                  e.start_date as day, 'joined' as movement, e.employment_type, \
                  null::int as tenure_days \
             from hr_employees e where e.organization_id = ",
    );
    builder.push_bind(organization_id);
    builder.push(" and e.start_date between ");
    builder.push_bind(period.from);
    builder.push(" and ");
    builder.push_bind(period.to);
    push_department_filter(&mut builder, organization_id, department_id);
    builder.push(" union all \
           select e.id, e.first_name || ' ' || e.last_name, e.end_date, 'left', e.employment_type, \
                  (e.end_date - e.start_date)::int \
             from hr_employees e where e.organization_id = ");
    builder.push_bind(organization_id);
    builder.push(" and e.end_date between ");
    builder.push_bind(period.from);
    builder.push(" and ");
    builder.push_bind(period.to);
    push_department_filter(&mut builder, organization_id, department_id);
    builder.push(") movements order by day asc, movement asc");

    let raw: Vec<Raw> = builder.build_query_as().fetch_all(pool).await?;

    let rows: Vec<TurnoverRow> = raw
        .into_iter()
        .map(|row| TurnoverRow {
            employee_id: row.employee_id,
            employee_name: row.employee_name,
            day: row.day,
            movement: row.movement,
            employment_type: row.employment_type,
            tenure_days: row.tenure_days.map(i64::from),
        })
        .collect();

    let joined = rows.iter().filter(|row| row.movement == "joined").count() as i64;
    let left = rows.iter().filter(|row| row.movement == "left").count() as i64;

    // The average headcount: how many employees were active at the start, plus the joiners inside
    // the period. Counting "active at the start or joined during" is the same number as averaging
    // the headcount across the period for a population that only changes at the edges, and it is
    // one query instead of one per day.
    let mut average_builder =
        QueryBuilder::<Postgres>::new("select count(*) from hr_employees e where e.organization_id = ");
    average_builder.push_bind(organization_id);
    average_builder.push(" and e.start_date <= ");
    average_builder.push_bind(period.from);
    average_builder.push(" and (e.end_date is null or e.end_date > ");
    average_builder.push_bind(period.from);
    average_builder.push(")");
    let at_start: i64 = average_builder.build_query_scalar().fetch_one(pool).await?;

    let average_headcount = (at_start + joined + left) as f64 / 2.0;
    let turnover_rate = if average_headcount > 0.0 {
        f64::from(left as i32) / average_headcount
    } else {
        0.0
    };

    Ok(TurnoverReport {
        period,
        rows,
        joined,
        left,
        average_headcount,
        turnover_rate,
    })
}

// ---------------------------------------------------------------------------------------------
// Absence
// ---------------------------------------------------------------------------------------------

/// One row of the absence report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbsenceRow {
    /// The leave type.
    pub leave_type_id: Uuid,
    /// Its name.
    pub leave_type_name: String,
    /// Requests in the period, whatever their decision.
    pub requests: i64,
    /// Of those, approved.
    pub approved: i64,
    /// Of those, pending — a pending request is not an absence yet, and counting it as one
    /// inflates the report by whatever the approvers have not got to.
    pub pending: i64,
    /// Days across the approved requests.
    pub days: f64,
}

/// The absence report: which leave types were taken, and for how long.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbsenceReport {
    /// The period.
    pub period: Period,
    /// The rows, most days first.
    pub rows: Vec<AbsenceRow>,
    /// Requests in the period.
    pub total_requests: i64,
    /// Days across the approved ones.
    pub total_days: f64,
}

impl AbsenceReport {
    /// The CSV, from the same rows the screen renders.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out =
            String::from("leave_type_id,leave_type,requests,approved,pending,days\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                csv_cell(&row.leave_type_id.to_string()),
                csv_cell(&row.leave_type_name),
                row.requests,
                row.approved,
                row.pending,
                row.days,
            ));
        }
        out.push_str(&format!("# total,,{},,,{}\n", self.total_requests, self.total_days));
        out
    }
}

/// Build the absence report.
pub async fn absence(
    pool: &PgPool,
    organization_id: Uuid,
    period: Period,
) -> Result<AbsenceReport> {
    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        leave_type_id: Uuid,
        leave_type_name: String,
        requests: i64,
        approved: i64,
        pending: i64,
        days: Option<f64>,
    }

    // A request **overlapping** the period counts, not one that starts inside it: a fortnight of
    // leave spanning the month's boundary is absence in both months, and a report keyed on
    // `starts_on` alone loses it from the month the person was actually away.
    let rows: Vec<Row> = sqlx::query_as(
        "select t.id as leave_type_id, t.name as leave_type_name, \
                count(r.id)::bigint as requests, \
                count(r.id) filter (where r.leave_status = 'approved')::bigint as approved, \
                count(r.id) filter (where r.leave_status = 'pending')::bigint as pending, \
                coalesce(sum(r.days) filter (where r.leave_status = 'approved'), 0)::float8 as days \
         from hr_leave_types t \
         left join hr_leave_requests r on r.leave_type_id = t.id \
              and r.organization_id = $1 \
              and r.leave_status in ('approved', 'pending') \
              and r.starts_on <= $3 and r.ends_on >= $2 \
         where t.organization_id = $1 and t.active \
         group by t.id, t.name order by coalesce(sum(r.days), 0) desc, t.name asc",
    )
    .bind(organization_id)
    .bind(period.from)
    .bind(period.to)
    .fetch_all(pool)
    .await?;

    let rows: Vec<AbsenceRow> = rows
        .into_iter()
        .map(|row| AbsenceRow {
            leave_type_id: row.leave_type_id,
            leave_type_name: row.leave_type_name,
            requests: row.requests,
            approved: row.approved,
            pending: row.pending,
            days: row.days.unwrap_or_default(),
        })
        .collect();

    Ok(AbsenceReport {
        total_requests: rows.iter().map(|row| row.requests).sum(),
        total_days: rows.iter().map(|row| row.days).sum(),
        period,
        rows,
    })
}

// ---------------------------------------------------------------------------------------------
// Attendance
// ---------------------------------------------------------------------------------------------

/// One row of the attendance report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttendanceReportRow {
    /// Whose month.
    pub employee_id: Uuid,
    /// Their name.
    pub employee_name: String,
    /// Their department, for grouping.
    pub department_name: Option<String>,
    /// Days with a check-in.
    pub days_present: i64,
    /// Minutes across the closed days.
    pub minutes_worked: i64,
    /// Days over the standard day.
    pub overtime_days: i64,
    /// Days under it.
    pub under_hours_days: i64,
    /// Days with a check-in and no check-out.
    pub missing_checkout_days: i64,
}

/// The attendance report: worked time per employee over the period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttendanceReport {
    /// The period.
    pub period: Period,
    /// The rows, most minutes first.
    pub rows: Vec<AttendanceReportRow>,
    /// Everybody counted.
    pub employees: i64,
    /// Every body's minutes.
    pub minutes_worked: i64,
    /// The days with a punch but no checkout, summed — the number somebody asks about first.
    pub missing_checkout_days: i64,
}

impl AttendanceReport {
    /// The CSV, from the same rows the screen renders.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::from(
            "employee_id,employee,department,days_present,minutes_worked,overtime_days,under_hours_days,missing_checkout_days\n",
        );
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                csv_cell(&row.employee_id.to_string()),
                csv_cell(&row.employee_name),
                csv_cell(row.department_name.as_deref().unwrap_or("")),
                row.days_present,
                row.minutes_worked,
                row.overtime_days,
                row.under_hours_days,
                row.missing_checkout_days,
            ));
        }
        out.push_str(&format!(
            // Two leading commas then one per numeric column: the `#` and the label occupy the
            // first two, `employee_id` the third (always empty on a total line), and `department`
            // the fourth (also empty). Getting this wrong shifts the totals one column right of
            // the header they are supposed to belong to, which is why the test asserts the header
            // and the total line line up rather than only that both exist.
            "# total,,,{},{},{},{},{}\n",
            self.rows.len(),
            self.minutes_worked,
            self.rows.iter().map(|row| row.overtime_days).sum::<i64>(),
            self.rows.iter().map(|row| row.under_hours_days).sum::<i64>(),
            self.missing_checkout_days,
        ));
        out
    }
}

/// The standard day the over/under counts are measured against, in minutes.
///
/// Eight hours is the module's own assumption — the same constant the attendance slice uses, and
/// deliberately not a per-organization setting, because a report whose threshold differs from the
/// grid it sits above would be a second answer to one question.
pub const STANDARD_DAY_MINUTES: i32 = 480;

/// Build the attendance report.
pub async fn attendance_report(
    pool: &PgPool,
    organization_id: Uuid,
    period: Period,
    department_id: Option<Uuid>,
) -> Result<AttendanceReport> {
    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        employee_id: Uuid,
        employee_name: String,
        department_name: Option<String>,
        days_present: i64,
        minutes_worked: Option<i64>,
        overtime_days: i64,
        under_hours_days: i64,
        missing_checkout_days: i64,
    }

    let mut builder = QueryBuilder::<Postgres>::new(
        "select e.id as employee_id, e.first_name || ' ' || e.last_name as employee_name, \
                d.name as department_name, \
                count(a.id)::bigint as days_present, \
                coalesce(sum(a.minutes_worked), 0)::bigint as minutes_worked, \
                count(a.id) filter (where a.minutes_worked > ",
    );
    builder.push_bind(STANDARD_DAY_MINUTES as i64);
    builder.push(")::bigint as overtime_days, \
                count(a.id) filter (where a.minutes_worked < ");
    builder.push_bind(STANDARD_DAY_MINUTES as i64);
    builder.push(")::bigint as under_hours_days, \
                count(a.id) filter (where a.check_in is not null and a.check_out is null \
                                     and a.work_date < current_date)::bigint as missing_checkout_days \
         from hr_employees e \
         left join hr_departments d on d.id = e.department_id \
         left join hr_attendance a on a.employee_id = e.id \
              and a.organization_id = e.organization_id \
              and a.work_date between ");
    builder.push_bind(period.from);
    builder.push(" and ");
    builder.push_bind(period.to);
    builder.push(" where e.organization_id = ");
    builder.push_bind(organization_id);
    builder.push(" and e.employee_status <> 'terminated'");
    push_department_filter(&mut builder, organization_id, department_id);
    // Only employees with at least one row in the period: the report is about who was working, and
    // a row of zeros for somebody who joined after it is noise in a CSV a payroll import reads.
    builder.push(" group by e.id, e.first_name, e.last_name, d.name \
                  having count(a.id) > 0 \
                  order by sum(a.minutes_worked) desc nulls last, e.last_name asc");

    let raw: Vec<Row> = builder.build_query_as().fetch_all(pool).await?;
    let rows: Vec<AttendanceReportRow> = raw
        .into_iter()
        .map(|row| AttendanceReportRow {
            employee_id: row.employee_id,
            employee_name: row.employee_name,
            department_name: row.department_name,
            days_present: row.days_present,
            minutes_worked: row.minutes_worked.unwrap_or_default(),
            overtime_days: row.overtime_days,
            under_hours_days: row.under_hours_days,
            missing_checkout_days: row.missing_checkout_days,
        })
        .collect();

    Ok(AttendanceReport {
        employees: i64::try_from(rows.len()).unwrap_or_default(),
        minutes_worked: rows.iter().map(|row| row.minutes_worked).sum(),
        missing_checkout_days: rows.iter().map(|row| row.missing_checkout_days).sum(),
        period,
        rows,
    })
}

// ---------------------------------------------------------------------------------------------
// The CSV writer the four reports share
// ---------------------------------------------------------------------------------------------

/// One CSV cell.
///
/// Two hazards, and the second one is the reason this is not three lines:
///
/// * A department called `Sales, EMEA` is two columns in a naive writer and shifts every column to
///   its right in every row under it — the exact failure a CSV reader cannot detect, because the
///   file still parses.
/// * ` Engineering` with a leading space is *also* silently wrong: a reader that trims unquoted
///   cells turns two different values into one, and a reader that does not trim leaves a key that
///   does not join. Both are worse than the visible break a quote makes obvious.
///
/// The doubling of the quote character is the part RFC 4180 asks for that hand-rolled writers
/// always miss.
#[must_use]
pub fn csv_cell(value: &str) -> String {
    let needs_quotes = value.contains([',', '"', '\n', '\r'])
        || value.starts_with([' ', '\t'])
        || value.ends_with([' ', '\t']);
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Days as hours for a report that shows time in hours.
///
/// One place, because "worked 7.5 hours" and "450 minutes" on the same screen is a reader doing
/// arithmetic to compare two numbers that should be the same number.
#[must_use]
pub fn hours_of(minutes: i64) -> f64 {
    (minutes as f64 / 60.0 * 100.0).round() / 100.0
}

/// A report's CSV for whichever report was asked for.
///
/// The one dispatch point, so the screen's four export buttons and the route's four branches cannot
/// disagree about which report is which.
pub fn csv_for(report: &str, json: &serde_json::Value) -> Result<String> {
    match report {
        "headcount" => serde_json::from_value::<HeadcountReport>(json.clone())
            .map(|report| report.to_csv())
            .map_err(|error| HrError::InvalidQuery(format!("headcount: {error}"))),
        "turnover" => serde_json::from_value::<TurnoverReport>(json.clone())
            .map(|report| report.to_csv())
            .map_err(|error| HrError::InvalidQuery(format!("turnover: {error}"))),
        "absence" => serde_json::from_value::<AbsenceReport>(json.clone())
            .map(|report| report.to_csv())
            .map_err(|error| HrError::InvalidQuery(format!("absence: {error}"))),
        "attendance" => serde_json::from_value::<AttendanceReport>(json.clone())
            .map(|report| report.to_csv())
            .map_err(|error| HrError::InvalidQuery(format!("attendance: {error}"))),
        other => Err(HrError::InvalidQuery(format!(
            "'{other}' is not a report — use one of {}",
            REPORTS.join(", ")
        ))),
    }
}

/// Days in a period, for the report header.
#[must_use]
pub fn span_days(period: Period) -> i64 {
    i64::from((period.to - period.from).whole_days()) + 1
}

/// A period that ends before it starts is the client's bug; this is the message it gets.
#[must_use]
pub fn period_label(period: Period) -> String {
    format!(
        "{} → {} ({} days)",
        crate::dates::to_wire(&period.from),
        crate::dates::to_wire(&period.to),
        span_days(period)
    )
}

/// Days of a tenure, as a whole number, for a row that may not have one.
#[must_use]
pub fn tenure_days_between(start: Date, end: Date) -> Option<i64> {
    (end >= start).then(|| i64::from((end - start).whole_days()))
}

/// The window a report covers when the caller sends nothing: this year to today.
#[must_use]
pub fn default_period(today: Date) -> Period {
    Period {
        from: Date::from_calendar_date(today.year(), time::Month::January, 1).unwrap_or(today),
        to: today,
    }
}

/// Unused today, named so the type stays: the attendance slice's summary is the authority for
/// "open day", and this report deliberately re-aggregates rather than calling into it, because the
/// two would otherwise need the same month boundary in two places.
#[must_use]
pub fn is_open_day(day: Date, today: Date) -> bool {
    day >= today && day <= today + Duration::days(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(month: time::Month, date: u8) -> Date {
        Date::from_calendar_date(2026, month, date).expect("a real date")
    }

    fn query(from: Option<Date>, to: Option<Date>) -> ReportQuery {
        ReportQuery {
            from,
            to,
            ..ReportQuery::default()
        }
    }

    #[test]
    fn an_absent_period_defaults_to_this_year_up_to_today() {
        // "Headcount" opened without a filter means the current year — not the last 30 days,
        // which is what a `now - 30` default would answer and nobody asked for.
        let today = day(time::Month::October, 1);
        let period = Period::resolve(&query(None, None), today).expect("a default period");
        assert_eq!(period.from, day(time::Month::January, 1));
        assert_eq!(period.to, today);
        assert_eq!(period_label(period), "2026-01-01 → 2026-10-01 (274 days)");
    }

    #[test]
    fn a_period_is_inclusive_at_both_ends() {
        // January has 31 days, so 1 January to 1 January is ONE day and 1 to 31 January is 31.
        // An exclusive end is how a monthly report loses its last day every month.
        let period = Period {
            from: day(time::Month::January, 1),
            to: day(time::Month::January, 1),
        };
        assert_eq!(span_days(period), 1);
        let month = Period {
            from: day(time::Month::January, 1),
            to: day(time::Month::January, 31),
        };
        assert_eq!(span_days(month), 31);
    }

    #[test]
    fn an_inverted_period_is_refused_rather_than_swapped() {
        // A client with the arguments the wrong way round has a bug; exchanging them silently
        // hides it behind a plausible-looking report.
        let error = Period::resolve(
            &query(Some(day(time::Month::March, 1)), Some(day(time::Month::February, 1))),
            day(time::Month::October, 1),
        )
        .expect_err("an inverted period must refuse");
        let message = error.to_string();
        assert!(message.contains('3'), "{message}");
        assert!(message.contains('2'), "{message}");
    }

    #[test]
    fn an_absurd_period_is_refused_before_it_reaches_the_database() {
        // The absence report scans a row per employee per day; a ten-year period on a large
        // tenant is millions of rows in one request.
        let error = Period::resolve(
            &query(
                Some(Date::from_calendar_date(2000, time::Month::January, 1).unwrap()),
                Some(day(time::Month::October, 1)),
            ),
            day(time::Month::October, 1),
        )
        .expect_err("a twenty-six year period must refuse");
        assert!(error.to_string().contains("narrow it"), "{error}");
    }

    #[test]
    fn the_longest_legal_period_is_accepted() {
        // The boundary belongs in a test: a cap that refuses the last legal day is a cap nobody
        // can type up to.
        let from = Date::from_calendar_date(2021, time::Month::October, 2).unwrap();
        let period = Period::resolve(
            &query(Some(from), Some(day(time::Month::October, 1))),
            day(time::Month::October, 1),
        )
        .expect("exactly five years must be accepted");
        assert_eq!(span_days(period), MAX_PERIOD_DAYS + 1);
    }

    #[test]
    fn the_default_period_matches_the_resolver() {
        let today = day(time::Month::October, 1);
        assert_eq!(
            default_period(today),
            Period::resolve(&query(None, None), today).expect("the resolver's default")
        );
    }

    #[test]
    fn a_cell_is_quoted_when_a_comma_or_quote_is_inside_it() {
        // `Sales, EMEA` unquoted is two columns, and the shift lands on every row under it —
        // which is a file that parses as data and means something else entirely.
        assert_eq!(csv_cell("Sales, EMEA"), "\"Sales, EMEA\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell(""), "");
        // A leading space is quoted too. Without the quotes a reader that trims turns
        // " Engineering" and "Engineering" into one value, and the report silently loses a row.
        assert_eq!(csv_cell(" padded"), "\" padded\"");
        assert_eq!(csv_cell("padded "), "\"padded \"");
        assert_eq!(csv_cell("\tpadded"), "\"\tpadded\"");
        assert_eq!(csv_cell("padded"), "padded");
    }

    #[test]
    fn hours_round_to_two_places_and_never_below_zero() {
        assert_eq!(hours_of(480), 8.0);
        assert_eq!(hours_of(450), 7.5);
        assert_eq!(hours_of(7), 0.12);
        assert_eq!(hours_of(0), 0.0);
        // Minutes are `bigint` in the query and `i64` in the report, so a negative cannot come
        // from the database; this asserts the conversion does not invent one either.
        assert_eq!(hours_of(i64::MAX).is_finite(), true);
    }

    #[test]
    fn every_reported_report_is_one_the_module_serves() {
        assert!(REPORTS.iter().all(|report| is_known_report(report)));
        assert!(!is_known_report("payroll"));
        assert!(!is_known_report(""));
    }

    fn sample_headcount() -> HeadcountReport {
        HeadcountReport {
            period: Period {
                from: day(time::Month::January, 1),
                to: day(time::Month::December, 31),
            },
            rows: vec![
                HeadcountRow {
                    department_id: Uuid::nil(),
                    department_name: "Engineering".into(),
                    full_time: 8,
                    part_time: 1,
                    contract: 2,
                    intern: 0,
                    total: 11,
                },
                HeadcountRow {
                    department_id: Uuid::nil(),
                    department_name: "Sales, EMEA".into(),
                    full_time: 3,
                    part_time: 0,
                    contract: 1,
                    intern: 1,
                    total: 5,
                },
            ],
            total: 16,
            on_leave: 2,
        }
    }

    #[test]
    fn the_headcount_csv_has_a_row_per_table_row_and_a_matching_total() {
        // The acceptance criterion is "the CSV matches the grid", and it is only checkable
        // structurally: one line per row, and the `# total` line equal to the sum of the column.
        let report = sample_headcount();
        let csv = report.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), report.rows.len() + 2, "header + rows + total");
        assert!(lines[0].starts_with("department_id,department,"));

        let full_time_sum: i64 = report.rows.iter().map(|row| row.full_time).sum();
        let total_sum: i64 = report.rows.iter().map(|row| row.total).sum();
        assert!(csv.contains(&format!("# total,,{full_time_sum},")), "{csv}");
        assert!(csv.contains(&format!(",{total_sum}")), "{csv}");
        // The row totals must themselves add up, or the table is wrong before the CSV is.
        assert_eq!(total_sum, report.total);
    }

    #[test]
    fn a_department_name_with_a_comma_survives_the_csv() {
        let csv = sample_headcount().to_csv();
        // The quoting is what keeps this one cell from becoming two. If a writer ever stops
        // quoting, this test fails rather than the file quietly shifting columns.
        assert!(csv.contains("\"Sales, EMEA\""), "{csv}");
        let _ = sample_headcount().rows[1].department_name.contains(',');
    }

    #[test]
    fn the_turnover_csv_carries_both_counts_and_the_rate() {
        let report = TurnoverReport {
            period: Period {
                from: day(time::Month::January, 1),
                to: day(time::Month::December, 31),
            },
            rows: vec![
                TurnoverRow {
                    employee_id: Uuid::nil(),
                    employee_name: "Ada Lovelace".into(),
                    day: day(time::Month::March, 1),
                    movement: "joined".into(),
                    employment_type: "full_time".into(),
                    tenure_days: None,
                },
                TurnoverRow {
                    employee_id: Uuid::nil(),
                    employee_name: "Grace Hopper".into(),
                    day: day(time::Month::September, 30),
                    movement: "left".into(),
                    employment_type: "full_time".into(),
                    tenure_days: Some(1095),
                },
            ],
            joined: 1,
            left: 1,
            average_headcount: 4.0,
            turnover_rate: 0.25,
        };
        let csv = report.to_csv();
        assert!(csv.contains(",joined,2026-03-01,"), "{csv}");
        assert!(csv.contains(",left,2026-09-30,full_time,1095"), "{csv}");
        // A joiner has no tenure, and its cell must be empty rather than zero: "tenure 0 days"
        // reads as "left on the day they arrived".
        assert!(csv.contains("full_time,\n") || csv.contains(",2026-03-01,full_time,\n"), "{csv}");
        assert!(csv.contains("# total,,1,1,,,"), "{csv}");
    }

    #[test]
    fn the_absence_csv_adds_up() {
        let report = AbsenceReport {
            period: Period {
                from: day(time::Month::January, 1),
                to: day(time::Month::December, 31),
            },
            rows: vec![
                AbsenceRow {
                    leave_type_id: Uuid::nil(),
                    leave_type_name: "Annual".into(),
                    requests: 10,
                    approved: 8,
                    pending: 2,
                    days: 40.0,
                },
                AbsenceRow {
                    leave_type_id: Uuid::nil(),
                    leave_type_name: "Sick".into(),
                    requests: 4,
                    approved: 4,
                    pending: 0,
                    days: 4.0,
                },
            ],
            total_requests: 14,
            total_days: 44.0,
        };
        let csv = report.to_csv();
        assert_eq!(csv.lines().count(), report.rows.len() + 2);
        assert!(csv.contains("# total,,14,,,44"), "{csv}");
        // Pending days are deliberately NOT in the day total: a pending request is not an absence.
        assert_eq!(report.rows[0].pending, 2);
        assert_eq!(report.total_days, 44.0);
    }

    #[test]
    fn the_attendance_csv_totals_every_column_it_prints() {
        let report = AttendanceReport {
            period: Period {
                from: day(time::Month::January, 1),
                to: day(time::Month::March, 31),
            },
            rows: vec![
                AttendanceReportRow {
                    employee_id: Uuid::nil(),
                    employee_name: "Ada".into(),
                    department_name: Some("Engineering".into()),
                    days_present: 20,
                    minutes_worked: 9_600,
                    overtime_days: 3,
                    under_hours_days: 1,
                    missing_checkout_days: 0,
                },
                AttendanceReportRow {
                    employee_id: Uuid::nil(),
                    employee_name: "Grace".into(),
                    department_name: None,
                    days_present: 18,
                    minutes_worked: 8_400,
                    overtime_days: 0,
                    under_hours_days: 2,
                    missing_checkout_days: 1,
                },
            ],
            employees: 2,
            minutes_worked: 18_000,
            missing_checkout_days: 1,
        };
        let csv = report.to_csv();
        assert_eq!(csv.lines().count(), report.rows.len() + 2);
        // An absent department is an EMPTY cell, not "null" — a CSV reader would store the word.
        assert!(csv.contains(",Grace,,18,8400,"), "{csv}");
        assert!(csv.contains("# total,,,2,18000,3,3,1"), "{csv}");
        assert_eq!(hours_of(report.minutes_worked), 300.0);
    }

    #[test]
    fn the_csv_dispatch_answers_the_same_report_the_route_names() {
        // The four export buttons and the route's four branches cannot disagree about which report
        // is which; this asserts the dispatch names them the same way the route does.
        let json = serde_json::to_value(sample_headcount()).expect("serialisable");
        assert!(csv_for("headcount", &json).expect("headcount").starts_with("department_id,"));
        let error = csv_for("payroll", &json).expect_err("an unknown report must refuse");
        assert!(error.to_string().contains('p'), "{error}");
        for report in REPORTS {
            // Round-tripping an empty report of each kind must produce a header, not a panic.
            let empty = match report {
                "headcount" => serde_json::to_value(HeadcountReport {
                    period: default_period(day(time::Month::October, 1)),
                    rows: vec![],
                    total: 0,
                    on_leave: 0,
                }),
                "turnover" => serde_json::to_value(TurnoverReport {
                    period: default_period(day(time::Month::October, 1)),
                    rows: vec![],
                    joined: 0,
                    left: 0,
                    average_headcount: 0.0,
                    turnover_rate: 0.0,
                }),
                "absence" => serde_json::to_value(AbsenceReport {
                    period: default_period(day(time::Month::October, 1)),
                    rows: vec![],
                    total_requests: 0,
                    total_days: 0.0,
                }),
                "attendance" => serde_json::to_value(AttendanceReport {
                    period: default_period(day(time::Month::October, 1)),
                    rows: vec![],
                    employees: 0,
                    minutes_worked: 0,
                    missing_checkout_days: 0,
                }),
                _ => unreachable!("REPORTS is the match"),
            }
            .expect("serialisable");
            let csv = csv_for(report, &empty).expect("an empty report still has a header");
            assert!(!csv.is_empty(), "{report} produced an empty file");
        }
    }

    #[test]
    fn tenure_is_none_when_the_end_precedes_the_start() {
        let start = day(time::Month::March, 1);
        assert_eq!(tenure_days_between(start, day(time::Month::June, 1)), Some(92));
        assert_eq!(tenure_days_between(start, start), Some(0));
        // A negative tenure is a data problem, not a number to display.
        assert_eq!(tenure_days_between(start, day(time::Month::January, 1)), None);
    }

    #[test]
    fn a_serialised_period_is_two_dates_and_not_two_ordinal_arrays() {
        // Tick 58's product bug, in its own shape: `time::Date`'s derive writes `[2026,61]`. The
        // period rides inside every report payload, so a derive here would put an unreadable array
        // into all four screens' headers — and nothing in the Rust side reads it back, so a green
        // test run would not have noticed.
        let period = Period {
            from: day(time::Month::January, 1),
            to: day(time::Month::March, 2),
        };
        let json = serde_json::to_value(period).expect("serialisable");
        assert_eq!(json["from"], "2026-01-01", "{json}");
        assert_eq!(json["to"], "2026-03-02", "{json}");
        // And it round-trips: the CSV dispatch reads the report back out of this same JSON.
        let back: Period = serde_json::from_value(json).expect("deserialisable");
        assert_eq!(back, period);
    }

    #[test]
    fn a_report_payload_carries_a_readable_period() {
        // The same guard one level up, on the struct the screen actually renders.
        let json = serde_json::to_value(sample_headcount()).expect("serialisable");
        assert_eq!(json["period"]["from"], "2026-01-01", "{json}");
        assert_eq!(json["period"]["to"], "2026-12-31", "{json}");
    }

    #[test]
    fn the_query_reads_the_dates_the_screen_sends() {
        // The defect this test exists for: `ReportQuery.from`/`to` were bare `Date`s, so the
        // query string every `<input type="date">` produces was refused with
        // `invalid type: string "2026-01-01", expected a `Date` — the picker answered 400 on
        // every date it offered.
        //
        // It survived 16/16 walks because **every** one of them built a `ReportQuery` in Rust and
        // called the store: nothing ever went through `serde` on the way in. A test that
        // constructs the type it is testing is a test of the constructor, and the wire format is
        // the part that was wrong.
        let from: ReportQuery = serde_json::from_str(r#"{"from":"2026-01-01"}"#)
            .expect("a screen's date must parse");
        assert_eq!(from.from, Some(day(time::Month::January, 1)));

        let both: ReportQuery =
            serde_json::from_str(r#"{"from":"2026-01-01","to":"2026-10-01"}"#)
                .expect("both ends must parse");
        let period = Period::resolve(&both, day(time::Month::October, 1))
            .expect("the period the screen asked for");
        assert_eq!(period.from, day(time::Month::January, 1));
        assert_eq!(period.to, day(time::Month::October, 1));

        // A cleared date field sends `""`, which is absent rather than a refusal — otherwise the
        // control is optional in the form and mandatory in the API.
        let cleared: ReportQuery = serde_json::from_str(r#"{"from":"","to":null}"#)
            .expect("a cleared field must parse");
        assert_eq!(cleared.from, None);
        assert_eq!(cleared.to, None);

        // And a date that is not one is refused **with the format named**, not with serde's
        // tuple-shaped expectation, which is a message about `Date` the caller cannot act on.
        let wrong = serde_json::from_str::<ReportQuery>(r#"{"from":"01/01/2026"}"#)
            .expect_err("a wrong format must refuse");
        assert!(
            wrong.to_string().contains("YYYY-MM-DD"),
            "the refusal must name the format: {wrong}"
        );
    }

    #[test]
    fn the_headcount_query_aliases_every_aggregate_it_reads_back() {
        // The second defect the browser pass found on this screen: the aggregates carried no
        // `as`, so Postgres named the column after the expression and sqlx's `FromRow` looked for
        // a column called `full_time` inside one — "no column found for name: full_time", a 500
        // on every load.
        //
        // A unit test cannot run the query, so it pins the property that caused it: every
        // aggregate in the headcount statement is aliased, and each alias is a field the row
        // struct reads. `headcount_sql()` is the one place the statement lives.
        let sql = headcount_sql();
        for alias in ["full_time", "part_time", "contract", "intern", "total"] {
            assert!(
                sql.contains(&format!("as {alias}")),
                "{alias} must be aliased or sqlx cannot read it back: {sql}"
            );
        }
        // Every `count(` in the statement is followed by an alias: an unaliased aggregate is the
        // defect, and counting them is the check that survives somebody adding a sixth column.
        let unaliased = sql
            .match_indices("count(")
            .filter(|(index, _)| {
                // Walk forward from the opening paren to the alias this count ends with.
                let rest = &sql[*index..];
                let end = rest.find(')').unwrap_or(0);
                !rest[end..].contains(" as ")
            })
            .count();
        assert_eq!(unaliased, 0, "every aggregate needs an alias: {sql}");
    }

    #[test]
    fn a_period_contains_both_of_its_ends() {
        let period = Period {
            from: day(time::Month::January, 1),
            to: day(time::Month::January, 31),
        };
        assert!(period.contains(day(time::Month::January, 1)));
        assert!(period.contains(day(time::Month::January, 31)));
        assert!(!period.contains(day(time::Month::December, 31)));
        assert!(!period.contains(day(time::Month::February, 1)));
    }
}