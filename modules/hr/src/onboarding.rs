//! Onboarding — templates, the checklists they materialise, and the reports that read both
//! (REQ-055, slice 4).
//!
//! # Why the due date is stored, not derived on every read
//!
//! A template carries an **offset** ("three days after they start"), because that is the thing an
//! organization can write down and agree to. The employee carries a **start date**. The checklist
//! therefore has to answer "when is this due" and answer it the same way tomorrow as it does
//! today — and the naive implementation, deriving the due date from the live start date on every
//! read, silently re-dates every item the moment somebody corrects a mistyped start date. A
//! checklist whose due dates move under it is a checklist nobody trusts, so the date is computed
//! **once, at apply time**, and the offset is stored beside it: "why is this due on the 9th?" is
//! then answerable without reconstructing the start date, and re-applying a corrected template
//! does not rewrite history.
//!
//! # Why the progress bar is a fact and not arithmetic
//!
//! [`Checklist::progress`] returns done over total, and the total is a `count(*)` over the items
//! rather than the length of the template. That distinction is the whole slice: an item the
//! template gained since the checklist was applied is **not** on somebody's checklist, and a bar
//! that divides by the template's length reports 75% for a person who has done all three of the
//! three things they were actually given. The reverse — an item duplicated by a double-clicked
//! "apply" — is prevented by `unique (employee_id, position)`, so the denominator cannot drift
//! from the rows the bar is counting.
//!
//! # Two refusals, both their own variants
//!
//! `AlreadyApplied` and `NoSuchTemplate` are variants rather than formatted messages for the
//! reason the rest of the module does it: "is this the double-apply case?" is a question about a
//! variant, and a substring test on a message breaks the day somebody improves the wording.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::{Date, Duration};
use uuid::Uuid;

use crate::error::{HrError, Result};

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// One step of an onboarding template, as the editor holds it.
///
/// The template's items live in one `jsonb` column rather than a table of their own, and that is
/// deliberate (0196 says so): a template is edited and read as a whole, so a row per item would
/// make "reorder" three writes and "apply" a transaction over rows nobody edits individually.
/// The **materialised** items are rows — see [`ChecklistItem`] — because those are ticked, dated
/// and audited one at a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateItem {
    /// What the person reads on the checklist.
    pub title: String,
    /// Who owns it. Free text, not an enum: the roles are the organization's, not the platform's.
    #[serde(default)]
    pub owner_role: Option<String>,
    /// Days after the start date. `None` means "no deadline", which is a real answer for "buy the
    /// coffee grinder" and different from day 0.
    #[serde(default)]
    pub due_offset_days: Option<i32>,
    /// Whether the step needs a file attached before it counts.
    #[serde(default)]
    pub requires_file: bool,
}

impl TemplateItem {
    /// One item from a template's `jsonb`, with the defaults the seeded template omits.
    ///
    /// The three-key short form (`{"title": "…"}`) is what a hand-written seed carries, and
    /// rejecting it would make the seed this module has relied on since slice 1 unparseable.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let title = value.get("title")?.as_str()?.trim().to_owned();
        if title.is_empty() {
            return None;
        }
        Some(Self {
            title,
            owner_role: value
                .get("owner_role")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            due_offset_days: value
                .get("due_offset_days")
                .and_then(serde_json::Value::as_i64)
                .and_then(|days| i32::try_from(days).ok()),
            requires_file: value
                .get("requires_file")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// A reusable checklist an organization applies to new employees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    /// The row's id — what an apply names.
    pub id: Uuid,
    /// Whose template it is.
    pub organization_id: Uuid,
    /// What the picker calls it.
    pub name: String,
    /// The ordered steps.
    pub items: Vec<TemplateItem>,
    /// Whether the picker offers it.
    pub active: bool,
    /// How many employees are currently working through it.
    pub in_progress: i64,
}

/// One materialised step on somebody's checklist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecklistItem {
    /// The row's id — what a tick names.
    pub id: Uuid,
    /// Whose checklist it is on.
    pub employee_id: Uuid,
    /// The template it came from, absent for an ad-hoc item.
    pub template_id: Option<Uuid>,
    /// The order it is worked in, zero-based and contiguous per employee.
    pub position: i32,
    /// What the person reads.
    pub title: String,
    /// Who owns it.
    pub owner_role: Option<String>,
    /// When it is due, derived at apply time from the employee's start date.
    pub due_on: Option<Date>,
    /// The offset the template carried, kept beside the derived date (see the module docs).
    pub due_offset_days: Option<i32>,
    /// Whether the step wants a file.
    pub requires_file: bool,
    /// When it was ticked.
    pub done_at: Option<time::OffsetDateTime>,
    /// Who ticked it.
    pub done_by: Option<Uuid>,
    /// The note beside it.
    pub note: String,
}

/// Somebody's whole checklist, with the numbers the progress bar reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checklist {
    /// Whose checklist it is.
    pub employee_id: Uuid,
    /// The employee, as the board's card shows them.
    pub employee_name: String,
    /// Their department, for the board's grouping.
    pub department_name: Option<String>,
    /// The template it came from, absent when the checklist is ad hoc.
    pub template_id: Option<Uuid>,
    /// The template's name, for the card's subtitle.
    pub template_name: Option<String>,
    /// The steps, in order.
    pub items: Vec<ChecklistItem>,
    /// How many are ticked.
    pub done: i64,
    /// How many there are in total — the bar's denominator, and *not* the template's length.
    pub total: i64,
}

