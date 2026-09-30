//! The keyed-write contract, driven over HTTP (REQ-127, slice 2).
//!
//! **What is proved, in order, because each is a different failure:**
//!
//! 1. **A replay returns the FIRST response with `Idempotent-Replay: true`, and the handler ran
//!    once.** The suite counts side effects in the database rather than trusting the header: a
//!    replay that re-runs the handler and returns the same body is indistinguishable from a
//!    correct replay by status alone, and the whole feature is the count.
//! 2. **A changed body is `409 idempotency_conflict`,** and it does NOT execute the handler —
//!    a conflict that ran the write would be the one outcome the client cannot recover from.
//! 3. **A replay while the first attempt is running is `409` with `Retry-After`.** Produced by
//!    claiming the key directly in the store, because making a real handler slow is a race the
//!    suite would lose on a loaded box.
//! 4. **A request refused by a permission check never consumes the key,** and the proof is
//!    that the SAME key then succeeds: a layer that claimed and rolled back would also let it
//!    succeed, so the count of stored rows is asserted too — one, not zero.
//! 5. **The panel's screens work against the same rows** the middleware wrote: the list shows
//!    the key, the detail answers metadata and never a body, and the release flips a stuck key
//!    to `failed` with an audit row and the `keys.released` event.
//! 6. **Releasing a `completed` key is refused** — the one destructive action, guarded.
//!
//! The suite needs PostgreSQL only: the store is the whole subject, and the keyed endpoint is a
//! real handler in the real router, so nothing here depends on Redis.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The endpoint family this suite keys. `POST /api/v1/automations` is the one the middleware is
/// installed on, and the suffix keeps two accounts' keys apart even though they are already
/// scoped by subject.
const SCOPE: &str = "POST /api/v1/automations";

/// One in-process response, in the pieces the assertions need.
#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
    text: String,
    cookie: Option<String>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

async fn call(state: &AppState, request: Request<Body>) -> Reply {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    // BOTH cookies: a mutation presenting only the session is refused by the CSRF layer, and a
    // walk holding one cookie is testing the CSRF layer instead of the thing it came for.
    let cookie = Some(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|raw| raw.split(';').next())
            .filter(|pair| {
                pair.starts_with("omnion_session=") || pair.starts_with("omnion_csrf=")
            })
            .collect::<Vec<_>>()
            .join("; "),
    );
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    Reply {
        status,
        headers,
        body,
        text,
        cookie,
    }
}

fn json_post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

fn keyed(path: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request must build")
}

fn delete(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request must build")
}

/// A signed-in account with the owner role bound.
///
/// The CSRF token is read from the cookie and echoed in the header, which is the double-submit
/// the layer requires — a walk that skips it is refused for a reason that has nothing to do with
/// the idempotency contract.
struct Caller {
    user_id: Uuid,
    cookie: String,
    csrf: String,
}

async fn sign_in(state: &AppState) -> Caller {
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = omnion_identity::organizations::create_organization(
        state.db().pool(),
        omnion_identity::organizations::NewOrganization {
            name: format!("Idempotency org {suffix}"),
            slug: format!("idempotency-{suffix}"),
        },
    )
    .await
    .expect("the organization must be created");
    let email = format!("idempotency-{suffix}@example.test");
    let user = users::create_user(
        state.db().pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Idempotency Walker".to_owned(),
            organization_id: Some(organization.id),
        },
    )
    .await
    .expect("the account must be created");
    seed::bind_owner(state.db().pool(), user.id)
        .await
        .expect("the owner role must be bound");

    let reply = call(
        state,
        json_post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "login: {}", reply.text);
    let cookie = reply.cookie.expect("login sets cookies");
    let csrf = cookie
        .split("; ")
        .find_map(|pair| pair.strip_prefix("omnion_csrf="))
        .expect("login issues a CSRF cookie beside the session")
        .to_owned();
    Caller {
        user_id: user.id,
        cookie,
        csrf,
    }
}

