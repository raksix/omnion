//! Integration tests for folder and file grants (REQ-010, slice 4).
//!
//! These walk the **real router against a real PostgreSQL**, because the rule under test is
//! about *ordering* — a deny on a file against an allow inherited from four folders up — and
//! ordering is exactly what a unit test over a hand-built chain gets right by construction.
//! The chain in production is read from rows, through a folder walk, in a statement per level;
//! a fixture that skips that walk tests the resolver and not the platform.
//!
//! Every assertion that concerns a row is made **against the row read out of PostgreSQL**. A
//! response that quietly omits a field is indistinguishable from one that stored it and
//! chose not to say so, and this table's whole job is to be believed.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::{Storage, StorageError};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the editor of this suite holds.
///
/// `media.manage` and `media.share` are both here on purpose: the whole point is that the
/// *route guard* passes and the *grant chain* still refuses, so a test that dropped them would
/// be refused by the guard and would prove nothing about grants.
const EDITOR_PERMISSIONS: [&str; 7] = [
    "media.read",
    "media.upload",
    "media.update",
    "media.delete",
    "media.manage",
    "media.share",
    "media.settings.manage",
];

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    raw: Vec<u8>,
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
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let raw = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes()
        .to_vec();
    let body = match content_type.as_deref() {
        Some(value) if value.starts_with("application/json") && !raw.is_empty() => {
            serde_json::from_slice(&raw).expect("a JSON body must parse")
        }
        _ => Value::Null,
    };

    TestResponse {
        status,
        set_cookie,
        body,
        raw,
    }
}

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
}

/// Open the object store the library writes into; `None` means it is not running.
async fn live_storage() -> Option<Storage> {
    let storage = Storage::from_env().expect("the storage configuration must be valid");
    match storage.probe().await {
        Ok(()) | Err(StorageError::NotFound { .. }) => Some(storage),
        Err(err) => {
            eprintln!("SKIP: the object store is not reachable ({err})");
            None
        }
    }
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    let mut config = config.clone();
    config.database.max_connections = 2;
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

/// A state whose database has all migrations applied and the object store open.
async fn live_state() -> Option<(AppState, Db, Storage)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
    db.migrate().await.expect("migrations must apply");
    let storage = live_storage().await?;
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        storage.clone(),
    );
    Some((state, db, storage))
}

/// One account plus its id and email.
struct Account {
    id: Uuid,
    email: String,
}

/// Create an account in an organization.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> Account {
    let email = format!("grant-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Grant Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    Account {
        id: user.id,
        email,
    }
}

