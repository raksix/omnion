//! The SLA worker's *effects* — the escalation notification and the two events (REQ-117,
//! acceptance 11).
//!
//! Run through `scripts/qa/run-crm-sla.sh` (which runs this suite against a fresh database).
//!
//! ## Why this is a separate file from the module gate
//!
//! `modules/crm-intake/tests/crm_sla.rs` proves the claims: the breach sweep, the once-only
//! escalation, the reminder window and its race. It cannot prove the *notification* — doing so
//! would mean the business module taking a dependency on `omnion-notifications`, which is
//! exactly the "core stays thin" rule the architecture exists to keep. The write lives in
//! `apps/api` (the worker), which already depends on both crates, so that is where the
//! assertion belongs.
//!
//! ## What is asserted, and why the obvious test would pass without the code
//!
//! A test that calls `crm_sla_runner::tick` and asserts a row appeared is easy to write and easy
//! to satisfy wrongly: `notifications::record` returns `false` for a duplicate and the claim
//! returns `false` for a loser, so a test that ignores the return values passes whether the
//! worker notified once, twice, or not at all. So each of the three facts is checked twice —
//! once for the row and once for the **absence** of a second one — because "exactly once" is
//! two assertions, not one.
//!
//! The third fact is the one nobody asked for and everybody needs: **a recipient that is not a
//! real account is skipped, not crashed on.** `users.organization_id` is nullable, so a policy
//! can name a person who was deleted (the column is `on delete set null`, but a *policy* can
//! still carry a stale uuid), and a worker that inserted anyway would either fail the whole tick
//! on the foreign key or — worse, if the constraint were ever relaxed — tell somebody outside
//! the organization that they missed a lead.

use omnion_api::crm_sla_runner;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use uuid::Uuid;

/// A throwaway database with every migration applied and the IAM seed loaded.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("the environment must be valid");
        let database = format!("omnion_crm_sla_{}", Uuid::new_v4().simple());

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
        db.migrate().await.expect("the migrations must apply");
        omnion_permissions::seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let redis = RedisClient::new(&config.redis.url).expect("the redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    async fn dispose(self) {
        drop(self.state);
        drop(self.db);
        let _ = sqlx::query(&format!(
            "drop database if exists \"{}\" with (force)",
            self.database
        ))
        .execute(self.maintenance.pool())
        .await;
    }
}

/// Point a database URL at a different database name, keeping user, password and port.
fn swap_database(url: &str, database: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a postgres URL");
    let (authority, _path) = rest.split_once('/').unwrap_or((rest, ""));
    format!("{scheme}://{authority}/{database}")
}

async fn org_and_user(state: &AppState, label: &str) -> (Uuid, Uuid) {
    let pool = state.db().pool();
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {org}"))
        .bind(format!("{label}-{}", org.simple()))
        .execute(pool)
        .await
        .expect("an organization");
    let user = Uuid::new_v4();
    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, status) \
         values ($1, $2, $3, $4, 'x', 'active')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("{user}@example.com"))
    .bind("Ada Colleague")
    .execute(pool)
    .await
    .expect("an account to notify");
    (org, user)
}

async fn overdue_lead(state: &AppState, org: Uuid, owner: Uuid, escalate_to: Option<Uuid>) -> Uuid {
    let pool = state.db().pool();
    let sla = Uuid::new_v4();
    sqlx::query(
        "insert into crm_sla_policies \
         (id, organization_id, name, first_response_minutes, business_hours_only, \
          reminder_minutes, escalate_to_user_id, business_hours, active) \
         values ($1, $2, 'gate', 60, false, 15, $3, '{}'::jsonb, true)",
    )
    .bind(sla)
    .bind(org)
    .bind(escalate_to)
    .execute(pool)
    .await
    .expect("an sla policy");

    let lead = Uuid::new_v4();
    sqlx::query(
        "insert into crm_leads \
         (id, organization_id, status, email, owner_user_id, sla_policy_id, \
          first_response_due_at, received_at) \
         values ($1, $2, 'assigned', $3, $4, $5, now() - interval '30 minutes', now() - interval '2 hours')",
    )
    .bind(lead)
    .bind(org)
    .bind(format!("lead-{lead}@example.com"))
    .bind(owner)
    .bind(sla)
    .execute(pool)
    .await
    .expect("an overdue lead");
    lead
}

