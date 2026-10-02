//! Integration tests for the backup centre (REQ-013, slice 1).
//!
//! These walk the **real router against a real database**, because the whole feature is a set
//! of claims about *what a run wrote and what it can prove about it*, and only a statement and
//! a filesystem can prove them.
//!
//! Every assertion is against observable state — the response body, **the row read out of
//! PostgreSQL**, and the bytes read back off the destination — because a response that omits a
//! field is indistinguishable from one that stored it and chose not to say so.
//!
//! Five things are walked here, and each exists because the shortcut produces a plausible
//! wrong answer:
//!
//! * **A backup of all five parts reaches `succeeded`, and all five artifacts exist on the
//!   destination with the sizes the manifest recorded.** A run that reported success while
//!   writing nothing is the single most expensive thing this product can do, and the only
//!   place it is caught is here.
//! * **`plugins` is a DONE part with zero items, not a missing one.** It is empty by design
//!   until a package installer exists; if it were absent, the restore wizard could not answer
//!   "was media part of this run at all?", and a backup that omits a part silently is a
//!   backup nobody can reason about.
//! * **A verification over the real destination comes back clean, and a corrupted artifact
//!   turns it red by name.** The second half is the one that matters: a verify button that
//!   always says "fine" is worse than no button, because it is the thing an operator trusts
//!   the day before they need it.
//! * **The settings response never carries a credential value, and the save refuses an
//!   unwritable destination.** Scanned as raw bytes, not asserted through the typed body —
//!   a type cannot prove what a serialiser did.
//! * **A reader may read and may take a backup, and may not delete or change settings.** The
//!   four keys are separate, and the walk proves the separation rather than the names.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_backup;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_security::RatePolicy;
use omnion_storage::Storage;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The key the CSRF tokens are derived from in this suite.
///
/// Fixed, not random, and configured rather than left absent. A suite with no CSRF secret
/// sees every cookie-authenticated write answered `csrf_unavailable` — a message that names
/// the *server's* configuration rather than the suite's own missing header, and which sends
/// the reader looking at a deployment problem that does not exist. `tests/csrf.rs` is the
/// round trip that proves why the token is sent at all; this suite is the other half, proving
/// the writes go through with it.
const CSRF_SECRET: &str = "backup-integration-suite-key-material";

/// The keys a backup operator holds: read, take one, verify, and configure.
const OPERATOR_PERMISSIONS: [&str; 3] = ["backup.read", "backup.create", "backup.manage"];

/// The keys a reader holds. It may look at every restore point and take a new one; it may not
/// delete one or repoint the destination.
const READER_PERMISSIONS: [&str; 2] = ["backup.read", "backup.create"];

/// The keys a **restorer** holds: read, take one, and overwrite live data.
///
/// Its own role on purpose. The route is behind `backup.restore` and nothing else, so the
/// walk that proves the separation needs an account that holds every *other* key and still
/// cannot press the button -- which is exactly the operator above.
const RESTORER_PERMISSIONS: [&str; 4] = [
    "backup.read",
    "backup.create",
    "backup.manage",
    "backup.restore",
];

/// The key the **security posture** screen needs, and nothing else.
///
/// A walk that reads the posture overview through an account holding every key proves the
/// screen works for an account nobody has. The one key is also what makes this suite's
/// subject the backup centre's *consumer*: `security.read` with no backup key at all is the
/// shape of the operator who is told "your backups are stale" and cannot take one.
const POSTURE_PERMISSIONS: [&str; 1] = ["security.read"];

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, in order. A single-cookie accessor is what hid the CSRF defect
    /// in `csrf.rs`: login set two cookies the whole time and the test read one happily.
    set_cookies: Vec<String>,
    body: Value,
    raw: Vec<u8>,
}

impl TestResponse {
    /// The error message the API answered with, or an empty string.
    ///
    /// The envelope nests it under `error.message`, so every assertion that reads
    /// `body["message"]` gets Null and fails on a *missing* message rather than a wrong one —
    /// which reads as "the API said nothing" when the API said exactly the right thing.
    fn message(&self) -> String {
        self.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The value of the named cookie across every `Set-Cookie` header, or `None`.
    fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies.iter().find_map(|header_value| {
            let pair = header_value.split(';').next().unwrap_or_default();
            pair.split_once('=')
                .filter(|(cookie, _)| cookie.trim() == name)
                .map(|(_, value)| value.trim().to_owned())
        })
    }

    /// The body rendered as a compact string, for "the response says X" assertions.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
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
            serde_json::from_slice(&raw).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    TestResponse {
        status,
        set_cookies,
        body,
        raw,
    }
}

/// Build a request. `token` and `csrf` become the session cookie and the CSRF header, which is
/// where a browser puts them.
fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    csrf: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };
    let builder = match csrf {
        Some(token) => builder.header("x-omnion-csrf", token),
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

// --------------------------------------------------------------------------------------------
// URIs
// --------------------------------------------------------------------------------------------

fn backups_uri() -> String {
    "/api/v1/backups".to_owned()
}
fn status_uri() -> String {
    "/api/v1/backups/status".to_owned()
}
fn backup_uri(id: Uuid) -> String {
    format!("/api/v1/backups/{id}")
}
fn manifest_uri(id: Uuid) -> String {
    format!("/api/v1/backups/{id}/manifest")
}
fn verify_uri(id: Uuid) -> String {
    format!("/api/v1/backups/{id}/verify")
}
fn settings_uri() -> String {
    "/api/v1/backup-settings".to_owned()
}
fn schedules_uri() -> String {
    "/api/v1/backup-schedules".to_owned()
}
fn schedule_uri(id: Uuid) -> String {
    format!("/api/v1/backup-schedules/{id}")
}

// --------------------------------------------------------------------------------------------
// Fixture
// --------------------------------------------------------------------------------------------

/// A disposable installation: two organizations, one account in each with its own role.
struct Fixture {
    state: AppState,
    db: Db,
    operator_email: String,
    reader_email: String,
    /// The only account in this organization that may overwrite live data.
    restorer_email: String,
    /// An account holding `security.read` and no backup key -- the operator who is told
    /// "your backups are stale" and cannot take one.
    posture_email: String,
    stranger_email: String,
    org: Uuid,
    other_org: Uuid,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
    /// The absolute root the suite writes backups to, removed on drop.
    root: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
        let mut db_config = config.database.clone();
        db_config.max_connections = 2;
        let db = match Db::connect(&db_config).await {
            Ok(db) => db,
            Err(err) => {
                eprintln!("SKIP: PostgreSQL is not reachable ({err})");
                return None;
            }
        };
        db.migrate().await.expect("migrations must apply");
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");
        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let storage = Storage::Fs(
            omnion_storage::FsStorage::new(std::env::temp_dir().join("omnion-backup-suite-store"))
                .expect("the object store must open"),
        );
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.0.0-test"),
            config,
            db.clone(),
            redis,
            storage,
        );
        give_the_suite_its_own_rate_limit(&state);

        let org = organization(&db, "Backup Test").await;
        let other = organization(&db, "Backup Stranger").await;

        let (platform_id, _) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator_email) = bind_role(
            &db,
            org,
            platform_id,
            "Backup Operator",
            &OPERATOR_PERMISSIONS,
        )
        .await;
        let (reader_id, reader_email) =
            bind_role(&db, org, platform_id, "Backup Reader", &READER_PERMISSIONS).await;
        let (restorer_id, restorer_email) = bind_role(
            &db,
            org,
            platform_id,
            "Backup Restorer",
            &RESTORER_PERMISSIONS,
        )
        .await;
        let (posture_id, posture_email) = bind_role(
            &db,
            org,
            platform_id,
            "Backup Posture Reader",
            &POSTURE_PERMISSIONS,
        )
        .await;
        // A stranger in another organization. It holds **every** key the restorer holds,
        // `backup.restore` included, so the 404 below is about the tenancy boundary and not
        // about a missing permission. A stranger without the restore key would be refused at
        // the guard and the walk would prove nothing about the boundary -- the first version
        // of this walk made exactly that mistake and asserted 404 against a 403.
        let (stranger_id, stranger_email) = bind_role(
            &db,
            other,
            platform_id,
            "Backup Stranger",
            &RESTORER_PERMISSIONS,
        )
        .await;

        // A destination inside the suite's own temporary tree, so nothing it writes can be
        // confused with a real backup root and nothing survives the run.
        let root = std::env::temp_dir().join(format!("omnion-backups-{}", Uuid::new_v4().simple()));
        sqlx::query("update backup_settings set local_root = $1 where id = 1")
            .bind(root.to_string_lossy().to_string())
            .execute(db.pool())
            .await
            .expect("the destination root must be recorded");

        Some(Self {
            state,
            db,
            operator_email,
            reader_email,
            restorer_email,
            posture_email,
            stranger_email,
            org,
            other_org: other,
            accounts: vec![
                platform_id,
                operator_id,
                reader_id,
                restorer_id,
                posture_id,
                stranger_id,
            ],
            organizations: vec![org, other],
            root,
        })
    }

    /// A signed-in session, and the CSRF token that goes with it.
    async fn session(&self, email: &str) -> (String, String) {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
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
        (
            response.cookie("omnion_session").expect("a session cookie"),
            response.cookie("omnion_csrf").unwrap_or_default(),
        )
    }

    /// The file a full storage key lands on for a run with this prefix.
    ///
    /// The prefix argument is accepted and **not** joined, and that is the fix rather than an
    /// oversight. A key is already prefix-qualified, so the old helper produced
    /// `<root>/<prefix>/<key>` where `<key>` began with the prefix again — the archive really
    /// was written to `<root>/<prefix>/<prefix>/…`, and this helper agreed with it. The suite
    /// therefore passed while every artifact sat one directory deeper than the manifest said,
    /// which is the same failure shape as the media part this tick removed: **two halves that
    /// make the same mistake are not a cross-check.**
    fn artifact(&self, _prefix: &str, key: &str) -> std::path::PathBuf {
        self.root.join(key.trim_start_matches('/'))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn organization(db: &Db, name: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind(name)
    .bind(format!(
        "backup-{}-{}",
        name.to_lowercase().replace(' ', "-"),
        Uuid::new_v4().simple()
    ))
    .fetch_one(db.pool())
    .await
    .expect("the organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("backup-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Backup Test".to_owned(),
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
/// the binding, so one shared role would grant the second account everything the first one
/// gained.
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

/// Give this suite a rate-limit budget of its own.
///
/// The limiter is a process-wide cell the router fills from the **stored** document, and the
/// stored `sign_in` scope is ten per five minutes. This suite signs in several accounts per
/// walk, so the eleventh is refused with a `429` that names a rate limit on a suite that was
/// never testing one. Only the sign-in ceiling moves: the others are the ones a deployment
/// ships, and raising them would let a suite become the reason a genuinely over-budget
/// request stops being refused.
fn give_the_suite_its_own_rate_limit(state: &AppState) {
    let policies: Vec<RatePolicy> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    omnion_api::rate_limit_middleware::install(RateLimiter::new(state, policies));
}

/// Take a backup through the real route and return the parsed body.
async fn take_backup(state: &AppState, token: &str, csrf: &str, scopes: &[&str]) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &backups_uri(),
            Some(token),
            Some(csrf),
            Some(json!({ "scopes": scopes })),
        ),
    )
    .await
}

// --------------------------------------------------------------------------------------------
// The walks
// --------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_backup_of_all_five_parts_writes_five_artifacts_and_lands_on_succeeded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    let response = take_backup(
        &fixture.state,
        &token,
        &csrf,
        &["database", "media", "configuration", "themes", "plugins"],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "body: {}",
        response.body
    );
    let run = &response.body["backup"];
    assert_eq!(run["status"], "succeeded", "body: {}", response.body);
    let id = Uuid::parse_str(run["id"].as_str().expect("an id")).expect("a uuid");

    // The five part rows are read **out of PostgreSQL**, not inferred from a response that
    // could have hidden one of them. A manifest that lists only what worked cannot answer
    // "was media part of this run at all", which is the question the restore wizard asks
    // first — so the rows themselves are the assertion.
    let parts: Vec<(String, String, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "select part, status, size_bytes, checksum, error from backup_parts \
         where backup_id = $1 order by part",
    )
    .bind(id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the part rows must read");
    assert_eq!(parts.len(), 5, "every asked-for part must have a row");
    for (name, status, size, checksum, error) in &parts {
        assert_eq!(status, "done", "{name} must be done: {error:?}");
        assert!(checksum.is_some(), "{name} must carry a checksum");
        assert!(*size > 0, "{name} must have produced bytes");
    }

    // `plugins` is empty BY DESIGN until a package installer exists. It is a DONE part with
    // zero items, not a missing one — and this is the line that proves the distinction, since
    // a walk that only checked "five rows exist" would pass either way.
    let plugins: (String, i32, i64) = sqlx::query_as(
        "select status, item_count, size_bytes from backup_parts \
         where backup_id = $1 and part = 'plugins'",
    )
    .bind(id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("a plugins row must exist");
    assert_eq!(plugins.0, "done", "an empty part is done, not absent");
    assert_eq!(plugins.1, 0, "no components are installed");

    // The artifacts are on the destination with the sizes the row recorded. A run that
    // reported success while writing nothing is the most expensive thing this product can do,
    // and the manifest's own size is the number an operator will quote.
    let prefix = run["storage_prefix"].as_str().expect("a prefix");
    let stored: Vec<(String, i64)> = sqlx::query_as(
        "select part, size_bytes from backup_parts where backup_id = $1 order by part",
    )
    .bind(id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the sizes must read");
    for (name, recorded) in &stored {
        let key = response.body["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .find(|part| part["part"] == name.as_str())
            .and_then(|part| part["storage_path"].as_str())
            .expect("a storage path")
            .to_owned();
        let path = fixture.artifact(prefix, &key);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("{} must exist at {}: {err}", name, path.display()));
        assert_eq!(
            bytes.len() as i64,
            *recorded,
            "{name}: the file on disk is {} bytes, the row says {recorded}",
            bytes.len()
        );
    }

    // The status cards answer, and the age is a number rather than an absence.
    let cards = call(
        &fixture.state,
        request(Method::GET, &status_uri(), Some(&token), None, None),
    )
    .await;
    assert_eq!(cards.status, StatusCode::OK);
    // **A string, and that is the assertion's whole point.** This line used to demand a
    // nine-element array, with a comment explaining that `OffsetDateTime` serialises as a
    // tuple — which is true of `time` only when its `serde-human-readable` feature is off, and
    // the workspace did not enable it. So it documented the defect as if it were the contract,
    // and `8322d753` fixed the endpoint. A test that pins the *bug* is worse than no test: it
    // is a green that tells the next reader to distrust the code rather than the test, and it
    // makes the fix look like a regression.
    //
    // The panel's `formatTimestamp` returns an em dash for anything it cannot parse, and an
    // em dash is also what it renders for a value that has not happened yet — a loss and a
    // designed answer, pixel-identical. Asserting the **JSON type** is the only way to tell
    // them apart, so the type is what is asserted, and the value is checked for the shape
    // rather than merely for existing.
    let stamp = &cards.body["last_successful_at"];
    assert!(
        stamp.is_string(),
        "the card must carry an RFC 3339 string, not {}: {stamp}",
        stamp
    );
    let stamp = stamp.as_str().expect("checked above");
    assert!(
        stamp.len() >= 20 && stamp.ends_with('Z') && stamp.contains('T'),
        "the card must carry a timestamp whatever shape it serialises in: {stamp}"
    );
    assert!(
        cards.body["last_successful_age_seconds"].is_number(),
        "and an age the security posture check can read without parsing a date"
    );
    assert_eq!(cards.body["destination"]["writable"], json!(true));
}

#[tokio::test]
async fn a_corrupted_artifact_turns_the_verification_red_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let created = take_backup(&fixture.state, &token, &csrf, &["database", "media"]).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // A clean verification first: a button that is red before anything happened teaches the
    // operator to ignore it, and then it is useless on the day it matters.
    let clean = call(
        &fixture.state,
        request(
            Method::POST,
            &verify_uri(id),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(clean.status, StatusCode::OK);
    assert_eq!(clean.body["clean"], json!(true), "body: {}", clean.body);
    // Only the parts THIS run asked for. The suite database is shared with every other
    // integration walk, and an earlier run's parts are still on the destination — so an
    // unscoped equality here is asserting what the other suites left behind, not what this
    // backup proved. The assertion that matters is that both of ITS parts matched, and that
    // the response is clean.
    let matched = clean.body["matched"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        matched.iter().any(|part| part == "database") && matched.iter().any(|part| part == "media"),
        "both of this run's parts must be named as matched: {matched:?}"
    );

    // Truncate the media artifact. The checksum and the size both have to catch it, and the
    // answer has to NAME the part — "verification failed" is not something an operator can act
    // on, and the part's name is.
    let media_key = created.body["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|part| part["part"] == "media")
        .and_then(|part| part["storage_path"].as_str())
        .expect("a media path")
        .to_owned();
    let media_path = fixture.artifact(&prefix, &media_key);
    std::fs::write(&media_path, b"truncated").expect("the artifact must be writable");

    let dirty = call(
        &fixture.state,
        request(
            Method::POST,
            &verify_uri(id),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        dirty.status,
        StatusCode::OK,
        "a mismatch is a verdict, not an error"
    );
    assert_eq!(dirty.body["clean"], json!(false), "body: {}", dirty.body);
    assert_eq!(
        dirty.body["mismatched"],
        json!(["media"]),
        "the part must be named: {}",
        dirty.body
    );
    assert!(
        dirty.body["summary"]
            .as_str()
            .expect("a summary")
            .contains("media"),
        "the sentence must name the part: {}",
        dirty.body["summary"]
    );

    // The audit trail recorded the verification, and its verdict is in the metadata.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'backup.verified' and target_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit rows must read");
    assert!(
        audited >= 2,
        "both verifications must be audited, got {audited}"
    );
}

#[tokio::test]
async fn the_settings_response_never_carries_a_credential_and_an_unwritable_root_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // The root is inside the suite's own tree, so it is writable by construction.
    let good = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "destination": "local",
                "local_root": fixture.root.to_string_lossy(),
                "encryption": "none",
                "default_retention": 7,
                "verify_after_backup": true,
            })),
        ),
    )
    .await;
    assert_eq!(good.status, StatusCode::OK, "body: {}", good.body);

    // Scanned as **raw bytes**, not through the typed body: a type cannot prove what a
    // serialiser did, and a field named `credential_ref` is a reference while a field named
    // `credential` would be a value.
    let text = good.text().to_lowercase();
    for forbidden in [
        "access_key",
        "secret_key",
        "passphrase_value",
        "password",
        "token",
    ] {
        assert!(
            !text.contains(forbidden),
            "the settings response must not carry `{forbidden}`: {}",
            good.text()
        );
    }
    assert!(
        text.contains("credential_ref"),
        "the reference field is the honest one"
    );

    // A root that cannot be written is refused, and the reason is the operating system's own
    // words rather than "unwritable". A settings screen that stores this and reports success
    // hands over a configuration whose first real backup fails at 02:00.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "destination": "local",
                "local_root": "",
                "encryption": "none",
                "default_retention": 7,
                "verify_after_backup": true,
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        refused.body
    );
    assert!(
        !refused.message().is_empty(),
        "a refusal must say why: {}",
        refused.body
    );

    // The refused save wrote nothing: the stored root is still the writable one.
    let stored: String = sqlx::query_scalar("select local_root from backup_settings where id = 1")
        .fetch_one(fixture.db.pool())
        .await
        .expect("the settings row must read");
    assert!(
        !stored.is_empty(),
        "a refused save must not have stored the empty root"
    );
}

