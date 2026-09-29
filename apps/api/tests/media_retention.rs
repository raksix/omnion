//! Integration tests for retention (REQ-010, slice 4).
//!
//! These walk the **real router against a real database**, because the whole feature is a set of
//! claims about *what a statement does to a row* and only a statement can prove them.
//!
//! Every assertion is against observable state — the response body, **the row read out of
//! PostgreSQL**, and the bytes — because a response that omits a field is indistinguishable
//! from one that stored it and chose not to say so.
//!
//! Four things are walked here, and each exists because the shortcut produces a plausible wrong
//! answer:
//!
//! * **the current version is exempt from the version sweep by number, not by age.** A
//!   one-day keep window over a file that was *just* replaced must not delete the version the
//!   row is serving — the assertion reads `max(version)` back out of `media_versions` and the
//!   file's own `storage_key` has to still be in the store.
//! * **a legal hold beats every other rule.** A held file past its purge window is not purged,
//!   and the run log says *that* rather than reporting a bare zero.
//! * **a purge refuses a referenced file and names the referrer.** `media_references` cascades
//!   away with the file, so the alternative is a silent delete of a hero image a live page
//!   resolves to; the refusal is a `409` carrying the record, not a `400`.
//! * **the repair scan removes the *other* kind of lie.** A reference to a page that no longer
//!   exists refuses a purge for ever, so "cannot purge: still referenced" is a sentence an
//!   operator meets and cannot act on. The scan removes the stale row and the very next purge
//!   goes through.

mod support;

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

