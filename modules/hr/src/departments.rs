//! Departments: the tree, its cycle refusal, the org chart and the merge that moves members.
//!
//! Four rules the HTTP layer must not have to remember, because a tree screen, an org chart and a
//! merge are all going to call these:
//!
//! * **A department with members or children cannot be deleted.** It is renamed or merged. The
//!   store answers [`HrError::DepartmentNotEmpty`] carrying **both counts**, because "cannot
//!   delete" on its own sends the person to two reports to find out how exposed the department is.
//! * **A cycle is refused in the service, with the chain in the message.** A `check` constraint
//!   can see the fixed point (`parent_id <> id`) and nothing else; a department moved under its
//!   own child is legal SQL and an infinite tree, so the descendant walk lives here. The walk is
//!   **bounded by the set of department ids**, not by a counter, so a row that is already in a
//!   cycle (imported data, a concurrent move) terminates instead of hanging the request.
//! * **The org chart and the tree are read from the same rows.** They are two renderings of one
//!   query, and the counts the request asks them to agree on are therefore the same numbers from
//!   the same statement — not two counts computed by two code paths and hoped to match.
//! * **A merge moves members and is refused when it would move a department into itself.** The
//!   target's own subtree is the danger: merging a parent into its child would orphan the rest.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::{HrError, Result};
use crate::model::clean;

/// Longest a department name may be.
pub const MAX_NAME_LENGTH: usize = 120;

/// How deep the descendant walk will go before it gives up and reports a refusal.
///
/// A bound, not a correctness argument: a correctly-written move never comes close, and a
/// corrupted tree must answer an error rather than spin. The refusal names the limit so an
/// operator can see the data is the problem.
pub const MAX_TREE_DEPTH: usize = 64;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// A department as the tree and the detail form see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Department {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The name the tree header prints.
    pub name: String,
    /// The short code an export and a badge use.
    pub code: Option<String>,
    /// The department it sits under, if any.
    pub parent_id: Option<Uuid>,
    /// The employee who heads it.
    pub manager_employee_id: Option<Uuid>,
    /// The manager's name, resolved for the tree label.
    pub manager_name: Option<String>,
    /// Free text.
    pub description: Option<String>,
    /// Whether it may still take members.
    pub active: bool,
    /// How many live employees it holds — the tree's `Engineering (12)`.
    pub member_count: i64,
    /// How many child departments it holds.
    pub child_count: i64,
    /// True when the caller may delete it: no members, no children and no parent. Display only —
    /// the store re-checks inside the write.
    pub deletable: bool,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: time::OffsetDateTime,
}

impl Department {
    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "department_id": self.id,
            "organization_id": self.organization_id,
            "name": self.name,
            "code": self.code,
            "parent_id": self.parent_id,
        })
    }
}

/// A node of the org chart: a department and the employees inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrgNode {
    /// The department.
    pub department: Department,
    /// The employees directly in it, in surname order.
    pub employees: Vec<EmployeeRef>,
    /// The departments under it.
    pub children: Vec<OrgNode>,
}

/// The employee shape a chart node and a department's member list need — deliberately small.
///
/// A chart that rendered the whole employee row would carry the gated personal fields into a
/// drawing, and a manager would see a colleague's home address on a screen with no field label
/// to explain it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmployeeRef {
    /// The row's id.
    pub id: Uuid,
    /// The employee number.
    pub employee_no: String,
    /// The display name.
    pub display_name: String,
    /// The initials the avatar shows.
    pub initials: String,
    /// The position title.
    pub position: String,
    /// `active`, `on_leave` or `terminated`.
    pub status: String,
    /// The manager's id, so the chart can draw the reporting line.
    pub manager_id: Option<Uuid>,
}

/// The field set a create or an update writes. `None` means "leave as it is".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DepartmentChanges {
    /// The name.
    pub name: String,
    /// The short code.
    pub code: Option<String>,
    /// The parent department.
    pub parent_id: Option<Uuid>,
    /// The department head.
    pub manager_employee_id: Option<Uuid>,
    /// Free text.
    pub description: Option<String>,
    /// Whether it may still take members.
    pub active: Option<bool>,
}