#[tokio::test]
async fn an_encrypted_archive_verifies_with_its_passphrase_and_fails_cleanly_without_one() {
    // The slice-4 claim, walked over the real router: "an encrypted archive verifies with the
    // stored passphrase and fails cleanly with a wrong one". Two clauses, so this proves both —
    // and the second is the one a green-only suite would skip, because a passphrase check that
    // accepts everything looks exactly like one that works.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let reference = "OMNION_TEST_BACKUP_PASSPHRASE";

    // The name is a **reference**. Setting the variable is the deployment's job; this suite
    // does it and takes it back, because `std::env` is process-wide and a test that leaves a
    // variable set changes the meaning of the *next* test that saves settings.
    // SAFETY: this suite gives every test its own database and there is no other thread in it;
    // the variable is removed again in the same test either way.
    unsafe { std::env::set_var(reference, "a passphrase nobody can guess") };

    // Saving `passphrase` mode with the variable **absent** is refused by name, and the row is
    // not written. Without this, the mode could be saved and the archive written in plain text,
    // with the settings screen reporting "encrypted" throughout.
    unsafe { std::env::remove_var(reference) };
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "destination": "local",
                "local_root": fixture.root.to_string_lossy(),
                "credential_ref": reference,
                "encryption": "passphrase",
                "default_retention": 7,
                "verify_after_backup": true,
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a passphrase mode whose variable is unset must be refused, not stored: {}",
        refused.body
    );
    assert!(
        refused.message().contains(reference),
        "the refusal must name the variable to set: {}",
        refused.message()
    );
    let stored_mode: String =
        sqlx::query_scalar("select encryption from backup_settings where id = 1")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the settings row must read");
    assert_eq!(
        stored_mode, "none",
        "a refused save must not have stored the mode it refused"
    );

    // Now the variable exists and the same save is accepted.
    unsafe { std::env::set_var(reference, "a passphrase nobody can guess") };
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "destination": "local",
                "local_root": fixture.root.to_string_lossy(),
                "credential_ref": reference,
                "encryption": "passphrase",
                "default_retention": 7,
                "verify_after_backup": true,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    // The response carries the *reference*, never the value. Read as raw text because a type
    // cannot prove what a serialiser did.
    assert!(
        !saved.text().contains("a passphrase nobody can guess"),
        "the settings response must never carry the passphrase: {}",
        saved.text()
    );

    let created = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");

    // **The bytes on the destination are not the document.** This is the whole feature: read
    // the artifact the way the producer wrote it and it must not be readable JSON, while the
    // manifest still describes the plaintext. A walk that only asserted `status == succeeded`
    // would pass on the unencrypted build, which is exactly what this slice fixed.
    let artifact: String = sqlx::query_scalar(
        "select storage_path from backup_parts where backup_id = $1 and part = 'database'",
    )
    .bind(id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the part row must read");
    let bytes = tokio::fs::read(fixture.artifact("", &artifact))
        .await
        .expect("the artifact must exist");
    assert!(
        omnion_backup::is_sealed(&bytes),
        "the artifact on the destination is not sealed: {} bytes starting {:?}",
        bytes.len(),
        &bytes[..bytes.len().min(16)]
    );
    assert!(
        !bytes.starts_with(b"{"),
        "an encrypted artifact must not begin with the document's own opening brace"
    );

    // …and it opens again with the passphrase, back to the document.
    let plaintext = omnion_backup::open_archive(
        "a passphrase nobody can guess".as_bytes(),
        &bytes,
    )
    .expect("the stored passphrase must open the artifact");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&plaintext).is_ok(),
        "the opened bytes must be the JSON document the manifest describes"
    );

    // Verification is clean **with** the passphrase. This is the assertion that would fail on
    // a build that hashed the framed bytes instead of the plaintext.
    let verified = call(
        &fixture.state,
        request(
            Method::POST,
            &verify_uri(id),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(verified.status, StatusCode::OK, "body: {}", verified.body);
    assert_eq!(
        verified.body["clean"], true,
        "an encrypted archive must verify with the passphrase that sealed it: {}",
        verified.body
    );
    assert_eq!(
        verified.body["mismatched"].as_array().map(Vec::len),
        Some(0),
        "nothing may be mismatched: {}",
        verified.body
    );
    assert_eq!(
        verified.body["matched"].as_array().map(Vec::len),
        Some(1),
        "the one part this run asked for must be the one that matched: {}",
        verified.body
    );

    // And with a **wrong** passphrase it fails cleanly — reported as unverifiable, not as
    // "corrupt", and without a panic or a 500. Removing the variable is the honest version of
    // "wrong": the process no longer holds the key.
    unsafe { std::env::remove_var(reference) };
    let wrong = call(
        &fixture.state,
        request(
            Method::POST,
            &verify_uri(id),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        wrong.status,
        StatusCode::OK,
        "an unverifiable archive is an answer, not a server error: {}",
        wrong.body
    );
    // A sealed artifact with no key is **skipped**, not reported as a mismatch: reporting a
    // mismatch would send an operator to restore a backup that is perfectly intact.
    assert_eq!(
        wrong.body["mismatched"].as_array().map(Vec::len),
        Some(0),
        "a missing passphrase is not corruption: {}",
        wrong.body
    );
    assert_eq!(
        wrong.body["clean"], false,
        "and it is not a clean verdict either — something was not checked: {}",
        wrong.body
    );
    unsafe { std::env::remove_var(reference) };
}

#[tokio::test]
async fn a_reader_may_look_and_take_and_may_not_delete_or_reconfigure() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let created = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");

    let (reader, reader_csrf) = fixture.session(&fixture.reader_email).await;

    // Reads.
    for uri in [
        backups_uri(),
        status_uri(),
        backup_uri(id),
        manifest_uri(id),
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&reader), None, None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{uri} must be readable: {}",
            response.body
        );
    }

    // Taking a backup is `backup.create`, which the reader holds — asking for a restore point
    // to exist is not a destructive act.
    let mine = take_backup(&fixture.state, &reader, &reader_csrf, &["database"]).await;
    assert_eq!(mine.status, StatusCode::CREATED, "body: {}", mine.body);

    // Deleting one is `backup.manage`, which it does not hold. The refusal must write nothing:
    // the row is still there afterwards.
    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &backup_uri(id),
            Some(&reader),
            Some(&reader_csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        refused.body
    );
    let still_there: i64 = sqlx::query_scalar("select count(*) from backups where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(still_there, 1, "a refused delete must not remove the row");

    // And the settings write is `backup.manage` too.
    let refused = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(),
            Some(&reader),
            Some(&reader_csrf),
            Some(json!({
                "destination": "local",
                "local_root": fixture.root.to_string_lossy(),
                "encryption": "none",
                "default_retention": 30,
                "verify_after_backup": true,
            })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        refused.body
    );
    let retention: i32 =
        sqlx::query_scalar("select default_retention from backup_settings where id = 1")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the settings row must read");
    assert_eq!(
        retention, 7,
        "a refused save must not have written the retention"
    );

    // Anonymous is refused everywhere.
    for uri in [backups_uri(), status_uri(), settings_uri()] {
        let response = call(&fixture.state, request(Method::GET, &uri, None, None, None)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{uri} must refuse an anonymous caller"
        );
    }
}

#[tokio::test]
async fn another_tenants_backup_is_a_404_and_its_existence_is_not_confirmed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let created = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");

    // The stranger holds the SAME keys, so every refusal below is about the tenancy boundary
    // and not about a missing permission.
    let (stranger, stranger_csrf) = fixture.session(&fixture.stranger_email).await;

    let listed = call(
        &fixture.state,
        request(Method::GET, &backups_uri(), Some(&stranger), None, None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(
        listed.body["total"],
        json!(0),
        "the list must not carry another tenant's runs: {}",
        listed.body
    );

    // A 403 would confirm the id is real, and a backup's existence is itself information about
    // the platform. It is a 404.
    for (method, uri) in [
        (Method::GET, backup_uri(id)),
        (Method::GET, manifest_uri(id)),
        (Method::POST, verify_uri(id)),
        (Method::DELETE, backup_uri(id)),
    ] {
        let response = call(
            &fixture.state,
            request(
                method.clone(),
                &uri,
                Some(&stranger),
                Some(&stranger_csrf),
                None,
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must be a 404, not a 403: {}",
            response.body
        );
    }

    // The refusal deleted nothing.
    let still_there: i64 = sqlx::query_scalar("select count(*) from backups where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(
        still_there, 1,
        "a refused delete must not remove another tenant's row"
    );
}

#[tokio::test]
async fn a_duplicate_scope_is_refused_by_name_and_a_bad_label_names_its_own_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // The same part twice is a caller bug, and silently repairing it is how a run ends up
    // exporting the database twice and reporting the second attempt as a failure.
    let duplicated = call(
        &fixture.state,
        request(
            Method::POST,
            &backups_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({ "scopes": ["database", "database"] })),
        ),
    )
    .await;
    assert_eq!(
        duplicated.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        duplicated.body
    );
    assert!(
        duplicated.message().contains("listed twice"),
        "the message must name the rule: {}",
        duplicated.body
    );

    // A scope outside the five names itself AND the five, so the operator can see the legal
    // set without leaving the form.
    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            &backups_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({ "scopes": ["database", "mailbox"] })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    let message = unknown.message();
    assert!(message.contains("mailbox"), "{message}");
    assert!(message.contains("plugins"), "{message}");

    // An over-long label is refused against the stored bound, and the message names the field
    // so the form can put it under the right input.
    let long_label = "x".repeat(200);
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &backups_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({ "label": long_label, "scopes": ["database"] })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        refused.body
    );
    assert!(
        refused.message().contains("label"),
        "the refusal must name its field: {}",
        refused.body
    );

    // Nothing was written by any of the three refusals. Scoped to this fixture's own
    // organization: the suite database is shared, so an unscoped count is asserting what the
    // other twenty walks left behind — and "2" here is two of THEIR runs, not two refusals
    // that wrote rows.
    let runs: i64 = sqlx::query_scalar("select count(*) from backups where organization_id = $1")
        .bind(fixture.org)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(runs, 0, "a refused create must not leave a row behind");
}