async fn notifications_for(pool: &sqlx::PgPool, user: Uuid, source_id: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "select title, dedupe_key from notifications \
         where user_id = $1 and source_id = $2 order by title",
    )
    .bind(user)
    .bind(source_id)
    .fetch_all(pool)
    .await
    .expect("the inbox")
}

/// The headline: a breached lead puts exactly one notification in the escalation target's
/// inbox, and a second tick grows nothing.
#[tokio::test]
async fn a_breach_notifies_the_escalation_target_exactly_once() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (org, target) = org_and_user(&harness.state, "sla-escalate").await;
    let lead = overdue_lead(&harness.state, org, target, Some(target)).await;
    let pool = harness.state.db().pool();

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick runs");
    assert!(report.escalated >= 1, "the overdue lead is escalated: {report:?}");
    assert!(!report.is_idle());

    let inbox = notifications_for(pool, target, &lead.to_string()).await;
    assert_eq!(
        inbox.len(),
        1,
        "one breach, one notification: {inbox:?}"
    );
    assert!(
        inbox[0].0.contains("first-response"),
        "and the title says what happened, so an operator does not have to open it: {:?}",
        inbox[0].0
    );
    assert_eq!(
        inbox[0].1,
        format!("crm.sla_breached:{lead}"),
        "the dedupe key is the claim's own content — a timestamp would make every retry new"
    );

    // The word "exactly" is the second half of the test. A worker that notified on every tick
    // would pass the assertion above on its first run and fail an installation by morning.
    let second = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the second tick runs");
    assert_eq!(second.escalated, 0, "the lead was already escalated");
    assert_eq!(
        notifications_for(pool, target, &lead.to_string()).await.len(),
        1,
        "and the inbox did not grow"
    );

    harness.dispose().await;
}

/// A reminder reaches the *owner*, and it is a different notification from the breach.
#[tokio::test]
async fn a_reminder_reaches_the_owner_and_is_not_the_breach() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (org, target) = org_and_user(&harness.state, "sla-remind").await;
    // 10 minutes from its deadline with a 15-minute reminder: inside the window, not past it.
    let lead = overdue_lead(&harness.state, org, target, Some(target)).await;
    sqlx::query(
        "update crm_leads set first_response_due_at = now() + interval '10 minutes' where id = $1",
    )
    .bind(lead)
    .execute(harness.state.db().pool())
    .await
    .expect("the deadline moves");
    let pool = harness.state.db().pool();

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick runs");
    assert_eq!(report.reminded, 1, "the owner is reminded: {report:?}");
    assert_eq!(report.escalated, 0, "and nothing is escalated yet");

    let inbox = notifications_for(pool, target, &lead.to_string()).await;
    assert_eq!(inbox.len(), 1, "one reminder: {inbox:?}");
    assert_eq!(
        inbox[0].1,
        format!("crm.sla_reminder:{lead}"),
        "a reminder and a breach are two facts and must not collapse into one row"
    );

    // Two more ticks: the reminder is claimed once and stays claimed.
    for _ in 0..2 {
        crm_sla_runner::tick(&harness.state, 10)
            .await
            .expect("the tick runs");
    }
    assert_eq!(
        notifications_for(pool, target, &lead.to_string()).await.len(),
        1,
        "the reminder fires once, whatever the clock does afterwards"
    );

    harness.dispose().await;
}

/// A breach with nobody to escalate to is still recorded — on the trail, with the reason.
#[tokio::test]
async fn a_breach_with_no_target_is_recorded_and_nobody_is_notified() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (org, target) = org_and_user(&harness.state, "sla-untargeted").await;
    // A lead nobody owns, under a policy with no escalation target: the configuration an
    // operator half-finishes and then forgets about.
    let lead = overdue_lead(&harness.state, org, target, None).await;
    sqlx::query("update crm_leads set owner_user_id = null where id = $1")
        .bind(lead)
        .execute(harness.state.db().pool())
        .await
        .expect("the lead loses its owner");
    let pool = harness.state.db().pool();

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick runs");
    assert_eq!(
        report.untargeted, 1,
        "an escalation with nowhere to go is counted as untargeted, NOT as escalated — the \
         two counters exist so `escalated=12` cannot hide the fact that nobody was told: {report:?}"
    );
    assert_eq!(
        report.escalated, 0,
        "claiming a breach is not the same fact as delivering it"
    );

    let reason: Option<String> = sqlx::query_scalar(
        "select detail->>'reason' from crm_lead_events where lead_id = $1 and kind = 'sla_breached'",
    )
    .bind(lead)
    .fetch_one(pool)
    .await
    .expect("the trail line");
    assert_eq!(
        reason.as_deref(),
        Some("no escalation target"),
        "without the reason, this lead reads identically to a working escalation"
    );
    assert!(
        notifications_for(pool, target, &lead.to_string()).await.is_empty(),
        "and nobody outside the organization is told about it"
    );

    harness.dispose().await;
}

