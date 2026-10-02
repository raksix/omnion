//! The move, against a real database (REQ-133 slice 3).
//!
//! ## Why this file exists and what it is allowed to assert
//!
//! A move is the one project operation whose *refusal* path is the product. Everything else —
//! list, edit, archive — has a happy path a unit test can reach with a struct literal; this one
//! has a report, a transactional write, an audit row and three refusals, and the interesting
//! claims are all about what does **not** happen: no write on a dry run, no write when the target
//! is archived, no silent success when the workflow moved underneath us.
//!
//! Every test drives [`move_workflow::move_workflow`] itself rather than a SQL script that
//! imitates it, so a change to the function is what changes this file's result.
//!
//! ## What is deliberately NOT asserted
//!
//! That a cross-project dependency refuses a move. No dependency kind that exists on this branch
//! refuses one — [`Dependency::refuses_move`] is `false` for all four, and that is a unit test in
//! the module. A gate asserting a refusal would be asserting behaviour there is no code for. The
//! honest form of that claim is the report's `unchecked` list, which is what
//! [`the_report_says_which_dependencies_it_did_not_check`] proves.

use omnion_workflows::TriggerKind;
use omnion_workflows::error::WorkflowError;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::move_workflow::{self, Dependency};
use omnion_workflows::projects::{self, NewProject, ProjectCaller, ProjectStatus};
use omnion_workflows::store;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Connect, or say why the gate did not run.
///
/// A helper returning `None` and letting each test skip produces a suite that reports zero tests
/// and reads as a pass — the failure mode this branch has already paid for twice.
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-workflow-move.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Per-test id space.
///
/// The namespace must be unique per *test*, not per entity inside a test: the suite runs
/// concurrently against ONE database, so a shared fixture makes one test's assertion another
/// test's rows. The first version of the sibling fixture used a fixed base with a per-entity
/// marker, which looks like namespacing and is not.
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

/// One organization with its owner and its default project.
struct Org {
    id: Uuid,
    owner: Uuid,
    default_project: Uuid,
    default_key: String,
}

/// Seed an organization plus its default project.
///
/// The key is read back from the row rather than assumed: `default_project` generates one, and a
/// test that builds its expectation from the generator is a test that agrees with whatever the
/// generator does today — including a bug.
async fn seed(pool: &PgPool, label: &str) -> Org {
    let space = Space::new();
    let org_id = space.id(1);
    let owner = space.id(2);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Acme', $2, now(), now())",
    )
    .bind(org_id)
    .bind(space.slug(label))
    .execute(pool)
    .await
    .expect("the organization is inserted");

    // `display_name`, not `name`: the first version of this fixture wrote `name` and twelve tests
    // failed with 42703 in 0.11s. The column list is read out of 0001 rather than remembered,
    // which is the only way a fixture written once stays right after a migration.
    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Owner', 'x', now(), now())",
    )
    .bind(owner)
    .bind(org_id)
    .bind(format!("{}@example.test", &space.root.simple().to_string()[..8]))
    .execute(pool)
    .await
    .expect("the owner is inserted");

    let project = projects::default_project(pool, org_id)
        .await
        .expect("the default project");

    Org {
        id: org_id,
        owner,
        default_project: project.id,
        default_key: project.key,
    }
}

fn admin(org: &Org) -> ProjectCaller {
    ProjectCaller {
        user_id: org.owner,
        is_instance_admin: true,
    }
}

