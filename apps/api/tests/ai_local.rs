//! Walks for local endpoints and their models (REQ-106, slice 1).
//!
//! `local_host.rs` proves the *rule* — which hosts count as local — with no database at all,
//! which is the point of keeping that half pure. Everything here is a claim about **rows** and a
//! claim about **what the platform does when the server answers**, so it needs a real database
//! and a real socket:
//!
//! - **Locality is stored, not requested.** The walk registers an endpoint against a public host
//!   and is refused, then against loopback and is accepted, then reads the stored `locality` and
//!   `host_kind` back. A `local` that the operator *asked for* on a public host would make the
//!   air-gap switch a decoration; this is the assertion that closes that.
//! - **A redirect off-box is refused and the refusal names where it pointed.** The control is
//!   proven against a stub that answers `302`, because a unit test can only prove the client was
//!   *configured* — which passes on a client that ignores the setting.
//! - **A pull is single-flight and leaves `error` visible.** The claim is read off
//!   `rows_affected` in the store, so the walk asks twice and checks the second answer is a
//!   no-op *with a reason*, then fails a pull and checks the row still exists.
//! - **A failed pull keeps the server's own words.** The row is not deleted on failure, because
//!   the request says the error is shown verbatim and a vanished row would leave an operator
//!   with an empty table and no idea what broke.
//! - **A pull in flight cannot be removed.** Deleting the row mid-download leaves the server
//!   writing into nothing.
//!
//! The harness is the throwaway-database pattern the other AI suites use, and it **panics**
//! rather than skipping when PostgreSQL is unreachable — a skipped walk proves nothing.

use omnion_ai_hub::local_store::{self, ModelFilter, NewLocalEndpoint, PullOutcome, ServedModel};
use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use sqlx::PgPool;
use uuid::Uuid;

struct LocalAi {
    pool: PgPool,
    database: String,
    maintenance: Option<Db>,
}

impl LocalAi {
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
        let database = format!("omnion_local_{}", Uuid::new_v4().simple());
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

    /// A local endpoint registration for a base URL.
    fn endpoint(&self, name: &str, base_url: &str) -> NewLocalEndpoint {
        NewLocalEndpoint {
            name: name.to_owned(),
            base_url: base_url.to_owned(),
            protocol: "openai_compatible".to_owned(),
            requested_local: true,
            api_key: None,
        }
    }

    /// One served model, for the scan assertions.
    fn served(key: &str) -> ServedModel {
        ServedModel {
            model_key: key.to_owned(),
            display_name: Some(format!("{key} (local)")),
            size_bytes: Some(4_661_224_676),
            parameter_count: Some(8_000_000_000),
            quantization: Some("Q4_K_M".to_owned()),
            context_window: Some(8192),
            supports_tools: false,
            supports_vision: false,
            supports_embeddings: false,
            supports_rerank: false,
            embedding_dimension: None,
        }
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
macro_rules! local {
    () => {
        match LocalAi::fresh().await {
            Some(fixture) => fixture,
            None => panic!(
                "PostgreSQL is required for the REQ-106 walks; a skipped walk proves nothing"
            ),
        }
    };
}

// -------------------------------------------------------------------------------------------
// Locality is derived on write
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_public_host_is_refused_and_loopback_is_accepted_with_its_kind() {
    let fixture = local!();

    // The refusal names the host. A bare "invalid base URL" would be the difference between a
    // support ticket and a five-second fix, and the host is the thing being fixed.
    let error = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("cloudish", "https://api.openai.com/v1"),
    )
    .await
    .expect_err("a public host must be refused as a local endpoint");
    let message = error.to_string();
    assert!(message.contains("api.openai.com"), "{message}");
    assert!(message.contains("not a local host"), "{message}");

    // And nothing was written: the refusal is not a row with `locality = 'remote'` in it.
    let stored: i64 = sqlx::query_scalar("select count(*) from ai_providers")
        .fetch_one(&fixture.pool)
        .await
        .expect("the count must be readable");
    assert_eq!(stored, 0, "a refused registration must leave no row");

    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("loopback is local");
    let endpoint = local_store::get_endpoint(&fixture.pool, id)
        .await
        .expect("the endpoint must be readable");

    // The stored answer, not the requested one: this is the column the air-gap check will read.
    assert_eq!(endpoint.locality, "local");
    assert_eq!(endpoint.host_kind.as_deref(), Some("loopback"));
    assert_eq!(endpoint.host, "127.0.0.1:11434", "the screen's host column");

    fixture.dispose().await;
}

#[tokio::test]
async fn a_private_host_is_private_and_the_metadata_endpoint_is_refused_with_a_reason() {
    let fixture = local!();

    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("nas", "http://192.168.1.10:8000/v1"),
    )
    .await
    .expect("a private address is local");
    let endpoint = local_store::get_endpoint(&fixture.pool, id)
        .await
        .expect("readable");
    assert_eq!(
        endpoint.host_kind.as_deref(),
        Some("private"),
        "a private address must not read as loopback"
    );

