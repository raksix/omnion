//! Automation project isolation (REQ-133, slice 2) — against a real database.
//!
//! ## Why this is an integration test and not more unit tests
//!
//! The defect this file exists for was **invisible to every unit test on the branch**, and that
//! is the reason worth writing down. Slice 1 shipped migration 0164, which made
//! `workflows.project_id` `not null` after backfilling the existing rows. The store's insert
//! statement named twelve columns, none of them `project_id`, and the unit tests all passed —
//! because the unit tests never executed SQL. So the moment 0164 was applied, *every workflow
//! creation on the branch* raised `23502 null value in column "project_id"`, and a green test
//! suite reported the feature as working.
//!
//! The general rule: **a `not null` column added by a migration is a contract with every write
//! path, and nothing in a type system or a unit test connects the two.** It is a database
//! question, so it is answered by a database.
//!
//! ## What is asserted here
//!
//! 1. A workflow created through the store with an explicit project lands there.
//! 2. A workflow created with the default project's id reads back from the default.
//! 3. `resolve_target` with no project gives the organization's default — never an error, never a
//!    guess at the caller's first project.
//! 4. `visible_project_ids` contains the default even for an account with **no** membership row
//!    at all, and contains a non-default project only for a member.
//! 5. Two members of one organization, in different projects, do not see each other's workflows
//!    through the scoped list — the sentence that makes project membership mean anything, and
//!    the one an organization-only check passes.

use omnion_workflows::TriggerKind;
use omnion_workflows::model::NewWorkflow;
use omnion_workflows::projects::{self, ProjectCaller, ProjectRole};
use omnion_workflows::store;
use sqlx::PgPool;
use uuid::Uuid;

/// The fixture, laid out as the feature describes: two organizations, two colleagues inside the
/// first one, a default project per organization, and a second project in the first.
struct World {
    pool: PgPool,
    /// Organization the assertions live in.
    acme: Uuid,
    /// The other organization, which no amount of permission may reach across.
    globex: Uuid,
    /// A member of the default project only.
    owner: Uuid,
    /// A member of `payroll` only.
    colleague: Uuid,
    /// Acme's default project.
    acme_default: Uuid,
    /// A non-default project in Acme.
    payroll: Uuid,
    /// Globex's default project.
    globex_default: Uuid,
}

/// Connect, or say why the gate did not run.
///
/// A panic with the reason is what a failing fixture should look like. The alternative — a
/// helper returning `None` and letting each test skip — produces a suite that reports zero tests
/// and reads as a pass, which is the failure mode this branch has already paid for twice.
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-project-isolation.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Per-test id space.
///
/// The first version of this fixture used a fixed base with a per-*entity* marker
/// (`id(1)`, `id(2)`, …), which looks like namespacing and is not: the ids were distinct from each
/// other and **identical from test to test**, so eight concurrent tests shared one organization,
/// one pair of colleagues and one default project. The symptom was a genuine-looking isolation
/// failure — the scoped list carrying a default project id nobody in that test had written — and
/// the real cause was two lines of arithmetic.
///
/// The rule: a namespace must be unique per *test*, not per *entity within a test*. Tests in a
/// suite share a database; entities inside a test do not.
struct Space {
    root: Uuid,
}

impl Space {
    fn new() -> Self {
        Self {
            root: Uuid::new_v4(),
        }
    }

    /// A stable id inside this test's space.
    fn id(&self, marker: u8) -> Uuid {
        let mut bytes = *self.root.as_bytes();
        // The last two bytes carry the marker, so the mapping is injective inside the space and
        // the fixture stays readable (`id(1)` is the organization) instead of being opaque hex.
        bytes[14] = marker;
        bytes[15] = 0;
        Uuid::from_bytes(bytes)
    }

    /// A slug that satisfies the unique constraint and says which space it came from.
    fn slug(&self, prefix: &str) -> String {
        format!("{prefix}-{}", &self.root.simple().to_string()[..8])
    }
}

