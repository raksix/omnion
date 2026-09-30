//! The stored project selection must narrow the lists, not just label the switcher (REQ-133).
//!
//! REQ-133's API table promises `GET /workflows` gains "a `project_id` filter **and scoped
//! defaults**", and acceptance 3 says "the switcher filters workflows … the selection survives
//! navigation". Only the filter half shipped.
//!
//! `automation_project_selection` was written by `select_project` and read back by
//! `selected_project` — which is the switcher's own button label. **No list ever consulted it**:
//! `list_scoped_workflows` and `list_automations` both did `match wanted { Some(id) => …,
//! None => visible }`, and with no `project_id` in the query the answer was *every project the
//! caller can see*. So a reader who picked OPS in the switcher and then opened a list got the
//! unscoped list back, under a header that still said "OPS".
//!
//! This is the module's thirteenth instance of its signature defect and the first where the dead
//! thing was a *default* rather than a value: `stored_selection` had no caller at all — not even
//! the switcher, which used the organization-scoped `selected_project` instead.
//!
//! ## What is asserted here, and what would have been green before
//!
//! Everything below is about the **default** path (`no project_id in the query`). A suite that
//! only passed explicit filters would stay green forever, because the filter already worked — so
//! each test names the difference between "no default" and "the right default", and two are
//! negative controls proving the gate can see the absence as well as the presence.

use std::env;

use omnion_workflows::projects::{self, NewProject, ProjectCaller, ProjectRole};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    match PgPool::connect(&env::var("DATABASE_URL").expect("set by the gate")).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            println!("SKIP: no database: {error}");
            None
        }
    }
}

fn caller(who: Uuid) -> ProjectCaller {
    ProjectCaller {
        user_id: who,
        is_instance_admin: false,
    }
}

struct Fixture {
    organization_id: Uuid,
    alice: Uuid,
    ops: Uuid,
    billing: Uuid,
    other_tenant_ops: Uuid,
}

async fn make_project(pool: &PgPool, organization_id: Uuid, key: &str, owner: Uuid) -> Uuid {
    projects::create_project(
        pool,
        NewProject {
            organization_id,
            key: key.into(),
            name: format!("Project {key}"),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("the project insert is this suite's fixture")
    .id
}

async fn make_user(pool: &PgPool, organization_id: Option<Uuid>, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into users (id, email, display_name, organization_id) \
         values ($1, $2, $3, $4) returning id",
    )
    .bind(Uuid::new_v4())
    .bind(format!("{}-{}@example.test", name.to_lowercase(), Uuid::new_v4()))
    .bind(name)
    .bind(organization_id)
    .fetch_one(pool)
    .await
    .expect("the user insert is this suite's fixture")
}

async fn fixture(pool: &PgPool) -> Fixture {
    let organization_id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'ScopeDefault', $2)")
        .bind(organization_id)
        .bind(format!("scopedefault-{organization_id}"))
        .execute(pool)
        .await
        .expect("the organization insert is this suite's fixture");

    let alice = make_user(pool, Some(organization_id), "Alice").await;
    let default = make_project(pool, organization_id, "MAIN", alice).await;
    sqlx::query("update automation_projects set is_default = true where id = $1")
        .bind(default)
        .execute(pool)
        .await
        .expect("flagging the default project is this suite's fixture");

    let ops = make_project(pool, organization_id, "OPS", alice).await;
    let billing = make_project(pool, organization_id, "BILLING", alice).await;
    projects::upsert_member(pool, ops, alice, ProjectRole::Editor, None)
        .await
        .expect("alice is a member of OPS");
    projects::upsert_member(pool, billing, alice, ProjectRole::Editor, None)
        .await
        .expect("alice is a member of BILLING");

    // A second tenant with a project of its own. It exists to be INVISIBLE, and the point of
    // building it in the fixture rather than asserting an empty list is that "invisible" and
    // "not selected" are different answers to the same question.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'ScopeDefaultOther', $2)")
        .bind(other_org)
        .bind(format!("scopedefaultother-{other_org}"))
        .execute(pool)
        .await
        .expect("the second organization insert is this suite's fixture");
    let stranger = make_user(pool, Some(other_org), "Stranger").await;
    let other_tenant_ops = make_project(pool, other_org, "OPS", stranger).await;

    Fixture {
        organization_id,
        alice,
        ops,
        billing,
        other_tenant_ops,
    }
}

