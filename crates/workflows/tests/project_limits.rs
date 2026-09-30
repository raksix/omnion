//! Project limits, usage counters and ownership transfer, against a real database (REQ-133
//! slice 4).
//!
//! ## What makes these integration tests and not more unit tests
//!
//! Every claim in this file is about a consequence — a run that was refused, a counter that
//! matched the rows it counts, an owner who was demoted rather than deleted — and each of them has
//! a unit test that is true and says nothing about the thing being claimed. `Limits::exceeded`
//! proves the arithmetic. It does not prove any run path asks the question.
//!
//! So the enforcement tests here start runs **through `store::create_execution`**, which is where
//! the guard lives, and read the refusal's code. A guard that was moved out of `create_execution_in`
//! into the manual handler would pass every unit test on this branch and fail the first test here.

use omnion_workflows::TriggerKind;
use omnion_workflows::limits::{self, Limits};
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::projects::{self, NewProject, ProjectRole};
use omnion_workflows::store;
use sqlx::{PgPool, Row};
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-project-limits.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Per-test id namespace. Unique per *test*, not per entity: the suite runs concurrently against
/// ONE database, so a shared fixture makes one test's assertion another test's rows.
struct Space(Uuid);

impl Space {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
    fn id(&self, marker: u8) -> Uuid {
        let mut bytes = *self.0.as_bytes();
        bytes[14] = marker;
        bytes[15] = 0;
        Uuid::from_bytes(bytes)
    }
    fn slug(&self, prefix: &str) -> String {
        format!("{prefix}-{}", &self.0.simple().to_string()[..8])
    }
}

struct Org {
    id: Uuid,
    owner: Uuid,
    default_project: Uuid,
}

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

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Owner', 'x', now(), now())",
    )
    .bind(owner)
    .bind(org_id)
    .bind(format!("{}@example.test", &space.0.simple().to_string()[..8]))
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
    }
}

/// The five numbers a limits test writes, in the order the struct declares them.
fn caps(
    max_workflows: i32,
    max_credentials: i32,
    max_runs_per_day: i32,
    max_concurrent_runs: i32,
    updated_by: Option<Uuid>,
) -> limits::LimitOverrides {
    limits::LimitOverrides {
        max_workflows,
        max_credentials,
        max_runs_per_day,
        max_concurrent_runs,
        warn_at_percent: 80,
        updated_by,
    }
}

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

async fn add_user(pool: &PgPool, org_id: Uuid, marker: u8, space: &Space, name: &str) -> Uuid {
    let id = space.id(marker);
    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, $4, 'x', now(), now())",
    )
    .bind(id)
    .bind(org_id)
    .bind(format!("{marker}-{}@example.test", &space.0.simple().to_string()[..8]))
    .bind(name)
    .execute(pool)
    .await
    .expect("the user is inserted");
    id
}