async fn world() -> World {
    let pool = pool().await;

    // Each test builds its own namespace, because the suite runs concurrently against ONE
    // database and a shared fixture makes one test's assertion another test's rows.
    let space = Space::new();

    let acme = space.id(1);
    let globex = space.id(2);
    let owner = space.id(3);
    let colleague = space.id(4);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Acme', $2, now(), now()), ($3, 'Globex', $4, now(), now())",
    )
    .bind(acme)
    .bind(space.slug("acme"))
    .bind(globex)
    .bind(space.slug("globex"))
    .execute(&pool)
    .await
    .expect("two organizations");

    for (id, org, email, name) in [
        (
            owner,
            acme,
            format!("owner@{}", space.slug("acme")),
            "Acme Owner",
        ),
        (
            colleague,
            acme,
            format!("col@{}", space.slug("acme")),
            "Acme Colleague",
        ),
    ] {
        sqlx::query(
            "insert into users (id, organization_id, email, display_name, password_hash, \
             created_at, updated_at) values ($1, $2, $3, $4, 'x', now(), now())",
        )
        .bind(id)
        .bind(org)
        .bind(email)
        .bind(name)
        .execute(&pool)
        .await
        .expect("an account");
    }

    let acme_default = projects::default_project(&pool, acme)
        .await
        .expect("acme's default project");
    let globex_default = projects::default_project(&pool, globex)
        .await
        .expect("globex's default project");
    let payroll = projects::create_project(
        &pool,
        projects::NewProject {
            organization_id: acme,
            key: "PAYROLL".to_owned(),
            name: "Payroll".to_owned(),
            description: "Sensitive".to_owned(),
            color: None,
            icon: None,
            owner_user_id: colleague,
            created_by: Some(owner),
        },
    )
    .await
    .expect("a second project");

    World {
        pool,
        acme,
        globex,
        owner,
        colleague,
        acme_default: acme_default.id,
        payroll: payroll.id,
        globex_default: globex_default.id,
    }
}

fn caller(user_id: Uuid, is_instance_admin: bool) -> ProjectCaller {
    ProjectCaller {
        user_id,
        is_instance_admin,
    }
}

fn definition(name: &str, project_id: Uuid, organization_id: Uuid) -> NewWorkflow {
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
        steps: serde_json::json!([]),
        created_by: None,
    }
}

/// The regression this whole file is for: the insert path must name the project column.
///
/// Against the slice-1 code this fails with the production error verbatim — `null value in
/// column "project_id" violates not-null constraint` — because 0164 made the column mandatory and
/// `insert_workflow` did not know the column existed. That is the whole defect: a migration that
/// made a column `not null` and a write path that was never told.
#[tokio::test]
async fn a_workflow_is_written_into_the_project_it_names() {
    let w = world().await;

    let created = store::insert_workflow(
        &w.pool,
        definition("Nightly digest", w.acme_default, w.acme),
    )
    .await
    .expect("the insert names project_id, so 0164's not-null constraint is satisfied");

    assert_eq!(created.project_id, w.acme_default);
}

/// The case the fix has to get *right*, not merely *running*: the default project is a real
/// answer, and a workflow written there reads back from the project the caller is in.
#[tokio::test]
async fn a_workflow_in_the_default_project_reads_back_from_it() {
    let w = world().await;

    let target = projects::resolve_target(&w.pool, w.acme, None, caller(w.owner, false))
        .await
        .expect("resolving with no project is never an error");

    assert_eq!(
        target.id, w.acme_default,
        "no project means the default, always"
    );
    assert!(target.is_default, "and the row says so");

    store::insert_workflow(&w.pool, definition("In default", target.id, w.acme))
        .await
        .expect("a write into the default project");

    let listed = store::list_workflows_in_projects(&w.pool, Some(w.acme), None, &[w.acme_default])
        .await
        .expect("scoped list");
    assert_eq!(
        listed.len(),
        1,
        "the default project holds the one workflow"
    );
    assert_eq!(listed[0].project_id, w.acme_default);
}

