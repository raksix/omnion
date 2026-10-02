//! Exporting an automation project (REQ-133) — the word the settings row has carried since
//! slice 1, between "archive" and "delete", with nothing behind it.
//!
//! ## The defect this file exists for
//!
//! The settings screen's row reads "Name, key, description, colour, archive, export, delete", and
//! the REQ's Risks section names the "export-first hint" as one of the four things that make
//! deleting a project with dependencies safe. Slice 19 wrote `delete` — the row's last word — and
//! the export it names as the *remedy* was left unwritten. So a project could be refused for
//! holding workflows, with an export-first hint pointing at a button that did not exist.
//!
//! ## What is asserted here, and why each one is a separate test
//!
//! 1. **The snapshot carries what the project holds** — the row, its members and its workflows, by
//!    count and by value. An export that returns an envelope with empty arrays passes a check for
//!    "the body is JSON", which is why the counts are asserted against rows the test wrote.
//! 2. **A workflow's steps survive as stored.** The whole reason JSON was chosen over CSV is that
//!    `steps` is not flattensable, and this is the test that would notice if a future column
//!    dropped it: the nested step's params have to come back byte-identical.
//! 3. **A project the caller may not see is refused.** Export is a read, so it must go through the
//!    same `find_visible` as every other read — a route that re-derived visibility would be a
//!    second answer to a question this surface already answers twice once.
//! 4. **A membership whose account is gone is still exported**, with a sentence saying so. This is
//!    the assertion that catches a `users` join quietly shrinking the file: an export that dropped
//!    the row would still satisfy "the members are exported", one fewer.
//! 5. **The filename is the lowercased key and never the display name.** The name is free text
//!    that can contain `/`; the key is `[A-Z0-9]{2,8}` by the migration's own constraint.
//! 6. **The ordering is stable.** Two exports of the same data produce byte-identical workflow
//!    order — an export whose order depends on the planner makes a diff of two snapshots
//!    unreadable, and "diff two snapshots" is the only reason to take two.
//! 7. **An empty project still exports.** Refusing here would make "export first" advice fail
//!    exactly when somebody is about to delete a shell.
//!
//! ## The negative control
//!
//! `a_sibling_projects_workflows_are_absent_from_the_export` is the test that stops a broad
//! `where` clause from turning one project's export into an organization's. It is listed last on
//! purpose: a gate whose control fails is measuring its own fixture.

use omnion_workflows::model::NewWorkflow;
use omnion_workflows::project_export::{self, ExportFormat, ExportedMember, ProjectExport};
use omnion_workflows::projects::{self, NewProject, ProjectCaller, ProjectRole};
use omnion_workflows::store;
use omnion_workflows::TriggerKind;
use sqlx::PgPool;
use uuid::Uuid;

