//! The project switcher persists a choice and ranks it (REQ-133, acceptance 3) — against a real
//! database.
//!
//! The sentence this file exists for is "the selection **persists per user** and is written into
//! URLs", and the half that had no code was the persistence: `ProjectListQuery.mine` was declared on
//! the API's query struct, the REQ's API table named `GET /api/v1/projects?mine=1` as "the
//! switcher's list", and nothing on this branch read the parameter — so the switcher had no
//! server-side list and no way to remember a choice. A browser-only selection would have satisfied
//! "survives navigation" and failed the "per user" half on a second device, which is why the tests
//! below drive the *store* and read the rows back rather than asserting a constant.
//!
//! The ranking claim is the one worth having a gate for. "Recents first" is an `order by` with
//! `nulls last`, and the demotion of the previous head is a `case when rank = 0 then null` — two
//! clauses that are individually reasonable and jointly wrong in the easy way: a naive
//! `on conflict do nothing` upsert pins the first project ever selected at rank 0 for ever, so the
//! project a person is working in today sits below one they touched once a fortnight ago. That is
//! the *shape* of a feature working while being wrong, and a unit test over a helper would not see
//! it.

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

struct Fixture {
    organization_id: Uuid,
    /// The organization's default project, created by the 0164 backfill.
    default_project: Uuid,
    /// The person doing the switching.
    user_id: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, 'Switcher', $2)")
        .bind(organization_id)
        .bind(format!("switcher-{}", organization_id))
        .execute(pool)
        .await
        .expect("the organization insert is this suite's fixture");
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Switcher')")
        .bind(user_id)
        .bind(format!("switcher-{}@example.test", user_id))
        .execute(pool)
        .await
        .expect("the user insert is this suite's fixture");
    // The default project, made through the insert path — `NewProject` has no `is_default`, and
    // the flag is set the way the migration's backfill sets it. Creating the project and then
    // flagging it exercises the singleton index the switcher's "the default is always visible"
    // clause depends on.
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
    let default_project = created.id;

    Fixture {
        organization_id,
        default_project,
        user_id,
    }
}

fn caller_for(user_id: Uuid, is_instance_admin: bool) -> ProjectCaller {
    ProjectCaller {
        user_id,
        is_instance_admin,
    }
}

/// A second project the caller is a member of, with a distinct key.
async fn another_project(pool: &PgPool, org: Uuid, owner: Uuid, key: &str) -> Uuid {
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
    projects::upsert_member(pool, project.id, owner, ProjectRole::Editor, Some(owner))
        .await
        .expect("the membership insert is this suite's fixture");
    project.id
}

#[tokio::test]
async fn the_selection_persists_and_is_answered_as_a_row_flag() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // Nobody has chosen anything yet, and the switcher has to be able to say so — a client that
    // cannot distinguish "never chosen" from "chose the default" is a switcher that lies on the
    // first load of every fresh account.
    let before = projects::selected_project(&pool, f.organization_id, caller)
        .await
        .expect("the selection is read");
    assert_eq!(before, None, "a fresh account must report no selection, not the default");

    let other = another_project(&pool, f.organization_id, f.user_id, "OPS").await;
    let chosen = projects::select_project(&pool, f.organization_id, other, caller)
        .await
        .expect("the switch writes");
    assert_eq!(chosen.id, other);

    // Read back through a DIFFERENT function than the one that wrote it: `select_project` returns
    // the project it read, so asserting on its return value would pass even if the selection row
    // were never written at all.
    let after = projects::selected_project(&pool, f.organization_id, caller)
        .await
        .expect("the selection is read back");
    assert_eq!(after, Some(other), "the selection did not persist");

    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is read");
    let flags: Vec<_> = entries
        .iter()
        .map(|entry| (entry.project.id, entry.selected))
        .collect();
    assert_eq!(
        flags,
        vec![(other, true), (f.default_project, false)],
        "exactly one row may be marked, and it must be the chosen one"
    );
}