/// An account with no membership row still sees the default — the clause that keeps a fresh
/// installation usable. Without it, the owner's first workflow sits in a project the owner cannot
/// open, and no migration would catch it.
#[tokio::test]
async fn the_default_project_is_visible_to_an_account_with_no_membership() {
    let w = world().await;

    let visible = projects::visible_project_ids(&w.pool, w.acme, caller(w.owner, false))
        .await
        .expect("visible ids");

    assert!(
        visible.contains(&w.acme_default),
        "the default is where every un-targeted resource lands, so hiding it hides the product"
    );
    assert!(
        !visible.contains(&w.payroll),
        "a project they are not a member of stays invisible"
    );
}

/// The sentence the whole slice exists for: two people, one organization, different projects.
///
/// An organization-only check — which is what slice 1 shipped — passes this test, because both
/// members are inside the same organization. Isolation is a fact about *projects*, so only a
/// project-aware check can fail here, and this is that check.
///
/// The third project is what makes the assertion possible. With only a default and `payroll`, the
/// colleague sees the default *by design* (see [`the_default_project_is_the_shared_bucket_by
/// design`]) and the test cannot tell a deliberate shared bucket from a leak. `billing` is a
/// named project the colleague is not in, so its workflow must be absent — and its absence is
/// the claim.
#[tokio::test]
async fn a_member_of_one_project_cannot_see_another_projects_workflow() {
    let w = world().await;
    let billing = projects::create_project(
        &w.pool,
        projects::NewProject {
            organization_id: w.acme,
            key: "BILLING".to_owned(),
            name: "Billing".to_owned(),
            description: "Owner's project".to_owned(),
            color: None,
            icon: None,
            owner_user_id: w.owner,
            created_by: Some(w.owner),
        },
    )
    .await
    .expect("a third project");

    let secret = store::insert_workflow(&w.pool, definition("Invoices", billing.id, w.acme))
        .await
        .expect("a workflow in a project the colleague is not in");

    let visible = projects::visible_project_ids(&w.pool, w.acme, caller(w.colleague, false))
        .await
        .expect("visible ids");
    assert!(
        visible.contains(&w.payroll),
        "a member sees their own project"
    );
    assert!(
        !visible.contains(&billing.id),
        "and not one they were never added to"
    );

    let scoped = store::list_workflows_in_projects(&w.pool, Some(w.acme), None, &visible)
        .await
        .expect("scoped list");
    assert!(
        scoped.iter().all(|wf| wf.project_id != billing.id),
        "the scoped list never carries another project's rows: {:?}",
        scoped.iter().map(|wf| wf.project_id).collect::<Vec<_>>()
    );

    // And the single-row check answers the same way, because that is what a read endpoint calls:
    // a deep link to `secret` is a 404, not a 403.
    let may_see =
        projects::can_see_workflow(&w.pool, w.acme, billing.id, caller(w.colleague, false))
            .await
            .expect("can_see_workflow");
    assert!(
        !may_see,
        "a project member may not open another project's workflow by id"
    );
}

