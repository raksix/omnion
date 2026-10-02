//! A shared link's `?project=` resolves for the person who OPENS it (REQ-133, acceptance 3) —
//! against a real database.
//!
//! The sentence is "the selection … is encoded in URLs so a shared link reproduces the view", and
//! for twenty-nine ticks the *writer* half was true and the *reader* half did not exist. The
//! switcher wrote `?project=<id>` on every choice (`router.replace` in `choose`) and read it in
//! exactly one place — the button's own label — so a colleague opening your link saw **their**
//! stored selection, not the project you sent them, with the address bar naming yours. Nothing
//! failed: the API compiled, the panel typechecked, the harness drove the screen, and the feature
//! was a no-op dressed as a link.
//!
//! This is the twelfth instance of this branch's signature defect, and the first whose dead half
//! was the READER rather than the writer. The previous eleven were a caller, a table, a state or a
//! claim that was missing; here the write had been shipped and tested for many ticks, which is
//! exactly what made the gap invisible — a parameter that is written, named in the REQ, asserted
//! round-tripped by the client and read by nothing is the shape of a feature that only needs one
//! more line, and nobody goes looking for the one line nobody needed.
//!
//! ## What this gate pins
//!
//! * No link: `None`. The ordinary navigation, which must not be described as a refusal.
//! * A link to a project the caller may see: `Some((id, true))` — the view is reproduced.
//! * A link to a project the caller may NOT see: `Some((id, false))` — **the id is returned**.
//!   This is the assertion the design turns on. `None` would be a silent fallback to the caller's
//!   own selection, which is the exact outcome the link was sent to prevent, and it is the answer
//!   a `bool` return type cannot give.
//! * Another organization's project is refused rather than resolved: `find_visible` is the ONLY
//!   thing consulted, so "may see" means the same thing here as on every other project read.
//! * A missing project is refused, not a lookup error: a link to a project that was deleted reads
//!   as "you may not see this", never as a 500 the reader cannot act on.
//! * **Following a link never writes the caller's selection.** Asserted by reading the stored row
//!   back after the resolution, because the whole reason this slice exists is that a link and a
//!   preference are different things. A reader that silently adopted the sender's project as their
//!   own would satisfy "reproduces the view" and destroy the next page they open.

use std::env;

use omnion_workflows::projects::{self, NewProject, ProjectCaller, ProjectRole};
use sqlx::PgPool;
use uuid::Uuid;

/// A pool, or a printed reason and a skip — the platform's convention for a machine without Docker.
async fn pool() -> Option<PgPool> {
    match PgPool::connect(&env::var("DATABASE_URL").expect("set by the gate")).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            println!("SKIP: no database: {error}");
            None
        }
    }
}

/// A namespace unique per TEST, not per entity within a test: eight tests sharing one
/// organization is how a fixture produces assertions about another test's rows.
struct Fixture {
    organization_id: Uuid,
    default_project: Uuid,
    user_id: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'Link', $2)")
        .bind(organization_id)
        .bind(format!("link-{}", organization_id))
        .execute(pool)
        .await
        .expect("the organization insert is this suite's fixture");
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Link')")
        .bind(user_id)
        .bind(format!("link-{}@example.test", user_id))
        .execute(pool)
        .await
        .expect("the user insert is this suite's fixture");
    let created = projects::create_project(
        pool,
        NewProject {
            organization_id,
            key: "MAIN".into(),
            name: "Main".into(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: user_id,
            created_by: Some(user_id),
        },
    )
    .await
    .expect("the default project insert is this suite's fixture");
    sqlx::query("update automation_projects set is_default = true where id = $1")
        .bind(created.id)
        .execute(pool)
        .await
        .expect("the default flag is this suite's fixture");
    Fixture {
        organization_id,
        default_project: created.id,
        user_id,
    }
}

fn caller_for(user_id: Uuid, is_instance_admin: bool) -> ProjectCaller {
    ProjectCaller {
        user_id,
        is_instance_admin,
    }
}

/// A project the given user is an owner of, and therefore a member of.
async fn owned_project(pool: &PgPool, org: Uuid, owner: Uuid, key: &str) -> Uuid {
    let project = projects::create_project(
        pool,
        NewProject {
            organization_id: org,
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
    .expect("the project insert is this suite's fixture");
    projects::upsert_member(pool, project.id, owner, ProjectRole::Owner, Some(owner))
        .await
        .expect("the membership insert is this suite's fixture");
    project.id
}

#[tokio::test]
async fn no_link_is_not_a_refusal() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // "All projects" and "a link you may not open" are different sentences. Collapsing them is how
    // an access boundary turns into an empty screen, so the ordinary navigation must answer
    // `None` — and this assertion is the one that would catch a `default` arm doing the refusing.
    let resolved = projects::resolve_linked_project(&pool, f.organization_id, None, caller)
        .await
        .expect("a request with no link is not an error");
    assert_eq!(
        resolved, None,
        "no ?project= must resolve to None, not to a refusal of the caller's own project"
    );
}

#[tokio::test]
async fn a_link_to_a_visible_project_reproduces_the_view() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);
    let target = owned_project(&pool, f.organization_id, f.user_id, "OPS").await;

    let resolved = projects::resolve_linked_project(&pool, f.organization_id, Some(target), caller)
        .await
        .expect("a visible link resolves");
    assert_eq!(
        resolved,
        Some((target, true)),
        "a member's own project must resolve as visible, or a shared link reproduces nothing"
    );
}

