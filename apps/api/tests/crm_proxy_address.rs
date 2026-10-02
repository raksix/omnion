//! REQ-117, slice 20 — who the platform thinks the caller is, when a proxy is in front.
//!
//! Run through `scripts/qa/run-crm-proxy-address.sh`.
//!
//! ## The gap this file measures
//!
//! Slice 19 shipped `MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR`: ten submissions an hour from one
//! address, counted from durable `crm_leads.submitter_ip` rows. That column is written by
//! exactly one caller — `POST /api/v1/crm/intake/{source_key}`, whose address comes from the
//! `ClientAddress` extractor.
//!
//! And that extractor read the **socket only**. Behind the platform's own proxy every visitor
//! arrives from one address, so the column held the proxy's address for all of them, the
//! per-address counter counted the proxy as a single caller, and the dial did not exist in the
//! only topology the platform is deployed in. Slice 19's gate could not see it: it drove
//! `store::capture` directly with an address argument, so it tested the counter against an
//! address it supplied itself — and the address's *provenance* was never in question.
//!
//! That is the trap this file is written to avoid, and it is the same shape as the other
//! eighteen gates that went green against a promise they never touched: **begin at the
//! surface a real request enters by, and read the durable row back.** So every test here drives
//! the actual HTTP endpoint through the actual router, with a `ConnectInfo` extension the way
//! `into_make_service_with_connect_info` puts one there in production, and then asks the
//! database what address it stored.
//!
//! ## What each assertion is for
//!
//! * **A loopback proxy speaks for its caller** — the positive line. Behind the edge the row
//!   must carry the visitor, or the ceiling counts the proxy.
//! * **A public peer keeps its own address** — the negative control, and the reason the rule is
//!   not "always read the header". A caller that may write `X-Forwarded-For` freely would
//!   otherwise mint a fresh bucket per request and walk straight out of its own ceiling.
//! * **The ceiling is per visitor, not per proxy** — the end-to-end claim, and the assertion
//!   that would have caught slice 19 shipping inert. Ten from one visitor behind the proxy, then
//!   the eleventh is `429`, while a *different* visitor behind the same proxy still gets through.
//! * **A misbehaving proxy stores no address at all** — the shape the first version of the fix
//!   got wrong, found by its own unit test: falling back to the peer re-creates the very defect
//!   (every visitor sharing the loopback address), so the honest answer is `None`, which the
//!   ceiling reads as "not throttled" and never as "one bucket with everybody else".
//!
//! ## Why no rate-limit middleware assertion appears here
//!
//! The platform limiter is exercised by `apps/api/tests/rate_limit.rs` over its own scopes.
//! This file is about one fact — which address reaches a **durable CRM row** — and adding a
//! limiter assertion would measure a second surface with a second counter while claiming the
//! first. The shared resolver's own rule is unit-tested in `apps/api/src/client_ip.rs` and
//! through the limiter's `ClientId::key` in `rate_limit_middleware.rs`; here it is only ever
//! observed as "what the database holds".

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_module_crm_intake::vocabulary::MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A visitor's address, RFC 5737 documentation range — it can never be a real client.
const VISITOR: &str = "203.0.113.9";
/// A second visitor at the same proxy: the negative control for the per-address claim.
const OTHER_VISITOR: &str = "203.0.113.10";
/// A public peer, with a header it wrote for itself.
const PUBLIC_PEER: &str = "198.51.100.4";
/// The loopback address a reverse proxy on this host arrives from.
const LOCAL_PROXY: &str = "127.0.0.1";

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A throwaway database with every migration applied.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let database = format!("omnion_crm_proxy_{}", Uuid::new_v4().simple());

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

        let redis = RedisClient::new(&config.redis.url).ok()?;
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );

        Some(Self { state, db, maintenance, database })
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

fn swap_database(url: &str, database: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a postgres URL");
    let (authority, _path) = rest.split_once('/').unwrap_or((rest, ""));
    format!("{scheme}://{authority}/{database}")
}

// ---------------------------------------------------------------------------------------------
// A keyed endpoint and a submission
// ---------------------------------------------------------------------------------------------

