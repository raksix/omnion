//! The walk for "a deactivation says how many live sessions it ended, as a number" (REQ-065, slice
//! 4, acceptance criterion 8's reporting half).
//!
//! **`f0246f4` made the revocation real and put the count into a sentence.** The line read
//! `"… now reads disabled — 3 session(s) ended"`. That reads well, is worth nothing to the
//! platform, and cannot be consumed: `detail` is English prose, so it cannot be summed, filtered,
//! sorted, or rendered as a number, and every consumer that wants the figure has to re-parse a
//! sentence in the one language it was written in.
//!
//! **Why a re-parse is worse than no number at all.** Two consumers parse the same sentence two
//! ways and neither is caught, because both compile. The panel that renders the log shows the
//! prose, and the export that an offboarding review reads has to extract the figure — and the
//! export's regex is the one that quietly returns 0 the day somebody re-words the sentence, with
//! no type error, no test failure, and a security report that under-states a deprovisioning. A
//! number has no phrasing to drift.
//!
//! So this walk asserts the shape, not the wording: the same deactivation is reported as a JSON
//! **number**, the count equals the number of live sessions that were actually ended, and a
//! *create* — the write path that revokes nothing — reports `0` rather than omitting the field or
//! reporting `null`. The third clause matters as much as the first: a nullable column would mean
//! a client that reads `revoked_sessions` has to handle "unknown", and a client that does not
//! handle it renders a blank cell exactly where the reader is looking for a figure.
//!
//! It also pins the *sum*: a log of one create (0) and one deactivation (2 sessions) reports 2,
//! and summing the column gives the same answer as counting the dead session rows. Those are two
//! different sources of truth, and a screen that quotes one while the database has the other is
//! the exact failure this walk exists to prevent.
//!
//! Run it on a disposable database with
//! `bash scripts/qa/run-media-walk.sh iam_revocation_count`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::sessions;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

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

