//! Walks for eval suites and cases (REQ-107, slice 1).
//!
//! `eval_case.rs` proves the *scoring* with no database at all, and `eval_store.rs` proves the
//! *rules* the same way. Everything here is a claim about **rows** — so it needs a real
//! database, and the claims are the ones a unit test structurally cannot make:
//!
//! - **The target constraint is the database's, not the API's.** A suite row is inserted
//!   directly, with a target that disagrees with its reference, and the insert is refused. The
//!   unit test proves the validator refuses it; this proves there is no *other* path that writes
//!   the row, which is the property that makes the validator worth having.
//! - **A case that asserts nothing is refused by the column.** Again by direct insert, because a
//!   case with an empty `expected` document is the one that silently inflates a pass rate, and
//!   the column is the last place it can be caught.
//! - **Keys are per tenant, and a second tenant may reuse one.** Two organizations, the same
//!   key, both rows exist. A globally unique key would make the second suite's save fail with a
//!   violation that names no field.
//! - **Tenancy is a 404, not a 403.** Another organization's suite read by id and by key answers
//!   "not found" — otherwise the suite screen becomes an existence oracle for every suite in
//!   the installation.
//! - **A rubric case makes a blocking suite need a judge, and the rule lands on edit.** The
//!   acceptance row is about the *combination*, so the walk creates a valid suite, adds the
//!   rubric case, and then tries to clear the judge through the edit path — the path an
//!   operator actually uses, and the one a create-time check alone would miss.
//! - **The judge's prompt version moves only when the text moves.** A run pins the version, so a
//!   version that drifts without the text drifting would make two runs look different when they
//!   were not.
//! - **A case edit is validated on the merged row.** Removing the only property is refused even
//!   though the patch itself is well-formed.
//!
//! The harness is the throwaway-database pattern the other AI suites use, and it **panics**
//! rather than skipping when PostgreSQL is unreachable — a skipped walk proves nothing.

use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::eval_case;
use omnion_ai_hub::eval_store::{
    self, CaseChanges, CaseRow, NewCase, NewSuite, SuiteChanges, SuiteRow,
};
use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use sqlx::PgPool;
use uuid::Uuid;

struct EvalAi {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl EvalAi {
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
        let database = format!("omnion_eval_{}", Uuid::new_v4().simple());
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
        Some(Self { pool: db.pool().clone(), database, maintenance: Some(maintenance) })
    }

    /// A tenant, created directly because nothing in this request needs a signup flow.
    async fn organization(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(id)
            .bind(format!("eval {id}"))
            .bind(format!("eval-{id}"))
            .execute(&self.pool)
            .await
            .expect("the tenant must be created");
        id
    }

    /// A provider and two models, so a suite has something real to target.
    ///
    /// The fixture originally pinned `Uuid::from_u128(1)`, and the walk failed on the foreign
    /// key — which is the constraint working. A suite that targets a model nobody registered
    /// is a suite that fails at run time with "unknown target" and no way for the editor to
    /// have caught it, so the walk creates real rows and asserts against those ids.
    async fn models(&self) -> (Uuid, Uuid) {
        let provider = Uuid::new_v4();
        sqlx::query(
            "insert into ai_providers (id, name, kind, base_url, enabled) \
             values ($1, $2, 'cloud', 'https://models.invalid/v1', true)",
        )
        .bind(provider)
        .bind(format!("eval provider {provider}"))
        .execute(&self.pool)
        .await
        .expect("the provider must be created");
        let mut ids = Vec::new();
        for key in ["eval-model-under-test", "eval-model-judge"] {
            let id = Uuid::new_v4();
            sqlx::query(
                "insert into ai_models (id, provider_id, model_key, display_name) \
                 values ($1, $2, $3, $3)",
            )
            .bind(id)
            .bind(provider)
            .bind(key)
            .execute(&self.pool)
            .await
            .expect("the model must be created");
            ids.push(id);
        }
        (ids[0], ids[1])
    }