/// A live `endpoint` source with a clear key, mapping an e-mail the way the ceiling needs.
///
/// The mapping is the point: without a required e-mail the verdict is "rejected" for a reason
/// that has nothing to do with the ceiling, and a test that only reads a status code would call
/// that a pass.
async fn keyed_source(harness: &Harness, org: Uuid) -> (Uuid, String) {
    let (source, key) = sqlx::query_as::<_, (Uuid, String)>(
        "insert into crm_intake_sources \
         (id, organization_id, name, kind, mapping, required_targets, consent_required, active) \
         values (gen_random_uuid(), $1, 'proxy gate', 'endpoint', \
                 '[{\"target\": \"email\", \"source_key\": \"email\", \"required\": true}]'::jsonb, \
                 '{}', false, true) \
         returning id, 'clearkey-' || id::text",
    )
    .bind(org)
    .fetch_one(harness.db.pool())
    .await
    .expect("a keyed source");
    // The stored hash is what the endpoint looks the key up by, so the row has to carry the
    // hash of the key the test sends — a key that exists only in the test would 401.
    sqlx::query(
        "update crm_intake_sources set endpoint_key_hash = $2, endpoint_key_hint = 'clearkey' \
         where id = $1",
    )
    .bind(source)
    .bind(omnion_module_crm_intake::keys::hash_key(&key))
    .execute(harness.db.pool())
    .await
    .expect("the key hash");
    (source, key)
}

async fn fresh_org(harness: &Harness, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {org}"))
        .bind(format!("{label}-{}", org.simple()))
        .execute(harness.db.pool())
        .await
        .expect("an organization");
    org
}

/// One submission through the real endpoint, arriving from `peer`.
///
/// The `ConnectInfo` extension is where `into_make_service_with_connect_info` puts the peer in
/// production, and it is the only place the extractor reads it from — a test that set the
/// header alone would be proving the no-socket branch, not the deployment.
async fn submit(
    harness: &Harness,
    key: &str,
    peer: Option<&str>,
    forwarded: Option<&str>,
    index: i64,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/crm/intake/{key}"))
        .header(header::CONTENT_TYPE, "application/json")
        // A per-attempt idempotency key, so repeated submissions are distinct leads rather than
        // the same one deduplicated: this file counts deliveries, and a colliding key would
        // measure the dedupe policy instead.
        .header("x-idempotency-key", format!("{}-{index}", Uuid::new_v4()));
    if let Some(forwarded) = forwarded {
        builder = builder.header("x-forwarded-for", forwarded);
    }
    let mut request = builder
        .body(Body::from(
            json!({ "email": format!("visitor-{index}@example.test") }).to_string(),
        ))
        .expect("request must build");

    if let Some(peer) = peer {
        use axum::extract::ConnectInfo;
        let address: std::net::SocketAddr = format!("{peer}:40000")
            .parse()
            .expect("a peer address with a port parses");
        request.extensions_mut().insert(ConnectInfo(address));
    }

    let response = routes::router(harness.state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// The address a lead's row holds, read as the database holds it.
async fn stored_address(harness: &Harness, org: Uuid) -> Vec<Option<String>> {
    sqlx::query_scalar(
        "select submitter_ip::text from crm_leads where organization_id = $1 \
         order by received_at, id",
    )
    .bind(org)
    .fetch_all(harness.db.pool())
    .await
    .expect("the rows are readable")
}

fn host_of(value: &str) -> &str {
    value.split('/').next().unwrap_or(value)
}

/// How many rows this organization holds.
async fn lead_count(harness: &Harness, org: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from crm_leads where organization_id = $1")
        .bind(org)
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must run")
}

// ---------------------------------------------------------------------------------------------
// The assertions
// ---------------------------------------------------------------------------------------------

/// **The positive line.** A proxy on this host speaks for its caller, so the durable row
/// carries the visitor's address and not the loopback one.
///
/// Before the fix this read `127.0.0.1/32` for every visitor, and every assertion in slice
/// 19's gate still passed — because that gate supplied the address itself.
#[tokio::test]
async fn a_local_proxys_forwarded_caller_is_what_reaches_the_database() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-forwarded").await;
    let (_source, key) = keyed_source(&harness, org).await;

    let (status, _body) = submit(&harness, &key, Some(LOCAL_PROXY), Some(VISITOR), 1).await;
    assert_eq!(status, StatusCode::ACCEPTED, "a valid submission is accepted");

    let stored = stored_address(&harness, org).await;
    assert_eq!(stored.len(), 1, "one submission writes one lead");
    assert_eq!(
        stored[0].as_deref().map(host_of),
        Some(VISITOR),
        "the visitor's address is what the CRM stores — the whole point of this file"
    );

    harness.dispose().await;
}

/// **The negative control.** A caller that reaches the platform from the open internet writes
/// its own `X-Forwarded-For`, and believing it would let any visitor walk out of the ceiling
/// with one header per request.
#[tokio::test]
async fn a_public_peer_cannot_forge_the_address_it_is_counted_under() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-public").await;
    let (_source, key) = keyed_source(&harness, org).await;

    let (status, _body) = submit(
        &harness,
        &key,
        Some(PUBLIC_PEER),
        Some(VISITOR),
        1,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let stored = stored_address(&harness, org).await;
    assert_eq!(
        stored[0].as_deref().map(host_of),
        Some(PUBLIC_PEER),
        "the forged header is ignored and the socket address is kept"
    );

    harness.dispose().await;
}

/// **The end-to-end claim, and the assertion slice 19's gate structurally could not make.**
///
/// Ten submissions from one visitor behind the proxy are stored, the eleventh is refused
/// `429`, and a *different* visitor behind the same proxy is unaffected. The last part is the
/// one that distinguishes a per-address ceiling from "the proxy is throttled": without it every
/// assertion here would still pass if the platform were refusing all traffic from the edge
/// after ten requests, which is a different bug with the same symptom.
#[tokio::test]
async fn the_ceiling_counts_the_visitor_behind_the_proxy_not_the_proxy() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-ceiling").await;
    let (_source, key) = keyed_source(&harness, org).await;

    for index in 0..MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR {
        let (status, body) =
            submit(&harness, &key, Some(LOCAL_PROXY), Some(VISITOR), index).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "submission {index} is under the ceiling (body: {body})"
        );
    }

    let (status, body) = submit(
        &harness,
        &key,
        Some(LOCAL_PROXY),
        Some(VISITOR),
        MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the eleventh submission from one visitor is refused (body: {body})"
    );
    assert_eq!(body["error"]["code"], "rate_limited");
    assert_eq!(
        lead_count(&harness, org).await,
        i64::from(MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR),
        "a refused submission writes no row, so it cannot be counted either"
    );

    // The negative control, on the same proxy connection: a different visitor is a different
    // bucket, and is served.
    let (status, body) = submit(&harness, &key, Some(LOCAL_PROXY), Some(OTHER_VISITOR), 0).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the ceiling is per address, not per proxy (body: {body})"
    );
    assert_eq!(
        lead_count(&harness, org).await,
        i64::from(MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR) + 1,
        "the second visitor's lead is stored under its own address"
    );

    harness.dispose().await;
}