/// Which credential a call carries. Minting a provisioning token needs a signed-in *session*;
/// every SCIM call needs the provisioning *bearer*. Sending one as the other is a 401 that reads
/// like a bad token rather than a bad header.
enum Auth<'a> {
    /// A provisioning token.
    Bearer(&'a str),
    /// A signed-in session.
    Session(&'a str),
}

fn request(
    method: Method,
    uri: &str,
    auth: Option<Auth<'_>>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        builder = match auth {
            Auth::Bearer(token) => builder.header(header::AUTHORIZATION, format!("Bearer {token}")),
            Auth::Session(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        };
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
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

/// Read `revoked_sessions` as a **number**, failing loudly on anything else.
///
/// The type check is the assertion. `as_i64()` returns `None` for a JSON string just as it does
/// for a missing key, so a walk that only compared values would see `None` and the two very
/// different shapes — "the field is gone" and "the field is prose" — would need two separate
/// checks. `is_number()` first says *which* one happened.
fn revoked_count(entry: &Value, context: &str) -> i64 {
    let raw = entry
        .get("revoked_sessions")
        .unwrap_or_else(|| panic!("{context}: the log line must carry `revoked_sessions` at all"));
    assert!(
        raw.is_number(),
        "{context}: `revoked_sessions` must be a JSON number so it can be summed and sorted, \
         got {raw} — a caller cannot read a figure out of a sentence"
    );
    raw.as_i64().expect("a number that is an integer")
}

/// The walk. One test, because each step's state is the next step's precondition.
#[tokio::test]
async fn a_deactivation_reports_how_many_sessions_it_ended_as_a_number() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let organization_id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Revocation Count Walk Organization")
    .bind(format!("revo-{}-{}", Uuid::new_v4().simple(), "walk"))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");

    // ---- 1. An owner session, so a provisioning token can be minted ---------------------------
    let owner_email = format!("revo-owner-{}@omnion.test", Uuid::new_v4().simple());
    let owner = users::create_user(
        db.pool(),
        NewUser {
            email: owner_email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Revocation Count Owner".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the owner must be created");
    seed::bind_owner(db.pool(), owner.id)
        .await
        .expect("the owner binding must be created");

    let login = call(
        &state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": owner_email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "login: {}", login.body);
    let session_cookie = login
        .set_cookie
        .clone()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned();

    let minted = call(
        &state,
        request(
            Method::POST,
            "/api/v1/iam/provisioning/tokens",
            Some(Auth::Session(&session_cookie)),
            Some(json!({ "name": "revocation count walk" })),
        ),
    )
    .await;
    assert_eq!(minted.status, StatusCode::CREATED, "mint: {}", minted.body);
    let secret = minted
        .body
        .get("secret")
        .and_then(Value::as_str)
        .expect("mint must return the secret once")
        .to_owned();

    // ---- 2. Provision a user through the real SCIM route ---------------------------------------
    let subject_email = format!("count-{}@omnion.test", Uuid::new_v4().simple());
    let created = call(
        &state,
        request(
            Method::POST,
            "/api/v1/scim/v2/Users",
            Some(Auth::Bearer(&secret)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                "userName": subject_email,
                "externalId": "revo-1",
                "displayName": "Two Sessions Person",
                "active": true,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {}", created.body);
    let user_id = created.body["id"]
        .as_str()
        .expect("SCIM must return an id")
        .to_owned();
    let user_uuid = Uuid::parse_str(&user_id).expect("the SCIM id must parse as a UUID");

    // ---- 3. A create line reports 0, not null and not absent -----------------------------------
    // The create is the write path with nothing to revoke, and it is the *only* place the two
    // wrong designs are visible: a nullable column reports `null` here, and a client that skipped
    // the field reports nothing at all. Both would force every consumer to treat "this line
    // cannot revoke" as a third state next to 0 and N, and the panel would render a blank cell
    // for the most common kind of line on the page.
    let after_create = call(
        &state,
        request(
            Method::GET,
            "/api/v1/iam/provisioning/log?limit=50",
            Some(Auth::Session(&session_cookie)),
            None,
        ),
    )
    .await;
    assert_eq!(
        after_create.status,
        StatusCode::OK,
        "log: {}",
        after_create.body
    );
    let create_line = after_create.body["log"]
        .as_array()
        .expect("the log must be an array")
        .iter()
        // The bare action name, not `scim/create`: the `scim/` prefix is a *run-ledger* namespacing
        // applied in `touch_run` so a run can tell its own lines from anything else sharing the
        // table. The row itself stores what the operation was called, and asserting the prefixed
        // form here would be asserting a column that does not exist.
        .find(|entry| {
            entry["entity_id"] == json!(user_id) && entry["action"] == json!("create")
        })
        .expect("the create must have written a line")
        .clone();
    assert_eq!(
        revoked_count(&create_line, "a create"),
        0,
        "a create ends nothing and must say 0 — not null, not absent: a client reading this column \
         must not need a third state for 'this line cannot revoke'"
    );

    // ---- 4. Two live sessions, then a deactivation --------------------------------------------
    // Two, not one: a count of 1 is indistinguishable from a hard-coded 1, from a boolean
    // rendered as a digit, and from "the first session only". A figure that is right exactly when
    // it is 1 passes every check a single-session walk can make.
    let mut live_tokens = Vec::new();
    for _ in 0..2 {
        let (session, token) = sessions::create_session(db.pool(), user_uuid, None, None)
            .await
            .expect("a session must be created");
        assert!(
            sessions::resolve_session(db.pool(), &token)
                .await
                .expect("the resolver must run")
                .is_some(),
            "the session must work before the deactivation, or the count below proves nothing"
        );
        live_tokens.push((session.id, token));
    }

    let patched = call(
        &state,
        request(
            Method::PATCH,
            &format!("/api/v1/scim/v2/Users/{user_id}"),
            Some(Auth::Bearer(&secret)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "replace", "path": "active", "value": false }],
            })),
        ),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "patch: {}", patched.body);

    // ---- 5. The API reports the count as a number that equals the sessions actually ended ------
    let after_deactivate = call(
        &state,
        request(
            Method::GET,
            "/api/v1/iam/provisioning/log?limit=50",
            Some(Auth::Session(&session_cookie)),
            None,
        ),
    )
    .await;
    assert_eq!(
        after_deactivate.status,
        StatusCode::OK,
        "log: {}",
        after_deactivate.body
    );
    let log = after_deactivate.body["log"]
        .as_array()
        .expect("the log must be an array")
        .clone();

    let deactivate_line = log
        .iter()
        .find(|entry| {
            entry["entity_id"] == json!(user_id)
                && entry["outcome"] == json!("deactivated")
        })
        .expect("the deactivation must have written a line")
        .clone();

    assert_eq!(
        revoked_count(&deactivate_line, "a deactivation"),
        2,
        "two live sessions were ended and the line must say two — a count of 1 would also come from \
         a hard-coded 1, a boolean, or 'only the first session'"
    );

    // ---- 6. The number agrees with the database, which is a separate source of truth ----------
    // Summing the column and counting the dead session rows are two independent answers. A screen
    // that quotes the log while the database has the sessions is the failure this assertion
    // exists for, and the drift between them is invisible until somebody counts by hand.
    let total_reported: i64 = log
        .iter()
        .map(|entry| revoked_count(entry, "the whole page"))
        .sum();
    let dead_sessions: i64 = sqlx::query_scalar(
        "select count(*) from sessions where user_id = $1 and revoked_at is not null",
    )
    .bind(user_uuid)
    .fetch_one(db.pool())
    .await
    .expect("the session count must read");
    assert_eq!(
        total_reported, dead_sessions,
        "the log's own total must equal the number of sessions the database has revoked: a panel \
         that sums the column must not contradict the rows"
    );

    // ---- 7. Both tokens are genuinely dead ----------------------------------------------------
    for (session_id, token) in &live_tokens {
        assert!(
            sessions::resolve_session(db.pool(), token)
                .await
                .expect("the resolver must run")
                .is_none(),
            "a session the line says it ended must actually be unresolvable"
        );
        let revoked_at: Option<time::OffsetDateTime> =
            sqlx::query_scalar("select revoked_at from sessions where id = $1")
                .bind(*session_id)
                .fetch_one(db.pool())
                .await
                .expect("the session row must exist");
        assert!(
            revoked_at.is_some(),
            "the line reported 2 ended but session {session_id} is merely unresolvable: a status \
             filter hides it until the account is re-activated, and then the token works again"
        );
    }

    // ---- 8. The DELETE path reports it too -----------------------------------------------------
    // A connector that offboards somebody sends `DELETE /Users/{id}` last, so the PATCH path above
    // is not the only way a deactivation is recorded — and `0126` gives the two paths one column,
    // so a count that only the PATCH path fills leaves DELETE as the one line an offboarding
    // review cannot read a figure from.
    users::set_status(db.pool(), user_uuid, "active")
        .await
        .expect("the account must be re-activated")
        .expect("the account must still exist");
    let (_, third_token) = sessions::create_session(db.pool(), user_uuid, None, None)
        .await
        .expect("a session must be created");
    assert!(
        sessions::resolve_session(db.pool(), &third_token)
            .await
            .expect("the resolver must run")
            .is_some(),
        "the third session must work while the account is active"
    );

    let deleted = call(
        &state,
        request(
            Method::DELETE,
            &format!("/api/v1/scim/v2/Users/{user_id}"),
            Some(Auth::Bearer(&secret)),
            None,
        ),
    )
    .await;
    // 204, not 200: a SCIM `DELETE` deactivates rather than removes (the account stays so the
    // account keeps its history), so there is no body to describe. The code is asserted rather
    // than written as `is_success` so that a later change to 200-with-a-body is a visible change
    // to the *contract* rather than something this walk quietly stops caring about.
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "a SCIM delete deactivates and answers 204 with no body"
    );

    let after_delete = call(
        &state,
        request(
            Method::GET,
            "/api/v1/iam/provisioning/log?limit=50",
            Some(Auth::Session(&session_cookie)),
            None,
        ),
    )
    .await;
    assert_eq!(
        after_delete.status,
        StatusCode::OK,
        "log: {}",
        after_delete.body
    );
    let delete_line = after_delete.body["log"]
        .as_array()
        .expect("the log must be an array")
        .iter()
        .find(|entry| {
            entry["entity_id"] == json!(user_id) && entry["action"] == json!("deactivate")
        })
        .expect("the DELETE must have written a line")
        .clone();
    assert_eq!(
        revoked_count(&delete_line, "a DELETE deactivation"),
        1,
        "DELETE is how a connector offboards somebody, and it must report the figure the same way \
         PATCH does — one live session went with it"
    );

    // ---- cleanup -------------------------------------------------------------------------------
    sqlx::query("delete from sessions where user_id = $1")
        .bind(user_uuid)
        .execute(db.pool())
        .await
        .expect("session cleanup must run");
    sqlx::query("delete from provisioning_log where organization_id = $1")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .expect("sync-log cleanup must run");
    sqlx::query("delete from users where id = any($1)")
        .bind(vec![owner.id, user_uuid])
        .execute(db.pool())
        .await
        .expect("user cleanup must run");
    sqlx::query("delete from organizations where id = $1")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .expect("organization cleanup must run");
}