    /// A minimal valid suite for this tenant, targeting a real model.
    fn suite_for(&self, key: &str, model: Uuid) -> NewSuite {
        NewSuite {
            key: key.to_owned(),
            name: format!("Suite {key}"),
            description: "walk fixture".to_owned(),
            target: "model".to_owned(),
            model_id: Some(model),
            tools: serde_json::json!([]),
            collections: serde_json::json!([]),
            threshold_percent: 90,
            max_regression_points: 5.0,
            blocking: false,
            schedule: None,
            judge_model_id: None,
            judge_prompt: None,
            enabled: true,
            created_by: None,
            ..Default::default()
        }
    }

    /// A case asserting one property.
    fn case(&self, name: &str, expected: serde_json::Value) -> NewCase {
        NewCase {
            name: name.to_owned(),
            input: serde_json::json!({ "prompt": "hello" }),
            expected,
            weight: 1.0,
            tags: vec!["smoke".to_owned()],
            enabled: true,
            source: "manual".to_owned(),
            source_run_id: None,
        }
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

/// The harness, or a panic.
macro_rules! evals {
    () => {
        match EvalAi::fresh().await {
            Some(fixture) => fixture,
            None => panic!(
                "PostgreSQL is required for the REQ-107 walks; a skipped walk proves nothing"
            ),
        }
    };
}

/// Read a suite, insisting it exists.
async fn suite_of(pool: &PgPool, organization_id: Uuid, key: &str) -> SuiteRow {
    eval_store::find_suite(pool, organization_id, key)
        .await
        .expect("the read must succeed")
        .unwrap_or_else(|| panic!("the suite {key} must exist"))
}

#[tokio::test]
async fn a_suite_is_stored_with_the_counts_the_list_renders() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;

    let created = eval_store::create_suite(&fx.pool, org, &fx.suite_for("support-replies", model))
        .await
        .expect("the suite must be created");
    assert_eq!(created.key, "support-replies");
    assert_eq!(created.threshold_percent, 90);
    assert_eq!(created.case_count, 0);
    assert_eq!(created.enabled_case_count, 0);
    assert_eq!(created.rubric_case_count, 0);
    assert!(created.last_pass_rate.is_none(), "a suite that never ran has no pass rate");

    eval_store::create_case(
        &fx.pool,
        org,
        created.id,
        &fx.case("exact", serde_json::json!({ "exact": "yes" })),
    )
    .await
    .expect("the case must be created");
    eval_store::create_case(
        &fx.pool,
        org,
        created.id,
        &fx.case(
            "rubric",
            serde_json::json!({ "rubric": "one clear sentence" }),
        ),
    )
    .await
    .expect("the rubric case must be created");
    eval_store::create_case(
        &fx.pool,
        org,
        created.id,
        &NewCase {
            enabled: false,
            ..fx.case("disabled", serde_json::json!({ "contains": ["x"] }))
        },
    )
    .await
    .expect("the disabled case must be created");

    let listed = eval_store::find_suite(&fx.pool, org, "support-replies")
        .await
        .expect("the read must succeed")
        .expect("the suite must be there");
    assert_eq!(listed.case_count, 3, "every case counts, enabled or not");
    assert_eq!(listed.enabled_case_count, 2, "a run would execute only the enabled ones");
    assert_eq!(
        listed.rubric_case_count, 1,
        "the suite header has to know whether a judge is required"
    );

    // A run executes the enabled cases only, and that is a different question from what the
    // cases tab shows — which is why the runner has its own read.
    let for_run = eval_store::cases_for_run(&fx.pool, org, created.id)
        .await
        .expect("the runner read must succeed");
    assert_eq!(for_run.len(), 2, "a disabled case never runs");
    assert!(for_run.iter().all(|c| c.enabled));

    fx.dispose().await;
}

#[tokio::test]
async fn the_target_constraint_is_the_databases_and_not_only_the_validators() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;

