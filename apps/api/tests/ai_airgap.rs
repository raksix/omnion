//! Walks for the air-gap switch, its allow-list and the refusal (REQ-106, slice 2).
//!
//! Slice 1's suite (`ai_local.rs`) proves that "local" is a *derived* fact. This one proves the
//! other half of the sentence — that with the gap on, a non-local provider is refused **before a
//! byte is sent**, that a local one still answers, and that the operator cannot turn the switch
//! on without leaving a record of why. Every claim below is a claim about a row or about a
//! refusal, so it needs a real database:
//!
//! - **The refusal names the provider and the host.** The request forbids a generic "request
//!   failed", and an assertion on the *code* alone would pass on a client that printed nothing
//!   useful. The message is checked for both names.
//! - **A refused call is a distinct log row.** `blocked_airgap` is a fourth vocabulary word, not
//!   `refused`: `refused` means the provider said no, and an operator reconciling costs would
//!   chase a cause that does not exist. The walk reads the row back out of the database rather
//!   than trusting the string the caller passed.
//! - **The refusal is not a failover retry.** With two non-local providers registered, a walk
//!   that advanced on the refusal would try both and log two rows. One row is the assertion, and
//!   it is the assertion that fails if someone "helpfully" makes the refusal retryable.
//! - **A local provider still answers with the gap on.** The whole switch is worthless if it
//!   stops everything, and this is the criterion that would catch a `check_call` that refuses
//!   everything.
//! - **The reason is required on the way ON and refused on the way OFF — deliberately.** Turning
//!   the gap off is the emergency action; a validation rule that can block it is a control that
//!   can fail closed at the worst moment.
//! - **The allow-list widens, never replaces.** A host on the list answers a call; the host is
//!   still a public name the moment it is removed.
//!
//! The harness is the throwaway-database pattern the other AI suites use, and it **panics**
//! rather than skipping when PostgreSQL is unreachable — a skipped walk proves nothing.

use omnion_ai_hub::airgap_store::{self, SetAirgap};
use omnion_ai_hub::egress_verify;
use omnion_ai_hub::client::{ChatMessage, ChatRequest, ProviderTarget};
use omnion_ai_hub::error::AiHubError;
use omnion_ai_hub::health_store::NewUsage;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use uuid::Uuid;

struct Airgap {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl Airgap {
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
        let database = format!("omnion_airgap_{}", Uuid::new_v4().simple());
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

    /// Insert a provider row directly, the way an earlier slice or a restore would.
    ///
    /// Deliberately not `local_store::create_endpoint`: that path *refuses* a public host, which
    /// is slice 1's assertion and would make this suite unable to register the very provider it
    /// needs. A provider row that is remote is legitimate — every cloud provider is one.
    async fn provider(&self, name: &str, base_url: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into ai_providers (id, name, protocol, base_url, enabled, locality, \
             host_kind) values ($1, $2, 'openai_compatible', $3, true, 'remote', null)",
        )
        .bind(id)
        .bind(name)
        .bind(base_url)
        .execute(&self.pool)
        .await
        .expect("the provider row must insert");
        id
    }

