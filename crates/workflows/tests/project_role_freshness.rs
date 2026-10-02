//! Project role freshness (REQ-133, acceptance 6) — against a real database.
//!
//! ## The sentence under test
//!
//! *"Project member role changes take effect **without re-login**: the very next request reflects
//! the new role, **in both directions (grant and revoke)**."*
//!
//! ## Why this is not a unit test, and why it is not about caching
//!
//! The REQ's own risk note names the mechanism it expected: *"the cache key includes a membership
//! revision, and a test proves grant and revoke both bite on the next request."* Checked rather
//! than assumed before writing a line of this file: **nothing on this branch caches a role.**
//! `role_of` is a single indexed primary-key lookup on `automation_project_members`, and
//! `crates/permissions/src/groups.rs` says it outright — "resolution never caches". So there is
//! no revision to add and nothing to invalidate, and a test that only proved "the row changed"
//! would be testing the migration.
//!
//! What was actually missing was the opposite: `role_of` existed, was correct, was unit-tested,
//! and **no write path ever called it**. `PUT /workflows/{id}` reached the store through
//! `workflow_in_scope`, which answers *"may this caller see this row"* — a question every member
//! of a project answers yes to. A `viewer` could therefore replace, delete and run. The rule was
//! right and unreachable, and the acceptance line was false on the branch.
//!
//! So the property this file proves is not "a cache gets invalidated". It is the sentence the line
//! actually makes, in both directions, on one session:
//!
//! > the *next* call to the decision function sees the row as it is now, not as it was when the
//! > session was created.
//!
//! ## The three ways this test could lie, and what closes each
//!
//! 1. **It could pass because the session was re-created between the two assertions.** Every test
//!    here takes its `user_id` once and mints the membership around it; the caller struct is built
//!    from the same id, so there is no "after the change" identity to differ from "before".
//! 2. **It could pass in one direction only.** Both directions are asserted, in the same test, on
//!    the same project — a guard that memoised per session would bite on the grant and stay silent
//!    on the revoke.
//! 3. **It could prove nothing because no membership ever existed.** Every positive assertion first
//!    asserts the *refusal*, so "allowed" can never be vacuously true, and the fixture reads the
//!    row back with `role_of` before the second half.
//!
//! ## The negative case is the load-bearing one
//!
//! `effective_role` for a caller who is **not** a member of a **non-default** project answers
//! `None`, and `None` may do nothing. Without that assertion a suite could pass with an
//! `effective_role` that answers `Owner` for everybody, which is the failure that would make every
//! project in a tenant writable by any account in it.

use omnion_workflows::model::NewWorkflow;
use omnion_workflows::projects::{self, ProjectCaller, ProjectRole};
use omnion_workflows::{TriggerKind, store};
use sqlx::PgPool;
use uuid::Uuid;

/// Connect, or say why the gate did not run.
///
/// A panic with the reason is what a failing fixture should look like: a helper returning `None`
/// and letting each test skip produces a suite that reports zero tests and reads as a pass.
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-project-role-freshness.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// One organization, two accounts, one project.
///
/// The accounts are `owner` (the project's first owner, who can change membership) and
/// `subject` (the account whose role is moved around). They are created *without* a membership row
/// on purpose: the grant is the test's first write, so a fixture that pre-created the membership
/// would prove only the revoke.
struct World {
    pool: PgPool,
    organization_id: Uuid,
    owner: Uuid,
    subject: Uuid,
    project: Uuid,
}

/// Per-test id space.
///
/// Unique per **test**, not per entity within a test — the suite runs concurrently against one
/// database, and a namespace that is unique per entity makes two tests share an organization and
/// an owner, which reads as a genuine isolation failure and is two lines of arithmetic.
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

