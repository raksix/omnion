//! Integration tests for the media surface: upload, list, read and remove, the two read paths
//! (panel and public), what a browser may render inline, and the permission/scope rules around
//! all of it (docs/01-VISION.md §5, docs/requests/REQ-010, phase P08).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`), which ships the object
//! store the library writes into — locally and in CI. When PostgreSQL or the object store is not
//! reachable the suite skips itself with a printed reason, so `cargo test` stays usable on a
//! machine without Docker.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_media::MAX_UPLOAD_BYTES;
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_storage::{Storage, StorageError};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the media editor of this suite holds.
///
/// `media.manage` is the folder-and-storage power the file manager routes require, and
/// `media.update` is the rename/move key, so the editor can drive the whole screen.
const MEDIA_PERMISSIONS: [&str; 5] = [
    "media.read",
    "media.upload",
    "media.delete",
    "media.update",
    "media.manage",
];

/// Boundary of the multipart bodies this suite sends.
const BOUNDARY: &str = "omnion-media-test-boundary";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    nosniff: bool,
    body: Value,
    bytes: Vec<u8>,
}

/// Drive the real router without a network socket.
async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");

    let status = response.status();
    let header_text = |name: axum::http::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let set_cookie = header_text(header::SET_COOKIE);
    let content_type = header_text(header::CONTENT_TYPE);
    let content_disposition = header_text(header::CONTENT_DISPOSITION);
    let nosniff = header_text(header::X_CONTENT_TYPE_OPTIONS).as_deref() == Some("nosniff");

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes()
        .to_vec();
    let body = match content_type.as_deref() {
        Some(value) if value.starts_with("application/json") && !bytes.is_empty() => {
            serde_json::from_slice(&bytes).expect("a JSON body must parse")
        }
        _ => Value::Null,
    };

    TestResponse {
        status,
        set_cookie,
        content_type,
        content_disposition,
        nosniff,
        body,
        bytes,
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

/// Build a `multipart/form-data` body carrying one `file` part.
fn multipart_body(boundary: &str, filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Build an upload request: the multipart body plus, when given, the session cookie.
fn upload_request(
    uri: &str,
    token: Option<&str>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Request<Body> {
    let builder = Request::builder().method(Method::POST).uri(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, format!("omnion_session={token}")),
        None => builder,
    };

    builder
        .body(Body::from(multipart_body(
            BOUNDARY,
            filename,
            content_type,
            bytes,
        )))
        .expect("request must build")
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
///
/// The pool is clamped to two connections. `Db::connect` honours `OMNION_DB_MAX_CONNECTIONS`
/// (default 10) and this suite runs its tests in parallel, so eight fixtures at ten
/// connections each is eighty of the container's hundred slots: the suite starves *itself*
/// and reports `PoolTimedOut` out of `seed::ensure`, which reads as a broken IAM seed and is
/// really a test that asked for more connections than the database has. Two per fixture is
/// plenty for these in-process walks and leaves the headroom the other suites need.
async fn live_db(config: &Config) -> Option<Db> {
    let mut config = config.clone();
    config.database.max_connections = 2;
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// Open the object store the library writes into; `None` means it is not running.
///
/// A missing bucket is not a problem — the library creates it on the first write — so only a
/// store that cannot be reached at all skips the suite.
async fn live_storage() -> Option<Storage> {
    let storage = Storage::from_env().expect("the storage configuration must be valid");
    match storage.probe().await {
        Ok(()) | Err(StorageError::NotFound { .. }) => Some(storage),
        Err(err) => {
            eprintln!(
                "SKIP: the object store is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied, the IAM seed loaded and the object store
/// open — everything the media surface needs.
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

/// Two organizations with one site each, a platform Owner, a media editor of the first
/// organization and a plain member without any media permission.
///
/// Every row carries a `media-` prefix or a random address, and cleanup removes exactly the rows
/// this fixture created — by id, never by pattern, so parallel suites cannot collide.
struct Fixture {
    state: AppState,
    db: Db,
    storage: Storage,
    platform_email: String,
    site_a: Uuid,
    site_b: Uuid,
    editor_email: String,
    member_email: String,
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

        let org_a = create_organization_row(&db, "a", "Media Test A").await;
        let org_b = create_organization_row(&db, "b", "Media Test B").await;
        let site_a = create_site_row(&db, org_a, "main", "Media Site A").await;
        let site_b = create_site_row(&db, org_b, "main", "Media Site B").await;

        // The platform Owner: no primary organization, so it may work across tenants.
        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The media editor: the library keys bound at organization scope.
        let (editor_id, editor_email) = create_account(&db, Some(org_a)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org_a,
                key: format!("media-editor-{}", Uuid::new_v4().simple()),
                name: "Media Editor".to_owned(),
                description: "Keeps the media library of one organization".to_owned(),
                priority: 400,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the organization role must be created");

        let entries: Vec<RolePermissionInput> = MEDIA_PERMISSIONS
            .iter()
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect();
        role_store::set_role_permissions(db.pool(), role.id, &entries)
            .await
            .expect("the role permission set must be written");

        let binding = NewBinding {
            role_id: role.id,
            user_id: editor_id,
            scope: Scope::Organization {
                organization_id: org_a,
            },
            granted_by: Some(platform_id),
            expires_at: None,
        };
        bindings::validate(db.pool(), &binding)
            .await
            .expect("the binding must validate");
        bindings::grant(db.pool(), binding)
            .await
            .expect("the binding must be granted");

        // A plain member of the same organization, without any media permission.
        let (member_id, member_email) = create_account(&db, Some(org_a)).await;

        Some(Self {
            state,
            db,
            storage,
            platform_email,
            site_a,
            site_b,
            editor_email,
            member_email,
            accounts: vec![platform_id, editor_id, member_id],
            organizations: vec![org_a, org_b],
            sites: vec![site_a, site_b],
        })
    }

    /// The platform Owner, signed in.
    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// The media editor of the first organization, signed in.
    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    /// The plain member of the first organization, signed in.
    async fn member_token(&self) -> String {
        login(&self.state, &self.member_email).await
    }

    /// Remove what this fixture created — objects first, then rows.
    async fn cleanup(&self) {
        let keys: Vec<String> =
            sqlx::query_scalar("select storage_key from media where site_id = any($1)")
                .bind(&self.sites)
                .fetch_all(self.db.pool())
                .await
                .expect("the fixture keys must read");
        for key in keys {
            let _ = self.storage.delete(&key).await;
        }

        sqlx::query("delete from media where site_id = any($1)")
            .bind(&self.sites)
            .execute(self.db.pool())
            .await
            .expect("media cleanup must run");
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

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("media-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create a site row inside an organization.
async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("the test site must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("media-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Media Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Sign an account in and return the raw session token.
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

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

/// The `id` fields of an array inside `pointer`.
fn ids_of(body: &Value, pointer: &str) -> Vec<String> {
    body[pointer]
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} must be an array in {body}"))
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

/// How many audit rows carry one action for one target.
async fn audit_rows(db: &Db, action: &str, target_id: &str) -> i64 {
    sqlx::query_scalar("select count(*) from audit_log where action = $1 and target_id = $2")
        .bind(action)
        .bind(target_id)
        .fetch_one(db.pool())
        .await
        .expect("audit rows must read")
}

#[tokio::test]
async fn the_media_surface_is_permission_gated() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let library = format!("/api/v1/media?site_id={site}");
    let member = fixture.member_token().await;
    let editor = fixture.editor_token().await;

    // Without a session the library, the bytes and the upload are all closed.
    for (method, uri) in [
        (Method::GET, library.clone()),
        (Method::DELETE, format!("/api/v1/media/{}", Uuid::new_v4())),
    ] {
        let response = call(&fixture.state, request(method.clone(), &uri, None, None)).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(response.body["error"]["code"], "unauthenticated");
    }
    let anonymous_upload = call(
        &fixture.state,
        upload_request(&library, None, "one.txt", "text/plain", b"one"),
    )
    .await;
    assert_eq!(anonymous_upload.status, StatusCode::UNAUTHORIZED);

    // A signed-in account without a media permission sees none of it.
    let listing = call(
        &fixture.state,
        request(Method::GET, &library, Some(&member), None),
    )
    .await;
    assert_eq!(listing.status, StatusCode::FORBIDDEN);
    assert_eq!(listing.body["error"]["code"], "permission_denied");

    let denied_upload = call(
        &fixture.state,
        upload_request(&library, Some(&member), "one.txt", "text/plain", b"one"),
    )
    .await;
    assert_eq!(denied_upload.status, StatusCode::FORBIDDEN);

    // The editor holds the library keys.
    let listing = call(
        &fixture.state,
        request(Method::GET, &library, Some(&editor), None),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK, "body: {}", listing.body);
    assert_eq!(
        listing.body["site_id"].as_str(),
        Some(site.to_string().as_str())
    );
    assert_eq!(listing.body["media"].as_array().map(Vec::len), Some(0));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_file_round_trips_from_upload_to_fetch() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");
    let bytes = b"\x89PNG\r\n\x1a\nomnion media round trip";

    let upload = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "Logo (1).PNG", "image/png", bytes),
    )
    .await;
    assert_eq!(upload.status, StatusCode::CREATED, "body: {}", upload.body);
    let media_id = id_of(&upload.body);
    assert_eq!(
        upload.body["filename"], "Logo-1-.PNG",
        "the file name is reduced before it is stored"
    );
    assert_eq!(upload.body["content_type"], "image/png");
    assert_eq!(upload.body["size_bytes"].as_u64(), Some(bytes.len() as u64));
    assert_eq!(
        upload.body["checksum"].as_str(),
        Some(omnion_storage::signing::payload_hash(bytes).as_str())
    );

    // The library lists it.
    let listing = call(
        &fixture.state,
        request(Method::GET, &library, Some(&editor), None),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK);
    assert!(
        ids_of(&listing.body, "media").contains(&media_id),
        "the upload must be listed: {}",
        listing.body
    );

    // The panel read path answers the very bytes back, with the stored type.
    let raw = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.content_type.as_deref(), Some("image/png"));
    assert_eq!(
        raw.content_disposition.as_deref(),
        Some("inline; filename=\"Logo-1-.PNG\"")
    );
    assert!(raw.nosniff, "the bytes leave with nosniff");
    assert_eq!(raw.bytes, bytes);

    // The public read path serves the same bytes to a visitor without a session.
    let public = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/media/{media_id}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK);
    assert_eq!(public.bytes, bytes);
    assert_eq!(public.content_type.as_deref(), Some("image/png"));

    // The upload is audited.
    assert_eq!(
        audit_rows(&fixture.db, "media.uploaded", &media_id).await,
        1
    );

    // A second upload of the same name is a second file, not a clash: keys are ids.
    let second = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "Logo (1).PNG", "image/png", bytes),
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED, "body: {}", second.body);
    assert_ne!(id_of(&second.body), media_id);

    // Removing a file takes its row and its object with it.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/{media_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.body["error"]["code"], "media_not_found");

    let public_gone = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/media/{media_id}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(public_gone.status, StatusCode::NOT_FOUND);

    assert_eq!(audit_rows(&fixture.db, "media.deleted", &media_id).await, 1);

    // The object itself is gone from the bucket as well.
    let key = sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
        .bind(Uuid::parse_str(&media_id).expect("the id is a uuid"))
        .fetch_optional(fixture.db.pool())
        .await
        .expect("the row lookup must run");
    assert!(key.is_none(), "the row must be gone");

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_upload_into_another_tenant_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let editor = fixture.editor_token().await;

    let upload = call(
        &fixture.state,
        upload_request(
            &format!("/api/v1/media?site_id={}", fixture.site_b),
            Some(&editor),
            "one.txt",
            "text/plain",
            b"one",
        ),
    )
    .await;
    assert_eq!(
        upload.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        upload.body
    );
    assert_eq!(upload.body["error"]["code"], "cross_organization");

    let listing = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media?site_id={}", fixture.site_b),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(listing.status, StatusCode::FORBIDDEN);

    // The platform Owner, on the other hand, reaches the tenant.
    let owner = fixture.platform_token().await;
    let listing = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media?site_id={}", fixture.site_b),
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK, "body: {}", listing.body);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_type_a_browser_cannot_render_leaves_as_a_download() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");
    let bytes = b"<html><script>alert(1)</script></html>";

    let upload = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "page.html", "text/html", bytes),
    )
    .await;
    assert_eq!(upload.status, StatusCode::CREATED, "body: {}", upload.body);
    let media_id = id_of(&upload.body);
    assert_eq!(upload.body["content_type"], "text/html");

    let raw = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(
        raw.content_type.as_deref(),
        Some("application/octet-stream"),
        "markup is never served as markup from the platform's own origin"
    );
    assert_eq!(
        raw.content_disposition.as_deref(),
        Some("attachment; filename=\"page.html\"")
    );
    assert!(raw.nosniff);
    assert_eq!(raw.bytes, bytes, "the bytes themselves are untouched");

    fixture.cleanup().await;
}

#[tokio::test]
async fn uploads_without_bytes_or_over_the_limit_are_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    let empty = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "empty.txt", "text/plain", b""),
    )
    .await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        empty.body
    );
    assert_eq!(empty.body["error"]["code"], "invalid_request");

    let oversized = vec![b'a'; MAX_UPLOAD_BYTES as usize + 1];
    let too_big = call(
        &fixture.state,
        upload_request(
            &library,
            Some(&editor),
            "big.bin",
            "application/octet-stream",
            &oversized,
        ),
    )
    .await;
    assert_eq!(
        too_big.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "body: {}",
        too_big.body
    );
    assert_eq!(too_big.body["error"]["code"], "payload_too_large");

    // Nothing of either attempt reached the library.
    let listing = call(
        &fixture.state,
        request(Method::GET, &library, Some(&editor), None),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK);
    assert_eq!(listing.body["media"].as_array().map(Vec::len), Some(0));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_request_without_a_file_part_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;

    // A multipart body that carries no `file` part at all.
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nno file here\r\n--{BOUNDARY}--\r\n"
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/media?site_id={site}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::COOKIE, format!("omnion_session={editor}"))
        .body(Body::from(body))
        .expect("request must build");

    let response = call(&fixture.state, request).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        response.body
    );
    assert_eq!(response.body["error"]["code"], "missing_file");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The file manager (REQ-010, slice 1)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_site_created_after_the_migration_still_gets_a_library_root() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let tree = format!("/api/v1/media/folders?site_id={site}");

    // The fixture's site was inserted directly, so it is exactly the case the migration's
    // backfill cannot cover: a site that did not exist when the migration ran. A browser needs a
    // stable root id, so the tree has to materialise one on first read.
    let before: i64 = sqlx::query_scalar("select count(*) from media_folders where site_id = $1")
        .bind(site)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the folder count must read");
    assert_eq!(before, 0, "the fixture site starts with no folder at all");

    let response = call(
        &fixture.state,
        request(Method::GET, &tree, Some(&editor), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert_eq!(response.body["root"]["is_root"], json!(true));
    assert_eq!(response.body["root"]["name"], "Media");
    assert_eq!(response.body["root"]["depth"], 0);
    assert_eq!(
        response.body["folders"].as_array().map(Vec::len),
        Some(1),
        "the root is the only folder"
    );

    // Reading it again is the same answer, not a second root.
    let again = call(
        &fixture.state,
        request(Method::GET, &tree, Some(&editor), None),
    )
    .await;
    assert_eq!(again.body["root"]["id"], response.body["root"]["id"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from media_folders where site_id = $1")
            .bind(site)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the folder count must read"),
        1,
        "one root per site, however many times it is read"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_file_manager_walks_folders_files_and_the_trash() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let member = fixture.member_token().await;
    let tree = format!("/api/v1/media/folders?site_id={site}");

    // The tree answers without a session and refuses an account with no media permission.
    assert_eq!(
        call(&fixture.state, request(Method::GET, &tree, None, None))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &fixture.state,
            request(Method::GET, &tree, Some(&member), None)
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    let root = call(
        &fixture.state,
        request(Method::GET, &tree, Some(&editor), None),
    )
    .await
    .body["root"]["id"]
        .as_str()
        .expect("the root has an id")
        .to_owned();

    // Create two folders, one inside the other, and read the paths back.
    let campaigns = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "Campaigns" })),
        ),
    )
    .await;
    assert_eq!(campaigns.status, StatusCode::CREATED, "body: {}", campaigns.body);
    assert_eq!(campaigns.body["path"], "Media/Campaigns");
    let campaigns_id = id_of(&campaigns.body);

    let year = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "2026", "parent_id": campaigns_id })),
        ),
    )
    .await;
    assert_eq!(year.status, StatusCode::CREATED, "body: {}", year.body);
    assert_eq!(year.body["path"], "Media/Campaigns/2026");
    let year_id = id_of(&year.body);

    // A duplicate sibling name is refused by name, not by a bare constraint error.
    let duplicate = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "Campaigns" })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "folder_name_taken");

    // A blank name names the field it refused.
    let blank = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "   " })),
        ),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    assert_eq!(blank.body["error"]["code"], "invalid_folder_name");
    assert_eq!(blank.body["error"]["details"]["field"], "name");

    // A folder holding a subfolder is refused, and the answer says what is in the way.
    let not_empty = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/folders/{campaigns_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(not_empty.status, StatusCode::CONFLICT);
    assert_eq!(not_empty.body["error"]["code"], "folder_not_empty");

    // The library root is structural: it is neither renamed nor deleted.
    let root_delete = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/folders/{root}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(root_delete.status, StatusCode::CONFLICT);
    assert_eq!(root_delete.body["error"]["code"], "root_folder_protected");

    // Moving a folder rewrites the paths of the WHOLE subtree, and refuses a cycle. The `2026`
    // folder is moved up to the root and then back under `Campaigns`, so the walk proves both a
    // rewrite of a nested path and a move that has no ancestor to be forbidden against.
    let moved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/folders/{year_id}"),
            Some(&editor),
            Some(json!({ "name": "2026 Launch" })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "body: {}", moved.body);
    assert_eq!(moved.body["path"], "Media/Campaigns/2026 Launch");
    assert_eq!(
        sqlx::query_scalar::<_, String>("select path from media_folders where id = $1")
            .bind(Uuid::parse_str(&year_id).expect("the folder id is a uuid"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the folder path must read"),
        "Media/Campaigns/2026 Launch"
    );

    // A move has to actually change the tree, so `Campaigns` (which already sits at the root) is
    // moved INTO a new top-level folder. That is the case where a subtree rewrite is visible: the
    // child has to follow its parent, or the tree is a lie about where the files are.
    let archive = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "Archive" })),
        ),
    )
    .await;
    assert_eq!(archive.status, StatusCode::CREATED, "body: {}", archive.body);

    let reparented = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/folders/{campaigns_id}"),
            Some(&editor),
            Some(json!({ "parent_id": id_of(&archive.body) })),
        ),
    )
    .await;
    assert_eq!(reparented.status, StatusCode::OK, "body: {}", reparented.body);
    assert_eq!(reparented.body["path"], "Media/Archive/Campaigns");
    // The child followed its parent, which is the invariant the subtree rewrite exists to keep.
    assert_eq!(
        sqlx::query_scalar::<_, String>("select path from media_folders where id = $1")
            .bind(Uuid::parse_str(&year_id).expect("the folder id is a uuid"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the folder path must read"),
        "Media/Archive/Campaigns/2026 Launch",
        "a folder move rewrites the path of everything under it"
    );
    // And the parent link really points at the new parent, not at the moved row itself.
    assert_eq!(
        reparented.body["parent_id"],
        json!(id_of(&archive.body)),
        "the moved folder's parent is the folder it was moved into"
    );

    // A folder cannot be moved inside itself: the API refuses the cycle instead of writing a
    // path that is its own prefix.
    let cycle = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/folders/{campaigns_id}"),
            Some(&editor),
            Some(json!({ "parent_id": year_id })),
        ),
    )
    .await;
    assert_eq!(cycle.status, StatusCode::CONFLICT, "body: {}", cycle.body);
    assert_eq!(cycle.body["error"]["code"], "folder_cycle");

    // Moving a folder to where it already is a no-op, not a "cycle": the walk proved the real
    // cycle above, so this would otherwise be the only answer the route gives for a move that has
    // nothing to do.
    let noop = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/folders/{campaigns_id}"),
            Some(&editor),
            Some(json!({ "name": "Campaigns", "parent_id": id_of(&archive.body) })),
        ),
    )
    .await;
    assert_eq!(noop.status, StatusCode::OK, "body: {}", noop.body);
    assert_eq!(noop.body["path"], "Media/Archive/Campaigns");

    // The breadcrumb of a deep folder names the chain the walk built, root first.
    let deep_folder = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}&folder_id={year_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(deep_folder.status, StatusCode::OK, "body: {}", deep_folder.body);
    let crumbs: Vec<String> = deep_folder.body["breadcrumb"]
        .as_array()
        .expect("the breadcrumb is an array")
        .iter()
        .map(|crumb| crumb["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(crumbs, vec!["Media", "Archive", "Campaigns", "2026 Launch"], "{crumbs:?}");

    // The confirmation text on the screen ("a folder that still holds files or subfolders is
    // refused") and the API have to agree on BOTH sides of the rule: the refusal was proved above
    // with `Campaigns`, and the acceptance is proved here with a folder that really is empty. A
    // scratch folder is used rather than `Campaigns` itself, because the rest of the walk moves a
    // file into it.
    let scratch = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/folders",
            Some(&editor),
            Some(json!({ "site_id": site, "name": "Scratch" })),
        ),
    )
    .await;
    assert_eq!(scratch.status, StatusCode::CREATED, "body: {}", scratch.body);
    let delete_empty = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/folders/{}", id_of(&scratch.body)),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        delete_empty.status,
        StatusCode::NO_CONTENT,
        "body: {}",
        delete_empty.body
    );
    assert!(
        call(
            &fixture.state,
            request(
                Method::GET,
                &format!("/api/v1/media/files?site_id={site}&folder_id={}", id_of(&scratch.body)),
                Some(&editor),
                None,
            )
        )
        .await
        .status
            == StatusCode::NOT_FOUND,
        "a deleted folder is a 404 by id, so a stale deep link names what is missing"
    );

    // Two uploads: one at the root, one in the deep folder.
    let library = format!("/api/v1/media?site_id={site}");
    let first = call(
        &fixture.state,
        upload_request(
            &library,
            Some(&editor),
            "logo.png",
            "image/png",
            b"\x89PNG\r\n\x1a\nfile manager walk",
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "body: {}", first.body);
    let first_id = id_of(&first.body);

    let deep = call(
        &fixture.state,
        upload_request(
            &library,
            Some(&editor),
            "brief.txt",
            "text/plain",
            b"the deep folder's file",
        ),
    )
    .await;
    assert_eq!(deep.status, StatusCode::CREATED, "body: {}", deep.body);
    let deep_id = id_of(&deep.body);

    // Moving a file changes `folder_id` and nothing else - the storage key is immutable.
    //
    // `id_of` answers a `String` (it reads the JSON body), so the row is addressed by a parsed
    // `Uuid` bound BY VALUE, and `query_scalar::<_, String>` names the output type. Bind a `&Uuid`
    // (or omit the type annotation) and the parameter is inferred from the wrong place: the first
    // reads as TEXT and fails `uuid = text`, the second as a text parameter holding a binary uuid
    // and fails "incorrect binary data format". Both look like schema bugs; both are bind bugs.
    let deep_uuid = Uuid::parse_str(&deep_id).expect("the created file id must be a uuid");
    let key_before: String = sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
        .bind(deep_uuid)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the key must read");
    let moved = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/files/{deep_id}"),
            Some(&editor),
            Some(json!({ "folder_id": campaigns_id })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "body: {}", moved.body);
    assert_eq!(moved.body["folder_id"], json!(campaigns_id));
    assert_eq!(
        sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
            .bind(deep_uuid)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the key must read"),
        key_before,
        "a move never rewrites the storage key"
    );

    // The listing of a folder holds exactly that folder's files, and its total matches its rows.
    let listing = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}&folder_id={campaigns_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(listing.status, StatusCode::OK, "body: {}", listing.body);
    assert_eq!(listing.body["total"], json!(1));
    assert_eq!(listing.body["files"].as_array().map(Vec::len), Some(1));
    assert_eq!(ids_of(&listing.body, "files"), vec![deep_id.clone()]);
    // The breadcrumb names the chain as the walk left it, root first.
    assert_eq!(listing.body["breadcrumb"][1]["name"], "Archive");
    assert_eq!(listing.body["breadcrumb"][2]["name"], "Campaigns");

    // A filter narrows the listing, and the reported total follows it.
    let filtered = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}&folder_id={campaigns_id}&kind=image"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(filtered.body["total"], json!(0));
    assert_eq!(filtered.body["files"].as_array().map(Vec::len), Some(0));

    // A search term that contains a `like` wildcard is treated as the text that was typed.
    let wildcard = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}&search=%25"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        wildcard.body["total"],
        json!(0),
        "a literal % must not match every file"
    );

    // A folder of another site is not reachable through this site's scope. The id belongs to no
    // folder at all, which is what a stale deep link looks like from the browser.
    let stranger = Uuid::new_v4();
    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}&folder_id={stranger}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(foreign.body["error"]["code"], "folder_not_found");

    // A move into one's own subtree is refused before anything is written.
    let cycle = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/folders/{campaigns_id}"),
            Some(&editor),
            Some(json!({ "name": "Campaigns", "parent_id": year_id })),
        ),
    )
    .await;
    assert_eq!(cycle.status, StatusCode::CONFLICT);
    assert_eq!(cycle.body["error"]["code"], "folder_cycle");

    // A delete is a trash: the file leaves the listing, keeps its bytes, and comes back.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/files/{first_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "body: {}", deleted.body);

    let after_delete = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert!(
        !ids_of(&after_delete.body, "files").contains(&first_id),
        "a trashed file is out of the listing"
    );
    let key_after_delete: String =
        sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
            .bind(Uuid::parse_str(&first_id).expect("the uploaded id is a uuid"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the key must read");
    assert!(
        fixture
            .storage
            .get(&key_after_delete)
            .await
            .is_ok(),
        "a delete keeps the bytes: only a purge removes them"
    );

    // The trash lists it with a countdown, and a live file is never in the trash.
    let trash = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/trash?site_id={site}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(trash.status, StatusCode::OK, "body: {}", trash.body);
    assert_eq!(trash.body["file_count"], json!(1));
    assert_eq!(trash.body["retention_days"], json!(30));
    let entries = trash.body["entries"].as_array().expect("entries is an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], json!(first_id));
    assert!(entries[0]["purges_at"].as_str().is_some(), "the countdown is real");

    // Restoring puts the file back where it was.
    let restored = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/files/{first_id}/restore"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(restored.status, StatusCode::OK, "body: {}", restored.body);
    let after_restore = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files?site_id={site}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert!(ids_of(&after_restore.body, "files").contains(&first_id));

    // A bulk selection mixes the real files with a stale id: the good ones still change, and the
    // failure is reported per file rather than failing the whole call.
    let bulk = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/bulk",
            Some(&editor),
            Some(json!({
                "site_id": site,
                "action": "tag",
                "ids": [first_id, deep_id, Uuid::new_v4()],
                "tags": ["Summer", "summer", "  "],
            })),
        ),
    )
    .await;
    assert_eq!(bulk.status, StatusCode::OK, "body: {}", bulk.body);
    assert_eq!(bulk.body["requested"], json!(3));
    assert_eq!(bulk.body["changed"], json!(2));
    assert_eq!(bulk.body["failures"].as_array().map(Vec::len), Some(1));

    let tags: Value = sqlx::query_scalar("select to_jsonb(tags) from media where id = $1")
        .bind(Uuid::parse_str(&first_id).expect("the uploaded id is a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the tags must read");
    assert_eq!(tags, json!(["summer"]), "tags are trimmed, lower-cased and unique");

    // An unknown action is refused by name.
    let unknown = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/bulk",
            Some(&editor),
            Some(json!({ "site_id": site, "action": "incinerate", "ids": [first_id] })),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.body["error"]["code"], "unknown_bulk_action");

    // A purge removes the row *and* the bytes, and refuses a file that is not in the trash.
    let live_purge = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/files/{first_id}/purge"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(live_purge.status, StatusCode::BAD_REQUEST);
    assert_eq!(live_purge.body["error"]["code"], "file_not_trashed");

    call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/files/{first_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    let purged = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/files/{first_id}/purge"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(purged.status, StatusCode::NO_CONTENT, "body: {}", purged.body);
    assert!(
        fixture.storage.get(&key_after_delete).await.is_err(),
        "a purge removes the bytes as well as the row"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from media where id = $1")
            .bind(Uuid::parse_str(&first_id).expect("the uploaded id is a uuid"))
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row count must read"),
        0
    );

    // Every privileged step of the walk left an audit row.
    for action in [
        "media.folder_created",
        "media.folder_moved",
        "media.deleted",
        "media.restored",
        "media.purged",
        "media.bulk_action",
    ] {
        let rows: i64 = sqlx::query_scalar(
            "select count(*) from audit_log where action = $1 and metadata->>'site_id' = $2",
        )
        .bind(action)
        .bind(site.to_string())
        .fetch_one(fixture.db.pool())
        .await
        .expect("the audit count must read");
        assert!(rows > 0, "{action} must be audited for this site");
    }

    fixture.cleanup().await;
}