/// An escalation target who has been **disabled** is still delivered to, and a *deleted* one
/// cannot be named at all — the foreign key decides that, and this test is the assertion.
///
/// The first draft of this test handed the policy a random uuid and expected the worker to skip
/// it. The database refused the *insert*, which is the better answer: `crm_sla_policies` carries
/// `escalate_to_user_id → users(id)`, so a policy can never name somebody who does not exist and
/// the `existing_users` guard in the worker is a second belt rather than the only one. What is
/// still reachable — and what the guard exists for — is the account being present but *disabled*
/// after the policy was saved, and the orgless platform account: `users.organization_id` is
/// nullable, so "a real account" and "an account in this organization" are different questions.
/// Asserting the reachable one is worth more than the unreachable one, because the FK means the
/// unreachable one cannot reach production.
#[tokio::test]
async fn a_disabled_target_is_still_a_real_account_and_is_told() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let (org, target) = org_and_user(&harness.state, "sla-disabled").await;
    let lead = overdue_lead(&harness.state, org, target, Some(target)).await;
    let pool = harness.state.db().pool();

    // The colleague is disabled after the policy was saved — the state an operator leaves
    // somebody in when they go on leave, without editing the SLA policy at all.
    sqlx::query("update users set status = 'disabled' where id = $1")
        .bind(target)
        .execute(pool)
        .await
        .expect("the account is disabled");

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick must not fail over a disabled colleague");
    assert_eq!(
        report.escalated, 1,
        "a disabled account is still a real account, and the escalation is recorded as delivered: \
         {report:?}"
    );
    assert_eq!(
        notifications_for(pool, target, &lead.to_string()).await.len(),
        1,
        "the escalation is delivered — refusing to tell a disabled person would silently drop \
         the escalation, and `users.status` is the access system, not the SLA's business"
    );

    harness.dispose().await;
}

/// An empty pass is idle — the difference between "nothing to do" and "it is broken".
#[tokio::test]
async fn a_tick_with_no_live_clocks_is_idle() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    org_and_user(&harness.state, "sla-empty").await;

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick runs on an empty table");
    assert_eq!(report.organizations, 0);
    assert!(report.is_idle(), "an empty pass is idle, not a failure: {report:?}");

    harness.dispose().await;
}