async fn world() -> World {
    let pool = pool().await;
    let space = Space::new();

    let organization_id = space.id(1);
    let owner = space.id(2);
    let subject = space.id(3);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Freshness Co', $2, now(), now())",
    )
    .bind(organization_id)
    .bind(space.slug("freshness"))
    .execute(&pool)
    .await
    .expect("one organization");

    for (id, email, name) in [
        (owner, format!("owner@{}", space.slug("freshness")), "Owner"),
        (subject, format!("who@{}", space.slug("freshness")), "Subject"),
    ] {
        sqlx::query(
            "insert into users (id, organization_id, email, display_name, password_hash, \
             created_at, updated_at) values ($1, $2, $3, $4, 'x', now(), now())",
        )
        .bind(id)
        .bind(organization_id)
        .bind(email)
        .bind(name)
        .execute(&pool)
        .await
        .expect("an account");
    }

    let project = projects::create_project(
        &pool,
        projects::NewProject {
            organization_id,
            key: "TEAM".to_owned(),
            name: "Team".to_owned(),
            description: "Role freshness".to_owned(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a project");

    World {
        pool,
        organization_id,
        owner,
        subject,
        project: project.id,
    }
}

fn caller(user_id: Uuid) -> ProjectCaller {
    ProjectCaller {
        user_id,
        is_instance_admin: false,
    }
}

/// Whether this caller may `capability` in the project — the shape a route handler asks.
async fn permits(
    world: &World,
    user_id: Uuid,
    capability: fn(ProjectRole) -> bool,
) -> (bool, Option<ProjectRole>) {
    projects::permits(
        &world.pool,
        world.organization_id,
        world.project,
        caller(user_id),
        capability,
    )
    .await
    .expect("the capability lookup must answer")
}

/// Insert a workflow in the project, so the row a caller acts on exists independently of roles.
async fn seed_workflow(world: &World, name: &str) -> Uuid {
    let workflow = store::insert_workflow(
        &world.pool,
        NewWorkflow {
            organization_id: world.organization_id,
            project_id: world.project,
            site_id: None,
            name: name.to_owned(),
            description: String::new(),
            enabled: true,
            trigger: TriggerKind::Manual,
            schedule: None,
            trigger_event: None,
            conditions: serde_json::json!([]),
            next_run_at: None,
            steps: serde_json::json!([]),
            created_by: Some(world.owner),
        },
    )
    .await
    .expect("a workflow");
    workflow.id
}

/// **The acceptance sentence, both directions, on one caller and one session.**
///
/// Four decisions, one project, one account, and the only thing that changes between them is a
/// membership row:
/// 1. no membership      → refused (so every later "allowed" is non-vacuous)
/// 2. granted `operator` → `can_run` allowed, `can_edit` still refused
/// 3. revoked            → `can_run` refused again
/// 4. granted `editor`   → `can_edit` allowed
///
/// Step 2 is the one that carries the sentence: an `operator` may run but not edit, so it proves
/// the role was *read* rather than merely "is a member" — a guard that treated any member as
/// allowed would pass 1, 3 and 4 and fail 2. Step 3 is the mirror the acceptance line names.
#[tokio::test]
async fn a_role_change_bites_on_the_next_decision_in_both_directions() {
    let world = world().await;

    // 1 — no membership row at all. `role_of` says none, `effective_role` says none, and a `None`
    // role may do nothing. Without this the three assertions below could all be vacuous.
    let (can_run, role) = permits(&world, world.subject, ProjectRole::can_run).await;
    assert_eq!(role, None, "a non-member of a non-default project is nobody");
    assert!(!can_run, "a caller with no role may not run anything");

    let (can_edit, role) = permits(&world, world.subject, ProjectRole::can_edit).await;
    assert_eq!(role, None);
    assert!(!can_edit, "a caller with no role may not edit anything");

    // 2 — the grant. One write, and the very next decision reads it.
    projects::upsert_member(
        &world.pool,
        world.project,
        world.subject,
        ProjectRole::Operator,
        Some(world.owner),
    )
    .await
    .expect("the grant must land");

    // Read the row back through the OTHER function, so a broken `upsert_member` cannot make this
    // test green by agreeing with itself.
    assert_eq!(
        projects::role_of(&world.pool, world.project, world.subject)
            .await
            .expect("role_of must answer"),
        Some(ProjectRole::Operator),
        "the membership row really says operator"
    );

    let (can_run, role) = permits(&world, world.subject, ProjectRole::can_run).await;
    assert_eq!(role, Some(ProjectRole::Operator), "the next decision sees the new role");
    assert!(can_run, "an operator may start a run — the very next request, no re-login");

    let (can_edit, role) = permits(&world, world.subject, ProjectRole::can_edit).await;
    assert_eq!(role, Some(ProjectRole::Operator));
    assert!(
        !can_edit,
        "an operator may NOT edit — a guard that only asked 'is a member' would pass this wrongly"
    );

    // 3 — the revoke, the mirror the acceptance line names. A per-session memo bites on the grant
    // and stays silent here, which is why the acceptance line says "in both directions".
    assert!(
        projects::remove_member(&world.pool, world.project, world.subject)
            .await
            .expect("the revoke must be answered"),
        "the membership row must go"
    );
    assert_eq!(
        projects::role_of(&world.pool, world.project, world.subject)
            .await
            .expect("role_of must answer"),
        None,
        "the row is gone from the table, not merely from a cache"
    );

    let (can_run, role) = permits(&world, world.subject, ProjectRole::can_run).await;
    assert_eq!(role, None, "the revoke bites on the next decision");
    assert!(!can_run, "a revoked caller may not run again — same account, same project");

    // 4 — and a different grant lands on the next request too, so the rule is not
    // "one write then stuck".
    projects::upsert_member(
        &world.pool,
        world.project,
        world.subject,
        ProjectRole::Editor,
        Some(world.owner),
    )
    .await
    .expect("the second grant must land");

    let (can_edit, role) = permits(&world, world.subject, ProjectRole::can_edit).await;
    assert_eq!(role, Some(ProjectRole::Editor));
    assert!(can_edit, "an editor may edit on the very next decision");

    let _ = seed_workflow(&world, "role freshness").await;
}

/// A re-role *in place* — same row, new role — also bites immediately.
///
/// This is the shape the members screen actually writes (`upsert_member` on an existing row), and
/// it is a different statement from the grant-and-revoke pair above: nothing was inserted and
/// nothing was deleted, so a guard reading a stale *join* rather than a stale cache would still
/// get this wrong.
#[tokio::test]
async fn changing_a_role_in_place_bites_on_the_next_decision() {
    let world = world().await;

    projects::upsert_member(
        &world.pool,
        world.project,
        world.subject,
        ProjectRole::Viewer,
        Some(world.owner),
    )
    .await
    .expect("the first grant must land");

    let (can_run, role) = permits(&world, world.subject, ProjectRole::can_run).await;
    assert_eq!(role, Some(ProjectRole::Viewer));
    assert!(!can_run, "a viewer may not start a run");

    // The same row, re-roled. No insert, no delete.
    projects::upsert_member(
        &world.pool,
        world.project,
        world.subject,
        ProjectRole::Operator,
        Some(world.owner),
    )
    .await
    .expect("the re-role must land");

    let (can_run, role) = permits(&world, world.subject, ProjectRole::can_run).await;
    assert_eq!(
        role,
        Some(ProjectRole::Operator),
        "the re-role is visible to the very next decision"
    );
    assert!(can_run, "operator now may run — the re-role bit");
}

/// The **default project** is visible to every account in the tenant without a membership row, so
/// it is the one project where "not a member" has to answer *something*, and the answer comes from
/// the project's own column.
///
/// This is migration 0181's whole SQL half, and it is asserted by behaviour rather than by reading
/// the column: a `viewer` by default (may not run), then the column is changed to `operator` and the
/// same account, with **no membership row written**, may run.
#[tokio::test]
async fn the_default_projects_own_role_decides_what_a_non_member_may_do() {
    let pool = pool().await;
    let space = Space::new();
    let organization_id = space.id(1);
    let subject = space.id(2);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Defaults Co', $2, now(), now())",
    )
    .bind(organization_id)
    .bind(space.slug("defaults"))
    .execute(&pool)
    .await
    .expect("one organization");

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Subject', 'x', now(), now())",
    )
    .bind(subject)
    .bind(organization_id)
    .bind(format!("who@{}", space.slug("defaults")))
    .execute(&pool)
    .await
    .expect("an account");

    let default = projects::default_project(&pool, organization_id)
        .await
        .expect("the organization has a default project");

    let permits_in_default = |capability: fn(ProjectRole) -> bool| {
        let pool = pool.clone();
        async move {
            projects::permits(
                &pool,
                organization_id,
                default.id,
                caller(subject),
                capability,
            )
            .await
            .expect("the capability lookup must answer")
        }
    };

    // No membership row exists for `subject` anywhere in this test, and never will.
    let (can_run, role) = permits_in_default(ProjectRole::can_run).await;
    assert_eq!(
        role,
        Some(ProjectRole::Viewer),
        "a non-member of the default project is a viewer, not nobody and not owner"
    );
    assert!(!can_run, "a viewer may not start a run in the default project");

    let (can_edit, _) = permits_in_default(ProjectRole::can_edit).await;
    assert!(!can_edit, "nor edit one");

    // The column is read, not decoration: change it and the next decision follows.
    sqlx::query("update automation_projects set default_member_role = 'operator' where id = $1")
        .bind(default.id)
        .execute(&pool)
        .await
        .expect("the project's own default role must be settable");

    let (can_run, role) = permits_in_default(ProjectRole::can_run).await;
    assert_eq!(role, Some(ProjectRole::Operator), "the column answers, per call");
    assert!(can_run, "with the column at operator, a non-member may run — still no membership row");

    let (can_edit, _) = permits_in_default(ProjectRole::can_edit).await;
    assert!(!can_edit, "operator is still not editor: the matrix, not the column, decides");
}