async fn new_workflow(
    pool: &PgPool,
    org: &Org,
    project_id: Uuid,
    name: &str,
) -> omnion_workflows::model::Workflow {
    store::insert_workflow(
        pool,
        NewWorkflow {
            organization_id: org.id,
            project_id,
            site_id: None,
            name: name.into(),
            description: String::new(),
            enabled: true,
            trigger: TriggerKind::Manual,
            schedule: None,
            trigger_event: None,
            conditions: serde_json::json!([]),
            next_run_at: None,
            steps: serde_json::json!([{ "name": "go", "kind": "task", "action": "noop" }]),
            created_by: Some(org.owner),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("the workflow {name} is written: {e}"))
}

#[tokio::test]
async fn a_fresh_project_is_unlimited_rather_than_broken() {
    let pool = pool().await;
    let org = seed(&pool, "limits-default").await;

    let limits = limits::read_limits(&pool, org.default_project)
        .await
        .expect("the limits row is readable");

    // Migration 0170 gives every project a row of zeros. Reading a zero as "no runs allowed" would
    // make a fresh installation unable to start anything, which is why this is a test and not a
    // comment.
    assert_eq!(limits.max_runs_per_day, 0);
    assert!(Limits::is_unlimited(limits.max_runs_per_day));
    assert_eq!(limits.warn_at_percent, 80);

    let workflow = new_workflow(&pool, &org, org.default_project, "runs-anyway").await;
    let started = store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("an unlimited project starts a run");
    assert_eq!(started.0.status, "running");
}

#[tokio::test]
async fn the_daily_limit_is_refused_at_the_engine_boundary_by_name() {
    let pool = pool().await;
    let org = seed(&pool, "limits-daily").await;
    let project = make_project(&pool, &org, "QUOTA", "Quota").await;
    let workflow = new_workflow(&pool, &org, project, "counted").await;

    limits::set_limits(&pool, project, caps(0, 0, 2, 0, Some(org.owner)))
        .await
        .expect("the limit is set");

    // Two runs inside the cap. **Nothing here counts them by hand.** This test used to call
    // `limits::record_usage` after each run, and that line is why the gate was 14/14 green while
    // the product counted nothing: the fixture supplied the number the product was supposed to
    // produce. The cap is only real if starting a run is what spends it, so the test now starts
    // runs and reads the counter back.
    for index in 0..2 {
        store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
            .await
            .unwrap_or_else(|e| panic!("run {index} is inside the cap: {e}"));
    }
    let after_two = limits::usage_today(&pool, project).await.expect("today's counters");
    assert_eq!(
        after_two.runs, 2,
        "two started runs are two counted runs — the counter is written by the start path, not by a test"
    );

    // The third is refused, and the refusal comes from `create_execution_in` — the function all
    // four run-start paths share — rather than from the HTTP handler.
    let error = store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect_err("the third run exceeds the daily cap");
    assert_eq!(error.code(), "project_runs_per_day_exceeded");
    assert!(error.to_string().contains("QUOTA"), "names the project: {error}");
    assert!(error.to_string().contains("of 2"), "names the numbers: {error}");
    assert!(error.to_string().contains("Owner"), "names the owner: {error}");

    // …and the refusal wrote no execution row. A refusal that still counted is how a project gets
    // permanently wedged one refused run at a time.
    let runs: i64 = sqlx::query_scalar(
        "select count(*) from workflow_executions e join workflows w on w.id = e.workflow_id \
         where w.project_id = $1",
    )
    .bind(project)
    .fetch_one(&pool)
    .await
    .expect("counted");
    assert_eq!(runs, 2, "the refused run wrote no execution row");
}

#[tokio::test]
async fn raising_the_limit_lets_the_run_through_again() {
    let pool = pool().await;
    let org = seed(&pool, "limits-raise").await;
    let project = make_project(&pool, &org, "GROW", "Grow").await;
    let workflow = new_workflow(&pool, &org, project, "growing").await;

    limits::set_limits(&pool, project, caps(0, 0, 1, 0, Some(org.owner)))
        .await
        .expect("cap of one");
    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("the first run");
    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect_err("the second run is refused");

    limits::set_limits(&pool, project, caps(0, 0, 10, 0, Some(org.owner)))
        .await
        .expect("the cap is raised");
    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("the refusal is a fact about the cap, not about the project");
}

#[tokio::test]
async fn the_concurrent_limit_counts_rows_and_not_a_counter() {
    let pool = pool().await;
    let org = seed(&pool, "limits-concurrent").await;
    let project = make_project(&pool, &org, "FLAT", "Flat").await;
    let workflow = new_workflow(&pool, &org, project, "in-flight").await;

    limits::set_limits(&pool, project, caps(0, 0, 0, 1, Some(org.owner)))
        .await
        .expect("one at a time");

    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("the first run starts");

    let error = store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect_err("a second concurrent run is refused");
    assert_eq!(error.code(), "project_concurrent_runs_exceeded");

    assert_eq!(
        limits::concurrent_runs(&pool, project).await.expect("counted"),
        1,
        "the refusal did not add a second in-flight row"
    );

    // Finishing the run frees the slot — the limit is about in-flight rows, so it reads the table
    // rather than a counter that would need its own decrement.
    sqlx::query("update workflow_executions set status = 'completed', finished_at = now() where status = 'running'")
        .execute(&pool)
        .await
        .expect("the run finishes");
    assert_eq!(limits::concurrent_runs(&pool, project).await.expect("counted"), 0);
    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("a run starts again once nothing is in flight");
}

#[tokio::test]
async fn counters_agree_with_the_runs_they_count() {
    let pool = pool().await;
    let org = seed(&pool, "limits-counters").await;
    let project = make_project(&pool, &org, "METER", "Meter").await;
    let workflow = new_workflow(&pool, &org, project, "metered").await;

    // Four started runs and nothing else: this test is the acceptance line ("usage counters match
    // the underlying run records for a day"), so counting the runs itself would make it compare a
    // number with itself.
    //
    // **Two of them are then made to genuinely fail**, through the real step row and the real
    // settlement path, because `failures` is counted where the outcome is known and no other place
    // can know it. The old version of this test passed `index % 2 == 0` to `record_usage`, which
    // meant the failure counter was a value the test typed in rather than a consequence the
    // platform reached — the same compensating fixture that hid the dead writer.
    for index in 0..4 {
        let (execution, steps) = store::create_execution(
            &pool,
            &workflow,
            TriggerKind::Manual,
            Some(org.owner),
            &[omnion_workflows::definition::StepDefinition::task("go", "noop", serde_json::json!({}))],
        )
        .await
        .unwrap_or_else(|e| panic!("run {index}: {e}"));
        if index % 2 == 0 {
            let step = steps.first().expect("the run materialised its step");
            // The step has to be **running** before it can fail: `fail_step` matches on that status
            // and silently no-ops on a pending one, because in the engine the run is always claimed
            // first. Writing the status directly is the fixture standing in for the claim, and the
            // row count is asserted so the fixture cannot quietly become a no-op later.
            let claimed = sqlx::query(
                "update workflow_steps set status = 'running', started_at = now() where id = $1",
            )
            .bind(step.id)
            .execute(&pool)
            .await
            .expect("the step is claimed");
            assert_eq!(claimed.rows_affected(), 1, "the step exists to be claimed");
            store::fail_step(&pool, step.id, "the action refused")
                .await
                .expect("the step fails");
            assert_eq!(
                store::settle_execution(&pool, execution.id)
                    .await
                    .expect("the run settles"),
                Some(omnion_workflows::model::ExecutionStatus::Failed),
                "a run with a failed step settles as failed"
            );
        }
    }
    // The compute time is the one number the start path cannot know, so it is seeded directly and
    // asserted on its own rather than smuggled in through a counter call.
    sqlx::query(
        "update automation_project_usage set compute_ms = 400 where project_id = $1",
    )
    .bind(project)
    .execute(&pool)
    .await
    .expect("the compute time is seeded");

    let today = limits::usage_today(&pool, project).await.expect("today's counters");
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from workflow_executions e join workflows w on w.id = e.workflow_id \
         where w.project_id = $1 and e.started_at::date = current_date",
    )
    .bind(project)
    .fetch_one(&pool)
    .await
    .expect("counted");

    // The acceptance line is "counters match the underlying run records for a day", and the only
    // way that sentence is true is if both numbers are read on the same day from the same run set.
    assert_eq!(i64::from(today.runs), rows, "the counter equals the rows it counts");
    assert_eq!(i64::from(today.failures), 2, "two of the four were marked failed");
    assert_eq!(today.compute_ms, 400);
}

