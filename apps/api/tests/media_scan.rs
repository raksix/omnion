//! Integration tests for the scanning pipeline (REQ-010, slice 4).
//!
//! These walk the **real router against a real scanner over a real socket**. A stub that
//! returns a verdict from a function call proves the bookkeeping and nothing about the
//! client, and the client is where a scanner outage lives: the ingest rule is that an
//! unreachable scanner must never lose an upload and must never serve an unscanned file, and
//! that rule can only be proved by pointing the platform at a port where nothing is listening.
//!
//! Every assertion is against observable state — the response body, **the row read out of
//! PostgreSQL**, and the bytes — because a response that omits a field is indistinguishable
//! from one that stored it and chose not to say so.

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The permission keys the editor of this suite holds.
///
/// `media.scan.manage` is deliberately *separate* from `media.manage`: an editor who may
/// organise a whole library still may not release a quarantined file, and this suite is where
/// that separation is proved rather than asserted.
const EDITOR_PERMISSIONS: [&str; 8] = [
    "media.read",
    "media.upload",
    "media.delete",
    "media.update",
    "media.manage",
    "media.settings.manage",
    "media.share",
    "media.scan.manage",
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
        "the upload must succeed whatever the scanner does: {}",
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

// ---------------------------------------------------------------------------------------------
// A real scanner, on a real socket
// ---------------------------------------------------------------------------------------------

/// What a stub scanner should answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScannerAnswer {
    /// The scanner looked and found nothing.
    Clean,
    /// The scanner found something.
    Flagged,
    /// The scanner answers with something the platform must not read as a pass.
    Nonsense,
}

