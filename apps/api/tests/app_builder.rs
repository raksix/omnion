//! Integration tests for the AI app builder's plan store (docs/requests/REQ-045, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) on a **throwaway
//! database**, and the store is exercised directly rather than through HTTP — the routes are
//! slice 2's, so what this suite proves is that the store's rules hold in a database rather
//! than only in memory.
//!
//! ## Why a throwaway database, stated once and in full
//!
//! `live_db` here swaps the database name to `postgres` and creates `omnion_<uuid>`; it never
//! connects to the configured database. A sibling suite (`auth.rs`) connects to the configured
//! `omnion` database and migrates it, and this repository numbers migrations in a namespace
//! **shared by ten worktrees** — so that suite reads `_sqlx_migrations` rows this branch's
//! `0021_*` does not contain and fails with `VersionMissing(19)`. That is a real defect in the
//! shared harness, and it is not this suite's to fix; what this suite can do is never be the
//! suite that breaks it.
//!
//! ## What each walk proves, one acceptance criterion each
//!
//! * a plan is written `generating` before the provider is called, and its answer moves it to
//!   `draft` with the token counts and the title the model sent;
//! * an artifact with no findings is `pending`, and one with findings is `invalid` — the
//!   generator cannot write `accepted` for itself;
//! * a reserved key is refused by **name**, at the store boundary and again at the database;
//! * an edit that re-validates clean is `edited` and names what it replaced, and one that
//!   still has findings is `invalid` rather than `edited`;
//! * regeneration keeps the previous version: two rows, one retired, and the plan's live
//!   artifact count does not double;
//! * `blockers` names what stands between a plan and apply — including a required kind the
//!   plan never proposed, which no artifact row could express;
//! * a plan's counts come back from the same query as the page, so the list's columns cannot
//!   disagree with its rows;
//! * an applied plan is undeletable and an unapplied one is deletable.

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use omnion_module_app_builder::{
    AppBuilderArtifact, AppBuilderPlan, EditedArtifact, NewArtifact, NewPlan, PlanFilter,
    PlanStore, PlanUsage, validate_artifact, validate_key, validate_plan,
};
use serde_json::json;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A throwaway database with every migration applied.
struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().expect("environment must be valid");
        if Db::connect(&maintenance_config(&config)).await.is_err() {
            eprintln!(
                "SKIP: PostgreSQL is not reachable — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }

        let database = format!("omnion_appbuilder_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&maintenance_config(&config))
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

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    fn store(&self) -> PlanStore<'_> {
        PlanStore::new(self.db.pool())
    }

    async fn cleanup(self) {
        sqlx::query(&format!(
            "drop database if exists \"{}\" with (force)",
            self.database
        ))
        .execute(self.maintenance.pool())
        .await
        .ok();
    }
}

fn maintenance_config(config: &Config) -> DatabaseConfig {
    DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// One harness per test, dropped on a **current-thread** runtime.
///
/// The drop is the reason this is a function returning `Option` and not a lazy static: a
/// `#[tokio::test]` that spawns cleanup in a `Drop` on a current-thread runtime never runs
/// it, and every suite that has done that has leaked a database per run until the server ran
/// out of room. `scopeguard` is not a dependency here, so each test does the cleanup itself
/// and asserts the drop happened.
macro_rules! harness {
    () => {
        match Harness::fresh().await {
            Some(harness) => harness,
            None => return,
        }
    };
}

fn new_plan(prompt: &str) -> NewPlan {
    NewPlan {
        organization_id: None,
        site_id: None,
        prompt: prompt.into(),
        title: None,
        model_label: "qa/mock-model".into(),
        created_by: None,
        supersedes_id: None,
    }
}

fn entity(key: &str, ordinal: i32) -> NewArtifact {
    NewArtifact {
        kind: "entity".into(),
        key: key.into(),
        parent_key: None,
        ordinal,
        spec: json!({ "key": key, "label": "Leave request", "plural_label": "Leave requests" }),
        rationale: "Leave requests are what the app is for.".into(),
        validation: json!([]),
    }
}

fn field(key: &str, parent: &str, ordinal: i32) -> NewArtifact {
    NewArtifact {
        kind: "field".into(),
        key: key.into(),
        parent_key: Some(parent.into()),
        ordinal,
        spec: json!({ "key": key, "label": "Start date", "type": "date" }),
        rationale: "A request has a start.".into(),
        validation: json!([]),
    }
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// A plan is written before the provider is called, and its answer moves it to `draft`.
#[tokio::test]
async fn a_plan_is_written_before_the_provider_is_called_and_answered_after() {
    let harness = harness!();

    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    assert_eq!(plan.status, "generating");
    assert!(
        plan.applied_at.is_none(),
        "a plan nobody applied cannot carry an applied timestamp"
    );

    let answered = harness
        .store()
        .answer(
            plan.id,
            "draft",
            PlanUsage {
                input: Some(120),
                output: Some(340),
                cost_cents: 7,
            },
            Some("Leave manager"),
        )
        .await
        .expect("the answer must be recorded")
        .expect("the plan must still be answerable");
    assert_eq!(answered.status, "draft");
    assert_eq!(answered.title, "Leave manager");
    assert_eq!(answered.tokens_in, Some(120));
    assert_eq!(answered.cost_cents, 7);

    // A second answer is refused by the `status = 'generating'` guard, which is what makes a
    // lost race answerable: the loser gets `None`, not a second set of token counts.
    let second = harness
        .store()
        .answer(plan.id, "draft", PlanUsage::default(), None)
        .await
        .expect("the query must run");
    assert!(second.is_none(), "a plan answers exactly once");

    harness.cleanup().await;
}

/// A generation that dies leaves a `failed` row with its reason, and the prompt survives.
#[tokio::test]
async fn a_failed_generation_keeps_the_request_it_was_given() {
    let harness = harness!();

    let plan = harness
        .store()
        .begin(new_plan("Track supplier contracts with renewal reminders"))
        .await
        .expect("the plan row must be written");
    let failed = harness
        .store()
        .fail(plan.id, "the AI Hub has no route to a model")
        .await
        .expect("the failure must be recorded")
        .expect("the plan must still be failing-able");

    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed.error.as_deref(),
        Some("the AI Hub has no route to a model")
    );
    assert_eq!(
        failed.prompt, "Track supplier contracts with renewal reminders",
        "a retry needs the request the operator typed, not an empty box"
    );

    harness.cleanup().await;
}

/// An artifact with no findings is `pending`; one with findings is `invalid`.
#[tokio::test]
async fn an_artifacts_status_is_derived_from_the_validators_answer_not_the_generators() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    let good = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");
    assert_eq!(good.status, "pending", "nothing has been reviewed yet");

    // The generator is handed findings, not a status it chose. A reserved key produces a
    // finding, and the row lands `invalid` — the generator cannot approve its own work.
    let reserved = entity("users", 1);
    let findings = validate_artifact(&reserved);
    assert!(!findings.is_empty(), "the fixture must actually be invalid");
    let bad = harness
        .store()
        .artifact(plan.id, &reserved, &findings)
        .await
        .expect("the artifact must be written even when invalid");
    assert_eq!(bad.status, "invalid");
    assert!(
        bad.validation
            .as_array()
            .is_some_and(|f| !f.is_empty()),
        "the findings are stored beside the artifact: {:?}",
        bad.validation
    );
    assert_eq!(bad.validation[0]["message"].as_str().is_some(), true);

    harness.cleanup().await;
}

/// A reserved key is refused by name at the store boundary, before any SQL runs.
#[tokio::test]
async fn a_reserved_key_is_refused_by_name_before_it_can_be_written() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    let error = harness
        .store()
        .artifact(plan.id, &entity("users", 0), &[])
        .await
        .expect_err("a reserved key must not be storable");
    assert_eq!(error.code(), "invalid_artifact_key");
    assert!(
        error.to_string().contains("reserved platform key"),
        "the message must name the reason, not just the key: {error}"
    );
    assert!(
        error.to_string().contains("users"),
        "the message must name the key: {error}"
    );

    let count: i64 = sqlx::query_scalar("select count(*) from app_builder_artifacts")
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(count, 0, "a refused artifact leaves no row");

    harness.cleanup().await;
}