#[tokio::test]
async fn a_day_with_no_usage_reads_as_zero_rather_than_missing() {
    let pool = pool().await;
    let org = seed(&pool, "limits-zero-day").await;
    let project = make_project(&pool, &org, "FRESH", "Fresh").await;

    let today = limits::usage_today(&pool, project).await.expect("readable");
    assert_eq!(today.runs, 0, "a project created this morning has run nothing, not nothing-at-all");
    assert_eq!(today.failures, 0);
    assert_eq!(today.compute_ms, 0);
}

#[tokio::test]
async fn the_series_is_ordered_and_bounded() {
    let pool = pool().await;
    let org = seed(&pool, "limits-series").await;
    let project = make_project(&pool, &org, "TREND", "Trend").await;

    limits::record_usage(&pool, project, false, 7).await.expect("counted");
    let series = limits::usage_series(&pool, project, 30).await.expect("the series");
    assert_eq!(series.len(), 1, "one day of usage is one row");
    assert_eq!(series[0].compute_ms, 7);

    // An unbounded request is clamped rather than obeyed: the limits screen reads 30, and a
    // caller asking for 100000 is asking for a table nobody scrolls.
    let clamped = limits::usage_series(&pool, project, 100_000).await.expect("clamped");
    assert!(clamped.len() <= series.len() + 1);
}