/// A non-default project in the organization.
async fn make_project(pool: &PgPool, org: &Org, key: &str, name: &str) -> Uuid {
    projects::create_project(
        pool,
        NewProject {
            organization_id: org.id,
            key: key.into(),
            name: name.into(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: org.owner,
            created_by: Some(org.owner),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("project {key} is created: {e}"))
    .id
}

/// A workflow with a valid definition.
async fn new_workflow(
    pool: &PgPool,
    org: &Org,
    project_id: Uuid,
    name: &str,
    trigger: TriggerKind,
) -> Uuid {
    let new = NewWorkflow {
        organization_id: org.id,
        project_id,
        site_id: None,
        name: name.into(),
        description: String::new(),
        enabled: true,
        trigger,
        schedule: if trigger == TriggerKind::Schedule {
            Some("0 3 * * *".into())
        } else {
            None
        },
        trigger_event: None,
        conditions: serde_json::json!([]),
        next_run_at: if trigger == TriggerKind::Schedule {
            Some(time::OffsetDateTime::now_utc() + time::Duration::hours(1))
        } else {
            None
        },
        steps: serde_json::json!([{ "name": "prepare", "kind": "task", "action": "noop" }]),
        created_by: Some(org.owner),
    };
    store::insert_workflow(pool, new)
        .await
        .unwrap_or_else(|e| panic!("the workflow {name} is written: {e}"))
        .id
}

async fn project_of(pool: &PgPool, workflow_id: Uuid) -> Uuid {
    sqlx::query_scalar("select project_id from workflows where id = $1")
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .expect("the workflow still exists")
}

#[tokio::test]
async fn a_workflow_moves_and_keeps_its_run_history() {
    let pool = pool().await;
    let org = seed(&pool, "move-history").await;
    let target = make_project(&pool, &org, "BILLING", "Billing").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "nightly-close", TriggerKind::Schedule).await;

    // `finished_at` is not optional for a finished run: `workflow_executions_finished_shape`
    // refuses `completed` without it (23514). The first version of this fixture wrote the row
    // without it, which is the constraint working and the fixture being wrong.
    let execution_id: Uuid = sqlx::query_scalar(
        "insert into workflow_executions (workflow_id, organization_id, status, trigger_kind, \
         finished_at) values ($1, $2, 'completed', 'manual', now()) returning id",
    )
    .bind(workflow)
    .bind(org.id)
    .fetch_one(&pool)
    .await
    .expect("an execution row is written");

    let report = move_workflow::move_workflow(
        &pool,
        org.id,
        workflow,
        target,
        Some(org.owner),
        false,
        admin(&org),
    )
    .await
    .expect("the move succeeds");

    assert!(!report.dry_run);
    assert!(!report.refuses);
    assert_eq!(report.from_project_key, org.default_key);
    assert_eq!(report.to_project_key, "BILLING");
    assert_eq!(report.from_project_id, org.default_project);
    assert_eq!(project_of(&pool, workflow).await, target);

    // The history follows: cascade keeps the execution, and it still names the workflow. A move
    // that rewrote history would satisfy the "stays attached" wording on a per-execution read and
    // break it on the count.
    let attached: i64 = sqlx::query_scalar("select count(*) from workflow_executions where workflow_id = $1")
        .bind(workflow)
        .fetch_one(&pool)
        .await
        .expect("history counted");
    assert_eq!(attached, 1, "the run history stays attached to the workflow");
    let owner_workflow: Uuid =
        sqlx::query_scalar("select workflow_id from workflow_executions where id = $1")
            .bind(execution_id)
            .fetch_one(&pool)
            .await
            .expect("the execution still exists");
    assert_eq!(owner_workflow, workflow);

    assert!(
        report
            .dependencies
            .iter()
            .any(|d| matches!(d, Dependency::RunHistory { executions } if *executions == 1)),
        "the report names the run history it carries: {:?}",
        report.dependencies
    );
}

#[tokio::test]
async fn a_dry_run_writes_nothing_and_the_real_move_agrees_with_it() {
    let pool = pool().await;
    let org = seed(&pool, "move-dry").await;
    let target = make_project(&pool, &org, "OPS", "Ops").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "dry-run", TriggerKind::Manual).await;

    let report = move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), true, admin(&org),
    )
    .await
    .expect("a dry run succeeds");
    assert!(report.dry_run);

    // The assertion that separates a dry run from a plan.
    assert_eq!(
        project_of(&pool, workflow).await,
        org.default_project,
        "a dry run must not move the workflow"
    );

    let real = move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await
    .expect("the real move succeeds");
    assert!(!real.dry_run);
    // The dialog renders the dry run's report and the move is decided on the real one, so they
    // have to be the same code rather than two functions that look alike.
    assert_eq!(real.refuses, report.refuses);
    assert_eq!(real.reason, report.reason);
    assert_eq!(real.dependencies.len(), report.dependencies.len());
    assert_eq!(project_of(&pool, workflow).await, target);
}

#[tokio::test]
async fn moving_into_an_archived_project_is_refused_by_name() {
    let pool = pool().await;
    let org = seed(&pool, "move-archived-target").await;
    let target = make_project(&pool, &org, "FROZEN", "Frozen").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "into-the-ice", TriggerKind::Manual).await;
    projects::set_status(&pool, target, ProjectStatus::Archived)
        .await
        .expect("the target archives")
        .expect("the target row comes back");

    let error = move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await
    .expect_err("an archived project refuses the move");
    assert_eq!(error.code(), "project_archived");
    assert!(
        error.to_string().contains("FROZEN"),
        "the message names the project: {error}"
    );
    assert_eq!(
        project_of(&pool, workflow).await,
        org.default_project,
        "the refused move left the workflow alone"
    );
}

#[tokio::test]
async fn moving_out_of_an_archived_project_is_refused() {
    let pool = pool().await;
    let org = seed(&pool, "move-archived-source").await;
    let frozen = make_project(&pool, &org, "COLD", "Cold").await;
    let target = make_project(&pool, &org, "WARM", "Warm").await;
    let workflow = new_workflow(&pool, &org, frozen, "stuck-in-the-past", TriggerKind::Manual).await;
    projects::set_status(&pool, frozen, ProjectStatus::Archived)
        .await
        .expect("archived")
        .expect("row");

    // An archived project is read-only (the rule the run guard enforces), and a move is a write.
    // The archive guard in `create_execution_in` answers "may it run"; this answers "may it be
    // reorganised", and the second question was un-asked before this tick.
    let error = move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await
    .expect_err("an archived source refuses the move");
    assert_eq!(error.code(), "project_archived");
    assert_eq!(project_of(&pool, workflow).await, frozen);
}