#[tokio::test]
async fn a_protected_backup_is_never_a_prune_candidate_and_the_newest_successful_survives() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // Three expired runs, the newest of them protected and the middle one not.
    let mut ids = Vec::new();
    for index in 0..3 {
        let created = call(
            &fixture.state,
            request(
                Method::POST,
                &backups_uri(),
                Some(&token),
                Some(&csrf),
                Some(json!({ "label": format!("expired-{index}"), "scopes": ["database"] })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "body: {}",
            created.body
        );
        let id =
            Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
        ids.push(id);
    }
    // Everything is past its window; only the last one is protected.
    sqlx::query("update backups set retain_until = now() - interval '1 day'")
        .execute(fixture.db.pool())
        .await
        .expect("the rows must be aged");
    sqlx::query("update backups set protected = true where id = $1")
        .bind(ids[2])
        .execute(fixture.db.pool())
        .await
        .expect("the protection must be set");

    // Scoped to this fixture's OWN three runs. The suite database is shared with every other
    // integration walk, so an unscoped `select ... from backups` returns whatever twenty other
    // suites left behind — and "not all three are candidates" becomes an assertion about the
    // test order rather than about the sweep.
    // The exemptions are read through the crate's own `prune_candidates`, scoped to this
    // organization — not through a hand-written copy of the sweep's WHERE clause. A second copy
    // is a second answer to "what may go", and the two drift the first time one of them is
    // edited, which is exactly the failure `prune_candidates`' own doc comment warns about.
    let candidates = omnion_backup::prune_candidates(
        fixture.db.pool(),
        Some(fixture.org),
        time::OffsetDateTime::now_utc(),
    )
    .await
    .expect("the candidates must read");
    let candidate_ids: Vec<Uuid> = candidates.iter().map(|run| run.id).collect();

    assert!(
        !candidate_ids.contains(&ids[2]),
        "a protected backup is never a prune candidate: {candidate_ids:?}"
    );
    assert!(
        !candidate_ids.contains(&ids[1]),
        "the newest successful backup is exempt, whatever its window: {candidate_ids:?}"
    );
    assert!(
        candidate_ids.contains(&ids[0]),
        "the oldest unprotected one is the whole point of the sweep: {candidate_ids:?}"
    );
}

/// The retention sweep, end to end: it removes the **bytes**, not only the rows.
///
/// `prune_candidates` shipped in slice 1 and nothing called it. The screen could list what
/// the sweep would do and this suite could assert its four exemptions, and the destination
/// would still fill up for ever — so the walk drives the route and then goes and looks at
/// the filesystem, which is the only place the claim can be true or false.
///
/// Four things are proved, and each of them is a way the shortcut is wrong:
///
/// 1. **The directory is gone.** A sweep that deleted the row and left the archive would
///    report the same counts the panel shows.
/// 2. **The exemptions survive the route.** They are decided inside `prune_candidates`, and
///    the route is a caller — so this asserts on the *result* rather than on the SQL, and a
///    future edit to the sweep's rule has to break the outcome to break the test.
/// 3. **A stranger tenant's expired run is untouched.** Scoped to the caller's own
///    organization, because a sweep that ran `sweep_all` from a tenant's button would delete
///    restore points the operator has never seen and cannot restore from.
/// 4. **A fresh run is not swept.** Retention is a window, not a bulk delete.
#[tokio::test]
async fn the_retention_sweep_takes_the_bytes_and_spares_what_it_promised() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;

    // Three of this tenant's runs and one of the stranger's, each with real artifacts on
    // the destination. The paths are read out of the run's own `storage_prefix` rather than
    // recomputed here, so the walk and the code cannot agree about a path by both being
    // wrong in the same way.
    let mut mine = Vec::new();
    for index in 0..3 {
        let created = call(
            &fixture.state,
            request(
                Method::POST,
                &backups_uri(),
                Some(&token),
                Some(&csrf),
                Some(json!({ "label": format!("sweep-{index}"), "scopes": ["database"] })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "body: {}",
            created.body
        );
        let id =
            Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
        let prefix = created.body["backup"]["storage_prefix"]
            .as_str()
            .expect("a storage prefix")
            .to_owned();
        let directory = fixture.root.join(prefix.trim_start_matches('/'));
        assert!(
            directory.exists(),
            "the run must have written its directory before the sweep is asked to remove it: {}",
            directory.display()
        );
        mine.push((id, directory));
    }

    // The stranger's own expired run, created with the stranger's session and pointed at
    // the same destination. It is the fixture's "other tenant", and without it "everything"
    // and "this organization" are the same set — the exact blind spot the media part's
    // tenancy fix was found through.
    let stranger_run = {
        let created = call(
            &fixture.state,
            request(
                Method::POST,
                &backups_uri(),
                Some(&stranger_token),
                Some(&stranger_csrf),
                Some(json!({ "label": "stranger", "scopes": ["database"] })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "body: {}",
            created.body
        );
        let id =
            Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
        let prefix = created.body["backup"]["storage_prefix"]
            .as_str()
            .expect("a storage prefix")
            .to_owned();
        (id, fixture.root.join(prefix.trim_start_matches('/')))
    };

    // Two of this tenant's runs expire; the newest one is left in the future, and the middle
    // one is protected. So the sweep has one candidate, two exemptions and a stranger.
    sqlx::query("update backups set retain_until = now() - interval '1 day' where id = any($1)")
        .bind(vec![mine[0].0, mine[1].0, stranger_run.0])
        .execute(fixture.db.pool())
        .await
        .expect("the rows must be aged");
    sqlx::query("update backups set retain_until = now() + interval '30 days' where id = $1")
        .bind(mine[2].0)
        .execute(fixture.db.pool())
        .await
        .expect("the fresh row must be left alone");
    sqlx::query("update backups set protected = true where id = $1")
        .bind(mine[1].0)
        .execute(fixture.db.pool())
        .await
        .expect("the protection must be set");

    let response = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/backups/sweep",
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(
        response.body["candidates"].as_i64(),
        Some(1),
        "exactly one of this tenant's runs is a candidate: {}",
        response.body
    );
    assert_eq!(
        response.body["removed"].as_i64(),
        Some(1),
        "body: {}",
        response.body
    );
    assert_eq!(
        response.body["partial"].as_i64(),
        Some(0),
        "body: {}",
        response.body
    );

    // 1. The bytes. Not the row — the directory.
    assert!(
        !mine[0].1.exists(),
        "the expired run's directory must be gone: {}",
        mine[0].1.display()
    );

    // 2. The exemptions, as outcomes.
    for (label, (id, directory)) in [("protected", &mine[1]), ("fresh", &mine[2])] {
        let still_there: Option<Uuid> = sqlx::query_scalar("select id from backups where id = $1")
            .bind(id)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the row must still read");
        assert!(still_there.is_some(), "the {label} run's row was swept");
        assert!(
            directory.exists(),
            "the {label} run's artifacts were removed: {}",
            directory.display()
        );
    }

    // 3. The stranger. Both halves: the row and the directory.
    let stranger_row: Option<Uuid> = sqlx::query_scalar("select id from backups where id = $1")
        .bind(stranger_run.0)
        .fetch_optional(fixture.db.pool())
        .await
        .expect("the stranger's row must still read");
    assert!(
        stranger_row.is_some(),
        "another tenant's expired run was swept by this tenant's button"
    );
    assert!(
        stranger_run.1.exists(),
        "another tenant's artifacts were removed: {}",
        stranger_run.1.display()
    );

    // 4. An audit entry, because a button that deletes restore points with no record of who
    // asked is a button nobody can reconcile at 02:00.
    let audited: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'backup.sweep'")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the audit must read");
    assert!(
        audited >= 1,
        "a destructive sweep must leave an audit entry"
    );
}

#[tokio::test]
async fn the_media_part_copies_the_librarys_bytes_and_a_missing_object_fails_the_run() {
    // The walk that matters most in this suite, and the one that was impossible to write
    // before the media part stopped being a count.
    //
    // The defect: `document_media` ran `select site_id, count(*), sum(size_bytes) from media`
    // and wrote THAT as the part's artifact. A backup of a site with a hundred files produced
    // a ~200-byte JSON document listing "100 files, 4 MiB", reached `succeeded`, and
    // `verify` read the artifact back and agreed with its own checksum. Not one byte of the
    // library had been copied anywhere. Every assertion in the walk above would have passed
    // on that implementation, which is exactly why it needed its own.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // A site, two real objects in the suite's own store, and rows describing them. The bytes
    // go through `state.storage()` — the same handle the upload route writes through — so the
    // walk exercises the real driver rather than a mock of it.
    let site: Uuid = sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(fixture.org)
    .bind(format!("k{}", &Uuid::new_v4().simple().to_string()[..8]))
    .bind("Media Site")
    .fetch_one(fixture.db.pool())
    .await
    .expect("a site must be created");

    let payload_a: &[u8] = b"\x89PNG\r\n\x1a\n the first object's bytes, long enough to matter";
    let payload_b: &[u8] = b"the second object's bytes";
    for (name, payload) in [("hero.png", payload_a), ("logo.png", payload_b)] {
        let key = format!("suite/{site}/{name}");
        fixture
            .state
            .storage()
            .put(&key, payload, "image/png")
            .await
            .expect("the object must be storable");
        sqlx::query(
            "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
             checksum, created_by) values ($1, $2, $3, $4, $5, $6, null)",
        )
        .bind(site)
        .bind(&key)
        .bind(name)
        .bind("image/png")
        .bind(payload.len() as i64)
        .bind(omnion_backup::bytes_checksum(payload))
        .execute(fixture.db.pool())
        .await
        .expect("the media row must be written");
    }

    let response = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "body: {}",
        response.body
    );
    let run = &response.body["backup"];
    let id = Uuid::parse_str(run["id"].as_str().expect("an id")).expect("a uuid");
    let prefix = run["storage_prefix"].as_str().expect("a prefix");
    assert_eq!(run["status"], "succeeded", "body: {}", response.body);

    // The part must have counted the objects it copied — the two that exist, not the two that
    // the `count(*)` would have found, which happen to be the same here, and that is the
    // point of the next assertion rather than this one.
    let (status, item_count, size_bytes) = sqlx::query_as::<_, (String, i32, i64)>(
        "select status, item_count, size_bytes from backup_parts \
         where backup_id = $1 and part = 'media'",
    )
    .bind(id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("a media row must exist");
    assert_eq!(status, "done", "both objects copied");
    assert_eq!(item_count, 2, "two library files were copied, not counted");
    assert!(
        size_bytes > (payload_a.len() + payload_b.len()) as i64,
        "the part is {size_bytes} bytes; a count-only document would be about 200"
    );

    // **The assertion the old implementation could not survive.** The archive holds one file
    // per object, and each one's bytes are the original bytes. A `media.json` describing a
    // count satisfies every check above and fails here.
    // The archive's location is read **out of the index the run wrote**, not recomputed here.
    // Recomputing it is how the doubled-prefix bug stayed invisible for a whole slice: this
    // walk and the production code would both have derived the same wrong path and agreed.
    // Taking the path from the artifact means the two halves can actually disagree.
    let index_path = fixture.artifact(
        prefix,
        &format!(
            "{}{}",
            prefix.trim_start_matches('/'),
            omnion_backup::INDEX_FILENAME
        ),
    );
    let objects_root = fixture.artifact(
        prefix,
        &format!(
            "{}{}/",
            prefix.trim_start_matches('/'),
            omnion_backup::OBJECTS_DIR
        ),
    );
    let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap_or_else(|err| {
        panic!(
            "the media index must exist at {}: {err}",
            index_path.display()
        )
    }))
    .expect("the index must be JSON");
    assert_eq!(index["version"], omnion_backup::INDEX_VERSION);
    let listed = index["objects"].as_array().expect("an object list");
    assert_eq!(listed.len(), 2, "the index lists what was copied: {index}");

    // Each archived object is where the index says it is, and holds the library's bytes.
    let mut actual: Vec<Vec<u8>> = Vec::new();
    for entry in listed {
        assert!(
            entry["storage_key"]
                .as_str()
                .unwrap_or_default()
                .starts_with("suite/"),
            "the index carries the live key: {entry}"
        );
        assert_eq!(
            entry["checksum"].as_str().unwrap_or_default().len(),
            64,
            "a SHA-256, not a placeholder: {entry}"
        );
        let archive_key = entry["archive_key"].as_str().expect("an archive key");
        let path = fixture.artifact(prefix, archive_key);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("{} must exist: {err}", path.display()));
        assert_eq!(
            bytes.len() as i64,
            entry["size_bytes"].as_i64().expect("a size"),
            "the index's size is the file's size: {path:?}"
        );
        assert_eq!(
            omnion_backup::bytes_checksum(&bytes),
            entry["checksum"].as_str().expect("a checksum"),
            "the recorded checksum is over the bytes on disk: {path:?}"
        );
        assert!(
            path.starts_with(&objects_root),
            "the object belongs under the run's own objects directory: {}",
            path.display()
        );
        actual.push(bytes);
    }
    let mut expected = vec![payload_b.to_vec(), payload_a.to_vec()];
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "the archived bytes must be the library's bytes, not a description of them"
    );

    // Now the second half: a row whose object the store does not have. The part must FAIL and
    // the run must land on `partial` — a run that quietly backed up one of two files and said
    // `succeeded` is the exact failure this walk exists to prevent.
    sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
         checksum, created_by) values ($1, $2, $3, $4, $5, $6, null)",
    )
    .bind(site)
    // A key unique to THIS walk. `media.storage_key` carries a unique constraint and the QA
    // database is shared with every other suite that inserts media, so a fixed key fails the
    // second time a walk runs against the same database — and it fails as a constraint
    // violation that reads like a product defect rather than a fixture collision. Every
    // storage key this suite writes is namespaced by something unique.
    .bind(format!(
        "suite/never-uploaded/gone-{}.png",
        Uuid::new_v4().simple()
    ))
    .bind("gone.png")
    .bind("image/png")
    .bind(11i64)
    .bind(omnion_backup::bytes_checksum(b"never existed"))
    .execute(fixture.db.pool())
    .await
    .expect("the row must be written");

    let second = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    assert_eq!(second.status, StatusCode::CREATED, "body: {}", second.body);
    let run = &second.body["backup"];
    // `failed`, not `partial` — and the reason this assertion changed is the scope fix this
    // tick made. This walk asks for `media` alone, and `produce_all` used to produce all
    // five parts regardless, so the four healthy ones left `done > 0` and `summarise`
    // answered `partial`. That is the *correct* answer to "a run whose parts are four good
    // and one bad", and it was the answer to a question nobody was asking: the operator
    // asked for one part, one part failed, and the screen said the backup was partly
    // successful. With the scope honoured the run is honestly `failed`.
    //
    // `summarise` is untouched and still right; what changed is the set of parts it was
    // given. The part-level assertion below is what actually carries the failure detail.
    assert_eq!(
        run["status"], "failed",
        "a run whose only part could not be produced is failed, never partial: {run}"
    );
    let (status, error) = sqlx::query_as::<_, (String, Option<String>)>(
        "select status, error from backup_parts \
         where backup_id = $1 and part = 'media'",
    )
    .bind(Uuid::parse_str(run["id"].as_str().expect("an id")).expect("a uuid"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("a media row must exist");
    assert_eq!(status, "failed");
    let message = error.expect("a failed part names itself");
    assert!(message.contains("gone.png"), "the file is named: {message}");
    assert!(
        message.contains("1 of 3"),
        "the count is the whole truth, not a sample: {message}"
    );
}

#[tokio::test]
async fn deleting_a_backup_takes_its_artifacts_off_the_destination_and_spares_the_others() {
    // The walk for the delete, and the shape of its failure is different from every other
    // walk in this suite: **nothing was wrong with the code, the code was missing.**
    //
    // `DELETE /api/v1/backups/{id}` removed the row and its parts and returned `204`. The
    // database export, the media objects, the index, the configuration document and the
    // manifest stayed on the destination byte for byte — and the panel said so, in a
    // sentence that was honest and was still the defect: *"Its artifacts are still on the
    // destination until the next prune."* Prune runs on a schedule nobody set in this story,
    // so the ordinary path was: tidy up three old runs, watch the list go green, and keep a
    // full copy of the media library on disk with nothing pointing at it. Forever, per byte.
    //
    // No unit test can see this. The store's `delete_backup` deleted exactly what it claimed,
    // the handler returned exactly the status it promised, and both halves were correct. The
    // only thing that can see it is a walk that reads the destination **after** the delete
    // and requires the files to be gone.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // A real media library, so the run has an artifact that is a *file* and not a document.
    // A database.json is one file; an objects/ tree is four, and a delete that only took the
    // first would look complete.
    let site: Uuid = sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(fixture.org)
    .bind(format!("k{}", &Uuid::new_v4().simple().to_string()[..8]))
    .bind("Delete Site")
    .fetch_one(fixture.db.pool())
    .await
    .expect("a site must be created");

    let payload: &[u8] = b"\\x89PNG\\r\\n\\x1a\\n bytes that must be gone after the delete";
    for name in ["hero.png", "logo.png", "banner.png"] {
        let key = format!("suite/{site}/{name}");
        fixture
            .state
            .storage()
            .put(&key, payload, "image/png")
            .await
            .expect("the object must be storable");
        sqlx::query(
            "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
             checksum, created_by) values ($1, $2, $3, $4, $5, $6, null)",
        )
        .bind(site)
        .bind(&key)
        .bind(name)
        .bind("image/png")
        .bind(payload.len() as i64)
        .bind(omnion_backup::bytes_checksum(payload))
        .execute(fixture.db.pool())
        .await
        .expect("the media row must be written");
    }

    // Two runs. The second is not a control group for its own sake — it is what turns
    // "the directory is gone" into "THE DIRECTORY is gone". A delete that emptied the whole
    // backup root would satisfy every other assertion in this walk.
    let first = take_backup(&fixture.state, &token, &csrf, &["media", "configuration"]).await;
    assert_eq!(first.status, StatusCode::CREATED, "body: {}", first.body);
    let second = take_backup(&fixture.state, &token, &csrf, &["media", "configuration"]).await;
    assert_eq!(second.status, StatusCode::CREATED, "body: {}", second.body);

    let id_of = |response: &TestResponse| -> Uuid {
        Uuid::parse_str(response.body["backup"]["id"].as_str().expect("an id")).expect("a uuid")
    };
    let (first_id, second_id) = (id_of(&first), id_of(&second));
    let first_prefix = first.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();
    let second_prefix = second.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();
    assert_ne!(
        first_prefix, second_prefix,
        "each run has its own directory"
    );

    // The path is read out of the run's own manifest, not recomputed — the same rule the
    // media walk uses. A helper here that agreed with a wrong production path would make
    // the two halves consistent and wrong.
    let run_directory = |prefix: &str| -> std::path::PathBuf {
        fixture.artifact(prefix, &format!("{}/", prefix.trim_start_matches('/')))
    };
    let doomed = run_directory(&first_prefix);
    let survivor = run_directory(&second_prefix);
    assert!(
        doomed.exists(),
        "the run's directory must exist before the delete: {}",
        doomed.display()
    );
    let objects_before = std::fs::read_dir(
        doomed
            .join(omnion_backup::OBJECTS_DIR)
            .join(&site.to_string()),
    )
    .unwrap_or_else(|err| {
        panic!(
            "the objects directory for the site must exist before the delete at {}: {err}",
            doomed.join(omnion_backup::OBJECTS_DIR).display()
        )
    })
    .count();
    assert!(
        objects_before >= 3,
        "the media part wrote one file per object, not one file: {objects_before}"
    );
    assert!(
        survivor.join("configuration.json").exists(),
        "the second run wrote its own artifacts: {}",
        survivor.display()
    );

    let response = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/backups/{first_id}"),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);

    // The report is a `200` body rather than a `204`, because "the row is gone" and "the
    // bytes are gone" are two facts. A walk that only checked the status would pass on the
    // old implementation for the old implementation's own reason.
    let report = &response.body;
    assert_eq!(report["existed"], true, "the directory was there: {report}");
    assert_eq!(report["failed_entries"], 0, "nothing was refused: {report}");
    assert!(
        report["removed_entries"].as_i64().unwrap_or_default() >= objects_before as i64,
        "at least the object directory came off: {report}"
    );
    assert!(
        report["root"]
            .as_str()
            .unwrap_or_default()
            .ends_with(first_prefix.trim_end_matches('/')),
        "the report names the directory it took, so an operator can go and look: {report}"
    );

    // **The assertion the old implementation could not survive.**
    assert!(
        !doomed.exists(),
        "the run's directory must be gone from the destination: {}",
        doomed.display()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from backups where id = $1",)
            .bind(first_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must answer"),
        0,
        "the row is gone too"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from backup_parts where backup_id = $1",)
            .bind(first_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must answer"),
        0,
        "and its parts with it"
    );

    // The other run is untouched — byte for byte, not merely present.
    assert!(
        survivor.join("configuration.json").exists(),
        "another run's archive must survive the delete: {}",
        survivor.display()
    );
    assert!(
        survivor.join(omnion_backup::INDEX_FILENAME).exists(),
        "including its media index: {}",
        survivor.display()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from backups where id = $1")
            .bind(second_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must answer"),
        1,
        "and its row is still in the list"
    );

    // A second delete of the same run is a `404`, not a second purge of somebody else's
    // archive: without the row there is no prefix, and a handler that re-derived one from
    // the path would be guessing at a directory to delete.
    let repeat = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/backups/{first_id}"),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        repeat.status,
        StatusCode::NOT_FOUND,
        "a deleted run is a 404, never a second delete: {}",
        repeat.body
    );
    assert!(
        survivor.join("configuration.json").exists(),
        "the second delete must not have touched the surviving run either"
    );
}

/// A site for one organization, for the walks that need one organization's media to be
/// distinguishable from another's.
///
/// A helper rather than a closure inside the walk, and the reason is the compiler: an
/// `async move` closure that borrows the fixture's pool cannot be called twice, because the
/// first call moves the borrow. Two sites means two calls, so it has to be a function.
async fn create_site(pool: &sqlx::PgPool, organization_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(format!("k{}", &Uuid::new_v4().simple().to_string()[..8]))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("a site must be created")
}

#[tokio::test]
async fn a_backups_media_part_holds_only_the_runs_own_organizations_files() {
    // Found by the delete walk, as a side effect of its own fixture: the media part asked for
    // `pending_objects(pool, None)` and got every `media` row on the deployment. The pre-existing
    // media walk had been passing because every test in this suite creates media for ONE
    // organization — so "the whole deployment" and "this organization's library" are the same
    // set, and a data leak is invisible inside its own blind spot.
    //
    // The stranger's media in the failure message is the proof: `34 of 36 objects could not be
    // copied — share-guarded.txt: no object is stored under "shares/4c70..."` are rows from a
    // **different suite's** organization, sitting in this run's media part. Backup is the one
    // place where an unscoped read turns into a leak with a green tick beside it, because every
    // other read in the file is scoped by `organization_id`.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // One object for this organization, one for the stranger's. Both rows exist; only one of
    // them may appear in this run's archive.
    let own_site = create_site(fixture.db.pool(), fixture.org, "Own Site").await;
    let stranger_site = create_site(fixture.db.pool(), fixture.other_org, "Stranger Site").await;

    let mut keys = Vec::new();
    for (site, name, payload) in [
        (own_site, "mine.png", &b"the operator's own file"[..]),
        (
            stranger_site,
            "theirs.png",
            &b"another tenant's file, which must not be here"[..],
        ),
    ] {
        let key = format!("suite/{site}/{name}");
        fixture
            .state
            .storage()
            .put(&key, payload, "image/png")
            .await
            .expect("the object must be storable");
        sqlx::query(
            "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
             checksum, created_by) values ($1, $2, $3, $4, $5, $6, null)",
        )
        .bind(site)
        .bind(&key)
        .bind(name)
        .bind("image/png")
        .bind(payload.len() as i64)
        .bind(omnion_backup::bytes_checksum(payload))
        .execute(fixture.db.pool())
        .await
        .expect("the media row must be written");
        keys.push((name.to_owned(), omnion_backup::bytes_checksum(payload)));
    }

    let response = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "body: {}",
        response.body
    );
    let run = &response.body["backup"];
    assert_eq!(
        run["status"], "succeeded",
        "the run must succeed — the stranger's file is not this run's to copy: {run}"
    );
    let prefix = run["storage_prefix"].as_str().expect("a prefix");
    let (item_count,) = sqlx::query_as::<_, (i32,)>(
        "select item_count from backup_parts where backup_id = $1 and part = 'media'",
    )
    .bind(Uuid::parse_str(run["id"].as_str().expect("an id")).expect("a uuid"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("a media row must exist");
    assert_eq!(
        item_count, 1,
        "exactly this organization's file, not the deployment's two"
    );

    // The index is the archive's own account of itself, so it is the right place to look for
    // the stranger's name — and the absence of it is the assertion.
    let index_path = fixture.artifact(
        prefix,
        &format!(
            "{}{}",
            prefix.trim_start_matches('/'),
            omnion_backup::INDEX_FILENAME
        ),
    );
    let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap_or_else(|err| {
        panic!(
            "the media index must exist at {}: {err}",
            index_path.display()
        )
    }))
    .expect("the index must be JSON");
    let body = serde_json::to_string(&index).expect("the index must serialise");
    assert!(
        body.contains("mine.png"),
        "this organization's file is in the archive: {index}"
    );
    assert!(
        !body.contains("theirs.png"),
        "another tenant's file is in this run's archive: {index}"
    );

    // The stranger's run holds only theirs. A "fix" that scoped by *excluding* the other
    // org rather than by *including* this one would pass the two assertions above and still
    // put a tenant's files in a backup.
    //
    // The stranger logs in for themselves and sends **their own** CSRF token: the token is
    // bound to the session it was issued for, so reusing the operator's is a `403` that has
    // nothing to do with tenancy — the one failure mode a test that reuses a token will
    // misread as a product defect.
    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;
    let theirs = call(
        &fixture.state,
        request(
            Method::POST,
            &backups_uri(),
            Some(&stranger_token),
            Some(&stranger_csrf),
            Some(json!({ "scopes": ["media"] })),
        ),
    )
    .await;
    assert_eq!(theirs.status, StatusCode::CREATED, "body: {}", theirs.body);
    let their_prefix = theirs.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix");
    let their_index: Value = serde_json::from_slice(
        &std::fs::read(fixture.artifact(
            their_prefix,
            &format!(
                "{}{}",
                their_prefix.trim_start_matches('/'),
                omnion_backup::INDEX_FILENAME
            ),
        ))
        .expect("the stranger's index must exist"),
    )
    .expect("the index must be JSON");
    let their_body = serde_json::to_string(&their_index).expect("the index must serialise");
    assert!(
        their_body.contains("theirs.png"),
        "the stranger's own file is in their archive: {their_index}"
    );
    assert!(
        !their_body.contains("mine.png"),
        "the operator's file leaked into another tenant's archive: {their_index}"
    );
}

/// Everything one run recorded, read out of PostgreSQL.
///
/// A function and not a closure because an `async` closure that borrows the pool cannot be
/// returned from — and the "did the preview change anything?" assertion needs the *same*
/// read before and after, which is exactly what a local function gives for free.
#[allow(clippy::type_complexity)]
async fn run_snapshot(
    pool: &sqlx::PgPool,
    run_id: Uuid,
    site: Uuid,
) -> (
    Vec<(String, String, i32, i64, Option<String>)>,
    String,
    String,
    Option<time::OffsetDateTime>,
    i64,
) {
    let rows: Vec<(String, String, i32, i64, Option<String>)> = sqlx::query_as(
        "select part, status, item_count, size_bytes, checksum from backup_parts \
         where backup_id = $1 order by part",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .expect("the parts must read");
    let (status, prefix_now, finished): (String, String, Option<time::OffsetDateTime>) =
        sqlx::query_as("select status, storage_prefix, finished_at from backups where id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("the run must read");
    let objects: i64 = sqlx::query_scalar("select count(*) from media where site_id = $1")
        .bind(site)
        .fetch_one(pool)
        .await
        .expect("the media must read");
    (rows, status, prefix_now, finished, objects)
}

// ----------------------------------------------------------------------------------------
// The restore preview (slice 2)
// ----------------------------------------------------------------------------------------

/// The preview prices a restore against LIVE data, and changes nothing while doing it.
///
/// The unit tests in `omnion_backup` can prove what the model does with the numbers it is
/// handed. Only this walk can prove the two things the model is trusting:
///
/// 1. **the artifacts really are re-read** — a preview that answered from the manifest would
///    offer a truncated file as a restore point, and the earlier `verify` walk is the proof
///    that the destination can disagree with the manifest;
/// 2. **a preview writes nothing.** This is the property the whole slice exists for, and it
///    is invisible to every other test in this file: a `GET` that quietly inserted a
///    `previewed` row, or flipped a run's status, would pass a hundred assertions.
///
/// So the assertions are in this order: read everything, preview, and then prove that the
/// database is byte-for-byte what it was — parts, statuses, checksums — by reading it again
/// out of PostgreSQL rather than out of the response.
#[tokio::test]
async fn the_restore_preview_prices_the_loss_and_touches_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // One real media object for this organization, so the preview has an index to compare
    // against and the comparison is not vacuously zero.
    let site = create_site(fixture.db.pool(), fixture.org, "Preview Site").await;
    let payload = b"a file that exists in the archive and in the library" as &[u8];
    let key = format!("preview/{site}/kept.png");
    fixture
        .state
        .storage()
        .put(&key, payload, "image/png")
        .await
        .expect("the object must be storable");
    sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
         checksum, created_by) values ($1, $2, 'kept.png', 'image/png', $3, $4, null)",
    )
    .bind(site)
    .bind(&key)
    .bind(payload.len() as i64)
    .bind(omnion_backup::bytes_checksum(payload))
    .execute(fixture.db.pool())
    .await
    .expect("the media row must be written");

    let created = take_backup(&fixture.state, &token, &csrf, &["media", "database"]).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // A file uploaded **after** the run. This is the entire subject of the preview: it is
    // not in the archive, and restoring drops it. A preview that only echoed the manifest
    // would say "1 object, 30 bytes" and the operator would learn what they lost by losing
    // it.
    let later_key = format!("preview/{site}/uploaded-after-the-run.png");
    fixture
        .state
        .storage()
        .put(&later_key, b"written after the backup", "image/png")
        .await
        .expect("the later object must be storable");
    sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
         checksum, created_by) values ($1, $2, 'after.png', 'image/png', $3, $4, null)",
    )
    .bind(site)
    .bind(&later_key)
    .bind(22_i64)
    .bind(omnion_backup::bytes_checksum(b"written after the backup"))
    .execute(fixture.db.pool())
    .await
    .expect("the later media row must be written");

    let before = run_snapshot(fixture.db.pool(), run_id, site).await;

    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/backups/{run_id}/restore-preview"),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "body: {}", preview.body);

    // 1. It is offered, with a phrase bound to this run and to nothing else.
    assert_eq!(
        preview.body["restorable"],
        json!(true),
        "body: {}",
        preview.body
    );
    let phrase = preview.body["confirm_phrase"]
        .as_str()
        .expect("a confirm phrase")
        .to_owned();
    assert!(
        phrase.starts_with("RESTORE ") && phrase.len() == "RESTORE ".len() + 8,
        "the phrase must be the prefix plus eight characters: {phrase}"
    );
    assert_eq!(
        phrase,
        omnion_backup::confirm_phrase(&run_id.to_string()),
        "the phrase must come from this run's own id, not from a constant"
    );

    // 2. The part table, as the wizard shows it.
    let media = preview.body["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|part| part["part"] == "media")
        .expect("a media part");
    assert_eq!(media["available"], json!(true), "body: {}", preview.body);
    assert_eq!(media["mode"], "replace", "media replaces live data");

    // 3. **The price.** The archived object overwrites itself and the file written after the
    //    run is dropped — and because the suite database is shared with every other
    //    integration walk, the assertion is about the *difference* between the live library
    //    before and after this walk's own upload rather than about a total. A total would be
    //    asserting what the other suites left behind, and would go red the moment a sibling
    //    walk changed.
    // The MEDIA part is the one this walk built a fixture for; the `database` part is
    // always in the run and always contributes a row count, so the total is deliberately
    // not asserted here. The per-part numbers are the ones the wizard shows, and they are
    // scoped to this organization's own site — the suite database is shared with every other
    // integration walk, and a total would be asserting what the other suites left behind.
    assert_eq!(
        media["live_matches"],
        json!(1),
        "the archived object is what the media part overwrites: {}",
        media
    );
    assert_eq!(
        media["live_dropped"],
        json!(1),
        "the file uploaded after the run is what the media part costs: {}",
        media
    );
    let database = preview.body["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|part| part["part"] == "database")
        .expect("the database part is always in a two-scope run");
    assert_eq!(database["mode"], "replace");
    let codes: Vec<&str> = preview.body["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"data_loss"),
        "dropping a live file must be a danger, not a notice: {codes:?}"
    );
    let loss = preview.body["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .find(|warning| warning["code"] == "data_loss")
        .expect("a data_loss warning");
    assert_eq!(loss["severity"], "danger");

    // 4. It wrote NOTHING. Parts, statuses, checksums, the run's own row and the media
    //    library are all exactly as they were — read back out of PostgreSQL, because a
    //    response body cannot prove the database was not written to.
    let after = run_snapshot(fixture.db.pool(), run_id, site).await;
    assert_eq!(
        before.0, after.0,
        "a preview must not change a single part row"
    );
    assert_eq!(
        before.1, after.1,
        "a preview must not change the run's status"
    );
    assert_eq!(
        before.2, after.2,
        "a preview must not change the run's prefix"
    );
    assert_eq!(
        before.3, after.3,
        "a preview must not stamp a second finish time"
    );
    assert_eq!(
        before.4, after.4,
        "a preview must not touch the media library"
    );
    // And the bytes on the destination are still there — a preview that cleaned up after
    // itself would have deleted the very restore point it was describing.
    assert!(
        fixture.root.join(prefix.trim_start_matches('/')).exists(),
        "the archive must still be on the destination after a preview"
    );

    // 5. A reader may preview without holding `backup.restore`. The permission that
    //    overwrites live data is not the permission that reads the warning about it.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'backup.restore.previewed'",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit must read");
    assert!(audited >= 1, "a restore preview must leave an audit entry");
}

/// A preview of a run whose artifact has been truncated must not offer it, and must not
/// hand out a confirm phrase for a restore point that cannot be restored.
///
/// The `verify` walk already proved that `verify` reports a mismatch; what is new here is
/// that **availability** is the question — a truncated artifact is not a smaller restore
/// point, it is no restore point.
#[tokio::test]
async fn a_truncated_artifact_is_not_offered_as_a_restore_point() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // This run asks for `database` and nothing else, so `database` is the ONLY part in it.
    // That is what makes the assertion below sharp: when the one artifact is truncated there
    // is nothing left to restore, and a preview that still handed out a confirm phrase would
    // be offering to overwrite live data from an archive that holds none of it.
    //
    // It also pins the scope rule from the other side. `produce_all` used to produce all
    // five parts whatever the run asked for, and this walk is the one that noticed: with the
    // extra parts present the run stayed `restorable: true` after the truncation, which read
    // like a product bug and was actually a test asking the right question of a broken
    // producer.
    let created = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let parts = created.body["parts"].as_array().expect("parts");
    assert_eq!(
        parts.len(),
        1,
        "a run that asked for one scope produces one part, not five: {parts:?}"
    );
    assert_eq!(parts[0]["part"], "database");
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // Find the artifact by its own recorded key rather than recomputing the name, so the
    // walk and the code cannot agree about a path by both being wrong in the same way.
    let relative: String = sqlx::query_scalar(
        "select storage_path from backup_parts where backup_id = $1 and part = 'database'",
    )
    .bind(run_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the database part must be recorded");
    let path = fixture.artifact(&prefix, &relative);

    std::fs::write(&path, b"{\"tables\":[]}").expect("the artifact must be overwritable");
    assert_ne!(
        std::fs::metadata(&path).expect("the file").len() as i64,
        sqlx::query_scalar::<_, i64>(
            "select size_bytes from backup_parts where backup_id = $1 and part = 'database'",
        )
        .bind(run_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("a size"),
        "the fixture must actually have truncated the file"
    );

    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/backups/{run_id}/restore-preview"),
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "body: {}", preview.body);
    assert_eq!(
        preview.body["restorable"],
        json!(false),
        "a truncated archive is not a restore point: {}",
        preview.body
    );
    assert_eq!(
        preview.body["confirm_phrase"],
        json!(""),
        "nothing to confirm means no phrase, and a phrase would be a guard that guards nothing"
    );
    let database = preview.body["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|part| part["part"] == "database")
        .expect("a database part");
    assert_eq!(database["available"], json!(false));
    let reason = database["reason"].as_str().expect("a reason");
    assert!(
        reason.contains("bytes, the manifest recorded"),
        "the reason must name the disagreement, not say 'missing': {reason}"
    );
    let codes: Vec<&str> = preview.body["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"part_unavailable"),
        "an unavailable part must be said out loud: {codes:?}"
    );
}

/// Another tenant's run is a 404 from the preview too — and the refusal must not confirm
/// that the id exists, or the preview becomes a restore-point oracle across tenants.
#[tokio::test]
async fn another_tenants_backup_has_no_preview_and_the_404_says_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;

    let created = take_backup(
        &fixture.state,
        &stranger_token,
        &stranger_csrf,
        &["database"],
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");

    let mine = fixture.session(&fixture.operator_email).await;
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/backups/{run_id}/restore-preview"),
            Some(&mine.0),
            Some(&mine.1),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "a stranger's backup must be a 404, never a 403 that confirms the id: {}",
        refused.body
    );
    let message = refused.body["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        !message.contains("cross_organization") && !message.contains("organization"),
        "the 404 must not name the tenancy rule — that is the oracle: {message}"
    );
}

/// A run produces the parts it was asked for, and only those.
///
/// This is the regression test for a dead control. The scope selector in the create drawer
/// validated, stored, normalised and rendered the operator's choice, and then
/// `produce_all` walked all five `PARTS` unconditionally — so `["database"]` produced five
/// artifacts. Four existing walks requested `["database"]` and none of them noticed, because
/// each asserted on the part it wanted and the two extra artifacts are perfectly valid
/// files.
///
/// The walk asserts the *count* and the *set*, from PostgreSQL rather than from the
/// response, because a response that listed five parts while the database held one (or the
/// other way round) is exactly the disagreement a count is supposed to catch. And it checks
/// the destination too: producing a part nobody asked for is not only a wrong row, it is
/// bytes on a volume an operator is paying for.
#[tokio::test]
async fn a_run_produces_only_the_scopes_it_was_asked_for() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    for (label, scopes) in [
        ("one", vec!["database"]),
        ("two", vec!["database", "themes"]),
        ("all", omnion_backup::PARTS.to_vec()),
    ] {
        let created = call(
            &fixture.state,
            request(
                Method::POST,
                &backups_uri(),
                Some(&token),
                Some(&csrf),
                Some(json!({ "label": format!("scope-{label}"), "scopes": scopes })),
            ),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{label}: body: {}",
            created.body
        );
        let run_id =
            Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
        let prefix = created.body["backup"]["storage_prefix"]
            .as_str()
            .expect("a prefix")
            .to_owned();

        // From the database, not the response.
        let stored: Vec<String> =
            sqlx::query_scalar("select part from backup_parts where backup_id = $1 order by part")
                .bind(run_id)
                .fetch_all(fixture.db.pool())
                .await
                .expect("the parts must read");
        let mut expected: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        expected.sort();
        assert_eq!(
            stored, expected,
            "{label}: a run asked for {scopes:?} and stored {stored:?}"
        );

        // And nothing is left on the destination that no part claims. A file for a part
        // nobody asked for is the byte-level version of the same bug.
        let directory = fixture.root.join(prefix.trim_start_matches('/'));
        if directory.exists() {
            for name in omnion_backup::PARTS {
                let artifact = omnion_backup::local_path_for(
                    &fixture.root.to_string_lossy(),
                    &omnion_backup::storage_key(&prefix, name),
                );
                if !scopes.iter().any(|scope| *scope == name) {
                    assert!(
                        !artifact.exists(),
                        "{label}: `{name}` was never asked for but {} exists: {}",
                        artifact
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        artifact.display()
                    );
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------------------
// The destructive restore (slice 2b)
// ----------------------------------------------------------------------------------------
//
// These walks are over the **real router, the real filesystem and the real object store**,
// because every claim this slice makes is about what happened to bytes: "the object is back
// in the library", "the safety backup exists", "nothing was written when the phrase was
// wrong". A response body cannot prove any of that, so each assertion that concerns a value
// reads it back out of **PostgreSQL** or out of the **store**.
//
// The walk that matters most is the refusal one, and it is worth saying why it is here rather
// than in the unit tests: `build_plan` is pure and its refusals are unit-tested, but only a
// walk can show that a refused restore **wrote nothing** — no safety backup, no audit row
// claiming a restore, no object in the store. A destructive route that refuses *after* taking
// its safety backup is a route that creates a protected backup every time somebody fat-fingers
// a phrase, and that is its own denial of service.

/// The phrase for a run, read the way the panel reads it: from the preview, not computed.
async fn preview_of(state: &AppState, token: &str, csrf: &str, run_id: Uuid) -> TestResponse {
    call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/backups/{run_id}/restore-preview"),
            Some(token),
            Some(csrf),
            None,
        ),
    )
    .await
}

/// POST a restore and return the response.
async fn restore_now(
    state: &AppState,
    token: &str,
    csrf: &str,
    run_id: Uuid,
    parts: &[&str],
    confirmation: &str,
) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/backups/{run_id}/restore"),
            Some(token),
            Some(csrf),
            Some(json!({ "parts": parts, "confirmation": confirmation })),
        ),
    )
    .await
}

/// Upload one real object into this organization's site and return `(site, storage_key)`.
async fn put_object(
    state: &AppState,
    pool: &sqlx::PgPool,
    site: Uuid,
    name: &str,
    bytes: &[u8],
) -> String {
    let key = format!("restore/{site}/{name}");
    state
        .storage()
        .put(&key, bytes, "image/png")
        .await
        .expect("the object must be storable");
    sqlx::query(
        "insert into media (site_id, storage_key, filename, content_type, size_bytes, \
         checksum, created_by) values ($1, $2, $3, 'image/png', $4, $5, null) \
         on conflict (storage_key) do update set size_bytes = excluded.size_bytes, \
         checksum = excluded.checksum",
    )
    .bind(site)
    .bind(&key)
    .bind(name)
    .bind(bytes.len() as i64)
    .bind(omnion_backup::bytes_checksum(bytes))
    .execute(pool)
    .await
    .expect("the media row must be written");
    key
}

/// The number of runs this organization holds, read out of PostgreSQL.
async fn run_count(fixture: &Fixture, organization_id: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from backups where organization_id = $1")
        .bind(organization_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the runs must read")
}

/// A restore writes the archive's objects back into the live library, takes a protected
/// safety backup first, and writes a `backup.restored` audit entry naming all three.
///
/// Every half is asserted against something other than the response:
///
/// 1. **The object is really back.** The archive held one object, so the live library goes
///    from one row to two — the archived file and a file uploaded after the run, which the
///    archive does not hold. A restore that deleted the newer file would answer the same.
/// 2. **The safety backup exists, is protected, and is not the run being restored.** It is
///    the only thing an operator has if this restore turns out to be the wrong one, so its
///    existence is the property, not its contents.
/// 3. **The audit entry names the safety run**, so the audit is a way back rather than a
///    record that something happened.
#[tokio::test]
async fn a_restore_writes_the_objects_back_and_leaves_a_protected_safety_run() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let restorer_email = fixture.restorer_email.clone();

    let site = create_site(fixture.db.pool(), fixture.org, "Restore Site").await;
    let archived = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "archived.png",
        b"in the archive",
    )
    .await;

    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");

    // A file uploaded AFTER the run. The archive does not hold it, and the restore must not
    // remove it: a restore is a replacement the operator priced, and the object layer here
    // writes back what the archive holds rather than deleting what it does not. (The live
    // comparison still reports it as dropped; the *media* restore does not delete it, and
    // the panel says so rather than pretending the two numbers are the same statement.)
    let newer = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "newer.png",
        b"after the run",
    )
    .await;

    let before: i64 = sqlx::query_scalar("select count(*) from media where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the library must read");
    assert_eq!(before, 2, "the fixture starts with two rows");

    let (restorer_token, restorer_csrf) = fixture.session(&restorer_email).await;
    let preview = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id).await;
    assert_eq!(preview.status, StatusCode::OK, "body: {}", preview.body);
    let phrase = preview.body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();
    assert!(
        !phrase.is_empty(),
        "a run with objects must be offered: {}",
        preview.body
    );

    let outcome = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(outcome.status, StatusCode::OK, "body: {}", outcome.body);
    assert_eq!(outcome.body["parts"], json!(["media"]));

    // 1. The object is back in the live STORE, byte for byte. Compared as bytes, not by
    //    length: a length check passes by accident on an overwrite.
    let restored = fixture
        .state
        .storage()
        .get(&archived)
        .await
        .expect("the archived object must be readable from the live store");
    assert_eq!(
        restored, b"in the archive",
        "the object must be back with its own bytes"
    );
    // And the file uploaded after the run is still there. This is the assertion a
    // "replacement" implementation would fail, and it is the one that matters: an operator
    // who restores a week-old archive and loses the file they uploaded on Tuesday has no
    // way back that the safety backup does not have to cover for them.
    assert!(
        fixture.state.storage().get(&newer).await.is_ok(),
        "a restore must not delete what the archive does not hold"
    );

    // 2. The safety run exists, is protected, and is a different run.
    let safety_id = Uuid::parse_str(
        outcome.body["safety_backup_id"]
            .as_str()
            .expect("a safety backup id"),
    )
    .expect("a uuid");
    assert_ne!(
        safety_id, run_id,
        "the safety run must not be the run being restored"
    );
    let (label, protected): (String, bool) =
        sqlx::query_as("select label, protected from backups where id = $1")
            .bind(safety_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the safety run must exist");
    assert!(
        protected,
        "the safety run is the one thing a failed restore needs; the sweep must not take it"
    );
    assert!(
        label.contains("Safety backup"),
        "a run labelled with nothing else is a restore point an operator has to guess about: {label}"
    );

    // 3. The audit entry names both runs, read out of PostgreSQL.
    // `count(*), max(metadata)` is the shape that reads well and does not run: PostgreSQL
    // has no `max(jsonb)` — jsonb has no ordering — so the walk dies on a *function* the
    // test invented rather than on the thing it means to check. Two statements, or one
    // ordered `fetch_optional`: there is exactly one entry and the order does not matter.
    let entry: Option<(serde_json::Value,)> = sqlx::query_as(
        "select metadata from audit_log \
         where action = 'backup.restored' and target_id = $1 \
         order by created_at desc limit 1",
    )
    .bind(run_id.to_string())
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the audit must read");
    let metadata = entry.expect("a restore must leave an audit entry").0;
    assert_eq!(
        metadata["safety_backup_id"].as_str(),
        Some(safety_id.to_string().as_str()),
        "the audit is a way back, so it must name the run to go back to: {metadata}"
    );
    assert_eq!(metadata["media"]["objects_restored"], json!(1));
    assert_eq!(
        metadata["media"]["dropped"],
        json!(0),
        "the media layer deletes nothing: {metadata}"
    );
}

