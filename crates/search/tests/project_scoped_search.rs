//! Global search answers only with the caller's own projects (REQ-133, acceptance 5) — against
//! a real database.
//!
//! The sentence is "Global search (REQ-002) returns only resources the caller may see, **scoped
//! to their projects**, verified with two accounts", and the scoping half had no code anywhere on
//! this branch: `grep -rn project crates/search/src/` returned nothing at all, `search_documents`
//! carried `organization_id` and `site_id` from its first migration and had no project column, and
//! the query narrowed by provider and organization only. Two people in ONE organization, in
//! different projects, both passed every check the query made.
//!
//! This file is also the reason the slice needed a *provider*, not only a filter. Adding the
//! project clause alone would have produced the greenest possible lie: with no indexed row
//! carrying a project, the clause narrows nothing, every assertion below would pass, and nothing
//! would have been scoped. So the gate asserts a workflow from the other account's project is
//! actually IN the index and actually absent from the answer — "cannot see it" must mean "there
//! was something to not see".
//!
//! Two accounts is the REQ's own words, and it is the only arrangement that catches a
//! same-organization leak: a single-account fixture passes an organization-only filter, which is
//! precisely the filter that was already there.

use std::env;

use omnion_search::indexer;
use omnion_search::query::{self, Query as SearchQuery, SearchRequest, Sort};
use omnion_workflows::projects::{self, NewProject, ProjectCaller};
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

/// One organization, two accounts, two projects, one workflow in each.
struct Fixture {
    organization_id: Uuid,
    /// The project only `alice` is a member of.
    alice_project: Uuid,
    /// The project only `bob` is a member of.
    bob_project: Uuid,
    /// The organization default project, which BOTH may see.
    default_project: Uuid,
    alice: Uuid,
    bob: Uuid,
    /// A workflow inside `alice_project`, and one inside `bob_project`.
    alice_workflow: Uuid,
    bob_workflow: Uuid,
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

/// A workflow in `project_id`, written the way `resolve_target` would: a name the search can find.
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
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'Search', $2)")
        .bind(organization_id)
        .bind(format!("search-{}", organization_id))
        .execute(pool)
        .await
        .expect("the organization insert is this suite's fixture");

    let mut ids = Vec::new();
    for (role, who) in [("owner", "Alice"), ("owner", "Bob")] {
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
        let _ = role;
        ids.push(id);
    }
    let alice = ids[0];
    let bob = ids[1];

    // The default project, made the way the 0164 backfill makes it: `NewProject` carries no
    // `is_default`, and the flag is what makes it visible to every member. Without it, both
    // accounts would start with an empty project list and the leak test would be vacuous.
    let default_project = make_project(pool, organization_id, "MAIN", alice).await;
    sqlx::query("update automation_projects set is_default = true where id = $1")
        .bind(default_project)
        .execute(pool)
        .await
        .expect("flagging the default project is this suite's fixture");

    let alice_project = make_project(pool, organization_id, "ALICE", alice).await;
    let bob_project = make_project(pool, organization_id, "BOB", bob).await;

    // A workflow in each side's project. The name is shared on purpose: both accounts type the
    // same word, and the difference in the answers is membership, not ranking.
    let alice_workflow = make_workflow(pool, organization_id, alice_project, "Nightly digest").await;
    let bob_workflow = make_workflow(pool, organization_id, bob_project, "Nightly digest").await;

    Fixture {
        organization_id,
        alice_project,
        bob_project,
        default_project,
        alice,
        bob,
        alice_workflow,
        bob_workflow,
    }
}

/// The engine request a caller would make, with the project scope the route computes.
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