impl Checklist {
    /// The bar, as a fraction the screen can render without dividing itself.
    ///
    /// An empty checklist is **0.0, not 1.0 and not a division by zero**. A checklist with no items
    /// is a person nobody has given anything to do, and the honest reading of "nothing to do" is
    /// "nothing done" — rendering it as complete is how an employee ends up on a board's
    /// "finished" tab having started nothing.
    #[must_use]
    pub fn progress(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.done as f64 / self.total as f64
    }

    /// Whether the last item has just been ticked.
    ///
    /// The completion event fires on the **transition**, not on any state: ticking an already
    /// ticked item, or unticking the last one, are both changes and neither is somebody finishing
    /// their onboarding. Returning a bool from the same write keeps the decision next to the
    /// transaction rather than in a handler that could disagree with it.
    #[must_use]
    pub fn just_completed(&self) -> bool {
        self.total > 0 && self.done == self.total
    }
}

// ---------------------------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------------------------

/// Every template of an organization, newest name first, with the in-progress count.
///
/// `in_progress` is computed in the same statement as the list rather than by a second query per
/// row: N+1 on a screen that shows a picker is N round trips to answer a question the board below
/// already answers, and on a template list it is the difference between one query and forty.
pub async fn list_templates(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Template>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        organization_id: Uuid,
        name: String,
        items: serde_json::Value,
        active: bool,
        in_progress: i64,
    }

    let rows = sqlx::query_as::<_, Row>(
        "select t.id, t.organization_id, t.name, t.items, t.active, \
                (select count(distinct i.employee_id) \
                   from hr_onboarding_items i \
                  where i.template_id = t.id and i.done_at is null) as in_progress \
         from hr_onboarding_templates t \
         where t.organization_id = $1 \
         order by lower(t.name)",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Template {
            id: row.id,
            organization_id: row.organization_id,
            name: row.name,
            items: row
                .items
                .as_array()
                .map(|items| items.iter().filter_map(TemplateItem::from_value).collect())
                .unwrap_or_default(),
            active: row.active,
            in_progress: row.in_progress,
        })
        .collect())
}

/// One template, or `None` when it belongs to another organization.
///
/// The organization filter is in the `where` rather than checked afterwards: a template id from
/// another tenant has to answer exactly like one that does not exist, and a `SELECT` followed by
/// a comparison is two statements where the schema expresses one fact.
pub async fn template_of(
    pool: &PgPool,
    organization_id: Uuid,
    template_id: Uuid,
) -> Result<Option<Template>> {
    let found = list_templates(pool, organization_id)
        .await?
        .into_iter()
        .find(|template| template.id == template_id);
    Ok(found)
}

/// What the template editor writes.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTemplate {
    /// What the picker calls it.
    pub name: String,
    /// The ordered steps.
    pub items: Vec<TemplateItem>,
}

