//! The project write guard (REQ-133) — an archived project is read-only, and `max_workflows` is
//! a cap, against a real database.
//!
//! ## The defect this file exists for
//!
//! Two promises in the REQ were enforced by **nothing that a write path reaches**.
//!
//! * *"Archived projects are read-only — no new runs, **no edits**"*. Only the **runs** half had
//!   an enforcement point (`ensure_run_allowed`, inside `store::create_execution_in`). The other
//!   three workflow write doors — `insert_workflow`, `update_workflow`, `delete_workflow` — read
//!   nothing about the project's `status`, so an editor, an owner **and an instance
//!   administrator** could create, rewrite and delete workflows in an archived project. The
//!   administrator case is the sharpest one: `require_capability` short-circuits to `Owner`
//!   before any project row is read, so the account most likely to reorganise an installation was
//!   the one account for which "archived" did not apply.
//! * *"Per-project maxima for workflows, credentials, runs per day and concurrent runs."*
//!   `max_workflows` had a column, a limits screen that draws a bar for it, a notice sweep that
//!   emits `automation.project.limit_exceeded` when it crosses — and **no check at the write**.
//!   `ensure_run_within_limits` asks about *runs*; nothing asked about *rows*. A project capped at
//!   five workflows could hold five hundred, and the bar read "at the limit" throughout.
//!
//! ## Why the count is asserted rather than the predicate
//!
//! The trap this module keeps meeting: a refusal that creates the row and deletes it again would
//! pass a test that only reads the refusal message. Every test below counts `workflows` rows
//! after the attempt, so *"refused"* and *"refused without writing"* are different assertions and
//! only the second one is asserted.
//!
//! ## What is asserted here
//!
//! 1. An archived project refuses create, update and delete — each by its code, with the row
//!    count unchanged.
//! 2. Restoring the project makes all three work again, so the guard is the archive and not a
//!    permanently damaged row.
//! 3. `max_workflows` refuses the create that would cross it, and the refusal names the project
//!    key, the numbers **and the owner's display name**.
//! 4. A project with **no limits row** is unlimited: a fresh installation has no caps, and a
//!    guard that read "no row" as "no room" would refuse every first workflow.
//! 5. **A delete is not capped by `max_workflows`** — deleting a workflow must never be the thing
//!    that gets refused, because that is the operator's way out of a project that is over its
//!    limit. This is the negative control that keeps the guard honest, and it is the one a
//!    "refuse any write when over quota" implementation fails.
//! 6. **A sibling project is unaffected** — an over-quota project must not become an
//!    organization-wide shutdown, which is what a guard with one `where` clause too broad looks
//!    like.

use omnion_workflows::error::WorkflowError;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::store::WorkflowUpdate;
use omnion_workflows::projects::{self, ProjectStatus};
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
        .expect("DATABASE_URL is set by scripts/qa/run-project-write-guard.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// One organization, one owner, one non-default project.
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
        projects::NewProject {
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

    (pool, acme, owner, project.id, space.id(9))
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
        Ok(_) => panic!("expected a refusal and the write succeeded"),
        Err(WorkflowError::Invalid { code, .. }) => code.to_owned(),
        Err(other) => panic!("expected an Invalid refusal, got {other:?}"),
    }
}

async fn count_workflows(pool: &PgPool, project_id: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from workflows where project_id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await
        .expect("the count")
}