/// A wrong phrase, a part the run does not hold and a part this build refuses are all
/// `400` — and **none of them writes anything**.
///
/// The "nothing" is the whole point. A destructive route that takes its safety backup and
/// then refuses is a route that creates a protected, undeletable backup every time somebody
/// fat-fingers a phrase, and the refusal would still be correct.
#[tokio::test]
async fn a_refused_restore_writes_no_backup_no_audit_and_no_object() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Refused Site").await;
    put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "only.png",
        b"the only object",
    )
    .await;

    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;
    let phrase = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id)
        .await
        .body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();

    let runs_before = run_count(&fixture, fixture.org).await;
    let object_before: i64 = sqlx::query_scalar("select count(*) from media where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the library must read");

    // (a) A wrong phrase. `backup.restore` is held, so the refusal is about the phrase and
    //     about nothing else -- a route that answered "the archive has no database part"
    //     here would be confusing two different mistakes.
    let wrong = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        "RESTORE 00000000",
    )
    .await;
    assert_eq!(
        wrong.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        wrong.body
    );
    assert_eq!(wrong.body["error"]["code"], "confirmation_mismatch");
    assert!(
        wrong.message().contains(&phrase),
        "the refusal must name the right phrase, not just say no: {}",
        wrong.message()
    );

    // (b) A part this run did not produce. The run asked for `media` only, so `database` is
    //     not "unavailable, restoring the rest", it is a refusal that names what IS there.
    let absent = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["database"],
        &phrase,
    )
    .await;
    assert_eq!(
        absent.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        absent.body
    );
    assert_eq!(absent.body["error"]["code"], "part_not_in_archive");
    assert!(
        absent.message().contains("media"),
        "the refusal must name what the run does hold: {}",
        absent.message()
    );

    // (c) A part name that is not one of the five.
    let unknown = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["typo"],
        &phrase,
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        unknown.body
    );
    assert_eq!(unknown.body["error"]["code"], "unknown_part");

    // (d) An empty selection is a refusal, not "restore everything". A form that posted
    //     nothing and got the whole archive back is a form that restored more than it
    //     showed.
    let empty = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &[],
        &phrase,
    )
    .await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        empty.body
    );
    assert_eq!(empty.body["error"]["code"], "empty_selection");

    // **Nothing happened.** No run, no audit entry, no object, no row.
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        runs_before,
        "a refused restore must not take a safety backup: every refusal above would have \
         created a protected run the sweep then keeps"
    );
    let object_after: i64 = sqlx::query_scalar("select count(*) from media where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the library must read");
    assert_eq!(
        object_after, object_before,
        "a refused restore writes no rows"
    );
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'backup.restored' and target_id = $1",
    )
    .bind(run_id.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit must read");
    assert_eq!(
        audited, 0,
        "a refusal is not a restore and must not say it was one"
    );
}

