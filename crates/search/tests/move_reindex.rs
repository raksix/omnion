//! A move must re-stamp the search index, or the project scoping leaks the wrong way round.
//!
//! REQ-133 acceptance 5 (search is "scoped to their projects") and slice 3 (the move) meet here,
//! and nothing on the branch made them agree. Search scoping is enforced by one clause in
//! `query::search` — `d.project_id = any($n) or d.project_id is null` — matching the project
//! stored on the `search_documents` row, which the `workflows` provider upsert copies from
//! `workflows.project_id`.
//!
//! `move_workflow` updates `workflows.project_id` and writes the audit row. It does not update
//! `search_documents`, and **no workflow write path anywhere re-indexes**: `index_entity` has no
//! caller and there is no periodic reindex. So the stored project stays whatever it was when the
//! document was last indexed, and the failure is not symmetric — the person who LOST access to the
//! workflow is the one who keeps finding it. A shared `?project=<id>` link does not help: the
//! clause reads the stored value, and the stored value is stale.
//!
//! ## Why this is not already covered
//!
//! `project_scoped_search.rs` has a test named
//! `a_workflow_moved_to_another_project_stops_answering_for_the_old_membership`, and it moves the
//! workflow with a raw `update` and then calls `reindex` by hand. It exercises the reindexer, not
//! the move, and its comment is about the upsert's conflict tail — a property of the reindex and
//! never of `move_workflow`. **The gate that named this defect was the gate that could not see
//! it**: the fixture performed away the only step under test, so the assertion was green against
//! code that has never been exercised on this path.
//!
//! That fixture is kept, deliberately, and is still true — the reindex does fix a stale row. What
//! it does not do is make the *move* fix it, which is the only thing a user experiences. Both
//! claims are asserted here so neither can be quietly deleted: the move alone must suffice, and a
//! later reindex must not leave the answer different.

use std::env;

use omnion_search::indexer;
use omnion_search::query::{self, Query as SearchQuery, SearchRequest, Sort};
use omnion_workflows::move_workflow::move_workflow;
use omnion_workflows::projects::{self, NewProject, ProjectCaller};
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

struct Fixture {
    organization_id: Uuid,
    alice_project: Uuid,
    bob_project: Uuid,
    default_project: Uuid,
    alice: Uuid,
    bob: Uuid,
    alice_workflow: Uuid,
}

async fn make_project(pool: &PgPool, organization_id: Uuid, key: &str, owner: Uuid) -> Uuid {
    let created = projects::create_project(
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
    .expect("the project insert is this suite's fixture");
    created.id
}

async fn make_workflow(pool: &PgPool, organization_id: Uuid, project_id: Uuid, name: &str) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "insert into workflows (organization_id, name, trigger_kind, steps, project_id) \
         values ($1, $2, 'manual', '[]'::jsonb, $3) returning id",
    )
    .bind(organization_id)
    .bind(name)
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("the workflow insert is this suite's fixture");
    id
}

async fn fixture(pool: &PgPool) -> Fixture {
    let organization_id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'MoveReindex', $2)")
        .bind(organization_id)
        .bind(format!("movereindex-{organization_id}"))
        .execute(pool)
        .await
        .expect("the organization insert is this suite's fixture");

    let mut ids = Vec::new();
    for who in ["Alice", "Bob"] {
        let id: Uuid = sqlx::query_scalar(
            "insert into users (id, email, display_name, organization_id) \
             values ($1, $2, $3, $4) returning id",
        )
        .bind(Uuid::new_v4())
        .bind(format!("{}-{}@example.test", who.to_lowercase(), Uuid::new_v4()))
        .bind(who)
        .bind(organization_id)
        .fetch_one(pool)
        .await
        .expect("the user insert is this suite's fixture");
        ids.push(id);
    }
    let alice = ids[0];
    let bob = ids[1];

    let default_project = make_project(pool, organization_id, "MAIN", alice).await;
    sqlx::query("update automation_projects set is_default = true where id = $1")
        .bind(default_project)
        .execute(pool)
        .await
        .expect("flagging the default project is this suite's fixture");

    let alice_project = make_project(pool, organization_id, "ALICE", alice).await;
    let bob_project = make_project(pool, organization_id, "BOB", bob).await;
    let alice_workflow = make_workflow(pool, organization_id, alice_project, "Nightly digest").await;

    Fixture {
        organization_id,
        alice_project,
        bob_project,
        default_project,
        alice,
        bob,
        alice_workflow,
    }
}

fn request(user_id: Uuid, organization_id: Uuid, projects: Option<Vec<Uuid>>) -> SearchRequest {
    SearchRequest {
        query: SearchQuery::parse("Nightly").expect("the term parses"),
        filters: Default::default(),
        providers: vec!["workflows"],
        organization_id: Some(organization_id),
        project_ids: projects,
        user_id,
        page: 1,
        per_page: 50,
        sort: Sort::Relevance,
    }
}

