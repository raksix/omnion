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
const MEDIA_PERMISSIONS: [&str; 3] = ["media.read", "media.upload", "media.delete"];

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
async fn live_db(config: &Config) -> Option<Db> {
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
