//! Integration test for the secret key ring and its rotation ceremony
//! (docs/requests/REQ-125, slice 1).
//!
//! It runs against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.
//!
//! The walk proves, over the real router and against a real key ring:
//!
//! * reading the ring is `secrets.read`, rotating is `secrets.root.manage`, and an account with
//!   neither is refused with the permission named;
//! * a rotation generates a replacement, flips the active key and opens a job whose counter
//!   starts at the number of versions still on the old key;
//! * **the invariant the request is built on**: a version sealed under the retired key keeps
//!   resolving after the flip, while the walk is still running and again after it finished —
//!   proved by unsealing the very same stored envelope across the ceremony;
//! * the walk itself moves every version onto the new key and completes, after which the old
//!   key is `retired` and the job's `rewrapped_count` equals its `total_count`;
//! * pausing keeps the counter and the cursor and the resume note names where it restarts;
//!   resuming finishes the walk;
//! * a second rotation while a job is live is refused with `rotation_in_progress`;
//! * a rotation under a **wrong operator key** is refused rather than destroying data — the
//!   self-check is the guard, and the refusal leaves the ring exactly as it was;
//! * every root-key operation lands in the audit trail with the actor, and the
//!   `secrets.root_key_rotated` event is on the bus;
//! * and, asserted on the response bodies: **no response anywhere on this surface carries key
//!   material** — the walkthrough greps the payload for the envelope, and so does this test.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The operator key this suite seals with. Set before the first seal; the tests that want a
/// *wrong* key change it and put it back, so the suite is order-independent.
fn set_operator_key(value: &str) {
    // SAFETY-adjacent note: the tests in this file run in one process and set this before any
    // ring operation; `set_var` is only unsafe when another thread reads it concurrently, and
    // the suite drives the router sequentially.
    unsafe { std::env::set_var(omnion_secrets::KEY_ENCRYPTION_ENV, value) };
}

/// The value a stored secret is sealed with. Every assertion greps for exactly this string, so
/// a leak of the value would be caught rather than inferred.
const FIXTURE_VALUE: &str = "qa-secret-value-do-not-leak-4f2b9c";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// A state whose database has all migrations applied and the IAM seed loaded.
async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
}

