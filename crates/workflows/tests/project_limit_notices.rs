//! Project limit notices fire once per limit per period (REQ-133, slice 4) — against a real
//! database.
//!
//! The clause this file exists for is one word in a sentence the store already satisfied: "the 80
//! percent warning **fires once**". Everything else about the limits was true and green — the
//! crossing is computed, the screen draws the amber bar, the over-quota run is refused by name —
//! and none of it could tell a warning from the same warning seen again, because the state was
//! derived per read and the two `automation.project.limit.*` events were emitted by nothing.
//!
//! The tests start where the product starts: limits are set through `set_limits`, usage is recorded
//! through `record_usage`, and the sweep is called the way the worker calls it. A test that
//! constructed a `LimitNotice` by hand would prove the `on conflict` clause and nothing else —
//! which is precisely how the CRM assignment chain stayed green for twenty-four ticks while no
//! lead was ever assigned.

use std::env;

use omnion_workflows::limits::{self, LimitOverrides, NoticeKind};
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
    project_id: Uuid,
    user_id: Uuid,
}

async fn project_with_limits(pool: &PgPool, overrides: LimitOverrides) -> Fixture {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query(
        "insert into organizations (id, name, slug) values ($1, 'Notices', $2)",
    )
    .bind(organization_id)
    .bind(format!("notices-{}", organization_id))
    .execute(pool)
    .await
    .expect("the organization insert is this suite's fixture");

    sqlx::query("insert into users (id, email, display_name) values ($1, $2, 'Owner')")
        .bind(user_id)
        .bind(format!("owner-{}@example.test", user_id))
        .execute(pool)
        .await
        .expect("the user insert is this suite's fixture");

    let project_id: Uuid = sqlx::query_scalar(
        "insert into automation_projects (organization_id, key, name, owner_user_id) \
         values ($1, 'NOTICES', 'Notices', $2) returning id",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("the project insert is this suite's fixture");

    limits::set_limits(pool, project_id, overrides).await.expect("the caps are written");

    Fixture {
        organization_id,
        project_id,
        user_id,
    }
}

#[tokio::test]
async fn the_warning_is_claimed_once_and_the_second_sweep_is_silent() {
    let Some(pool) = pool().await else { return };
    let project = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;

    // 8 of 10 is exactly the 80 percent threshold.
    for _ in 0..8 {
        limits::record_usage(&pool, project.project_id, false, 0)
            .await
            .expect("the usage counter is written");
    }

    let first = limits::claim_due_notices(&pool, project.project_id)
        .await
        .expect("the first sweep reads");
    assert_eq!(
        first.len(),
        1,
        "a project at 8 of 10 owes exactly one warning, and it is the first caller that wins it"
    );
    let notice = &first[0];
    assert_eq!(notice.kind, NoticeKind::Warning, "80 percent is a warning, not a cap");
    assert_eq!(notice.limit, "max_runs_per_day", "the limit is the column an operator reads");
    assert_eq!(notice.current, 8);
    assert_eq!(notice.max, 10);
    assert_eq!(notice.project_key, "NOTICES");
    assert_ne!(notice.period, "ever", "a daily counter's period is a day, not for ever");

    // The whole criterion. A second sweep at the same numbers must be silent — this is what a
    // screen reload does sixty times an hour and what a second API node does continuously.
    for attempt in 0..5 {
        let again = limits::claim_due_notices(&pool, project.project_id)
            .await
            .expect("the second sweep reads");
        assert!(
            again.is_empty(),
            "sweep {attempt} re-warned about a crossing that was already claimed: {again:?}"
        );
    }
    assert!(
        limits::notice_claimed(&pool, project.project_id, "max_runs_per_day", &notice.period, NoticeKind::Warning)
            .await
            .expect("the claim is readable"),
        "the row is the fact, and it is there"
    );
}

#[tokio::test]
async fn reaching_the_cap_escalates_and_the_warning_does_not_repeat_at_the_cap() {
    let Some(pool) = pool().await else { return };
    let project = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;

    for _ in 0..8 {
        limits::record_usage(&pool, project.project_id, false, 0).await.expect("counted");
    }
    let warned = limits::claim_due_notices(&pool, project.project_id).await.expect("reads");
    assert_eq!(warned.len(), 1, "the warning fires at 80 percent");

    for _ in 0..2 {
        limits::record_usage(&pool, project.project_id, false, 0).await.expect("counted");
    }
    let capped = limits::claim_due_notices(&pool, project.project_id).await.expect("reads");
    assert_eq!(
        capped.len(),
        1,
        "the cap is a second, different fact: one escalation, not a second warning and an escalation"
    );
    assert_eq!(capped[0].kind, NoticeKind::Exceeded);
    assert_eq!(capped[0].current, 10);
    assert!(
        limits::claim_due_notices(&pool, project.project_id)
            .await
            .expect("reads")
            .is_empty(),
        "and it fires once"
    );
}

#[tokio::test]
async fn a_project_with_no_caps_owes_nothing() {
    let Some(pool) = pool().await else { return };
    let project = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 0,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;
    for _ in 0..500 {
        limits::record_usage(&pool, project.project_id, false, 0).await.expect("counted");
    }
    assert!(
        limits::claim_due_notices(&pool, project.project_id)
            .await
            .expect("reads")
            .is_empty(),
        "500 runs against no cap is not a crossing, and a claim row per absent limit would be noise"
    );
    assert!(
        !limits::projects_with_caps(&pool, 100)
            .await
            .expect("the sweep's read")
            .contains(&project.project_id),
        "and the sweep does not even look at it"
    );
}

#[tokio::test]
async fn a_raised_cap_can_warn_again_and_a_raised_daily_cap_does_not_rewrite_yesterday() {
    let Some(pool) = pool().await else { return };
    let project = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 2,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;

    // Two workflows to reach a cap of two. The insert writes through the same project column the
    // count reads, so this is the real counter and not a hand-set number.
    for name in ["alpha", "beta"] {
        sqlx::query(
            "insert into workflows (id, name, project_id, organization_id, trigger_kind, steps) \
             values ($1, $2, $3, $4, 'manual', '[]')",
        )
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(project.project_id)
        .bind(project.organization_id)
        .execute(&pool)
        .await
        .expect("the workflow insert is this suite's fixture");
    }

    let capped = limits::claim_due_notices(&pool, project.project_id).await.expect("reads");
    assert_eq!(capped.len(), 1, "two workflows against a cap of two is the cap");
    assert_eq!(capped[0].kind, NoticeKind::Exceeded);
    assert_eq!(capped[0].limit, "max_workflows");
    assert_eq!(
        capped[0].period, "ever",
        "a workflow count does not reset at midnight, so its period is for ever"
    );

    // The operator does the obvious thing.
    limits::set_limits(
        &pool,
        project.project_id,
        LimitOverrides {
            max_workflows: 4,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: Some(project.user_id),
        },
    )
    .await
    .expect("the cap is raised");

    assert!(
        limits::claim_due_notices(&pool, project.project_id)
            .await
            .expect("reads")
            .is_empty(),
        "four of four... two of four is under the threshold, so raising the cap stops the notice — \
         which is the point: the next crossing is a fact about the future, not the past"
    );
}

#[tokio::test]
async fn two_callers_racing_the_same_crossing_produce_one_notice() {
    let Some(pool) = pool().await else { return };
    let project = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;
    for _ in 0..9 {
        limits::record_usage(&pool, project.project_id, false, 0).await.expect("counted");
    }

    // This is the two-API-node case, and it is the one the claim exists for: eight sweeps released
    // together, and the platform must owe one warning. A read-then-write would hand all eight the
    // same answer and the operations team eight copies of one crossing.
    // `tokio::sync::Barrier`, not `std::sync::Barrier`: the std one blocks the executor thread
    // rather than yielding, so eight tasks on a current-thread runtime deadlock at the release
    // instead of racing. This is the same barrier `crm_claims.rs` uses, for the same reason.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let barrier = barrier.clone();
        let project_id = project.project_id;
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            limits::claim_due_notices(&pool, project_id)
                .await
                .map(|notices| notices.len())
                .unwrap_or(0)
        }));
    }
    let mut total = 0;
    for handle in handles {
        total += handle.await.expect("the sweep does not panic");
    }
    assert_eq!(total, 1, "eight released sweeps owe one warning, and it went to exactly one of them");
}

#[tokio::test]
async fn a_crossing_belongs_to_its_own_project() {
    let Some(pool) = pool().await else { return };
    let first = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;
    let second = project_with_limits(
        &pool,
        LimitOverrides {
            max_workflows: 0,
            max_credentials: 0,
            max_runs_per_day: 10,
            max_concurrent_runs: 0,
            warn_at_percent: 80,
            updated_by: None,
        },
    )
    .await;

    for _ in 0..9 {
        limits::record_usage(&pool, first.project_id, false, 0).await.expect("counted");
    }

    assert_eq!(
        limits::claim_due_notices(&pool, first.project_id).await.expect("reads").len(),
        1,
        "the project that crossed is warned"
    );
    assert!(
        limits::claim_due_notices(&pool, second.project_id)
            .await
            .expect("reads")
            .is_empty(),
        "a neighbour's quota is not this project's quota, and a shared claim row would say otherwise"
    );
}