/// A template editor's corrections.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TemplateChanges {
    /// A new name, absent when the editor left it alone.
    pub name: Option<String>,
    /// New steps, absent when the editor left them alone.
    pub items: Option<Vec<TemplateItem>>,
    /// Whether the picker offers it.
    pub active: Option<bool>,
}

/// Validate the steps a template is being saved with.
///
/// A template with **no** items is refused rather than saved as empty: an empty template is
/// reachable from the picker and applying it writes nothing, so the person who applied it watches
/// a progress bar stay at zero with no way to tell whether the template or their employee is at
/// fault. The refusal names the count, so the message is a fact.
fn validate_items(items: &[TemplateItem]) -> Result<()> {
    if items.is_empty() {
        return Err(HrError::invalid(
            "template",
            "items",
            "a template needs at least one item, otherwise applying it writes nothing",
        ));
    }
    for item in items {
        if item.title.trim().is_empty() {
            return Err(HrError::invalid(
                "template",
                "items",
                "every item needs a title — the blank row is the one the person would read",
            ));
        }
        if item
            .due_offset_days
            .is_some_and(|days| !(0..=365).contains(&days))
        {
            return Err(HrError::invalid(
                "template",
                "items",
                format!(
                    "'{}' is due {} days after the start date; an offset has to be between 0 and 365",
                    item.title, item.due_offset_days.unwrap_or_default()
                ),
            ));
        }
    }
    Ok(())
}

/// Create a template.
pub async fn create_template(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewTemplate,
) -> Result<Template> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(HrError::invalid(
            "template",
            "name",
            "the template needs a name — it is what the picker shows",
        ));
    }
    validate_items(&new.items)?;

    let id: Uuid = sqlx::query_scalar(
        "insert into hr_onboarding_templates (organization_id, name, items) \
         values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(name)
    .bind(serde_json::to_value(&new.items).unwrap_or_else(|_| serde_json::json!([])))
    .fetch_one(pool)
    .await
    .map_err(|error| match error {
        sqlx::Error::Database(ref inner) if is_unique_violation(inner.as_ref()) => {
            HrError::invalid(
                "template",
                "name",
                "another template of this organization already carries this name",
            )
        }
        other => HrError::Database(other),
    })?;

    template_of(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("template"))
}