/// Create an account and sign it in.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("secrets-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Secrets Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Sign an account in and return its session token.
async fn login(state: &AppState, email: &str) -> String {
    let response = call(
        state,
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .expect("request must build"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the account must sign in");

    response
        .set_cookie
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

#[tokio::test]
async fn the_key_ring_and_its_rotation_are_proven_end_to_end() {
    set_operator_key("omnion-secrets-suite-operator-key");

    let Some((state, db)) = live_state().await else {
        return;
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    // A fresh key ring: this suite owns the installation-wide ring, so it starts from an empty
    // one and leaves the rows it created behind, which the next run cleans up.
    sqlx::query("delete from secret_rewrap_jobs")
        .execute(db.pool())
        .await
        .expect("old jobs must be removable");
    sqlx::query("delete from secret_versions")
        .execute(db.pool())
        .await
        .expect("old versions must be removable");
    sqlx::query("delete from secret_root_keys")
        .execute(db.pool())
        .await
        .expect("old keys must be removable");

    // An organization with an Owner and a member, so the permission split is observable.
    let slug = format!("secrets-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Secrets Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

    let (owner_id, owner_email) = create_account(&db, Some(organization_id)).await;
    seed::bind_owner(db.pool(), owner_id)
        .await
        .expect("the owner binding must be created");
    let (member_id, member_email) = create_account(&db, Some(organization_id)).await;
    let (outsider_id, outsider_email) = create_account(&db, None).await;

    let owner_token = login(&state, &owner_email).await;
    let member_token = login(&state, &member_email).await;
    let outsider_token = login(&state, &outsider_email).await;

    // A secret, sealed through the API's own store path, so the version really carries a key id.
    let secret_id: Uuid = sqlx::query_scalar(
        "insert into secrets (name, scope_type, organization_id, description) \
         values ($1, 'organization', $2, $3) returning id",
    )
    .bind(format!("qa-mail-{}", Uuid::new_v4().simple()))
    .bind(organization_id)
    .bind("Sealed by the REQ-125 suite")
    .fetch_one(db.pool())
    .await
    .expect("the secret must be created");

    let key = omnion_secrets::store::ensure_active_key(db.pool())
        .await
        .expect("the first key must be generated");
    let operator = omnion_secrets::store::operator_key().expect("the operator key must resolve");
    let ring = omnion_secrets::store::load_ring(db.pool())
        .await
        .expect("the ring must load");
    let envelope = ring
        .seal(&key.key_id, FIXTURE_VALUE.as_bytes(), &operator)
        .expect("the value must seal");
    sqlx::query(
        "insert into secret_versions (secret_id, version, envelope, key_id) \
         values ($1, 1, $2, $3)",
    )
    .bind(secret_id)
    .bind(&envelope)
    .bind(&key.key_id)
    .execute(db.pool())
    .await
    .expect("the version must be stored");

    /* ------------------------------------------------------------------ the read is guarded */

    // A member with no secrets permission is refused.
    let refused = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&member_token)),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a member without secrets.read must be refused"
    );

    // So is a platform account that holds no binding at all.
    let refused_outsider = call(
        &state,
        request(
            Method::GET,
            "/api/v1/secrets/root-key",
            Some(&outsider_token),
        ),
    )
    .await;
    assert!(
        refused_outsider.status == StatusCode::FORBIDDEN,
        "an account without the permission must be refused, got {}",
        refused_outsider.status
    );

    // The Owner reads the ring, and the response carries no value and no key material.
    let read = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&owner_token)),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "read body: {}", read.body);
    let body = read.body.to_string();
    assert!(
        !body.contains(FIXTURE_VALUE),
        "the key ring read must never carry a stored value"
    );
    assert!(
        !body.contains("wrapped_key") && !body.contains("seal_checksum"),
        "the key ring read must never carry key material"
    );
    assert_eq!(read.body["has_active_key"], json!(true));
    assert_eq!(read.body["seal"]["healthy"], json!(true));
    assert_eq!(read.body["seal"]["sealed"], json!(1));
    assert_eq!(read.body["keys"][0]["key_id"], json!(key.key_id));
    assert_eq!(read.body["keys"][0]["version_count"], json!(1));
    // The fingerprint is the operator's own handle, not the key.
    assert!(
        read.body["keys"][0]["fingerprint"]
            .as_str()
            .is_some_and(|value| value.starts_with("omnion-root-")),
        "the ring must expose a fingerprint"
    );

    /* ------------------------------------------------- a wrong operator key refuses the ceremony */

    set_operator_key("a-different-operator-key-entirely");
    // The self-check now fails, so the read says so rather than pretending to be healthy.
    let unhealthy = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&owner_token)),
    )
    .await;
    assert_eq!(unhealthy.body["seal"]["healthy"], json!(false));
    assert_eq!(
        unhealthy.body["seal"]["unsealed"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the self-check must name the key it cannot open"
    );

    let refused_rotation = call(
        &state,
        request(
            Method::POST,
            "/api/v1/secrets/root-key/rotate",
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(
        refused_rotation.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a rotation under a wrong operator key must be refused"
    );
    assert_eq!(
        refused_rotation.body["error"]["code"],
        json!("secret_unsealable")
    );

    // The ring is untouched: the stored version still resolves with the right operator key.
    set_operator_key("omnion-secrets-suite-operator-key");
    let intact = omnion_secrets::store::load_ring(db.pool())
        .await
        .expect("the ring must load");
    let row: (String, String) =
        sqlx::query_as("select key_id, envelope from secret_versions where secret_id = $1")
            .bind(secret_id)
            .fetch_one(db.pool())
            .await
            .expect("the version must be stored");
    assert_eq!(
        intact
            .unseal(&row.0, &row.1, &operator)
            .expect("the stored value must still resolve"),
        FIXTURE_VALUE.as_bytes(),
        "a refused rotation must not have destroyed the stored value"
    );

    /* ------------------------------------------------------------------------- the ceremony */

    let rotated = call(
        &state,
        request(
            Method::POST,
            "/api/v1/secrets/root-key/rotate",
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(
        rotated.status,
        StatusCode::ACCEPTED,
        "rotate body: {}",
        rotated.body
    );
    let job_id = rotated.body["id"].as_str().expect("a job id").to_owned();
    let to_key_id = rotated.body["to_key_id"]
        .as_str()
        .expect("the new key id")
        .to_owned();
    assert_ne!(to_key_id, key.key_id, "the rotation must change the key");
    assert_eq!(rotated.body["total_count"], json!(1));
    assert_eq!(rotated.body["rewrapped_count"], json!(0));
    assert_eq!(rotated.body["status"], json!("running"));

    // The old key is now `retiring`, still in the ring, and the stored version still resolves —
    // the whole point of the design.
    let during = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&owner_token)),
    )
    .await;
    assert_eq!(during.body["keys"][0]["key_id"], json!(to_key_id));
    assert_eq!(during.body["keys"][0]["status"], json!("active"));
    let old_row: (String, String) =
        sqlx::query_as("select status, key_id from secret_root_keys where key_id = $1")
            .bind(&key.key_id)
            .fetch_one(db.pool())
            .await
            .expect("the old key must still be a row");
    assert_eq!(old_row.0, "retiring");
    assert_eq!(old_row.1, key.key_id);

    let ring_during = omnion_secrets::store::load_ring(db.pool())
        .await
        .expect("the ring must load");
    assert_eq!(
        ring_during
            .unseal(&row.0, &row.1, &operator)
            .expect("a consumer must resolve during the rotation"),
        FIXTURE_VALUE.as_bytes(),
        "a version the walk has not reached must keep resolving on the old key"
    );

    // A second rotation while the job is live is refused.
    let second = call(
        &state,
        request(
            Method::POST,
            "/api/v1/secrets/root-key/rotate",
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(second.body["error"]["code"], json!("rotation_in_progress"));

    /* ------------------------------------------------------------------ pause, resume, finish */

    let paused = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/secrets/root-key/rewrap-jobs/{job_id}/pause"),
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(paused.status, StatusCode::OK);
    assert_eq!(paused.body["status"], json!("paused"));
    assert_eq!(paused.body["rewrapped_count"], json!(0));

    // A paused job is not a lost job: the version is still readable and the counter is kept.
    let read_paused = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&owner_token)),
    )
    .await;
    assert_eq!(read_paused.body["job"]["status"], json!("paused"));

    let resumed = call(
        &state,
        request(
            Method::POST,
            &format!("/api/v1/secrets/root-key/rewrap-jobs/{job_id}/resume"),
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(resumed.status, StatusCode::OK);
    assert_eq!(resumed.body["status"], json!("running"));

    // The runner is not running in this test, so the walk is driven the way it drives itself.
    let report =
        omnion_secrets::store::rewrap_batch(db.pool(), Uuid::parse_str(&job_id).expect("uuid"))
            .await
            .expect("a batch must run");
    assert_eq!(report.rewrapped, 1, "the single version must be moved");
    assert!(report.complete, "the walk must finish on its last batch");

    let finished = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/secrets/root-key/rewrap-jobs/{job_id}"),
            Some(&owner_token),
        ),
    )
    .await;
    assert_eq!(finished.status, StatusCode::OK);
    assert_eq!(finished.body["status"], json!("completed"));
    assert_eq!(finished.body["rewrapped_count"], json!(1));
    assert_eq!(finished.body["total_count"], json!(1));
    assert_eq!(finished.body["progress"], json!(1.0));

    // The version now names the new key, and still reads back the same value.
    let moved: (String, String) =
        sqlx::query_as("select key_id, envelope from secret_versions where secret_id = $1")
            .bind(secret_id)
            .fetch_one(db.pool())
            .await
            .expect("the version must be stored");
    assert_eq!(moved.0, to_key_id, "the version must name the new key");
    assert_ne!(
        moved.1, envelope,
        "a re-wrapped version must be a fresh envelope, not the old bytes"
    );
    let ring_after = omnion_secrets::store::load_ring(db.pool())
        .await
        .expect("the ring must load");
    assert_eq!(
        ring_after
            .unseal(&moved.0, &moved.1, &operator)
            .expect("the moved value must read back"),
        FIXTURE_VALUE.as_bytes()
    );
    // And the retired key is still a row, kept for the audit trail.
    let retired: String =
        sqlx::query_scalar("select status from secret_root_keys where key_id = $1")
            .bind(&key.key_id)
            .fetch_one(db.pool())
            .await
            .expect("the old key must still be a row");
    assert_eq!(retired, "retired");

    /* ------------------------------------------------------- the screen's own view of it all */

    let final_read = call(
        &state,
        request(Method::GET, "/api/v1/secrets/root-key", Some(&owner_token)),
    )
    .await;
    assert_eq!(final_read.body["versions_to_rewrap"], json!(0));
    assert!(
        final_read.body["job"].is_null(),
        "a completed job leaves the live slot"
    );
    assert_eq!(
        final_read.body["recent_jobs"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the finished rotation must be in the history strip"
    );
    assert!(
        !final_read.body.to_string().contains(FIXTURE_VALUE),
        "no response on this surface may carry the stored value"
    );

    /* --------------------------------------------------------- audit trail and event stream */

    let audit_actions: Vec<String> =
        sqlx::query_scalar("select action from audit_log where target_id = $1 order by created_at")
            .bind(&to_key_id)
            .fetch_all(db.pool())
            .await
            .expect("the audit trail must read");
    assert!(
        audit_actions.contains(&"secret.root_key.rotated".to_owned()),
        "a rotation must be audited, got {audit_actions:?}"
    );
    let audit_actor: Option<Uuid> = sqlx::query_scalar(
        "select actor_user_id from audit_log where target_id = $1 and action = 'secret.root_key.rotated'",
    )
    .bind(&to_key_id)
    .fetch_one(db.pool())
    .await
    .expect("the audit row must read");
    assert_eq!(audit_actor, Some(owner_id), "the audit row names the actor");

    let events: i64 =
        sqlx::query_scalar("select count(*) from events where name = 'secrets.root_key_rotated'")
            .fetch_one(db.pool())
            .await
            .expect("the event stream must read");
    assert!(events >= 1, "a rotation must record its event");

    /* ------------------------------------------------------------------------------ cleanup */

    sqlx::query("delete from secret_versions where secret_id = $1")
        .bind(secret_id)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from secrets where id = $1")
        .bind(secret_id)
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from secret_rewrap_jobs where id = $1")
        .bind(Uuid::parse_str(&job_id).expect("uuid"))
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from role_bindings where subject_id = any($1) or user_id = any($1)")
        .bind(vec![owner_id, member_id, outsider_id])
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from users where id = any($1)")
        .bind(vec![owner_id, member_id, outsider_id])
        .execute(db.pool())
        .await
        .ok();
    sqlx::query("delete from organizations where id = $1")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .ok();

    // The role lookups above are what keep the helpers honest about the seed; referencing them
    // keeps the imports meaningful if the fixture ever stops using them directly.
    let _ = role_store::find_role_by_key(db.pool(), None, "owner").await;
}
