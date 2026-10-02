//! Integration tests for AI identities and the permission matrix (REQ-100, slice 2).
//!
//! The unit tests in `identity.rs` prove the *rules* — the tri-state's stored form, the deny's
//! ordering, the key's shape. These walks prove the things only a database can answer, and every
//! one of them is about **a row and a rule agreeing**, never about the happy path:
//!
//! - the **tri-state persists exactly**. The spec's own words: "an inherited cell writes no grant
//!   row, a deny writes an `effect = false` row, and re-toggling to inherit removes it". A UI that
//!   *looks* right while a stale deny survives in the table produces an agent that is refused a
//!   tool its operator un-refused last Tuesday — and nothing on any screen shows it.
//! - the **folded** unique index is real. `unique (organization_id, key)` does not fire for NULL
//!   in PostgreSQL, so a plain constraint would let every tenant install a row that claims to be
//!   the platform default.
//! - **a deny survives an allow.** Both are rows on the same pair, so the question of which one
//!   wins is decided in [`identity::resolve`] and the walks prove the row the resolver reads is
//!   the row that was written.
//! - **a bulk replace is a replace.** Removing a cell from a map has to remove the row; a merge
//!   would keep a deny alive forever and the editor would be lying about the default.
//! - **tenancy**: another organization's identity is `None`, never a 403 — the status code must
//!   not be an existence oracle for the whole installation's AI permissions.
//! - **the one-default index** really holds, and a walk that tries to break it gets refused
//!   rather than quietly creating a second default.