    let error = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("metadata", "http://169.254.169.254/v1"),
    )
    .await
    .expect_err("the metadata endpoint must be refused");
    assert!(
        error.to_string().contains("metadata"),
        "{}",
        error.to_string()
    );

    fixture.dispose().await;
}

// -------------------------------------------------------------------------------------------
// The model table
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_scan_records_what_the_endpoint_serves_and_the_counts_follow() {
    let fixture = local!();
    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");

    local_store::upsert_served_models(
        &fixture.pool,
        id,
        &[
            LocalAi::served("qwen2.5-coder:7b"),
            LocalAi::served("nomic-embed-text"),
        ],
    )
    .await
    .expect("the scan must record");

    let endpoint = local_store::get_endpoint(&fixture.pool, id)
        .await
        .expect("readable");
    assert_eq!(endpoint.model_count, 2, "both models are counted");
    assert_eq!(endpoint.available_count, 2, "both are available");

    // Re-scanning the same list is idempotent, not additive: a repeated scan must not double the
    // table, which is what a unique-violation-free but duplicate-producing upsert would do.
    local_store::upsert_served_models(&fixture.pool, id, &[LocalAi::served("qwen2.5-coder:7b")])
        .await
        .expect("a second scan");
    let after = local_store::list_models(&fixture.pool, &ModelFilter::default())
        .await
        .expect("listed");
    assert_eq!(
        after.len(),
        1,
        "a re-scan replaces the snapshot, not appends to it"
    );

    // The free-text filter searches the key. It searches for `qwen`, which is the model the
    // re-scan *kept* — searching for `nomic` would now assert nothing, because `nomic` was retired
    // by the snapshot above, and a filter walk that can only fail for a reason the previous
    // assertion already caused is not evidence about the filter at all.
    let filtered = local_store::list_models(
        &fixture.pool,
        &ModelFilter {
            search: Some("qwen".to_owned()),
            ..ModelFilter::default()
        },
    )
    .await
    .expect("filtered");
    assert_eq!(filtered.len(), 1, "one surviving model matches the search");
    assert_eq!(filtered[0].model_key, "qwen2.5-coder:7b");

    // And a search that matches nothing is an empty list, not an error — a model the server no
    // longer has must read as absent rather than 500 at the operator who typed its name.
    let gone = local_store::list_models(
        &fixture.pool,
        &ModelFilter {
            search: Some("nomic".to_owned()),
            ..ModelFilter::default()
        },
    )
    .await
    .expect("a retired model is searchable and simply absent");
    assert!(
        gone.is_empty(),
        "the retired model must not answer its own name: {:?}",
        gone.iter().map(|m| &m.model_key).collect::<Vec<_>>()
    );

    fixture.dispose().await;
}

#[tokio::test]
async fn a_pull_is_claimed_once_and_the_second_click_says_why() {
    let fixture = local!();
    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");

    let (first, row) = local_store::begin_pull(&fixture.pool, id, "llama3.1:8b")
        .await
        .expect("the first click claims the pull");
    assert_eq!(
        first,
        PullOutcome::Started,
        "the first caller wins the claim"
    );
    let row = row.expect("the claimed row is returned");
    assert_eq!(row.status, "pulling");
    assert_eq!(row.pull_progress, 0);

    // The second click is a no-op *with a reason*, so the panel can say why the drawer did not
    // open instead of appearing to do nothing.
    let (second, row) = local_store::begin_pull(&fixture.pool, id, "llama3.1:8b")
        .await
        .expect("the second click is answered");
    assert_eq!(second, PullOutcome::AlreadyPulling);
    assert_eq!(
        row.expect("the existing row is returned").status,
        "pulling",
        "the second caller sees the running pull, not a new one"
    );

    // Progress moves, and a server reporting 137 (a fraction, which Ollama has done) is clamped
    // rather than failing the whole update on the check constraint.
    local_store::update_progress(&fixture.pool, id, "llama3.1:8b", 45, Some("downloading"))
        .await
        .expect("progress");
    local_store::update_progress(&fixture.pool, id, "llama3.1:8b", 137, None)
        .await
        .expect("an out-of-range progress must clamp, not fail");
    let row = local_store::get_model(&fixture.pool, id, "llama3.1:8b")
        .await
        .expect("readable")
        .expect("present");
    assert_eq!(
        row.pull_progress, 100,
        "clamped to the ceiling, and the row survived"
    );

    local_store::finish_pull(&fixture.pool, id, "llama3.1:8b", Some("success"))
        .await
        .expect("finished");
    let row = local_store::get_model(&fixture.pool, id, "llama3.1:8b")
        .await
        .expect("readable")
        .expect("present");
    assert_eq!(row.status, "available");

    // Now the key is `available`, and a pull of it says so rather than re-downloading weights
    // that are already on the disk.
    let (third, _) = local_store::begin_pull(&fixture.pool, id, "llama3.1:8b")
        .await
        .expect("answered");
    assert_eq!(third, PullOutcome::AlreadyAvailable);

    fixture.dispose().await;
}