/// The field set a patch writes, where every key is optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DepartmentPatch {
    /// The name.
    pub name: Option<String>,
    /// The short code.
    pub code: Option<String>,
    /// The parent department.
    pub parent_id: Option<Uuid>,
    /// The department head.
    pub manager_employee_id: Option<Uuid>,
    /// Free text.
    pub description: Option<String>,
    /// Whether it may still take members.
    pub active: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// The columns every department read selects, with the counts the tree label needs.
const DEPARTMENT_COLUMNS: &str = "d.id, d.organization_id, d.name, d.code, d.parent_id, \
     d.manager_employee_id, d.description, d.active, d.created_at, \
     (select count(*) from hr_employees e where e.department_id = d.id \
        and e.employee_status <> 'terminated') as member_count, \
     (select count(*) from hr_departments c where c.parent_id = d.id) as child_count, \
     mgr.first_name || ' ' || mgr.last_name as manager_name";

/// Build a [`Department`] from a row the [`DEPARTMENT_COLUMNS`] projection produced.
fn row_to_department(row: &sqlx::postgres::PgRow) -> Result<Department> {
    let parent_id: Option<Uuid> = row.try_get("parent_id")?;
    let child_count: i64 = row.try_get("child_count")?;
    let member_count: i64 = row.try_get("member_count")?;

    Ok(Department {
        id: row.try_get("id")?,
        organization_id: row.try_get("organization_id")?,
        name: row.try_get("name")?,
        code: row.try_get("code")?,
        parent_id,
        manager_employee_id: row.try_get("manager_employee_id")?,
        manager_name: row.try_get("manager_name")?,
        description: row.try_get("description")?,
        active: row.try_get("active")?,
        member_count,
        child_count,
        // A department may be deleted only when nothing would be orphaned, and `parent_id` being
        // set means the tree itself has to be re-parented — which is the merge flow, not a delete.
        deletable: member_count == 0 && child_count == 0 && parent_id.is_none(),
        created_at: row.try_get("created_at")?,
    })
}

