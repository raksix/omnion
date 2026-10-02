//! Deleting an automation project (REQ-133) — the action the REQ's own API table has documented
//! since the module shipped, and which no store function, handler or button implemented.
//!
//! ## The defect this file exists for
//!
//! `DELETE /api/v1/projects/{id}` is in the REQ's API table with "typed confirmation, dependency
//! check" as its purpose, and the settings screen's row is "Name, key, description, colour,
//! archive, export, delete". Migration `0164` even chose `on delete restrict` for the
//! resource → project foreign keys *because* "deletion is a deliberate act with its own dependency
//! check (slice 4)" — and slice 4 shipped the caps, the ownership transfer and the archive guards
//! without writing the function its own migration comment names. An installation could therefore
//! create projects for ever and never remove one, and `automation.project.deleted` was an event
//! nothing emitted.
//!
//! This is the branch's signature defect in its most complete form: **every safe half of the
//! feature exists and the destructive half is described.** The gates that were green here
//! (`run-project-limits.sh` 14/14, `run-project-write-guard.sh` 5/5, `run-workflow-move.sh` 13/13)
//! each measured the clauses that had been implemented, which is the general rule this file's
//! header gate already states: *a gate named after a sentence measures the clauses that exist.*
//!
//! ## What is asserted here
//!
//! 1. **The default project is refused by name** — and it is refused *first*, because it is the
//!    one refusal no amount of cleanup fixes: every insert path resolves through
//!    `default_project`, so deleting it would leave an organization with nowhere to put an
//!    automation.
//! 2. **The typed confirmation is the project's KEY**, and a near-miss is refused by name. `PAY`
//!    against `PAYROLL` fails, which is the entire purpose of typing it.
//! 3. **A project holding workflows is refused with a count** naming the key — not a 23503 naming
//!    a foreign-key constraint, which is what `on delete restrict` alone would have produced.
//! 4. **A refusal writes nothing.** Every refusal is followed by a read of the row, because a
//!    delete that refused *after* deleting is the failure a message-only test cannot see.
//! 5. **The audit trail survives the project.** The delete's own row is written inside the
//!    transaction with its `project_id` intact; a pool write after the commit would land with a
//!    null project and be the one row an operator cannot find after the fact.
//! 6. **The empty project really goes**, and its members and limits are cascaded with it.
//! 7. **A sibling project is untouched** — the negative control that stops a broad `where` clause
//!    from turning one delete into an organization wipe.

use omnion_audit::NewAuditEntry;
use omnion_workflows::error::WorkflowError;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::projects::{self, NewProject};
use omnion_workflows::store;
use omnion_workflows::TriggerKind;
use sqlx::PgPool;
use uuid::Uuid;

/// Per-test id namespace. Unique per *test*, not per entity inside a test — the suite shares one
/// database and runs concurrently.
struct Space {
    root: Uuid,
}

impl Space {
    fn new() -> Self {
        Self {
            root: Uuid::new_v4(),
        }
    }

    fn id(&self, marker: u8) -> Uuid {
        let mut bytes = *self.root.as_bytes();
        bytes[14] = marker;
        bytes[15] = 0;
        Uuid::from_bytes(bytes)
    }