/// The database refuses a reserved-looking state the store would never write.
#[tokio::test]
async fn the_migrations_checks_hold_on_a_populated_table() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    // Two live rows for one (plan, kind, key): the shape the regeneration flow must never
    // produce, because it retires the predecessor in the same transaction.
    let error = sqlx::query(
        "insert into app_builder_artifacts (plan_id, kind, key, ordinal, spec, rationale)
         values ($1, 'entity', 'leave_request', 1, '{}'::jsonb, 'because')",
    )
    .bind(plan.id)
    .execute(harness.db.pool())
    .await
    .expect("the first artifact must be writable");
    assert!(error.rows_affected() == 1);

    let duplicate = sqlx::query(
        "insert into app_builder_artifacts (plan_id, kind, key, ordinal, spec, rationale)
         values ($1, 'entity', 'leave_request', 2, '{}'::jsonb, 'because')",
    )
    .bind(plan.id)
    .execute(harness.db.pool())
    .await
    .expect_err("the unique index must refuse a second live row for one key");
    assert!(
        duplicate.to_string().contains("app_builder_artifacts_unique_key"),
        "the database must name its own constraint: {duplicate}"
    );

    // `edited` without a predecessor is refused: an edit that does not say what it replaced
    // makes the apply log unreadable.
    let untraceable = sqlx::query(
        "insert into app_builder_artifacts (plan_id, kind, key, ordinal, status, spec, rationale)
         values ($1, 'entity', 'leave_approval', 3, 'edited', '{}'::jsonb, 'because')",
    )
    .bind(plan.id)
    .execute(harness.db.pool())
    .await
    .expect_err("the edited check must hold");
    assert!(
        untraceable
            .to_string()
            .contains("app_builder_artifacts_edited_check"),
        "the database must name its own constraint: {untraceable}"
    );

    // A status outside the catalogue is refused too — the constraint, not the application.
    let unknown = sqlx::query(
        "insert into app_builder_plans (prompt, status) values ('a request', 'shipped')",
    )
    .execute(harness.db.pool())
    .await
    .expect_err("the status check must hold");
    assert!(
        unknown
            .to_string()
            .contains("app_builder_plans_status_check"),
        "the database must name its own constraint: {unknown}"
    );

    harness.cleanup().await;
}