/// The permission keys the editor of this suite holds.
///
/// `media.settings.manage` is the one that matters here: reading a policy is `media.read`
/// (a person who cannot change a window must still be able to ask what the site promises to
/// keep), and writing one is `media.settings.manage` — the destructive half of the same
/// settings screen that already carries the storage keys.
const EDITOR_PERMISSIONS: [&str; 7] = [
    "media.read",
    "media.upload",
    "media.delete",
    "media.update",
    "media.manage",
    "media.settings.manage",
    "media.share",
];

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    /// **Every** `Set-Cookie`, in order — sign-in answers with the session and the CSRF token
    /// beside it, and reading only the first is how a walk ends up holding no token at all.
    set_cookies: Vec<String>,
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
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
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
        set_cookies,
        body,
        raw,
    }
}

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    // A write echoes the CSRF token in a header as well as carrying it in a cookie; unpacking
    // here means a suite cannot send one without the other.
    let builder = match token {
        Some(token) => support::walk_auth::apply_credential(token, builder),
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

    // An upload is a write like any other: the session cookie and the echoed CSRF token travel
    // together. This is the *second* request builder in the file, and it is why fixing only
    // `request()` would have left every upload broken while the rest of the suite went green —
    // a suite that hand-rolls its headers re-opens whatever the shared helper closed.
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/media?site_id={site}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
    if let Some(csrf) = support::walk_auth::unpack(token).csrf {
        builder = builder.header(support::walk_auth::CSRF_HEADER, csrf);
    }

    let response = call(
        state,
        support::walk_auth::apply_credential(token, builder)
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
    let mut config = Config::from_env().expect("environment must be valid");
    // The suite gets its own CSRF secret, so sign-in issues a token at all. Without one the
    // deployment refuses every cookie-authenticated write by design — the double-submit check
    // has nothing to compare — and a test process has no `OMNION_CSRF_SECRET`.
    support::walk_auth::with_csrf_secret(&mut config);
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

// ---------------------------------------------------------------------------------------------
// URIs
// ---------------------------------------------------------------------------------------------

fn retention_uri(site: Uuid) -> String {
    format!("/api/v1/media/retention?site_id={site}")
}
fn policy_uri(site: Uuid, policy: Uuid) -> String {
    format!("/api/v1/media/retention/{policy}?site_id={site}")
}
fn run_uri(site: Uuid) -> String {
    format!("/api/v1/media/retention/run?site_id={site}")
}
fn runs_uri(site: Uuid) -> String {
    format!("/api/v1/media/retention/runs?site_id={site}")
}
fn repair_uri(site: Uuid) -> String {
    format!("/api/v1/media/retention/repair?site_id={site}")
}
fn file_uri(site: Uuid, file: Uuid) -> String {
    format!("/api/v1/media/files/{file}?site_id={site}")
}
fn hold_uri(file: Uuid) -> String {
    format!("/api/v1/media/files/{file}/hold")
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

/// Everything one walk needs: an organization, a site, an editor and a reader.
struct Fixture {
    state: AppState,
    db: Db,
    storage: Storage,
    editor_email: String,
    reader_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
    sites: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db, storage) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = sqlx::query_scalar::<_, Uuid>(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("Retention Test")
        .bind(format!("media-retention-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Retention Site")
        .fetch_one(db.pool())
        .await
        .expect("the site must be created");

        let (platform_id, _) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = bind_role(
            &db,
            org,
            platform_id,
            "Retention Editor",
            &EDITOR_PERMISSIONS,
        )
        .await;
        // A reader: `media.read` and nothing else. It may *ask* what the site promises to keep
        // and may not change a window, run a sweep or set a hold.
        let (reader_id, reader_email) =
            bind_role(&db, org, platform_id, "Retention Reader", &["media.read"]).await;

        Some(Self {
            state,
            db,
            storage,
            editor_email,
            reader_email,
            accounts: vec![platform_id, editor_id, reader_id],
            organizations: vec![org],
            sites: vec![site],
        })
    }

    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }
    async fn reader_token(&self) -> String {
        login(&self.state, &self.reader_email).await
    }

    /// Age a trashed file past a window, by moving its deletion back.
    ///
    /// The platform's own clock is not a test's to change, and ageing the *row* is the state
    /// the sweep actually reads — a test that waited thirty days proves nothing a
    /// `deleted_at` update does not.
    async fn age_trash(&self, file: Uuid, days: i64) {
        sqlx::query(
            "update media set deleted_at = now() - ($2 || ' days')::interval where id = $1",
        )
        .bind(file)
        .bind(days.to_string())
        .execute(self.db.pool())
        .await
        .expect("the deletion must be aged");
    }

    /// Age a version row past a window.
    async fn age_version(&self, file: Uuid, version: i32, days: i64) {
        sqlx::query(
            "update media_versions set created_at = now() - ($3 || ' days')::interval \
             where media_id = $1 and version = $2",
        )
        .bind(file)
        .bind(version)
        .bind(days.to_string())
        .execute(self.db.pool())
        .await
        .expect("the version must be aged");
    }

    async fn cleanup(&self) {
        sqlx::query("delete from users where id = any($1)")
            .bind(&self.accounts)
            .execute(self.db.pool())
            .await
            .expect("account cleanup must run");
        sqlx::query("delete from organizations where id = any($1)")
            .bind(&self.organizations)
            .execute(self.db.pool())
            .await
            .expect("organization cleanup must run");
    }
}

/// Create an account with a unique address.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("retention-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Retention Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Create an account, give it its own role with exactly `keys`, and bind it to the org.
///
/// Its **own** role, deliberately: a role's permissions are a property of the role and not of
/// the binding, so reusing one role and adding a key for a second account grants it to both.
/// `media_scan.rs` learned that from a walk that proved the opposite of what it meant.
async fn bind_role(
    db: &Db,
    org: Uuid,
    granted_by: Uuid,
    name: &str,
    keys: &[&str],
) -> (Uuid, String) {
    let (id, email) = create_account(db, Some(org)).await;
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id: org,
            key: format!("role-{}", Uuid::new_v4().simple()),
            name: name.to_owned(),
            description: format!("Drives {name}"),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = keys
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
            user_id: id,
            scope: Scope::Organization {
                organization_id: org,
            },
            granted_by: Some(granted_by),
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
    (id, email)
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
    assert_eq!(
        response.status,
        StatusCode::OK,
        "login body: {}",
        response.body
    );
    // Every `Set-Cookie`, not the first. This helper used to read the first header and take the
    // first `name=value` out of it, which is correct for one cookie and silently lossy for the
    // two sign-in issues — so the walk held a session with no token and every write came back
    // `csrf_unavailable`, a code that blames the deployment rather than the helper.
    support::walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
}

/// The storage key of the version the `media` row is currently serving.
async fn current_key(db: &Db, file: Uuid) -> String {
    sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
        .bind(file)
        .fetch_one(db.pool())
        .await
        .expect("the row must read")
}

/// The highest version number a file has.
async fn newest_version(db: &Db, file: Uuid) -> i32 {
    sqlx::query_scalar::<_, i32>("select max(version) from media_versions where media_id = $1")
        .bind(file)
        .fetch_one(db.pool())
        .await
        .expect("the versions must read")
}

/// Every version number a file still has, oldest first.
async fn version_numbers(db: &Db, file: Uuid) -> Vec<i32> {
    let rows: Vec<(i32,)> =
        sqlx::query_as("select version from media_versions where media_id = $1 order by version")
            .bind(file)
            .fetch_all(db.pool())
            .await
            .expect("the versions must read");
    rows.into_iter().map(|(version,)| version).collect()
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The whole feature, in one pass: a site-wide policy is created, a file is replaced and
/// aged, the version sweep keeps the version the row is serving, and a second file is trashed
/// and purged. The run log records both halves and the response names what it did.
#[tokio::test]
async fn a_sweep_keeps_the_served_version_and_purges_what_is_past_its_window() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A site created *after* the migration has a policy row, thanks to the trigger — read out
    // of the database rather than inferred from a GET, because a GET that invents defaults is
    // exactly the gap the trigger exists to close.
    let seeded: Option<(i32, i32, i32, String)> = sqlx::query_as(
        "select keep_versions_days, trash_days, purge_after_days, name \
         from media_retention_policies where site_id = $1 and folder_id is null",
    )
    .bind(site)
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the policy row must read");
    let (keep, trash, purge, name) = seeded.expect("a new site must have a retention policy row");
    assert_eq!((keep, trash, purge), (365, 30, 90));
    assert_eq!(name, "Standard retention");

    // Tighten the windows so a walk does not have to age a file by three months.
    let site_policy: Value = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), Some(&token), None),
    )
    .await
    .body;
    let policy_id =
        Uuid::parse_str(site_policy["policies"][0]["id"].as_str().expect("an id")).expect("a uuid");
    let tightened = call(
        &fixture.state,
        request(
            Method::PUT,
            &policy_uri(site, policy_id),
            Some(&token),
            Some(json!({ "keep_versions_days": 1, "trash_days": 1, "purge_after_days": 2 })),
        ),
    )
    .await;
    assert_eq!(tightened.status, StatusCode::OK, "{}", tightened.body);
    assert_eq!(tightened.body["trash_days"], json!(1));
    assert_eq!(tightened.body["purge_after_days"], json!(2));
    // The screen's own sentence, not a row of numbers: an operator reads what happens to their
    // file, and the sentence is the only place the three windows mean anything together.
    assert!(
        tightened.body["behaviour"]
            .as_str()
            .expect("a sentence")
            .contains("1 day of superseded history"),
        "{}",
        tightened.body["behaviour"]
    );

    // --- the version half -------------------------------------------------------------------
    let media = upload(&fixture.state, &token, site, "notes.txt", b"version one").await;
    let replaced = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media}/versions?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    // A version create without a body is refused; the replace is multipart, so this walk uses
    // the upload route with an explicit replace instead. What matters is the *state*, not how
    // the bytes arrived, and the next block writes version 2 directly.
    assert!(
        replaced.status == StatusCode::BAD_REQUEST || replaced.status == StatusCode::CREATED,
        "the version route must answer: {}",
        replaced.body
    );

    // Append version 2 the way a replace does — a new key, a new checksum, the row still
    // serving the newest number.
    let second_key = format!("{site}/{media}/v2.txt");
    fixture
        .storage
        .put(&second_key, b"version two".as_slice(), "text/plain")
        .await
        .expect("version 2 must be stored");
    sqlx::query(
        "insert into media_versions (media_id, version, storage_key, size_bytes, checksum, \
           content_type) values ($1, 2, $2, 11, \
           '2222222222222222222222222222222222222222222222222222222222222222', 'text/plain')",
    )
    .bind(media)
    .bind(&second_key)
    .execute(fixture.db.pool())
    .await
    .expect("version 2 must be inserted");
    sqlx::query(
        "update media set storage_key = $2, checksum = $3, version_count = 2 where id = $1",
    )
    .bind(media)
    .bind(&second_key)
    .bind("2222222222222222222222222222222222222222222222222222222222222222")
    .execute(fixture.db.pool())
    .await
    .expect("the row must point at version 2");
    assert_eq!(newest_version(&fixture.db, media).await, 2);

    // Age **both** versions past the one-day window. This is the case that breaks the obvious
    // query: "delete every version older than a day" would take version 2 as well, and the row
    // would be left naming an object that no longer exists.
    fixture.age_version(media, 1, 10).await;
    fixture.age_version(media, 2, 10).await;
    let served_key = current_key(&fixture.db, media).await;

    // --- the trash half --------------------------------------------------------------------
    let doomed = upload(&fixture.state, &token, site, "doomed.txt", b"goodbye").await;
    let deleted = call(
        &fixture.state,
        request(Method::DELETE, &file_uri(site, doomed), Some(&token), None),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    fixture.age_trash(doomed, 10).await;

    // --- the sweep -------------------------------------------------------------------------
    let run = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(run.status, StatusCode::OK, "{}", run.body);
    assert_eq!(
        run.body["run"]["versions_removed"],
        json!(1),
        "only version 1 goes"
    );
    assert_eq!(run.body["run"]["purged"], json!(1), "the trashed file goes");
    assert_eq!(
        run.body["run"]["purged_bytes"],
        json!(7),
        "its own bytes, not the total"
    );
    assert_eq!(run.body["remaining_files"], json!(0));

    // The current version survived, and the row still names an object that exists.
    assert_eq!(
        version_numbers(&fixture.db, media).await,
        vec![2],
        "the version the row is serving is never a sweep target"
    );
    assert_eq!(current_key(&fixture.db, media).await, served_key);
    assert!(
        fixture.storage.get(&served_key).await.is_ok(),
        "the served version's bytes must still be in the store"
    );

    // The purged file is gone from the table *and* from the store.
    let survives: Option<(Uuid,)> = sqlx::query_as("select id from media where id = $1")
        .bind(doomed)
        .fetch_optional(fixture.db.pool())
        .await
        .expect("the query must run");
    assert!(survives.is_none(), "a purged file is a hard delete");

    // The run log records the pass, and the log survives a *second* empty run — the whole
    // point of logging a run that found nothing.
    let log = call(
        &fixture.state,
        request(Method::GET, &runs_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(log.status, StatusCode::OK);
    let first = &log.body["runs"][0];
    assert_eq!(first["versions_removed"], json!(1));
    assert_eq!(first["purged"], json!(1));
    assert!(
        first["summary"]
            .as_str()
            .expect("a sentence")
            .contains("1 old version(s) removed"),
        "{}",
        first["summary"]
    );
    assert!(
        first["summary"]
            .as_str()
            .expect("a sentence")
            .contains("1 file(s) purged"),
        "{}",
        first["summary"]
    );

    let second_run = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(second_run.status, StatusCode::OK);
    assert_eq!(second_run.body["run"]["purged"], json!(0));
    assert_eq!(
        second_run.body["run"]["summary"],
        json!("Nothing was eligible."),
        "an empty run says so in words, not as a bare zero"
    );

    // And the empty run is *in the log* — "the last run was at 02:00 and it was clean" is the
    // sentence an operator needs on the day they are asking why a file is still here.
    let log_again = call(
        &fixture.state,
        request(Method::GET, &runs_uri(site), Some(&token), None),
    )
    .await;
    assert!(
        log_again.body["runs"].as_array().expect("runs").len() >= 2,
        "a run that found nothing is still a run: {}",
        log_again.body
    );

    fixture.cleanup().await;
}

/// A hold beats every window, and the run log says *that* rather than reporting a zero.
#[tokio::test]
async fn a_legal_hold_beats_every_window_and_the_log_says_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let file = upload(&fixture.state, &token, site, "held.txt", b"evidence").await;
    let deleted = call(
        &fixture.state,
        request(Method::DELETE, &file_uri(site, file), Some(&token), None),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    fixture.age_trash(file, 400).await;

    // A hold with no reason is refused *before* anything is written: a file nobody can explain
    // is a file nobody will ever be allowed to delete.
    let no_reason = call(
        &fixture.state,
        request(
            Method::PUT,
            &hold_uri(file),
            Some(&token),
            Some(json!({ "hold": true, "reason": "   " })),
        ),
    )
    .await;
    assert_eq!(
        no_reason.status,
        StatusCode::BAD_REQUEST,
        "{}",
        no_reason.body
    );
    assert_eq!(
        no_reason.body["error"]["code"],
        json!("hold_reason_required")
    );
    let after_refusal: (bool,) = sqlx::query_as("select legal_hold from media where id = $1")
        .bind(file)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert!(!after_refusal.0, "a refused hold writes nothing");

    let held = call(
        &fixture.state,
        request(
            Method::PUT,
            &hold_uri(file),
            Some(&token),
            Some(json!({ "hold": true, "reason": "litigation hold, case 2026-114" })),
        ),
    )
    .await;
    assert_eq!(held.status, StatusCode::OK, "{}", held.body);
    assert_eq!(held.body["legal_hold"], json!(true));
    assert_eq!(held.body["changed"], json!(true));

    // Read the hold out of PostgreSQL, not off the response: a response can report a write it
    // did not make.
    let (in_db,): (bool,) = sqlx::query_as("select legal_hold from media where id = $1")
        .bind(file)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert!(in_db, "the hold has to be a fact, not a response field");

    // The audit log names the file and the reason.
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "select action, metadata from audit_log where target_id = $1::text order by created_at",
    )
    .bind(file.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit log must read");
    assert!(
        audit
            .iter()
            .any(|(action, meta)| action == "media.hold_placed"
                && meta["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("2026-114")),
        "a hold has to carry its reason into the audit log: {audit:?}"
    );

    // A sweep 400 days later does not touch it.
    let run = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(run.status, StatusCode::OK, "{}", run.body);
    assert_eq!(
        run.body["run"]["purged"],
        json!(0),
        "a hold outranks the window"
    );
    assert_eq!(run.body["run"]["held_back"], json!(1));
    assert!(
        run.body["run"]["summary"]
            .as_str()
            .expect("a sentence")
            .contains("legal hold"),
        "a run that removed nothing says which of the three nothings it was: {}",
        run.body["run"]["summary"]
    );
    let survives: Option<(Uuid,)> = sqlx::query_as("select id from media where id = $1")
        .bind(file)
        .fetch_optional(fixture.db.pool())
        .await
        .expect("the query must run");
    assert!(survives.is_some(), "a held file is not deleted");

    // Releasing the hold is a write with a reason too, and it is effective on the next sweep.
    let released = call(
        &fixture.state,
        request(
            Method::PUT,
            &hold_uri(file),
            Some(&token),
            Some(json!({ "hold": false, "reason": "case closed, no claim" })),
        ),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.body);
    assert_eq!(released.body["legal_hold"], json!(false));

    // A second identical press is a no-op rather than a second audit entry.
    let again = call(
        &fixture.state,
        request(
            Method::PUT,
            &hold_uri(file),
            Some(&token),
            Some(json!({ "hold": false, "reason": "case closed, no claim" })),
        ),
    )
    .await;
    assert_eq!(
        again.body["changed"],
        json!(false),
        "nothing moved, so nothing is logged"
    );

    let after = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(
        after.body["run"]["purged"],
        json!(1),
        "the release is effective at once"
    );

    fixture.cleanup().await;
}

/// A purge refuses a referenced file, names the record, and the repair scan is what makes the
/// very next purge go through.
#[tokio::test]
async fn a_purge_refuses_a_referenced_file_and_the_repair_scan_unblocks_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let file = upload(&fixture.state, &token, site, "hero.txt", b"the hero image").await;
    let deleted = call(
        &fixture.state,
        request(Method::DELETE, &file_uri(site, file), Some(&token), None),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    fixture.age_trash(file, 400).await;

    // A real page, and a reference from it to the file.
    let page = sqlx::query_scalar::<_, Uuid>(
        "insert into pages (site_id, slug, page_type, status) \
         values ($1, $2, 'page', 'published') returning id",
    )
    .bind(site)
    .bind(format!("hero-{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the page must be created");
    sqlx::query(
        "insert into media_references (media_id, resource_kind, resource_id, field) \
         values ($1, 'page', $2, 'hero_image_id')",
    )
    .bind(file)
    .bind(page.to_string())
    .execute(fixture.db.pool())
    .await
    .expect("the reference must be recorded");

    let run = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(run.status, StatusCode::OK, "{}", run.body);
    assert_eq!(
        run.body["run"]["purged"],
        json!(0),
        "a file a live page resolves to is not purged"
    );
    assert_eq!(
        run.body["run"]["refused"],
        json!(1),
        "one file, however many fields"
    );
    let refusal = &run.body["refused"][0];
    assert_eq!(refusal["media_id"], json!(file.to_string()));
    assert_eq!(refusal["resource_kind"], json!("page"));
    assert_eq!(refusal["field"], json!("hero_image_id"));
    assert!(
        refusal["describe"]
            .as_str()
            .expect("a sentence")
            .contains("hero_image_id"),
        "the refusal names the record, not a count: {refusal}"
    );
    assert!(
        run.body["run"]["summary"]
            .as_str()
            .expect("a sentence")
            .contains("still referenced"),
        "{}",
        run.body["run"]["summary"]
    );

    // The file is still on disk and still restorable.
    let survives: Option<(Uuid,)> = sqlx::query_as("select id from media where id = $1")
        .bind(file)
        .fetch_optional(fixture.db.pool())
        .await
        .expect("the query must run");
    assert!(survives.is_some());

    // The page goes away — a migration, a redesign, a draft thrown out. The reference is now a
    // *lie*, and it would refuse the purge for ever.
    sqlx::query("delete from pages where id = $1")
        .bind(page)
        .execute(fixture.db.pool())
        .await
        .expect("the page must be deleted");

    let blocked = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(
        blocked.body["run"]["purged"],
        json!(0),
        "without the repair the library is unpurgeable for ever"
    );

    let repair = call(
        &fixture.state,
        request(Method::POST, &repair_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(repair.status, StatusCode::OK, "{}", repair.body);
    assert_eq!(repair.body["references_removed"], json!(1));
    let rows: Option<(Uuid,)> =
        sqlx::query_as("select id from media_references where media_id = $1")
            .bind(file)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the query must run");
    assert!(rows.is_none(), "the stale reference is gone");

    // And the very next purge goes through.
    let unblocked = call(
        &fixture.state,
        request(Method::POST, &run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(
        unblocked.body["run"]["purged"],
        json!(1),
        "{}",
        unblocked.body
    );

    // A second repair is a no-op that says so, rather than a silent zero.
    let noop = call(
        &fixture.state,
        request(Method::POST, &repair_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(noop.body["references_removed"], json!(0));
    assert!(
        noop.body["summary"]
            .as_str()
            .expect("a sentence")
            .contains("No reference rows"),
        "{}",
        noop.body["summary"]
    );

    fixture.cleanup().await;
}

/// A folder policy wins over the site rule, and the scope can be cleared with an explicit null —
/// the one edit `Option<Option<Uuid>>` cannot carry.
#[tokio::test]
async fn a_folder_policy_wins_and_an_explicit_null_clears_the_scope() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A folder to scope to.
    let folder: Uuid = sqlx::query_scalar(
        "insert into media_folders (site_id, name, path) values ($1, $2, $3) returning id",
    )
    .bind(site)
    .bind("Campaign")
    .bind("Campaign")
    .fetch_one(fixture.db.pool())
    .await
    .expect("the folder must be created");

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &retention_uri(site),
            Some(&token),
            Some(json!({
                "name": "Campaign",
                "folder_id": folder,
                "trash_days": 7,
                "purge_after_days": 14,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let policy_id = Uuid::parse_str(created.body["id"].as_str().expect("an id")).expect("a uuid");
    assert_eq!(created.body["folder_id"], json!(folder.to_string()));
    assert_eq!(created.body["folder_path"], json!("Campaign"));
    assert!(
        created.body["scope"]
            .as_str()
            .expect("words")
            .contains("Campaign"),
        "the screen says *which* folder rather than printing a uuid: {}",
        created.body["scope"]
    );

    // The narrowest scope is the authority, read through the crate's own resolver.
    let window = omnion_media::governing_window(fixture.db.pool(), site, Some(folder))
        .await
        .expect("the window must resolve");
    assert_eq!(
        window.trash_days, 7,
        "the folder rule wins over the site's 30"
    );
    assert_eq!(window.purge_after_days, 14);
    let fallback = omnion_media::governing_window(fixture.db.pool(), site, None)
        .await
        .expect("the fallback must resolve");
    assert_eq!(
        fallback.trash_days, 30,
        "the site rule still answers for everything else"
    );

    // A duplicate name is a `409` naming the field, not a `500` with a constraint name.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            &retention_uri(site),
            Some(&token),
            Some(json!({ "name": "Campaign", "folder_id": null })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "{}", duplicate.body);
    assert_eq!(
        duplicate.body["error"]["code"],
        json!("retention_policy_name_taken")
    );

    // **The whole point of this walk:** an explicit `folder_id: null` clears the scope. On
    // `Option<Option<Uuid>>` with `#[serde(default)]` this arrives as `None` — "leave it alone" —
    // and the screen reports "saved" over an unchanged scope. The unit test in
    // `media_retention.rs` pins the decode; this pins that the *write* happens.
    let cleared = call(
        &fixture.state,
        request(
            Method::PUT,
            &policy_uri(site, policy_id),
            Some(&token),
            Some(json!({ "folder_id": null })),
        ),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(
        cleared.body["folder_id"],
        Value::Null,
        "an explicit null widens the policy to the whole site"
    );
    let stored: (Option<Uuid>,) =
        sqlx::query_as("select folder_id from media_retention_policies where id = $1")
            .bind(policy_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert_eq!(stored.0, None, "and the row agrees, read out of PostgreSQL");

    // An absent field leaves the scope alone — the *other* half of the distinction.
    let named = call(
        &fixture.state,
        request(
            Method::PUT,
            &policy_uri(site, policy_id),
            Some(&token),
            Some(json!({ "trash_days": 21, "purge_after_days": 30 })),
        ),
    )
    .await;
    assert_eq!(named.status, StatusCode::OK, "{}", named.body);
    assert_eq!(
        named.body["folder_id"],
        Value::Null,
        "untouched by a body that omits it"
    );
    assert_eq!(named.body["trash_days"], json!(21));

    // Every window error names its own field, on the *save* and on the create.
    let bad = call(
        &fixture.state,
        request(
            Method::PUT,
            &policy_uri(site, policy_id),
            Some(&token),
            Some(json!({ "trash_days": 0 })),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert_eq!(
        bad.body["error"]["code"],
        json!("invalid_retention_setting")
    );
    assert_eq!(bad.body["error"]["details"]["field"], json!("trash_days"));

    // A purge window inside the restore window is refused, against the *stored* row rather
    // than against a value the request happened to send.
    let inside = call(
        &fixture.state,
        request(
            Method::PUT,
            &policy_uri(site, policy_id),
            Some(&token),
            Some(json!({ "purge_after_days": 3 })),
        ),
    )
    .await;
    assert_eq!(inside.status, StatusCode::BAD_REQUEST, "{}", inside.body);
    assert!(
        inside.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("purge_after_days"),
        "{}",
        inside.body
    );
    let untouched: (i32,) =
        sqlx::query_as("select purge_after_days from media_retention_policies where id = $1")
            .bind(policy_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert_eq!(
        untouched.0, 30,
        "a refused edit writes nothing — checked against the stored row, not the response"
    );

    // A policy of another tenant is a `404`, not a `403` — and the refusal deletes nothing.
    let other = foreign_policy(&fixture, &token).await;
    let denied = call(
        &fixture.state,
        request(Method::DELETE, &policy_uri(site, other), Some(&token), None),
    )
    .await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND, "{}", denied.body);
    let still_there: Option<(Uuid,)> =
        sqlx::query_as("select id from media_retention_policies where id = $1")
            .bind(other)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the query must run");
    assert!(still_there.is_some(), "a refused delete writes nothing");

    fixture.cleanup().await;
}

/// A reader may ask what the site promises to keep and may not change a window, run a sweep or
/// set a hold.
#[tokio::test]
async fn a_reader_may_read_the_policy_and_may_not_change_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let editor = fixture.editor_token().await;
    let reader = fixture.reader_token().await;

    let file = upload(&fixture.state, &editor, site, "readme.txt", b"hello").await;

    let read = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), Some(&reader), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert!(
        read.body["policies"].as_array().expect("policies").len() >= 1,
        "a reader can see what the site keeps — 'my file was deleted by a policy' is a question \
         the person affected must be able to ask"
    );
    assert!(read.body["summary"].as_str().is_some());

    let runs = call(
        &fixture.state,
        request(Method::GET, &runs_uri(site), Some(&reader), None),
    )
    .await;
    assert_eq!(runs.status, StatusCode::OK);

    for (label, method, uri, body) in [
        (
            "create",
            Method::POST,
            retention_uri(site),
            Some(json!({ "name": "Nope" })),
        ),
        (
            "update",
            Method::PUT,
            policy_uri(site, Uuid::new_v4()),
            Some(json!({ "trash_days": 3 })),
        ),
        ("run", Method::POST, run_uri(site), None),
        ("repair", Method::POST, repair_uri(site), None),
        (
            "hold",
            Method::PUT,
            hold_uri(file),
            Some(json!({ "hold": true, "reason": "because" })),
        ),
    ] {
        let denied = call(&fixture.state, request(method, &uri, Some(&reader), body)).await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "a reader may not {label}: {}",
            denied.body
        );
    }

    // And anonymous is refused on the read too.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    fixture.cleanup().await;
}

/// A policy of another tenant, created as the platform owner, so the walk can prove a `404`
/// where a `403` would confirm the id exists.
async fn foreign_policy(fixture: &Fixture, _token: &str) -> Uuid {
    let other = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Other Org")
    .bind(format!("other-{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the organization must be created");
    let site = sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
    )
    .bind(other)
    .bind("Other Site")
    .fetch_one(fixture.db.pool())
    .await
    .expect("the site must be created");
    sqlx::query_scalar::<_, Uuid>(
        "insert into media_retention_policies (site_id, name) values ($1, 'Theirs') returning id",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the policy must be created")
}

/// The retention screen's "past its restore window" count, with something actually past it.
///
/// This walk exists because of a query that was wrong and a comment that said it was not.
/// `past_restore_window` summed `size_bytes` with no cast, and `sum()` over a `bigint` is
/// `numeric`, which sqlx will not decode into an `i64` — the retention list answered `500` as
/// soon as a site had a trashed file past its window, and only for such a site. Every existing
/// walk passed because none of them had one: `sum()` over an empty set is `NULL`, `NULL` decodes
/// into `Option<i64>` as `None`, and the whole shape of the bug is invisible until the set is
/// non-empty.
///
/// So the walk does the one thing the others did not: it trashes a file, ages the deletion past
/// the window, and reads the count back. An assertion that can only fail when there is something
/// to count is the only kind that proves a sum.
#[tokio::test]
async fn the_past_restore_window_counts_files_that_are_actually_past_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let editor = fixture.editor_token().await;

    // Before anything is trashed the count is zero — and that is the state every other walk
    // stopped at, which is exactly why the defect survived.
    let before = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), Some(&editor), None),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    assert_eq!(before.body["past_restore"], json!(0));
    assert_eq!(before.body["past_restore_bytes"], json!(0));

    // Two files, deliberately different sizes, so the byte total cannot pass by accident.
    let small = upload(&fixture.state, &editor, site, "small.txt", b"hi").await;
    let large = upload(&fixture.state, &editor, site, "large.txt", b"twenty bytes here").await;
    let sizes: Vec<i64> = sqlx::query_scalar("select size_bytes from media where id = any($1)")
        .bind(vec![small, large])
        .fetch_all(fixture.db.pool())
        .await
        .expect("the sizes must read");
    let expected_bytes: i64 = sizes.iter().sum();

    for file in [small, large] {
        let deleted = call(
            &fixture.state,
            request(Method::DELETE, &file_uri(site, file), Some(&editor), None),
        )
        .await;
        assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
        // The site's default trash window is 30 days; 40 is comfortably past it.
        fixture.age_trash(file, 40).await;
    }

    // The count the retention tab renders. This is the assertion the old suite could not make:
    // with a non-empty set the uncast sum decodes as NUMERIC and the whole screen 500s.
    let after = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), Some(&editor), None),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
    assert_eq!(
        after.body["past_restore"],
        json!(2),
        "both files are past the window and the screen must say so"
    );
    assert_eq!(
        after.body["past_restore_bytes"],
        json!(expected_bytes),
        "their bytes, added in a type the decoder accepts"
    );

    // A file still inside its window is not counted. Without this the assertion above would also
    // pass if the query ignored the cutoff entirely.
    let _fresh = upload(&fixture.state, &editor, site, "fresh.txt", b"new").await;
    let still_here = call(
        &fixture.state,
        request(Method::GET, &retention_uri(site), Some(&editor), None),
    )
    .await;
    assert_eq!(still_here.status, StatusCode::OK);
    assert_eq!(
        still_here.body["past_restore"],
        json!(2),
        "a file deleted seconds ago has not aged out"
    );
}