use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::identity::{self, GrantEffect, NewIdentity};
use omnion_ai_hub::registry;
use omnion_ai_hub::run_store::{NewAgent, create_agent};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct Identities {
    pool: PgPool,
    organization_id: Uuid,
    other_organization_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl Identities {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("skipping: PostgreSQL is not reachable: {err}");
            return None;
        }

        let database = format!("omnion_aidn_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let organization_id = seed_organization(db.pool(), "initech").await;
        let other_organization_id = seed_organization(db.pool(), "umbrella").await;
        // The registry has to exist before a grant can reference it — the FK is on the tool key,
        // and the seeder is the only thing that writes those rows.
        let seeded = registry::seed(db.pool()).await.expect("the registry seed must run");
        assert!(seeded.inserted > 0, "the seeder must insert the v1 tool set");

        Some(Self {
            pool: db.pool().clone(),
            organization_id,
            other_organization_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    fn draft(&self, key: &str) -> NewIdentity {
        NewIdentity {
            organization_id: Some(self.organization_id),
            key: key.to_owned(),
            name: format!("{key} title"),
            description: "A fixture identity.".to_owned(),
            is_default: false,
            created_by: None,
        }
    }

    /// Count the raw rows for one pair, straight from SQL.
    ///
    /// The walks assert on the ROW, not on what the store read back: a store that filtered
    /// `effect is not null` would report the same tri-state while leaving the row behind, and
    /// that is precisely the defect this file exists to catch.
    async fn grant_rows(&self, identity_id: Uuid, tool_key: &str) -> Vec<bool> {
        let effects: Vec<bool> = sqlx::query_scalar(
            "select effect from ai_tool_grants where identity_id = $1 and tool_key = $2",
        )
        .bind(identity_id)
        .bind(tool_key)
        .fetch_all(&self.pool)
        .await
        .expect("the grant rows must be readable");
        effects
    }

    async fn dispose(mut self) {
        self.pool.close().await;
        let database = std::mem::take(&mut self.database);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
                .execute(maintenance.pool())
                .await
                .expect("the temporary database must be removed");
            maintenance.pool().close().await;
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

async fn seed_organization(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(id)
        .bind(label)
        .bind(format!("{label}-{}", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .expect("the fixture organization must be created");
    id
}

macro_rules! identities {
    () => {
        match Identities::fresh().await {
            Some(store) => store,
            None => {
                eprintln!("skipping: PostgreSQL is not reachable");
                return;
            }
        }
    };
}

/// A tool key that is guaranteed to exist, so a walk never asserts on a key a future catalogue
/// change could remove. Read from the registry rather than hard-coded for exactly that reason.
async fn any_tool(store: &Identities) -> String {
    registry::list_tools(&store.pool)
        .await
        .expect("the registry must list")
        .first()
        .expect("the seeder inserted rows")
        .key
        .clone()
}

// -------------------------------------------------------------------------------------------
// The tri-state, which is the whole slice
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_inherited_cell_writes_no_row_at_all() {
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("editor"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;

    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Inherit, None)
        .await
        .expect("inherit must be accepted");

    let rows = store.grant_rows(identity.id, &tool).await;
    assert!(
        rows.is_empty(),
        "inherit must leave no row, found {rows:?} — a stored inherit is a third state in the \
         database that every read would have to remember to filter"
    );
    assert_eq!(
        identity::grants_of(&store.pool, identity.id)
            .await
            .expect("the grant map must read")
            .get(&tool),
        None,
        "and the grant map must report it as absent, not as a value"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_deny_writes_a_false_row_and_a_re_toggle_removes_it() {
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("ops"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;

    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Deny, None)
        .await
        .expect("a deny must be accepted");
    assert_eq!(
        store.grant_rows(identity.id, &tool).await,
        vec![false],
        "a deny is a row whose effect is false, not the absence of an allow"
    );

    // The exact spec sentence: "re-toggling to inherit removes it". A merge instead of a delete
    // would leave this row alive, and the identity would keep refusing a tool its operator
    // un-refused — with nothing on any screen saying so.
    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Inherit, None)
        .await
        .expect("inherit must be accepted again");
    assert!(
        store.grant_rows(identity.id, &tool).await.is_empty(),
        "re-toggling to inherit must DELETE the row, not merely stop applying it"
    );

    store.dispose().await;
}

#[tokio::test]
async fn an_allow_toggled_to_a_deny_replaces_the_row_rather_than_adding_one() {
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("mixed"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;

    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Allow, None)
        .await
        .expect("an allow must be accepted");
    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Deny, None)
        .await
        .expect("a deny must be accepted");

    assert_eq!(
        store.grant_rows(identity.id, &tool).await,
        vec![false],
        "one decision per pair: the pair's unique index has to be an upsert, not two inserts"
    );

    store.dispose().await;
}

#[tokio::test]
async fn the_deny_the_resolver_reads_is_the_row_that_was_written() {
    // The walks so far prove the ROW. This one closes the loop: the pure function the runtime
    // calls must see that row. A resolver reading a different column (or a cached map) would
    // leave every storage assertion green while the runtime allows the call.
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("denied"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;
    let agent_tools = vec![tool.clone()];

    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Deny, None)
        .await
        .expect("a deny must be accepted");
    let stored = identity::grants_of(&store.pool, identity.id)
        .await
        .expect("the grant map must read");
    let resolved = identity::resolve(&stored, &agent_tools, &tool, true);
    assert_eq!(
        resolved.effect,
        GrantEffect::Deny,
        "an agent that lists the tool anyway must still be refused, because the identity denies it"
    );

    // And the same resolver, same rows, with the deny removed — so the refusal above is
    // provably the DENY and not "the tool is broken" or "the agent list is empty".
    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Inherit, None)
        .await
        .expect("inherit must be accepted");
    let cleared = identity::grants_of(&store.pool, identity.id)
        .await
        .expect("the grant map must read again");
    assert_eq!(
        identity::resolve(&cleared, &agent_tools, &tool, true).effect,
        GrantEffect::Allow,
        "with the deny gone and the agent listing the tool, the same rows must now allow it"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_grant_naming_a_tool_the_registry_does_not_carry_is_refused_with_the_key() {
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("typo"))
        .await
        .expect("the identity must be created");

    let refused = identity::set_grant(
        &store.pool,
        identity.id,
        "content.publsh",
        GrantEffect::Allow,
        None,
    )
    .await;
    assert!(
        matches!(refused, Err(AiHubError::ToolNotFound(ref key)) if key == "content.publsh"),
        "a typo must be named, not answered with a foreign-key violation: {refused:?}"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_bulk_replace_drops_the_cells_the_client_removed() {
    // "A cell that is no longer in the map is inherit now" is the contract. A merge would let a
    // deny nobody can see any more keep refusing calls, which is the worst failure this feature
    // has: the operator believes they removed it and the run still dies.
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("bulk"))
        .await
        .expect("the identity must be created");
    let first = any_tool(&store).await;
    let tools = registry::list_tools(&store.pool).await.expect("the registry must list");
    let second = tools
        .get(1)
        .expect("the v1 set has more than one tool")
        .key
        .clone();

    identity::replace_grants(
        &store.pool,
        identity.id,
        &[
            (first.clone(), GrantEffect::Deny),
            (second.clone(), GrantEffect::Allow),
        ],
    )
    .await
    .expect("the first save must be accepted");
    assert_eq!(store.grant_rows(identity.id, &first).await, vec![false]);
    assert_eq!(store.grant_rows(identity.id, &second).await, vec![true]);

    // The second save names only one of them. The other must be gone, not preserved.
    identity::replace_grants(&store.pool, identity.id, &[(second.clone(), GrantEffect::Allow)])
        .await
        .expect("the narrowing save must be accepted");
    assert!(
        store.grant_rows(identity.id, &first).await.is_empty(),
        "a cell absent from the replacement map must lose its row — this is a replace, not a merge"
    );
    assert_eq!(store.grant_rows(identity.id, &second).await, vec![true]);

    store.dispose().await;
}

#[tokio::test]
async fn a_bulk_save_naming_the_same_tool_twice_is_refused_before_anything_is_written() {
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("dup"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;

    let refused = identity::replace_grants(
        &store.pool,
        identity.id,
        &[
            (tool.clone(), GrantEffect::Allow),
            (tool.clone(), GrantEffect::Deny),
        ],
    )
    .await;
    assert!(
        refused.is_err(),
        "a duplicated cell is a client that lost a race; the last write must not silently win"
    );
    assert!(
        store.grant_rows(identity.id, &tool).await.is_empty(),
        "and the refusal must happen BEFORE the delete, or a bad body wipes the identity's grants"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_bulk_save_refusing_one_unknown_tool_writes_none_of_them() {
    // The transaction is the point: half an applied grant map is an identity that allows tools
    // its operator believes it refuses.
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("partial"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;

    let refused = identity::replace_grants(
        &store.pool,
        identity.id,
        &[
            (tool.clone(), GrantEffect::Allow),
            ("ops.nothing_here".to_owned(), GrantEffect::Deny),
        ],
    )
    .await;
    assert!(refused.is_err(), "an unknown key must refuse the whole map");
    assert!(
        store.grant_rows(identity.id, &tool).await.is_empty(),
        "the valid entry must not have been written before the refusal"
    );

    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// Uniqueness, defaults, tenancy
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_second_identity_with_the_same_key_in_the_same_organization_is_refused() {
    let store = identities!();
    identity::create_identity(&store.pool, &store.draft("editor"))
        .await
        .expect("the first must be created");

    let second = identity::create_identity(&store.pool, &store.draft("editor")).await;
    assert!(
        matches!(second, Err(AiHubError::IdentityConflict(_))),
        "the pair index has to hold, or two identities answer to one name: {second:?}"
    );

    store.dispose().await;
}

#[tokio::test]
async fn two_organizations_may_each_hold_the_same_key() {
    // The complement of the walk above, and the reason the index is *folded*: a plain
    // `unique (organization_id, key)` also allows this, but a plain one does not cover the NULL
    // (platform-level) row, which the next walk checks.
    let store = identities!();
    identity::create_identity(&store.pool, &store.draft("editor"))
        .await
        .expect("the first tenant's identity must be created");

    let mut other = store.draft("editor");
    other.organization_id = Some(store.other_organization_id);
    identity::create_identity(&store.pool, &other)
        .await
        .expect("the second tenant may reuse the key — that is what tenant scope means");

    store.dispose().await;
}

#[tokio::test]
async fn a_second_platform_level_identity_with_the_same_key_is_refused() {
    // This is the walk that earns the migration's `coalesce(organization_id, '0000…')` index. A
    // plain unique constraint does not fire for NULL, so without the fold every organization
    // could install a row that claims to be the platform default — and the first agent to
    // resolve an identity would get whichever one the planner returned.
    let store = identities!();
    let platform = NewIdentity {
        organization_id: None,
        key: "shared".to_owned(),
        name: "Shared".to_owned(),
        description: String::new(),
        is_default: false,
        created_by: None,
    };
    identity::create_identity(&store.pool, &platform)
        .await
        .expect("the first platform identity must be created");

    let second = identity::create_identity(&store.pool, &platform).await;
    assert!(
        matches!(second, Err(AiHubError::IdentityConflict(_))),
        "the folded index must refuse a second NULL-organization row: {second:?}"
    );

    store.dispose().await;
}

#[tokio::test]
async fn promoting_a_second_default_moves_the_flag_rather_than_creating_two() {
    let store = identities!();
    let mut first = store.draft("primary");
    first.is_default = true;
    let one = identity::create_identity(&store.pool, &first)
        .await
        .expect("the first default must be created");

    let mut second = store.draft("secondary");
    second.is_default = true;
    let two = identity::create_identity(&store.pool, &second)
        .await
        .expect("promoting the second must be accepted");

    let defaults: i64 = sqlx::query_scalar(
        "select count(*) from ai_identities where organization_id = $1 and is_default",
    )
    .bind(store.organization_id)
    .fetch_one(&store.pool)
    .await
    .expect("the count must read");
    assert_eq!(defaults, 1, "exactly one default per organization, always");

    let reread = identity::get_identity(&store.pool, store.organization_id, one.id)
        .await
        .expect("the read must work")
        .expect("the identity must still be there");
    assert!(!reread.is_default, "the old default must have been demoted");
    assert!(
        identity::get_identity(&store.pool, store.organization_id, two.id)
            .await
            .expect("the read must work")
            .expect("the identity must still be there")
            .is_default,
        "and the new one must carry it"
    );

    store.dispose().await;
}

#[tokio::test]
async fn an_organizations_default_wins_over_the_platform_default() {
    // Shadowing is legal — the folded index treats `(NULL, 'ops')` and `(org, 'ops')` as two
    // different keys — so the read has to state whose row it wants. Without the ordering,
    // shadowing a platform identity is a coin flip.
    let store = identities!();
    let platform = NewIdentity {
        organization_id: None,
        key: "ops".to_owned(),
        name: "Platform ops".to_owned(),
        description: String::new(),
        is_default: true,
        created_by: None,
    };
    identity::create_identity(&store.pool, &platform)
        .await
        .expect("the platform default must be created");

    let mut own = store.draft("ops");
    own.is_default = true;
    identity::create_identity(&store.pool, &own)
        .await
        .expect("the tenant's own default must be created");

    let resolved = identity::default_identity(&store.pool, store.organization_id)
        .await
        .expect("the read must work")
        .expect("a default must resolve");
    assert_eq!(
        resolved.organization_id,
        Some(store.organization_id),
        "the organization's own default must win over the shared one"
    );

    // And the other tenant, which has none of its own, still gets the platform row.
    let other = identity::default_identity(&store.pool, store.other_organization_id)
        .await
        .expect("the read must work")
        .expect("the platform default must be the fallback")
        .organization_id;
    assert_eq!(other, None, "with no row of its own, the tenant gets the shared default");

    store.dispose().await;
}

#[tokio::test]
async fn an_identity_in_another_organization_is_not_found_and_not_refused() {
    // `None`, never a 403: the status code must not confirm that somebody else's identity
    // exists, or the identity list becomes an existence oracle for the installation's AI
    // permissions.
    let store = identities!();
    let mine = identity::create_identity(&store.pool, &store.draft("private"))
        .await
        .expect("the identity must be created");

    let seen = identity::get_identity(&store.pool, store.other_organization_id, mine.id)
        .await
        .expect("the cross-tenant read must work, not error");
    assert!(
        seen.is_none(),
        "another tenant's identity must read as absent, never as forbidden"
    );

    // A platform-level identity is the deliberate exception: it is shared, so it is visible.
    let platform = NewIdentity {
        organization_id: None,
        key: "shared".to_owned(),
        name: "Shared".to_owned(),
        description: String::new(),
        is_default: false,
        created_by: None,
    };
    let shared = identity::create_identity(&store.pool, &platform)
        .await
        .expect("the platform identity must be created");
    assert!(
        identity::get_identity(&store.pool, store.other_organization_id, shared.id)
            .await
            .expect("the read must work")
            .is_some(),
        "a platform-level identity is readable by every organization — the request says so"
    );
    assert!(
        shared.is_platform_level(),
        "and the row knows it, so a route can refuse to edit it"
    );

    store.dispose().await;
}

#[tokio::test]
async fn removing_an_identity_removes_its_grants_and_leaves_the_tool_row() {
    // The tool's row is the operator's copy of compiled code and outlives any identity; the
    // grants are that identity's decisions and go with it (the FK cascades).
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("doomed"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;
    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Deny, None)
        .await
        .expect("the deny must be accepted");

    assert!(
        identity::delete_identity(&store.pool, identity.id)
            .await
            .expect("the delete must work"),
        "the identity must be gone"
    );
    let grants: i64 =
        sqlx::query_scalar("select count(*) from ai_tool_grants where identity_id = $1")
            .bind(identity.id)
            .fetch_one(&store.pool)
            .await
            .expect("the count must read");
    assert_eq!(grants, 0, "its grants go with it");
    let tools: i64 = sqlx::query_scalar("select count(*) from ai_tools")
        .fetch_one(&store.pool)
        .await
        .expect("the count must read");
    assert!(tools > 0, "but the registry itself is untouched");

    store.dispose().await;
}

#[tokio::test]
async fn a_retired_tool_keeps_its_row_so_its_grants_survive() {
    // "Removing a tool from the compiled set leaves its row with a retired note and never
    // silently deletes grants." The FK is on `tool_key` precisely so a grant keeps pointing at a
    // real, possibly-retired, tool — this walk proves a grant written before the retirement is
    // still there after it.
    let store = identities!();
    let identity = identity::create_identity(&store.pool, &store.draft("retired"))
        .await
        .expect("the identity must be created");
    let tool = any_tool(&store).await;
    identity::set_grant(&store.pool, identity.id, &tool, GrantEffect::Deny, None)
        .await
        .expect("the deny must be accepted");

    // Retire it the way the seeder does, then re-run the seeder so the code half comes back —
    // the point is the row and the grant, not the retirement itself.
    sqlx::query(
        "update ai_tools set enabled = false, retired_note = 'left the catalogue' where key = $1",
    )
    .bind(&tool)
    .execute(&store.pool)
    .await
    .expect("the retirement must apply");
    assert_eq!(
        store.grant_rows(identity.id, &tool).await,
        vec![false],
        "retiring a tool must not touch the grants made about it"
    );

    store.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The agent side of the matrix
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_agents_own_list_is_where_the_matrix_reads_its_agent_cells() {
    // The matrix's agent columns come from `ai_agents.tools`, and the resolver reads the same
    // list. A matrix that showed grants where the agent list disagrees would be showing an
    // operator a permission that does not exist at run time.
    let store = identities!();
    let agent = create_agent(
        &store.pool,
        &NewAgent::with_defaults(store.organization_id, "writer", "Writer"),
    )
    .await
    .expect("the fixture agent must be created");
    let tool = any_tool(&store).await;

    sqlx::query("update ai_agents set tools = $2::jsonb where id = $1")
        .bind(agent.id)
        .bind(serde_json::json!([tool.clone()]))
        .execute(&store.pool)
        .await
        .expect("the allow-list must be written");

    let reread = omnion_ai_hub::run_store::get_agent(
        &store.pool,
        store.organization_id,
        agent.id,
    )
    .await
    .expect("the read must work")
    .expect("the agent must be there");
    assert_eq!(
        reread.tools,
        vec![tool.clone()],
        "the matrix cell and the runtime's allow-list must be the same list"
    );
    assert_eq!(
        identity::resolve(
            &std::collections::BTreeMap::new(),
            &reread.tools,
            &tool,
            true
        )
        .effect,
        GrantEffect::Allow,
        "and an inherited tool the agent lists is allowed"
    );
    assert!(
        registry::agents_using(&store.pool, &tool)
            .await
            .expect("the agent lookup must work")
            .iter()
            .any(|found| found.id == reread.id),
        "the registry's disable confirmation must name the agent that carries the tool"
    );

    store.dispose().await;
}