/// A scanner on loopback, speaking the platform's wire shape.
///
/// Deliberately a hand-rolled HTTP/1.1 server rather than an axum sub-app: the point is to
/// prove the client works against *something that is not this codebase*, and a sub-app would
/// share every assumption the client makes, including the ones that are wrong.
struct StubScanner {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
    /// How many requests it answered, so a walk can prove the file really reached it.
    seen: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StubScanner {
    /// Start a scanner that answers `answer` to everything.
    async fn start(answer: ScannerAnswer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the stub scanner must bind");
        let port = listener.local_addr().expect("a local address").port();
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let counter = seen.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    // Read the head, then drain the body by content-length. Enough to be a
                    // real server for one request; the client is what is under test.
                    let mut buffer = vec![0_u8; 8192];
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    while head.len() < length {
                        let Ok(more) = stream.read(&mut buffer).await else {
                            break;
                        };
                        if more == 0 {
                            break;
                        }
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    let body = match answer {
                        ScannerAnswer::Clean => json!({
                            "status": "clean",
                            "detail": "no threats found",
                            "engine": "stub-clean",
                        })
                        .to_string(),
                        ScannerAnswer::Flagged => json!({
                            "status": "flagged",
                            "detail": "threat: Eicar-Test-Signature in payload",
                            "engine": "stub-flagger",
                        })
                        .to_string(),
                        // A scanner that changed its API shape. The client must refuse this
                        // rather than read the absence of a known word as a pass.
                        ScannerAnswer::Nonsense => json!({
                            "verdict_code": 3,
                            "message": "unmodelled response",
                        })
                        .to_string(),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { port, handle, seen }
    }

    /// The endpoint an operator would type into the settings screen.
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// How many scans it actually answered.
    fn requests(&self) -> usize {
        self.seen.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for StubScanner {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// An origin nothing is listening on, for the outage walk.
///
/// A port that is bound and immediately dropped is the honest version: it is refused, rather
/// than a hostname that resolves nowhere and produces a DNS error instead of a connection
/// error — the platform has to survive both, but the connection one is the common case.
async fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port must be available");
    let port = listener.local_addr().expect("a local address").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

/// Everything one walk needs: an organization, a site, an editor, a reader and a scannerless one.
struct Fixture {
    state: AppState,
    db: Db,
    storage: Storage,
    editor_email: String,
    reader_email: String,
    editorless_email: String,
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
        .bind("Scan Test")
        .bind(format!("media-scan-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Scan Site")
        .fetch_one(db.pool())
        .await
        .expect("the site must be created");

        let (platform_id, _) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("media-editor-{}", Uuid::new_v4().simple()),
                name: "Media Editor".to_owned(),
                description: "Drives the scanning pipeline".to_owned(),
                priority: 400,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the role must be created");
        let entries: Vec<RolePermissionInput> = EDITOR_PERMISSIONS
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
                user_id: editor_id,
                scope: Scope::Organization {
                    organization_id: org,
                },
                granted_by: Some(platform_id),
                expires_at: None,
            },
        )
        .await
        .expect("the binding must be granted");

        // A reader: `media.read` and nothing else. The release and the sweep must both refuse
        // it, and so must the settings write.
        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        let reader_role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("media-reader-{}", Uuid::new_v4().simple()),
                name: "Media Reader".to_owned(),
                description: "Reads the library and nothing more".to_owned(),
                priority: 300,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the reader role must be created");
        role_store::set_role_permissions(
            db.pool(),
            reader_role.id,
            &[RolePermissionInput {
                key: "media.read".to_owned(),
                effect: Effect::Allow,
            }],
        )
        .await
        .expect("the reader permissions must be written");
        bindings::grant(
            db.pool(),
            NewBinding {
                role_id: reader_role.id,
                user_id: reader_id,
                scope: Scope::Organization {
                    organization_id: org,
                },
                granted_by: Some(platform_id),
                expires_at: None,
            },
        )
        .await
        .expect("the reader binding must be granted");

        // An *editor without* `media.scan.manage`: everything a media editor may do except
        // release a quarantined file. This is the account the permission separation is about.
        //
        // Its **own** role, deliberately. The first version of this reused the editor's role,
        // took one permission away, granted it, and then put the permission back for the
        // editor — and because a role's permissions are a property of the role rather than of
        // the binding, restoring them for one account restored them for both. The walk then
        // proved the opposite of what it meant to: the "editor without" released a held file
        // and the assertion that should have read `403` read `200`. Two roles, one
        // permission apart, is the only shape in which the two accounts can differ at all.
        let (editorless_id, editorless_email) = create_account(&db, Some(org)).await;
        let no_scan_role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("media-editor-{}", Uuid::new_v4().simple()),
                name: "Media Editor (no scanning)".to_owned(),
                description: "Everything but releasing a quarantined file".to_owned(),
                priority: 401,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the reduced role must be created");
        let no_scan_keys: Vec<RolePermissionInput> = EDITOR_PERMISSIONS
            .iter()
            .filter(|key| **key != "media.scan.manage")
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect();
        assert_eq!(
            no_scan_keys.len(),
            EDITOR_PERMISSIONS.len() - 1,
            "exactly one permission is taken away"
        );
        role_store::set_role_permissions(db.pool(), no_scan_role.id, &no_scan_keys)
            .await
            .expect("the reduced permissions must be written");
        bindings::grant(
            db.pool(),
            NewBinding {
                role_id: no_scan_role.id,
                user_id: editorless_id,
                scope: Scope::Organization {
                    organization_id: org,
                },
                granted_by: Some(platform_id),
                expires_at: None,
            },
        )
        .await
        .expect("the reduced binding must be granted");

        Some(Self {
            state,
            db,
            storage,
            editor_email,
            reader_email,
            editorless_email,
            accounts: vec![platform_id, editor_id, reader_id, editorless_id],
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
    async fn editorless_token(&self) -> String {
        login(&self.state, &self.editorless_email).await
    }

    /// Turn scanning on for a site, pointing it at `endpoint`.
    async fn enable_scanning(&self, site: Uuid, token: &str, endpoint: &str) {
        let response = call(
            &self.state,
            request(
                Method::PUT,
                &scan_settings_uri(site),
                Some(token),
                Some(json!({ "enabled": true, "endpoint": endpoint })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "scanning must be switchable on: {}",
            response.body
        );
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
    let email = format!("scan-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Scan Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
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

/// The scanning settings route for a site.
fn scan_settings_uri(site: Uuid) -> String {
    format!("/api/v1/media/scan-settings?site_id={site}")
}

/// The sweep route for a site.
fn scan_run_uri(site: Uuid) -> String {
    format!("/api/v1/media/scan/run?site_id={site}")
}

/// The quarantine list of a site.
fn quarantine_uri(site: Uuid) -> String {
    format!("/api/v1/media/quarantine?site_id={site}")
}

/// The raw path of one file.
fn raw_uri(media: Uuid) -> String {
    format!("/api/v1/media/{media}/raw?preset=card")
}

/// The public path of one file.
fn public_uri(media: Uuid) -> String {
    format!("/api/v1/public/media/{media}")
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The ingest rule, the serve rule and a release: the whole pipeline over a real socket.
#[tokio::test]
async fn a_flagged_file_is_held_everywhere_and_a_release_puts_it_back() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A site created *after* the migration has a policy row, thanks to the trigger — read out
    // of the database rather than inferred from a GET, because a GET that invents defaults is
    // exactly the gap the trigger exists to close.
    let seeded: Option<(bool, String)> =
        sqlx::query_as("select enabled, on_error from media_scan_settings where site_id = $1")
            .bind(site)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the policy row must read");
    let (enabled, on_error) = seeded.expect("a new site must have a scanning policy row");
    assert!(!enabled, "scanning is off until somebody turns it on");
    assert_eq!(
        on_error, "hold",
        "an outage must not publish an unscanned file"
    );

    // Point the site at a real scanner that flags everything.
    let scanner = StubScanner::start(ScannerAnswer::Flagged).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;

    // The upload *succeeds* even though the scanner will flag it: the ingest half of
    // "best-effort at ingest". If this returned an error the rule would be broken.
    let media = upload(
        &fixture.state,
        &token,
        site,
        "payload.txt",
        b"harmless looking bytes",
    )
    .await;

    // Before any sweep the file is `pending`, and `pending` is not servable on a site that
    // turned scanning on. This is the strict half, and it is the state every upload is in for
    // as long as the sweep has not run — so it is a state the platform has to survive.
    let before: (String,) = sqlx::query_as("select scan_status from media where id = $1")
        .bind(media)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(before.0, "pending");

    let raw_before = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(
        raw_before.status,
        StatusCode::FORBIDDEN,
        "an unscanned file is not served on a site that asked for scanning: {}",
        raw_before.body
    );
    assert_eq!(raw_before.body["error"]["code"], json!("file_not_scanned"));

    // The public renderer refuses it too, with the same code: the public path is the one an
    // unauthenticated visitor reaches, and it is strictly the worst place to serve it.
    let public_before = call(
        &fixture.state,
        request(Method::GET, &public_uri(media), None, None),
    )
    .await;
    assert_eq!(public_before.status, StatusCode::FORBIDDEN);
    assert_eq!(
        public_before.body["error"]["code"],
        json!("file_not_scanned")
    );

    // Run the sweep.
    let sweep = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(sweep.status, StatusCode::OK, "body: {}", sweep.body);
    assert_eq!(sweep.body["flagged"], json!(1), "the scanner flagged it");
    assert_eq!(sweep.body["outcome"], json!("flagged"));
    assert!(
        scanner.requests() >= 1,
        "the file really reached the scanner over a socket"
    );

    // The row, out of the database: `flagged`, and the scanner's *own words*.
    let after: (String, String) =
        sqlx::query_as("select scan_status, scan_detail from media where id = $1")
            .bind(media)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert_eq!(after.0, "flagged");
    assert!(
        after.1.contains("Eicar-Test-Signature"),
        "the scanner's words are recorded, not a translation: {}",
        after.1
    );

    // A quarantine row is open, it names the run, and there is exactly one of it.
    let held: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "select id, detail, run_id from media_quarantines where media_id = $1 and released_at is null",
    )
    .bind(media)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the quarantine rows must read");
    assert_eq!(held.len(), 1, "one open quarantine, not two");
    let quarantine_id = held[0].0;
    assert!(held[0].2.is_some(), "it names the run that flagged it");

    // Held on every serve path.
    for (name, uri) in [
        ("raw with a preset", raw_uri(media)),
        ("public", public_uri(media)),
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&token), None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{name} must refuse a held file: {}",
            response.body
        );
        assert_eq!(response.body["error"]["code"], json!("file_quarantined"));
    }

    // The run log says what happened, in a sentence.
    let runs = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/scan/runs?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(runs.status, StatusCode::OK);
    assert_eq!(runs.body["runs"][0]["outcome"], json!("flagged"));
    assert!(
        runs.body["runs"][0]["summary"]
            .as_str()
            .expect("a summary")
            .contains("held")
    );

    // A release with no reason is refused *before* anything is written.
    let no_reason = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/quarantine/{quarantine_id}/release"),
            Some(&token),
            Some(json!({ "reason": "   " })),
        ),
    )
    .await;
    assert_eq!(no_reason.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        no_reason.body["error"]["code"],
        json!("release_reason_required")
    );
    let still_open: i64 = sqlx::query_scalar(
        "select count(*) from media_quarantines where id = $1 and released_at is null",
    )
    .bind(quarantine_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(still_open, 1, "a refused release changes nothing");

    // An editor *without* `media.scan.manage` may not release it — the separation the
    // permission exists for, proved rather than asserted.
    let editorless = fixture.editorless_token().await;
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/quarantine/{quarantine_id}/release"),
            Some(&editorless),
            Some(json!({ "reason": "I looked at it" })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "organising a library is not the power to release a held file: {}",
        refused.body
    );

    // A reader may read the list and may neither run a sweep nor release.
    let reader = fixture.reader_token().await;
    let reader_list = call(
        &fixture.state,
        request(Method::GET, &quarantine_uri(site), Some(&reader), None),
    )
    .await;
    assert_eq!(
        reader_list.status,
        StatusCode::OK,
        "a reader may see what is held"
    );
    assert_eq!(reader_list.body["file_count"], json!(1));
    assert_eq!(
        call(
            &fixture.state,
            request(Method::POST, &scan_run_uri(site), Some(&reader), None),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // The release, with a reason, by somebody who holds the key.
    let released = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/quarantine/{quarantine_id}/release"),
            Some(&token),
            Some(json!({ "reason": "checked by hand: an internal test payload" })),
        ),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "body: {}", released.body);
    assert!(
        released.body["reason"]
            .as_str()
            .expect("the reason is echoed")
            .contains("checked by hand")
    );

    // The quarantine row is *closed*, not deleted: the history of the hold is the whole point.
    let closed: (Option<OffsetDateTimeAlias>, String) =
        sqlx::query_as("select released_at, release_reason from media_quarantines where id = $1")
            .bind(quarantine_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must still be there");
    assert!(
        closed.0.is_some(),
        "a release closes the row, never deletes it"
    );
    assert!(closed.1.contains("checked by hand"));

    // And the file serves again — but as `skipped`, never as `clean`. Nobody has said the
    // file is safe; a human decided to let it go, and the row has to say exactly that.
    let after_release: (String,) = sqlx::query_as("select scan_status from media where id = $1")
        .bind(media)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(after_release.0, "skipped", "a release is not a clean scan");

    let served = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK, "body: {}", served.body);
    assert_eq!(
        served.raw, b"harmless looking bytes",
        "the real bytes, not a stub"
    );

    // The audit trail names the release, its reason and the scanner's finding.
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "select action, metadata from audit_log where target_id = $1 order by created_at",
    )
    .bind(media.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit log must read");
    let release_entry = audit
        .iter()
        .find(|(action, _)| action == "media.scan_released")
        .expect("a release writes an audit entry");
    assert!(
        release_entry.1["reason"]
            .as_str()
            .expect("the reason is recorded")
            .contains("checked by hand")
    );
    assert!(
        release_entry.1["detail"]
            .as_str()
            .expect("the scanner's finding is recorded")
            .contains("Eicar")
    );

    // A second release of the same quarantine is a conflict, not a second undo.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/quarantine/{quarantine_id}/release"),
            Some(&token),
            Some(json!({ "reason": "again" })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "it is already closed");

    fixture.cleanup().await;
}

/// A scanner outage never loses an upload, and never serves the file.
#[tokio::test]
async fn an_unreachable_scanner_stores_the_file_and_serves_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A port with nothing on it. Not a bad hostname: a DNS failure and a refused connection
    // are different paths and both have to survive, but the refused one is the common case.
    let dead = dead_endpoint().await;
    fixture.enable_scanning(site, &token, &dead).await;

    let media = upload(
        &fixture.state,
        &token,
        site,
        "outage.txt",
        b"uploaded during an outage",
    )
    .await;

    // The upload succeeded: the bytes are stored and the row exists.
    let exists: (String, i64) =
        sqlx::query_as("select storage_key, size_bytes from media where id = $1")
            .bind(media)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must exist — an outage loses nothing");
    assert!(!exists.0.is_empty());
    assert_eq!(
        exists.1,
        i64::try_from(b"uploaded during an outage".len()).expect("a length"),
        "the bytes are stored even though the scan never completed"
    );

    let sweep = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(
        sweep.status,
        StatusCode::OK,
        "the sweep itself must not fail"
    );
    assert_eq!(sweep.body["errors"], json!(1), "the scanner did not answer");
    // A pass whose only result was errors is reported as an error, never as `clean`. This is
    // the sentence that matters on the morning the scanner was down.
    assert_eq!(
        sweep.body["outcome"],
        json!("error"),
        "a failed pass must not read as a clean one"
    );

    let status: (String,) = sqlx::query_as("select scan_status from media where id = $1")
        .bind(media)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(status.0, "error");

    // `hold` is the default and it is the strict one: an unfinished scan is not served.
    let refused = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"]["code"], json!("file_scan_failed"));

    // Switching the site to `serve` is the operator's risk decision about their own site, and
    // it takes effect on the *next* request — the policy is read per request, not cached.
    let permissive = call(
        &fixture.state,
        request(
            Method::PUT,
            &scan_settings_uri(site),
            Some(&token),
            Some(json!({ "on_error": "serve" })),
        ),
    )
    .await;
    assert_eq!(permissive.status, StatusCode::OK);
    let now_served = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(
        now_served.status,
        StatusCode::OK,
        "an operator who chose `serve` gets a working site"
    );

    fixture.cleanup().await;
}

/// An answer the platform cannot read is an error, never a pass.
#[tokio::test]
async fn a_scanner_that_changes_its_api_shape_fails_closed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A scanner answering `{"verdict_code": 3, "message": "..."}` — a real, different API.
    // The pipeline must not read the absence of a known word as a clean result, which is the
    // single worst default in a security pipeline.
    let scanner = StubScanner::start(ScannerAnswer::Nonsense).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;

    let media = upload(&fixture.state, &token, site, "odd.txt", b"bytes").await;
    let sweep = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(sweep.status, StatusCode::OK);
    assert_eq!(sweep.body["errors"], json!(1));
    assert_eq!(
        sweep.body["flagged"],
        json!(0),
        "an unreadable answer is not a flag"
    );

    let status: (String,) = sqlx::query_as("select scan_status from media where id = $1")
        .bind(media)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(status.0, "error", "fails closed");

    let held: i64 = sqlx::query_scalar(
        "select count(*) from media_quarantines where media_id = $1 and released_at is null",
    )
    .bind(media)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(held, 0, "an unreadable answer is not a quarantine either");

    fixture.cleanup().await;
}

/// A clean file is served, and a sweep that finds nothing still writes a run.
#[tokio::test]
async fn a_clean_file_is_served_and_an_empty_sweep_still_logs_a_run() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let scanner = StubScanner::start(ScannerAnswer::Clean).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;

    // A sweep with nothing pending, *before* anything is uploaded, is the case the run log
    // exists for: "the last run was at 02:00 and it was clean" has to be answerable on the
    // day nothing was found. Run after an upload it is not an empty sweep at all — the
    // pending file is exactly what it claims.
    let empty = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.body["scanned"], json!(0));
    assert_eq!(empty.body["outcome"], json!("clean"));
    assert_eq!(scanner.requests(), 0, "an empty sweep talks to nobody");

    let media = upload(&fixture.state, &token, site, "clean.txt", b"all good").await;

    let runs = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/scan/runs?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(runs.status, StatusCode::OK);
    assert_eq!(
        runs.body["runs"].as_array().expect("an array").len(),
        1,
        "a run that found nothing is still a run"
    );

    // Now the real sweep.
    let sweep = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(sweep.body["scanned"], json!(1));
    assert_eq!(sweep.body["outcome"], json!("clean"));
    assert_eq!(scanner.requests(), 1, "the file reached the scanner");

    // A clean file serves, and its detail column is *empty* — a report that greps the detail
    // must not find "no threats found" on every file and claim a positive for all of them.
    let row: (String, String) =
        sqlx::query_as("select scan_status, scan_detail from media where id = $1")
            .bind(media)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert_eq!(row.0, "clean");
    assert_eq!(row.1, "", "a clean scan leaves the detail column empty");

    let served = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.raw, b"all good");

    let held: i64 = sqlx::query_scalar("select count(*) from media_quarantines where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(held, 0, "a clean scan quarantines nothing");

    fixture.cleanup().await;
}

/// Every out-of-range setting names its own field, and the response carries no secret.
#[tokio::test]
async fn the_scan_settings_refuse_out_of_range_values_and_hold_no_secret() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // Enabling with no endpoint is refused, and the message is under the endpoint.
    let no_endpoint = call(
        &fixture.state,
        request(
            Method::PUT,
            &scan_settings_uri(site),
            Some(&token),
            Some(json!({ "enabled": true, "endpoint": "" })),
        ),
    )
    .await;
    assert_eq!(no_endpoint.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        no_endpoint.body["error"]["code"],
        json!("invalid_scan_setting")
    );
    assert_eq!(
        no_endpoint.body["error"]["details"]["field"],
        json!("endpoint")
    );

    // Each out-of-range value names its own field, on the *save* and on the *test*: the two
    // paths agreeing is the assertion, because a screen that renders the message under the
    // wrong input is the bug this catches.
    for (field, body) in [
        ("timeout_seconds", json!({ "timeout_seconds": 0 })),
        ("timeout_seconds", json!({ "timeout_seconds": 121 })),
        ("on_error", json!({ "on_error": "ignore" })),
        ("max_scan_mb", json!({ "max_scan_mb": 0 })),
        ("max_scan_mb", json!({ "max_scan_mb": 2000 })),
    ] {
        for (method, uri) in [
            (Method::PUT, scan_settings_uri(site)),
            (
                Method::POST,
                format!("/api/v1/media/scan/test?site_id={site}"),
            ),
        ] {
            let response = call(
                &fixture.state,
                request(method.clone(), &uri, Some(&token), Some(body.clone())),
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{field} on {method}"
            );
            assert_eq!(
                response.body["error"]["details"]["field"],
                json!(field),
                "the message must be under the `{field}` input on {method}"
            );
        }
    }

    // A pasted key in the *reference* field is refused, because a settings row that holds
    // key material has to be scrubbed from every response and guarded as a credential.
    let pasted = call(
        &fixture.state,
        request(
            Method::PUT,
            &scan_settings_uri(site),
            Some(&token),
            Some(json!({ "secret_env": "s3cr3t-value" })),
        ),
    )
    .await;
    assert_eq!(pasted.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        pasted.body["error"]["details"]["field"],
        json!("secret_env")
    );

    // A name is accepted, and the *response* never carries a value: scanned over the raw
    // bytes for anything that could be a credential.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &scan_settings_uri(site),
            Some(&token),
            Some(json!({
                "enabled": true,
                "endpoint": "http://127.0.0.1:3310",
                "secret_env": "MEDIA_SCAN_SECRET",
                "max_scan_mb": 64,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(saved.body["secret_env"], json!("MEDIA_SCAN_SECRET"));
    assert!(
        saved.body["behaviour"]
            .as_str()
            .expect("a behaviour sentence")
            .contains("64-MB")
    );

    let raw = String::from_utf8_lossy(&saved.raw).to_lowercase();
    for needle in [
        "\"secret\"",
        "access_key",
        "password",
        "credential",
        "bearer",
    ] {
        assert!(
            !raw.contains(needle),
            "the response carries `{needle}`: {raw}"
        );
    }

    // The reference is stored as a *name* and the name is not resolvable here, which the
    // screen must be able to say — "scanning is on and nothing is ever scanned" otherwise
    // looks exactly like a scanner that finds nothing.
    let stored: (String,) =
        sqlx::query_as("select secret_env from media_scan_settings where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert_eq!(stored.0, "MEDIA_SCAN_SECRET");

    let read = call(
        &fixture.state,
        request(Method::GET, &scan_settings_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(read.body["secret_available"], json!(false));
    assert_eq!(read.body["configured"], json!(true));
    assert_eq!(read.body["max_scan_mb"], json!(64));

    // A partial save preserves what it did not send — the rule that stops a six-field form
    // from resetting the other four.
    let partial = call(
        &fixture.state,
        request(
            Method::PUT,
            &scan_settings_uri(site),
            Some(&token),
            Some(json!({ "timeout_seconds": 45 })),
        ),
    )
    .await;
    assert_eq!(partial.body["timeout_seconds"], json!(45));
    assert_eq!(
        partial.body["max_scan_mb"],
        json!(64),
        "untouched fields survive"
    );
    assert_eq!(partial.body["secret_env"], json!("MEDIA_SCAN_SECRET"));

    // A reader may read the policy and may not write it.
    let reader = fixture.reader_token().await;
    assert_eq!(
        call(
            &fixture.state,
            request(Method::GET, &scan_settings_uri(site), Some(&reader), None),
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &fixture.state,
            request(
                Method::PUT,
                &scan_settings_uri(site),
                Some(&reader),
                Some(json!({ "max_scan_mb": 10 })),
            ),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &fixture.state,
            request(Method::POST, &scan_run_uri(site), Some(&reader), None),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &fixture.state,
            request(Method::GET, &scan_settings_uri(site), None, None),
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );

    fixture.cleanup().await;
}

/// A sweep on a site that has not turned scanning on is refused rather than quietly passing.
#[tokio::test]
async fn a_sweep_without_scanning_enabled_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let response = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], json!("scanning_disabled"));

    // And with scanning off, a pending file serves normally: the strict rule applies to a
    // site that asked for it, and applying it everywhere would empty every library.
    let media = upload(&fixture.state, &token, site, "unscanned.txt", b"fine").await;
    let served = call(
        &fixture.state,
        request(Method::GET, &raw_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK, "body: {}", served.body);
    assert_eq!(served.raw, b"fine");

    fixture.cleanup().await;
}

/// The scanner probe answers about the *posted* configuration, and a dead scanner says so.
#[tokio::test]
async fn the_scanner_probe_reports_a_real_scan_of_the_candidate() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let scanner = StubScanner::start(ScannerAnswer::Clean).await;

    let ok = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/scan/test?site_id={site}"),
            Some(&token),
            Some(json!({ "enabled": true, "endpoint": scanner.endpoint() })),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body["ok"], json!(true), "body: {}", ok.body);
    assert_eq!(ok.body["status"], json!("clean"));
    assert_eq!(ok.body["engine"], json!("stub-clean"));
    assert_eq!(
        scanner.requests(),
        1,
        "the probe posts real bytes, not a health check"
    );

    // A dead endpoint is reported as *not ok* with the reason, and it does not save anything:
    // the operator is looking at unsaved edits, so a result about the saved row would be a
    // result about something they are no longer looking at.
    let dead = dead_endpoint().await;
    let failed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/scan/test?site_id={site}"),
            Some(&token),
            Some(json!({ "enabled": true, "endpoint": dead })),
        ),
    )
    .await;
    assert_eq!(
        failed.status,
        StatusCode::OK,
        "a dead scanner is a result, not a 500"
    );
    assert_eq!(failed.body["ok"], json!(false));
    assert_eq!(failed.body["status"], json!("error"));
    assert!(
        failed.body["detail"]
            .as_str()
            .expect("a reason")
            .contains("could not be reached"),
        "{}",
        failed.body
    );

    // Nothing was written by either probe.
    let stored: Option<(bool, String)> =
        sqlx::query_as("select enabled, endpoint from media_scan_settings where site_id = $1")
            .bind(site)
            .fetch_optional(fixture.db.pool())
            .await
            .expect("the row must read");
    let (enabled, endpoint) = stored.expect("the row exists");
    assert!(!enabled, "a probe never saves");
    assert!(endpoint.is_empty(), "a probe never saves");

    fixture.cleanup().await;
}

/// A share link over a held file stops serving, and says so without leaking the finding.
#[tokio::test]
async fn a_share_link_over_a_held_file_stops_serving() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // Scanning is *off* for this half, so the file is servable when the link is made — the
    // point is that turning scanning on later is what closes it.
    let media = upload(&fixture.state, &token, site, "shared.txt", b"shared bytes").await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media}/shares"),
            Some(&token),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let token_value = created.body["token"]
        .as_str()
        .expect("the token is returned exactly once")
        .to_owned();

    let before = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/media/shared/{token_value}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK);
    assert_eq!(before.raw, b"shared bytes");

    // Now a scanner flags the file.
    let scanner = StubScanner::start(ScannerAnswer::Flagged).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;
    call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;

    // The link made yesterday must stop working. This is the check-at-serve-time rule, and
    // it is why the share route reads the file's state rather than trusting creation.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/media/shared/{token_value}"),
            None,
            None,
        ),
    )
    .await;
    // `403 file_unavailable`, not `410`: the share module maps an unavailable file to
    // FORBIDDEN on purpose, because a `410` tells the holder to ask the owner for a new link
    // — worse advice than "this file is held", and a pointless request for the owner.
    assert_eq!(
        after.status,
        StatusCode::FORBIDDEN,
        "a link made yesterday must not keep serving a held file: {}",
        after.body
    );
    assert_eq!(after.body["error"]["code"], json!("file_unavailable"));
    // And the refusal does not carry the scanner's finding to whoever holds the link.
    let raw = String::from_utf8_lossy(&after.raw);
    assert!(
        !raw.contains("Eicar"),
        "the holder of a link learns nothing about the finding: {raw}"
    );

    fixture.cleanup().await;
}

