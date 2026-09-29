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
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_backup as omnion_backup;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_api::rate_limit_middleware::RateLimiter;
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
    TestResponse { status, set_cookies, body, raw }
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

// --------------------------------------------------------------------------------------------
// Fixture
// --------------------------------------------------------------------------------------------

/// A disposable installation: two organizations, one account in each with its own role.
struct Fixture {
    state: AppState,
    db: Db,
    operator_email: String,
    reader_email: String,
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
        seed::ensure(db.pool()).await.expect("the IAM seed must run");
        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let storage = Storage::Fs(
            omnion_storage::FsStorage::new(
                std::env::temp_dir().join("omnion-backup-suite-store"),
            )
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
        // A stranger in another organization. It holds the *same* keys, so every refusal this
        // suite proves is about the tenancy boundary and not about a missing permission.
        let (stranger_id, stranger_email) = bind_role(
            &db,
            other,
            platform_id,
            "Backup Stranger",
            &OPERATOR_PERMISSIONS,
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
            stranger_email,
            org,
            other_org: other,
            accounts: vec![platform_id, operator_id, reader_id, stranger_id],
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
        assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
        (
            response.cookie("omnion_session").expect("a session cookie"),
            response
                .cookie("omnion_csrf")
                .unwrap_or_default(),
        )
    }

    /// The root a run's artifacts landed under, for a run with this prefix.
    fn artifact(&self, prefix: &str, key: &str) -> std::path::PathBuf {
        self.root.join(prefix.trim_matches('/')).join(key.trim_start_matches('/'))
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
    .bind(format!("backup-{}-{}", name.to_lowercase().replace(' ', "-"), Uuid::new_v4().simple()))
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
            scope: Scope::Organization { organization_id: org },
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
async fn take_backup(
    state: &AppState,
    token: &str,
    csrf: &str,
    scopes: &[&str],
) -> TestResponse {
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
    assert_eq!(response.status, StatusCode::CREATED, "body: {}", response.body);
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
            bytes.len() as i64, *recorded,
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
    // `OffsetDateTime` serialises as a tuple, not an RFC 3339 string, so the card is a JSON
    // array — an assertion written for the string form fails on a working endpoint, which is
    // worse than no assertion: it trains the reader to distrust the test rather than the code.
    assert!(
        cards.body["last_successful_at"].is_array(),
        "the card carries a timestamp, whatever shape it serialises in: {}",
        cards.body["last_successful_at"]
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
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
    let prefix = created.body["backup"]["storage_prefix"]
        .as_str()
        .expect("a prefix")
        .to_owned();

    // A clean verification first: a button that is red before anything happened teaches the
    // operator to ignore it, and then it is useless on the day it matters.
    let clean = call(
        &fixture.state,
        request(Method::POST, &verify_uri(id), Some(&token), Some(&csrf), None),
    )
    .await;
    assert_eq!(clean.status, StatusCode::OK);
    assert_eq!(clean.body["clean"], json!(true), "body: {}", clean.body);
    // Only the parts THIS run asked for. The suite database is shared with every other
    // integration walk, and an earlier run's parts are still on the destination — so an
    // unscoped equality here is asserting what the other suites left behind, not what this
    // backup proved. The assertion that matters is that both of ITS parts matched, and that
    // the response is clean.
    let matched = clean.body["matched"].as_array().cloned().unwrap_or_default();
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
        request(Method::POST, &verify_uri(id), Some(&token), Some(&csrf), None),
    )
    .await;
    assert_eq!(dirty.status, StatusCode::OK, "a mismatch is a verdict, not an error");
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
    assert!(audited >= 2, "both verifications must be audited, got {audited}");
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
    for forbidden in ["access_key", "secret_key", "passphrase_value", "password", "token"] {
        assert!(
            !text.contains(forbidden),
            "the settings response must not carry `{forbidden}`: {}",
            good.text()
        );
    }
    assert!(text.contains("credential_ref"), "the reference field is the honest one");

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
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
    assert!(!refused.message().is_empty(), "a refusal must say why: {}", refused.body);

    // The refused save wrote nothing: the stored root is still the writable one.
    let stored: String = sqlx::query_scalar("select local_root from backup_settings where id = 1")
        .fetch_one(fixture.db.pool())
        .await
        .expect("the settings row must read");
    assert!(!stored.is_empty(), "a refused save must not have stored the empty root");
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
    for uri in [backups_uri(), status_uri(), backup_uri(id), manifest_uri(id)] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&reader), None, None),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{uri} must be readable: {}", response.body);
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
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "body: {}", refused.body);
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
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "body: {}", refused.body);
    let retention: i32 =
        sqlx::query_scalar("select default_retention from backup_settings where id = 1")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the settings row must read");
    assert_eq!(retention, 7, "a refused save must not have written the retention");

    // Anonymous is refused everywhere.
    for uri in [backups_uri(), status_uri(), settings_uri()] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, None, None, None),
        )
        .await;
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
            request(method.clone(), &uri, Some(&stranger), Some(&stranger_csrf), None),
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
    assert_eq!(still_there, 1, "a refused delete must not remove another tenant's row");
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
    assert_eq!(duplicated.status, StatusCode::BAD_REQUEST, "body: {}", duplicated.body);
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
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
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
        assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
        let id = Uuid::parse_str(created.body["backup"]["id"].as_str().expect("an id")).expect("uuid");
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