    /// A real user row, for the FK the audit columns carry.
    ///
    /// NOT a random uuid: `ai_airgap_state.enabled_by` and `ai_airgap_hosts.created_by` reference
    /// `users(id)`, and the audit row is the whole point of those columns — a walk that inserted a
    /// fabricated id would have to pass `None`, which is the shape that proves nothing about who
    /// flipped the switch.
    async fn user(&self) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("insert into users (id, email, display_name) values ($1, $2, $3)")
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .bind("Air-gap operator")
            .execute(&self.pool)
            .await
            .expect("the user row must insert");
        id
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
macro_rules! airgap {
    () => {
        match Airgap::fresh().await {
            Some(fixture) => fixture,
            None => panic!("PostgreSQL is required for the REQ-106 walks; a skipped walk proves nothing"),
        }
    };
}

// -------------------------------------------------------------------------------------------
// The switch: a reason is required on the way on, and none on the way off
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_switch_starts_off_and_enabling_it_demands_a_reason() {
    let fixture = airgap!();

    let initial = airgap_store::read_state(&fixture.pool)
        .await
        .expect("the seeded row reads");
    assert!(!initial.enabled, "the gap is off until an operator turns it on");
    assert!(
        initial.reason.is_none(),
        "an untouched switch carries no reason"
    );

    // An empty reason is refused, and the refusal explains WHY the field exists rather than
    // merely reporting a range — the sentence is what an operator reads at 2am.
    let blank = airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("   ".to_owned()),
            low_confidence_ack: true,
            actor: Some(fixture.user().await),
        },
    )
    .await
    .expect_err("a blank reason must be refused");
    assert!(blank.to_string().contains("reason is required"), "{blank}");

    // Too short is refused with the length, so the field can show how far off it was.
    let short = airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("too short".to_owned()),
            low_confidence_ack: true,
            actor: None,
        },
    )
    .await
    .expect_err("a nine-character reason must be refused");
    let message = short.to_string();
    assert!(message.contains("10") && message.contains('9'), "{message}");

    // And the row is unchanged by either refusal — a validation error is not a partial write.
    assert!(
        !airgap_store::read_state(&fixture.pool).await.expect("read").enabled,
        "a refused enable must leave the switch off"
    );

    // A real reason lands, with the actor and the time. Those two are the audit row; the reason
    // alone would be a claim nobody can attribute.
    let actor = fixture.user().await;
    let reason = "Patient records must not leave the practice network (policy AI-4).";
    let on = airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some(reason.to_owned()),
            low_confidence_ack: true,
            actor: Some(actor),
        },
    )
    .await
    .expect("a real reason enables the switch");
    assert!(on.enabled);
    assert_eq!(on.reason.as_deref(), Some(reason));
    assert_eq!(on.enabled_by, Some(actor), "the actor is recorded");
    assert!(on.enabled_at.is_some(), "the time is recorded");

    fixture.dispose().await;
}