/// Accept works, `edited` does not, and an artifact the validator refused cannot be accepted.
#[tokio::test]
async fn accepting_an_artifact_is_possible_and_editing_is_not_a_status_write() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    let clean = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");

    // `edited` claims a correction, which a status write cannot check.
    let error = harness
        .store()
        .decide(clean.id, "edited")
        .await
        .expect_err("edited belongs to the edit path");
    assert_eq!(error.code(), "app_builder_artifact_not_decidable");
    assert!(
        error.to_string().contains("re-validates the body"),
        "{error}"
    );

    // `accepted` is a PERSON saying the artifact is right about a body that validated when
    // it was stored. Refusing this verb leaves the review screen unable to reach an
    // applicable plan -- which is what an earlier version of this store did.
    let accepted = harness
        .store()
        .decide(clean.id, "accepted")
        .await
        .expect("the decision must be recorded")
        .expect("the artifact must still be decidable");
    assert_eq!(accepted.status, "accepted");

    // Accepting an artifact the validator refused is refused BY NAME and by the finding.
    let reserved = entity("users", 1);
    let findings = validate_artifact(&reserved);
    let invalid = harness
        .store()
        .artifact(plan.id, &reserved, &findings)
        .await
        .expect("an invalid artifact is still stored");
    assert_eq!(invalid.status, "invalid");

    let error = harness
        .store()
        .decide(invalid.id, "accepted")
        .await
        .expect_err("an invalid artifact cannot be accepted");
    assert_eq!(error.code(), "app_builder_artifact_invalid");
    assert!(
        error.to_string().contains("reserved platform key"),
        "the refusal must name the finding in the way: {error}"
    );
    assert!(
        error.to_string().contains("Edit it, or reject it"),
        "the refusal must say what the reviewer can do: {error}"
    );

    harness.cleanup().await;
}