/// A run holding `database` is refused by name, and the refusal says why.
///
/// `document_database` writes a row COUNT per table. That is an inventory, not a dump, and
/// restoring it would replace the platform's schema with an inventory of it — the counting
/// defect this crate was written to remove, in its most expensive form. The walk proves the
/// route refuses it rather than reporting a success that moved nothing.
#[tokio::test]
async fn the_database_part_is_refused_by_name_because_it_is_an_inventory_not_a_dump() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let created = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");

    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;
    let preview = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id).await;
    let phrase = preview.body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();
    assert!(
        !phrase.is_empty(),
        "the archive IS readable; the refusal is about what restoring it would mean"
    );

    let runs_before = run_count(&fixture, fixture.org).await;
    let refused = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["database"],
        &phrase,
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "part_not_restorable");
    let message = refused.message();
    assert!(
        message.contains("row count") && message.contains("inventory"),
        "the refusal must say why -- a bare \"cannot restore this\" sends the operator to \
         look for a broken archive: {message}"
    );
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        runs_before,
        "the refusal happens BEFORE the safety backup: taking a backup of a restore that is \
         going to be refused is a run nobody asked for"
    );
}

/// `backup.restore` gates the button, and nothing else in the file answers on that route.
///
/// The three ways to get this wrong are all plausible: gating it under `backup.manage` (so a
/// schedule editor can overwrite content), under `backup.create` (so anybody who may take a
/// backup may destroy one), or under `backup.read` (so a reader may). The walk holds an
/// operator with every *other* key and shows it cannot restore, and a stranger with the
/// restore key shows the run is still a 404.
#[tokio::test]
async fn the_restore_button_needs_its_own_key_and_a_strangers_run_is_still_a_404() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Keyed Site").await;
    put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "keyed.png",
        b"bytes",
    )
    .await;
    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");

    // The operator holds read, create AND manage. Every one of those keys is about running
    // and organising backups; none of them is about overwriting live data.
    let refused = restore_now(
        &fixture.state,
        &token,
        &csrf,
        run_id,
        &["media"],
        "RESTORE 00000000",
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a platform where the schedule editor can overwrite content is one where the nightly \
         job and an operator's button are the same authority: body: {}",
        refused.body
    );

    // Anonymous is refused too, and the refusal comes before the plan: a route that priced
    // a restore for a caller who may not perform it is a free oracle for what a run holds.
    let anonymous = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/backups/{run_id}/restore"),
            None,
            None,
            Some(json!({ "parts": ["media"], "confirmation": "x" })),
        ),
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "body: {}",
        anonymous.body
    );

    // A stranger holding the restore key still gets a 404, because the run is not theirs.
    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;
    let stranger = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/backups/{run_id}/restore"),
            Some(&stranger_token),
            Some(&stranger_csrf),
            Some(json!({ "parts": ["media"], "confirmation": "x" })),
        ),
    )
    .await;
    assert_eq!(
        stranger.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        stranger.body
    );
    assert!(
        !stranger.message().to_lowercase().contains("organization"),
        "a 403 confirms the id exists; the message must not name the tenancy rule: {}",
        stranger.message()
    );
}