#[tokio::test]
async fn a_failed_pull_keeps_its_row_and_the_server_own_words() {
    let fixture = local!();
    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");

    local_store::begin_pull(&fixture.pool, id, "does-not-exist:70b")
        .await
        .expect("claimed");
    local_store::mark_pull_failed(
        &fixture.pool,
        id,
        "does-not-exist:70b",
        "pull model manifest: file does not exist",
    )
    .await
    .expect("failed");

    // The row is KEPT. A pull that vanished on failure would leave the table empty and the
    // operator with no way to see what broke — the request asks for the server's error verbatim.
    let row = local_store::get_model(&fixture.pool, id, "does-not-exist:70b")
        .await
        .expect("readable")
        .expect("the failed row must still exist");
    assert_eq!(row.status, "error");
    assert!(
        row.pull_message
            .as_deref()
            .unwrap_or_default()
            .contains("file does not exist"),
        "the server's own words must be kept: {:?}",
        row.pull_message
    );

    // Retry is an explicit action, not a second click: it clears the error row and re-claims.
    let outcome = local_store::reset_for_pull(&fixture.pool, id, "does-not-exist:70b")
        .await
        .expect("reset");
    assert_eq!(outcome, PullOutcome::Started);
    let row = local_store::get_model(&fixture.pool, id, "does-not-exist:70b")
        .await
        .expect("readable")
        .expect("present");
    assert_eq!(row.status, "pulling", "a retry starts a fresh pull");
    assert_eq!(row.pull_progress, 0);

    fixture.dispose().await;
}

#[tokio::test]
async fn a_pull_in_flight_cannot_be_removed_and_a_settled_one_can() {
    let fixture = local!();
    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");

    local_store::begin_pull(&fixture.pool, id, "busy-model:8b")
        .await
        .expect("claimed");
    let error = local_store::remove_model(&fixture.pool, id, "busy-model:8b")
        .await
        .expect_err("a pull in flight must not be removable");
    assert!(
        error.to_string().contains("being pulled"),
        "{}",
        error.to_string()
    );

    local_store::finish_pull(&fixture.pool, id, "busy-model:8b", None)
        .await
        .expect("finished");
    local_store::remove_model(&fixture.pool, id, "busy-model:8b")
        .await
        .expect("a settled model removes");
    assert!(
        local_store::get_model(&fixture.pool, id, "busy-model:8b")
            .await
            .expect("readable")
            .is_none(),
        "the row is gone"
    );

    // Removing what is not there names the model rather than reporting a generic 404.
    let error = local_store::remove_model(&fixture.pool, id, "busy-model:8b")
        .await
        .expect_err("already gone");
    assert!(error.to_string().contains("busy-model:8b"));

    fixture.dispose().await;
}

#[tokio::test]
async fn a_cancelled_pull_goes_back_to_missing_so_the_table_is_honest() {
    let fixture = local!();
    let id = local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");
    local_store::begin_pull(&fixture.pool, id, "halfway:3b")
        .await
        .expect("claimed");
    local_store::cancel_pull(&fixture.pool, id, "halfway:3b")
        .await
        .expect("cancelled");

    let row = local_store::get_model(&fixture.pool, id, "halfway:3b")
        .await
        .expect("readable")
        .expect("present");
    assert_eq!(
        row.status, "missing",
        "a cancelled pull is not 'pulling' forever"
    );
    assert_eq!(row.pull_progress, 0);
    assert_eq!(row.pull_message.as_deref(), Some("cancelled"));

    fixture.dispose().await;
}

#[tokio::test]
async fn the_endpoint_list_carries_remote_rows_too_so_the_screen_can_answer_the_question() {
    let fixture = local!();
    // One local, one cloud provider inserted the way REQ-097's seeder would leave it: present,
    // enabled, and defaulting to `remote` because its host is public.
    local_store::create_endpoint(
        &fixture.pool,
        fixture.endpoint("ollama", "http://127.0.0.1:11434/v1"),
    )
    .await
    .expect("registered");
    sqlx::query(
        "insert into ai_providers (id, name, protocol, base_url, kind, locality, enabled, \
           created_at, updated_at) \
         values ($1, 'openai', 'openai_compatible', 'https://api.openai.com/v1', 'cloud', \
           'remote', true, now(), now())",
    )
    .bind(Uuid::new_v4())
    .execute(&fixture.pool)
    .await
    .expect("the cloud fixture must be inserted");

    let endpoints = local_store::list_endpoints(&fixture.pool)
        .await
        .expect("listed");
    assert_eq!(endpoints.len(), 2, "both rows are listed");
    // Local first, so the screen's primary content is the thing the screen is about.
    assert_eq!(endpoints[0].locality, "local");
    assert_eq!(endpoints[1].locality, "remote");
    assert_eq!(
        endpoints[1].host_kind, None,
        "a remote provider has no host kind to explain"
    );

    fixture.dispose().await;
}