/// Create a role, write its permissions and bind it to one account at organization scope.
async fn account_with(
    db: &Db,
    org: Uuid,
    granted_by: Uuid,
    permissions: &[&str],
    label: &str,
) -> Account {
    let account = create_account(db, Some(org)).await;
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id: org,
            key: format!("grant-{label}-{}", Uuid::new_v4().simple()),
            name: format!("Grant {label}"),
            description: "Drives the grant walks".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = permissions
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permissions must be written");
    bindings::grant(
        db.pool(),
        NewBinding {
            role_id: role.id,
            user_id: account.id,
            scope: Scope::Organization {
                organization_id: org,
            },
            granted_by: Some(granted_by),
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
    account
}

/// Sign in and return the session token.
async fn login(state: &AppState, email: &str) -> String {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
    response
        .set_cookie
        .clone()
        .expect("login must set the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
        .split_once('=')
        .expect("cookie is name=value")
        .1
        .to_owned()
}

/// Upload one file through the real upload route, as multipart.
async fn upload(state: &AppState, token: &str, site: Uuid, filename: &str, body: &[u8]) -> Uuid {
    let boundary = format!("----omnion{}", Uuid::new_v4().simple());
    let mut parts = Vec::new();
    parts.push(format!("--{boundary}\r\n").into_bytes());
    parts.push(
        format!(
            "content-disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             content-type: text/plain\r\n\r\n"
        )
        .into_bytes(),
    );
    parts.push(body.to_vec());
    parts.push(format!("\r\n--{boundary}--\r\n").into_bytes());

    let response = call(
        state,
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/media?site_id={site}"))
            .header(header::COOKIE, format!("omnion_session={token}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(parts.concat()))
            .expect("request must build"),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the upload must succeed: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// Create a folder and return its id.
async fn folder(state: &AppState, token: &str, site: Uuid, name: &str, parent: Option<Uuid>) -> Uuid {
    let mut body = json!({ "name": name, "site_id": site });
    if let Some(parent) = parent {
        body["parent_id"] = json!(parent);
    }
    let response = call(
        state,
        request(Method::POST, "/api/v1/media/folders", Some(token), Some(body)),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the folder must be created: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// Write a grant through the real route.
async fn put_grant(
    state: &AppState,
    token: &str,
    path: &str,
    body: Value,
) -> TestResponse {
    call(
        state,
        request(Method::PUT, path, Some(token), Some(body)),
    )
    .await
}

/// The fixture every walk shares: an organization, a site, an editor, a reader and
/// a stranger, plus the tokens to act as each of them.
struct Fixture {
    state: AppState,
    db: Db,
    storage: Storage,
    org: Uuid,
    site: Uuid,
    platform_id: Uuid,
    editor: Account,
    reader: Account,
    /// A second editor — the account grants are written *about*, which is the whole point: the
    /// caller changing a grant and the caller affected by it are different people.
    subject: Account,
    editor_token: String,
    reader_token: String,
    subject_token: String,
}

async fn fixture() -> Option<Fixture> {
    let (state, db, storage) = live_state().await?;
    // The base roles have to exist before `bind_owner` can find one — the seed is what creates
    // them, and a walk that forgets it fails on `RoleNotFound` at the first line of the fixture,
    // which reads as a grants problem and is not one.
    seed::ensure(db.pool())
        .await
        .expect("the IAM seed must run");

    let org = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Grant Test")
    .bind(format!("media-grant-{}", Uuid::new_v4().simple()))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created");

    let site = sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
    )
    .bind(org)
    .bind("Grant Site")
    .fetch_one(db.pool())
    .await
    .expect("the site must be created");

    let platform_id = create_account(&db, None).await.id;
    seed::bind_owner(db.pool(), platform_id)
        .await
        .expect("the owner binding must be created");

    let editor = account_with(&db, org, platform_id, &EDITOR_PERMISSIONS, "editor").await;
    let reader = account_with(&db, org, platform_id, &["media.read"], "reader").await;
    let subject = account_with(&db, org, platform_id, &EDITOR_PERMISSIONS, "subject").await;

    // The editor is the account that drives the walks: it holds every media key, which is the
    // point — a grant has to be *narrowing* against a caller the catalogue already allows
    // everything, or the test would pass against a route guard alone.
    let editor_token = login(&state, &editor.email).await;
    let reader_token = login(&state, &reader.email).await;
    let subject_token = login(&state, &subject.email).await;

    Some(Fixture {
        state,
        db,
        storage,
        org,
        site,
        platform_id,
        editor,
        reader,
        subject,
        editor_token,
        reader_token,
        subject_token,
    })
}

/// Read a grant row out of PostgreSQL rather than trusting a response.
async fn grant_row(db: &Db, id: Uuid) -> (bool, bool, bool, bool, String) {
    sqlx::query_as::<_, (bool, bool, bool, bool, String)>(
        "select can_read, can_write, can_delete, can_share, effect \
         from media_grants where id = $1",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .expect("the grant row must be readable")
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The headline rule: a `deny` on a file survives an `allow` inherited from four folders up.
///
/// The folders are built one inside the next rather than four siblings, because the answer
/// only differs from "nearest wins" when the allow and the deny are *on the same path* — a
/// deny on one branch and an allow on another is a different question, and a test that only
/// used that shape would pass an implementation that got the ordering wrong.
#[tokio::test]
async fn a_deny_on_a_file_beats_an_allow_inherited_from_four_folders_up() {
    let Some(fx) = fixture().await else {
        return;
    };

    // root → campaigns → 2026 → drafts → launch
    let root = folder(&fx.state, &fx.editor_token, fx.site, "root", None).await;
    let one = folder(&fx.state, &fx.editor_token, fx.site, "campaigns", Some(root)).await;
    let two = folder(&fx.state, &fx.editor_token, fx.site, "2026", Some(one)).await;
    let three = folder(&fx.state, &fx.editor_token, fx.site, "drafts", Some(two)).await;
    let four = folder(&fx.state, &fx.editor_token, fx.site, "launch", Some(three)).await;

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "brief.txt",
        b"the unreleased launch brief",
    )
    .await;

    // Move it to the deepest folder so the chain is four long.
    let moved = call(
        &fx.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/files/{file}"),
            Some(&fx.editor_token),
            Some(json!({ "folder_id": four })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    // The allow sits on the *root*, four levels up: exactly the "grant everybody read on the
    // whole library" row an organization creates without thinking.
    let allowed = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/folders/{root}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "allow",
        }),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);

    // Before the deny: the subject can read it, over the inherited allow.
    let before = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        before.status,
        StatusCode::OK,
        "the inherited allow must work before the deny: {}",
        String::from_utf8_lossy(&before.raw)
    );

    // The deny is on the file itself.
    let denied = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);

    // After: refused on the raw route, with a code that names the rule.
    let after = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::FORBIDDEN,
        "a file deny must survive an allow four levels up: {}",
        after.body
    );
    assert_eq!(after.body["error"]["code"], "media_grant_denied");

    // The *other* editor is untouched: a deny names one subject and nobody else.
    let bystander = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        bystander.status,
        StatusCode::OK,
        "a deny for one subject must not refuse another: {}",
        String::from_utf8_lossy(&bystander.raw)
    );

    // And the debug endpoint says *where* the deny was found, so an operator can point at it.
    let effective = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grant-effective"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(effective.status, StatusCode::OK, "{}", effective.body);
    assert_eq!(effective.body["touched"], true);
    assert!(
        effective.body["effective"].as_array().expect("a list").is_empty(),
        "the answer must be nothing: {}",
        effective.body
    );
    assert!(
        effective.body["reason"]
            .as_str()
            .expect("a sentence")
            .contains("the file"),
        "the sentence must name where the deny was: {}",
        effective.body["reason"]
    );
}

/// A deny that removes only `share` leaves reading alone, and a `share` link is refused.
#[tokio::test]
async fn a_deny_that_removes_only_share_leaves_reading_alone() {
    let Some(fx) = fixture().await else {
        return;
    };

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "handout.txt",
        b"a file somebody may read but not hand out",
    )
    .await;

    // The subject's folder carries the deny; the file carries the allow. **Different nodes**,
    // because there is one row per (node, subject): a second row on the same file would
    // overwrite the first, and the walk would then be asserting that a replacement preserved
    // two contradictory rows. This is the shape the spec means by "a deny that removes only
    // share" — the granularity is per capability, and the granularity of the *rows* is per node.
    // A freshly uploaded file has **no folder** — the library root is materialised on the
    // first read of the tree, not by the upload. The walk therefore creates a folder and moves
    // the file into it, rather than reading a column that is legitimately null. Reading it
    // anyway is how a walk starts asserting on a shape the platform does not promise.
    let holder = folder(&fx.state, &fx.editor_token, fx.site, "brand-book", None).await;
    let moved = call(
        &fx.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/files/{file}"),
            Some(&fx.editor_token),
            Some(json!({ "folder_id": holder })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    let folder_of_file: Uuid = sqlx::query_scalar("select folder_id from media where id = $1")
        .bind(file)
        .fetch_one(fx.db.pool())
        .await
        .expect("the file's folder must be readable after the move");

    let read_allowed = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "allow",
        }),
    )
    .await;
    assert_eq!(read_allowed.status, StatusCode::OK, "{}", read_allowed.body);

    let share_denied = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/folders/{folder_of_file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_share": true,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(share_denied.status, StatusCode::OK, "{}", share_denied.body);

    // Both rows, read out of PostgreSQL: the file's allow says read, the folder's deny says
    // share. A response that hid one of them would still look right here.
    let (can_read, can_write, can_delete, can_share, effect) = grant_row(
        &fx.db,
        Uuid::parse_str(read_allowed.body["id"].as_str().unwrap()).unwrap(),
    )
    .await;
    assert!(can_read, "the file's allow grants read");
    assert!(!can_write && !can_delete && !can_share);
    assert_eq!(effect, "allow");
    let (dc_read, dc_write, dc_delete, dc_share, dc_effect) = grant_row(
        &fx.db,
        Uuid::parse_str(share_denied.body["id"].as_str().unwrap()).unwrap(),
    )
    .await;
    assert!(!dc_read && !dc_write && !dc_delete, "the deny names only share");
    assert!(dc_share);
    assert_eq!(dc_effect, "deny");

    // Reading still works: the deny only removed `share`.
    let raw = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        raw.status,
        StatusCode::OK,
        "removing share must not remove read: {}",
        String::from_utf8_lossy(&raw.raw)
    );

    // Sharing is refused, and the refusal is the grant's, not the route guard's: the subject
    // holds `media.share` in the catalogue, so without the chain this would have been allowed.
    let share = call(
        &fx.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{file}/shares"),
            Some(&fx.subject_token),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        share.status,
        StatusCode::FORBIDDEN,
        "a write-capable editor may not share what the chain took away: {}",
        share.body
    );
    assert_eq!(share.body["error"]["code"], "media_grant_denied");
    assert!(
        share.body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("may not share"),
        "{}",
        share.body
    );
}

/// A group grant reaches every member and nobody else.
#[tokio::test]
async fn a_group_grant_reaches_its_members_and_nobody_else() {
    let Some(fx) = fixture().await else {
        return;
    };

    let group: Uuid = sqlx::query_scalar(
        "insert into groups (organization_id, name, slug) values ($1, $2, $3) returning id",
    )
    .bind(fx.org)
    .bind("Launch Team")
    .bind(format!("launch-{}", Uuid::new_v4().simple()))
    .fetch_one(fx.db.pool())
    .await
    .expect("the group must be created");
    sqlx::query("insert into group_members (group_id, user_id) values ($1, $2)")
        .bind(group)
        .bind(fx.subject.id)
        .execute(fx.db.pool())
        .await
        .expect("the membership must be created");

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "team.txt",
        b"for the team only",
    )
    .await;

    let denied = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "group",
            "subject_id": group,
            "can_read": true,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);

    // The member is refused.
    let member = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        member.status,
        StatusCode::FORBIDDEN,
        "a group deny reaches its members: {}",
        member.body
    );

    // The other editor is not, and the list resolves the group's name rather than a uuid.
    let other = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grants"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    let first = &other.body["grants"][0];
    assert_eq!(first["subject_kind"], "group");
    assert_eq!(
        first["subject_label"], "Launch Team",
        "the list must resolve the subject's name, not print a uuid: {}",
        other.body
    );

    // And membership is what changed the answer: taking the member out of the team restores it
    // on the very next request, because nothing caches.
    sqlx::query("delete from group_members where group_id = $1 and user_id = $2")
        .bind(group)
        .bind(fx.subject.id)
        .execute(fx.db.pool())
        .await
        .expect("the membership must be removed");
    let after = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "leaving the team must take effect on the next request: {}",
        after.body
    );
}

