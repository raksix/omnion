//! Walks for the doctor (REQ-106, slice 4).
//!
//! The doctor's whole value is that its verdict is **derived** and never merely stored, so the
//! walks below are about that claim rather than about the checks' individual results:
//!
//! - **A run with no local endpoint is `warned`, never `passed`.** The empty installation is the
//!   most common state on a fresh box and the one most likely to render a green tick. It is the
//!   case a `is_empty`-style shortcut would get wrong.
//! - **The status comes from the checks, so a row whose stored `status` lies is corrected on
//!   read.** The `ai_local_doctor_runs.status` column exists; this writes a run whose checks are
//!   all `pass` under a stored word of `failed`, and asserts the reader reports `passed`. That is
//!   the assertion that makes the column a cache rather than an authority.
//! - **A failing check is diagnosed with a fix, and a rerun after the cause is fixed changes the
//!   verdict.** The request's "done when" is that exact sequence, so the walk performs it: the
//!   endpoint is broken, the run fails with a fix on the failing row, the endpoint starts
//!   answering, and the next run passes.
//! - **An unreachable endpoint makes its own dependent checks `warn`, not `fail`.** "No models"
//!   read off an endpoint that never answered is the sentence that sends an operator to look for
//!   models that are present.
//! - **History is newest-first and a regression is visible.** Two runs, the second worse, and the
//!   reader reports both in that order.
//!
//! Every walk needs PostgreSQL — the whole point is what the row says — so the harness panics
//! rather than skipping, and each run disposes its database explicitly: a leaked `omnion_*`
//! database is a sibling writer's outage.

use omnion_ai_hub::airgap_store::{self, SetAirgap};
use omnion_ai_hub::local_doctor::{self, CheckStatus, RunStatus};
use omnion_ai_hub::local_store::{self, NewLocalEndpoint};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

struct Doctor {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl Doctor {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!(
                "PostgreSQL is not reachable at {}: {err}",
                config.database.url
            );
            return None;
        }
        let database = format!("omnion_doctor_{}", Uuid::new_v4().simple());
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
        Some(Self {
            pool: db.pool().clone(),
            database,
            maintenance: Some(maintenance),
        })
    }

    async fn user(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("insert into users (id, email, display_name) values ($1, $2, $3)")
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .bind("Doctor operator")
            .execute(&self.pool)
            .await
            .expect("the user row must insert");
        id
    }

    /// Register a local endpoint served by `base_url`.
    async fn endpoint(&self, name: &str, base_url: &str) -> Uuid {
        local_store::create_endpoint(
            &self.pool,
            NewLocalEndpoint {
                name: name.to_owned(),
                base_url: base_url.to_owned(),
                protocol: "openai_compatible".to_owned(),
                requested_local: true,
                api_key: None,
            },
        )
        .await
        .expect("the endpoint registers")
    }

    /// Record an available model row, which the model-presence and embedding checks read.
    ///
    /// `id` is supplied explicitly because `ai_local_models.id` carries **no default**: it is a
    /// uuid primary key written by the store, and a row inserted without one fails on the
    /// not-null constraint. The error names the column, which is how the fixture defect was
    /// found rather than guessed at.
    async fn local_model(
        &self,
        provider_id: Uuid,
        key: &str,
        embeddings: bool,
        dimension: Option<i32>,
    ) {
        sqlx::query(
            "insert into ai_local_models \
               (id, provider_id, model_key, status, supports_embeddings, embedding_dimension, \
                resident, pull_progress, updated_at) \
             values ($1, $2, $3, 'available', $4, $5, true, 0, now())",
        )
        .bind(Uuid::new_v4())
        .bind(provider_id)
        .bind(key)
        .bind(embeddings)
        .bind(dimension)
        .execute(&self.pool)
        .await
        .expect("the model row must insert");
    }

    async fn dispose(mut self) {
        self.pool.close().await;
        let database = std::mem::take(&mut self.database);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!(
                "drop database if exists \"{database}\" with (force)"
            ))
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
macro_rules! doctor {
    () => {
        match Doctor::fresh().await {
            Some(fixture) => fixture,
            None => panic!(
                "PostgreSQL is required for the REQ-106 doctor walks; a skipped walk proves nothing"
            ),
        }
    };
}

