//! The walk for "an account disabled in the directory is refused at the next sync and its
//! active sessions are revoked" (docs/requests/REQ-065, acceptance criterion 8).
//!
//! **The criterion was half absent rather than unproven.** The *first* half was true: SCIM wrote
//! `status = 'disabled'`, and `resolve_session` filters on `u.status = 'active'`, so the account
//! was refused. The *second* half did not exist. `revoke_sessions_for_user` had been in
//! `crates/identity/src/sessions.rs` since the module was written, documented "used by password
//! resets and admin actions", and had **zero callers** anywhere in the workspace. Nothing revoked
//! a session when a directory said an account was gone.
//!
//! **Why that reads as green.** Every existing assertion stops at "the session does not work" —
//! which it did, because the status filter masks it. The failure is one step further out, and it
//! is the step a real deployment takes: a directory deactivates somebody who left, and weeks later
//! a re-sync notices the account reappeared (a stale row, a re-hire, a connector that flipped the
//! flag back) and re-activates it. `status` is `active` again, the session row was never touched,
//! and every old browser tab holds a working token again — with its original expiry, so it can
//! last months. A status flag is a *query filter*; a session token is a *bearer credential* the
//! user already holds. Only revoking the session makes it dead.
//!
//! So this walk asserts the claim that only the fixed code can pass: **deactivate, reactivate,
//! and the old session is still dead** — checked by presenting the real token to the real
//! resolver, after first asserting that same token worked a moment earlier.
//!
//! It runs against the development stack and skips with a printed reason when PostgreSQL is not
//! reachable. Run it on a disposable database with
//! `bash scripts/qa/run-media-walk.sh iam_deprovision_revocation`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_identity::{sessions, users as users_mod};
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