/// A library with no grants at all must serve every file — the "silence is the catalogue's
/// answer" rule, proved through the router rather than through the resolver.
#[tokio::test]
async fn a_library_with_no_grants_serves_every_file() {
    let Some(fx) = fixture().await else {
        return;
    };

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "open.txt",
        b"nobody narrowed anything here",
    )
    .await;

    let raw = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        raw.status,
        StatusCode::OK,
        "an untouched chain must leave the catalogue's answer standing: {}",
        String::from_utf8_lossy(&raw.raw)
    );
    assert_eq!(raw.raw, b"nobody narrowed anything here");

    // The debug endpoint agrees: nothing was touched, and the catalogue alone answers.
    let effective = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grant-effective"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(effective.status, StatusCode::OK, "{}", effective.body);
    assert_eq!(effective.body["touched"], false);
    assert_eq!(
        effective.body["removed"].as_array().expect("a list").len(),
        0,
        "nothing was removed: {}",
        effective.body
    );
    let kept = effective.body["effective"].as_array().expect("a list");
    assert!(kept.iter().any(|value| value == "read"), "{:?}", kept);
}

/// A grant on the root folder reaches a file in a subfolder, and the chain reports it.
#[tokio::test]
async fn a_folder_grant_reaches_the_files_beneath_it_and_the_chain_shows_where() {
    let Some(fx) = fixture().await else {
        return;
    };

    let root = folder(&fx.state, &fx.editor_token, fx.site, "root", None).await;
    let child = folder(&fx.state, &fx.editor_token, fx.site, "child", Some(root)).await;

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "inherited.txt",
        b"two folders down",
    )
    .await;
    let moved = call(
        &fx.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/files/{file}"),
            Some(&fx.editor_token),
            Some(json!({ "folder_id": child })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);

    let denied = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/folders/{root}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);

    // A folder deny reaches the file inside it.
    let raw = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        raw.status,
        StatusCode::FORBIDDEN,
        "a folder deny reaches the files beneath it: {}",
        raw.body
    );

    // The file's grant tab shows the chain it inherits from, so the operator can see which
    // folder did it — nearest first.
    let tab = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grants"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(tab.status, StatusCode::OK, "{}", tab.body);
    assert_eq!(tab.body["target_kind"], "file");
    // Three nodes, not two: the library root is materialised on first use, so every file's
    // chain ends there. Asserting two would have been a test that passed for the wrong reason
    // on a site whose root happened to be the folder we called "root".
    let chain = tab.body["chain"].as_array().expect("a chain");
    assert_eq!(chain.len(), 3, "child, root, library root: {}", tab.body);
    assert_eq!(chain[0]["id"].as_str().expect("an id"), child.to_string());
    assert_eq!(chain[1]["id"].as_str().expect("an id"), root.to_string());
    assert_eq!(chain[0]["has_deny"], false, "the child carries nothing");
    assert_eq!(
        chain[1]["has_deny"], true,
        "the folder we denied on carries the deny: {}",
        tab.body
    );
    assert_eq!(chain[2]["has_deny"], false, "the library root carries nothing");
    assert_eq!(tab.body["inherits"], false, "a file has no inheritance of its own");

    // The folder's own tab says it does inherit, and names the state.
    let folder_tab = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/folders/{root}/grants"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(folder_tab.status, StatusCode::OK, "{}", folder_tab.body);
    assert_eq!(folder_tab.body["inherits"], true);
    assert_eq!(folder_tab.body["grants"][0]["effect"], "deny");
}