/// The archive guard: three doors, three refusals, and not one row written by any of them.
#[tokio::test]
async fn an_archived_project_refuses_every_workflow_write() {
    let (pool, acme, owner, project, other) = world().await;

    let existing = store::insert_workflow(&pool, new_workflow(acme, project, "kept"))
        .await
        .expect("a workflow to guard");
    assert_eq!(count_workflows(&pool, project).await, 1);

    projects::set_status(&pool, project, ProjectStatus::Archived)
        .await
        .expect("the archive");

    let refused = code(store::insert_workflow(&pool, new_workflow(acme, project, "new")).await);
    assert_eq!(refused, "project_archived", "create must be refused");

    let update = WorkflowUpdate {
        name: "renamed".to_owned(),
        description: String::new(),
        site_id: None,
        enabled: true,
        trigger: TriggerKind::Manual,
        schedule: None,
        next_run_at: None,
        steps: serde_json::json!([]),
        trigger_event: None,
        conditions: serde_json::json!([]),
    };
    let refusal = code(store::update_workflow(&pool, existing.id, update).await);
    assert_eq!(refusal, "project_archived", "edit must be refused");

    let refusal = code(store::delete_workflow(&pool, existing.id).await);
    assert_eq!(refusal, "project_archived", "delete must be refused");

    // The count is the assertion that matters: a guard that refused after writing would leave 2.
    assert_eq!(
        count_workflows(&pool, project).await,
        1,
        "a refusal must not leave a row behind, and the kept workflow must still be readable"
    );

    // And the name on disk is the one from before the refusal, so the *edit* was refused too.
    let name: String = sqlx::query_scalar("select name from workflows where id = $1")
        .bind(existing.id)
        .fetch_one(&pool)
        .await
        .expect("the workflow row");
    assert_eq!(name, "kept", "the refused edit must not have landed");

    // A sibling project keeps working: the guard is about one container, not the organization.
    let sibling = projects::create_project(
        &pool,
        projects::NewProject {
            organization_id: acme,
            key: "BILL".to_owned(),
            name: "Billing".to_owned(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: None,
        },
    )
    .await
    .expect("a sibling project");
    store::insert_workflow(&pool, new_workflow(acme, sibling.id, "still fine"))
        .await
        .expect("a sibling project's create is unaffected by another project's archive");
    let _ = other;
}

/// Restoring is the remedy the refusal names, so it has to work.
#[tokio::test]
async fn restoring_the_project_makes_the_writes_work_again() {
    let (pool, acme, _owner, project, _) = world().await;

    projects::set_status(&pool, project, ProjectStatus::Archived)
        .await
        .expect("the archive");
    assert_eq!(
        code(store::insert_workflow(&pool, new_workflow(acme, project, "too soon")).await),
        "project_archived"
    );

    projects::set_status(&pool, project, ProjectStatus::Active)
        .await
        .expect("the restore");

    store::insert_workflow(&pool, new_workflow(acme, project, "after the restore"))
        .await
        .expect("the create the restore was supposed to unblock");
    assert_eq!(count_workflows(&pool, project).await, 1);
}

/// `max_workflows` is the cap the limits screen draws a bar for, enforced at the write.
#[tokio::test]
async fn the_workflow_cap_is_refused_at_the_create_that_would_cross_it() {
    let (pool, acme, owner, project, _) = world().await;

    omnion_workflows::limits::set_limits(
        &pool,
        project,
        omnion_workflows::limits::LimitOverrides {
            max_workflows: 2,
            max_credentials: 0,
            max_runs_per_day: 0,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: Some(owner),
        },
    )
    .await
    .expect("the cap");

    store::insert_workflow(&pool, new_workflow(acme, project, "one"))
        .await
        .expect("first workflow: under the cap");
    store::insert_workflow(&pool, new_workflow(acme, project, "two"))
        .await
        .expect("second workflow: exactly at the cap");
    assert_eq!(count_workflows(&pool, project).await, 2);

    let refusal = code(store::insert_workflow(&pool, new_workflow(acme, project, "three")).await);
    assert_eq!(
        refusal, "project_workflow_limit_exceeded",
        "the cap is a hard refusal, not a warning"
    );
    assert_eq!(
        count_workflows(&pool, project).await,
        2,
        "the refused create must not have written"
    );

    // The message names the limit, the numbers, the project **and the owner** — the REQ asks for
    // a message naming the limit and the project owner, and a message naming neither is a code.
    let message = store::insert_workflow(&pool, new_workflow(acme, project, "four"))
        .await
        .expect_err("still refused")
        .to_string();
    assert!(
        message.contains("OPS"),
        "the refusal names the project: {message}"
    );
    assert!(
        message.contains("Acme Owner"),
        "the refusal names the owner's display name, not an id: {message}"
    );
    assert!(
        message.contains("2 of 2"),
        "the refusal names the numbers: {message}"
    );
}

/// A project with no limits row is unlimited. A fresh installation has no caps, so a guard that
/// read "no row" as "no room" would refuse the first workflow anybody ever creates.
#[tokio::test]
async fn a_project_with_no_limits_row_is_unlimited() {
    let (pool, acme, _owner, project, _) = world().await;
    let row: Option<i32> = sqlx::query_scalar(
        "select max_workflows from automation_project_limits where project_id = $1",
    )
    .bind(project)
    .fetch_optional(&pool)
    .await
    .expect("the limits read");
    assert!(row.is_none(), "the fixture must have no caps at all");

    for name in ["one", "two", "three", "four", "five"] {
        store::insert_workflow(&pool, new_workflow(acme, project, name))
            .await
            .unwrap_or_else(|e| panic!("an uncapped project accepts {name}: {e}"));
    }
    assert_eq!(count_workflows(&pool, project).await, 5);
}

/// **The negative control.** Deleting must never be what gets refused: it is the operator's way
/// out of a project that is over its cap, and a guard that blocked it would make the cap
/// unrecoverable without a second route nobody wrote.
#[tokio::test]
async fn a_delete_is_never_refused_by_the_workflow_cap() {
    let (pool, acme, owner, project, _) = world().await;

    omnion_workflows::limits::set_limits(
        &pool,
        project,
        omnion_workflows::limits::LimitOverrides {
            max_workflows: 1,
            max_credentials: 0,
            max_runs_per_day: 0,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: Some(owner),
        },
    )
    .await
    .expect("the cap");

    let first = store::insert_workflow(&pool, new_workflow(acme, project, "one"))
        .await
        .expect("first workflow");
    assert_eq!(
        code(store::insert_workflow(&pool, new_workflow(acme, project, "two")).await),
        "project_workflow_limit_exceeded"
    );

    assert!(
        store::delete_workflow(&pool, first.id).await.expect("the delete"),
        "an over-cap project must still be able to delete its way back under the cap"
    );
    assert_eq!(count_workflows(&pool, project).await, 0);

    // And the create that was refused now lands, which is what makes the cap recoverable rather
    // than a one-way door.
    store::insert_workflow(&pool, new_workflow(acme, project, "two"))
        .await
        .expect("room again after the delete");
}