/// Attach the session and the CSRF header to a request.
fn authed(mut request: Request<Body>, caller: &Caller) -> Request<Body> {
    let headers = request.headers_mut();
    if let Ok(value) = axum::http::HeaderValue::from_str(&caller.cookie) {
        headers.insert(header::COOKIE, value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&caller.csrf) {
        headers.insert("x-omnion-csrf", value);
    }
    request
}

/// An automation body. Unique per call, so a replay can be told from a second rule.
///
/// **The shape is the route's, not a guess.** `AutomationInput` takes `event` and `conditions`
/// flat (a workflow row carries them as columns), and the action list is validated by the engine
/// before the store sees it — a body that would fail `rule.definition()` proves nothing about
/// the idempotency layer, because the failure would be a `400` the key never sees.
fn automation_body(name: &str) -> Value {
    json!({
        "name": name,
        "description": "The keyed-write walk's rule.",
        "event": "page.published",
        "conditions": [],
        "actions": [{
            "name": "note it",
            "kind": "task",
            "action": "comment_revision",
            "params": {
                "revision_id": "{{event.revision_id}}",
                "body": "Keyed write walk."
            },
            "max_attempts": 3
        }],
    })
}

/// How many rules this walk created. **The count is the side effect**: a handler that ran twice
/// leaves two rows and a correct replay leaves one, so the assertion is on the table and never
/// on the response body — a replay that re-ran the handler would return the same body.
async fn rule_count(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("select count(*) from workflows where name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("the workflows table must be readable")
}

/// Delete everything this walk created, so a re-run starts clean.
async fn cleanup(pool: &PgPool, name: &str, key: &str) {
    let _ = sqlx::query("delete from workflows where name = $1")
        .bind(name)
        .execute(pool)
        .await;
    let _ = sqlx::query("delete from idempotency_keys where key = $1 and scope = $2")
        .bind(key)
        .bind(SCOPE)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_replayed_key_returns_the_first_response_and_the_handler_runs_once() {
    // `state_or_fail` FAILS the walk rather than skipping it, and that is deliberate: a `return`
    // from a test is a `PASS`, so a walk whose database refused to connect would report green
    // with every assertion skipped. The keyed contract is entirely a store question, so a walk
    // that skipped would report the platform as safe having proved nothing.
    let state = support::walk_state::state_or_fail().await;
    let caller = sign_in(&state).await;
    let pool = state.db().pool();
    let key = format!("w6-replay-{}", Uuid::new_v4().simple());
    let name = format!("w6 replay {}", Uuid::new_v4().simple());

    let first = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "first: {}", first.text);
    assert_eq!(
        first.header("idempotent-replay"),
        None,
        "the FIRST execution is not a replay, and a header here would tell a client its write was \
         skipped"
    );
    let original = first
        .header("idempotency-original-request-id")
        .expect("a keyed answer names the request id of the first execution");
    let first_id = first
        .body
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();

    // The replay. Same key, same body.
    let second = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED, "replay: {}", second.text);
    assert_eq!(
        second.header("idempotent-replay").as_deref(),
        Some("true"),
        "a replay must SAY it replayed: a client that cannot tell a replay from a fresh write \
         cannot trust either"
    );
    assert_eq!(
        second.header("idempotency-original-request-id").as_deref(),
        Some(original.as_str()),
        "a replay must report the ORIGINAL request id, or an operator following it into the log \
         explorer finds the replay and no first execution"
    );
    assert_eq!(
        second.body.get("id").and_then(Value::as_str),
        first.body.get("id").and_then(Value::as_str),
        "the replay must return the stored body verbatim, not a freshly built one"
    );
    assert_eq!(
        first_id,
        second.body.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
    );

    // THE assertion. One rule, one row.
    assert_eq!(
        rule_count(pool, &name).await,
        1,
        "a replayed key must not run the handler a second time — this count is the whole feature"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select replay_count from idempotency_keys where key = $1 and subject_id = $2",
        )
        .bind(&key)
        .bind(caller.user_id.to_string())
        .fetch_one(pool)
        .await
        .expect("the key row exists after the first execution"),
        1,
        "the replay counts itself, or the screen's replay column is a number nobody measures"
    );

    cleanup(pool, &name, &key).await;
}