#[tokio::test]
async fn the_selection_is_per_user_and_two_people_do_not_share_one() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;

    let second = Uuid::new_v4();
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Second')")
        .bind(second)
        .bind(format!("second-{}@example.test", second))
        .execute(&pool)
        .await
        .expect("the second user insert is this suite's fixture");

    let ops = another_project(&pool, f.organization_id, f.user_id, "OPS").await;
    projects::upsert_member(&pool, ops, second, ProjectRole::Viewer, Some(f.user_id))
        .await
        .expect("the second membership insert is this suite's fixture");

    projects::select_project(&pool, f.organization_id, ops, caller_for(f.user_id, false))
        .await
        .expect("the first switch writes");

    // The second person has never switched, and the point of storing it server-side per user is
    // that they must NOT inherit the first person's choice — a single "current project" column
    // would answer this wrong and a cookie-scoped one would hide it entirely.
    assert_eq!(
        projects::selected_project(&pool, f.organization_id, caller_for(second, false))
            .await
            .expect("the second selection is read"),
        None,
        "one person's switch leaked to another"
    );

    // And the recents are per user too: the second person's list must be empty, so their
    // switcher shows the projects rather than a ranking they never produced.
    let their_entries = projects::list_switcher_entries(&pool, f.organization_id, caller_for(second, false))
        .await
        .expect("the second switcher list is read");
    assert!(
        their_entries.iter().all(|entry| entry.recent_rank.is_none()),
        "another person's recents leaked: {:?}",
        their_entries
            .iter()
            .map(|entry| (entry.project.key.clone(), entry.recent_rank))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn recents_are_ranked_most_recent_first_and_the_previous_head_demotes() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    let a = another_project(&pool, f.organization_id, f.user_id, "AAAA").await;
    let b = another_project(&pool, f.organization_id, f.user_id, "BBBB").await;
    let c = another_project(&pool, f.organization_id, f.user_id, "CCCC").await;

    // Select in a known order. The default project is not in the recents, so the list is exactly
    // these three plus the default, and the assertion below is on the first three.
    projects::select_project(&pool, f.organization_id, a, caller)
        .await
        .expect("a is selected");
    projects::select_project(&pool, f.organization_id, b, caller)
        .await
        .expect("b is selected");
    projects::select_project(&pool, f.organization_id, c, caller)
        .await
        .expect("c is selected");

    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is read");
    let order: Vec<_> = entries
        .iter()
        .map(|entry| (entry.project.key.clone(), entry.recent_rank))
        .collect();
    assert_eq!(
        order,
        vec![
            ("CCCC".to_string(), Some(0)),
            ("BBBB".to_string(), Some(1)),
            ("AAAA".to_string(), Some(2)),
            // The default was never selected, so it has no rank and sorts last — the nullable
            // rank is the fact "you have never been here", not a zero standing in for it.
            ("MAIN".to_string(), None),
        ],
        "recents are not ranked most-recent-first"
    );

    // The claim in the sentence is that the OLD head moves down rather than staying at 0. Select
    // `a` again and read the rows: a naive `on conflict do nothing` upsert leaves `a` at its old
    // rank and the second insert does nothing, so the list keeps `c` at 0 — this is the exact
    // failure, asserted before the fix rather than after.
    projects::select_project(&pool, f.organization_id, a, caller)
        .await
        .expect("a is selected again");
    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is re-read");
    let order: Vec<_> = entries
        .iter()
        .map(|entry| (entry.project.key.clone(), entry.recent_rank))
        .collect();
    assert_eq!(
        order,
        vec![
            ("AAAA".to_string(), Some(0)),
            ("CCCC".to_string(), Some(1)),
            ("BBBB".to_string(), Some(2)),
            ("MAIN".to_string(), None),
        ],
        "re-selecting a project did not move it to the front and demote the previous head"
    );
}