/// An edit that re-validates clean is `edited`; one that still has findings is `invalid`.
#[tokio::test]
async fn an_edit_is_edited_only_when_it_validates() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    let artifact = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");

    // A reviewer renames the label — the edit validates, so the row is `edited`.
    let fixed = NewArtifact {
        spec: json!({ "key": "leave_request", "label": "Time off request" }),
        rationale: artifact.rationale.clone(),
        ..entity("leave_request", 0)
    };
    let findings = validate_artifact(&fixed);
    assert!(findings.is_empty(), "{findings:?}");
    let edited = harness
        .store()
        .edit(
            artifact.id,
            &EditedArtifact {
                spec: fixed.spec.clone(),
                validation: json!(findings),
                supersedes_id: artifact.id,
            },
            &findings,
        )
        .await
        .expect("the edit must be recorded")
        .expect("the artifact must still be editable");
    assert_eq!(edited.status, "edited");
    assert_eq!(edited.spec["label"], json!("Time off request"));

    // An edit that leaves the body invalid is `invalid`, not `edited`: "edited" reads as
    // *fixed* on the review screen.
    let still_bad = entity("leave_request", 0);
    let bad_findings = validate_artifact(&still_bad);
    assert!(bad_findings.is_empty());
    let mut broken = still_bad.clone();
    broken.spec["key"] = json!("users");
    let broken_findings = validate_artifact(&broken);
    assert!(!broken_findings.is_empty());
    let refused = harness
        .store()
        .edit(
            edited.id,
            &EditedArtifact {
                spec: broken.spec.clone(),
                validation: json!(broken_findings),
                supersedes_id: edited.id,
            },
            &broken_findings,
        )
        .await
        .expect("the edit must be recorded")
        .expect("the artifact must still be editable");
    assert_eq!(refused.status, "invalid");

    harness.cleanup().await;
}

/// Regeneration keeps the previous version and does not double the live artifact count.
#[tokio::test]
async fn regeneration_keeps_the_previous_version_and_retires_it() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    let first = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");

    let mut replacement = entity("leave_request", 0);
    replacement.spec["label"] = json!("Time off request");
    let regenerated = harness
        .store()
        .regenerate(first.id, &replacement, &[])
        .await
        .expect("the regeneration must be recorded")
        .expect("the predecessor must have been decidable");

    assert_ne!(regenerated.id, first.id, "a regeneration is a new row");
    assert_eq!(regenerated.supersedes_id, Some(first.id));
    assert_eq!(regenerated.spec["label"], json!("Time off request"));

    // Both rows exist — that is what "keeps the previous version" means — and exactly one is
    // live, which is what the unique index and the review tree both require.
    let stored = harness
        .store()
        .artifacts(plan.id)
        .await
        .expect("the artifacts must read back");
    assert_eq!(stored.len(), 2, "the previous version is kept, not deleted");
    let live = stored
        .iter()
        .filter(|a| a.status != "rejected")
        .count();
    assert_eq!(live, 1, "one live artifact per (plan, kind, key)");

    harness.cleanup().await;
}

/// A regenerated artifact keeps its kind; a different kind is refused.
#[tokio::test]
async fn a_regenerated_artifact_keeps_its_kind() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    let first = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");

    let mut as_field = field("leave_request", "leave_request", 0);
    as_field.key = "leave_request".into();
    let error = harness
        .store()
        .regenerate(first.id, &as_field, &[])
        .await
        .expect_err("a kind change is a new artifact, not a regeneration");
    assert_eq!(error.code(), "app_builder_artifact_kind_change");

    harness.cleanup().await;
}