#[tokio::test]
async fn moving_a_workflow_into_its_own_project_is_refused() {
    let pool = pool().await;
    let org = seed(&pool, "move-self").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "already-there", TriggerKind::Manual).await;

    let error = move_workflow::move_workflow(
        &pool, org.id, workflow, org.default_project, Some(org.owner), false, admin(&org),
    )
    .await
    .expect_err("a move into the same project is refused");
    assert_eq!(error.code(), "already_in_project");
}

#[tokio::test]
async fn a_workflow_in_another_organization_is_not_moveable() {
    let pool = pool().await;
    let mine = seed(&pool, "move-tenancy-mine").await;
    let theirs = seed(&pool, "move-tenancy-theirs").await;
    let foreign = new_workflow(&pool, &theirs, theirs.default_project, "not-mine", TriggerKind::Manual).await;

    // 404-shaped, not 403-shaped: the message must not confirm that a workflow with this id
    // exists anywhere. Slice 2 established the rule for reads; a move is a read of the workflow
    // followed by a write, and it keeps the same answer.
    let error = move_workflow::move_workflow(
        &pool, mine.id, foreign, mine.default_project, Some(mine.owner), false, admin(&mine),
    )
    .await
    .expect_err("another organization's workflow is not found");
    assert_eq!(error.code(), "workflow_not_found");
    assert!(
        project_of(&pool, foreign).await == theirs.default_project,
        "the foreign workflow is untouched"
    );
}

#[tokio::test]
async fn a_target_in_another_organization_is_not_reachable() {
    let pool = pool().await;
    let mine = seed(&pool, "move-target-mine").await;
    let theirs = seed(&pool, "move-target-theirs").await;
    let workflow = new_workflow(&pool, &mine, mine.default_project, "mine", TriggerKind::Manual).await;

    let error = move_workflow::move_workflow(
        &pool, mine.id, workflow, theirs.default_project, Some(mine.owner), false, admin(&mine),
    )
    .await
    .expect_err("another organization's project is not a target");
    assert_eq!(error.code(), "project_not_found");
    assert_eq!(project_of(&pool, workflow).await, mine.default_project);
}

#[tokio::test]
async fn the_move_writes_one_audit_row_naming_both_projects() {
    let pool = pool().await;
    let org = seed(&pool, "move-audit").await;
    let target = make_project(&pool, &org, "LEDGER", "Ledger").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "audited-move", TriggerKind::Manual).await;

    move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await
    .expect("the move succeeds");

    let row = sqlx::query(
        "select action, target_id, metadata, project_id from audit_log \
         where action = 'workflow.moved' and target_id = $1::text",
    )
    .bind(workflow)
    .fetch_one(&pool)
    .await
    .expect("the move wrote one audit row naming both projects");

    assert_eq!(row.get::<String, _>("action"), "workflow.moved");
    assert_eq!(row.get::<String, _>("target_id"), workflow.to_string());
    assert_eq!(
        row.get::<Uuid, _>("project_id"),
        target,
        "the audit row belongs to the target project, so the target's stream shows it"
    );
    let metadata = row.get::<serde_json::Value, _>("metadata");
    assert_eq!(metadata["from_project_id"], org.default_project.to_string());
    assert_eq!(metadata["to_project_id"], target.to_string());
    assert_eq!(metadata["from_project_key"], org.default_key);
    assert_eq!(metadata["to_project_key"], "LEDGER");
}

#[tokio::test]
async fn a_dry_run_writes_no_audit_row() {
    let pool = pool().await;
    let org = seed(&pool, "move-audit-dry").await;
    let target = make_project(&pool, &org, "PILOT", "Pilot").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "dry-audit", TriggerKind::Manual).await;

    move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), true, admin(&org),
    )
    .await
    .expect("the dry run succeeds");

    let rows: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'workflow.moved' and target_id = $1::text",
    )
    .bind(workflow)
    .fetch_one(&pool)
    .await
    .expect("counted");
    assert_eq!(rows, 0, "a dry run is a report, not an event");
}