#[tokio::test]
async fn a_workflow_from_another_accounts_project_is_absent_from_the_answer() {
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;

    // Reindex, so the index holds what the upsert would write in production. The workflows
    // provider's own report is read back: a reindex that indexed nothing would make every
    // "cannot see it" assertion below pass for the wrong reason.
    let report = indexer::reindex(&pool, "workflows")
        .await
        .expect("the workflows reindex runs");
    assert!(
        report.indexed >= 2,
        "both workflows must be indexed before anything can be hidden, got {}",
        report.indexed
    );

    let alice_visible = projects::visible_project_ids(
        &pool,
        fx.organization_id,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list");
    assert!(
        alice_visible.contains(&fx.alice_project) && !alice_visible.contains(&fx.bob_project),
        "the fixture itself must put the two accounts in different projects: {alice_visible:?}"
    );

    let page = query::search(&pool, &request(fx.alice, fx.organization_id, Some(alice_visible)))
        .await
        .expect("the search runs");
    let ids: Vec<String> = page.hits.iter().map(|hit| hit.entity_id.clone()).collect();

    assert!(
        ids.contains(&fx.alice_workflow.to_string()),
        "alice must see her own project's workflow, got {ids:?}"
    );
    assert!(
        !ids.contains(&fx.bob_workflow.to_string()),
        "alice must not see bob's project workflow, got {ids:?}"
    );
    // The disclosure half: a name is not the leak, a link to a screen that 404s is. Asserting
    // the subtitle is absent as well is what stops a "scoped but still describes it" answer.
    assert!(
        !page
            .hits
            .iter()
            .any(|hit| hit.subtitle.contains("Project BOB")),
        "the hit must not describe the foreign project: {:?}",
        page.hits.iter().map(|h| &h.subtitle).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_other_side_is_not_a_special_case_and_sees_the_mirror_image() {
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let bob_visible = projects::visible_project_ids(
        &pool,
        fx.organization_id,
        ProjectCaller {
            user_id: fx.bob,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list");
    let page = query::search(&pool, &request(fx.bob, fx.organization_id, Some(bob_visible)))
        .await
        .expect("the search runs");
    let ids: Vec<String> = page.hits.iter().map(|hit| hit.entity_id.clone()).collect();

    assert!(
        ids.contains(&fx.bob_workflow.to_string()),
        "bob must see his own project's workflow, got {ids:?}"
    );
    assert!(
        !ids.contains(&fx.alice_workflow.to_string()),
        "bob must not see alice's project workflow, got {ids:?}"
    );
}

#[tokio::test]
async fn the_shared_default_project_stays_visible_to_both() {
    // The scope must not become a wall. The default project belongs to everyone, and a filter
    // that hid it would be a "scoping fix" that empties the automation screens — the failure
    // mode that makes an operator prefer a leak over the fix.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    let shared = make_workflow(&pool, fx.organization_id, fx.default_project, "Nightly digest")
        .await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let alice_visible = projects::visible_project_ids(
        &pool,
        fx.organization_id,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list");
    assert!(
        alice_visible.contains(&fx.default_project),
        "the default project must be in the visible set"
    );

    let page = query::search(&pool, &request(fx.alice, fx.organization_id, Some(alice_visible)))
        .await
        .expect("the search runs");
    assert!(
        page.hits
            .iter()
            .any(|hit| hit.entity_id == shared.to_string()),
        "a workflow in the shared default project must be searchable by a member"
    );
}

#[tokio::test]
async fn an_instance_administrator_is_not_filtered_by_membership() {
    // `None` means "no filtering" and is the answer for an instance administrator. A member of
    // no project at all is a different answer — `Some(vec![])` — and a caller that is neither
    // must not inherit the administrator's unfiltered answer by accident.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let page = query::search(&pool, &request(fx.alice, fx.organization_id, None))
        .await
        .expect("the search runs");
    let ids: Vec<String> = page.hits.iter().map(|hit| hit.entity_id.clone()).collect();
    assert!(
        ids.contains(&fx.bob_workflow.to_string()),
        "an unfiltered scope must reach every project in the organization, got {ids:?}"
    );
}

#[tokio::test]
async fn a_member_of_no_project_sees_no_project_workflow_at_all() {
    // `Some(vec![])` is a real answer, not an empty filter that leaks everything: `= any('{}')`
    // is false for every row, so the caller gets nothing rather than everything.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let outsider: Uuid = sqlx::query_scalar(
        "insert into users (id, email, display_name, organization_id) \
         values ($1, $2, 'Outsider', $3) returning id",
    )
    .bind(Uuid::new_v4())
    .bind(format!("outsider-{}@example.test", Uuid::new_v4()))
    .bind(fx.organization_id)
    .fetch_one(&pool)
    .await
    .expect("the outsider insert is this suite's fixture");

    let page = query::search(
        &pool,
        &request(outsider, fx.organization_id, Some(Vec::new())),
    )
    .await
    .expect("the search runs");
    assert!(
        page.hits.is_empty(),
        "a member of no project must see no project workflow, got {:?}",
        page.hits.iter().map(|h| &h.entity_id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_suggestion_query_is_narrowed_by_the_same_rule_as_the_results() {
    // The palette's suggestion path is a **second statement** over the same index. Scoping only
    // the results screen leaves the first paint of the ⌘K box answering with a foreign project's
    // workflow — the leak surviving in the path people touch first, and invisible to every other
    // test in this file because they all go through `query::search`.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let alice_visible = projects::visible_project_ids(
        &pool,
        fx.organization_id,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list");

    let rows = query::suggest(
        &pool,
        Some(fx.organization_id),
        &["workflows"],
        Some(&alice_visible),
        "Nightly",
    )
    .await
    .expect("the suggestion query runs");
    assert!(
        !rows.is_empty(),
        "the fixture's own workflow must be suggested, or the assertion below is vacuous"
    );
    assert_eq!(
        rows.len(),
        1,
        "exactly one account's workflow may be suggested, got {:?}",
        rows.iter().map(|row| &row.title).collect::<Vec<_>>()
    );
    assert!(
        rows.iter().all(|row| row.url.contains(&fx.alice_workflow.to_string())),
        "the suggestion must be the caller's own: {:?}",
        rows.iter().map(|row| &row.url).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_workflow_moved_to_another_project_stops_answering_for_the_old_membership() {
    // The conflict tail refreshes `project_id`; without that line the index keeps the old
    // membership for ever, and a moved workflow stays visible to exactly the people who lost
    // access to it. This is the assertion that makes the move surface and the search agree.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    sqlx::query("update workflows set project_id = $1 where id = $2")
        .bind(fx.bob_project)
        .bind(fx.alice_workflow)
        .execute(&pool)
        .await
        .expect("the move is this suite's fixture");
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let alice_visible = projects::visible_project_ids(
        &pool,
        fx.organization_id,
        ProjectCaller {
            user_id: fx.alice,
            is_instance_admin: false,
        },
    )
    .await
    .expect("the visible project list");
    let page = query::search(&pool, &request(fx.alice, fx.organization_id, Some(alice_visible)))
        .await
        .expect("the search runs");
    let ids: Vec<String> = page.hits.iter().map(|hit| hit.entity_id.clone()).collect();
    assert!(
        !ids.contains(&fx.alice_workflow.to_string()),
        "a workflow moved out of alice's project must stop answering for her, got {ids:?}"
    );
}

#[tokio::test]
async fn the_index_row_carries_the_project_and_the_scoping_holds_inside_the_facet_counts() {
    // The fact the rest of this file stands on, asserted directly so a failure names which one
    // broke: the row stores the project. Without it the clause has nothing to match and every
    // "cannot see it" assertion above is vacuously true.
    //
    // The second half is `counts` — the facet rail's own per-provider totals. It is a separate
    // statement over the same index, and it is where a scope that only reached `search()` would
    // show up as a lie: a rail reading "Workflows 2" above a result list showing one row is the
    // disclosure this acceptance is about, told twice. (`hidden_count` is deliberately NOT
    // asserted here — it counts *providers the caller cannot read at all*, which is a different
    // question; an earlier version of this test asserted 1 there and was wrong about the
    // function, not about the product.)
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    indexer::reindex(&pool, "workflows").await.expect("reindex");

    let stored: Option<Uuid> = sqlx::query_scalar(
        "select project_id from search_documents \
         where provider = 'workflows' and entity_id = $1",
    )
    .bind(fx.bob_workflow.to_string())
    .fetch_one(&pool)
    .await
    .expect("the indexed row exists");
    assert_eq!(
        stored,
        Some(fx.bob_project),
        "the indexed document must carry its project, or the clause has nothing to match"
    );

    let alice_visible = vec![fx.default_project, fx.alice_project];
    let counts = query::counts(
        &pool,
        &request(fx.alice, fx.organization_id, Some(alice_visible)),
    )
    .await
    .expect("the facet counts run");
    let workflows: i64 = counts
        .iter()
        .find(|row| row.provider == "workflows")
        .map(|row| row.count)
        .unwrap_or_default();
    assert_eq!(
        workflows, 1,
        "the facet rail must count the one workflow inside alice's scope and not bob's — a rail \
         reading the whole index is the disclosure this acceptance is about, told twice; got \
         {workflows} for {:?}",
        counts
            .iter()
            .map(|row| (&row.provider, row.count))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_page_containing_no_project_row_is_still_reachable() {
    // The four non-project providers (pages, media, users, sites) write `project_id = null`, and
    // the clause reads `d.project_id is null or ...`. Without the null arm this suite would
    // still be green — it only ever asks about workflows — while the search results screen
    // emptied of everything except automations. The index needs a site and a page to prove it.
    let Some(pool) = pool().await else { return };
    let fx = fixture(&pool).await;
    let site_id: Uuid = sqlx::query_scalar(
        "insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4) returning id",
    )
    .bind(Uuid::new_v4())
    .bind(fx.organization_id)
    .bind(format!("s{}", Uuid::new_v4()))
    .bind("Nightly landing")
    .fetch_one(&pool)
    .await
    .expect("the site insert is this suite's fixture");
    let page_id: Uuid = sqlx::query_scalar(
        "insert into pages (id, site_id, slug, status) \
         values ($1, $2, 'nightly', 'published') returning id",
    )
    .bind(Uuid::new_v4())
    .bind(site_id)
    .fetch_one(&pool)
    .await
    .expect("the page insert is this suite's fixture");
    sqlx::query(
        "insert into page_revisions (page_id, revision_no, title, state) \
         values ($1, 1, 'Nightly landing', 'published')",
    )
    .bind(page_id)
    .execute(&pool)
    .await
    .expect("the revision insert is this suite's fixture");
    indexer::reindex(&pool, "pages").await.expect("reindex");

    let mut request = request(fx.alice, fx.organization_id, Some(Vec::new()));
    request.providers = vec!["pages"];
    let page = query::search(&pool, &request).await.expect("the search runs");
    assert!(
        page.hits
            .iter()
            .any(|hit| hit.entity_id == page_id.to_string()),
        "a page with no project must stay searchable under an empty project scope, got {:?}",
        page.hits.iter().map(|h| &h.entity_id).collect::<Vec<_>>()
    );
}