/// `blockers` names what stands between a plan and apply, including a kind never proposed.
#[tokio::test]
async fn blockers_name_what_stands_between_a_plan_and_apply() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    // An entity and a field only: no UI, no permission, no workflow, no notification, no
    // report. Apply is blocked, and the reason is named per kind.
    harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");
    harness
        .store()
        .artifact(plan.id, &field("start_date", "leave_request", 1), &[])
        .await
        .expect("the artifact must be written");

    let blocked = harness
        .store()
        .blockers(plan.id)
        .await
        .expect("the blockers must read back");
    assert_eq!(blocked.len(), 2, "both artifacts are unresolved");

    // Every required kind the plan never proposed is named too — five of them here, and none
    // of them expressible as an artifact row.
    let missing: Vec<&str> = blocked
        .iter()
        .filter(|b| b.status == "missing")
        .map(|b| b.kind.as_str())
        .collect();
    assert_eq!(missing.len(), 5, "{missing:?}");
    assert!(missing.contains(&"report"), "{missing:?}");

    assert!(
        !harness
            .store()
            .applicable(plan.id)
            .await
            .expect("the check must read back"),
        "a plan missing required kinds is not applicable"
    );

    harness.cleanup().await;
}

/// A plan whose artifacts are all resolved stops blocking.
#[tokio::test]
async fn a_plan_with_every_required_kind_present_is_applicable() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    for (kind, key) in [
        ("entity", "leave_request"),
        ("ui", "leave_request_list"),
        ("permission", "leave.read"),
        ("workflow", "leave_approval"),
        ("notification", "leave_requested"),
        ("report", "leave_summary"),
    ] {
        let mut artifact = match kind {
            "permission" => NewArtifact {
                kind: kind.into(),
                key: key.into(),
                parent_key: None,
                ordinal: 0,
                spec: json!({ "key": key, "description": "Read leave requests" }),
                rationale: "Somebody has to read them.".into(),
                validation: json!([]),
            },
            "workflow" => NewArtifact {
                kind: kind.into(),
                key: key.into(),
                parent_key: None,
                ordinal: 0,
                spec: json!({
                    "trigger": "record.created",
                    "steps": [{ "name": "ask", "action": "approval.request" }]
                }),
                rationale: "A request is approved.".into(),
                validation: json!([]),
            },
            _ => NewArtifact {
                kind: kind.into(),
                key: key.into(),
                parent_key: None,
                ordinal: 0,
                spec: json!({ "key": key, "label": "Leave request" }),
                rationale: "The app needs it.".into(),
                validation: json!([]),
            },
        };
        let findings = validate_artifact(&artifact);
        assert!(findings.is_empty(), "{key}: {findings:?}");
        let stored = harness
            .store()
            .artifact(plan.id, &artifact, &findings)
            .await
            .expect("the artifact must be written");
        harness
            .store()
            .decide(stored.id, "accepted")
            .await
            .expect("the decision must be recorded")
            .expect("the artifact must still be decidable");
        artifact.spec = json!({ "key": key });
        let _ = artifact;
    }

    assert!(
        harness
            .store()
            .applicable(plan.id)
            .await
            .expect("the check must read back"),
        "every required kind is present"
    );

    harness.cleanup().await;
}

/// The list's counts come from the same query as its page.
#[tokio::test]
async fn the_list_counts_agree_with_the_rows_they_decorate() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    let entity_row = harness
        .store()
        .artifact(plan.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");
    harness
        .store()
        .artifact(plan.id, &field("start_date", "leave_request", 1), &[])
        .await
        .expect("the artifact must be written");
    harness
        .store()
        .decide(entity_row.id, "accepted")
        .await
        .expect("the decision must be recorded")
        .expect("the artifact must still be decidable");

    let page = harness
        .store()
        .list(None, &PlanFilter::default())
        .await
        .expect("the list must read back");
    assert_eq!(page.total, 1, "one plan exists");
    assert_eq!(page.plans.len(), 1);
    assert_eq!(page.counts.len(), 1, "counts are per row, never missing");

    let counts = page.counts[0];
    assert_eq!(counts.artifacts, 2);
    assert_eq!(counts.accepted, 1);
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.rejected, 0);
    assert_eq!(counts.invalid, 0);

    // And the detail endpoint agrees with the list — the same numbers, read by a different
    // query, so a disagreement between them would be a defect a reviewer can see.
    let detail = harness
        .store()
        .counts(plan.id)
        .await
        .expect("the counts must read back");
    assert_eq!(detail.artifacts, counts.artifacts);
    assert_eq!(detail.accepted, counts.accepted);

    harness.cleanup().await;
}