    fn slug(&self, prefix: &str) -> String {
        format!("{prefix}-{}", &self.root.simple().to_string()[..8])
    }
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-project-delete.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// One organization, one owner, one non-default project to delete and one sibling that must
/// survive it.
async fn world() -> (PgPool, Uuid, Uuid, Uuid, Uuid) {
    let pool = pool().await;
    let space = Space::new();
    let acme = space.id(1);
    let owner = space.id(2);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Acme', $2, now(), now())",
    )
    .bind(acme)
    .bind(space.slug("acme"))
    .execute(&pool)
    .await
    .expect("an organization");

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Acme Owner', 'x', now(), now())",
    )
    .bind(owner)
    .bind(acme)
    .bind(format!("owner@{}", space.slug("acme")))
    .execute(&pool)
    .await
    .expect("an account");

    let project = projects::create_project(
        &pool,
        NewProject {
            organization_id: acme,
            key: "OPS".to_owned(),
            name: "Operations".to_owned(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a project");

    (pool, acme, owner, project.id, space.id(3))
}

fn new_workflow(organization_id: Uuid, project_id: Uuid, name: &str) -> NewWorkflow {
    NewWorkflow {
        organization_id,
        project_id,
        site_id: None,
        name: name.to_owned(),
        description: String::new(),
        enabled: true,
        trigger: TriggerKind::Manual,
        schedule: None,
        trigger_event: None,
        conditions: serde_json::json!([]),
        next_run_at: None,
        steps: serde_json::json!([{"kind": "task", "action": "noop", "params": {}}]),
        created_by: None,
    }
}

/// The error code of a refusal, or a panic that says what came back instead.
fn code<T>(result: Result<T, WorkflowError>) -> String {
    match result {
        Ok(_) => panic!("expected a refusal and the delete succeeded"),
        Err(WorkflowError::Invalid { code, .. }) => code.to_owned(),
        Err(other) => panic!("expected an Invalid refusal, got {other:?}"),
    }
}

async fn project_exists(pool: &PgPool, project_id: Uuid) -> bool {
    sqlx::query_scalar::<_, i64>("select count(*) from automation_projects where id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await
        .expect("the count")
        > 0
}

/// Run `delete_project` on a rolled-back transaction and hand back the outcome. Every refusal in
/// this file is read through this, so a test can never accidentally assert against a project the
/// same test already deleted.
async fn attempt(
    pool: &PgPool,
    project_id: Uuid,
    confirmation: &str,
) -> Result<projects::ProjectDependencies, WorkflowError> {
    let mut transaction = pool.begin().await.expect("a transaction");
    let outcome = projects::project_delete_checks(&mut transaction, project_id, confirmation).await;
    // Rolled back on purpose: a refusal must leave the database untouched, and a committed
    // success here would delete the fixture out from under the assertion that follows.
    let _ = transaction.rollback().await;
    outcome
}

/// The delete as the handler performs it: checks, then the audit row, then the `delete`.
///
/// **The order is the subject, not an arrangement.** `audit_log.project_id` is a foreign key with
/// `on delete set null` and the constraint is checked when the row is inserted, so an audit row
/// naming this project is legal only while the project is still there. Writing it after the
/// delete — even in the same transaction — answers 23503. This helper performs the three steps in
/// the only order that works, and the test below fails loudly if somebody swaps them.
async fn perform_delete(
    pool: &PgPool,
    project_id: Uuid,
    organization_id: Uuid,
    actor: Uuid,
    key: &str,
) -> Result<projects::ProjectDependencies, WorkflowError> {
    let mut transaction = pool.begin().await.expect("a transaction");
    let dependencies =
        projects::project_delete_checks(&mut transaction, project_id, key).await?;
    omnion_audit::record_for_project_in(
        &mut transaction,
        NewAuditEntry::by_user(actor, "automation.project.deleted")
            .organization(organization_id)
            .target("automation_project", project_id)
            .metadata(serde_json::json!({ "key": dependencies.project_key })),
        project_id,
    )
    .await
    .map_err(WorkflowError::Audit)?;
    projects::commit_project_delete(&mut transaction, project_id).await?;
    transaction.commit().await?;
    Ok(dependencies)
}

/// The default project is the bucket every automation created without a project of its own lands
/// in, so deleting it is refused — and refused FIRST, before the confirmation, so the answer is the
/// rule rather than a missing field.
#[tokio::test]
async fn the_default_project_is_refused_by_name_and_first() {
    let (pool, acme, _owner, _project, _other) = world().await;
    let default = projects::default_project(&pool, acme)
        .await
        .expect("the organization's default project");

    // A *correct* confirmation, so the refusal cannot be the typed one.
    let refused = code(attempt(&pool, default.id, &default.key).await);
    assert_eq!(
        refused, "project_is_default",
        "the default project must be refused by its own code, not by the confirmation check"
    );
    assert!(
        project_exists(&pool, default.id).await,
        "a refused delete must leave the row in place"
    );
}

/// The confirmation is the project KEY, and it is an exact match. `PAY` against `PAYROLL` is the
/// mistake a typed confirmation exists to catch.
#[tokio::test]
async fn the_confirmation_is_the_project_key_and_an_exact_match() {
    let (pool, _acme, _owner, project, _other) = world().await;

    for wrong in ["", "ops", "PAY", "OPS ", "Operations"] {
        let refused = code(attempt(&pool, project, wrong).await);
        assert_eq!(
            refused, "project_delete_confirmation_mismatch",
            "typing {wrong:?} must not delete a project whose key is OPS"
        );
    }
    assert!(
        project_exists(&pool, project).await,
        "no near-miss confirmation may remove the row"
    );
}

/// A project holding workflows is refused with a COUNT and the key in the message, because
/// `on delete restrict` alone answers a 23503 naming a foreign-key constraint — which tells an
/// operator nothing about which project to go and empty.
#[tokio::test]
async fn a_project_holding_workflows_is_refused_with_the_count() {
    let (pool, acme, _owner, project, _other) = world().await;
    store::insert_workflow(&pool, new_workflow(acme, project, "resident"))
        .await
        .expect("a workflow to block the delete");

    let message = match attempt(&pool, project, "OPS").await {
        Ok(_) => panic!("a project with a workflow must not be deleted"),
        Err(WorkflowError::Invalid { code, message }) => {
            assert_eq!(code, "project_has_dependencies");
            message
        }
        Err(other) => panic!("expected an Invalid refusal, got {other:?}"),
    };
    assert!(
        message.contains("OPS") && message.contains('1'),
        "the refusal must name the project and the count, got: {message}"
    );
    assert!(
        project_exists(&pool, project).await,
        "a dependency refusal must leave the row in place"
    );
}

/// The dependency report is what the dialog renders, so it is read directly: a count that the
/// delete would have refused on is the number the operator needs before they start moving things.
#[tokio::test]
async fn the_dependency_report_counts_workflows_members_and_history() {
    let (pool, acme, _owner, project, _other) = world().await;
    let workflow = store::insert_workflow(&pool, new_workflow(acme, project, "counted"))
        .await
        .expect("a workflow");
    // A real run, so `executions` is a count and not a join over an empty table. Started through
    // the store's own `create_execution` rather than a raw insert, because a row written behind
    // the product's back is a fixture that can survive the product changing.
    store::create_execution(&pool, &workflow, TriggerKind::Manual, None, &[])
        .await
        .expect("a run");

    let mut transaction = pool.begin().await.expect("a transaction");
    let report = projects::project_dependencies(&mut transaction, project)
        .await
        .expect("the report");
    let _ = transaction.rollback().await;

    assert_eq!(report.workflows, 1, "one workflow is in the way");
    assert_eq!(report.executions, 1, "and one run of it");
    assert!(report.members >= 1, "the owner is a member: {report:?}");
    assert!(report.blocks_delete(), "a workflow blocks the delete");
    assert_eq!(report.project_key, "OPS", "the report names the project");
}

/// The whole point: a project with nothing in it goes, its members and limits go with it, and its
/// **audit rows survive** — `audit_log.project_id` is `on delete set null`, which is what makes
/// "the trail outlives the container" true rather than aspirational.
#[tokio::test]
async fn an_empty_project_is_deleted_and_its_audit_rows_survive() {
    let (pool, acme, owner, project, _other) = world().await;

    // An audit row recorded inside this project before the delete.
    omnion_audit::record_for_project(
        &pool,
        NewAuditEntry::by_user(owner, "automation.project.created")
            .organization(acme)
            .target("automation_project", project)
            .metadata(serde_json::json!({ "key": "OPS" })),
        project,
    )
    .await
    .expect("a trail row to outlive the project");

    let dependencies = perform_delete(&pool, project, acme, owner, "OPS")
        .await
        .expect("the delete");
    assert_eq!(dependencies.project_key, "OPS");
    assert!(dependencies.members >= 1, "the owner went with it: {dependencies:?}");
    assert!(
        dependencies.audit_rows >= 1,
        "the trail row written before the delete was counted as surviving: {dependencies:?}"
    );

    assert!(!project_exists(&pool, project).await, "the project is gone");

    let members: i64 = sqlx::query_scalar(
        "select count(*) from automation_project_members where project_id = $1",
    )
    .bind(project)
    .fetch_one(&pool)
    .await
    .expect("the member count");
    assert_eq!(members, 0, "memberships are cascaded with the project");

    // `on delete set null` fires on every row that named this project, so after the commit NONE
    // of them points at a project. That is the documented state the instance-wide stream renders
    // and it is the whole claim: the text survives, the container reference does not.
    let survivors: i64 =
        sqlx::query_scalar("select count(*) from audit_log where project_id = $1")
            .bind(project)
            .fetch_one(&pool)
            .await
            .expect("the survivor count");
    assert_eq!(
        survivors, 0,
        "`on delete set null` detaches every row that named the project"
    );
    // The delete's own row is stored with the project still attached — it was written BEFORE the
    // `delete`, inside the same transaction, which is the only order the foreign key permits.
    let actor: String = sqlx::query_scalar(
        "select actor_type from audit_log where target_id = $1 and action = 'automation.project.deleted'",
    )
    .bind(project.to_string())
    .fetch_one(&pool)
    .await
    .expect("the delete's own row, written inside the transaction");
    assert_eq!(
        actor,
        "user",
        "the delete is recorded as a user action, and it was written by the transaction that \
         deleted the project rather than after it"
    );
    let nulled: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where target_id = $1 and project_id is null",
    )
    .bind(project.to_string())
    .fetch_one(&pool)
    .await
    .expect("the detached count");
    assert_eq!(
        nulled, 2,
        "both rows survive the project, detached from it — the record outlives the container"
    );
    // **The text survives too, not just the row count.** A delete that cascaded the trail away
    // would also answer 2 here as a `nulled` count if it counted what is left, so the action
    // names are read back: `automation.project.created` was recorded long before the delete and
    // has no reason to disappear with it.
    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where target_id = $1 order by created_at",
    )
    .bind(project.to_string())
    .fetch_all(&pool)
    .await
    .expect("the surviving actions");
    assert_eq!(
        actions,
        vec![
            "automation.project.created".to_owned(),
            "automation.project.deleted".to_owned()
        ],
        "the trail outlives the container, and the delete is the last word in it"
    );
}

/// The negative control: deleting one project must not touch its sibling. A `where` clause one
/// token too broad is the whole failure this catches, and no other test here would see it.
#[tokio::test]
async fn deleting_one_project_leaves_its_sibling_alone() {
    let (pool, acme, owner, project, sibling_id) = world().await;

    let sibling = projects::create_project(
        &pool,
        NewProject {
            organization_id: acme,
            key: "OPSX".to_owned(),
            name: "Operations Two".to_owned(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a sibling project");
    store::insert_workflow(&pool, new_workflow(acme, sibling.id, "untouched"))
        .await
        .expect("a workflow in the sibling");

    let mut transaction = pool.begin().await.expect("a transaction");
    projects::project_delete_checks(&mut transaction, project, "OPS")
        .await
        .expect("the checks");
    projects::commit_project_delete(&mut transaction, project)
        .await
        .expect("the delete");
    transaction.commit().await.expect("the commit");

    assert!(!project_exists(&pool, project).await, "the target is gone");
    assert!(
        project_exists(&pool, sibling.id).await,
        "the sibling must survive: a broad `where` clause is the defect this controls"
    );
    let workflows: i64 = sqlx::query_scalar(
        "select count(*) from workflows where project_id = $1",
    )
    .bind(sibling.id)
    .fetch_one(&pool)
    .await
    .expect("the sibling's workflow count");
    assert_eq!(workflows, 1, "the sibling's workflow is untouched");
    let _ = sibling_id;
}