#[tokio::test]
async fn a_link_to_a_project_you_cannot_see_returns_the_id_and_says_no() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // Someone else's project, in the same organization: a real project, a real id, and this
    // reader is not a member of it.
    let other_owner = Uuid::new_v4();
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Other')")
        .bind(other_owner)
        .bind(format!("link-other-{}@example.test", other_owner))
        .execute(&pool)
        .await
        .expect("the second user insert is this suite's fixture");
    let foreign = owned_project(&pool, f.organization_id, other_owner, "OPS").await;

    let resolved = projects::resolve_linked_project(&pool, f.organization_id, Some(foreign), caller)
        .await
        .expect("a link to a project you may not see is not an error");

    // **The id is returned.** This is the assertion the whole slice turns on. `Some((id, false))`
    // is what lets the panel say "you are not a member of this project" and offer the remedy;
    // `None` would silently drop the reader on their own selection, which is the one outcome a
    // shared link must never have — the sender asked them to look at something specific.
    assert_eq!(
        resolved,
        Some((foreign, false)),
        "an unreadable link must answer (id, false) so the panel can explain it, never None"
    );
}

#[tokio::test]
async fn another_organizations_link_is_refused_rather_than_resolved() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // A second organization, and a project inside it. The reader is not a member, and the default
    // project of an organization they do not belong to is not theirs either.
    let other_org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'Elsewhere', $2)")
        .bind(other_org)
        .bind(format!("link-elsewhere-{}", other_org))
        .execute(&pool)
        .await
        .expect("the second organization insert is this suite's fixture");
    let stranger = owned_project(&pool, other_org, f.user_id, "OPS").await;

    let resolved =
        projects::resolve_linked_project(&pool, f.organization_id, Some(stranger), caller)
            .await
            .expect("a cross-tenant link is answered, not raised");
    assert_eq!(
        resolved,
        Some((stranger, false)),
        "a project in another organization must be refused, not resolved for this caller"
    );
}

#[tokio::test]
async fn a_deleted_project_is_refused_rather_than_raising() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // A link to something that no longer exists. The reader must get a sentence they can act on,
    // not a 500 from a lookup that found nothing — a `404` the panel could show, rendered as the
    // "not a member of this project" strip, which is the same advice for the same situation.
    let ghost = Uuid::new_v4();
    let resolved =
        projects::resolve_linked_project(&pool, f.organization_id, Some(ghost), caller)
            .await
            .expect("a link to a missing project is answered, not raised");
    assert_eq!(
        resolved,
        Some((ghost, false)),
        "a missing project must read as 'not visible', never as a database error"
    );
}

#[tokio::test]
async fn following_a_link_does_not_become_your_own_selection() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);
    let target = owned_project(&pool, f.organization_id, f.user_id, "OPS").await;

    // This reader has chosen a project of their own.
    let mine = owned_project(&pool, f.organization_id, f.user_id, "MINE").await;
    projects::select_project(&pool, f.organization_id, mine, caller)
        .await
        .expect("the reader's own switch writes");

    // A colleague sends them a link to someone else's project.
    projects::resolve_linked_project(&pool, f.organization_id, Some(target), caller)
        .await
        .expect("the link resolves");

    // Reading a link is a READ. If this assertion ever fails, every shared link silently
    // overwrote the recipient's default — and the next page they open lands somewhere they never
    // chose, which is the failure this separation exists to prevent.
    let stored = projects::selected_project(&pool, f.organization_id, caller)
        .await
        .expect("the stored selection is readable");
    assert_eq!(
        stored,
        Some(mine),
        "resolving a shared link must not change the caller's own stored selection"
    );
}

#[tokio::test]
async fn an_instance_administrator_sees_the_linked_project() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let admin = caller_for(f.user_id, true);
    let target = owned_project(&pool, f.organization_id, f.user_id, "OPS").await;

    // The platform administrator is the one caller for whom "may see" is organization-wide. A
    // refusal here would be the same class of defect as `visible_project_ids` disagreeing with
    // `find_visible`: the switcher would show an operator a link that works for nobody.
    let resolved = projects::resolve_linked_project(&pool, f.organization_id, Some(target), admin)
        .await
        .expect("the administrator's link resolves");
    assert_eq!(
        resolved,
        Some((target, true)),
        "an instance administrator reaches every project in the organization, links included"
    );
}