/// Which credential a call carries. The two are different tokens for different surfaces —
/// minting a provisioning token needs a signed-in *session*, every SCIM call needs the
/// provisioning *bearer* — and sending one as the other is a 401 that reads like a bad token
/// rather than a bad header.
enum Auth<'a> {
    /// A provisioning token, as `Authorization: Bearer`.
    Bearer(&'a str),
    /// A signed-in session, as the `omnion_session` cookie.
    Session(&'a str),
}

fn request(method: Method, uri: &str, auth: Option<Auth<'_>>, body: Option<Value>) -> Request<Body> {
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

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
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

/// The walk. One test, because each step's state is the next step's precondition.
#[tokio::test]
async fn a_deprovisioned_account_keeps_no_working_session() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let organization_id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Deprovision Walk Organization")
    .bind(format!("deprov-{}", Uuid::new_v4().simple()))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");

    // ---- 1. Mint a provisioning token so the deactivation goes through the real SCIM route -----
    let owner_email = format!("deprov-owner-{}@omnion.test", Uuid::new_v4().simple());
    let owner = users::create_user(
        db.pool(),
        NewUser {
            email: owner_email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Deprovision Owner".to_owned(),
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
            Some(json!({ "name": "deprovision walk" })),
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

    // ---- 2. Provision somebody through SCIM, then give them a live session --------------------
    let subject_email = format!("leaver-{}@omnion.test", Uuid::new_v4().simple());
    let created = call(
        &state,
        request(
            Method::POST,
            "/api/v1/scim/v2/Users",
            Some(Auth::Bearer(&secret)),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                "userName": subject_email,
                "externalId": "deprov-1",
                "displayName": "Somebody Who Left",
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

    // A live session for the provisioned account. A SCIM-provisioned account has no password of
    // its own, so the session is opened through the store directly — which is exactly what a real
    // sign-in does after the callback, and keeps this walk about the *revocation* rather than
    // about which door minted the session.
    let (session, session_token) = sessions::create_session(db.pool(), user_uuid, None, None)
        .await
        .expect("a session must be created");
    let session_id = session.id;

    // The session resolves NOW, asserted before the deactivation: a test that only checks the
    // session is dead afterwards passes against a session that never worked.
    assert!(
        sessions::resolve_session(db.pool(), &session_token)
            .await
            .expect("the resolver must run")
            .is_some(),
        "the session must work before the deactivation, or the assertions after it prove nothing"
    );

    // ---- 3. The directory deactivates the account (PATCH active=false) ------------------------
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
    assert_eq!(patched.body["active"], json!(false));

    let stored: String = sqlx::query_scalar("select status from users where id = $1")
        .bind(user_uuid)
        .fetch_one(db.pool())
        .await
        .expect("the account must still exist");
    assert_eq!(
        stored, "disabled",
        "deactivation disables, never deletes"
    );

    // The account is refused at the next request — the half that already worked.
    assert!(
        sessions::resolve_session(db.pool(), &session_token)
            .await
            .expect("the resolver must run")
            .is_none(),
        "a deactivated account's session must not resolve"
    );

    // ---- 4. The session was ENDED, not merely masked -------------------------------------------
    // This is the assertion the old code cannot pass. `revoke_reason` is written by
    // `sign_out_all`/`set_status_and_end_sessions` and by nothing else; a row whose session is
    // merely filtered out by the status check still reads `revoke_reason = null`.
    let (revoked_at, reason): (Option<time::OffsetDateTime>, Option<String>) = sqlx::query_as(
        "select revoked_at, revoke_reason from sessions where id = $1",
    )
    .bind(session_id)
    .fetch_one(db.pool())
    .await
    .expect("the session row must exist");
    assert!(
        revoked_at.is_some(),
        "the session must be REVOKED, not merely unresolvable: a status filter hides it until the \
         account is re-activated, and then the token works again with its original expiry"
    );
    assert_eq!(
        reason.as_deref(),
        Some("scim_deactivated"),
        "the reason says a directory took this person out of service, which is what an operator \
         reading the session list needs to see"
    );

    // ---- 5. Re-activation must NOT resurrect it -------------------------------------------------
    // The clause that only the fix satisfies: a re-sync that flips the account back to active
    // must not hand the old browser tabs a working session. On the old code the account reads
    // `active` and the untouched session resolves — a departed employee's access returning.
    users_mod::set_status(db.pool(), user_uuid, "active")
        .await
        .expect("the account must be re-activated")
        .expect("the account must still exist");

    assert!(
        sessions::resolve_session(db.pool(), &session_token)
            .await
            .expect("the resolver must run")
            .is_none(),
        "re-activating the account must NOT bring a pre-deactivation session back: this is the \
         exact hole — the token is still in the browser, the status filter no longer masks it, \
         and it carries its original expiry"
    );

    // ---- 6. The count is in the sync log, because the account row cannot say it ----------------
    let detail: String = sqlx::query_scalar(
        "select detail from provisioning_log \
         where organization_id = $1 and entity_id = $2 order by created_at desc limit 1",
    )
    .bind(organization_id)
    .bind(user_uuid)
    .fetch_one(db.pool())
    .await
    .expect("a sync-log line must exist");
    assert!(
        detail.contains("session"),
        "the sync log must say how many sessions ended: {detail:?} — \"deactivated\" alone cannot \
         tell an operator that three live tokens went with the account"
    );

    // ---- 7. A no-op deactivation revokes nothing -------------------------------------------------
    // A connector re-sends the same document on its timer. Setting `disabled` on an already
    // disabled account must NOT end whatever sessions have appeared since — the operator would be
    // signing a colleague out every few minutes with no action of their own.
    users_mod::set_status(db.pool(), user_uuid, "active")
        .await
        .expect("re-activation must run");

    let (_, second_token) = sessions::create_session(db.pool(), user_uuid, None, None)
        .await
        .expect("a session must be created");
    assert!(
        sessions::resolve_session(db.pool(), &second_token)
            .await
            .expect("the resolver must run")
            .is_some(),
        "the second session must work while the account is active, or the no-op case below would \
         pass against a session that never worked"
    );

    // The first deactivation is a real change and ends the session it finds.
    let (_, first) =
        users_mod::set_status_and_end_sessions(db.pool(), user_uuid, "disabled", "scim_deactivated")
            .await
            .expect("the call must run")
            .expect("the account must exist");
    assert_eq!(first.revoked_sessions, 1, "one live session, one revoked");

    // Now the no-op: the account already reads `disabled`, so this must end nothing.
    let (_, second) =
        users_mod::set_status_and_end_sessions(db.pool(), user_uuid, "disabled", "scim_deactivated")
            .await
            .expect("the call must run")
            .expect("the account must exist");
    assert_eq!(
        second.revoked_sessions, 0,
        "deactivating an ALREADY disabled account must end nothing: a connector that re-sends the \
         same document on a timer would otherwise sign a colleague out every few minutes"
    );

    // And the session ended by the first call stays ended, whatever the no-op did. A re-activation
    // brings the account back without bringing the token back with it.
    users_mod::set_status(db.pool(), user_uuid, "active")
        .await
        .expect("re-activation must run");
    assert!(
        sessions::resolve_session(db.pool(), &second_token)
            .await
            .expect("the resolver must run")
            .is_none(),
        "the session ended by the first deactivation must stay ended across any number of later \
         no-op deactivations and re-activations"
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