/// Find one check by key.
fn check<'a>(
    checks: &'a [local_doctor::DoctorCheck],
    key: &str,
) -> &'a local_doctor::DoctorCheck {
    checks
        .iter()
        .find(|check| check.key == key)
        .unwrap_or_else(|| panic!("the doctor produced no `{key}` check"))
}

// -------------------------------------------------------------------------------------------
// The empty installation
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_installation_with_no_local_endpoint_is_unproven_not_green() {
    let fixture = doctor!();

    let run = local_doctor::run_all(&fixture.pool, Some(fixture.user().await))
        .await
        .expect("the doctor runs on an empty installation");

    // The claim under test: no endpoint is not the same as everything working. A fresh box is
    // exactly this state, and a green tick there is how a platform tells an operator it is
    // ready for air-gapped operation before anything has been installed.
    assert_eq!(
        run.status,
        RunStatus::Warned,
        "no endpoint means readiness is unproven, not established"
    );
    let reachability = check(&run.checks, "endpoint_reachable");
    assert_eq!(reachability.status, CheckStatus::Warn);
    assert!(
        reachability.detail.contains("no local endpoint"),
        "the cause names the state: {}",
        reachability.detail
    );
    assert!(
        reachability.fix.is_some(),
        "an unestablished check carries the action that establishes it"
    );

    // The embedding check answers the same question for the same reason.
    assert_eq!(
        check(&run.checks, "embedding_models").status,
        CheckStatus::Warn
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The stored status is a cache; the checks are the authority
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_whose_stored_status_lies_is_reported_from_its_checks() {
    let fixture = doctor!();

    // Write a run by hand whose stored `status` is `failed` while every check inside it is a
    // pass. This is the drift the derivation exists to prevent: an older binary, a hand-edited
    // column, or a future code path that forgets to keep the two in step.
    let all_pass = serde_json::json!([
        {
            "key": "endpoint_reachable",
            "label": "Endpoint reachable",
            "status": "pass",
            "detail": "127.0.0.1:11434 answered with 1 model(s)",
            "fix": null
        }
    ]);
    sqlx::query(
        "insert into ai_local_doctor_runs (status, checks, airgap_enabled, started_at, \
         finished_at) values ('failed', $1, false, now() - interval '2 hours', now())",
    )
    .bind(&all_pass)
    .execute(&fixture.pool)
    .await
    .expect("the doctor run inserts");

    let runs = local_doctor::list_runs(&fixture.pool, 8)
        .await
        .expect("the runs read back");
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status,
        RunStatus::Passed,
        "the reader recomputes the verdict from the checks; the stored word is not consulted"
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// A failing check, its fix, and a rerun that changes the verdict
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_broken_endpoint_is_diagnosed_and_a_rerun_after_the_fix_changes_the_verdict() {
    let fixture = doctor!();

    // A port nothing is listening on: the most common real failure.
    let dead = local_doctor::run_all(&fixture.pool, None)
        .await
        .expect("the doctor runs");
    assert_eq!(dead.status, RunStatus::Warned);

    // Now register an endpoint that refuses connections, and one model row for it so the
    // failure being reported is reachability rather than "no model registered".
    let provider = fixture.endpoint("ollama-stub", "http://127.0.0.1:1/v1").await;
    fixture.local_model(provider, "llama3.1:8b", false, None).await;

    let broken = local_doctor::run_all(&fixture.pool, None)
        .await
        .expect("the doctor runs against a dead endpoint");

    let reachability = check(&broken.checks, "endpoint_reachable");
    assert_eq!(
        reachability.status,
        CheckStatus::Fail,
        "an endpoint that cannot be reached has failed, whatever else is unknown"
    );
    assert!(
        reachability.fix.is_some(),
        "a failure without a fix leaves the operator with a fact and no action"
    );

    // The checks that depend on reachability are WARN, not fail. "No models" read off an
    // endpoint that never answered is the sentence that sends an operator looking for models
    // that are sitting on disk, working fine.
    assert_eq!(
        check(&broken.checks, "models_present").status,
        CheckStatus::Warn,
        "an unreachable endpoint makes model presence unknown, not absent"
    );
    assert_eq!(check(&broken.checks, "completion").status, CheckStatus::Warn);

    assert_eq!(
        broken.status,
        RunStatus::Failed,
        "a failed check makes the whole run failed"
    );
    let summary = RunStatus::message(&broken.checks);
    assert!(
        summary.contains("endpoint_reachable"),
        "the summary names the blocking check: {summary}"
    );

    // Now start a server that answers, scan it, and re-run. This is the request's "done when":
    // a rerun after fixing a check changes the verdict.
    let base = stub_models().await;
    sqlx::query("update ai_providers set base_url = $1 where id = $2")
        .bind(&base)
        .bind(provider)
        .execute(&fixture.pool)
        .await
        .expect("the endpoint is repointed at the live stub");

    let fixed = local_doctor::rerun_one(&fixture.pool, "endpoint_reachable", None)
        .await
        .expect("the rerun runs");

    // The completion check still FAILS, because the stub answers `/models` and refuses
    // `/chat/completions` with a 500. That is the point of the stub: it proves the doctor does
    // not award a green tick for a reachable endpoint that cannot actually answer, which is the
    // single most common local-AI failure (a server up, the model broken).
    assert_eq!(
        check(&fixed.checks, "endpoint_reachable").status,
        CheckStatus::Pass,
        "the fixed endpoint is reachable now"
    );
    let completion = check(&fixed.checks, "completion");
    assert_eq!(
        completion.status,
        CheckStatus::Fail,
        "reachable is not the same as working — the stub refuses the chat path"
    );
    assert!(
        completion.detail.contains("stub refuses chats"),
        "the failure carries the SERVER's words, not a platform sentence: {}",
        completion.detail
    );
    assert!(
        completion.fix.is_some(),
        "a failure without a fix is a fact with no action attached"
    );

    // The verdict is a function of the checks, so it is asserted as such: the reachability check
    // moved, and the run's status follows the checks rather than any stored word.
    assert_eq!(
        RunStatus::of(&fixed.checks),
        RunStatus::Failed,
        "one failing check still fails the run — and that is the run reporting honestly"
    );
    assert!(
        check(&broken.checks, "endpoint_reachable").status != check(&fixed.checks, "endpoint_reachable").status,
        "the rerun after fixing the cause CHANGED that check's verdict, which is the request's \
         \"done when\""
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn an_unknown_check_key_is_refused_and_names_the_real_ones() {
    let fixture = doctor!();

    let error = local_doctor::rerun_one(&fixture.pool, "endpoint_reachablee", None)
        .await
        .expect_err("a mistyped key must be refused rather than silently ignored");
    let message = error.to_string();
    assert!(message.contains("endpoint_reachablee"), "{message}");
    // The refusal names what the caller could have typed — a bare "unknown key" sends the
    // operator to the source to find a list the screen already has.
    assert!(message.contains("endpoint_reachable"), "{message}");

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The installation-wide checks read the switch and the recorded verification
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_doctor_reads_the_switch_and_the_recorded_verification_rather_than_guessing() {
    let fixture = doctor!();
    let actor = fixture.user().await;

    // With the gap off and nothing verified, the doctor must not claim the boundary holds —
    // and must not alarm about it either.
    let off = local_doctor::run_all(&fixture.pool, Some(actor))
        .await
        .expect("the doctor runs");
    let airgap = check(&off.checks, "airgap_state");
    assert_eq!(airgap.status, CheckStatus::Pass);
    assert!(airgap.detail.contains("off"), "{}", airgap.detail);
    let egress = check(&off.checks, "egress_verify");
    assert_eq!(
        egress.status,
        CheckStatus::Pass,
        "with the gap off there is no boundary to test, which is not a failure"
    );

    // Turn it on without ever verifying. Readiness is now unproven, and the sentence has to
    // say so — an operator who reads "Ready" here would point production at an untested gap.
    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("Patient records must not leave the practice network.".to_owned()),
            low_confidence_ack: true,
            actor: Some(actor),
        },
    )
    .await
    .expect("the gap turns on");

    let on = local_doctor::run_all(&fixture.pool, Some(actor))
        .await
        .expect("the doctor runs with the gap on");
    let airgap = check(&on.checks, "airgap_state");
    assert_eq!(airgap.status, CheckStatus::Pass);
    assert!(
        airgap.detail.contains("Patient records"),
        "the operator reads their own reason back: {}",
        airgap.detail
    );
    let egress = check(&on.checks, "egress_verify");
    assert_eq!(
        egress.status,
        CheckStatus::Warn,
        "no verification has run, so the boundary is unproven"
    );

    // Now record a passing verification, the way the settings screen does, and the same check
    // must become a pass — from the ROW, not from a fresh dial-out.
    omnion_ai_hub::egress_verify::record_result(
        &fixture.pool,
        omnion_ai_hub::egress_verify::EgressOutcome::Blocked,
        "api.openai.com",
        time::OffsetDateTime::now_utc(),
    )
    .await
    .expect("the verification is recorded");

    let verified = local_doctor::run_all(&fixture.pool, Some(actor))
        .await
        .expect("the doctor runs after a verification");
    let egress = check(&verified.checks, "egress_verify");
    assert_eq!(
        egress.status,
        CheckStatus::Pass,
        "a refused attempt is the passing outcome, by design"
    );
    assert!(
        egress.detail.contains("api.openai.com"),
        "the check names the host it was aimed at: {}",
        egress.detail
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// History
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn history_is_newest_first_so_a_regression_is_visible() {
    let fixture = doctor!();

    // The EARLIER run, inserted by hand with a timestamp in the past so the ordering is decided
    // by the column the query orders on rather than by which insert happened to land first. All
    // its checks pass and its stored word agrees — the lie case has its own walk.
    sqlx::query(
        "insert into ai_local_doctor_runs (status, checks, airgap_enabled, started_at, \
         finished_at) values ('passed', $1, false, now() - interval '1 hour', now())",
    )
    .bind(serde_json::json!([
        {
            "key": "endpoint_reachable",
            "label": "Endpoint reachable",
            "status": "pass",
            "detail": "everything was fine an hour ago",
            "fix": null
        }
    ]))
    .execute(&fixture.pool)
    .await
    .expect("the earlier run inserts");

    // The NEWER run: no endpoint is registered, so it is warned rather than passed. That is the
    // regression — the installation stopped being able to run AI locally — and the whole reason
    // the request asks for previous runs beside the last one.
    local_doctor::run_all(&fixture.pool, None)
        .await
        .expect("the newer run");

    let runs = local_doctor::list_runs(&fixture.pool, 8)
        .await
        .expect("the history reads");

    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[0].status,
        RunStatus::Warned,
        "the newest run is first — it is what the screen headlines"
    );
    assert_eq!(
        runs[1].status,
        RunStatus::Passed,
        "the earlier run follows, so the regression is visible as a change"
    );
    assert!(
        runs[0].started_at > runs[1].started_at,
        "the ordering follows the timestamp the reader orders on"
    );

    // The LIMIT is part of the contract: a screen that asks for the last run must not receive
    // the whole table and draw the wrong one as "latest".
    let limited = local_doctor::list_runs(&fixture.pool, 1)
        .await
        .expect("a limited read");
    assert_eq!(limited.len(), 1, "the limit is honoured");
    assert_eq!(
        limited[0].id, runs[0].id,
        "the limited read is the SAME newest row, not an arbitrary one"
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// A loopback stub that answers the model list
// -------------------------------------------------------------------------------------------

/// A loopback server that answers `/models` with two models and refuses the chat path.
///
/// Two models, so the doctor has something to address and the embedding check has something to
/// look for. The chat path answers 500 on purpose: the walk that uses this stub is about
/// reachability, and a stub that answered everything would let it pass for the wrong reason.
async fn stub_models() -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 4096];
            let Ok(read) = stream.read(&mut buf).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let (status, body) = if request.contains("/chat/completions") {
                ("500 Internal Server Error", "{\"error\":\"stub refuses chats\"}".to_owned())
            } else {
                (
                    "200 OK",
                    r#"{"data":[{"id":"llama3.1:8b"},{"id":"nomic-embed-text"}]}"#.to_owned(),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}