/// The default project is **shared by the whole organization**, deliberately.
///
/// This is the one design decision in slice 2 that is not a straight reading of the REQ, so it is
/// stated here as a test rather than left as a surprise.
///
/// The tension: `visible_project_ids` cannot require a membership row for the default, because
/// on a fresh installation the owner's first workflow lands there and they are in no membership
/// row — a rule that demands one makes a new installation a platform that cannot see its own
/// first automation. And the default is also where 0164 put *every pre-existing workflow*, so
/// requiring a membership would hide automations people already rely on from people who already
/// relied on them.
///
/// The cost, stated plainly: **the default is a shared bucket, and isolation does not apply
/// inside it.** Anything an organization wants isolated belongs in a named project, and the
/// migration's own backfill is what makes that migration path — move the resource out of the
/// default once, and it is scoped. Pretending otherwise would be a feature that reports isolation
/// and delivers a shared bucket, which is worse than a documented one.
#[tokio::test]
async fn the_default_project_is_the_shared_bucket_by_design() {
    let w = world().await;
    store::insert_workflow(
        &w.pool,
        definition("Unassigned work", w.acme_default, w.acme),
    )
    .await
    .expect("a workflow in the shared bucket");

    let visible = projects::visible_project_ids(&w.pool, w.acme, caller(w.colleague, false))
        .await
        .expect("visible ids");
    assert!(
        visible.contains(&w.acme_default),
        "the default is where every un-targeted resource lands; hiding it would hide the product"
    );

    let scoped = store::list_workflows_in_projects(&w.pool, Some(w.acme), None, &visible)
        .await
        .expect("scoped list");
    assert_eq!(
        scoped.len(),
        1,
        "and a colleague CAN read it — the shared bucket is documented, not accidental"
    );
}

/// Another organization's default is not reachable, even to an instance administrator of the
/// first one. Scoping is tenancy first, and tenancy is not negotiable by a permission.
#[tokio::test]
async fn an_instance_admin_does_not_see_another_organizations_project() {
    let w = world().await;

    let as_admin = projects::visible_project_ids(&w.pool, w.acme, caller(w.owner, true))
        .await
        .expect("visible ids");
    assert!(as_admin.contains(&w.acme_default));
    assert!(
        as_admin.contains(&w.payroll),
        "an instance admin sees the whole organization"
    );
    assert!(
        !as_admin.contains(&w.globex_default),
        "but never another organization's project"
    );
}

/// The empty set is a real answer, not a shortcut past the query.
///
/// This is the assertion that catches the classic scoping bug: `where project_id = any(null)` in
/// SQL is `= any(NULL)`, which is **not** true for every row and not false for every row — a
/// caller with no visible project must be handed nothing, and the query has to say so itself.
#[tokio::test]
async fn an_empty_project_set_yields_no_rows_rather_than_every_row() {
    let w = world().await;
    store::insert_workflow(&w.pool, definition("Anything", w.acme_default, w.acme))
        .await
        .expect("a workflow exists somewhere");

    let listed = store::list_workflows_in_projects(&w.pool, Some(w.acme), None, &[])
        .await
        .expect("the empty set is answerable");
    assert!(
        listed.is_empty(),
        "a caller with no visible project must see nothing — the assertion that catches a \
         `where project_id = any(null)` filter"
    );
}

/// The last-owner refusal is slice 4's rule, but the store function carrying it landed in slice 1,
/// and a regression in it would be silent until slice 4 has a screen to notice.
#[tokio::test]
async fn removing_the_last_owner_is_refused_by_name() {
    let w = world().await;
    let err = projects::remove_member(&w.pool, w.payroll, w.colleague)
        .await
        .expect_err("the only owner cannot be removed");
    assert_eq!(err.code(), "last_project_owner");
    assert!(
        err.to_string().contains("transfer ownership"),
        "the message must name the remedy: {err}"
    );
}

/// A role change is one statement, not check-then-write, and the row is read back rather than
/// trusted: an `on conflict do update` that silently did nothing is a member list that lies.
#[tokio::test]
async fn a_role_change_is_one_write_and_the_row_agrees() {
    let w = world().await;

    projects::upsert_member(
        &w.pool,
        w.payroll,
        w.colleague,
        ProjectRole::Viewer,
        Some(w.owner),
    )
    .await
    .expect("a real role is accepted");

    let stored: String = sqlx::query_scalar(
        "select role from automation_project_members where project_id = $1 and user_id = $2",
    )
    .bind(w.payroll)
    .bind(w.colleague)
    .fetch_one(&w.pool)
    .await
    .expect("the membership row");

    assert_eq!(
        stored, "viewer",
        "the role change is one statement, not check-then-write"
    );
}
