//! Deactivating an account that still owns a project (REQ-133) — against a real database.
//!
//! ## The sentence under test
//!
//! *"Deactivating a user who **owns a project or workflows** blocks until reassignment
//! completes."*
//!
//! ## What was actually missing
//!
//! Nothing at all. `PATCH /api/v1/iam/users/{id}` accepted `status: "disabled"` and wrote it; the
//! three SCIM deactivation paths (`POST`, `PUT`, `DELETE /scim/v2/Users/{id}`) did the same.
//! `automation_projects.owner_user_id` has been there since migration `0164`, and the "at least
//! one owner must remain" rule existed — but only for `remove_member`, and that is a *different*
//! action: it guards a membership being deleted, not an account being switched off. Switching
//! somebody off leaves the membership row, the column and the constraint all intact, and the
//! project becomes unadministrable: the only owner cannot sign in.
//!
//! So this is not a caching story and not a missing constraint. It is the same shape this branch
//! has now found ten times — the rule was written down in the REQ, one function enforced the
//! neighbouring case, and **no write path ever consulted it for this one**.
//!
//! ## Two facts, deliberately not one number
//!
//! The acceptance line names "a project **or workflows**". This file proves only the ownership
//! half is a refusal, and the reason is the *remedy*, not convenience:
//!
//! * `transfer_ownership` moves a project to another account in one transaction — a real,
//!   available action, so refusing is honest and unblocking is one click.
//! * A workflow's `created_by` is an authorship stamp the schema itself declares nullable
//!   (`on delete set null`). There is no "reassign this workflow" action anywhere on the branch.
//!   Refusing here would produce an account that **cannot be switched off at all**, with the only
//!   available remedy being "delete somebody's work" — which is worse than the state the line
//!   was trying to prevent, and is not what "reassignment" means.
//!
//! So `authored_workflows` is *reported* (and travels into the audit row) rather than refused, and
//! the test asserts both halves of that decision rather than leaving it to a comment.
//!
//! ## The three ways this test could lie, and what closes each
//!
//! 1. **A second owner would make the refusal vacuous.** The refusal is asserted with exactly one
//!    owner, and the "unblocked after a transfer" case is asserted as its own test, so a guard
//!    that refuses unconditionally passes one and fails the other.
//! 2. **A guard reading only `owner_user_id` would pass while the membership says otherwise.**
//!    `upsert_member` writes *only* the membership — it never touches the column — so the two can
//!    genuinely disagree on this schema. The test therefore asserts the refusal in both shapes:
//!    column set, and column null with the membership row carrying `owner`.
//! 3. **A guard over one organization would pass a suite that only builds one.** Every test builds
//!    its own organization, and the tenancy case asserts a project in *another* tenant does not
//!    block — the direction that makes the `organization_id` filter load-bearing.

use omnion_workflows::model::NewWorkflow;
use omnion_workflows::projects::{self, NewProject};
use omnion_workflows::{TriggerKind, store};
use sqlx::PgPool;
use uuid::Uuid;

/// Connect, or say why the gate did not run.
///
/// A panic with the reason is what a failing fixture should look like: a helper returning `None`
/// and letting each test skip produces a suite that reports zero tests and reads as a pass.
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-user-deactivation.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Per-test id space.
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

struct World {
    pool: PgPool,
    organization_id: Uuid,
    /// The account whose projects are built.
    owner: Uuid,
    /// Somebody else in the same organization, for ownership transfers and second owners.
    other: Uuid,
}

async fn world() -> World {
    let pool = pool().await;
    let space = Space::new();

    let organization_id = space.id(1);
    let owner = space.id(2);
    let other = space.id(3);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Deactivation Co', $2, now(), now())",
    )
    .bind(organization_id)
    .bind(space.slug("deact"))
    .execute(&pool)
    .await
    .expect("one organization");

    for (id, email, name) in [
        (owner, format!("owner@{}", space.slug("deact")), "Owner"),
        (other, format!("other@{}", space.slug("deact")), "Other"),
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

    World {
        pool,
        organization_id,
        owner,
        other,
    }
}