/// The refusals: another tenant's node is a `404`, a reader may not write, and a deny with no
/// bit is refused rather than stored.
#[tokio::test]
async fn the_refusals_are_the_ones_the_screen_needs() {
    let Some(fx) = fixture().await else {
        return;
    };

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "refusals.txt",
        b"nothing here is readable by a reader's pen",
    )
    .await;

    // A reader may read the grant table and may not write it.
    let read_tab = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grants"),
            Some(&fx.reader_token),
            None,
        ),
    )
    .await;
    assert_eq!(read_tab.status, StatusCode::OK, "{}", read_tab.body);

    let reader_write = put_grant(
        &fx.state,
        &fx.reader_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.reader.id,
            "can_read": true,
            "effect": "allow",
        }),
    )
    .await;
    assert_eq!(
        reader_write.status,
        StatusCode::FORBIDDEN,
        "a reader may not write a grant: {}",
        reader_write.body
    );

    // Anonymous is refused on both.
    let anonymous_read = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/grants"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(anonymous_read.status, StatusCode::UNAUTHORIZED);
    let anonymous_write = call(
        &fx.state,
        request(
            Method::PUT,
            &format!("/api/v1/media/{file}/grants"),
            None,
            Some(json!({ "subject_kind": "user", "subject_id": fx.reader.id })),
        ),
    )
    .await;
    assert_eq!(anonymous_write.status, StatusCode::UNAUTHORIZED);

    // A deny with no bit names the field and writes nothing.
    let empty_deny = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(empty_deny.status, StatusCode::BAD_REQUEST, "{}", empty_deny.body);
    assert_eq!(empty_deny.body["error"]["code"], "effect");
    let stored: i64 =
        sqlx::query_scalar("select count(*) from media_grants where media_id = $1")
            .bind(file)
            .fetch_one(fx.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(stored, 0, "a refused grant must not be written");

    // An unknown subject kind names the field and the three legal values.
    let bad_kind = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "team",
            "subject_id": fx.subject.id,
            "can_read": true,
        }),
    )
    .await;
    assert_eq!(bad_kind.status, StatusCode::BAD_REQUEST, "{}", bad_kind.body);
    assert_eq!(bad_kind.body["error"]["code"], "subject_kind");
    let message = bad_kind.body["error"]["message"].as_str().expect("a message");
    assert!(message.contains("user") && message.contains("group") && message.contains("role"));

    // Another tenant's *site* is refused by the tenancy layer with `cross_organization` before
    // the node is ever read. That is a different question from "is this grant id real", which
    // is the one the `404` rule is about — and it is the answer the rest of the API gives for
    // an out-of-scope site, so a grants route that answered `404` here would be the odd one.
    let other_org = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Elsewhere")
    .bind(format!("elsewhere-{}", Uuid::new_v4().simple()))
    .fetch_one(fx.db.pool())
    .await
    .expect("the organization must be created");
    let other_site = sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
    )
    .bind(other_org)
    .bind("Elsewhere Site")
    .fetch_one(fx.db.pool())
    .await
    .expect("the site must be created");
    // Uploaded by an account of *their* organization: our editor is correctly refused by the
    // tenancy check, which is the first half of what the next assertion proves.
    let theirs = account_with(
        &fx.db,
        other_org,
        fx.platform_id,
        &EDITOR_PERMISSIONS,
        "elsewhere",
    )
    .await;
    let other_file = upload(
        &fx.state,
        &login(&fx.state, &theirs.email).await,
        other_site,
        "theirs.txt",
        b"another tenant's bytes",
    )
    .await;
    let cross = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{other_file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "allow",
        }),
    )
    .await;
    assert_eq!(
        cross.status,
        StatusCode::FORBIDDEN,
        "another tenant's site is refused by the tenancy layer: {}",
        cross.body
    );
    assert_eq!(cross.body["error"]["code"], "cross_organization");

    // And the grant *removal* route is where the `404` rule does apply: a grant id from
    // another organization is not found, because reading the row unscoped and scoping
    // afterwards would answer `403` and confirm the id exists.
    let their_grant: Uuid = sqlx::query_scalar(
        "insert into media_grants (media_id, subject_kind, subject_id, can_read, effect) \
         values ($1, 'user', $2, true, 'deny') returning id",
    )
    .bind(other_file)
    .bind(theirs.id)
    .fetch_one(fx.db.pool())
    .await
    .expect("their grant must be created directly, to make the point");
    let cross_delete = call(
        &fx.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/grants/{their_grant}"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        cross_delete.status,
        StatusCode::NOT_FOUND,
        "another tenant's grant must be a 404, not a 403: {}",
        cross_delete.body
    );
    let still_there: i64 = sqlx::query_scalar("select count(*) from media_grants where id = $1")
        .bind(their_grant)
        .fetch_one(fx.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(still_there, 1, "the refusal must not have deleted their row");

    // And the subject picker is scoped by site for the same reason. A *site* outside the
    // organization is `cross_organization` (403) — the same answer every other route gives —
    // because the site id is a thing the caller may already know. The `404` rule is for the
    // *grant* ids above, which are the ones a caller walks.
    let picker = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/grant-subjects?site_id={other_site}"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        picker.status,
        StatusCode::FORBIDDEN,
        "the picker must refuse another tenant's site: {}",
        picker.body
    );
    assert_eq!(picker.body["error"]["code"], "cross_organization");

    // And the picker offers nobody from that organization when asked about ours: the query is
    // bound to *our* organization, so an account of theirs can never be named.
    let ours = call(
        &fx.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/media/grant-subjects?site_id={}&search={}",
                fx.site, theirs.email
            ),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(ours.status, StatusCode::OK, "{}", ours.body);
    assert_eq!(
        ours.body.as_array().expect("a list").len(),
        0,
        "another organization's account must not be offered: {}",
        ours.body
    );
}

