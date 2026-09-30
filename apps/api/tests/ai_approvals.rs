//! Walks for the approval gate (REQ-101, slice 1).
//!
//! The unit tests in `crates/ai-hub/src/approvals.rs` prove the *rules* — the six classes, the
//! class mapping, the fail-closed default, the policy precedence, `is_decidable`. Every one of
//! them is a pure function, which is the only way they could be written at all.
//!
//! These walks prove the four things a pure function cannot:
//!
//! 1. **The gate really gates.** The first acceptance criterion is "all six dangerous classes
//!    are gated by default on a fresh installation", and "on a fresh installation" is a claim
//!    about the **migration's seed**, not about a Rust default. A unit test on
//!    `ClassPolicy::default()` would pass with the seed deleted — the default would still say
//!    `require` while every real deployment had no row at all and inherited nothing. This suite
//!    reads the seeded table.
//!
//! 2. **A decision is single-use, and the loser writes nothing.** `audit_log` is append-only and
//!    has no delete, so a second decision that slipped through would leave a permanent record
//!    that two people approved the same deletion. The walk approves twice and asserts the row's
//!    `decided_by`/`decided_at` are still the *first* decider's, and that the trail holds
//!    exactly one approval row.
//!
//! 3. **The typed confirmation cannot be bypassed through the store.** "A checkbox is not a
//!    confirmation" is a claim about the API path, and the API calls the store — so the store is
//!    where it has to hold, for a caller that never went through the screen.
//!
//! 4. **The sweeper hands the parked run back.** A run parked forever leaves its step `running`,
//!    and `resume_point` then reports the tool as ambiguous for the rest of the installation's
//!    life. The walk creates a real run, expires the approval, and reads the run's status back.
//!
//! Everything runs against a **fresh database** created and dropped by the harness, so a walk
//! that leaves a row behind cannot make the next one pass.