    // Direct insert: a suite that says it targets an agent while pinning a model. The
    // validator refuses this, and the column refuses it too — which is what proves there is no
    // second path that writes the row.
    let error = sqlx::query(
        "insert into ai_eval_suites (organization_id, key, name, target, model_id) \
         values ($1, 'mismatched', 'Mismatched', 'agent', $2)",
    )
    .bind(org)
    .bind(model)
    .execute(&fx.pool)
    .await
    .expect_err("a target that disagrees with its reference must be refused by the column");
    let message = error.to_string();
    assert!(
        message.contains("ai_eval_suites_target_is_one"),
        "the refusal must name the constraint, got: {message}"
    );

    // And the same row through the store is refused by the validator with a message an
    // operator can act on.
    let mut suite = fx.suite_for("mismatched", model);
    // An agent id that does not exist is a second, unrelated failure; the walk wants the
    // TARGET rule, so the row has to be otherwise perfect.
    let agent = Uuid::new_v4();
    sqlx::query(
        "insert into ai_agents (id, organization_id, key, name, model_id) \
         values ($1, $2, $3, $3, $4)",
    )
    .bind(agent)
    .bind(org)
    .bind(format!("eval-agent-{agent}"))
    .bind(model)
    .execute(&fx.pool)
    .await
    .expect("the agent must be created");
    suite.agent_id = Some(agent);
    let error = eval_store::create_suite(&fx.pool, org, &suite)
        .await
        .expect_err("the store must refuse the same combination");
    assert_eq!(error.code(), "invalid_eval");

    fx.dispose().await;
}