/// An account that exists but belongs to **another tenant** is not an addressee.
///
/// The guard this pins is `existing_users`, and the worker's own comment says what it is for:
/// *"a person on another tenant (or on the platform itself) can be selected as an escalation
/// target and the foreign key will happily accept it — which makes the naive 'notify whoever
/// the policy names' a tenant leak wearing a select element."* **That comment described a check
/// the function does not perform.** `existing_users` asks `select id from users where id = any($1)`
/// — a question about the *user table* and nothing else — so it answers "is this a real account"
/// and the caller reads that as "is this somebody we may tell". `users.organization_id` is
/// nullable, so the two answers differ for exactly the population the comment is about, and the
/// platform-side account (organization `null`) is the one the platform is *for*.
///
/// Both doors are exercised, because they are two separate call sites that each asked the
/// weaker question:
///
/// * the **escalation target** — a policy naming a colleague on tenant B (a reachable state:
///   `crm_sla_policies.escalate_to_user_id → users(id)` constrains existence, not tenancy, and
///   `POST /crm/assignment/policies` binds the id verbatim);
/// * the **lead's own owner** — the reminder path, which is a different function with a
///   different call site and its own copy of the reasoning.
///
/// The assertion is the absence of the row **and** that the breach is still *recorded*, because
/// "nobody was told" must not be reachable by breaking the escalation: the trail line is the
/// fact, the notification is a courtesy, and a guard that quietly stopped counting a breach
/// would trade a tenant leak for silent data loss.
#[tokio::test]
async fn an_escalation_target_on_another_tenant_is_never_told() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.state.db().pool();
    let (org, colleague) = org_and_user(&harness.state, "sla-home").await;
    // Tenant B exists only to own a person. Nothing of its own is ever swept.
    let (_other_org, stranger) = org_and_user(&harness.state, "sla-other").await;
    let lead = overdue_lead(&harness.state, org, colleague, Some(stranger)).await;

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick must not fail over an out-of-tenant target");
    assert_eq!(
        report.escalated, 0,
        "a breach delivered to somebody on another tenant is not an escalation: {report:?}"
    );
    assert_eq!(
        report.untargeted, 1,
        "it is the same shape as a policy with no target at all — the breach happened, there was \
         nowhere to send it, and the tick has to say which of the two it was: {report:?}"
    );

    // Read the whole table for this lead's source rather than one inbox: a leak that reached a
    // *third* row would satisfy "the stranger's inbox is empty" on a fixture that happens to
    // have only two people in it. The count is over `source_id`, which is the lead, so it is the
    // statement "this breach produced no notification anywhere" rather than "not in this one".
    let anywhere: i64 = sqlx::query_scalar(
        "select count(*) from notifications where source_id = $1",
    )
    .bind(lead.to_string())
    .fetch_one(pool)
    .await
    .expect("the notification table");
    assert_eq!(
        anywhere, 0,
        "a person on another tenant must never see this tenant's lead — and the assertion is \
         over every inbox, not the one the fixture happens to have built"
    );

    // The breach is still a fact. The trail line is what stops the lead reading "not breached",
    // and a tenancy guard that made the worker skip the row entirely would trade a tenant leak
    // for silent data loss — which is the wrong way round.
    let reason: Option<String> = sqlx::query_scalar(
        "select detail->>'reason' from crm_lead_events where lead_id = $1 and kind = 'sla_breached'",
    )
    .bind(lead)
    .fetch_one(pool)
    .await
    .expect("the breach is still recorded on the trail");
    assert_eq!(
        reason.as_deref(),
        Some("no escalation target"),
        "an out-of-tenant target and an absent one are the same state to an operator: nobody \
         who may see this lead was told"
    );

    harness.dispose().await;
}

/// The same rule on the **reminder** door, which is a different function and a different call
/// site with its own copy of the reasoning.
///
/// A test that only pins the escalation path leaves the reminder path asserting the same
/// promise from its own body, and this branch has shipped exactly that shape more than once —
/// two guards that read the same rule and disagree, a check that one site got and the other
/// did not. The lead's `owner_user_id` is bound verbatim by `PATCH /crm/leads/{id}/assign` and
/// constrained by nothing but the foreign key, so this is the same reachable state seen from
/// the other door.
#[tokio::test]
async fn a_lead_owned_by_somebody_on_another_tenant_is_never_reminded() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.state.db().pool();
    let (org, colleague) = org_and_user(&harness.state, "sla-owner-home").await;
    let (_other_org, stranger) = org_and_user(&harness.state, "sla-owner-other").await;
    // The lead belongs to the stranger; the policy's escalation target is the local colleague,
    // so a *fix* to the escalation path alone cannot make this test pass.
    let lead = overdue_lead(&harness.state, org, stranger, Some(colleague)).await;
    // Inside the reminder window: due in ten minutes with a fifteen-minute reminder.
    sqlx::query(
        "update crm_leads set first_response_due_at = now() + interval '10 minutes' where id = $1",
    )
    .bind(lead)
    .execute(pool)
    .await
    .expect("the deadline moves into the window");

    let report = crm_sla_runner::tick(&harness.state, 10)
        .await
        .expect("the tick must not fail over an out-of-tenant owner");
    assert_eq!(
        report.reminded, 0,
        "a reminder addressed to somebody on another tenant is a leak, not a reminder: {report:?}"
    );
    assert!(
        notifications_for(pool, stranger, &lead.to_string()).await.is_empty(),
        "the stranger's inbox is this tenant's business only if the tenant shares it"
    );

    // And the local colleague is not told about it either: the reminder is addressed to the
    // owner, and a fix that redirected it would be a different (and worse) feature.
    assert!(
        notifications_for(pool, colleague, &lead.to_string()).await.is_empty(),
        "the reminder is addressed to the owner; nobody else is a substitute"
    );

    harness.dispose().await;
}