#[tokio::test]
async fn a_negative_limit_is_refused_by_field_name() {
    let pool = pool().await;
    let org = seed(&pool, "limits-negative").await;
    let project = make_project(&pool, &org, "NEG", "Negative").await;

    let error = limits::set_limits(&pool, project, caps(-1, 0, 0, 0, None))
        .await
        .expect_err("a negative limit is refused");
    assert_eq!(error.code(), "invalid_project_limit");
    assert!(error.to_string().contains("max_workflows"), "{error}");

    let error = limits::set_limits(&pool, project, caps(0, 0, 0, 0, None).warn(0))
        .await
        .expect_err("a zero warning threshold is refused");
    assert_eq!(error.code(), "invalid_warn_threshold");
}

#[tokio::test]
async fn transferring_ownership_moves_both_facts_and_demotes_rather_than_removes() {
    let pool = pool().await;
    let org = seed(&pool, "limits-transfer").await;
    let project = make_project(&pool, &org, "HANDOVER", "Handover").await;
    let space = Space::new();
    let successor = add_user(&pool, org.id, 3, &space, "Successor").await;

    projects::upsert_member(&pool, project, successor, ProjectRole::Editor, Some(org.owner))
        .await
        .expect("the successor joins as an editor");

    let previous = limits::transfer_ownership(&pool, project, successor, Some(org.owner), org.id)
        .await
        .expect("the transfer succeeds");
    assert_eq!(previous, Some(org.owner));

    // Fact one: the project names the new owner.
    let owner: Option<Uuid> = sqlx::query_scalar("select owner_user_id from automation_projects where id = $1")
        .bind(project)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(owner, Some(successor));

    // Fact two: the membership says so too. A transfer that wrote only the column leaves a project
    // whose owner cannot administer it.
    assert_eq!(
        projects::role_of(&pool, project, successor).await.expect("read"),
        Some(ProjectRole::Owner)
    );

    // And the previous owner is an editor, not gone. Deleting their row would silently remove their
    // access as a side effect of handing the project over.
    assert_eq!(
        projects::role_of(&pool, project, org.owner).await.expect("read"),
        Some(ProjectRole::Editor),
        "the previous owner is demoted, not removed"
    );
}

#[tokio::test]
async fn promoting_a_viewer_makes_an_editor_and_not_a_useless_owner() {
    let pool = pool().await;
    let org = seed(&pool, "limits-promote-viewer").await;
    let project = make_project(&pool, &org, "PROMOTE", "Promote").await;
    let space = Space::new();
    let reader = add_user(&pool, org.id, 3, &space, "Reader").await;

    projects::upsert_member(&pool, project, reader, ProjectRole::Viewer, Some(org.owner))
        .await
        .expect("the reader joins as a viewer");

    limits::transfer_ownership(&pool, project, reader, Some(org.owner), org.id)
        .await
        .expect("the transfer succeeds");

    // **This is the assertion the compiler's "unused variable: new_role" warning was pointing at.**
    // The insert used to hardcode `'owner'`, which would have made a viewer into an owner who
    // cannot edit anything — and because the removal check counts owners, such an account also
    // blocks "remove the last owner" from ever firing for a real owner.
    assert_eq!(
        projects::role_of(&pool, project, reader).await.expect("read"),
        Some(ProjectRole::Editor),
        "a viewer promoted to owner becomes an editor, not a powerless owner"
    );
}