async fn visible(pool: &PgPool, fx: &Fixture, who: Uuid) -> Vec<Uuid> {
    projects::visible_project_ids(
        pool,
        fx.organization_id,
        ProjectCaller {
            user_id: who,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list")
}

async fn ids(pool: &PgPool, fx: &Fixture, who: Uuid) -> Vec<String> {
    let scope = visible(pool, fx, who).await;
    query::search(pool, &request(who, fx.organization_id, Some(scope)))
        .await
        .expect("the search runs")
        .hits
        .iter()
        .map(|hit| hit.entity_id.clone())
        .collect()
}

/// The project stored on the indexed document, read back from the row.
async fn indexed_project(pool: &PgPool, workflow: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(
        "select project_id from search_documents where provider = 'workflows' and entity_id = $1::text",
    )
    .bind(workflow)
    .fetch_one(pool)
    .await
    .expect("the index row exists; the workflow was indexed")
}

#[tokio::test]
async fn the_move_alone_must_re_stamp_the_index() {
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let workflow = fx.alice_workflow.to_string();

    // The state the move is supposed to change: alice sees it, bob does not.
    assert!(
        ids(&pool, &fx, fx.alice).await.contains(&workflow),
        "precondition: the workflow is findable by the owner of its project"
    );
    assert!(
        !ids(&pool, &fx, fx.bob).await.contains(&workflow),
        "precondition: it is not findable by a member of another project"
    );

    // The move under test — the function the API calls. No reindex anywhere after this point:
    // that is the shipped behaviour, and a test that re-indexed would be testing the reindex.
    move_workflow(
        &pool,
        fx.organization_id,
        fx.alice_workflow,
        fx.bob_project,
        Some(fx.alice),
        false,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the move succeeds");

    // The precondition the rest of this file stands on, asserted after the move as well as
    // before: without this the gate would be able to prove search wrong about a workflow the
    // move never touched.
    let stored: Uuid =
        sqlx::query_scalar("select project_id from workflows where id = $1")
            .bind(fx.alice_workflow)
            .fetch_one(&pool)
            .await
            .expect("the workflow row");
    assert_eq!(
        stored, fx.bob_project,
        "the workflow row really did move — otherwise this gate is measuring nothing"
    );

    let bob_hits = ids(&pool, &fx, fx.bob).await;
    assert!(
        bob_hits.contains(&workflow),
        "the member who GAINED access cannot find it: the index still names the old project \
         and the scoping clause filters on the stored value, got {bob_hits:?}"
    );

    let alice_hits = ids(&pool, &fx, fx.alice).await;
    assert!(
        !alice_hits.contains(&workflow),
        "THE DEFECT: a workflow moved out of alice's project is still findable by alice. \
         `move_workflow` writes workflows.project_id and the audit row but never \
         search_documents.project_id, and no workflow write path re-indexes, so the person who \
         LOST access is the one who keeps finding it. Got {alice_hits:?}"
    );
}

#[tokio::test]
async fn the_index_row_names_the_project_the_workflow_left() {
    // The same fact one layer down, so a failure says WHICH half is wrong instead of only "the
    // search leaked". A caller reading a green search test has to be able to trust this row.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    move_workflow(
        &pool,
        fx.organization_id,
        fx.alice_workflow,
        fx.bob_project,
        Some(fx.alice),
        false,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the move succeeds");

    let indexed = indexed_project(&pool, fx.alice_workflow).await;
    assert_eq!(
        indexed,
        Some(fx.bob_project),
        "search_documents.project_id still names the project the workflow left. The scoping \
         clause in query::search reads this column, so a stale value is a stale scope."
    );
}

#[tokio::test]
async fn a_dry_run_reports_the_move_without_stamping_or_writing() {
    // The dry run writes nothing at all, so it must not stamp the index either — and a stamp
    // here would be worse than a missing one, because the report a dialog renders would then
    // disagree with the store.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let report = move_workflow(
        &pool,
        fx.organization_id,
        fx.alice_workflow,
        fx.bob_project,
        Some(fx.alice),
        true,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the dry run succeeds");

    assert!(report.dry_run, "the report says it was a dry run");
    assert_eq!(report.from_project_id, fx.alice_project, "the source is named");
    assert_eq!(report.to_project_id, fx.bob_project, "the target is named");

    let stored: Uuid = sqlx::query_scalar("select project_id from workflows where id = $1")
        .bind(fx.alice_workflow)
        .fetch_one(&pool)
        .await
        .expect("the workflow row");
    assert_eq!(
        stored, fx.alice_project,
        "a dry run must write nothing to the workflow row"
    );
    assert_eq!(
        indexed_project(&pool, fx.alice_workflow).await,
        Some(fx.alice_project),
        "a dry run must not stamp the index either — the report a dialog renders has to \
         describe the store as it actually is"
    );
}

#[tokio::test]
async fn the_scope_still_answers_after_a_move_it_was_not_part_of() {
    // The negative control. A fix that empties the index, or that filters on something the move
    // destroyed, would make the two assertions above pass. This is what shows the gate names the
    // defect and not its neighbourhood: unrelated searching keeps working, and the shared default
    // project stays reachable for both accounts.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    // A second workflow in the shared default project, which neither account owns alone.
    // Indexed AFTER it is written: a row that was never indexed cannot be asserted on, and
    // asserting it anyway is the fixture bug this file is about.
    let shared = make_workflow(&pool, fx.organization_id, fx.default_project, "Nightly shared").await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    move_workflow(
        &pool,
        fx.organization_id,
        fx.alice_workflow,
        fx.bob_project,
        Some(fx.alice),
        false,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the move succeeds");

    for who in [fx.alice, fx.bob] {
        let hits = ids(&pool, &fx, who).await;
        assert!(
            hits.contains(&shared.to_string()),
            "the shared default project must stay reachable after a move (account {who}), \
             got {hits:?}"
        );
    }
}