#[tokio::test]
async fn a_case_that_asserts_nothing_is_refused_by_the_column() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let suite = eval_store::create_suite(&fx.pool, org, &fx.suite_for("coverage", model))
        .await
        .expect("the suite must be created");

    // Direct insert with an empty expectations document. A case asserting nothing passes every
    // model including a broken one, so it contributes only false confidence to a pass rate.
    let error = sqlx::query(
        "insert into ai_eval_cases (suite_id, organization_id, name, input, expected) \
         values ($1, $2, 'empty', '{\"prompt\":\"hi\"}'::jsonb, '{}'::jsonb)",
    )
    .bind(suite.id)
    .bind(org)
    .execute(&fx.pool)
    .await
    .expect_err("an empty expected document must be refused by the column");
    assert!(
        error.to_string().contains("ai_eval_cases_expects_something"),
        "the refusal must name the constraint, got: {error}"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn keys_are_per_tenant_so_a_second_tenant_may_reuse_one() {
    let fx = evals!();
    let first = fx.organization().await;
    let (model, judge) = fx.models().await;
    let second = fx.organization().await;
    let (model, judge) = fx.models().await;

    eval_store::create_suite(&fx.pool, first, &fx.suite_for("seo-refresh", model))
        .await
        .expect("the first tenant's suite must be created");

    // The same key in another tenant is normal, not a collision.
    let second_suite = eval_store::create_suite(&fx.pool, second, &fx.suite_for("seo-refresh", model))
        .await
        .expect("two tenants may both own a suite with the same key");
    assert_eq!(second_suite.key, "seo-refresh");

    // Within one tenant it is a conflict with its own code, so the editor can offer another.
    let error = eval_store::create_suite(&fx.pool, first, &fx.suite_for("seo-refresh", model))
        .await
        .expect_err("the same tenant may not take the key twice");
    assert_eq!(error.code(), "eval_suite_key_taken");
    assert!(error.to_string().contains("seo-refresh"), "{error}");

    fx.dispose().await;
}

#[tokio::test]
async fn another_tenants_suite_is_not_found_and_never_forbidden() {
    let fx = evals!();
    let mine = fx.organization().await;
    let (model, judge) = fx.models().await;
    let theirs = fx.organization().await;
    let (model, judge) = fx.models().await;

    let their_suite = eval_store::create_suite(&fx.pool, theirs, &fx.suite_for("their-secret", model))
        .await
        .expect("the other tenant's suite must be created");
    eval_store::create_suite(&fx.pool, mine, &fx.suite_for("mine", model))
        .await
        .expect("this tenant's suite must be created");

    // By key and by id, both answer "not found". A 403 here would turn the suite screen into an
    // existence oracle for every suite key in the installation.
    assert!(
        eval_store::find_suite(&fx.pool, mine, "their-secret")
            .await
            .expect("the read must succeed")
            .is_none(),
        "another tenant's key must not resolve"
    );
    assert!(
        eval_store::find_suite_by_id(&fx.pool, mine, their_suite.id)
            .await
            .expect("the read must succeed")
            .is_none(),
        "another tenant's id must not resolve"
    );
    assert!(
        eval_store::list_cases(&fx.pool, mine, their_suite.id)
            .await
            .expect("the read must succeed")
            .is_empty(),
        "another tenant's cases must not be listed"
    );

    // And a delete aimed at their id reports "not found" rather than succeeding.
    let error = eval_store::delete_suite(&fx.pool, mine, their_suite.id)
        .await
        .expect_err("deleting another tenant's suite must be refused");
    assert_eq!(error.code(), "eval_suite_not_found");
    assert!(
        eval_store::find_suite_by_id(&fx.pool, theirs, their_suite.id)
            .await
            .expect("the read must succeed")
            .is_some(),
        "the other tenant's suite must still exist"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn a_blocking_suite_with_a_rubric_case_needs_a_judge_and_the_rule_lands_on_edit() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let mut suite_spec = fx.suite_for("release-gate", model);
    suite_spec.blocking = true;
    suite_spec.judge_model_id = Some(judge);
    let suite = eval_store::create_suite(&fx.pool, org, &suite_spec)
        .await
        .expect("a blocking suite with a judge must be created");

    eval_store::create_case(
        &fx.pool,
        org,
        suite.id,
        &fx.case(
            "tone",
            serde_json::json!({ "rubric": "answers in one courteous sentence" }),
        ),
    )
    .await
    .expect("the rubric case must be created");
    assert!(eval_store::suite_has_rubric_cases(&fx.pool, suite.id)
        .await
        .expect("the check must run"));
    assert_eq!(suite.rubric_case_count, 0, "the count was read before the case existed");

    // The combination is the acceptance row: a blocking suite whose cases need a judge cannot
    // lose its judge. This is the EDIT path, which a create-time check alone would miss — and
    // the rule is checked after the write, so the suite keeps the judge it had.
    let error = eval_store::update_suite(
        &fx.pool,
        org,
        suite.id,
        &SuiteChanges { judge_model_id: Some(None), ..Default::default() },
    )
    .await
    .expect_err("clearing the judge on a blocking rubric suite must be refused");
    assert!(error.to_string().contains("judge_model_id"), "{error}");

    // The refusal did not silently drop the judge.
    let stored = suite_of(&fx.pool, org, "release-gate").await;
    assert_eq!(
        stored.judge_model_id,
        Some(judge),
        "a refused edit must not half-apply"
    );

    // Clearing the judge is fine once the suite is no longer blocking, and that is the
    // difference between a rule and a veto.
    let relaxed = eval_store::update_suite(
        &fx.pool,
        org,
        suite.id,
        &SuiteChanges { blocking: Some(false), judge_model_id: Some(None), ..Default::default() },
    )
    .await
    .expect("an advisory suite may have no judge");
    assert!(relaxed.judge_model_id.is_none());
    assert!(!relaxed.blocking);

    fx.dispose().await;
}

#[tokio::test]
async fn the_judge_must_not_be_the_model_under_test() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let model = Uuid::from_u128(1);

    let mut suite_spec = fx.suite_for("self-graded", model);
    suite_spec.judge_model_id = Some(model);
    let error = eval_store::create_suite(&fx.pool, org, &suite_spec)
        .await
        .expect_err("a judge that is the model under test must be refused");
    assert!(error.to_string().contains("judge_model_id"), "{error}");

    // The column refuses it too, for a write that skips the store.
    let direct = sqlx::query(
        "insert into ai_eval_suites (organization_id, key, name, target, model_id, judge_model_id) \
         values ($1, 'direct-self-grade', 'Direct', 'model', $2, $2)",
    )
    .bind(org)
    .bind(model)
    .execute(&fx.pool)
    .await
    .expect_err("the column must refuse the pair as well");
    assert!(
        direct.to_string().contains("ai_eval_suites_judge_differs"),
        "the refusal must name the constraint, got: {direct}"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn the_judge_prompt_version_moves_only_when_the_prompt_moves() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let mut suite_spec = fx.suite_for("judged", model);
    suite_spec.judge_model_id = Some(judge);
    suite_spec.judge_prompt = Some("Is the answer correct? Say pass or fail.".to_owned());
    let created = eval_store::create_suite(&fx.pool, org, &suite_spec)
        .await
        .expect("the suite must be created");
    assert_eq!(created.judge_prompt_version, 1, "a new suite starts at revision 1");

    // An edit that does not touch the prompt must not move the version, or two runs of the
    // same prompt would look like different configurations.
    let renamed = eval_store::update_suite(
        &fx.pool,
        org,
        created.id,
        &SuiteChanges { name: Some("Judged (renamed)".to_owned()), ..Default::default() },
    )
    .await
    .expect("a rename must succeed");
    assert_eq!(renamed.judge_prompt_version, 1, "a rename is not a prompt change");

    let reprompted = eval_store::update_suite(
        &fx.pool,
        org,
        created.id,
        &SuiteChanges {
            judge_prompt: Some(Some("Is the answer correct AND complete?".to_owned())),
            ..Default::default()
        },
    )
    .await
    .expect("a new prompt must be accepted");
    assert_eq!(reprompted.judge_prompt_version, 2, "a new prompt is a new revision");

    fx.dispose().await;
}

#[tokio::test]
async fn a_case_edit_is_validated_on_the_merged_row_not_on_the_patch() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let suite = eval_store::create_suite(&fx.pool, org, &fx.suite_for("merged", model))
        .await
        .expect("the suite must be created");
    let case: CaseRow = eval_store::create_case(
        &fx.pool,
        org,
        suite.id,
        &fx.case("only-property", serde_json::json!({ "exact": "yes" })),
    )
    .await
    .expect("the case must be created");

    // The patch is well-formed on its own — a valid document with a valid property. It still
    // has to be refused, because the merged case would assert nothing.
    let error = eval_store::update_case(
        &fx.pool,
        org,
        case.id,
        &CaseChanges { expected: Some(serde_json::json!({})), ..Default::default() },
    )
    .await
    .expect_err("a patch that empties the expectations must be refused");
    assert!(error.to_string().contains("at least one"), "{error}");

    // A patch that leaves a property behind is fine.
    let widened = eval_store::update_case(
        &fx.pool,
        org,
        case.id,
        &CaseChanges {
            expected: Some(serde_json::json!({ "exact": "yes", "contains": ["y"] })),
            ..Default::default()
        },
    )
    .await
    .expect("adding a property must be accepted");
    assert_eq!(
        eval_case::expectation_from(&widened.expected)
            .expect("the stored document must parse")
            .property_names(),
        vec!["exact", "contains"],
        "the stored document is what the scorer reads"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn coverage_is_a_count_per_tag_and_does_not_depend_on_the_page_size() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let suite = eval_store::create_suite(&fx.pool, org, &fx.suite_for("coverage", model))
        .await
        .expect("the suite must be created");

    for (name, tags) in [
        ("a", vec!["smoke".to_owned()]),
        ("b", vec!["smoke".to_owned(), "greeting".to_owned()]),
        ("c", vec!["greeting".to_owned()]),
    ] {
        eval_store::create_case(
            &fx.pool,
            org,
            suite.id,
            &NewCase { tags, ..fx.case(name, serde_json::json!({ "contains": ["x"] })) },
        )
        .await
        .expect("the case must be created");
    }

    let coverage = eval_store::tag_coverage(&fx.pool, org)
        .await
        .expect("the coverage query must succeed");
    // A count per tag, over every case in the tenant — not over the cases the list happened to
    // page in, which is how a coverage number silently changes with the page size.
    assert_eq!(
        coverage,
        vec![("greeting".to_owned(), 2), ("smoke".to_owned(), 2)],
        "coverage is a per-tag count: {coverage:?}"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn deleting_a_suite_takes_its_cases_with_it() {
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let suite = eval_store::create_suite(&fx.pool, org, &fx.suite_for("doomed", model))
        .await
        .expect("the suite must be created");
    eval_store::create_case(
        &fx.pool,
        org,
        suite.id,
        &fx.case("child", serde_json::json!({ "contains": ["x"] })),
    )
    .await
    .expect("the case must be created");

    eval_store::delete_suite(&fx.pool, org, suite.id)
        .await
        .expect("the suite must be deleted");

    let left: (i64,) = sqlx::query_as("select count(*) from ai_eval_cases where suite_id = $1")
        .bind(suite.id)
        .fetch_one(&fx.pool)
        .await
        .expect("the count must run");
    assert_eq!(left.0, 0, "a deleted suite leaves no orphan cases behind");

    // A second delete reports "not found" rather than succeeding quietly, so the screen cannot
    // claim a deletion that did not happen.
    let error = eval_store::delete_suite(&fx.pool, org, suite.id)
        .await
        .expect_err("a second delete must be refused");
    assert_eq!(error.code(), "eval_suite_not_found");
    assert!(
        matches!(error, AiHubError::EvalSuiteNotFound(_)),
        "the error is the eval variant, not a generic one: {error}"
    );

    fx.dispose().await;
}

#[tokio::test]
async fn a_stored_case_document_is_what_the_scorer_actually_reads() {
    // The end-to-end shape of the slice: a case saved through the store, read back as the
    // scorer reads it, and scored. A store that stored a different document than the one the
    // editor validated would make every validation above theatre.
    let fx = evals!();
    let org = fx.organization().await;
    let (model, judge) = fx.models().await;
    let suite = eval_store::create_suite(&fx.pool, org, &fx.suite_for("round-trip", model))
        .await
        .expect("the suite must be created");

    // `exact` is whole-string equality, so the passing output is the expected text exactly.
    // Pairing it with a sentence and expecting a pass is a fixture that disagrees with the
    // property it claims to test, which is worse than no fixture: it teaches that a `Fail` is
    // sometimes a pass.
    let expectations = serde_json::json!({
        "exact": "refund within 30 days",
        "contains": ["30 days"],
        "max_latency_ms": 5000,
    });
    eval_store::create_case(
        &fx.pool,
        org,
        suite.id,
        &fx.case("policy", expectations.clone()),
    )
    .await
    .expect("the case must be created");

    let stored = &eval_store::cases_for_run(&fx.pool, org, suite.id)
        .await
        .expect("the runner read must succeed")[0];
    assert_eq!(stored.expected, expectations, "the document round-trips unchanged");

    let parsed = eval_case::expectation_from(&stored.expected).expect("the document must parse");
    let verdict = eval_case::score_output(
        &parsed,
        "refund within 30 days",
        &eval_case::Observations { latency_ms: Some(1200), ..Default::default() },
        None,
        None,
    );
    assert_eq!(verdict.status, eval_case::CaseStatus::Pass, "{}", verdict.summary);

    // The same case against a real sentence fails on `exact` alone while `contains` and the
    // budget still hold, which is the shape a one-property regression actually takes.
    let sentence = eval_case::score_output(
        &parsed,
        "We refund within 30 days of the request.",
        &eval_case::Observations { latency_ms: Some(1200), ..Default::default() },
        None,
        None,
    );
    assert_eq!(sentence.status, eval_case::CaseStatus::Fail);
    assert_eq!(sentence.checks.len(), 3, "one verdict per named property");
    assert!(
        sentence.checks.iter().filter(|c| c.passed).count() == 2,
        "two of the three properties still hold: {}",
        sentence.summary
    );

    // And the failure is a failure of the property, with the property named.
    let verdict = eval_case::score_output(
        &parsed,
        "Refunds are handled by the finance team.",
        &eval_case::Observations { latency_ms: Some(1200), ..Default::default() },
        None,
        None,
    );
    assert_eq!(verdict.status, eval_case::CaseStatus::Fail);
    assert!(verdict.summary.contains("exact"), "{}", verdict.summary);

    fx.dispose().await;
}