/// The instance administrator's answer is unchanged by any of this.
///
/// A guard written for acceptance 6 has an obvious way to over-reach: make `effective_role` strict
/// everywhere, and the one account that exists to reach every project starts getting `404`. This
/// asserts the short-circuit is still there — and it is the test that fails first if someone
/// deletes the `is_instance_admin` arm.
#[tokio::test]
async fn the_instance_administrator_is_still_an_owner_after_the_stricter_lookup() {
    let world = world().await;

    let admin = ProjectCaller {
        user_id: world.subject,
        is_instance_admin: true,
    };
    let (allowed, role) = projects::permits(
        &world.pool,
        world.organization_id,
        world.project,
        admin,
        ProjectRole::can_administer,
    )
    .await
    .expect("the capability lookup must answer");

    assert_eq!(role, Some(ProjectRole::Owner));
    assert!(
        allowed,
        "an instance administrator administers a project they are not a member of"
    );

    // And `role_of` — the *membership* question — still says "not a member", because the
    // last-owner check reads it and must not count an administrator who has no row.
    assert_eq!(
        projects::role_of(&world.pool, world.project, world.subject)
            .await
            .expect("role_of must answer"),
        None,
        "the administrator has no membership row, and this answer is not changed by this tick"
    );
}

/// The owner's own powers survive, which is the boring half that a too-strict guard breaks first.
///
/// Every rule added for acceptance 6 has a failure mode of refusing too much. This is the
/// counterweight: an `owner` may edit, run and administer, and the row it creates is a row the
/// store can see — so a regression that made every capability fail closed would be caught here
/// rather than in production.
#[tokio::test]
async fn an_owner_keeps_every_capability_the_matrix_grants() {
    let world = world().await;

    // The array is annotated rather than inferred: six *fn items* with the same signature are six
    // different types to the compiler, so the first element would silently decide the array's type
    // and the other five would not coerce. `cargo` names it (`consider casting both fn items to
    // fn pointers`) and it is a fixture mistake, not a product one.
    let capabilities: [(fn(ProjectRole) -> bool, &str); 6] = [
        (ProjectRole::can_edit, "edit"),
        (ProjectRole::can_run, "run"),
        (ProjectRole::can_administer, "administer"),
        (ProjectRole::can_manage_members, "manage members"),
        (ProjectRole::can_manage_credentials, "manage credentials"),
        (ProjectRole::can_read, "read"),
    ];
    for (capability, name) in capabilities {
        let (allowed, role) = permits(&world, world.owner, capability).await;
        assert_eq!(role, Some(ProjectRole::Owner), "{name}: the owner is the owner's role");
        assert!(allowed, "an owner may {name}");
    }
}