#[tokio::test]
async fn a_project_outside_the_window_is_dropped_rather_than_left_at_a_sparse_rank() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    // Ten distinct projects selected in order: the window is eight, so two must be gone rather
    // than sitting at rank 8 and 9 — and a `check (rank <= 7)` that rejected them would make the
    // tenth switch an error instead of an eviction, which is the other way this gets written.
    let mut keys = Vec::new();
    for index in 0..10 {
        let key = format!("K{index}");
        let id = another_project(&pool, f.organization_id, f.user_id, &key).await;
        projects::select_project(&pool, f.organization_id, id, caller)
            .await
            .unwrap_or_else(|error| panic!("switch {index} failed: {error}"));
        keys.push((key, id));
    }

    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is read");
    let ranked: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.recent_rank.map(|rank| (entry.project.key.clone(), rank)))
        .collect();
    assert_eq!(ranked.len(), 8, "the window is eight, got {ranked:?}");

    // Dense: every rank from 0 to 7 appears exactly once. A gap is what a sparse list looks like,
    // and the switcher renders gaps as "no project".
    let mut ranks: Vec<i16> = ranked.iter().map(|(_, rank)| *rank).collect();
    ranks.sort_unstable();
    assert_eq!(ranks, (0..8).collect::<Vec<_>>(), "the ranks are not dense");

    // The two oldest are the ones dropped, and the newest is the head.
    assert_eq!(ranked[0].0, "K9", "the newest selection is not the head");
    let survivors: Vec<&str> = ranked.iter().map(|(key, _)| key.as_str()).collect();
    assert!(!survivors.contains(&"K0") && !survivors.contains(&"K1"), "oldest kept: {survivors:?}");
    let (_dropped, _) = (&keys[0], &keys[1]);
}

#[tokio::test]
async fn the_switcher_is_membership_scoped_and_the_admin_reach_is_reachable() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;

    // A second person owns two projects the first person is in neither of.
    let owner = Uuid::new_v4();
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Owner')")
        .bind(owner)
        .bind(format!("owner-{}@example.test", owner))
        .execute(&pool)
        .await
        .expect("the owner insert is this suite's fixture");
    let payroll = another_project(&pool, f.organization_id, owner, "PAY").await;
    let billing = another_project(&pool, f.organization_id, owner, "BILL").await;

    // Neither caller is a member of either project, so neither sees them.
    let member_ids: Vec<_> = projects::list_switcher_entries(&pool, f.organization_id, caller_for(f.user_id, false))
        .await
        .expect("the member switcher is read")
        .iter()
        .map(|e| e.project.id)
        .collect();
    assert!(
        !member_ids.contains(&payroll) && !member_ids.contains(&billing),
        "a member's switcher listed a project they are not in"
    );
    let admin_ids: Vec<_> = projects::list_switcher_entries(&pool, f.organization_id, caller_for(f.user_id, true))
        .await
        .expect("the admin switcher is read")
        .iter()
        .map(|e| e.project.id)
        .collect();
    assert!(
        !admin_ids.contains(&payroll) && !admin_ids.contains(&billing),
        "the administrator's rows are membership-scoped too -- see the note below"
    );

    // **The rows are membership-scoped FOR EVERYONE, and that is the decision, not an oversight.**
    // The REQ asks for "`All projects` for instance admins only" in the same sentence as "recents
    // first", and the reason is the sentence after it: "a switcher that lists forty projects is a
    // list, and this is a switcher". So an administrator's forty projects arrive as ONE entry the
    // panel renders, and their own rows stay the projects they are actually a member of. A first
    // version of this test asserted the opposite -- that the admin's rows list everything -- and
    // the gate passed it only because the test and the implementation were written from the same
    // wrong reading. It is recorded here because "the admin sees more" and "the admin's list is
    // longer" are the sentence and its opposite.
    //
    // What must be true, and is asserted below, is that the administrator's REACH is real: they
    // can select a project they are not a member of, and it then appears in their switcher marked
    // as manageable. An administrator who could see forty projects but never switch into one would
    // have a switcher that lies about its own contents.
    let chosen = projects::select_project(&pool, f.organization_id, payroll, caller_for(f.user_id, true))
        .await
        .expect("an administrator can switch into a project they are not a member of");
    assert_eq!(chosen.id, payroll);

    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller_for(f.user_id, true))
        .await
        .expect("the admin switcher is re-read");
    let entry = entries
        .iter()
        .find(|entry| entry.project.id == payroll)
        .expect("the selected project is now in the administrator's switcher");
    assert!(entry.selected, "the newly selected project is not marked");
    assert!(entry.can_manage, "an administrator's own entry is not marked manageable");
    assert_eq!(entry.recent_rank, Some(0), "the newly selected project is not the recents head");
    // `caller_role` stays `None`: the administrator is not a MEMBER of it, and reporting
    // `owner` there would put a membership in the audit trail that does not exist. The two facts
    // are separate and this is where the store keeps them separate.
    assert_eq!(
        entry.caller_role, None,
        "the administrator was reported as a member of a project they are not in"
    );
}