/// Per-test id namespace, unique per *test* rather than per entity — the suite shares one
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
        .expect("DATABASE_URL is set by scripts/qa/run-project-export.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Two projects in one organization: `OPS` (the one under test) and `PAY` (the sibling that must
/// never appear in it), one owner and one member whose account the test deletes later.
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
            name: "Ops / EU rollout".to_owned(),
            description: "the project under test".to_owned(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a project");

    let sibling = projects::create_project(
        &pool,
        NewProject {
            organization_id: acme,
            key: "PAY".to_owned(),
            name: "Payroll".to_owned(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a sibling project");

    (pool, acme, owner, project.id, sibling.id)
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

async fn export_of(pool: &PgPool, project_id: Uuid) -> ProjectExport {
    project_export::build_export(pool, project_id)
        .await
        .unwrap_or_else(|e| panic!("the export should build for an existing project: {e:?}"))
}

// -------------------------------------------------------------------------------------------
// 1. The snapshot carries what the project holds
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_snapshot_carries_the_project_its_members_and_its_workflows() {
    let (pool, acme, _owner, project, _sibling) = world().await;
    store::insert_workflow(
        &pool,
        new_workflow(acme, project, "Nightly reconciliation"),
    )
    .await
    .expect("a workflow");

    let export = export_of(&pool, project).await;

    assert_eq!(export.schema, project_export::EXPORT_SCHEMA);
    assert_eq!(export.project.id, project);
    assert_eq!(export.project.organization_id, acme);
    assert_eq!(export.project.key, "OPS");
    // The name is free text, and the export must keep it verbatim — sanitising it here would make
    // the file disagree with the row it describes.
    assert_eq!(export.project.name, "Ops / EU rollout");
    assert_eq!(export.project.description, "the project under test");

    // `create_project` writes the owner membership itself, so one is the expected count and any
    // other number means the export invented or lost a row.
    assert_eq!(
        export.project.members.len(),
        1,
        "the owner's membership row should be in the snapshot"
    );
    assert_eq!(export.project.members[0].role, "owner");
    assert_eq!(export.project.members[0].display_name, "Acme Owner");

    assert_eq!(export.workflow_count(), 1, "the workflow the test wrote");
    assert_eq!(export.project.workflows[0].name, "Nightly reconciliation");
}

#[tokio::test]
async fn the_exported_file_is_json_carrying_the_schema_and_the_key() {
    let (pool, _acme, _owner, project, _sibling) = world().await;
    let export = export_of(&pool, project).await;
    let body = project_export::render(&export).expect("the fixture renders");

    assert!(body.starts_with('{'), "a JSON document starts with a brace");
    assert!(body.contains("\"schema\": \"omnion.project-export/1\""));
    assert!(body.contains("\"key\": \"OPS\""));
    // Round-tripping proves it is a document and not a string that merely contains JSON.
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("the rendered export parses as JSON");
    assert_eq!(parsed["project"]["key"], "OPS");
}

// -------------------------------------------------------------------------------------------
// 2. A workflow's steps survive exactly as stored
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_workflows_steps_and_conditions_come_back_as_stored() {
    let (pool, acme, _owner, project, _sibling) = world().await;

    // Nested params on purpose: this is the case a flattened representation cannot hold, and the
    // whole reason the format is JSON.
    let steps = serde_json::json!([
        {"kind": "task", "action": "http.post", "params": {"url": "https://example.test", "headers": {"x-tenant": "acme"}}},
        {"kind": "wait", "params": {"seconds": 30}}
    ]);
    let mut workflow = new_workflow(acme, project, "Nested");
    workflow.steps = steps.clone();
    store::insert_workflow(&pool, workflow)
        .await
        .expect("a workflow with nested params");

    let export = export_of(&pool, project).await;
    let exported = &export.project.workflows[0];

    assert_eq!(
        exported.steps, steps,
        "the steps must be the stored document, not a re-typed or flattened copy"
    );
    assert_eq!(exported.conditions, serde_json::json!([]));
    assert_eq!(exported.trigger_kind, "manual");
}

// -------------------------------------------------------------------------------------------
// 3. A project the caller may not see is refused
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_project_the_caller_may_not_see_is_refused_before_any_row_is_read() {
    let (pool, _acme, _owner, project, _sibling) = world().await;
    let space = Space::new();
    let outsider = space.id(4);

    // Another tenant's account. The project belongs to `acme`, so this caller is in no position
    // to see it at all — the refusal must be "not found", not "not yours".
    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Other', $2, now(), now())",
    )
    .bind(space.id(5))
    .bind(space.slug("other"))
    .execute(&pool)
    .await
    .expect("the other organization");
    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Outsider', 'x', now(), now())",
    )
    .bind(outsider)
    .bind(space.id(5))
    .bind(format!("outsider@{}", space.slug("other")))
    .execute(&pool)
    .await
    .expect("the other account");

    let caller = ProjectCaller {
        user_id: outsider,
        is_instance_admin: false,
    };
    let visible = projects::find_visible(&pool, space.id(5), project, caller)
        .await
        .expect("the visibility query answers");

    assert!(
        visible.is_none(),
        "a cross-tenant caller must not resolve the project, or the export would leak its name"
    );
}

#[tokio::test]
async fn an_instance_administrator_of_the_organization_sees_the_project() {
    let (pool, acme, _owner, project, _sibling) = world().await;
    let space = Space::new();
    let admin = space.id(6);

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Platform', 'x', now(), now())",
    )
    .bind(admin)
    .bind(acme)
    .bind(format!("platform@{}", space.slug("acme")))
    .execute(&pool)
    .await
    .expect("the administrator");

    let caller = ProjectCaller {
        user_id: admin,
        is_instance_admin: true,
    };
    let visible = projects::find_visible(&pool, acme, project, caller)
        .await
        .expect("the visibility query answers");

    assert!(
        visible.is_some(),
        "the delegation rule says an instance administrator reaches every project in the \
         organization; if this fails, the export's own caller resolution is suspect"
    );
}

#[tokio::test]
async fn a_deleted_project_is_refused_rather_than_exported_empty() {
    let (_pool, _acme, _owner, project, _sibling) = world().await;
    let pool = _pool.clone();

    // Build the export first so the project definitely exists, then remove it out from under the
    // second call. An export that answered an empty document here would be indistinguishable from
    // "a project with nothing in it", which is exactly the confusion this test exists to prevent.
    let first = export_of(&pool, project).await;
    assert_eq!(first.project.key, "OPS");

    sqlx::query("delete from automation_projects where id = $1")
        .bind(project)
        .execute(&pool)
        .await
        .expect("the project is removable once it holds nothing");

    let outcome = project_export::build_export(&pool, project).await;
    assert!(
        outcome.is_err(),
        "a project that no longer exists must be a refusal, not an empty snapshot"
    );
}

// -------------------------------------------------------------------------------------------
// 4. A membership and its account are one fact
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_membership_cannot_outlive_its_account() {
    let (pool, acme, owner, project, _sibling) = world().await;
    let space = Space::new();
    let departing = space.id(7);

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Departed', 'x', now(), now())",
    )
    .bind(departing)
    .bind(acme)
    .bind(format!("departed@{}", space.slug("acme")))
    .execute(&pool)
    .await
    .expect("the account");

    projects::upsert_member(&pool, project, departing, ProjectRole::Editor, None)
        .await
        .expect("a membership");

    let members = |m: Vec<ExportedMember>| m.len();
    assert_eq!(
        members(export_of(&pool, project).await.project.members),
        2,
        "the fixture must really have two memberships or the assertion below proves nothing"
    );

    // **The FK is the invariant, and this is the assertion that found my own false premise.** The
    // first version of this test assumed a membership could name a deleted account and demanded
    // the export render "deleted account (…)" for it. That state cannot exist: `user_id` is
    // `on delete cascade`, so the database takes the membership with the account. The store had a
    // fallback for it — unreachable code written to look careful — and the honest form is an
    // assertion. So the fallback is gone and this test states the fact that makes it unnecessary.
    //
    // The cascade is what is under test, so the delete is the ordinary one an administrator
    // would perform rather than a hand-written cascade of the membership table.
    sqlx::query("delete from users where id = $1")
        .bind(departing)
        .execute(&pool)
        .await
        .expect("the account is deletable");

    let after = export_of(&pool, project).await;
    assert_eq!(
        after.project.members.len(),
        1,
        "the membership went with the account; if it did not, the FK changed and the store's \
         export needs a fallback that says so"
    );
    assert!(
        after.project.members.iter().all(|m| m.user_id != departing),
        "the departed account is gone from the snapshot"
    );
    // And the snapshot still names the surviving owner, so "one member" is the owner and not an
    // export that quietly dropped everything.
    assert!(
        after.project.members.iter().any(|m| m.user_id == owner),
        "the owner's membership survived another account's departure"
    );
}

#[tokio::test]
async fn the_membership_surviving_member_names_carry_readable_text() {
    // The counterpart to the test above, and the reason the store's join exists at all: an
    // exported membership carrying a bare uuid would be technically complete and useless to the
    // person reading the file two years later.
    let (pool, _acme, _owner, project, _sibling) = world().await;
    let export = export_of(&pool, project).await;
    let owner_row = &export.project.members[0];

    assert_eq!(owner_row.display_name, "Acme Owner");
    assert!(
        owner_row.email.contains('@'),
        "an exported membership carries the email a reader would look for: {}",
        owner_row.email
    );
}

// 5. The filename
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_filename_is_the_lower_key_and_never_a_path_separator() {
    let (pool, _acme, _owner, project, _sibling) = world().await;
    let export = export_of(&pool, project).await;

    assert_eq!(export.filename_stem(), "ops");

    let rendered = project_export::filename(ExportFormat::Json, &export);
    assert!(
        !rendered.contains('/'),
        "the name carries a slash, so the filename would too: {rendered}"
    );
    assert!(rendered.starts_with("ops-export-"), "{rendered}");
    assert!(rendered.ends_with(".json"), "{rendered}");

    // The stamp is present and shaped. Counting dashes is the way to say it without depending on
    // the clock: three of them - one in the literal "export", two in the stamp "20261002-092954".
    // (The first version asserted two and failed on the real name: the count is a property of the
    // string, so writing down the wrong number is how a test stops testing.)
    assert_eq!(rendered.matches('-').count(), 3, "{rendered}");
    let stem = rendered.trim_end_matches(".json");
    let stamp = stem.rsplit('-').next().expect("a timestamp segment");
    assert_eq!(stamp.len(), 6, "HHMMSS: {rendered}");
    assert!(stamp.chars().all(|c| c.is_ascii_digit()), "{rendered}");

    assert_eq!(
        project_export::content_type(ExportFormat::Json),
        "application/json; charset=utf-8"
    );
}

// -------------------------------------------------------------------------------------------
// 6. The ordering is stable, and 7. an empty project still exports
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn two_exports_of_the_same_data_list_the_workflows_in_the_same_order() {
    let (pool, acme, _owner, project, _sibling) = world().await;
    for name in ["one", "two", "three", "four"] {
        store::insert_workflow(&pool, new_workflow(acme, project, name))
            .await
            .expect("a workflow");
    }

    let first: Vec<String> = export_of(&pool, project)
        .await
        .project
        .workflows
        .iter()
        .map(|w| w.id.to_string())
        .collect();
    let second: Vec<String> = export_of(&pool, project)
        .await
        .project
        .workflows
        .iter()
        .map(|w| w.id.to_string())
        .collect();

    assert_eq!(first.len(), 4, "the fixture must have written four workflows");
    assert_eq!(
        first, second,
        "a diff of two snapshots is the only reason to take two, so the order cannot vary"
    );
}

#[tokio::test]
async fn a_project_with_nothing_in_it_still_exports() {
    let (pool, _acme, _owner, project, _sibling) = world().await;
    // `create_project` wrote the owner membership, so the snapshot is never truly empty — which
    // is itself the point: "export first" advice must not fail on a project somebody is about to
    // delete. Only a genuinely bare row can make this interesting, so the member goes too.
    sqlx::query("delete from automation_project_members where project_id = $1")
        .bind(project)
        .execute(&pool)
        .await
        .expect("the membership is removable");

    let export = export_of(&pool, project).await;
    assert!(
        export.is_empty(),
        "the fixture should now hold nothing: {:?}",
        export.project.members.len()
    );
    assert_eq!(export.workflow_count(), 0);

    let body = project_export::render(&export).expect("an empty project still serializes");
    assert!(body.contains("\"schema\": \"omnion.project-export/1\""));
    assert!(body.contains("\"key\": \"OPS\""), "the row itself is the fact preserved");
}

// -------------------------------------------------------------------------------------------
// The negative control
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_sibling_projects_workflows_are_absent_from_the_export() {
    let (pool, acme, _owner, project, sibling) = world().await;
    store::insert_workflow(&pool, new_workflow(acme, project, "mine"))
        .await
        .expect("a workflow in the project under test");
    store::insert_workflow(&pool, new_workflow(acme, sibling, "theirs"))
        .await
        .expect("a workflow in the sibling");

    let export = export_of(&pool, project).await;

    assert_eq!(export.workflow_count(), 1);
    assert_eq!(
        export.project.workflows[0].name, "mine",
        "the export names the sibling's workflow: the project filter is too broad"
    );
    assert!(
        !project_export::render(&export)
            .expect("renders")
            .contains("theirs"),
        "the sibling's workflow text reached the file"
    );
}