use omnion_ai_hub::approvals::io::{self, NewApproval, PolicyChange, Requested};
use omnion_ai_hub::approvals::{
    ClassPolicy, DecisionOutcome, DANGEROUS_CLASSES, Gate, PolicyRow, is_dangerous_class,
};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct GateStore {
    pool: PgPool,
    organization_id: Uuid,
    other_organization_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl GateStore {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("PostgreSQL is not reachable at {}: {err}", config.database.url);
            return None;
        }

        let database = format!("omnion_aigate_{}", Uuid::new_v4().simple());
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
            // 4, not the default: this box runs ten writer loops against one
            // `max_connections = 100` server, and every walk opens its own database.
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let organization_id = seed_organization(db.pool(), "gateco").await;
        let other_organization_id = seed_organization(db.pool(), "othergate").await;

        Some(Self {
            pool: db.pool().clone(),
            organization_id,
            other_organization_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    /// A run row in `awaiting_approval`, written the way the runner parks one.
    async fn parked_run(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into ai_runs (id, organization_id, trigger, goal, status) \
             values ($1, $2, 'agent', 'publish the pricing page', 'awaiting_approval')",
        )
        .bind(id)
        .bind(self.organization_id)
        .execute(&self.pool)
        .await
        .expect("the fixture run must be created");
        id
    }

    async fn step(&self, run_id: Uuid, step_no: i32) -> Uuid {
        let id = Uuid::new_v4();
        // `tool` is NOT optional for a `tool_call` step: migration 0064 checks it, and the
        // constraint exists because a tool step with no tool is a step nobody can bill. The
        // fixture names `content.publish` so the parked step and the approval it carries agree.
        sqlx::query(
            "insert into ai_run_steps (id, run_id, step_no, kind, tool, status) \
             values ($1, $2, $3, 'tool_call', 'content.publish', 'running')",
        )
        .bind(id)
        .bind(run_id)
        .bind(step_no)
        .execute(&self.pool)
        .await
        .expect("the fixture step must be created");
        id
    }

    /// A request for a gated publish, the shape the loop's park path hands over.
    fn publish_request(&self, run_id: Option<Uuid>, step_id: Option<Uuid>) -> NewApproval {
        let now = Self::now();
        NewApproval {
            organization_id: self.organization_id,
            site_id: None,
            run_id,
            step_id,
            agent_id: None,
            identity_id: None,
            tool_key: "content.publish".to_owned(),
            tool_class: "content_publish".to_owned(),
            resource_type: Some("page".to_owned()),
            resource_id: Some("7f1c".to_owned()),
            resource_label: Some("Autumn pricing".to_owned()),
            title: "Publish Autumn pricing".to_owned(),
            summary: "The agent drafted a pricing page and asked to publish it.".to_owned(),
            operation_count: 1,
            preview: json!({
                "operations": [{
                    "action": "update",
                    "resource": "page:7f1c",
                    "fields": [{ "field": "status", "old": "draft", "new": "published" }]
                }]
            }),
            preview_hash: "hash-of-the-frozen-preview".to_owned(),
            base_revision: None,
            requested_by: None,
            model_id: None,
            risk: "medium".to_owned(),
            policy: ClassPolicy::default(),
            requested_at: now,
        }
    }

    /// A request in an irreversible class, so the phrase question is real.
    fn delete_request(&self) -> NewApproval {
        let mut new = self.publish_request(None, None);
        new.tool_key = "content.rollback".to_owned();
        new.tool_class = "content_delete".to_owned();
        new.title = "Delete the old pricing page".to_owned();
        new.resource_label = Some("Autumn pricing".to_owned());
        new.risk = "high".to_owned();
        new
    }

    async fn user(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into users (id, email, password_hash, display_name) \
             values ($1, $2, 'not-a-real-hash', 'Reviewer')",
        )
        .bind(id)
        .bind(format!("reviewer-{}@example.test", Uuid::new_v4().simple()))
        .execute(&self.pool)
        .await
        .expect("the fixture user must be created");
        id
    }

    async fn run_status(&self, run_id: Uuid) -> String {
        let status: String =
            sqlx::query_scalar("select status from ai_runs where id = $1")
                .bind(run_id)
                .fetch_one(&self.pool)
                .await
                .expect("the run row must be readable");
        status
    }

    async fn audit_count(&self, approval_id: Uuid, action: &str) -> i64 {
        let count: i64 = sqlx::query_scalar(
            "select count(*) from audit_log where target_type = 'ai_approval' \
             and target_id = $1 and action = $2",
        )
        .bind(approval_id.to_string())
        .bind(action)
        .fetch_one(&self.pool)
        .await
        .expect("the audit rows must be readable");
        count
    }

    async fn decided_by(&self, approval_id: Uuid) -> Option<Uuid> {
        let value: Option<Uuid> =
            sqlx::query_scalar("select decided_by from ai_approvals where id = $1")
                .bind(approval_id)
                .fetch_one(&self.pool)
                .await
                .expect("the approval row must be readable");
        value
    }

    async fn pending_total(&self) -> i64 {
        let count: i64 = sqlx::query_scalar("select count(*) from ai_approvals where status = 'pending'")
            .fetch_one(&self.pool)
            .await
            .expect("the approval count must be readable");
        count
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

/// The harness, or a **panic**.
///
/// The suites in this directory that print "skipping" and return are right for a developer who
/// has not started a database and wrong for a loop that reports "N passed": a URL naming a
/// database that does not exist makes every walk skip and the summary still reads green. That
/// happened once in `ai_tool_execution.rs`. A skipped walk proved nothing.
macro_rules! gate {
    () => {
        match GateStore::fresh().await {
            Some(store) => store,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED. \
                 Set OMNION_DATABASE_URL to an existing database — on this box the QA stack's is \
                 postgres://omnion:***@127.0.0.1:5433/omnion_qa_w7. A skip must not read as a pass."
            ),
        }
    };
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_installation_gates_all_six_classes() {
    let store = gate!();
    // Read straight from the table, not through `ClassPolicy::default()`: the criterion says
    // "on a fresh installation", which is a statement about the migration's seed.
    let rows: Vec<(String, String, bool, i32)> = sqlx::query_as(
        "select tool_class, mode, typed_confirmation, expires_minutes \
         from ai_approval_policies where organization_id is null order by tool_class",
    )
    .fetch_all(&store.pool)
    .await
    .expect("the seeded policies must be readable");

    assert_eq!(
        rows.len(),
        6,
        "the seed must carry exactly the request's six classes, not a subset"
    );
    for (class, mode, typed, minutes) in &rows {
        assert_eq!(mode, "require", "{class} must gate on a fresh installation");
        assert!(typed, "{class} must demand a typed confirmation by default");
        assert_eq!(*minutes, 60, "{class} must use the request's default expiry");
        assert!(is_dangerous_class(class), "{class} is not one of the six");
    }
    for class in DANGEROUS_CLASSES {
        assert!(
            rows.iter().any(|(stored, ..)| stored == class),
            "{class} is missing from the seed"
        );
    }
    store.dispose().await;
}

#[tokio::test]
async fn the_gate_parks_a_publish_even_when_the_caller_holds_every_permission() {
    let store = gate!();
    // `true` is "this caller holds the underlying domain permission". The criterion is
    // explicit that approval is a separate axis, and this is the walk that says so through the
    // same function the loop calls.
    let verdict = io::gate(&store.pool, store.organization_id, "content.publish", true)
        .await
        .expect("the gate must answer");
    assert!(verdict.parks(), "a publish must park whatever the caller may do");
    let policy = verdict.policy().expect("a park carries its policy");
    assert_eq!(policy.expires_minutes, 60);

    // A read does not park, and the two halves of that are different claims: gating a lookup
    // would train a reviewer to approve without reading.
    let read = io::gate(&store.pool, store.organization_id, "content.search", true)
        .await
        .expect("the gate must answer");
    assert!(!read.parks());
    store.dispose().await;
}

#[tokio::test]
async fn an_allow_policy_ungates_exactly_that_class_and_nothing_else() {
    let store = gate!();
    let user = store.user().await;
    io::set_policy(
        &store.pool,
        store.organization_id,
        &PolicyChange {
            tool_class: "deployment".to_owned(),
            mode: "allow".to_owned(),
            typed_confirmation: None,
            expires_minutes: None,
        },
        user,
        GateStore::now(),
    )
    .await
    .expect("the policy write must succeed");

    let deploy = io::gate(&store.pool, store.organization_id, "deployment.deploy", false)
        .await
        .expect("the gate must answer");
    assert!(!deploy.parks(), "the class was set to allow");
    // The other five are untouched — a policy write is per class, and a walk that only checked
    // the class it changed would pass for a write that cleared the whole table.
    let publish = io::gate(&store.pool, store.organization_id, "content.publish", false)
        .await
        .expect("the gate must answer");
    assert!(publish.parks(), "the publish class was not touched");
    store.dispose().await;
}

#[tokio::test]
async fn an_organization_policy_does_not_change_another_tenants_gate() {
    let store = gate!();
    let user = store.user().await;
    io::set_policy(
        &store.pool,
        store.organization_id,
        &PolicyChange {
            tool_class: "content_delete".to_owned(),
            mode: "allow".to_owned(),
            typed_confirmation: None,
            expires_minutes: Some(15),
        },
        user,
        GateStore::now(),
    )
    .await
    .expect("the policy write must succeed");

    let mine = io::gate(&store.pool, store.organization_id, "content.rollback", false)
        .await
        .expect("the gate must answer");
    assert!(!mine.parks());
    let theirs = io::gate(
        &store.pool,
        store.other_organization_id,
        "content.rollback",
        false,
    )
    .await
    .expect("the gate must answer");
    assert!(
        theirs.parks(),
        "another tenant's gate must not move because this one changed its policy"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_policy_beyond_the_bounds_is_refused_with_the_rule_named() {
    let store = gate!();
    let user = store.user().await;
    let error = io::set_policy(
        &store.pool,
        store.organization_id,
        &PolicyChange {
            tool_class: "content_publish".to_owned(),
            mode: "require".to_owned(),
            typed_confirmation: None,
            expires_minutes: Some(0),
        },
        user,
        GateStore::now(),
    )
    .await
    .expect_err("0 would expire every approval on arrival");
    let message = error.to_string();
    assert!(message.contains("5"), "{message}");
    assert!(message.contains("1440"), "{message}");

    let unknown = io::set_policy(
        &store.pool,
        store.organization_id,
        &PolicyChange {
            tool_class: "quantum_deploy".to_owned(),
            mode: "allow".to_owned(),
            typed_confirmation: None,
            expires_minutes: None,
        },
        user,
        GateStore::now(),
    )
    .await
    .expect_err("a class this build does not know cannot be gated");
    assert!(unknown.to_string().contains("quantum_deploy"), "{unknown}");
    store.dispose().await;
}

#[tokio::test]
async fn a_policy_change_is_audited_and_a_reset_removes_the_override() {
    let store = gate!();
    let user = store.user().await;
    io::set_policy(
        &store.pool,
        store.organization_id,
        &PolicyChange {
            tool_class: "plugin_install".to_owned(),
            mode: "allow".to_owned(),
            typed_confirmation: None,
            expires_minutes: Some(30),
        },
        user,
        GateStore::now(),
    )
    .await
    .expect("the policy write must succeed");

    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'ai.policy.changed' \
         and target_id = 'plugin_install' and metadata ->> 'mode' = 'allow'",
    )
    .fetch_one(&store.pool)
    .await
    .expect("the audit rows must be readable");
    assert_eq!(audited, 1, "a policy change rides audit_log with no webhook event");

    let view = io::policies(&store.pool, store.organization_id)
        .await
        .expect("the policy table must be readable");
    let plugin = view
        .iter()
        .find(|entry| entry.tool_class == "plugin_install")
        .expect("the class must be listed");
    assert!(plugin.permissive, "the screen stripes an allow row");
    assert_eq!(plugin.source, "organization");

    assert!(
        io::reset_policy(&store.pool, store.organization_id, "plugin_install")
            .await
            .expect("the reset must succeed"),
        "the override existed"
    );
    let after = io::policies(&store.pool, store.organization_id)
        .await
        .expect("the policy table must be readable");
    let plugin = after
        .iter()
        .find(|entry| entry.tool_class == "plugin_install")
        .expect("the class must still be listed");
    assert_eq!(plugin.source, "platform", "a reset inherits the default again");
    assert!(!plugin.permissive);
    store.dispose().await;
}

#[tokio::test]
async fn a_request_lands_pending_and_writes_an_agent_audit_row() {
    let store = gate!();
    let user = store.user().await;
    let run_id = store.parked_run().await;
    let step_id = store.step(run_id, 1).await;
    let mut request = store.publish_request(Some(run_id), Some(step_id));
    request.requested_by = Some(user);
    request.model_id = None;

    let created = io::request(&store.pool, &request)
        .await
        .expect("the request must be stored");
    let approval = match &created {
        Requested::Created(approval) => approval,
        Requested::AlreadyPending(_) => panic!("the first request must create a row"),
    };
    assert_eq!(approval.status, "pending");
    assert_eq!(approval.tool_class, "content_publish");
    assert_eq!(approval.requires_confirmation, true);
    assert_eq!(approval.confirmation_phrase.as_deref(), Some("Autumn pricing"));
    // The run stays parked: writing a request is not a decision, and a run that started
    // executing the moment somebody asked would make the whole gate advisory.
    assert_eq!(store.run_status(run_id).await, "awaiting_approval");

    io::audit_requested(&store.pool, approval)
        .await
        .expect("the request audit row must be written");
    let trail = io::audit_trail(&store.pool, store.organization_id, approval.id)
        .await
        .expect("the trail must be readable");
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].actor_type, "agent", "the asker is an agent");
    assert_eq!(trail[0].actor_user_id, Some(user), "the requester is the human");
    assert_eq!(
        trail[0].metadata["preview_hash"],
        json!("hash-of-the-frozen-preview"),
        "the criterion names the hash explicitly"
    );
    assert_eq!(trail[0].metadata["requester"], json!(user.to_string()));
    store.dispose().await;
}

#[tokio::test]
async fn a_second_request_for_the_same_step_returns_the_first_row() {
    let store = gate!();
    let run_id = store.parked_run().await;
    let step_id = store.step(run_id, 1).await;
    let first = io::request(&store.pool, &store.publish_request(Some(run_id), Some(step_id)))
        .await
        .expect("the first request must be stored");
    let second = io::request(&store.pool, &store.publish_request(Some(run_id), Some(step_id)))
        .await
        .expect("the second request must answer, not fail");

    // "Notification volume is bounded: requesting the same tool in a loop produces one pending
    // approval and an `already_pending` refusal, not a flood." The refusal is a *return value*
    // carrying the row, because a code with no id leaves the caller nothing to link to.
    assert!(matches!(first, Requested::Created(_)));
    assert!(matches!(second, Requested::AlreadyPending(_)));
    assert_eq!(
        first.approval().id,
        second.approval().id,
        "the second request must point at the row the reviewer will find"
    );
    assert_eq!(store.pending_total().await, 1, "one step, one pending row");
    store.dispose().await;
}

#[tokio::test]
async fn an_approval_cannot_be_decided_twice_and_the_second_writes_nothing() {
    let store = gate!();
    let first_decider = store.user().await;
    let second_decider = store.user().await;
    let created = io::request(&store.pool, &store.delete_request())
        .await
        .expect("the request must be stored");
    let id = created.approval().id;

    let approved = io::approve(
        &store.pool,
        store.organization_id,
        id,
        first_decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the first decision must be stored");
    assert!(approved.changed(), "{approved:?}");
    let decided_at = approved.approval().expect("a decision carries the row").decided_at;

    let again = io::approve(
        &store.pool,
        store.organization_id,
        id,
        second_decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the second decision must answer, not fail");
    assert_eq!(again.code(), Some("already_decided"));
    assert!(!again.changed());

    // The row still names the FIRST decider, and the second wrote nothing at all — which is
    // the half that matters, because `audit_log` is append-only and has no delete to fix it
    // with afterwards.
    assert_eq!(store.decided_by(id).await, Some(first_decider));
    let current = io::read(&store.pool, store.organization_id, id)
        .await
        .expect("the row must be readable")
        .expect("the row must exist");
    assert_eq!(current.decided_at, decided_at);
    assert_eq!(
        store.audit_count(id, "ai.approval.approved").await,
        1,
        "a second decision would leave a permanent record that two people approved one deletion"
    );
    store.dispose().await;
}

#[tokio::test]
async fn a_rejection_needs_a_reason_and_records_it() {
    let store = gate!();
    let decider = store.user().await;
    let created = io::request(&store.pool, &store.publish_request(None, None))
        .await
        .expect("the request must be stored");
    let id = created.approval().id;

    let blank = io::reject(
        &store.pool,
        store.organization_id,
        id,
        decider,
        "   ",
        GateStore::now(),
    )
    .await
    .expect_err("a rejection with no reason is not a decision");
    assert!(blank.to_string().contains("reason"), "{blank}");
    assert_eq!(
        store.audit_count(id, "ai.approval.rejected").await,
        0,
        "the refused rejection must not have written anything"
    );

    let rejected = io::reject(
        &store.pool,
        store.organization_id,
        id,
        decider,
        "the agent invented a price nobody approved",
        GateStore::now(),
    )
    .await
    .expect("the rejection must be stored");
    assert!(rejected.changed());
    assert_eq!(
        rejected.approval().expect("a decision carries the row").decision_note.as_deref(),
        Some("the agent invented a price nobody approved")
    );
    assert_eq!(store.audit_count(id, "ai.approval.rejected").await, 1);
    store.dispose().await;
}

#[tokio::test]
async fn a_typed_confirmation_cannot_be_skipped_or_guessed() {
    let store = gate!();
    let decider = store.user().await;
    let created = io::request(&store.pool, &store.delete_request())
        .await
        .expect("the request must be stored");
    let approval = created.approval();
    let id = approval.id;
    assert!(
        approval.requires_confirmation,
        "an irreversible class demands a phrase before the fixture can test it"
    );

    // No phrase at all.
    let missing = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        None,
        None,
        GateStore::now(),
    )
    .await
    .expect("the refusal must be an answer, not an error");
    assert_eq!(missing.code(), Some("confirmation_required"));

    // A checkbox's worth of "yes".
    let wrong = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("yes"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the refusal must be an answer, not an error");
    assert_eq!(wrong.code(), Some("confirmation_mismatch"));

    // The row is still pending: neither refusal may have moved it.
    let still = io::read(&store.pool, store.organization_id, id)
        .await
        .expect("the row must be readable")
        .expect("the row must exist");
    assert_eq!(still.status, "pending");

    let right = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the phrase must be accepted");
    assert!(right.changed());
    store.dispose().await;
}

#[tokio::test]
async fn an_expired_request_cannot_be_decided_and_the_sweeper_hands_the_run_back() {
    let store = gate!();
    let decider = store.user().await;
    let run_id = store.parked_run().await;
    let step_id = store.step(run_id, 1).await;
    let mut request = store.publish_request(Some(run_id), Some(step_id));
    // A five-minute policy, and a clock that has already passed it. The seam is the reason a
    // walk can see an expiry at all rather than waiting an hour for one.
    request.policy.expires_minutes = 5;
    let created = io::request(&store.pool, &request)
        .await
        .expect("the request must be stored");
    let id = created.approval().id;

    let after_expiry = GateStore::now() + time::Duration::minutes(6);
    let expired = io::expire_due(&store.pool, after_expiry, 50)
        .await
        .expect("the sweeper must run");
    assert_eq!(expired, vec![id], "the sweeper must find exactly this one");

    // The run is back on the queue rather than parked for ever: a step left `running` is what
    // `resume_point` reads as "a tool may already have fired", and that question then stays
    // open for the life of the installation.
    assert_eq!(
        store.run_status(run_id).await,
        "queued",
        "an expired approval must hand the run back so it ends cleanly"
    );

    let late = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the late decision must be an answer, not an error");
    assert_eq!(late.code(), Some("expired"));
    assert!(
        !late.changed(),
        "an expired request must not be decidable, and no audit row may say it was"
    );
    assert_eq!(store.audit_count(id, "ai.approval.approved").await, 0);
    assert_eq!(store.audit_count(id, "ai.approval.expired").await, 1);
    store.dispose().await;
}

#[tokio::test]
async fn a_request_the_resource_outgrew_is_stale_and_names_the_current_revision() {
    let store = gate!();
    let decider = store.user().await;
    let mut request = store.publish_request(None, None);
    request.base_revision = Some("revision-3".to_owned());
    let created = io::request(&store.pool, &request)
        .await
        .expect("the request must be stored");
    let id = created.approval().id;

    let stale = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        Some("revision-4"),
        GateStore::now(),
    )
    .await
    .expect("the refusal must be an answer, not an error");
    assert_eq!(stale.code(), Some("stale"));
    match &stale {
        DecisionOutcome::Stale { current_revision } => {
            assert_eq!(current_revision, "revision-4", "Re-preview needs a real target");
        }
        other => panic!("expected a stale answer, got {other:?}"),
    }
    // The expiry check runs first, so a stale request is not also asked for a phrase: a
    // reviewer whose only mistake was somebody else editing a page should not be told to type
    // a name.
    let row = io::read(&store.pool, store.organization_id, id)
        .await
        .expect("the row must be readable")
        .expect("the row must exist");
    assert_eq!(row.status, "pending", "a stale refusal moves nothing");

    let fresh = io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        Some("revision-3"),
        GateStore::now(),
    )
    .await
    .expect("the matching revision must be accepted");
    assert!(fresh.changed(), "the preview is still accurate");
    store.dispose().await;
}

#[tokio::test]
async fn another_organizations_approval_is_not_found_and_cannot_be_decided() {
    let store = gate!();
    let decider = store.user().await;
    let created = io::request(&store.pool, &store.delete_request())
        .await
        .expect("the request must be stored");
    let id = created.approval().id;

    // `None`, not a 403: a 403 on a sequential uuid is an existence oracle for every approval
    // in the installation, including the ones that name a page somebody is about to delete.
    assert!(
        io::read(&store.pool, store.other_organization_id, id)
            .await
            .expect("the read must run")
            .is_none()
    );
    let error = io::approve(
        &store.pool,
        store.other_organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect_err("a stranger may not decide");
    assert!(error.to_string().contains("approval"), "{error}");

    // And the row is untouched, which is the half that says the refusal came before the write.
    let row = io::read(&store.pool, store.organization_id, id)
        .await
        .expect("the row must be readable")
        .expect("the row must exist");
    assert_eq!(row.status, "pending");
    store.dispose().await;
}

#[tokio::test]
async fn the_inbox_filters_and_its_counts_come_from_the_same_table() {
    let store = gate!();
    let decider = store.user().await;
    let publish = store
        .publish_request(None, None)
        .await_request(&store)
        .await;
    let delete = store.delete_request().await_request(&store).await;

    let inbox = io::list(
        &store.pool,
        store.organization_id,
        &io::ApprovalFilter::default(),
    )
    .await
    .expect("the inbox must be readable");
    assert_eq!(inbox.approvals.len(), 2);
    assert_eq!(inbox.counts.get("pending"), Some(&2));

    let only_publish = io::list(
        &store.pool,
        store.organization_id,
        &io::ApprovalFilter {
            tool_class: Some("content_publish".to_owned()),
            ..io::ApprovalFilter::default()
        },
    )
    .await
    .expect("the inbox must be readable");
    assert_eq!(only_publish.approvals.len(), 1);
    assert_eq!(only_publish.approvals[0].id, publish.id());
    // The counts describe the whole table, not the filtered view — a tab strip whose numbers
    // change with the filter is a tab strip nobody can read.
    assert_eq!(only_publish.counts.get("pending"), Some(&2));

    let searched = io::list(
        &store.pool,
        store.organization_id,
        &io::ApprovalFilter {
            q: Some("Delete".to_owned()),
            ..io::ApprovalFilter::default()
        },
    )
    .await
    .expect("the inbox must be readable");
    assert_eq!(searched.approvals.len(), 1);
    assert_eq!(searched.approvals[0].id, delete.id());

    // A stranger's inbox is empty even though the table has two rows in it.
    let theirs = io::list(
        &store.pool,
        store.other_organization_id,
        &io::ApprovalFilter::default(),
    )
    .await
    .expect("the inbox must be readable");
    assert!(theirs.approvals.is_empty());
    assert_eq!(
        theirs.counts.get("pending"),
        Some(&0),
        "the badge must not leak another tenant's volume"
    );

    io::reject(
        &store.pool,
        store.organization_id,
        publish.id(),
        decider,
        "not this week",
        GateStore::now(),
    )
    .await
    .expect("the rejection must be stored");
    let after = io::list(
        &store.pool,
        store.organization_id,
        &io::ApprovalFilter::default(),
    )
    .await
    .expect("the inbox must be readable");
    assert_eq!(after.counts.get("pending"), Some(&1));
    assert_eq!(after.counts.get("rejected"), Some(&1));
    assert_eq!(
        io::pending_count(&store.pool, store.organization_id).await.expect("the count"),
        1
    );
    store.dispose().await;
}

#[tokio::test]
async fn the_database_refuses_a_decided_row_that_claims_to_be_pending() {
    let store = gate!();
    let decider = store.user().await;
    let created = io::request(&store.pool, &store.publish_request(None, None))
        .await
        .expect("the request must be stored");
    let id = created.approval().id;
    io::approve(
        &store.pool,
        store.organization_id,
        id,
        decider,
        Some("Autumn pricing"),
        None,
        GateStore::now(),
    )
    .await
    .expect("the decision must be stored");

    // The check is the invariant, not a convention: a row that is decided but carries no
    // timestamp (or the reverse) would make "waiting for you" show a decided request.
    //
    // The violation is `status = 'approved'` with `decided_at = null` — NOT "back to pending
    // with decided_at = nulled", which is a perfectly legal row and which an earlier version
    // of this walk asserted the database would refuse. That version passed for the wrong
    // reason on the runs where it failed and would have failed forever if the constraint were
    // dropped: the check is `(status = 'pending') = (decided_at is null)`, and both halves
    // being true is a satisfied constraint. A control that asserts the wrong violation is
    // worse than no control — it looks like coverage of the decision invariant while
    // covering nothing.
    let decided_without_a_timestamp = sqlx::query(
        "update ai_approvals set status = 'approved', decided_at = null where id = $1",
    )
    .bind(id)
    .execute(&store.pool)
    .await;
    assert!(
        decided_without_a_timestamp.is_err(),
        "the pending_iff_undecided check must refuse an approved row with no decided_at"
    );

    // And the reverse half: still pending, but pretending somebody already decided it.
    let second = io::request(&store.pool, &store.publish_request(None, None))
        .await
        .expect("a second, independent request must be stored");
    let pending_with_a_timestamp = sqlx::query(
        "update ai_approvals set decided_at = now() where id = $1",
    )
    .bind(second.approval().id)
    .execute(&store.pool)
    .await;
    assert!(
        pending_with_a_timestamp.is_err(),
        "the pending_iff_undecided check must refuse a pending row that carries decided_at"
    );

    let rejection_without_a_reason = sqlx::query(
        "update ai_approvals set status = 'rejected', decision_note = '  ' where id = $1",
    )
    .bind(id)
    .execute(&store.pool)
    .await;
    assert!(
        rejection_without_a_reason.is_err(),
        "a rejection with a blank reason is not auditable"
    );
    store.dispose().await;
}

#[tokio::test]
async fn the_policy_rows_the_resolver_reads_are_the_ones_the_migration_wrote() {
    let store = gate!();
    // Read the rows and feed them to the *resolver*, so the walk covers the join between the
    // migration's shape and the function the screen renders from. A resolver test with
    // hand-built rows would pass with the migration seeding nothing.
    let rows: Vec<PolicyRow> = sqlx::query_as(
        "select id, organization_id, tool_class, mode, typed_confirmation, expires_minutes, \
         updated_by, updated_at from ai_approval_policies where organization_id is null",
    )
    .fetch_all(&store.pool)
    .await
    .expect("the seeded policies must be readable");

    let resolved = omnion_ai_hub::approvals::resolve_policies(&rows);
    assert_eq!(resolved.len(), 6);
    for class in DANGEROUS_CLASSES {
        let view = resolved
            .get(class)
            .unwrap_or_else(|| panic!("{class} must resolve"));
        assert_eq!(view.source, "platform");
        assert!(!view.permissive, "{class} must gate on a fresh installation");
        assert_eq!(
            view.irreversible,
            omnion_ai_hub::approvals::is_irreversible_class(class)
        );
    }
    assert!(
        resolved.get("deployment").expect("deployment").irreversible,
        "a deployment is irreversible"
    );
    assert!(
        !resolved.get("content_publish").expect("content_publish").irreversible,
        "a publish can be undone by un-publishing"
    );
    store.dispose().await;
}

/// A tiny helper so the walks above read as one sentence each.
///
/// The alternative — matching on [`Requested`] at every call site — buries the assertion that
/// matters (this is the *second* request and it must be refused) under a match arm.
trait AwaitRequest {
    fn await_request(self, store: &GateStore) -> impl std::future::Future<Output = Requested>;
}

impl AwaitRequest for NewApproval {
    async fn await_request(self, store: &GateStore) -> Requested {
        io::request(&store.pool, &self)
            .await
            .expect("the request must be stored")
    }
}

/// The id out of a request outcome, for the walks that only care which row it was.
trait ApprovalId {
    fn id(&self) -> Uuid;
}

impl ApprovalId for Requested {
    fn id(&self) -> Uuid {
        self.approval().id
    }
}