#[tokio::test]
async fn the_transfer_is_audited_under_its_own_action_name() {
    let pool = pool().await;
    let org = seed(&pool, "limits-transfer-audit").await;
    let project = make_project(&pool, &org, "AUDITED", "Audited").await;
    let space = Space::new();
    let successor = add_user(&pool, org.id, 3, &space, "Successor").await;

    limits::transfer_ownership(&pool, project, successor, Some(org.owner), org.id)
        .await
        .expect("the transfer succeeds");

    // Scoped to this project AND its target: the suite runs concurrently against one database, and
    // the first version fetched the newest row of that action across all projects -- so it read
    // whichever sibling test committed first and compared its ids against this test's. A test that
    // passes against another test's data is worse than one that fails.
    let row = sqlx::query(
        "select action, target_type, metadata, project_id from audit_log \
         where action = 'project_ownership.transferred' and project_id = $1 \
         order by id desc limit 1",
    )
    .bind(project)
    .fetch_one(&pool)
    .await
    .expect("the transfer wrote its own audit action, not project.updated");

    assert_eq!(row.get::<String, _>("target_type"), "automation_project");
    assert_eq!(row.get::<Uuid, _>("project_id"), project);
    let metadata = row.get::<serde_json::Value, _>("metadata");
    assert_eq!(metadata["from_user_id"], org.owner.to_string());
    assert_eq!(metadata["to_user_id"], successor.to_string());
    assert_eq!(metadata["previous_owner_role"], "editor");
}

#[tokio::test]
async fn transferring_to_the_current_owner_is_refused() {
    let pool = pool().await;
    let org = seed(&pool, "limits-transfer-same").await;
    let project = make_project(&pool, &org, "SAME", "Same").await;

    let error = limits::transfer_ownership(&pool, project, org.owner, Some(org.owner), org.id)
        .await
        .expect_err("the current owner cannot be handed the project again");
    assert_eq!(error.code(), "already_project_owner");
}

#[tokio::test]
async fn a_project_with_no_owner_can_still_be_handed_over() {
    let pool = pool().await;
    let org = seed(&pool, "limits-transfer-ownerless").await;
    let project = make_project(&pool, &org, "ORPHAN", "Orphan").await;
    let space = Space::new();
    let successor = add_user(&pool, org.id, 3, &space, "Successor").await;

    // `owner_user_id` is `on delete set null`, so a deleted owner leaves exactly this state — and
    // the first version of this function returned `project_not_found` for it, because it tested
    // the row's presence rather than reading `None` as "no owner".
    sqlx::query("update automation_projects set owner_user_id = null where id = $1")
        .bind(project)
        .execute(&pool)
        .await
        .expect("the owner is cleared");

    limits::transfer_ownership(&pool, project, successor, Some(org.owner), org.id)
        .await
        .expect("an ownerless project can be handed to somebody");

    let owner: Option<Uuid> = sqlx::query_scalar("select owner_user_id from automation_projects where id = $1")
        .bind(project)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(owner, Some(successor));
}

#[tokio::test]
async fn an_archived_project_still_refuses_runs_before_the_limit_is_consulted() {
    let pool = pool().await;
    let org = seed(&pool, "limits-archived-order").await;
    let project = make_project(&pool, &org, "FROST", "Frost").await;
    let workflow = new_workflow(&pool, &org, project, "frozen").await;

    // A project that is BOTH archived and over its cap: the archive refusal is the one that must
    // come back, because "restore it" is the action that helps and "raise the limit" is not.
    limits::set_limits(&pool, project, caps(0, 0, 1, 0, Some(org.owner)))
        .await
        .expect("cap of one");
    store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect("one run inside the cap");

    projects::set_status(&pool, project, projects::ProjectStatus::Archived)
        .await
        .expect("archived")
        .expect("row");

    let error = store::create_execution(&pool, &workflow, TriggerKind::Manual, Some(org.owner), &[])
        .await
        .expect_err("the run is refused");
    assert_eq!(
        error.code(),
        "project_archived",
        "the archive guard is checked first, and its remedy is the one that works"
    );
}