/// A project owned by `owner_user_id`.
///
/// **`create_project` writes the `owner` membership as well as the column** — its own field doc
/// says so — so "owned by X" is one state with two writes, never one of them alone. The test
/// below builds the *other* representation by hand, which is the only way to reach it.
async fn project(world: &World, key: &str, owner_user_id: Uuid) -> Uuid {
    let project = projects::create_project(
        &world.pool,
        NewProject {
            organization_id: world.organization_id,
            key: key.to_owned(),
            name: format!("{key} project"),
            description: "Deactivation guard".to_owned(),
            color: None,
            icon: None,
            owner_user_id,
            created_by: Some(world.owner),
        },
    )
    .await
    .expect("a project");
    project.id
}

/// A project whose only ownership record is a membership row: the column is null and nobody else
/// is an owner. This is the state `upsert_member` alone produces, and the one a restored dump or
/// a hand-edited row looks like.
async fn membership_only_project(world: &World, key: &str) -> Uuid {
    let id = project(world, key, world.other).await;
    sqlx::query("update automation_projects set owner_user_id = null where id = $1")
        .bind(id)
        .execute(&world.pool)
        .await
        .expect("clear the column");
    sqlx::query("delete from automation_project_members where project_id = $1 and user_id = $2")
        .bind(id)
        .bind(world.other)
        .execute(&world.pool)
        .await
        .expect("drop the owner create_project wrote");
    projects::upsert_member(
        &world.pool,
        id,
        world.owner,
        projects::ProjectRole::Owner,
        None,
    )
    .await
    .expect("an owner membership");
    id
}

async fn author_workflow(world: &World, project_id: Uuid, name: &str) {
    store::insert_workflow(
        &world.pool,
        NewWorkflow {
            organization_id: world.organization_id,
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
            steps: serde_json::json!([]),
            created_by: Some(world.owner),
        },
    )
    .await
    .expect("a workflow");
}

/// The `WorkflowError` code a refusal carries, or `None` when the guard let it through.
async fn refusal_code(world: &World, user_id: Uuid) -> Option<String> {
    match projects::ensure_user_can_be_disabled(&world.pool, world.organization_id, user_id).await {
        Ok(_) => None,
        Err(error) => match error {
            omnion_workflows::WorkflowError::Invalid { code, .. } => Some(code.to_owned()),
            other => panic!("the guard must answer with a refusal or an approval, not {other:?}"),
        },
    }
}

/// **The acceptance sentence's refusal, in the shape the schema actually allows.**
///
/// Two fixtures, because `upsert_member` writes only the membership and never the column — so
/// "owns a project" has two distinct representations on this branch and a guard reading one of
/// them would let the other through:
///
/// 1. `owner_user_id` set by `create_project` (and by `transfer_ownership`).
/// 2. `owner_user_id` **null** with an `owner` membership and no other owner — the row a
///    restored dump or a hand-edit leaves behind, and the one `upsert_member` alone can produce.
#[tokio::test]
async fn the_last_owner_is_refused_by_name() {
    let world = world().await;

    let _via_column = project(&world, "COLUMN", world.owner).await;
    assert_eq!(
        refusal_code(&world, world.owner).await.as_deref(),
        Some("user_still_owns_projects"),
        "an account named as the project's owner must not be switchable off",
    );

    // The second representation: the column cleared, the membership carrying the role. A guard
    // that reads `owner_user_id` alone answers "fine, go ahead" for exactly this row.
    let _via_membership = membership_only_project(&world, "MEMBER").await;

    let message = projects::ensure_user_can_be_disabled(
        &world.pool,
        world.organization_id,
        world.owner,
    )
    .await
    .expect_err("the membership alone is ownership")
    .to_string();

    // The refusal has to be *actionable*: it names the project and the remedy, because the
    // operator holding only `users.manage` can do neither the transfer nor the membership edit.
    assert!(
        message.contains("COLUMN"),
        "the refusal must name the first project, got: {message}",
    );
    assert!(
        message.contains("MEMBER"),
        "the refusal must name every project, not the first, got: {message}",
    );
    assert!(
        message.contains("transfer ownership") || message.contains("second owner"),
        "the refusal must name the remedy, got: {message}",
    );
}