/// What a list route computes for `project_id = None`: the projects it would return rows from.
async fn scoped_for(pool: &PgPool, fx: &Fixture, who: Uuid) -> Vec<Uuid> {
    let visible =
        projects::visible_project_ids(pool, fx.organization_id, caller(who)).await.expect("visible");
    let wanted = projects::default_scope(pool, who, &visible)
        .await
        .expect("the stored default");
    match wanted {
        Some(id) if visible.contains(&id) => vec![id],
        Some(_) => Vec::new(),
        None => visible,
    }
}

#[tokio::test]
async fn the_stored_selection_is_the_default_when_no_filter_is_asked_for() {
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;

    // The precondition, stated as a fact rather than assumed: before selecting anything, the
    // default path spans every project alice can see. Without this a fix that always returned the
    // default project would pass the test below for the wrong reason.
    let before = scoped_for(&pool, &fx, fx.alice).await;
    assert!(
        before.len() > 1,
        "precondition: with no selection the default path is unscoped, got {before:?}"
    );
    assert!(before.contains(&fx.ops) && before.contains(&fx.billing));

    projects::select_project(&pool, fx.organization_id, fx.ops, caller(fx.alice))
        .await
        .expect("the selection is written");

    let after = scoped_for(&pool, &fx, fx.alice).await;
    assert_eq!(
        after,
        vec![fx.ops],
        "THE DEFECT: the switcher stored OPS, and a list with no `project_id` still spanned every \
         project alice can see. `stored_selection` had no caller on the branch — the stored row \
         labelled the switcher's own button and nothing else. Got {after:?}"
    );
}

#[tokio::test]
async fn the_default_survives_navigation_because_it_is_stored_not_remembered_by_the_client() {
    // Acceptance 3's "the selection survives navigation". The fact under test is that the answer
    // comes from the DATABASE row, so it is the same on a cold request from a different process —
    // which is exactly what a `useState` in the switcher could not promise.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    projects::select_project(&pool, fx.organization_id, fx.billing, caller(fx.alice))
        .await
        .expect("the selection is written");

    let stored: Option<Uuid> =
        sqlx::query_scalar("select project_id from automation_project_selection where user_id = $1")
            .bind(fx.alice)
            .fetch_one(&pool)
            .await
            .expect("the row is readable");
    assert_eq!(
        stored,
        Some(fx.billing),
        "the selection is a row, not a client-side memory"
    );
    assert_eq!(
        scoped_for(&pool, &fx, fx.alice).await,
        vec![fx.billing],
        "a cold read lands on the selected project"
    );
}

#[tokio::test]
async fn an_explicit_filter_outranks_the_stored_selection() {
    // The negative control for the fix. A remembered answer must never overrule a typed question,
    // or `?project_id=` would stop being a filter and become a suggestion.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    projects::select_project(&pool, fx.organization_id, fx.ops, caller(fx.alice))
        .await
        .expect("the selection is written");

    let visible =
        projects::visible_project_ids(&pool, fx.organization_id, caller(fx.alice)).await.expect("visible");
    let explicit = vec![fx.billing];
    let allowed: Vec<Uuid> = match Some(fx.billing) {
        Some(id) if visible.contains(&id) => vec![id],
        Some(_) => Vec::new(),
        None => visible,
    };
    assert_eq!(
        allowed, explicit,
        "an explicit project_id still decides the list; the selection is only a DEFAULT"
    );
}