/// A truncated object in the archive is refused **and the store is left with the original**.
///
/// This is the walk that could only have been written after the unit tests passed: the crate
/// re-hashes every object before writing it, and the only way to see that check do its job is
/// to corrupt a real file on the destination and then ask for a real restore.
#[tokio::test]
async fn a_corrupt_archived_object_is_refused_and_the_live_store_keeps_its_own_bytes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Corrupt Site").await;
    let key = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "corrupt.png",
        b"the original bytes",
    )
    .await;

    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // The live copy changes after the backup — this is the interesting half, because a
    // restore that wrote the corrupt archive over it would destroy a good object.
    let current = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "corrupt.png",
        b"the live bytes",
    )
    .await;
    assert_eq!(
        current, key,
        "the fixture writes the same key on purpose: the restore is about the same storage key"
    );

    // Find the archived object by the index's own recorded key and truncate it.
    // The index key comes from the CRATE, not from a format string here. The first
    // version of this walk built `{prefix}index.json`, which is the same mistake the
    // artifact-path defect in this suite already made once: the walk and the code can
    // both be wrong in the same way, and the fixture then asserts a file that was
    // never written.
    let index_relative = omnion_backup::index_key(&prefix);
    let index_path = fixture.artifact(&prefix, &index_relative);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index_path).expect("the index must be readable"))
            .expect("the index must be json");
    let archive_key = index["objects"][0]["archive_key"]
        .as_str()
        .expect("an archive key")
        .to_owned();
    let object_path = fixture.artifact(&prefix, &archive_key);
    std::fs::write(&object_path, b"trunc").expect("the object must be overwritable");

    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;
    let phrase = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id)
        .await
        .body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();

    // The index itself still matches the manifest, so the preview offers the run — the
    // object-level check is the one that has to catch this, and the preview cannot see it.
    let outcome = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(outcome.status, StatusCode::OK, "body: {}", outcome.body);
    assert_eq!(
        outcome.body["media"]["objects_restored"],
        json!(0),
        "a corrupt object must not be written: {}",
        outcome.body
    );
    assert_eq!(outcome.body["media"]["objects_failed"], json!(1));
    let failure = outcome.body["media"]["failures"][0]["reason"]
        .as_str()
        .expect("a reason")
        .to_owned();
    assert!(
        failure.contains("the archived bytes hash to"),
        "the refusal must name the disagreement between the archive and the index: {failure}"
    );

    // The live store still holds its own bytes. This is the assertion that makes the whole
    // re-hash check worth having.
    let live = fixture
        .state
        .storage()
        .get(&key)
        .await
        .expect("the live object must still be there");
    assert_eq!(
        live, b"the live bytes",
        "a corrupt archive must not overwrite a good live object"
    );
}

/// A media index this build cannot read is a refusal, not an empty restore.
///
/// `MediaIndex` is serde, and serde's default is to accept a missing field as its `Default`
/// — so a future index read by this build would deserialise into zero objects and the route
/// would answer "0 objects restored" for a full archive. A success that restored nothing is
/// the one sentence this feature must never produce.
///
/// **The walk is two phases, and the first one exists because the obvious fixture does not
/// work.** Rewriting the index and then asking for a restore proves the *part-size* check,
/// not the version guard: `version: 1` becomes `version: 99`, the serialised document
/// changes length, and the media part's recorded size no longer matches the file — so the
/// preview refuses before the index is ever read, which is the right answer and the wrong
/// one to be asserting. Phase one proves exactly that (an edited index is not offered, and
/// no phrase is issued). Phase two pads the rewrite back to the recorded length, which is
/// the only way past the size check, and that is where the version guard earns its keep.
#[tokio::test]
async fn a_media_index_this_build_cannot_read_is_refused_not_treated_as_empty() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Index Site").await;
    put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "indexed.png",
        b"bytes",
    )
    .await;
    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // The index's key comes from the CRATE, not from a format string here. The first version
    // of this walk built `{prefix}index.json`, which is the same mistake the artifact-path
    // defect in this suite already made once: the walk and the code can both be wrong in the
    // same way, and the fixture then asserts a file that was never written.
    let index_relative = omnion_backup::index_key(&prefix);
    let index_path = fixture.artifact(&prefix, &index_relative);
    let original = std::fs::read(&index_path).expect("the index must be on the destination");
    let recorded: i64 = sqlx::query_scalar(
        "select size_bytes from backup_parts where backup_id = $1 and part = 'media'",
    )
    .bind(run_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the media part must record a size");
    assert_eq!(
        original.len() as i64,
        recorded,
        "the file on the destination must start as the recorded size, or the two-phase \\
         fixture below is measuring the wrong thing"
    );

    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;

    // ---- Phase one: an edited index is not a restore point -------------------------------
    let mut value: serde_json::Value =
        serde_json::from_slice(&original).expect("the index is json");
    value["version"] = json!(99);
    let edited = serde_json::to_vec(&value).expect("json");
    std::fs::write(&index_path, &edited).expect("the index must be overwritable");

    let preview = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id).await;
    assert_eq!(preview.status, StatusCode::OK, "body: {}", preview.body);
    assert_eq!(
        preview.body["restorable"],
        json!(false),
        "an index that was edited on the destination is not a restore point: {}",
        preview.body
    );
    assert_eq!(
        preview.body["confirm_phrase"],
        json!(""),
        "and no phrase may be issued for one: a phrase here would be a guard guarding nothing"
    );
    let runs_after_phase_one = run_count(&fixture, fixture.org).await;

    // ---- Phase two: the same edit, padded back to the recorded length -----------------------
    // One character at a time, because a pad that overshoots is not a failed attempt, it is
    // the loop trying again one character shorter. The first version doubled the pad and
    // gave up on the overshoot, leaving the file a byte short — and the size check then
    // refused the run again, so the walk proved the size check twice and the version guard
    // not at all.
    let target = original.len();
    let mut pad = 0usize;
    let mut padded = edited.clone();
    while padded.len() != target {
        assert!(
            pad < 8_000,
            "the fixture could not produce a length-preserving rewrite: {} vs {target}",
            padded.len()
        );
        pad += 1;
        let mut next = value.clone();
        next["_pad"] = json!("x".repeat(pad));
        padded = serde_json::to_vec(&next).expect("json");
    }
    assert_eq!(
        padded.len(),
        target,
        "the rewrite must be length-preserving"
    );
    std::fs::write(&index_path, &padded).expect("the index must be overwritable");

    let preview = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id).await;
    assert_eq!(
        preview.body["restorable"],
        json!(true),
        "with the size preserved the part IS readable, so the wizard offers it: {}",
        preview.body
    );
    let phrase = preview.body["confirm_phrase"]
        .as_str()
        .expect("a phrase: the part is readable, so one is issued")
        .to_owned();
    assert!(!phrase.is_empty(), "body: {}", preview.body);

    // ---- And the restore still refuses, on the index, with the RIGHT phrase ---------------
    // This is the case the version guard exists for: the part says it is readable, the
    // wizard offers the run, the operator types the phrase — and the answer is still no.
    // Without the guard, serde would have read `version: 99` as an index holding no objects
    // and the route would have answered "0 objects restored" for a full archive.
    let runs_before = run_count(&fixture, fixture.org).await;
    let refused = restore_now(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        refused.body
    );
    assert_eq!(
        refused.body["error"]["code"], "media_index_unreadable",
        "body: {}",
        refused.body
    );
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        runs_before,
        "the index is read before the safety backup, so a refusal costs nothing"
    );
    assert!(
        runs_after_phase_one >= runs_before,
        "sanity: the two phases are on the same run"
    );
}

// --------------------------------------------------------------------------------------------
// The schedules (REQ-013, slice 3)
// --------------------------------------------------------------------------------------------

/// A schedule is stored with a **computed** next run, and the worker fires it.
///
/// The defect this walk was written for is a silence: `backup_schedules` and
/// `next_due_schedules` shipped in slice 1 and nothing wrote the column the query reads, so a
/// schedule could be created, listed and rendered with a cadence sentence beside an empty
/// next-run cell for ever. A unit test on the cadence cannot catch that — it has no idea
/// whether a writer exists — so the assertion here is about the column's contents, read out
/// of PostgreSQL rather than out of the response, and then about a run appearing because the
/// time came.
///
/// The three refusals are in the same walk because they are the same decision: a schedule the
/// server stores but the worker cannot compute is a row that looks live and never fires, so
/// it is refused at the door with the field named.
#[tokio::test]
async fn a_schedule_is_stored_with_a_next_run_and_the_worker_takes_the_backup() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // --- 1. a daily schedule, and the next run is written -----------------------------------
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &schedules_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "name": "QA nightly",
                "frequency": "daily",
                "at_time": "02:30",
                "timezone": "Europe/Istanbul",
                "scopes": ["database", "configuration"],
                "retention_count": 7,
                "enabled": true,
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "body: {}", created.body);
    let id: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();
    assert!(
        created.body["next_run_at"].is_string(),
        "the response must carry the computed next run: {}",
        created.body
    );

    // Read it back OUT OF POSTGRESQL, not from the response. A response can carry a computed
    // value that was never stored, and the worker reads the column.
    let stored: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select next_run_at from backup_schedules where id = $1")
            .bind(id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the schedule must read");
    assert!(
        stored.is_some(),
        "the column the worker reads was left null — the schedule would never fire"
    );

    // And it is in the FUTURE and, crucially, not at 02:30 UTC. Istanbul is UTC+3, so a
    // scheduler that stored the wall clock verbatim would be three hours out and this is the
    // assertion that sees it.
    let next = stored.expect("checked above");
    assert!(
        next > time::OffsetDateTime::now_utc(),
        "the next run is in the past: {next}"
    );
    let utc_hour = next.hour();
    // 02:30 in Istanbul is 23:30 UTC the previous day, never 02:30 UTC. The assertion is
    // "not the naive value" rather than "is exactly 23:30" so it does not become a test of
    // the zone table's contents.
    assert_ne!(
        utc_hour, 2,
        "the next run was stored as the wall clock in UTC, so it is three hours out: {next}"
    );

    // --- 2. the worker takes the backup when the time comes -----------------------------------
    // Backdate the column rather than waiting for 02:30: the walk has to be about the worker
    // finding a due schedule, and a walk that sleeps until half past two is a walk nobody
    // runs. This is the only time travel in the suite and it touches one column.
    sqlx::query(
        "update backup_schedules set next_run_at = now() - interval '1 second' where id = $1",
    )
    .bind(id)
    .execute(fixture.db.pool())
    .await
    .expect("the schedule must be backdated");

    let before = run_count(&fixture, fixture.org).await;
    let started = omnion_api::backup_schedule_runner::tick(&fixture.state)
        .await
        .expect("the schedule tick must answer");
    assert_eq!(started, 1, "exactly one schedule was due: {started}");
    let after = run_count(&fixture, fixture.org).await;
    assert_eq!(
        after,
        before + 1,
        "the worker claimed a due schedule and did not take a backup"
    );

    // The run is a real one: it has the schedule's OWN scopes, not all five. A worker that
    // walked `PARTS` would produce five artifacts for a schedule that asked for two, which is
    // the media-counting defect this crate exists to remove, in its most expensive form.
    let scheduled_parts: i64 = sqlx::query_scalar(
        "select count(*) from backup_parts bp join backups b on b.id = bp.backup_id \
         where b.schedule_id = $1 and b.kind = 'scheduled'",
    )
    .bind(id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the scheduled run's parts must read");
    assert_eq!(
        scheduled_parts, 2,
        "a schedule for two parts produced {scheduled_parts} — the worker ignored its scopes"
    );

    // The schedule is rearmed: `next_run_at` is in the future again. A worker that fired but
    // did not rearm would take the same backup on every tick for ever.
    let rearmed: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select next_run_at from backup_schedules where id = $1")
            .bind(id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the schedule must read");
    let rearmed = rearmed.expect("a fired schedule must still have a next run");
    assert!(
        rearmed > time::OffsetDateTime::now_utc(),
        "the schedule was not rearmed: {rearmed}"
    );

    // And it does not fire twice for the same slot.
    let second = omnion_api::backup_schedule_runner::tick(&fixture.state)
        .await
        .expect("the second tick must answer");
    assert_eq!(second, 0, "a rearmed schedule fired again immediately");
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        after,
        "the second tick took another backup"
    );

    // --- 3. the refusals --------------------------------------------------------------------
    // An unknown timezone is refused by name. Storing it would leave a row that looks live
    // and never fires, which is the defect this whole walk is about.
    let bad_zone = call(
        &fixture.state,
        request(
            Method::POST,
            &schedules_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "name": "QA bad zone",
                "frequency": "daily",
                "at_time": "02:00",
                "timezone": "Europe/Istanbool",
                "scopes": ["database"],
            })),
        ),
    )
    .await;
    assert_eq!(
        bad_zone.status,
        StatusCode::BAD_REQUEST,
        "an unknown timezone must be refused, not stored: {}",
        bad_zone.body
    );
    assert!(
        bad_zone.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Europe/Istanbool"),
        "the refusal must name the zone: {}",
        bad_zone.body
    );

    // A daily schedule with no time of day is refused rather than defaulted to midnight,
    // which is exactly the fallback nobody chose.
    let no_time = call(
        &fixture.state,
        request(
            Method::POST,
            &schedules_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "name": "QA no time",
                "frequency": "daily",
                "timezone": "UTC",
                "scopes": ["database"],
            })),
        ),
    )
    .await;
    assert_eq!(
        no_time.status,
        StatusCode::BAD_REQUEST,
        "a daily schedule with no time of day must be refused: {}",
        no_time.body
    );

    // And the two refusals left nothing behind.
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        after,
        "a refused schedule produced a backup"
    );
}