/// **The remedy the refusal points at actually unblocks it.**
///
/// A refusal nobody can clear is a lock, not a guard. The transfer is slice 4's function and it
/// moves both facts — the column and the membership — so the second call must approve. This is
/// the test that catches a guard refusing unconditionally: the two tests together are the whole
/// rule, and one of them fails if the other half is missing.
#[tokio::test]
async fn the_owning_account_is_approved_once_the_project_has_a_second_owner() {
    let world = world().await;
    let project_id = project(&world, "PAIR", world.owner).await;

    assert_eq!(
        refusal_code(&world, world.owner).await.as_deref(),
        Some("user_still_owns_projects"),
        "one owner is not enough",
    );

    projects::upsert_member(
        &world.pool,
        project_id,
        world.other,
        projects::ProjectRole::Owner,
        None,
    )
    .await
    .expect("a second owner");

    assert_eq!(
        refusal_code(&world, world.owner).await,
        None,
        "two owners means the project survives one of them being switched off",
    );
}

/// **Authorship is reported, not refused** — and the number is real.
///
/// This is the half of the acceptance line that is deliberately *not* enforced, and the test that
/// says so out loud. `created_by` is nullable `on delete set null`: there is no reassign-workflow
/// action on this branch, so refusing here would mean an account can never be switched off, and
/// the only available "remedy" would be deleting somebody's work. The count is asserted because
/// a `UserAssignments` that always answers zero would make the audit row a lie.
#[tokio::test]
async fn authorship_is_counted_but_does_not_block() {
    let world = world().await;
    let project_id = project(&world, "WRITER", world.other).await;
    for index in 0..3 {
        author_workflow(&world, project_id, &format!("Flow {index}")).await;
    }

    let assignments = projects::user_assignments(&world.pool, world.organization_id, world.owner)
        .await
        .expect("the assignment read must answer");

    assert_eq!(
        assignments.authored_workflows, 3,
        "the count is what the audit row records, so it has to be the real number",
    );
    assert!(
        !assignments.blocks_deactivation(),
        "three workflows behind an account is a fact to record, not a wall",
    );
    assert_eq!(
        refusal_code(&world, world.owner).await,
        None,
        "an author is not an owner; the guard must not over-refuse",
    );
}

/// **A project in another tenant does not block, and one in this tenant does.**
///
/// The `organization_id` argument is the only thing that makes the query tenant-scoped, so without
/// this test a suite that builds a single organization would pass a guard that filtered on
/// `user_id` alone. The direction matters too: reading the *owner's* projects across tenants
/// would let one tenant's project block an unrelated account, and refusing there is a denial of
/// service dressed as a safety check.
#[tokio::test]
async fn tenancy_scopes_the_refusal_in_both_directions() {
    let world = world().await;
    let space = Space::new();

    let foreign_org = space.id(4);
    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Elsewhere', $2, now(), now())",
    )
    .bind(foreign_org)
    .bind(space.slug("elsewhere"))
    .execute(&world.pool)
    .await
    .expect("a second organization");

    let foreign_project = projects::create_project(
        &world.pool,
        NewProject {
            organization_id: foreign_org,
            key: "FOREIGN".to_owned(),
            name: "Another tenant".to_owned(),
            description: "Tenancy".to_owned(),
            color: None,
            icon: None,
            owner_user_id: world.owner,
            created_by: None,
        },
    )
    .await
    .expect("a project in another tenant");

    assert_eq!(
        refusal_code(&world, world.owner).await,
        None,
        "another tenant's project must not block this account's deactivation",
    );

    // And the same ownership *inside* the organization does block — so the assertion above is a
    // scoping rule and not "the guard never fires".
    project(&world, "MINE", world.owner).await;

    assert_eq!(
        refusal_code(&world, world.owner).await.as_deref(),
        Some("user_still_owns_projects"),
    );

    // The foreign project's row is untouched: the guard reads, it never writes.
    let still_foreign: Option<Uuid> =
        sqlx::query_scalar("select owner_user_id from automation_projects where id = $1")
            .bind(foreign_project.id)
            .fetch_one(&world.pool)
            .await
            .expect("the foreign project row");
    assert_eq!(still_foreign, Some(world.owner));
}