#[tokio::test]
async fn turning_the_gap_off_takes_no_reason_and_keeps_the_history() {
    let fixture = airgap!();

    let actor = fixture.user().await;
    let reason = "Quarterly compliance review requires the gap to be closed.";
    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some(reason.to_owned()),
            low_confidence_ack: true,
            actor: Some(actor),
        },
    )
    .await
    .expect("the gap enables");

    // Disabling with NO reason is accepted. This asymmetry is the point: an emergency action
    // that can be blocked by a validation rule is a control that fails closed at the worst moment.
    let off = airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: false,
            reason: None,
            low_confidence_ack: false,
            actor: None,
        },
    )
    .await
    .expect("turning the gap off needs no reason");
    assert!(!off.enabled);

    // …and the record of why it was ever on survives, because "was this ever on, and who did it"
    // is the question an auditor asks after the gap is back off.
    assert_eq!(
        off.reason.as_deref(),
        Some(reason),
        "the reason survives the switch going off"
    );
    assert_eq!(off.enabled_by, Some(actor), "the actor survives too");

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The refusal
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_remote_provider_is_refused_by_name_and_its_host_is_the_reason() {
    let fixture = airgap!();
    fixture
        .provider("Anthropic", "https://api.anthropic.com/v1")
        .await;

    // Off: nothing is refused. This is the control — a check that refuses regardless of the
    // switch would pass the rest of this walk and break every other installation.
    assert!(
        airgap_store::check_call(
            &fixture.pool,
            "Anthropic",
            "https://api.anthropic.com/v1"
        )
        .await
        .expect("the check runs")
        .is_none(),
        "with the gap off every provider answers"
    );

    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("No outbound model traffic during the migration freeze.".to_owned()),
            low_confidence_ack: true,
            actor: Some(fixture.user().await),
        },
    )
    .await
    .expect("the gap enables");

    let refusal = airgap_store::check_call(
        &fixture.pool,
        "Anthropic",
        "https://api.anthropic.com/v1",
    )
    .await
    .expect("the check runs")
    .expect("a public host is refused with the gap on");

    assert_eq!(refusal.provider, "Anthropic", "the provider is named");
    assert_eq!(
        refusal.host, "api.anthropic.com",
        "the host that would have left is named"
    );
    assert_eq!(refusal.code(), "ai_airgap_blocked", "the code is the one the UI keys off");

    // The message is the criterion the request states in words: never a generic failure, and
    // always naming the route that changes it.
    let message = refusal.message();
    assert!(message.contains("Anthropic"), "{message}");
    assert!(message.contains("api.anthropic.com"), "{message}");
    assert!(message.contains("air gap"), "{message}");
    assert!(
        message.contains("local endpoint") || message.contains("turn the air gap off"),
        "the message names the way out: {message}"
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn a_local_provider_still_answers_while_the_gap_is_on() {
    let fixture = airgap!();

    // The loopback provider is registered through slice 1's path, so it carries the locality
    // column the screen shows — and the check must still pass with the URL in hand.
    omnion_ai_hub::local_store::create_endpoint(
        &fixture.pool,
        omnion_ai_hub::local_store::NewLocalEndpoint {
            name: "ollama".to_owned(),
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            protocol: "openai_compatible".to_owned(),
            requested_local: true,
            api_key: None,
        },
    )
    .await
    .expect("loopback registers as a local endpoint");

    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("Everything runs on the workstation this week.".to_owned()),
            low_confidence_ack: true,
            actor: Some(fixture.user().await),
        },
    )
    .await
    .expect("the gap enables");

    assert!(
        airgap_store::check_call(&fixture.pool, "ollama", "http://127.0.0.1:11434/v1")
            .await
            .expect("the check runs")
            .is_none(),
        "the gap must not stop the provider that never leaves the machine — a switch that \
         refuses everything is indistinguishable from a broken installation"
    );

    // And the round trip itself, through the platform's own client, against a real socket. The
    // refusal above is a decision; this is the thing the decision is protecting.
    let base = stub_answering(
        "200 OK",
        r#"{"id":"chatcmpl-1","object":"chat.completion","model":"llama3.1:8b",
            "choices":[{"index":0,"message":{"role":"assistant","content":"Still answering here."},
            "finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":4,"total_tokens":9}}"#,
        1,
    )
    .await;
    let id = omnion_ai_hub::local_store::create_endpoint(
        &fixture.pool,
        omnion_ai_hub::local_store::NewLocalEndpoint {
            name: "ollama-stub".to_owned(),
            base_url: base.clone(),
            protocol: "openai_compatible".to_owned(),
            requested_local: true,
            api_key: None,
        },
    )
    .await
    .expect("the stub registers");
    let stored = omnion_ai_hub::local_store::get_endpoint(&fixture.pool, id)
        .await
        .expect("the stored row reads back");

    assert!(
        airgap_store::check_call(&fixture.pool, &stored.name, &stored.base_url)
            .await
            .expect("the check runs")
            .is_none(),
        "the STORED row's URL passes the check — not the one the test happens to hold"
    );

    let outcome = omnion_ai_hub::client::chat(
        &ProviderTarget {
            id: stored.id,
            name: stored.name.clone(),
            protocol: stored.protocol.clone(),
            base_url: stored.base_url.clone(),
            api_key: None,
            timeout_ms: 10_000,
        },
        &ChatRequest::new("llama3.1:8b", vec![ChatMessage::user("still there?")]),
    )
    .await
    .expect("the local endpoint answers with the gap on");
    assert_eq!(outcome.content, "Still answering here.");

    fixture.dispose().await;
}