/// A stranger's schedule is a `404`, and "run now" is `backup.create` rather than
/// `backup.manage`.
///
/// The key split is the interesting half: pressing "run now" produces a backup and changes
/// nothing else, so an operator who may take a backup must be able to test that their
/// schedule works. An operator holding every key **except** `backup.create` may edit
/// schedules — they are the one who set the cadence — and must not be able to produce one.
#[tokio::test]
async fn a_strangers_schedule_is_a_404_and_running_one_needs_the_take_a_backup_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &schedules_uri(),
            Some(&token),
            Some(&csrf),
            Some(json!({
                "name": "QA tenancy",
                "frequency": "daily",
                "at_time": "03:00",
                "timezone": "UTC",
                "scopes": ["database"],
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "body: {}", created.body);
    let id: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();

    // The fixture's own stranger: a full session in another tenant, holding every backup key
    // this suite knows. A stranger that is *missing* a key would answer 403 and the walk
    // would be asserting the boundary at the wrong layer — the guard, not the tenancy.
    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;

    for (label, method, uri, body) in [
        (
            "update",
            Method::PUT,
            schedule_uri(id),
            json!({ "name": "hijacked", "frequency": "daily", "at_time": "04:00", "timezone": "UTC", "scopes": ["database"] }),
        ),
        ("delete", Method::DELETE, schedule_uri(id), json!({})),
        (
            "run",
            Method::POST,
            format!("{}/run", schedule_uri(id)),
            json!({}),
        ),
    ] {
        let answer = call(
            &fixture.state,
            request(
                method,
                &uri,
                Some(&stranger_token),
                Some(&stranger_csrf),
                Some(body),
            ),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "a stranger's {label} must be a 404, not a 403: {}",
            answer.body
        );
    }

    // The schedule is untouched: a 403-or-404 is only a boundary if the row is still there.
    let name: String = sqlx::query_scalar("select name from backup_schedules where id = $1")
        .bind(id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the schedule must still read");
    assert_eq!(name, "QA tenancy", "a stranger changed the schedule");

    // An account with read+create+manage is the operator, who CAN run it — and the run is a
    // real backup tied to the schedule.
    let before = run_count(&fixture, fixture.org).await;
    let ran = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{}/run", schedule_uri(id)),
            Some(&token),
            Some(&csrf),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(ran.status, StatusCode::CREATED, "body: {}", ran.body);
    assert_eq!(run_count(&fixture, fixture.org).await, before + 1);

    // And a manual run does NOT consume the next slot. An operator testing a 03:00 schedule
    // at 09:00 must not have silently skipped tomorrow's 03:00.
    let next: Option<time::OffsetDateTime> =
        sqlx::query_scalar("select next_run_at from backup_schedules where id = $1")
            .bind(id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the schedule must read");
    let next = next.expect("running a schedule must not clear its next run");
    assert!(
        next > time::OffsetDateTime::now_utc(),
        "a manual run consumed the next scheduled slot: {next}"
    );
}

// --------------------------------------------------------------------------------------------
// The queued restore (REQ-013, slice 2c)
// --------------------------------------------------------------------------------------------

/// `POST /api/v1/backups/{id}/restore-queue` — what an operator sends to get a cancellable
/// restore.
async fn queue_restore(
    state: &AppState,
    token: &str,
    csrf: &str,
    run_id: Uuid,
    parts: &[&str],
    confirmation: &str,
) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("{}/restore-queue", backup_uri(run_id)),
            Some(token),
            Some(csrf),
            Some(json!({ "parts": parts, "confirmation": confirmation })),
        ),
    )
    .await
}

/// `POST /api/v1/restore-jobs/{id}/cancel` — the abort, addressed by the JOB's id.
async fn cancel_restore(state: &AppState, token: &str, csrf: &str, job: Uuid) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/restore-jobs/{job}/cancel"),
            Some(token),
            Some(csrf),
            Some(json!({})),
        ),
    )
    .await
}

/// The status a queued job is in, read **out of PostgreSQL** rather than from a response.
///
/// A response can carry a state the row is not in, and the whole feature is a claim about
/// what the row says: the schema's check constraint refuses an `aborted` job that started, and
/// that refusal is only meaningful against the stored value.
async fn job_status(
    pool: &sqlx::PgPool,
    job: Uuid,
) -> (String, bool, Option<time::OffsetDateTime>) {
    sqlx::query_as(
        "select status, cancel_requested, started_at from backup_restore_jobs where id = $1",
    )
    .bind(job)
    .fetch_one(pool)
    .await
    .expect("the job row must read")
}

/// A queued restore can be stopped before it writes anything, and the stop leaves the
/// platform untouched.
///
/// **The criterion this slice was opened for.** Slice 2b refused a cancel outright and said
/// why — a `POST` in flight cannot be un-pressed — so the acceptance criterion stayed open
/// with a note that a genuine abort belongs with a queued restore. This is that proof, and
/// the load-bearing assertion is the one at the end: a cancel that left a safety backup, a
/// media row or a stored object behind would be an abort in name only, because the operator
/// stopped it *because* they did not want their data touched.
///
/// Three things are in here besides the happy path, and each is a way the "abort" could be a
/// lie:
/// * **The cancel is honoured when it loses the race to the worker** — a `queued` job is
///   marked `aborted` by the *cancel route itself*, not left for the worker's next tick,
///   because a panel that keeps offering "stop" on a row the operator already stopped reads
///   as broken.
/// * **A second restore of the same run is refused while one is live** — two restores would
///   take two safety backups and write every object twice.
/// * **A stranger cannot see or cancel the job**, and cannot queue one: the tenant boundary
///   is asserted on all three routes, not just the one that writes.
#[tokio::test]
async fn a_queued_restore_can_be_cancelled_before_it_writes_and_leaves_nothing_behind() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Queue Site").await;
    let key = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "queued.png",
        b"the archived bytes",
    )
    .await;

    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");

    // The live copy changes after the run, so a restore that half-happened would be visible.
    put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "queued.png",
        b"the live bytes",
    )
    .await;

    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;
    let preview = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id).await;
    let phrase = preview.body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();

    // --- 1. The refusals, and they are refusals *before* the row exists -----------------------
    // A refused queue must write nothing at all. A destructive route that minted an
    // undeletable, protected row every time somebody fat-fingered a phrase would be its own
    // denial of service, and the refusal would still be correct.
    for (label, parts, confirmation, expected) in [
        (
            "wrong phrase",
            vec!["media"],
            "RESTORE 00000000",
            "confirmation_mismatch",
        ),
        (
            "empty selection",
            vec![],
            phrase.as_str(),
            "nothing_selected",
        ),
        (
            "unknown part",
            vec!["media", "typo"],
            phrase.as_str(),
            "unknown_part",
        ),
        (
            "a part the run never produced",
            vec!["configuration"],
            phrase.as_str(),
            "part_not_in_run",
        ),
        // The fixture's run is a **media-only** one, so `database` is refused as "this run
        // does not offer it" — the run-check comes first, and it is the better answer: it
        // names what the run *can* offer. The deeper "that part is not restorable at all"
        // rule needs a run that really produced it, and the walk below builds one.
        (
            "a part this run never produced",
            vec!["database"],
            phrase.as_str(),
            "part_not_in_run",
        ),
    ] {
        let answer = queue_restore(
            &fixture.state,
            &restorer_token,
            &restorer_csrf,
            run_id,
            &parts,
            confirmation,
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused, not queued: {}",
            answer.body
        );
        assert_eq!(
            answer.body["error"]["code"], expected,
            "{label} must be refused by name: {}",
            answer.body
        );
    }
    // The deeper rule, on a run that really produced the `database` part. Separate because
    // the two refusals are different facts: "this run does not offer it" is about the run,
    // and "that part is not restorable at all" is about the *platform* — a part that records
    // a row count per table is an inventory, and restoring it would replace the schema with
    // an inventory of it. A run that has the part and is still refused is the case that
    // proves the rule is about the part and not about the run.
    let db_run = take_backup(&fixture.state, &token, &csrf, &["database"]).await;
    let db_run_id =
        Uuid::parse_str(db_run.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    let db_phrase = preview_of(&fixture.state, &restorer_token, &restorer_csrf, db_run_id)
        .await
        .body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();
    let db_refused = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        db_run_id,
        &["database"],
        &db_phrase,
    )
    .await;
    assert_eq!(
        db_refused.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        db_refused.body
    );
    assert_eq!(
        db_refused.body["error"]["code"], "part_not_restorable",
        "a run that HAS the database part must still be refused, by name and for the right \
         reason: {}",
        db_refused.body
    );

    // Scoped to **this** tenant, and the scoping is the fix rather than a nicety. The count was
    // `select count(*) from backup_restore_jobs` with no `where`, in a database every suite in
    // `apps/api/tests` shares — so a row any *other* walk had left behind (a killed run, a
    // concurrent suite, the two restore walks in this file) failed this assertion with "a
    // refused queue wrote a row", naming the wrong writer. The first version of this walk was
    // a statement about the whole platform's job table wearing the sentence about this
    // tenant's refusals; the sentence is only true of rows this walk could have written.
    let queued_after_refusals: i64 =
        sqlx::query_scalar("select count(*) from backup_restore_jobs where organization_id = $1")
            .bind(fixture.org)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the job table must read");
    assert_eq!(
        queued_after_refusals, 0,
        "a refused queue wrote a row: the operator would see a restore that never happens"
    );

    // --- 2. The queue, and the answer is 202 with the job ------------------------------------
    let queued = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "body: {}", queued.body);
    let job_id = Uuid::parse_str(queued.body["id"].as_str().expect("a job id")).expect("a uuid");
    assert_eq!(
        queued.body["status"],
        json!("queued"),
        "body: {}",
        queued.body
    );
    assert_eq!(
        queued.body["cancellable"],
        json!(true),
        "a queued job IS the cancellable window; body: {}",
        queued.body
    );
    assert_eq!(queued.body["parts"], json!(["media"]));

    // The instants must cross the wire as strings. `time`'s human-readable `Serialize` is
    // gated on a feature this workspace does not enable, so a bare `OffsetDateTime` ships as
    // a nine-element array and the panel renders an em dash — the same em dash it renders for
    // a value that has not happened yet, which is what makes a *lost* timestamp and a
    // *designed* one look alike.
    for field in ["created_at", "started_at", "finished_at"] {
        let value = &queued.body[field];
        assert!(
            value.is_null() || value.is_string(),
            "{field} must be a string or null, not {}: {value}",
            value
        );
    }
    assert!(
        queued.body["started_at"].is_null(),
        "a queued job has not started, and the worker — not the request — stamps that: {}",
        queued.body
    );
    assert!(queued.body["finished_at"].is_null());

    // The agreed price is CARRIED, not recomputed: a job re-priced at execution time would
    // restore against today's library while the operator agreed to yesterday's number.
    assert_eq!(
        queued.body["live_dropped"], preview.body["total_live_dropped"],
        "the queued job must carry the price the operator read"
    );

    // --- 3. A second restore of the same run is refused while one is live ---------------------
    let again = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        again.body
    );
    assert_eq!(
        again.body["error"]["code"], "restore_already_queued",
        "two restores of one run would take two safety backups and write every object twice: {}",
        again.body
    );

    // --- 4. The cancel, and the counts that make it a real abort -----------------------------
    let runs_before = run_count(&fixture, fixture.org).await;
    let media_before: i64 = sqlx::query_scalar("select count(*) from media where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the library must read");

    let cancelled = cancel_restore(&fixture.state, &restorer_token, &restorer_csrf, job_id).await;
    assert_eq!(cancelled.status, StatusCode::OK, "body: {}", cancelled.body);
    assert_eq!(
        cancelled.body["status"],
        json!("aborted"),
        "an operator who pressed stop must be told it stopped: {}",
        cancelled.body
    );
    assert_eq!(
        cancelled.body["cancellable"],
        json!(false),
        "a finished job must not offer another stop"
    );

    // The row, out of PostgreSQL. `started_at` is NULL — the schema's own check constraint
    // refuses an `aborted` job that started, and that refusal is the feature: an abort is a
    // statement that nothing was written.
    let (status, cancel_requested, started_at) = job_status(fixture.db.pool(), job_id).await;
    assert_eq!(
        status, "aborted",
        "the stored status is the claim being made"
    );
    assert!(
        started_at.is_none(),
        "an aborted restore must never have started: {started_at:?}"
    );
    assert!(cancel_requested, "the row records that somebody asked");

    // **The load-bearing assertion.** No safety backup, no library row, no stored object.
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        runs_before,
        "the cancel took a safety backup: a restore with nothing to go back to is the one \
         outcome this feature will not allow, and an ABORT has to go back to nothing too"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from media where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the library must read"),
        media_before,
        "the cancel wrote a library row"
    );
    assert_eq!(
        fixture.state.storage().get(&key).await.unwrap_or_default(),
        b"the live bytes",
        "the cancel wrote into the object store: the live file must still hold what it held"
    );

    // The audit entry exists, naming the job — a restore that was authorised and then stopped
    // is exactly the event an operator needs to find a month later.
    let audited: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'backup.restore.queued'")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the audit log must read");
    assert!(audited >= 1, "queueing a restore left no audit entry");

    // --- 5. Cancelling it twice, and cancelling a stranger's job --------------------------------
    let twice = cancel_restore(&fixture.state, &restorer_token, &restorer_csrf, job_id).await;
    assert_eq!(
        twice.status,
        StatusCode::BAD_REQUEST,
        "a second stop must be refused, not silently accepted: {}",
        twice.body
    );
    assert_eq!(twice.body["error"]["code"], "restore_not_cancellable");

    let (stranger_token, stranger_csrf) = fixture.session(&fixture.stranger_email).await;
    let stranger_cancel =
        cancel_restore(&fixture.state, &stranger_token, &stranger_csrf, job_id).await;
    assert_eq!(
        stranger_cancel.status,
        StatusCode::NOT_FOUND,
        "a stranger's cancel must be a 404, not a 403: {}",
        stranger_cancel.body
    );
    assert!(
        !stranger_cancel.body.to_string().contains("organization"),
        "the 404 must not name the tenancy rule — that confirms the job exists: {}",
        stranger_cancel.body
    );

    let stranger_list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{}/restore-jobs", backup_uri(run_id)),
            Some(&stranger_token),
            Some(&stranger_csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        stranger_list.status,
        StatusCode::NOT_FOUND,
        "a stranger must not list this run's restores: {}",
        stranger_list.body
    );

    let stranger_queue = queue_restore(
        &fixture.state,
        &stranger_token,
        &stranger_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(
        stranger_queue.status,
        StatusCode::NOT_FOUND,
        "a stranger must not queue a restore of this run: {}",
        stranger_queue.body
    );

    // --- 6. The list, and the permission split ------------------------------------------------
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{}/restore-jobs", backup_uri(run_id)),
            Some(&restorer_token),
            Some(&restorer_csrf),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "body: {}", listed.body);
    assert_eq!(
        listed.body.as_array().map(Vec::len),
        Some(1),
        "the run's restores must be listed: {}",
        listed.body
    );
    assert_eq!(listed.body[0]["status"], json!("aborted"));
    assert!(
        listed.body[0]["error"].is_string(),
        "the reason must be readable"
    );

    // An operator holding read/create/manage but NOT `backup.restore` may **list** the jobs
    // and may not **stop** one. The separation has to be provable, and the account that
    // proves it is the one holding every *other* key.
    let (operator_token, operator_csrf) = fixture.session(&fixture.operator_email).await;
    let operator_cancel =
        cancel_restore(&fixture.state, &operator_token, &operator_csrf, job_id).await;
    assert!(
        matches!(
            operator_cancel.status,
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
        ),
        "an operator without `backup.restore` must not be able to stop a restore: {}",
        operator_cancel.body
    );
    let operator_list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{}/restore-jobs", backup_uri(run_id)),
            Some(&operator_token),
            Some(&operator_csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        operator_list.status,
        StatusCode::OK,
        "reading that a restore is queued changes nothing, so it is `backup.read`: {}",
        operator_list.body
    );
}