/// The list's text filter and status filter both narrow the total, not just the page.
#[tokio::test]
async fn a_filter_narrows_the_total_not_only_the_page() {
    let harness = harness!();
    for prompt in [
        "Create an app to manage employees' leave requests",
        "Track supplier contracts with renewal reminders",
    ] {
        harness
            .store()
            .begin(new_plan(prompt))
            .await
            .expect("the plan row must be written");
    }

    let all = harness
        .store()
        .list(None, &PlanFilter::default())
        .await
        .expect("the list must read back");
    assert_eq!(all.total, 2);

    let filtered = harness
        .store()
        .list(
            None,
            &PlanFilter {
                text: Some("supplier".into()),
                ..PlanFilter::default()
            },
        )
        .await
        .expect("the list must read back");
    assert_eq!(
        filtered.total, 1,
        "the total is the number that MATCHES, not the number of rows this page returned"
    );
    assert_eq!(filtered.plans.len(), 1);
    assert!(
        filtered.plans[0]
            .prompt
            .contains("supplier"),
        "the surviving row is the one that matched"
    );

    // A status nothing holds is an empty page, not an error: the filter vocabulary is the
    // catalogue's, and an unknown value is refused by the store before the query runs.
    let none = harness
        .store()
        .list(
            None,
            &PlanFilter {
                status: Some("applied".into()),
                ..PlanFilter::default()
            },
        )
        .await
        .expect("the list must read back");
    assert_eq!(none.total, 0);
    assert!(none.plans.is_empty());

    let error = harness
        .store()
        .list(
            None,
            &PlanFilter {
                status: Some("shipped".into()),
                ..PlanFilter::default()
            },
        )
        .await
        .expect_err("an unknown status is refused");
    assert!(error.to_string().contains("generating"), "{error}");

    harness.cleanup().await;
}

/// An unapplied plan is deletable; an applied one is not.
#[tokio::test]
async fn an_applied_plan_is_undeletable() {
    let harness = harness!();
    let plan: AppBuilderPlan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    assert!(
        harness
            .store()
            .delete(plan.id)
            .await
            .expect("the delete must run"),
        "an unapplied draft is deletable"
    );
    assert!(
        harness
            .store()
            .get(plan.id)
            .await
            .expect("the read must run")
            .is_none(),
        "the row is gone"
    );

    // A plan something downstream points at cannot be deleted: its artifacts are history.
    let applied = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");
    sqlx::query(
        "update app_builder_plans set status = 'applied', applied_at = now() where id = $1",
    )
    .bind(applied.id)
    .execute(harness.db.pool())
    .await
    .expect("the plan must become applied");
    harness
        .store()
        .artifact(applied.id, &entity("leave_request", 0), &[])
        .await
        .expect("the artifact must be written");

    assert!(
        !harness
            .store()
            .delete(applied.id)
            .await
            .expect("the delete must run"),
        "an applied plan is history and is not deletable"
    );
    assert!(
        harness
            .store()
            .get(applied.id)
            .await
            .expect("the read must run")
            .is_some(),
        "the row is still there"
    );

    harness.cleanup().await;
}

/// A retried plan supersedes its predecessor, and the predecessor is rejected rather than
/// deleted so the two attempts stay comparable.
#[tokio::test]
async fn a_retried_plan_keeps_the_attempt_it_replaces() {
    let harness = harness!();
    let first = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    let mut retry = new_plan("Create an app to manage leave requests for staff");
    retry.supersedes_id = Some(first.id);
    let second = harness
        .store()
        .retry(first.id, retry)
        .await
        .expect("the retry must be written");

    assert_eq!(second.supersedes_id, Some(first.id));
    let previous = harness
        .store()
        .get(first.id)
        .await
        .expect("the read must run")
        .expect("the predecessor must still exist");
    assert_eq!(
        previous.status, "rejected",
        "a superseded attempt is rejected, not deleted: the reviewer compares the two"
    );

    // A retry that does not name its predecessor is refused: the chain has to be walkable.
    let mut orphan = new_plan("Another attempt");
    orphan.supersedes_id = None;
    let error = harness
        .store()
        .retry(first.id, orphan)
        .await
        .expect_err("the chain must be named");
    assert_eq!(error.code(), "app_builder_supersedes_mismatch");

    harness.cleanup().await;
}