#[tokio::test]
async fn selecting_a_project_the_caller_cannot_see_is_refused_and_writes_nothing() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);

    let other = Uuid::new_v4();
    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Stranger')")
        .bind(other)
        .bind(format!("stranger-{}@example.test", other))
        .execute(&pool)
        .await
        .expect("the stranger insert is this suite's fixture");
    let private = another_project(&pool, f.organization_id, other, "PRIV").await;

    // The refusal, and its code: `404`, not `403` — the same answer as every other project read,
    // because a different one would confirm the row exists.
    let error = projects::select_project(&pool, f.organization_id, private, caller)
        .await
        .expect_err("selecting an invisible project must be refused");
    assert_eq!(error.code(), "project_not_found", "the refusal uses the wrong code");

    // And the refusal wrote nothing. A switcher that records a selection it just refused leaves
    // the next load highlighting a project the person cannot open, which is the same lie the
    // 404-versus-403 rule exists to prevent — one layer up.
    assert_eq!(
        projects::selected_project(&pool, f.organization_id, caller)
            .await
            .expect("the selection is read"),
        None,
        "the refused switch still wrote a selection"
    );
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from automation_project_recent where user_id = $1",
    )
    .bind(f.user_id)
    .fetch_one(&pool)
    .await
    .expect("the recents are counted");
    assert_eq!(rows, 0, "the refused switch still wrote a recents row");
}

#[tokio::test]
async fn clearing_the_selection_is_a_choice_and_does_not_disturb_the_recents() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);
    let other = another_project(&pool, f.organization_id, f.user_id, "OPS").await;

    projects::select_project(&pool, f.organization_id, other, caller)
        .await
        .expect("the switch writes");
    assert_eq!(
        projects::clear_selection(&pool, f.user_id).await.expect("the clear runs"),
        true,
        "clearing an existing selection must report that it cleared one"
    );
    assert_eq!(
        projects::selected_project(&pool, f.organization_id, caller)
            .await
            .expect("the selection is read"),
        None,
        "the selection survived the clear"
    );

    // The recents are history, not a mirror of the current selection: "All projects" means "stop
    // filtering by one", not "forget where you have been". Clearing the *selection* and dropping
    // the *ranking* together would make the All-projects entry cost the reader their order.
    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is read");
    let ranked: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.recent_rank.map(|rank| (entry.project.key.clone(), rank)))
        .collect();
    assert_eq!(ranked, vec![("OPS".to_string(), 0)], "clearing the selection disturbed the recents");
    assert!(
        entries.iter().all(|entry| !entry.selected),
        "a cleared selection still marks a row"
    );

    // Idempotent: pressing "All projects" twice is not an error.
    assert_eq!(
        projects::clear_selection(&pool, f.user_id).await.expect("the second clear runs"),
        false,
        "the second clear must report that there was nothing to clear"
    );
}

#[tokio::test]
async fn a_selection_whose_project_is_deleted_leaves_the_reader_in_the_default_state() {
    let Some(pool) = pool().await else { return };
    let f = fixture(&pool).await;
    let caller = caller_for(f.user_id, false);
    let doomed = another_project(&pool, f.organization_id, f.user_id, "DOOM").await;

    projects::select_project(&pool, f.organization_id, doomed, caller)
        .await
        .expect("the switch writes");
    assert_eq!(
        projects::selected_project(&pool, f.organization_id, caller)
            .await
            .expect("the selection is read"),
        Some(doomed)
    );

    // `on delete cascade` on the selection row. A switcher whose current entry is a deleted project
    // renders a 404 on load and has no way out, because the entry it would have to press to escape
    // is the one that is gone.
    sqlx::query("delete from automation_projects where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the project delete is this suite's fixture");

    assert_eq!(
        projects::selected_project(&pool, f.organization_id, caller)
            .await
            .expect("the selection is read after the delete"),
        None,
        "a selection pointing at a deleted project was left behind"
    );
    let entries = projects::list_switcher_entries(&pool, f.organization_id, caller)
        .await
        .expect("the switcher list is read after the delete");
    assert!(entries.iter().all(|entry| !entry.selected), "a deleted project is still marked");
}