/// **The shape the first version of the fix got wrong, kept as a regression guard.** A local
/// proxy that writes an unusable value must store *no* address: falling back to the loopback
/// peer would put every visitor of that installation into one bucket, which is the defect this
/// file exists to close, reappearing through the fallback branch.
#[tokio::test]
async fn a_misbehaving_local_proxy_stores_no_address_rather_than_its_own() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-garbled").await;
    let (_source, key) = keyed_source(&harness, org).await;

    let (status, _body) = submit(
        &harness,
        &key,
        Some(LOCAL_PROXY),
        Some("not-an-address"),
        1,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let stored = stored_address(&harness, org).await;
    assert_eq!(
        stored[0], None,
        "an unusable header is no address — never the proxy's, which would make every \
         visitor behind it one caller"
    );

    harness.dispose().await;
}

/// A submission with no address at all is **never** throttled, proxy or no proxy. This is
/// slice 19's rule seen from the other side: the platform's own event-bus deliveries carry no
/// socket, and a surface where "no address" is a shared bucket would be throttle-able by
/// anybody who can reach the endpoint.
#[tokio::test]
async fn a_submission_with_no_address_at_all_is_never_throttled() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-no-address").await;
    let (_source, key) = keyed_source(&harness, org).await;

    for index in 0..MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR + 3 {
        let (status, body) = submit(&harness, &key, None, None, index).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "an in-process delivery is not throttle-able (submission {index}, body: {body})"
        );
    }
    assert!(
        lead_count(&harness, org).await >= i64::from(MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR) + 3,
        "every anonymous delivery was stored"
    );

    harness.dispose().await;
}

/// The same request through the same proxy, twice, is one address — the ceiling is not defeated
/// by a connection that simply reconnects. (Not a real attack model, but it pins that the
/// stored value is the caller's address and not, say, a per-connection identifier.)
#[tokio::test]
async fn two_submissions_through_one_proxy_share_one_address() {
    let Some(harness) = Harness::fresh().await else {
        eprintln!("SKIP: PostgreSQL or Redis is not reachable");
        return;
    };
    let org = fresh_org(&harness, "proxy-shared").await;
    let (_source, key) = keyed_source(&harness, org).await;

    for index in 0..2 {
        let (status, _body) = submit(&harness, &key, Some(LOCAL_PROXY), Some(VISITOR), index).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    let stored = stored_address(&harness, org).await;
    assert_eq!(stored.len(), 2, "both submissions were stored");
    assert!(
        stored.iter().all(|value| value.as_deref().map(host_of) == Some(VISITOR)),
        "both rows carry the same visitor address: {stored:?}"
    );

    harness.dispose().await;
}