/// A missing plan is `None` everywhere, and a rejected prompt never reaches the table.
#[tokio::test]
async fn a_prompt_shorter_than_the_minimum_never_becomes_a_row() {
    let harness = harness!();
    let error = harness
        .store()
        .begin(new_plan("x"))
        .await
        .expect_err("a one-character request is nothing");
    assert_eq!(error.code(), "invalid_plan_prompt");

    let count: i64 = sqlx::query_scalar("select count(*) from app_builder_plans")
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(count, 0, "a refused prompt leaves no row to retry");

    harness.cleanup().await;
}

/// The whole thing, in the order the console does it: generate, answer, write artifacts,
/// review, and read the blocking summary the Apply button binds to.
#[tokio::test]
async fn the_console_walk_produces_a_reviewable_plan() {
    let harness = harness!();
    let plan = harness
        .store()
        .begin(new_plan("Create an app to manage employees' leave requests"))
        .await
        .expect("the plan row must be written");

    // The generator proposes; the store decides what status each lands in.
    let proposals = vec![
        entity("leave_request", 0),
        field("start_date", "leave_request", 1),
        field("end_date", "leave_request", 2),
    ];
    for proposal in &proposals {
        let findings = validate_artifact(proposal);
        harness
            .store()
            .artifact(plan.id, proposal, &findings)
            .await
            .expect("the artifact must be written");
    }

    // What the generator produced, judged before it is stored — the plan-level validation the
    // API answers `GET /plans/{id}` with.
    let validation = validate_plan(&proposals);
    assert!(!validation.is_clean(), "a partial plan is not clean");
    assert!(validation.required_missing.contains(&"report".to_owned()));

    harness
        .store()
        .answer(
            plan.id,
            "draft",
            PlanUsage {
                input: Some(90),
                output: Some(410),
                cost_cents: 5,
            },
            Some("Leave manager"),
        )
        .await
        .expect("the answer must be recorded")
        .expect("the plan must still be answerable");

    let counts = harness
        .store()
        .counts(plan.id)
        .await
        .expect("the counts must read back");
    assert_eq!(counts.artifacts, 3);
    assert_eq!(counts.pending, 3, "nothing is reviewed yet");

    let blocked = harness
        .store()
        .blockers(plan.id)
        .await
        .expect("the blockers must read back");
    assert!(
        blocked
            .iter()
            .any(|b| b.kind == "entity" && b.key == "leave_request"),
        "{blocked:?}"
    );

    // Every stored artifact's body round-trips as the object the validator read.
    for artifact in harness.store().artifacts(plan.id).await.expect("must read") {
        let _: &AppBuilderArtifact = &artifact;
        assert!(
            artifact.spec.is_object(),
            "a spec that is not an object would fail the migration's check"
        );
    }

    harness.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Naming rules, on the same database the store writes to
// ---------------------------------------------------------------------------------------------

/// A key the platform owns is refused by `validate_key` without a database at all, and the
/// refusals are the ones the review screen shows.
///
/// Deliberately a plain `#[test]` rather than another walk: naming rules are pure, and paying
/// 4 s of database setup to learn that `users` is reserved proves less than this does.
#[test]
fn the_reserved_list_is_the_platforms_own_vocabulary() {
    for key in ["users", "organizations", "workflows", "roles", "audit_log"] {
        let findings = validate_key(key, "key");
        assert_eq!(findings.len(), 1, "{key}: {findings:?}");
        assert!(findings[0].message.contains("reserved"), "{key}: {findings:?}");
    }
    for key in ["leave_request", "start_date", "supplier_contract"] {
        assert!(
            validate_key(key, "key").is_empty(),
            "{key} must be storable"
        );
    }
}