/// Removing a grant takes effect immediately, and a stale id is a `404` rather than a `204`.
#[tokio::test]
async fn removing_a_grant_takes_effect_on_the_next_request() {
    let Some(fx) = fixture().await else {
        return;
    };

    let file = upload(
        &fx.state,
        &fx.editor_token,
        fx.site,
        "temporary.txt",
        b"closed for a moment",
    )
    .await;

    let created = put_grant(
        &fx.state,
        &fx.editor_token,
        &format!("/api/v1/media/{file}/grants"),
        json!({
            "subject_kind": "user",
            "subject_id": fx.subject.id,
            "can_read": true,
            "effect": "deny",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let grant_id = Uuid::parse_str(created.body["id"].as_str().expect("an id")).expect("a uuid");

    let refused = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    let removed = call(
        &fx.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/grants/{grant_id}"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        removed.status,
        StatusCode::NO_CONTENT,
        "the grant must go: {}",
        removed.body
    );

    // Effective on the very next request, with no worker and no reaper.
    let after = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file}/raw"),
            Some(&fx.subject_token),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "removing a grant must take effect at once: {}",
        after.body
    );

    // A second removal is a `404`, not a `204` for a row that was not there.
    let again = call(
        &fx.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/grants/{grant_id}"),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);

    // The audit log carries the change and the removal, naming what moved.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log \
         where action in ('media.grant_changed', 'media.grant_removed') \
           and organization_id = $1",
    )
    .bind(fx.org)
    .fetch_one(fx.db.pool())
    .await
    .expect("the audit count must read");
    assert_eq!(audited, 2, "both the change and the removal must be recorded");
}

/// The picker offers the organization's own subjects, a group first, and a search narrows it.
#[tokio::test]
async fn the_subject_picker_offers_this_organizations_subjects() {
    let Some(fx) = fixture().await else {
        return;
    };

    let group: Uuid = sqlx::query_scalar(
        "insert into groups (organization_id, name, slug) values ($1, $2, $3) returning id",
    )
    .bind(fx.org)
    .bind("Design Guild")
    .bind(format!("design-{}", Uuid::new_v4().simple()))
    .fetch_one(fx.db.pool())
    .await
    .expect("the group must be created");

    let all = call(
        &fx.state,
        request(
            Method::GET,
            &format!("/api/v1/media/grant-subjects?site_id={}", fx.site),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    let rows = all.body.as_array().expect("a list");
    assert!(!rows.is_empty(), "the picker must offer something: {}", all.body);

    let labels: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"Design Guild"),
        "the group must be offered: {labels:?}"
    );
    let guild = rows
        .iter()
        .find(|row| row["label"] == "Design Guild")
        .expect("the group row");
    assert_eq!(guild["kind"], "group");
    assert_eq!(guild["id"].as_str().expect("an id"), group.to_string());
    assert_eq!(guild["suggested"], true, "a group is the row to reach for");
    assert!(
        guild["detail"].as_str().expect("a detail").contains("members"),
        "a group says how many: {}",
        guild["detail"]
    );

    // A search narrows it, and an account of this organization is offered by its email.
    let searched = call(
        &fx.state,
        request(
            Method::GET,
            &format!(
                "/api/v1/media/grant-subjects?site_id={}&search={}",
                fx.site,
                fx.subject.email
            ),
            Some(&fx.editor_token),
            None,
        ),
    )
    .await;
    assert_eq!(searched.status, StatusCode::OK, "{}", searched.body);
    let found = searched.body.as_array().expect("a list");
    assert_eq!(found.len(), 1, "the search must narrow: {}", searched.body);
    assert_eq!(found[0]["kind"], "user");
    assert_eq!(found[0]["id"].as_str().expect("an id"), fx.subject.id.to_string());
}