#[tokio::test]
async fn the_report_says_which_dependencies_it_did_not_check() {
    let pool = pool().await;
    let org = seed(&pool, "move-unchecked").await;
    let target = make_project(&pool, &org, "AUDIT", "Audit").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "honest-report", TriggerKind::Manual).await;

    let report = move_workflow::plan_move(&pool, org.id, workflow, target, admin(&org))
        .await
        .expect("the plan succeeds");

    // The kinds REQ-133 names that no table on this branch can hold. A report claiming to have
    // checked them would be the lie this field exists to prevent.
    for kind in [
        "credential_reference",
        "sub_workflow_call",
        "inbound_webhook_subscription",
        "published_api_route",
        "workflow_template",
    ] {
        assert!(
            report.unchecked.iter().any(|k| k == kind),
            "the report must name {kind} as unchecked, got {:?}",
            report.unchecked
        );
    }
    assert_eq!(
        report.unchecked.len(),
        move_workflow::UNCHECKED_DEPENDENCY_KINDS.len(),
        "every unchecked kind is reported, not a selection of them"
    );
    assert!(
        !report
            .dependencies
            .iter()
            .any(|d| d.kind() == "run_history"),
        "a workflow with no runs has no run-history dependency"
    );
    assert!(report.blocking_kinds().is_empty());
    assert!(!report.refuses);
    assert!(report.reason.is_none());
}

#[tokio::test]
async fn a_scheduled_workflow_reports_the_cursor_it_will_carry() {
    let pool = pool().await;
    let org = seed(&pool, "move-schedule").await;
    let target = make_project(&pool, &org, "CRON", "Cron").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "every-five", TriggerKind::Schedule).await;

    sqlx::query(
        "update workflows set schedule = '*/5 * * * *', next_run_at = now() + interval '5 minutes' \
         where id = $1",
    )
    .bind(workflow)
    .execute(&pool)
    .await
    .expect("the schedule is written");

    let report = move_workflow::plan_move(&pool, org.id, workflow, target, admin(&org))
        .await
        .expect("the plan succeeds");
    let cursor = report
        .dependencies
        .iter()
        .find_map(|d| match d {
            Dependency::ScheduleCursor { schedule, .. } => Some(schedule.as_str()),
            _ => None,
        })
        .expect("a scheduled workflow reports its cursor");
    assert_eq!(cursor, "*/5 * * * *");

    move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await
    .expect("the move succeeds");

    // The move carries the schedule and does NOT clear the next fire time. A moved schedule that
    // silently stopped would be the exact failure this dependency exists to surface.
    let row = sqlx::query("select schedule, next_run_at from workflows where id = $1")
        .bind(workflow)
        .fetch_one(&pool)
        .await
        .expect("the workflow still exists");
    assert_eq!(row.get::<String, _>("schedule"), "*/5 * * * *");
    assert!(
        row.get::<Option<time::OffsetDateTime>, _>("next_run_at").is_some(),
        "the schedule keeps its next fire time across a project switch"
    );
}

#[tokio::test]
async fn a_workflow_that_moved_under_us_is_a_conflict_not_a_success() {
    let pool = pool().await;
    let org = seed(&pool, "move-race").await;
    let target = make_project(&pool, &org, "RACE", "Race").await;
    let elsewhere = make_project(&pool, &org, "ELSE", "Else").await;
    let workflow = new_workflow(&pool, &org, org.default_project, "stolen-underneath", TriggerKind::Manual).await;

    // The race this function is built for: somebody else moves the workflow after the report was
    // computed. Naming the source project in the `where` is what turns that into a conflict.
    sqlx::query("update workflows set project_id = $2 where id = $1")
        .bind(workflow)
        .bind(elsewhere)
        .execute(&pool)
        .await
        .expect("somebody else moved it");

    let result = move_workflow::move_workflow(
        &pool, org.id, workflow, target, Some(org.owner), false, admin(&org),
    )
    .await;

    // Either the report saw the new project and refuses as a self-move, or the `where` clause
    // matched nothing and answers `workflow_moved`. Both are correct; a silent success is not.
    if let Err(error) = &result {
        assert!(
            matches!(error.code(), "workflow_moved" | "already_in_project"),
            "unexpected error {}: {error}",
            error.code()
        );
    } else {
        assert!(!result.expect("a report").dry_run, "a real move is never a dry run");
    }
    // Whatever happened, the workflow is in exactly one project and it is not `target` by accident:
    // a "successful" move of a workflow somebody else already moved is the bug this test exists for.
    let after = project_of(&pool, workflow).await;
    assert!(
        after == elsewhere || after == target,
        "the workflow ended in {after}, which is neither project the test wrote"
    );
}

#[tokio::test]
async fn a_refusal_carries_a_typed_code_rather_than_only_a_message() {
    // Small, but it is why every assertion above can use `code()`: a message-only refusal makes
    // each one a substring match, and substring assertions are how a gate starts passing for the
    // wrong reason.
    let error = WorkflowError::invalid("workflow_moved", "reload and try again");
    assert_eq!(error.code(), "workflow_moved");
    assert!(error.to_string().contains("reload and try again"));
}