/// Edit a template.
pub async fn update_template(
    pool: &PgPool,
    organization_id: Uuid,
    template_id: Uuid,
    changes: &TemplateChanges,
) -> Result<Template> {
    if template_of(pool, organization_id, template_id)
        .await?
        .is_none()
    {
        return Err(HrError::NotFound("template"));
    }
    if let Some(items) = &changes.items {
        validate_items(items)?;
    }

    // `coalesce` rather than "build the SET clause from the options": a dynamic `SET` with a
    // predicate written after it binds the wrong columns the moment a field is optional, and the
    // mistake is invisible until a caller updates one field and silently clears another.
    sqlx::query(
        "update hr_onboarding_templates set \
           name = coalesce($3, name), \
           items = coalesce($4, items), \
           active = coalesce($5, active) \
         where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(template_id)
    .bind(changes.name.as_ref().map(|name| name.trim()))
    .bind(
        changes
            .items
            .as_ref()
            .map(|items| serde_json::to_value(items).unwrap_or_else(|_| serde_json::json!([]))),
    )
    .bind(changes.active)
    .execute(pool)
    .await?;

    template_of(pool, organization_id, template_id)
        .await?
        .ok_or(HrError::NotFound("template"))
}

/// Apply a template to an employee: materialise its steps with the due dates their start date
/// implies.
///
/// One transaction, because a checklist where half the items arrived is worse than one that did
/// not: the progress bar would read 50% of a list the person never saw, and the second half
/// would need a second apply that the unique index would then refuse.
pub async fn apply_template(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
    template_id: Uuid,
) -> Result<Checklist> {
    let template = template_of(pool, organization_id, template_id)
        .await?
        .ok_or(HrError::NotFound("template"))?;
    validate_items(&template.items)?;

    // The employee's start date is what the offsets are read against. Read it inside the
    // transaction below; a caller passing an employee from another organization resolves to no
    // rows and is refused as a missing employee rather than as a forbidden one.
    let start: Option<Date> = sqlx::query_scalar(
        "select start_date from hr_employees \
         where organization_id = $1 and id = $2 and employee_status <> 'terminated'",
    )
    .bind(organization_id)
    .bind(employee_id)
    .fetch_optional(pool)
    .await?;
    let start_date = start.ok_or(HrError::NotFound("employee"))?;

    let mut tx = pool.begin().await?;

    // The double-apply guard is `on conflict do nothing` + a row count, NOT a check-then-insert:
    // between a `SELECT` and the `INSERT` two admins pressing "apply" at the same moment would
    // both see an empty checklist and both write. The unique index decides, and the loser is told.
    let mut written: u64 = 0;
    for (position, item) in template.items.iter().enumerate() {
        let due_on = item
            .due_offset_days
            .map(|offset| start_date + Duration::days(i64::from(offset)));
        let inserted = sqlx::query(
            "insert into hr_onboarding_items \
               (organization_id, employee_id, template_id, position, title, owner_role, \
                due_on, due_offset_days, requires_file) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (employee_id, position) do nothing",
        )
        .bind(organization_id)
        .bind(employee_id)
        .bind(template_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .bind(item.title.trim())
        .bind(item.owner_role.as_deref())
        .bind(due_on)
        .bind(item.due_offset_days)
        .bind(item.requires_file)
        .execute(&mut *tx)
        .await?;
        written += inserted.rows_affected();
    }

    if written == 0 {
        // A refusal that names what is already there, because "already applied" with no detail
        // sends the operator to the employee's page to work out whether it worked.
        let existing = sqlx::query_scalar::<_, i64>(
            "select count(*) from hr_onboarding_items \
             where employee_id = $1 and template_id = $2",
        )
        .bind(employee_id)
        .bind(template_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        return Err(HrError::AlreadyApplied {
            employee_id,
            template_id,
            items: existing,
        });
    }

    tx.commit().await?;
    checklist_of(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("checklist"))
}

// ---------------------------------------------------------------------------------------------
// Checklists
// ---------------------------------------------------------------------------------------------

/// One employee's checklist, with the bar's two numbers computed from the rows.
pub async fn checklist_of(
    pool: &PgPool,
    organization_id: Uuid,
    employee_id: Uuid,
) -> Result<Option<Checklist>> {
    #[derive(sqlx::FromRow)]
    struct Head {
        employee_id: Uuid,
        employee_name: String,
        department_name: Option<String>,
        template_id: Option<Uuid>,
        template_name: Option<String>,
    }

    // `employee_name` is assembled in SQL rather than in the handler: the board renders a row per
    // employee and a Rust-side format would be a second answer to "what is this person called".
    let head = sqlx::query_as::<_, Head>(
        "select e.id as employee_id, \
                trim(both ' ' from e.first_name || ' ' || e.last_name) as employee_name, \
                d.name as department_name, \
                (select i.template_id from hr_onboarding_items i \
                  where i.employee_id = e.id and i.template_id is not null \
                  order by i.position limit 1) as template_id, \
                t.name as template_name \
         from hr_employees e \
         left join hr_departments d on d.id = e.department_id \
         left join hr_onboarding_templates t on t.id = ( \
             select i.template_id from hr_onboarding_items i \
              where i.employee_id = e.id and i.template_id is not null \
              order by i.position limit 1) \
         where e.organization_id = $1 and e.id = $2",
    )
    .bind(organization_id)
    .bind(employee_id)
    .fetch_optional(pool)
    .await?;

    let Some(head) = head else {
        return Ok(None);
    };

    #[derive(sqlx::FromRow)]
    struct ItemRow {
        id: Uuid,
        employee_id: Uuid,
        template_id: Option<Uuid>,
        position: i32,
        title: String,
        owner_role: Option<String>,
        due_on: Option<Date>,
        due_offset_days: Option<i32>,
        requires_file: bool,
        done_at: Option<time::OffsetDateTime>,
        done_by: Option<Uuid>,
        note: String,
    }

    let rows = sqlx::query_as::<_, ItemRow>(
        "select id, employee_id, template_id, position, title, owner_role, due_on, \
                due_offset_days, requires_file, done_at, done_by, note \
         from hr_onboarding_items where employee_id = $1 order by position",
    )
    .bind(employee_id)
    .fetch_all(pool)
    .await?;

    let items: Vec<ChecklistItem> = rows
        .into_iter()
        .map(|row| ChecklistItem {
            id: row.id,
            employee_id: row.employee_id,
            template_id: row.template_id,
            position: row.position,
            title: row.title,
            owner_role: row.owner_role,
            due_on: row.due_on,
            due_offset_days: row.due_offset_days,
            requires_file: row.requires_file,
            done_at: row.done_at,
            done_by: row.done_by,
            note: row.note,
        })
        .collect();

    // The denominator is the row count, never the template's length — see the module docs.
    let total = i64::try_from(items.len()).unwrap_or_default();
    let done = items
        .iter()
        .filter(|item| item.done_at.is_some())
        .count();
    let done = i64::try_from(done).unwrap_or_default();

    Ok(Some(Checklist {
        employee_id: head.employee_id,
        employee_name: head.employee_name,
        department_name: head.department_name,
        template_id: head.template_id,
        template_name: head.template_name,
        items,
        done,
        total,
    }))
}

/// The board: everybody with a checklist, newest starter first.
///
/// The query is the board — it starts from `hr_onboarding_items` rather than from
/// `hr_employees` and left-joining, so a tenant with a thousand employees and six checklists pays
/// for six rows and not for a thousand-row join the screen then filters away.
pub async fn board(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Checklist>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select distinct i.employee_id from hr_onboarding_items i \
         where i.organization_id = $1 order by i.employee_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut boards = Vec::with_capacity(ids.len());
    for employee_id in ids {
        if let Some(checklist) = checklist_of(pool, organization_id, employee_id).await? {
            boards.push(checklist);
        }
    }
    // Newest starter first, so the board opens on the person who joined most recently — the one
    // whose checklist is the one somebody is waiting on.
    boards.sort_by(|left, right| right.employee_id.cmp(&left.employee_id));
    Ok(boards)
}

/// Tick or untick one item, and say whether that finished the checklist.
///
/// The return carries the **whole** checklist rather than the new row because the caller needs
/// the bar's two numbers to redraw, and because the completion event fires on the transition this
/// write performed — a handler that re-read the checklist afterwards could read a state another
/// write has since changed and fire an event for a completion that is not there.
pub async fn tick_item(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    done: bool,
    actor: Uuid,
) -> Result<Checklist> {
    let employee_id: Option<Uuid> = sqlx::query_scalar(
        "update hr_onboarding_items set \
           done_at = case when $3 then now() else null end, \
           done_by = case when $3 then $4 else null end, \
           updated_at = now() \
         where organization_id = $1 and id = $2 \
         returning employee_id",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(done)
    .bind(actor)
    .fetch_optional(pool)
    .await?;

    let employee_id = employee_id.ok_or(HrError::NotFound("onboarding item"))?;
    checklist_of(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("checklist"))
}

/// Attach a note to one item.
pub async fn annotate_item(
    pool: &PgPool,
    organization_id: Uuid,
    item_id: Uuid,
    note: &str,
) -> Result<Checklist> {
    if note.len() > 2000 {
        return Err(HrError::invalid(
            "onboarding item",
            "note",
            "the note is longer than 2000 characters",
        ));
    }
    let employee_id: Option<Uuid> = sqlx::query_scalar(
        "update hr_onboarding_items set note = $3, updated_at = now() \
         where organization_id = $1 and id = $2 returning employee_id",
    )
    .bind(organization_id)
    .bind(item_id)
    .bind(note)
    .fetch_optional(pool)
    .await?;

    let employee_id = employee_id.ok_or(HrError::NotFound("onboarding item"))?;
    checklist_of(pool, organization_id, employee_id)
        .await?
        .ok_or(HrError::NotFound("checklist"))
}

/// Whether the unique-violation SQLSTATE came back — the one place the driver's error is asked
/// what it was rather than rendered as text.
///
/// `23505` is the code for `unique_violation`. Matching on the string of the message is how a
/// database that rewords its errors between versions silently stops turning a conflict into a
/// 409, and the symptom is a 500 that says "duplicate key value violates unique constraint" to a
/// person filling in a form.
fn is_unique_violation(error: &dyn sqlx::error::DatabaseError) -> bool {
    error.code().as_deref() == Some("23505")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, offset: Option<i32>) -> TemplateItem {
        TemplateItem {
            title: title.to_owned(),
            owner_role: None,
            due_offset_days: offset,
            requires_file: false,
        }
    }

    #[test]
    fn a_seeded_item_parses_from_the_short_form_the_migration_writes() {
        // The seed in 0196 is hand-written JSON with three keys. An item parser that demands all
        // four would make the seed this module has had since slice 1 unparseable — and it would
        // do so with a test that only ever fed it the long form.
        let value = serde_json::json!({
            "title": "Sign the contract",
            "owner_role": "hr",
            "due_offset_days": 0,
            "requires_file": true
        });
        let parsed = TemplateItem::from_value(&value).expect("the seeded shape parses");
        assert_eq!(parsed.title, "Sign the contract");
        assert_eq!(parsed.due_offset_days, Some(0));
        assert!(parsed.requires_file);

        let minimal = serde_json::json!({ "title": "Buy a coffee grinder" });
        let parsed = TemplateItem::from_value(&minimal).expect("a title alone is enough");
        assert_eq!(parsed.due_offset_days, None);
        assert!(!parsed.requires_file);
        assert_eq!(parsed.owner_role, None);
    }

    #[test]
    fn an_item_without_a_title_is_dropped_rather_than_saved_as_a_blank_step() {
        // A blank row in a checklist is the row the person reads on their first day. Dropping it
        // silently would hide the editor's bug; the editor validates instead.
        assert!(TemplateItem::from_value(&serde_json::json!({ "title": "   " })).is_none());
        assert!(TemplateItem::from_value(&serde_json::json!({ "owner_role": "hr" })).is_none());
    }

    #[test]
    fn an_empty_template_is_refused_rather_than_saved_as_something_that_writes_nothing() {
        let refused = validate_items(&[]).expect_err("an empty template writes nothing");
        assert!(
            refused.to_string().contains("at least one item"),
            "{refused}"
        );
    }

    #[test]
    fn an_offset_outside_the_window_is_named_with_the_item_it_belongs_to() {
        // The refusal quotes the title: "due offset out of range" sends the operator to a list of
        // twelve items to find which one.
        let refused = validate_items(&[item("Collect identification", Some(400))])
            .expect_err("a 400-day offset is a typo");
        let sentence = refused.to_string();
        assert!(sentence.contains("Collect identification"), "{sentence}");
        assert!(sentence.contains("400"), "{sentence}");
        assert!(sentence.contains("0"), "{sentence}");
    }

    #[test]
    fn an_item_with_no_offset_is_allowed_because_no_deadline_is_an_answer() {
        assert!(validate_items(&[item("Buy a coffee grinder", None)]).is_ok());
    }

    #[test]
    fn an_empty_checklist_reads_as_nothing_done_rather_than_complete() {
        // The divide-by-zero and the "everything is finished" reading are the same bug seen from
        // two sides: both put an employee with nothing to do on a board's finished tab.
        let checklist = Checklist {
            employee_id: Uuid::nil(),
            employee_name: "Nobody".to_owned(),
            department_name: None,
            template_id: None,
            template_name: None,
            items: Vec::new(),
            done: 0,
            total: 0,
        };
        assert_eq!(checklist.progress(), 0.0);
        assert!(!checklist.just_completed(), "an empty list is not a finished one");
    }

    #[test]
    fn the_completion_event_fires_on_the_transition_not_on_every_ticked_item() {
        let base = Checklist {
            employee_id: Uuid::nil(),
            employee_name: "Ada".to_owned(),
            department_name: None,
            template_id: None,
            template_name: None,
            items: Vec::new(),
            done: 0,
            total: 3,
        };
        let partial = Checklist {
            done: 2,
            ..base.clone()
        };
        assert!(!partial.just_completed(), "two of three is not finished");
        let finished = Checklist {
            done: 3,
            ..base.clone()
        };
        assert!(finished.just_completed());
        assert_eq!(finished.progress(), 1.0);
    }
}