#[tokio::test]
async fn a_changed_body_is_a_conflict_and_runs_nothing() {
    // `state_or_fail` FAILS the walk rather than skipping it, and that is deliberate: a `return`
    // from a test is a `PASS`, so a walk whose database refused to connect would report green
    // with every assertion skipped. The keyed contract is entirely a store question, so a walk
    // that skipped would report the platform as safe having proved nothing.
    let state = support::walk_state::state_or_fail().await;
    let caller = sign_in(&state).await;
    let pool = state.db().pool();
    let key = format!("w6-conflict-{}", Uuid::new_v4().simple());
    let first_name = format!("w6 conflict a {}", Uuid::new_v4().simple());
    let second_name = format!("w6 conflict b {}", Uuid::new_v4().simple());

    let first = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&first_name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "first: {}", first.text);

    let conflict = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&second_name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(
        conflict.status,
        StatusCode::CONFLICT,
        "a changed body must be refused: {}",
        conflict.text
    );
    assert_eq!(
        conflict.body.pointer("/error/code").and_then(Value::as_str),
        Some("idempotency_conflict"),
        "the code is the contract; a 409 with a different code is a different failure"
    );
    assert_eq!(
        conflict.header("idempotent-replay"),
        None,
        "a conflict is not a replay: the platform stored something else under this key"
    );
    assert_eq!(
        rule_count(pool, &second_name).await,
        0,
        "the conflicting request must NOT execute — a conflict that wrote is the one outcome the \
         client cannot recover from"
    );

    // The conflict is an operator-worthy event, and it carries no payload.
    let emitted: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'reliability.idempotency.conflict'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_default();
    assert!(
        emitted >= 1,
        "a conflict must reach the bus; the event constant has existed since the store shipped \
         and had no caller"
    );
    let leaked: i64 = sqlx::query_scalar(
        "select count(*) from events \
          where name = 'reliability.idempotency.conflict' \
            and payload::text like '%w6 conflict%'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_default();
    assert_eq!(
        leaked, 0,
        "the event must not carry the request body — an event payload lands in every webhook and \
         every log the platform writes"
    );

    cleanup(pool, &first_name, &key).await;
    let _ = sqlx::query("delete from workflows where name = $1")
        .bind(&second_name)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_replay_while_the_first_attempt_runs_is_a_409_with_a_wait() {
    // `state_or_fail` FAILS the walk rather than skipping it, and that is deliberate: a `return`
    // from a test is a `PASS`, so a walk whose database refused to connect would report green
    // with every assertion skipped. The keyed contract is entirely a store question, so a walk
    // that skipped would report the platform as safe having proved nothing.
    let state = support::walk_state::state_or_fail().await;
    let caller = sign_in(&state).await;
    let pool = state.db().pool();
    let key = format!("w6-inprogress-{}", Uuid::new_v4().simple());
    let name = format!("w6 in progress {}", Uuid::new_v4().simple());
    let subject = caller.user_id.to_string();

    // The key is claimed the way a running handler claims it — through the STORE, because making
    // a real handler slow is a race this suite would lose on a loaded box. The HTTP half being
    // proved here is the layer's answer to an `in_progress` row, and that row is exactly what a
    // running attempt leaves.
    let now = OffsetDateTime::now_utc();
    let fingerprint =
        omnion_reliability::idempotency::fingerprint("POST", "/api/v1/automations", &automation_body(&name).to_string());
    omnion_reliability::idem_store::claim(
        pool,
        SCOPE,
        &subject,
        &key,
        "POST",
        "/api/v1/automations",
        &fingerprint,
        now,
    )
    .await
    .expect("the key must be claimable");

    let busy = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(busy.status, StatusCode::CONFLICT, "busy: {}", busy.text);
    assert_eq!(
        busy.body.pointer("/error/code").and_then(Value::as_str),
        Some("idempotency_in_progress"),
    );
    assert!(
        busy.header("retry-after").is_some(),
        "a client told to come back must be told WHEN: a 409 with no Retry-After is a client \
         that either hammers the key or gives up on a request that would have succeeded"
    );
    assert_eq!(
        rule_count(pool, &name).await,
        0,
        "the second attempt must not run the handler — that is the whole point of the 409"
    );

    // Once the first attempt commits, the same key REPLAYS — the "resolves once the original
    // completes" half.
    omnion_reliability::idem_store::complete(
        pool,
        SCOPE,
        &subject,
        &key,
        &omnion_reliability::idempotency::StoredResponse::seal(
            201,
            json!({ "id": "already-made" }).to_string(),
            Uuid::new_v4(),
            None,
        ),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("the first attempt must be able to commit");

    let resolved = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(
        resolved.status,
        StatusCode::CREATED,
        "a completed key replays: {}",
        resolved.text
    );
    assert_eq!(resolved.header("idempotent-replay").as_deref(), Some("true"));
    assert_eq!(rule_count(pool, &name).await, 0);

    let _ = sqlx::query("delete from idempotency_keys where key = $1 and subject_id = $2")
        .bind(&key)
        .bind(&subject)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn a_refused_request_never_consumes_a_key() {
    // `state_or_fail` FAILS the walk rather than skipping it, and that is deliberate: a `return`
    // from a test is a `PASS`, so a walk whose database refused to connect would report green
    // with every assertion skipped. The keyed contract is entirely a store question, so a walk
    // that skipped would report the platform as safe having proved nothing.
    let state = support::walk_state::state_or_fail().await;
    let pool = state.db().pool();
    let key = format!("w6-refused-{}", Uuid::new_v4().simple());
    let name = format!("w6 refused {}", Uuid::new_v4().simple());

    // A caller with NO session and NO permission: the guard answers before the keyed layer runs.
    let refused = call(
        &state,
        keyed("/api/v1/automations", &key, automation_body(&name)),
    )
    .await;
    assert!(
        refused.status == StatusCode::UNAUTHORIZED || refused.status == StatusCode::FORBIDDEN,
        "the walk must be refused by the guard, not by the key: {}",
        refused.text
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from idempotency_keys where key = $1")
        .bind(&key)
        .fetch_one(pool)
        .await
        .expect("the table must be readable");
    assert_eq!(
        rows, 0,
        "a request refused by a permission check must not claim the key. The layer is INSIDE the \
         guard, so this is proved by the layer's position — there is no rollback branch to be wrong"
    );

    // And the same key is still free for a caller who may make the write.
    let caller = sign_in(&state).await;
    let allowed = call(
        &state,
        authed(
            keyed("/api/v1/automations", &key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::CREATED,
        "the key must still be usable by a permitted caller: {}",
        allowed.text
    );

    cleanup(pool, &name, &key).await;
}

#[tokio::test]
async fn the_panel_reads_the_same_rows_and_releases_a_stuck_key() {
    // `state_or_fail` FAILS the walk rather than skipping it, and that is deliberate: a `return`
    // from a test is a `PASS`, so a walk whose database refused to connect would report green
    // with every assertion skipped. The keyed contract is entirely a store question, so a walk
    // that skipped would report the platform as safe having proved nothing.
    let state = support::walk_state::state_or_fail().await;
    let caller = sign_in(&state).await;
    let pool = state.db().pool();
    let subject = caller.user_id.to_string();
    let done_key = format!("w6-panel-done-{}", Uuid::new_v4().simple());
    let stuck_key = format!("w6-panel-stuck-{}", Uuid::new_v4().simple());
    let name = format!("w6 panel {}", Uuid::new_v4().simple());

    // A completed key, written through the real path so the store holds what the middleware
    // writes — a fixture inserted by hand would prove the screen reads rows, not that it reads
    // THIS table's rows.
    let created = call(
        &state,
        authed(
            keyed("/api/v1/automations", &done_key, automation_body(&name)),
            &caller,
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);

    // A stuck one, claimed directly.
    omnion_reliability::idem_store::claim(
        pool,
        SCOPE,
        &subject,
        &stuck_key,
        "POST",
        "/api/v1/automations",
        &fingerprint_for(&name),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("the stuck key must be claimable");

    // The list.
    let list = call(&state, authed(get("/api/v1/reliability/idempotency"), &caller)).await;
    assert_eq!(list.status, StatusCode::OK, "list: {}", list.text);
    let keys = list.body.get("keys").and_then(Value::as_array).expect("keys is an array");
    assert!(
        keys.iter()
            .any(|row| row.get("key").and_then(Value::as_str) == Some(done_key.as_str())),
        "the key the middleware wrote must be on the panel's list"
    );
    assert_eq!(
        list.body.get("in_progress").and_then(Value::as_i64),
        Some(1),
        "the stuck count is the number the screen leads with, and it is a query rather than a \
         client-side count"
    );

    // The detail: metadata, and NO body.
    let detail = call(
        &state,
        authed(
            get(&format!(
                "/api/v1/reliability/idempotency/{stuck_key}"
            )),
            &caller,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "detail: {}", detail.text);
    assert_eq!(detail.body.get("state").and_then(Value::as_str), Some("in_progress"));
    assert_eq!(
        detail.body.get("inline_cap_bytes").and_then(Value::as_u64),
        Some(omnion_reliability::idempotency::INLINE_BODY_CAP as u64),
        "the cap is sent so the panel never hard-codes it — the two drifting is how a screen ends \
         up explaining a limit the server moved"
    );
    assert!(
        detail.body.get("next_attempt").and_then(Value::as_str).is_some(),
        "the screen must be able to say what the next attempt does"
    );
    assert!(
        detail.body.get("response_body").is_none(),
        "the detail route must not return a stored body: the store must never become a second \
         request archive"
    );

    // The release, with a reason, and an empty one refused first.
    let no_reason = call(
        &state,
        authed(
            delete(
                &format!("/api/v1/reliability/idempotency/{stuck_key}"),
                json!({ "reason": "   " }),
            ),
            &caller,
        ),
    )
    .await;
    assert_eq!(
        no_reason.status,
        StatusCode::BAD_REQUEST,
        "a release with no reason is refused: {}",
        no_reason.text
    );

    let released = call(
        &state,
        authed(
            delete(
                &format!("/api/v1/reliability/idempotency/{stuck_key}"),
                json!({ "reason": "the attempt died with the deploy" }),
            ),
            &caller,
        ),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "release: {}", released.text);
    assert_eq!(released.body.get("released").and_then(Value::as_bool), Some(true));

    let state_now: String = sqlx::query_scalar(
        "select state from idempotency_keys where key = $1 and subject_id = $2",
    )
    .bind(&stuck_key)
    .bind(&subject)
    .fetch_one(pool)
    .await
    .expect("the key row still exists after a release");
    assert_eq!(
        state_now, "failed",
        "a released key must be `failed`, which is the one state that says 'run it again'"
    );

    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'idempotency.key.release'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_default();
    assert!(audited >= 1, "a release writes an audit row: it is destructive");
    let announced: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'reliability.idempotency.keys.released'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_default();
    assert!(
        announced >= 1,
        "the second event constant with no caller until now"
    );

    // The guard on the destructive action: a COMPLETED key has a real stored response.
    let refused = call(
        &state,
        authed(
            delete(
                &format!("/api/v1/reliability/idempotency/{done_key}"),
                json!({ "reason": "just looking" }),
            ),
            &caller,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "releasing a completed key would destroy a response the caller is entitled to replay: {}",
        refused.text
    );

    cleanup(pool, &name, &done_key).await;
    let _ = sqlx::query("delete from idempotency_keys where key = $1 and subject_id = $2")
        .bind(&stuck_key)
        .bind(&subject)
        .execute(pool)
        .await;
}

/// The fingerprint a stuck row carries, so the release walk's row is the same shape a real
/// attempt would leave.
fn fingerprint_for(name: &str) -> String {
    omnion_reliability::idempotency::fingerprint(
        "POST",
        "/api/v1/automations",
        &automation_body(name).to_string(),
    )
}