/// A trashed file is not claimed by a sweep.
#[tokio::test]
async fn a_trashed_file_is_not_claimed_by_a_sweep() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let scanner = StubScanner::start(ScannerAnswer::Clean).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;

    let media = upload(&fixture.state, &token, site, "gone.txt", b"on its way out").await;
    call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/files/{media}"),
            Some(&token),
            None,
        ),
    )
    .await;

    let sweep = call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(sweep.status, StatusCode::OK, "body: {}", sweep.body);
    assert_eq!(
        sweep.body["scanned"],
        json!(0),
        "a file on its way out is not scanned"
    );
    assert_eq!(scanner.requests(), 0);

    // It stays `pending` and untouched: a sweep that marked a trashed file would write to a
    // row the retention worker is about to delete.
    let status: (String,) = sqlx::query_as("select scan_status from media where id = $1")
        .bind(media)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the row must read");
    assert_eq!(status.0, "pending");

    fixture.cleanup().await;
}

/// A release is scoped to its own site: another tenant's quarantine id is a `404`.
#[tokio::test]
async fn a_quarantine_of_another_site_is_not_found() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    let scanner = StubScanner::start(ScannerAnswer::Flagged).await;
    fixture
        .enable_scanning(site, &token, &scanner.endpoint())
        .await;
    let media = upload(&fixture.state, &token, site, "held.txt", b"bytes").await;
    call(
        &fixture.state,
        request(Method::POST, &scan_run_uri(site), Some(&token), None),
    )
    .await;
    let quarantine_id: (Uuid,) = sqlx::query_as(
        "select id from media_quarantines where media_id = $1 and released_at is null",
    )
    .bind(media)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the quarantine must exist");

    // A second organization, and an account inside it.
    let other_org = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Other Org")
    .bind(format!("other-{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the organization must be created");
    let (other_id, other_email) = create_account(&fixture.db, Some(other_org)).await;
    let other_role = role_store::create_role(
        fixture.db.pool(),
        NewRole {
            organization_id: other_org,
            key: format!("other-{}", Uuid::new_v4().simple()),
            name: "Other Editor".to_owned(),
            description: "Drives the other organization".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = EDITOR_PERMISSIONS
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(fixture.db.pool(), other_role.id, &entries)
        .await
        .expect("the permissions must be written");
    let (platform_id,) = sqlx::query_as::<_, (Uuid,)>("select id from users limit 1")
        .fetch_one(fixture.db.pool())
        .await
        .unwrap_or((Uuid::nil(),));
    bindings::grant(
        fixture.db.pool(),
        NewBinding {
            role_id: other_role.id,
            user_id: other_id,
            scope: Scope::Organization {
                organization_id: other_org,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
    let other_token = login(&fixture.state, &other_email).await;

    // A `403` here would confirm the id exists; a `404` does not, so guessing one is no use.
    let response = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/quarantine/{}/release", quarantine_id.0),
            Some(&other_token),
            Some(json!({ "reason": "mine now" })),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "another tenant's quarantine is not found, not forbidden: {}",
        response.body
    );

    // The row is untouched.
    let still_open: i64 = sqlx::query_scalar(
        "select count(*) from media_quarantines where id = $1 and released_at is null",
    )
    .bind(quarantine_id.0)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(still_open, 1);

    sqlx::query("delete from users where id = $1")
        .bind(other_id)
        .execute(fixture.db.pool())
        .await
        .expect("cleanup must run");
    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("cleanup must run");
    fixture.cleanup().await;
}

/// A time alias, so the walk can read a nullable `timestamptz` without importing `time`.
type OffsetDateTimeAlias = time::OffsetDateTime;