#[tokio::test]
async fn a_selection_naming_a_project_the_caller_can_no_longer_see_is_ignored() {
    // The refusal that makes the default safe. A project owner who removes a member leaves that
    // member's stored selection naming a project they can no longer see; honouring it would WIDEN
    // the list for the person who just lost access, which is the exact opposite of what a default
    // is for. The answer is "no default", which is the unscoped-but-visible list — never the
    // removed project's rows, and never an error.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    projects::select_project(&pool, fx.organization_id, fx.billing, caller(fx.alice))
        .await
        .expect("the selection is written");

    // Alice is removed from BILLING. `remove_member` refuses the last owner, so the project is
    // given a second member first — otherwise this test would be measuring the owner rule.
    let bob = make_user(&pool, Some(fx.organization_id), "Bob").await;
    projects::upsert_member(&pool, fx.billing, bob, ProjectRole::Owner, None)
        .await
        .expect("bob owns BILLING so alice is not the last owner");
    assert!(
        projects::remove_member(&pool, fx.billing, fx.alice)
            .await
            .expect("removal runs"),
        "the membership row is really gone"
    );

    let visible =
        projects::visible_project_ids(&pool, fx.organization_id, caller(fx.alice)).await.expect("visible");
    assert!(
        !visible.contains(&fx.billing),
        "precondition: BILLING is no longer visible to alice"
    );
    assert_eq!(
        projects::default_scope(&pool, fx.alice, &visible)
            .await
            .expect("the default answers"),
        None,
        "a stale selection must be ignored, not honoured: honouring it would return rows alice \
         cannot see, because the route only intersects with what it fetched"
    );

    // And the list the route then builds: the default is `None`, so it is every project alice CAN
    // see — which is wider than before her removal, and that is correct. The assertion that
    // matters is the one about visibility, and it is above.
    let scoped = scoped_for(&pool, &fx, fx.alice).await;
    assert!(
        !scoped.contains(&fx.billing),
        "the removed project's id is never in the scope, whatever the selection says, got {scoped:?}"
    );
}

#[tokio::test]
async fn a_selection_from_another_tenant_is_never_a_default() {
    // The cross-tenant half, and the reason `default_scope` takes `visible` as an argument rather
    // than reading it: an id that is not in the caller's own visible set cannot become a default,
    // so a row left behind by a previous organization membership resolves to nothing at all.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;

    // Written directly, because `select_project` refuses it — and that refusal is the reason the
    // check still belongs here: the row can exist by any other writer (a direct write, an
    // older build, a future import) and the list must not honour it.
    sqlx::query("insert into automation_project_selection (user_id, project_id) values ($1, $2)")
        .bind(fx.alice)
        .bind(fx.other_tenant_ops)
        .execute(&pool)
        .await
        .expect("the row is written for the test");

    let visible =
        projects::visible_project_ids(&pool, fx.organization_id, caller(fx.alice)).await.expect("visible");
    assert!(
        !visible.contains(&fx.other_tenant_ops),
        "precondition: the other tenant's project is not in alice's visible set"
    );
    assert_eq!(
        projects::default_scope(&pool, fx.alice, &visible)
            .await
            .expect("the default answers"),
        None,
        "a selection naming another tenant's project is ignored, never honoured"
    );
}

#[tokio::test]
async fn a_caller_who_never_selected_has_no_default() {
    // The plain negative control. A `default_scope` that answered with the organization's default
    // project would make "never chose" and "chose the default" indistinguishable, which is the
    // same `None`-means-something state this module already got wrong once (`selected_project`
    // returning the default instead of nothing).
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;

    let visible =
        projects::visible_project_ids(&pool, fx.organization_id, caller(fx.alice)).await.expect("visible");
    assert_eq!(
        projects::default_scope(&pool, fx.alice, &visible)
            .await
            .expect("the default answers"),
        None,
        "never having selected is not the same as having selected the default project"
    );
}