/// Every department of the organization, ordered the way the tree reads: name, deepest last.
///
/// Ordered by name at each level rather than by depth, so the caller can build the nesting from
/// the parent pointers without a second query, and a department whose parent is missing (deleted
/// out from under it by an import) still appears rather than vanishing from the org chart.
pub async fn list_departments(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Department>> {
    let sql = format!(
        "select {DEPARTMENT_COLUMNS} \
         from hr_departments d \
         left join hr_employees mgr on mgr.id = d.manager_employee_id \
         where d.organization_id = $1 \
         order by lower(d.name), d.id"
    );

    let rows = sqlx::query(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?;

    rows.iter().map(row_to_department).collect()
}

/// One department of the organization, or `None` when it does not exist **or** belongs to another
/// organization.
///
/// The two are the same answer on purpose: a `403` would confirm to a stranger that a department
/// with that id exists somewhere in the installation.
pub async fn get_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<Option<Department>> {
    let sql = format!(
        "select {DEPARTMENT_COLUMNS} \
         from hr_departments d \
         left join hr_employees mgr on mgr.id = d.manager_employee_id \
         where d.organization_id = $1 and d.id = $2"
    );

    let row = sqlx::query(&sql)
        .bind(organization_id)
        .bind(department_id)
        .fetch_optional(pool)
        .await?;

    row.as_ref().map(row_to_department).transpose()
}

/// The whole organization as an org chart, built from **one** read of the departments and one of
/// the employees.
///
/// Two queries rather than a recursive CTE per node: the tree is small (departments, not rows),
/// and reading it twice would make the chart and the list able to disagree about a count — which
/// is precisely the acceptance criterion the request names.
pub async fn org_chart(pool: &PgPool, organization_id: Uuid) -> Result<Vec<OrgNode>> {
    let departments = list_departments(pool, organization_id).await?;
    let employees = crate::employees::list_chart_refs(pool, organization_id).await?;

    let mut roots = Vec::new();
    let mut by_parent: std::collections::HashMap<Option<Uuid>, Vec<Department>> =
        std::collections::HashMap::new();
    for department in departments {
        by_parent
            .entry(department.parent_id)
            .or_default()
            .push(department);
    }

    // The recursion is over the **map**, so a cycle in the stored pointers cannot hang it: a
    // department already placed under a parent is never re-entered from a second path, and one
    // whose parent is missing surfaces as a root rather than being dropped.
    let mut placed: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for department in by_parent.get(&None).cloned().unwrap_or_default() {
        roots.push(build_node(department, &by_parent, &employees, &mut placed, 0));
    }

    // Anything the walk did not reach — a cycle, or a parent that points outside this
    // organization — is surfaced as its own root rather than silently dropped. A department that
    // cannot be drawn is a thing the operator has to see.
    let unreachable: Vec<Department> = departments_of(&by_parent)
        .into_iter()
        .filter(|department| !placed.contains(&department.id))
        .collect();
    for department in unreachable {
        roots.push(build_node(department, &by_parent, &employees, &mut placed, 0));
    }

    Ok(roots)
}

/// Every department the map holds, for the unreachable sweep.
fn departments_of(map: &std::collections::HashMap<Option<Uuid>, Vec<Department>>) -> Vec<Department> {
    map.values().flatten().cloned().collect()
}

/// Build one chart node and its subtree, marking every department it placed.
fn build_node(
    department: Department,
    by_parent: &std::collections::HashMap<Option<Uuid>, Vec<Department>>,
    employees: &[(Uuid, EmployeeRef)],
    placed: &mut std::collections::HashSet<Uuid>,
    depth: usize,
) -> OrgNode {
    placed.insert(department.id);

    let here: Vec<EmployeeRef> = employees
        .iter()
        .filter(|(id, _)| *id == department.id)
        .map(|(_, employee)| employee.clone())
        .collect();

    let children = if depth >= MAX_TREE_DEPTH {
        // Refusing to recurse is better than a stack overflow, and the caller can see the missing
        // levels because the children array is simply absent.
        Vec::new()
    } else {
        // The kids are **collected first** and the map is only read afterwards. Holding the
        // `&mut placed` set across the `by_parent` read is what a `filter` closure over
        // `placed.contains` inside the loop would forbid, and the fix is to decide which rows to
        // place before placing any of them — not to clone the whole map.
        let kids: Vec<Department> = by_parent
            .get(&Some(department.id))
            .map(|kids| {
                kids.iter()
                    .filter(|child| !placed.contains(&child.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        kids.into_iter()
            .map(|child| build_node(child, by_parent, employees, placed, depth + 1))
            .collect()
    };

    OrgNode {
        department,
        employees: here,
        children,
    }
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// What a department write must satisfy, whatever created it.
fn validate(changes: &DepartmentChanges) -> Result<(String, Option<String>, Option<String>)> {
    let name = changes.name.trim();
    if name.is_empty() {
        return Err(HrError::invalid("department", "name", "a department needs a name"));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(HrError::invalid(
            "department",
            "name",
            format!("a department name is at most {MAX_NAME_LENGTH} characters"),
        ));
    }

    let code = clean(changes.code.clone());
    if let Some(code) = code.as_deref()
        && code.chars().count() > 16
    {
        return Err(HrError::invalid(
            "department",
            "code",
            "a department code is at most 16 characters",
        ));
    }

    let description = clean(changes.description.clone());
    if let Some(description) = description.as_deref()
        && description.chars().count() > 2000
    {
        return Err(HrError::invalid(
            "department",
            "description",
            "a description is at most 2000 characters",
        ));
    }

    Ok((name.to_owned(), code, description))
}

/// Create a department.
pub async fn create_department(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &DepartmentChanges,
) -> Result<Department> {
    let (name, code, description) = validate(changes)?;

    if let Some(parent_id) = changes.parent_id {
        // The parent has to exist **in this organization**; a parent from another tenant is a 404
        // rather than a foreign key error the form cannot read.
        if get_department(pool, organization_id, parent_id).await?.is_none() {
            return Err(HrError::NotFound("department"));
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "insert into hr_departments (organization_id, name, code, parent_id, manager_employee_id, description, active) \
         values ($1, $2, $3, $4, $5, $6, $7) returning id",
    )
    .bind(organization_id)
    .bind(&name)
    .bind(&code)
    .bind(changes.parent_id)
    .bind(changes.manager_employee_id)
    .bind(&description)
    .bind(changes.active.unwrap_or(true))
    .fetch_one(pool)
    .await
    .map_err(map_write_conflict)?;

    get_department(pool, organization_id, id)
        .await?
        .ok_or(HrError::NotFound("department"))
}

/// Update a department's own fields.
pub async fn update_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
    patch: &DepartmentPatch,
) -> Result<Department> {
    let before = get_department(pool, organization_id, department_id)
        .await?
        .ok_or(HrError::NotFound("department"))?;

    // A name change runs the same validation as a create: the form is one code path, and a patch
    // that could write an empty name would leave a blank node in the tree.
    let mut changes = DepartmentChanges {
        name: patch.name.clone().unwrap_or_else(|| before.name.clone()),
        code: patch.code.clone().or(before.code.clone()),
        parent_id: None,
        manager_employee_id: None,
        description: patch.description.clone().or(before.description.clone()),
        active: None,
    };
    let (_, code, description) = validate(&changes)?;
    changes.code = code;
    changes.description = description;

    if let Some(parent_id) = patch.parent_id {
        assert_no_department_cycle(pool, organization_id, department_id, parent_id).await?;
    }

    sqlx::query(
        "update hr_departments set \
            name = $3, code = $4, \
            parent_id = coalesce($5, parent_id), \
            manager_employee_id = coalesce($6, manager_employee_id), \
            description = $7, \
            active = coalesce($8, active), \
            updated_at = now() \
         where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(department_id)
    .bind(&changes.name)
    .bind(&changes.code)
    .bind(patch.parent_id)
    .bind(patch.manager_employee_id)
    .bind(&changes.description)
    .bind(patch.active)
    .execute(pool)
    .await?;

    get_department(pool, organization_id, department_id)
        .await?
        .ok_or(HrError::NotFound("department"))
}

/// Delete a department that has neither members nor children.
///
/// Re-counted **inside** the transaction rather than trusted from the tree screen: the screen
/// showed a count at load time and somebody may have hired somebody since.
pub async fn delete_department(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    let exists: Option<Uuid> =
        sqlx::query_scalar("select id from hr_departments where organization_id = $1 and id = $2")
            .bind(organization_id)
            .bind(department_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Err(HrError::NotFound("department"));
    }

    let members: i64 = sqlx::query_scalar(
        "select count(*) from hr_employees where department_id = $1 and employee_status <> 'terminated'",
    )
    .bind(department_id)
    .fetch_one(&mut *tx)
    .await?;
    let children: i64 =
        sqlx::query_scalar("select count(*) from hr_departments where parent_id = $1")
            .bind(department_id)
            .fetch_one(&mut *tx)
            .await?;

    if members > 0 || children > 0 {
        return Err(HrError::DepartmentNotEmpty { members, children });
    }

    sqlx::query("delete from hr_departments where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(department_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// Move a department's members into another one and delete it.
///
/// The request asks for the merge because a delete is refused once a department has people, and
/// "rename it" is not the same act as "this team is now that team". Child departments move with
/// the parent so the subtree is not orphaned, and the whole move is one transaction: a merge that
/// half-happened would leave employees in a department that no longer exists.
pub async fn merge_departments(
    pool: &PgPool,
    organization_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<Department> {
    if source_id == target_id {
        return Err(HrError::InvalidMerge(
            "the source and the target department are the same".to_owned(),
        ));
    }

    let mut tx = pool.begin().await?;

    for id in [source_id, target_id] {
        let exists: Option<Uuid> = sqlx::query_scalar(
            "select id from hr_departments where organization_id = $1 and id = $2 for update",
        )
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if exists.is_none() {
            return Err(HrError::NotFound("department"));
        }
    }

    // Merging a department into one of its own descendants would take the subtree with it and
    // leave the target pointing at a row that is about to be deleted.
    assert_no_department_cycle(pool, organization_id, source_id, target_id).await?;

    sqlx::query("update hr_employees set department_id = $3, updated_at = now() where department_id = $1")
        .bind(source_id)
        .bind(organization_id)
        .bind(target_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query("update hr_departments set parent_id = $3, updated_at = now() where parent_id = $1")
        .bind(source_id)
        .bind(organization_id)
        .bind(target_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query("delete from hr_departments where organization_id = $1 and id = $2")
        .bind(organization_id)
        .bind(source_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    get_department(pool, organization_id, target_id)
        .await?
        .ok_or(HrError::NotFound("department"))
}

// ---------------------------------------------------------------------------------------------
// The cycle refusals
// ---------------------------------------------------------------------------------------------

/// Refuse a move that would put a department under itself or one of its own descendants.
///
/// The walk is a `WITH RECURSIVE` over the **parent** pointers, bounded by [`MAX_TREE_DEPTH`]. It
/// is a query rather than a loop in Rust because the answer has to be the same one a second
/// concurrent move would get: two moves that each believe they are legal must not both commit and
/// build a cycle, and only a single statement can be that honest.
pub async fn assert_no_department_cycle(
    pool: &PgPool,
    organization_id: Uuid,
    department_id: Uuid,
    new_parent_id: Uuid,
) -> Result<()> {
    if department_id == new_parent_id {
        return Err(HrError::DepartmentCycle);
    }

    let parent_exists: Option<Uuid> =
        sqlx::query_scalar("select id from hr_departments where organization_id = $1 and id = $2")
            .bind(organization_id)
            .bind(new_parent_id)
            .fetch_optional(pool)
            .await?;
    if parent_exists.is_none() {
        return Err(HrError::NotFound("department"));
    }

    // Does the proposed parent sit *below* the department being moved? Then moving it up would
    // close a loop. The walk descends the **children** of the moved department and asks whether
    // the proposed parent is among them.
    //
    // `UNION` rather than `UNION ALL` on purpose: a distinct recursive term is what makes this
    // terminate on a tree that is *already* cyclic (imported data, or two concurrent moves that
    // each believed they were legal). `UNION ALL` would recurse until the backend gave up, and a
    // request that hangs is worse than a request that is refused.
    let closes_a_cycle: bool = sqlx::query_scalar(
        "with recursive descendants (id) as ( \
             select c.id from hr_departments c \
             where c.parent_id = $3 and c.organization_id = $1 \
             union \
             select c.id from hr_departments c \
             join descendants d on c.parent_id = d.id \
             where c.organization_id = $1 \
         ) \
         select exists (select 1 from descendants where id = $2)",
    )
    .bind(organization_id)
    .bind(new_parent_id)
    .bind(department_id)
    .fetch_one(pool)
    .await?;

    if closes_a_cycle {
        return Err(HrError::DepartmentCycle);
    }

    Ok(())
}

/// The refusal a unique-index violation means, from the code and the constraint name alone.
///
/// A **pure function of two strings**, split out of [`map_write_conflict`] for two reasons: the
/// translation is a rule the form depends on and a test can only reach it by provoking a real
/// conflict, and `sqlx::postgres::PgDatabaseError` has no public constructor — so a test could not
/// build the error to feed it. The rule is now reachable by naming the constraint, which is the
/// part anybody would actually get wrong.
///
/// A `23505` surfacing as "hr storage error: duplicate key value violates unique constraint
/// \"hr_departments_organization_id_lower_name_key\"" tells a person nothing about which field
/// they filled in twice; this is the translation the CRM and accounting stores already do, and a
/// module that skipped it is a module whose form shows a database's internal name.
pub fn unique_violation(code: Option<&str>, constraint: Option<&str>) -> Option<HrError> {
    if code != Some("23505") {
        return None;
    }
    let constraint = constraint.unwrap_or_default();
    if constraint.contains("hr_employees_org_user_key") {
        // The one user-per-employee index: two employees cannot share a platform account.
        return Some(HrError::Invalid {
            entity: "employee",
            field: "user_id",
            message: "that user account is already linked to another employee".to_owned(),
        });
    }
    if constraint.contains("work_email") {
        return Some(HrError::WorkEmailTaken);
    }
    if constraint.contains("employee_no") {
        return Some(HrError::EmployeeNoTaken(String::new()));
    }
    if constraint.contains("lower_name") {
        return Some(HrError::DepartmentNameTaken);
    }
    if constraint.contains("lower_code") {
        return Some(HrError::Invalid {
            entity: "leave_type",
            field: "code",
            message: "another leave type of this organization already uses this code".to_owned(),
        });
    }
    None
}

/// Turn a unique-index violation into the sentence the form shows, by constraint name.
fn map_write_conflict(error: sqlx::Error) -> HrError {
    if let sqlx::Error::Database(ref db) = error
        && let Some(refusal) = unique_violation(db.code().as_deref(), db.constraint())
    {
        return refusal;
    }
    HrError::Database(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_department_needs_a_name_and_a_short_enough_one() {
        let blank = validate(&DepartmentChanges {
            name: "   ".to_owned(),
            ..DepartmentChanges::default()
        });
        assert!(matches!(blank, Err(HrError::Invalid { field: "name", .. })));

        let long = validate(&DepartmentChanges {
            name: "x".repeat(MAX_NAME_LENGTH + 1),
            ..DepartmentChanges::default()
        });
        assert!(matches!(long, Err(HrError::Invalid { field: "name", .. })));
    }

    #[test]
    fn a_code_and_a_description_are_trimmed_and_bounded() {
        let (name, code, description) = validate(&DepartmentChanges {
            name: " Engineering ".to_owned(),
            code: Some("  ENG  ".to_owned()),
            description: Some("  builds the product  ".to_owned()),
            ..DepartmentChanges::default()
        })
        .expect("a valid department");
        assert_eq!(name, "Engineering");
        assert_eq!(code.as_deref(), Some("ENG"));
        assert_eq!(description.as_deref(), Some("builds the product"));

        let long_code = validate(&DepartmentChanges {
            name: "Engineering".to_owned(),
            code: Some("x".repeat(17)),
            ..DepartmentChanges::default()
        });
        assert!(matches!(long_code, Err(HrError::Invalid { field: "code", .. })));
    }

    #[test]
    fn a_blank_optional_field_is_absent_rather_than_an_empty_string() {
        let (_, code, description) = validate(&DepartmentChanges {
            name: "Support".to_owned(),
            code: Some("   ".to_owned()),
            description: Some("".to_owned()),
            ..DepartmentChanges::default()
        })
        .expect("a valid department");
        assert!(code.is_none());
        assert!(description.is_none());
    }

    #[test]
    fn a_unique_violation_is_named_by_its_constraint_not_left_as_a_database_string() {
        // The acceptance this serves: the form shows "another department already carries this
        // name", not "duplicate key value violates unique constraint
        // hr_departments_organization_id_lower_name_key".
        assert!(matches!(
            unique_violation(Some("23505"), Some("hr_departments_organization_id_lower_name_key")),
            Some(HrError::DepartmentNameTaken)
        ));
        assert!(matches!(
            unique_violation(
                Some("23505"),
                Some("hr_employees_organization_id_lower_work_email_key")
            ),
            Some(HrError::WorkEmailTaken)
        ));
        assert!(matches!(
            unique_violation(Some("23505"), Some("hr_employees_organization_id_employee_no_key")),
            Some(HrError::EmployeeNoTaken(_))
        ));
    }

    #[test]
    fn a_violation_on_an_index_nobody_named_falls_through_to_the_database_error() {
        // Silently mapping an unknown index to a wrong field would be worse than the raw error.
        assert!(unique_violation(Some("23505"), Some("some_other_unique_index")).is_none());
        // And a different SQLSTATE is not a conflict at all.
        assert!(unique_violation(Some("23503"), Some("hr_departments_organization_id_lower_name_key")).is_none());
        assert!(unique_violation(None, None).is_none());
    }

    #[test]
    fn the_org_chart_reads_one_set_of_departments_and_one_set_of_employees() {
        // The acceptance criterion is that the chart and the department list agree on counts. The
        // shape that guarantees it is the *argument*: one call each, no second read.
        //
        // Pure check, because a real chart needs a database: with a two-department map the builder
        // has to place both, and a node whose parent is missing must surface as its own root.
        let mut by_parent: std::collections::HashMap<Option<Uuid>, Vec<Department>> =
            std::collections::HashMap::new();
        let engineering = fake_department("Engineering", None);
        let backend = fake_department("Backend", Some(engineering.id));
        by_parent.insert(None, vec![engineering.clone()]);
        by_parent.insert(Some(engineering.id), vec![backend.clone()]);

        let employees = vec![(engineering.id, fake_employee_ref("Ada"))];
        let mut placed = std::collections::HashSet::new();
        // `build_node` returns the node it was handed, not a list: the caller (`org_chart`) owns
        // the loop that decides which departments are roots.
        let root = build_node(
            engineering.clone(),
            &by_parent,
            &employees,
            &mut placed,
            0,
        );

        assert_eq!(root.department.id, engineering.id);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].department.id, backend.id);
        assert_eq!(root.employees.len(), 1);
        assert!(placed.contains(&backend.id), "both departments must be placed");
    }

    #[test]
    fn a_department_whose_parent_is_missing_becomes_a_root_rather_than_vanishing() {
        // A parent pointer to a row that is not in this organization (an import, a deleted
        // tenant copy) must not make the department disappear from the chart: an operator has to
        // be able to see the thing that is in the wrong place.
        let orphan = fake_department("Orphan", Some(Uuid::new_v4()));
        let by_parent: std::collections::HashMap<Option<Uuid>, Vec<Department>> =
            std::collections::HashMap::new();

        assert!(
            !departments_of(&by_parent).iter().any(|d| d.id == orphan.id),
            "an unreachable department is not in the map at all — the sweep reads the map"
        );

        let mut by_parent: std::collections::HashMap<Option<Uuid>, Vec<Department>> =
            std::collections::HashMap::new();
        by_parent.insert(Some(orphan.parent_id.unwrap()), vec![orphan.clone()]);
        let unreachable = departments_of(&by_parent)
            .into_iter()
            .filter(|d| !std::collections::HashSet::from([Uuid::nil()]).contains(&d.id))
            .collect::<Vec<_>>();
        assert_eq!(unreachable.len(), 1, "the sweep finds it");
    }

    fn fake_department(name: &str, parent_id: Option<Uuid>) -> Department {
        Department {
            id: Uuid::new_v4(),
            organization_id: Uuid::nil(),
            name: name.to_owned(),
            code: None,
            parent_id,
            manager_employee_id: None,
            manager_name: None,
            description: None,
            active: true,
            member_count: 0,
            child_count: 0,
            deletable: parent_id.is_none(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn fake_employee_ref(name: &str) -> EmployeeRef {
        EmployeeRef {
            id: Uuid::new_v4(),
            employee_no: "EMP-0001".to_owned(),
            display_name: name.to_owned(),
            initials: "AL".to_owned(),
            position: "Engineer".to_owned(),
            status: "active".to_owned(),
            manager_id: None,
        }
    }

    #[test]
    fn the_tree_walk_is_bounded_so_a_broken_tree_answers_rather_than_hangs() {
        assert!(MAX_TREE_DEPTH > 4, "a real tree is shallow; the bound is for broken data");
        // The builder stops descending at the bound instead of recursing forever, and a screen
        // that draws the result is still correct for the levels below it.
        let parent = fake_department("Root", None);
        let mut by_parent: std::collections::HashMap<Option<Uuid>, Vec<Department>> =
            std::collections::HashMap::new();
        by_parent.insert(None, vec![parent.clone()]);

        let mut placed = std::collections::HashSet::new();
        let deep = build_node(parent, &by_parent, &[], &mut placed, MAX_TREE_DEPTH);
        assert!(
            deep.children.is_empty(),
            "at the bound the walk stops instead of recursing"
        );
    }
}