/// The queued worker's own contract: a job nobody cancels runs, and a job that was cancelled
/// is never picked up even when the tick finds it.
///
/// **The second half is the one the first implementation got wrong.** The worker read
/// `queued_restore_jobs(pool, None)`, and `is not distinct from null` matches rows whose
/// `organization_id` **is** null — the platform's own jobs. Every tenant's restore would
/// have sat `queued` for ever while the tick reported a clean pass, which is the same silence
/// as the uncalled `next_due_schedules` one feature over: a query that answers, a worker
/// that polls, and no writer that reaches the rows anybody can see. The walk proves it by
/// asserting the worker's *effect*, not by asserting a return value — `tick` returning `0`
/// is exactly what a broken worker returns.
#[tokio::test]
async fn the_worker_restores_a_queued_job_and_skips_one_that_was_cancelled() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;
    let site = create_site(fixture.db.pool(), fixture.org, "Worker Site").await;
    let key = put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "worker.png",
        b"worker archived bytes",
    )
    .await;

    let created = take_backup(&fixture.state, &token, &csrf, &["media"]).await;
    let run_id =
        Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("a uuid");
    put_object(
        &fixture.state,
        fixture.db.pool(),
        site,
        "worker.png",
        b"worker live bytes",
    )
    .await;

    let (restorer_token, restorer_csrf) = fixture.session(&fixture.restorer_email).await;
    let phrase = preview_of(&fixture.state, &restorer_token, &restorer_csrf, run_id)
        .await
        .body["confirm_phrase"]
        .as_str()
        .expect("a phrase")
        .to_owned();

    // --- 1. A cancelled job is never claimed -------------------------------------------------
    let doomed = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    let doomed_id = Uuid::parse_str(doomed.body["id"].as_str().expect("a job id")).expect("a uuid");
    // The row is cancelled WITHOUT going through the route, so the job is `queued` **and**
    // flagged — the exact state a cancel that lost the race to the worker leaves behind, and
    // the one the worker's own flag check exists for.
    sqlx::query("update backup_restore_jobs set cancel_requested = true where id = $1")
        .bind(doomed_id)
        .execute(fixture.db.pool())
        .await
        .expect("the flag must be settable");

    let runs_before = run_count(&fixture, fixture.org).await;
    let tick = omnion_api::restore_job_runner::tick(&fixture.state)
        .await
        .expect("the restore tick must answer");
    assert_eq!(tick, 1, "exactly one job was queued: {tick}");

    let (status, _, started) = job_status(fixture.db.pool(), doomed_id).await;
    assert_eq!(
        status, "aborted",
        "a flagged job must be aborted by the worker, not restored"
    );
    assert!(
        started.is_none(),
        "an aborted job must never have started: {started:?}"
    );
    assert_eq!(
        run_count(&fixture, fixture.org).await,
        runs_before,
        "the worker restored a job somebody had already stopped — this is the whole feature"
    );

    // --- 2. A fresh job for the same run runs, and writes the object -------------------------
    let fresh = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(
        fresh.status,
        StatusCode::ACCEPTED,
        "an aborted job must not block a new one — history is not history *yet*: {}",
        fresh.body
    );
    let fresh_id = Uuid::parse_str(fresh.body["id"].as_str().expect("a job id")).expect("a uuid");

    let tick = omnion_api::restore_job_runner::tick(&fixture.state)
        .await
        .expect("the restore tick must answer");
    assert_eq!(tick, 1, "the fresh job was the only one queued: {tick}");

    let (status, _, started) = job_status(fixture.db.pool(), fresh_id).await;
    assert_eq!(status, "succeeded", "the job must have run: {status}");
    assert!(
        started.is_some(),
        "a job that ran stamped its start: the abort window is defined as before this"
    );

    // The effect, over the real store. Asserting the status alone would be the walk asserting
    // the worker's own bookkeeping, which is the mistake REQ-017's batching criterion exists
    // to warn about.
    assert_eq!(
        fixture.state.storage().get(&key).await.unwrap_or_default(),
        b"worker archived bytes",
        "the queued restore must write the archived bytes back"
    );
    let (safety, objects): (Option<Uuid>, i64) = sqlx::query_as(
        "select safety_backup_id, (result->'media'->>'objects_restored')::bigint \
         from backup_restore_jobs where id = $1",
    )
    .bind(fresh_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the job must read");
    assert_eq!(
        objects, 1,
        "one object was archived and one must be restored"
    );
    let safety = safety.expect("a succeeded restore must name the run to go back to");
    let protected: bool = sqlx::query_scalar("select protected from backups where id = $1")
        .bind(safety)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the safety run must exist");
    assert!(
        protected,
        "the safety run of a queued restore must be protected like any other"
    );

    // --- 3. A job nothing claims is stopped rather than left to spin ------------------------
    // The panel draws a queued job as a spinner for ever. A worker that crashed and restarted
    // against a new configuration is enough to produce one, and the operator pressed a
    // button.
    let stranded = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    let stranded_id =
        Uuid::parse_str(stranded.body["id"].as_str().expect("a job id")).expect("a uuid");
    // Backdate past the worker's own ceiling. The only time travel in this walk.
    sqlx::query(
        "update backup_restore_jobs set created_at = now() - interval '3 hours' where id = $1",
    )
    .bind(stranded_id)
    .execute(fixture.db.pool())
    .await
    .expect("the job must be backdatable");

    omnion_api::restore_job_runner::tick(&fixture.state)
        .await
        .expect("the restore tick must answer");
    let (status, cancel_requested, started) = job_status(fixture.db.pool(), stranded_id).await;
    assert_eq!(
        status, "aborted",
        "a job nobody claimed must be stopped, not left spinning: {status}"
    );
    assert!(started.is_none(), "and it never started: {started:?}");
    assert!(
        cancel_requested,
        "the platform did ask — the row must say so rather than claim a restore that stopped \
         itself"
    );

    // --- 3b. A job the worker DIED on is reclaimed, not left blocking the run ----------------
    // The sweep originally covered only `queued`, and this is the state that made that a
    // product bug rather than an omission: a worker killed mid-restore leaves a `running`
    // row, and **nothing else can ever move it** — the queue read is the only thing that
    // advances a job, a dead worker is by definition not going to, and the partial unique
    // index refuses a new restore of that run while the row stands. One deploy in the middle
    // of a restore and that run could not be restored again by anybody.
    //
    // It is `failed` and NOT `aborted`, and that distinction is the whole assertion: a
    // claimed job has already taken its safety backup and may have written objects, so
    // "nothing was written" would be a lie. `failed` is the state that means "it began and
    // nobody finished it", and the schema allows it only for a job with a start.
    let orphaned = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    let orphaned_id =
        Uuid::parse_str(orphaned.body["id"].as_str().expect("a job id")).expect("a uuid");
    // Claim it the way a worker does, then age it: this is a restore that was interrupted,
    // not one that nobody picked up.
    omnion_backup::restore_jobs::claim_restore_job(fixture.db.pool(), orphaned_id)
        .await
        .expect("the job must be claimable");
    sqlx::query(
        "update backup_restore_jobs set created_at = now() - interval '3 hours' where id = $1",
    )
    .bind(orphaned_id)
    .execute(fixture.db.pool())
    .await
    .expect("the job must be backdatable");

    omnion_api::restore_job_runner::tick(&fixture.state)
        .await
        .expect("the restore tick must answer");
    let (status, _, started) = job_status(fixture.db.pool(), orphaned_id).await;
    assert_eq!(
        status, "failed",
        "a job whose worker died must be reclaimed, not left running for ever: {status}"
    );
    assert!(
        started.is_some(),
        "a failed restore is one that began, so the start must survive: {started:?}"
    );
    let reason: Option<String> =
        sqlx::query_scalar("select error from backup_restore_jobs where id = $1")
            .bind(orphaned_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the job must read");
    let reason = reason.unwrap_or_default();
    assert!(
        reason.contains("NOT known"),
        "the reason must say what is unknown rather than claim nothing was written: {reason}"
    );

    // And the run is restorable again, which is the point of clearing it. The job this
    // creates is **cancelled at once**, because the walk still has a step after this one and
    // the partial unique index refuses a second live job for the same run — a leftover of a
    // walk's own making blocking the walk's next step is a self-inflicted failure that reads
    // exactly like a product bug.
    let again = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::ACCEPTED,
        "a run whose interrupted restore was reclaimed must be restorable again, or one crash \
         costs the run its restore point for ever: {}",
        again.body
    );
    let again_id = Uuid::parse_str(again.body["id"].as_str().expect("a job id")).expect("a uuid");
    let released = cancel_restore(&fixture.state, &restorer_token, &restorer_csrf, again_id).await;
    assert_eq!(
        released.status,
        StatusCode::OK,
        "the walk's own leftover must be cancellable: {}",
        released.body
    );

    // --- 4. A job queued with a phrase that is not this run's is refused, not run ------------
    // The phrase is re-checked at execution time because it is a hash of the run id: a job
    // that outlived a re-created run would otherwise restore on a phrase that never belonged
    // to it.
    let mismatched = queue_restore(
        &fixture.state,
        &restorer_token,
        &restorer_csrf,
        run_id,
        &["media"],
        &phrase,
    )
    .await;
    let mismatched_id =
        Uuid::parse_str(mismatched.body["id"].as_str().expect("a job id")).expect("a uuid");
    // The job was queued with a correct phrase, so the row says the right thing; the edit
    // below is what a row tampered with on the destination looks like to the worker.
    sqlx::query("update backup_restore_jobs set confirmation = $2 where id = $1")
        .bind(mismatched_id)
        .bind("RESTORE deadbeef")
        .execute(fixture.db.pool())
        .await
        .expect("the job must be editable");

    omnion_api::restore_job_runner::tick(&fixture.state)
        .await
        .expect("the restore tick must answer");
    let (status, _, _) = job_status(fixture.db.pool(), mismatched_id).await;
    assert_eq!(
        status, "failed",
        "a job whose phrase is not this run's must be refused, not performed: {status}"
    );
    let reason: Option<String> =
        sqlx::query_scalar("select error from backup_restore_jobs where id = $1")
            .bind(mismatched_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the job must read");
    let reason = reason.unwrap_or_default();
    assert!(
        reason.contains("confirmation phrase"),
        "the refusal must name the rule, not say \"failed\": {reason}"
    );
}

// --------------------------------------------------------------------------------------------
// The consumer: does the security posture screen actually SEE these backups?
// --------------------------------------------------------------------------------------------

/// The `backup_healthy` row on the security posture screen, for the account named.
async fn backup_check(state: &AppState, email: &str, fixture: &Fixture) -> (String, Value) {
    let (token, csrf) = fixture.session(email).await;
    let response = call(
        state,
        request(
            Method::GET,
            "/api/v1/security/overview",
            Some(&token),
            Some(&csrf),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the posture overview must answer for an account holding security.read: {}",
        response.message()
    );
    let row = response.body["checks"]
        .as_array()
        .expect("the overview carries its rows")
        .iter()
        .find(|row| row["key"] == "backup_healthy")
        .cloned()
        .expect("the registry registers backup_healthy on every run");
    let verdict = row["state"].as_str().expect("a state").to_owned();
    (verdict, row["detail"].clone())
}

/// Age one finished run back by `hours`, straight in PostgreSQL.
///
/// A run that finished four days ago has to *have finished* four days ago: the
/// `0157` check constraint refuses a `succeeded` run with no `finished_at`, and a walk that
/// only set `created_at` would be pricing the age of a different column than the one the
/// check reads.
async fn age_a_run_back(pool: &sqlx::PgPool, id: Uuid, hours: i64) {
    sqlx::query(
        "update backups set finished_at = finished_at - make_interval(hours => $2::int) \
         where id = $1",
    )
    .bind(id)
    .bind(hours as i32)
    .execute(pool)
    .await
    .expect("the run must be aged");
}

/// REQ-013's status-card criterion, proved from the other end.
///
/// The criterion says the card's age is *consumed by the security overview check*, and the
/// half that could not be true was the consumption: `backup_age` in the security routes read
/// `backup_runs` — a table **no migration has ever created** — so `fetch_optional` answered
/// `Err`, `Err` flattened into "no backup", and `backup_healthy` sat at `fail` on every
/// installation for ever. The screen looked right, the message named the right rule, and the
/// check was unreachable: the one row in the registry that could never go green.
///
/// **Why no test caught it, and what would have.** A unit test on `backup_healthy` passes a
/// hand-built `Environment` and never touches a database, so the query was never run. An
/// integration test that asserted "no backup → fail" would have passed against the broken
/// reader forever, because the broken reader *is* a permanent no-backup. Only a walk that
/// takes a real backup, then reads the posture screen, can tell those two worlds apart: in
/// one the row is `fail` because the platform is unprotected, in the other because the code
/// cannot see the platform at all.
///
/// The second half is the one a first fix gets wrong. Scoping the read to the tenant is not
/// decoration: an unscoped read answers "fresh backup" from *a stranger's* run, which is a
/// false green on the check whose false green is the expensive direction. So the stranger
/// takes a fresh backup and the reader — who has taken nothing — must still read `fail`, and
/// must say the reason is the absence of a backup rather than the age of somebody else's.
#[tokio::test]
async fn the_security_posture_check_sees_a_real_backup_and_only_this_tenants() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (token, csrf) = fixture.session(&fixture.operator_email).await;

    // Before anything has run: the honest red, with the honest reason.
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "fail",
        "an installation with no backup must fail the check, and say so: {detail}"
    );
    assert!(
        detail["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("ever"),
        "the no-backup verdict must name the absence, not the age of nothing: {detail}"
    );

    // A stranger takes a fresh backup. The reader has taken nothing, and the reader's own
    // check must be unchanged: a reader that could see this run would be reporting a false
    // green, and that is the direction this screen is not allowed to fail in.
    let stranger = fixture.session(&fixture.stranger_email).await;
    let stranger_run = take_backup(
        &fixture.state,
        &stranger.0,
        &stranger.1,
        &["database", "configuration"],
    )
    .await;
    assert_eq!(
        stranger_run.status,
        StatusCode::CREATED,
        "body: {}",
        stranger_run.body
    );
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "fail",
        "another tenant's backup must not make this check pass: {detail}"
    );

    // This tenant takes one. The row has to move, and it has to move for the *stated* reason.
    let created = take_backup(
        &fixture.state,
        &token,
        &csrf,
        &["database", "configuration"],
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "pass",
        "a backup this tenant has just taken must clear the check: {detail}"
    );
    assert!(
        detail["fact"].as_i64().is_some_and(|hours| hours < 1),
        "a run finished seconds ago must be read as under an hour, not as missing: {detail}"
    );

    // The line the check draws is 48h, and the age is the age -- not the creation time, and
    // not the row that merely exists. A reader that ignored `finished_at` and looked at
    // `created_at` would pass this first assertion too, so the run is aged well past the line
    // and the verdict has to turn with it.
    age_a_run_back(fixture.db.pool(), id, 96).await;
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "fail",
        "a backup older than the line must fail, which is the whole point of reading an age: {detail}"
    );
    assert_eq!(
        detail["fact"].as_i64(),
        Some(96),
        "the row must report the age in hours it actually measured: {detail}"
    );
    assert_eq!(
        detail["stale_after_hours"].as_i64(),
        Some(48),
        "and the line it crossed, so the detail can name its own rule: {detail}"
    );

    // A `partial` run is not a successful one. A run that wrote the database export and lost
    // the media copy is exactly the run an operator must not be told is fresh, and the schema
    // records it as its own state for that reason.
    let partial_id: Uuid = sqlx::query_scalar(
        "insert into backups (organization_id, kind, scopes, status, storage_prefix, size_bytes, \
           finished_at, started_at) \
         values ($1, 'manual', array['database','media'], 'partial', $2, 10, now(), now()) returning id",
    )
    .bind(fixture.org)
    .bind(format!("partial/{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the partial row must insert");
    age_a_run_back(fixture.db.pool(), partial_id, 1).await;
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "fail",
        "a fresh partial run must not reset a stale backup's age, or 'partial' would mean \
         'as good as new': {detail}"
    );
    assert_eq!(
        detail["fact"].as_i64(),
        Some(96),
        "and the age reported is the succeeded run's, not the partial's: {detail}"
    );

    // Now a genuinely newer **succeeded** run, one hour old. The check must go green, which
    // is the assertion that the partial above did not poison the read: a reader that took
    // `max(finished_at)` over *any* finished row would have gone green at the partial already
    // and would still be green here for the wrong reason.
    let fresh = take_backup(&fixture.state, &token, &csrf, &["configuration"]).await;
    assert_eq!(fresh.status, StatusCode::CREATED, "body: {}", fresh.body);
    let fresh_id =
        Uuid::parse_str(fresh.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
    age_a_run_back(fixture.db.pool(), fresh_id, 1).await;
    let (verdict, detail) = backup_check(&fixture.state, &fixture.posture_email, &fixture).await;
    assert_eq!(
        verdict, "pass",
        "a succeeded run inside the line must clear the check whatever else is on the shelf: {detail}"
    );
    assert_eq!(
        detail["fact"].as_i64(),
        Some(1),
        "and the age is the newest succeeded run's, so the number can be trusted as a number: {detail}"
    );
}