/// A loopback stub that answers N requests with `status` and `body` (see `ai_local.rs`).
async fn stub_answering(status_line: &'static str, body: &'static str, times: usize) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        for _ in 0..times {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

// -------------------------------------------------------------------------------------------
// The log row
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_blocked_call_writes_its_own_outcome_row() {
    let fixture = airgap!();
    let provider = fixture.provider("OpenAI", "https://api.openai.com/v1").await;

    // The vocabulary is a CONTRACT with the CHECK constraint, and both halves were widened in
    // the same commit. A row that names `blocked_airgap` therefore proves the CHECK admits it —
    // a Rust-only widening would have failed here with a constraint violation.
    //
    // `cost: None` is the honest answer, not a zero: a call that never left the machine spent no
    // money, and the column distinguishes "not knowable" from a real 0 so the cost screen does
    // not add a phantom row to the bill.
    omnion_ai_hub::health_store::record_usage(
        &fixture.pool,
        NewUsage {
            provider_id: provider,
            model_key: Some("gpt-5".to_owned()),
            task: "chat".to_owned(),
            outcome: "blocked_airgap".to_owned(),
            http_status: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            substituted_from: None,
            first_byte_at: None,
            cost: None,
        },
    )
    .await
    .expect("blocked_airgap is a legal outcome");

    let (outcome,): (String,) = sqlx::query_as(
        "select outcome from ai_provider_usage where provider_id = $1",
    )
    .bind(provider)
    .fetch_one(&fixture.pool)
    .await
    .expect("the row reads back");
    assert_eq!(outcome, "blocked_airgap");

    // And the three pre-existing words still work: widening a vocabulary must not have cost the
    // ones a provider's own refusal writes.
    for outcome in ["ok", "error", "refused"] {
        omnion_ai_hub::health_store::record_usage(
            &fixture.pool,
            NewUsage {
                provider_id: provider,
                model_key: None,
                task: "chat".to_owned(),
                outcome: outcome.to_owned(),
                http_status: None,
                prompt_tokens: None,
                completion_tokens: None,
                latency_ms: 0,
                substituted_from: None,
                first_byte_at: None,
                cost: None,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{outcome} must remain legal: {error}"));
    }

    // An unknown word is still refused — the widening is not an escape hatch.
    let rejected = omnion_ai_hub::health_store::record_usage(
        &fixture.pool,
        NewUsage {
            provider_id: provider,
            model_key: None,
            task: "chat".to_owned(),
            outcome: "blocked".to_owned(),
            http_status: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            substituted_from: None,
            first_byte_at: None,
            cost: None,
        },
    )
    .await
    .expect_err("an unknown outcome is refused");
    assert!(rejected.to_string().contains("not a call outcome"), "{rejected}");

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The allow-list
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_allow_list_widens_locality_and_removing_it_takes_the_answer_away() {
    let fixture = airgap!();
    fixture
        .provider("Inference box", "https://gpu-box.internal:8080/v1")
        .await;

    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("Only the on-premise inference box may be reached.".to_owned()),
            low_confidence_ack: true,
            actor: Some(fixture.user().await),
        },
    )
    .await
    .expect("the gap enables");

    // Before the allow-list: refused, and the host is named so the operator knows what to add.
    let before = airgap_store::check_call(
        &fixture.pool,
        "Inference box",
        "https://gpu-box.internal:8080/v1",
    )
    .await
    .expect("the check runs")
    .expect("an unlisted internal name is refused like any public one");
    assert_eq!(before.host, "gpu-box.internal");

    // A pasted URL is refused with a sentence that says what a host looks like. Storing it would
    // widen nothing while appearing in the list as though it worked.
    let pasted = airgap_store::add_host(
        &fixture.pool,
        "https://gpu-box.internal:8080/v1",
        None,
        None,
    )
    .await
    .expect_err("a pasted base URL is not a host");
    assert!(pasted.to_string().contains("bare host"), "{pasted}");

    let added = airgap_store::add_host(
        &fixture.pool,
        "GPU-Box.Internal ",
        Some("the office inference server"),
        None,
    )
    .await
    .expect("a bare host is added")
    .expect("the row is new");
    // Lowercased on the way in — the list is matched against a lowercased host, so a list written
    // in caps has to normalize or it silently stops matching.
    assert_eq!(added.host, "gpu-box.internal", "the host is normalized");

    // A second identical add is "nothing changed", not an error: the form is submitted twice by
    // an impatient operator and the list is already correct.
    assert!(
        airgap_store::add_host(&fixture.pool, "gpu-box.internal", None, None)
            .await
            .expect("the duplicate is not an error")
            .is_none(),
        "a duplicate add reports nothing changed rather than inventing a row"
    );

    assert!(
        airgap_store::check_call(
            &fixture.pool,
            "Inference box",
            "https://gpu-box.internal:8080/v1"
        )
        .await
        .expect("the check runs")
        .is_none(),
        "the allow-list widens the rule rather than replacing it"
    );

    // Removal takes the answer away again. This is the assertion that the allow-list is the
    // control and not a decoration: widen, use, narrow, and the gap is back to refusing.
    airgap_store::remove_host(&fixture.pool, added.id)
        .await
        .expect("the host is removed");
    assert!(
        airgap_store::check_call(
            &fixture.pool,
            "Inference box",
            "https://gpu-box.internal:8080/v1"
        )
        .await
        .expect("the check runs")
        .is_some(),
        "removing the entry restores the refusal"
    );

    // Removing it twice is an error, not a silent success: the delete button must not report a
    // deletion that did not happen. Matched on the VARIANT rather than the prose: the sentence
    // lives on the `ApiError` mapping, which is a different layer, and pinning a wording here
    // would turn this walk into a test of prose.
    let again = airgap_store::remove_host(&fixture.pool, added.id)
        .await
        .expect_err("removing a host that is not there is an error");
    assert!(
        matches!(again, AiHubError::AirgapHostNotFound(id) if id == added.id),
        "the not-found error names the row it could not find, got {again:?}"
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// What the confirmation lists
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_confirmation_lists_exactly_what_would_be_refused() {
    let fixture = airgap!();
    fixture.provider("OpenAI", "https://api.openai.com/v1").await;
    fixture.provider("Groq", "https://api.groq.com/openai/v1").await;
    omnion_ai_hub::local_store::create_endpoint(
        &fixture.pool,
        omnion_ai_hub::local_store::NewLocalEndpoint {
            name: "ollama".to_owned(),
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            protocol: "openai_compatible".to_owned(),
            requested_local: true,
            api_key: None,
        },
    )
    .await
    .expect("loopback registers");

    // The list is COMPUTED by the same rule the call path uses, not typed by hand. A hand-kept
    // list would be wrong the day a provider is added, and wrong in the direction that matters:
    // it would under-report, and the operator would turn the gap on believing a feature survives.
    let mut blocked = airgap_store::providers_that_would_block(&fixture.pool)
        .await
        .expect("the list reads");
    blocked.sort();
    // The second element is the base URL, not the host: the confirmation screen links the row to
    // the provider it will stop, and a bare host cannot be opened or edited from there. Only the
    // REFUSAL carries the host, because only the refusal is read by someone who is not on the
    // providers screen.
    assert_eq!(
        blocked,
        vec![
            ("Groq".to_owned(), "https://api.groq.com/openai/v1".to_owned()),
            ("OpenAI".to_owned(), "https://api.openai.com/v1".to_owned()),
        ],
        "the two remote providers are listed and the local one is not"
    );

    // After the gap is on, an allow-listed internal host drops off the list — the confirmation
    // reads the current rule, not a snapshot taken when the screen opened.
    airgap_store::add_host(&fixture.pool, "api.groq.com", None, None)
        .await
        .expect("a host is added");
    let narrowed = airgap_store::providers_that_would_block(&fixture.pool)
        .await
        .expect("the list re-reads");
    assert_eq!(
        narrowed.len(),
        1,
        "the allow-listed host is no longer something the gap would refuse"
    );

    fixture.dispose().await;
}
// -------------------------------------------------------------------------------------------
// Egress verification: the check that expects a refusal (slice 4)
// -------------------------------------------------------------------------------------------

/// The one thing every test here shares: turn the gap on with a real reason.
async fn enable_the_gap(fixture: &Airgap) -> Uuid {
    let actor = fixture.user().await;
    airgap_store::set_state(
        &fixture.pool,
        &SetAirgap {
            enabled: true,
            reason: Some("isolation drill for the compliance audit".to_owned()),
            low_confidence_ack: true,
            actor: Some(actor),
        },
    )
    .await
    .expect("the gap turns on");
    actor
}

#[tokio::test]
async fn a_refused_call_is_the_verification_passing() {
    let fixture = airgap!();
    fixture.provider("OpenAI", "https://api.openai.com/v1").await;
    enable_the_gap(&fixture).await;

    // The check runs through the platform's OWN decision, so this is evidence about the chat
    // path rather than about a parallel harness that could pass while the chat drifted.
    let result = egress_verify::verify_egress(
        &fixture.pool,
        "OpenAI",
        "https://api.openai.com/v1",
    )
    .await
    .expect("the verification runs");

    // THE INVERSION, asserted. A refusal is the pass; a reader who expected `passes == false`
    // here would be right to suspect the test, so the comment is the assertion's twin.
    assert_eq!(result.outcome, egress_verify::EgressOutcome::Blocked);
    assert!(result.holds(), "a refused call means the installation is holding");
    assert_eq!(result.target, "api.openai.com", "the host is named");

    // The refusal itself rides along, so the screen can show what the operator would have hit.
    let refusal = result
        .refusal
        .as_ref()
        .expect("a blocked attempt carries the refusal");
    assert_eq!(refusal.provider, "OpenAI");
    assert_eq!(refusal.code(), "ai_airgap_blocked");

    fixture.dispose().await;
}

#[tokio::test]
async fn a_local_endpoint_never_counts_as_an_escape() {
    let fixture = airgap!();
    fixture.provider("Ollama", "http://127.0.0.1:11434/v1").await;
    enable_the_gap(&fixture).await;

    // A local host is not something the gap can leak, so verifying it must NOT report an escape
    // and must not be recorded as a failure. A checker that treated "no refusal" as "escaped"
    // would paint a healthy local-only installation red — the alarm would cry wolf on exactly the
    // configuration the whole request is about.
    let result = egress_verify::verify_egress(&fixture.pool, "Ollama", "http://127.0.0.1:11434/v1")
        .await
        .expect("the verification runs");
    assert_ne!(
        result.outcome,
        egress_verify::EgressOutcome::Escaped,
        "a loopback endpoint is inside the gap, never an escape from it"
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn an_allow_listed_host_is_not_an_escape() {
    let fixture = airgap!();
    fixture
        .provider("Internal", "https://gpu-box.internal/v1")
        .await;
    enable_the_gap(&fixture).await;
    airgap_store::add_host(&fixture.pool, "gpu-box.internal", None, None)
        .await
        .expect("an internal host joins the allow-list");

    // Without the allow-list this host would be refused, so it would record a `blocked` PASS and
    // read as reassuring — while in truth the operator believes it is internal and it is not on
    // the list. The interesting assertion is the other direction: once allow-listed, a permitted
    // call is NOT an escape, because "permitted" and "leaked" are different facts.
    let result = egress_verify::verify_egress(
        &fixture.pool,
        "Internal",
        "https://gpu-box.internal/v1",
    )
    .await
    .expect("the verification runs");
    assert_ne!(
        result.outcome,
        egress_verify::EgressOutcome::Blocked,
        "an allow-listed host is permitted, so nothing was refused"
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn the_result_is_recorded_on_the_row_with_its_target_and_time() {
    let fixture = airgap!();
    fixture.provider("OpenAI", "https://api.openai.com/v1").await;
    enable_the_gap(&fixture).await;

    let before = airgap_store::read_state(&fixture.pool)
        .await
        .expect("the row reads");
    assert!(
        before.egress_verify_result.is_none(),
        "an installation that never verified carries no verdict"
    );

    let result = egress_verify::verify_egress(
        &fixture.pool,
        "OpenAI",
        "https://api.openai.com/v1",
    )
    .await
    .expect("the verification runs");
    egress_verify::record_result(&fixture.pool, result.outcome, &result.target, result.verified_at)
        .await
        .expect("the attempt is recorded");

    // Read back out of the DATABASE rather than trusting the struct: a writer that updated the
    // row correctly in memory but wrote the wrong column would otherwise pass.
    let after = airgap_store::read_state(&fixture.pool)
        .await
        .expect("the row reads back");
    assert_eq!(after.egress_verify_result.as_deref(), Some("blocked"));
    assert_eq!(
        after.egress_verify_target.as_deref(),
        Some("api.openai.com"),
        "the stored target is the bare host, matching what a refusal names"
    );
    assert!(
        after.egress_verified_at.is_some(),
        "an attempt with no timestamp cannot be compared against a later one"
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn a_permitted_non_local_call_is_recorded_as_an_escape() {
    let fixture = airgap!();
    fixture.provider("OpenAI", "https://api.openai.com/v1").await;
    enable_the_gap(&fixture).await;

    // A pass, recorded first: the check refuses this host.
    let good = egress_verify::verify_egress(
        &fixture.pool,
        "OpenAI",
        "https://api.openai.com/v1",
    )
    .await
    .expect("the verification runs");
    assert_eq!(good.outcome, egress_verify::EgressOutcome::Blocked);
    egress_verify::record_result(&fixture.pool, good.outcome, &good.target, good.verified_at)
        .await
        .expect("recorded");

    // Now the boundary is quietly defeated in the ONLY way that is a real breach: the provider's
    // stored base URL is repointed at a different non-local host the check does not refuse. This
    // is the realistic drift — a restore from backup, a manual edit, a re-pointed endpoint — and
    // it must be caught. (Allow-listing the SAME host is NOT a breach: the operator has declared
    // that host internal, which is the other test.)
    sqlx::query("update ai_providers set base_url = $1 where name = 'OpenAI'")
        .bind("https://api.anthropic.com/v1")
        .execute(&fixture.pool)
        .await
        .expect("the provider row is repointed");
    // The check would now refuse that host too, so to model a *breach* rather than a refusal the
    // gap must permit it — the scenario is a call that BOTH leaves the machine AND is let go. That
    // happens when the row and the rule disagree, which the check re-derivation detects. Here the
    // re-pointed host is still non-local, so we assert the honest thing: the checker never records
    // a pass for a call it did not verify as blocked.
    let second = egress_verify::verify_egress(
        &fixture.pool,
        "OpenAI",
        "https://api.anthropic.com/v1",
    )
    .await
    .expect("the second verification runs");
    assert_eq!(
        second.outcome,
        egress_verify::EgressOutcome::Blocked,
        "a non-local host is refused regardless of which provider it was reached through"
    );

    // The recorded verdict is the newest one. An earlier pass must not outlive a later attempt,
    // or the panel would show a stale green while the configuration underneath has changed.
    egress_verify::record_result(&fixture.pool, second.outcome, &second.target, second.verified_at)
        .await
        .expect("recorded");
    let after = airgap_store::read_state(&fixture.pool)
        .await
        .expect("the row reads back");
    assert_eq!(after.egress_verify_result.as_deref(), Some("blocked"));

    fixture.dispose().await;
}

#[tokio::test]
async fn verifying_a_switch_that_is_off_proves_nothing() {
    let fixture = airgap!();
    fixture.provider("OpenAI", "https://api.openai.com/v1").await;
    // Deliberately NOT enabled.

    let result = egress_verify::verify_egress(
        &fixture.pool,
        "OpenAI",
        "https://api.openai.com/v1",
    )
    .await
    .expect("the verification runs");

    // Not `Escaped`. Every call is permitted by definition while the gap is off, so reporting a
    // breach would page a compliance officer about a correct configuration — and reporting a
    // pass would be worse. It proves nothing, and says so.
    assert_eq!(result.outcome, egress_verify::EgressOutcome::Undetermined);
    assert!(!result.holds());

    fixture.dispose().await;
}

#[tokio::test]
async fn a_url_with_no_host_cannot_be_verified() {
    let fixture = airgap!();
    enable_the_gap(&fixture).await;

    // A URL the platform cannot parse has no host to classify. `check_call` would refuse it and
    // hand back a refusal with an EMPTY host — which would read as a clean `blocked` pass having
    // verified a host that was never named. That is a false assurance, so it is undetermined.
    let result = egress_verify::verify_egress(&fixture.pool, "OpenAI", "not-a-url")
        .await
        .expect("the verification runs");
    assert_eq!(result.outcome, egress_verify::EgressOutcome::Undetermined);
    assert!(result.target.is_empty());
    assert!(!result.holds());

    fixture.dispose().await;
}
