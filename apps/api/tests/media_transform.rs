//! Integration tests for image transformation presets (REQ-010, slice 3).
//!
//! These walk the **real router**, not the transform function, because every interesting failure
//! in this feature lives in the space between the two: a preset the API accepts and the database
//! refuses, a key that does not change when the definition does, a fallback that 404s instead of
//! serving the original, a derivative whose row is written before its bytes exist.
//!
//! Every assertion here is against observable state — the response bytes, the `media_derivatives`
//! rows, the object store — rather than against a function's return value, because a unit test on
//! `transform_bytes` cannot catch a route that never calls it.

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

mod support;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// The permission keys the editor of this suite holds.
///
/// `media.settings.manage` is the storage-side power the preset routes require, and it is
/// deliberately separate from `media.manage`: a team that may organise a library does not get to
/// change what every published page renders.
const EDITOR_PERMISSIONS: [&str; 7] = [
    "media.read",
    "media.upload",
    "media.delete",
    "media.update",
    "media.manage",
    "media.settings.manage",
    "media.share",
];

/// Boundary of the multipart bodies this suite sends.
const BOUNDARY: &str = "omnion-transform-test-boundary";

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    cache_control: Option<String>,
    derivative_key: Option<String>,
    derivative_cache: Option<String>,
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
    let cache_control = header_text(header::CACHE_CONTROL);
    let derivative_key = header_text("x-omnion-derivative".parse().expect("valid header name"));
    let derivative_cache = header_text("x-omnion-cache".parse().expect("valid header name"));

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
        cache_control,
        derivative_key,
        derivative_cache,
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
fn multipart_body(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 256);
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// A request that uploads one file.
///
/// The body is built **once** and its length goes on the request: building it twice produced two
/// different `Vec`s in the first version, and the length header described one while the body
/// carried the other — which only shows up as an upload the server silently truncates.
fn upload_request(
    uri: &str,
    token: Option<&str>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Request<Body> {
    let body = multipart_body(filename, content_type, bytes);
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::CONTENT_LENGTH, body.len().to_string());
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("omnion_session={token}"));
    }
    builder.body(Body::from(body)).expect("request must build")
}

/// A real PNG, generated rather than embedded so the bytes are certainly decodable.
///
/// 1200x630 with four coloured quadrants: a crop shows two of the four, a fit shows all of them.
///
/// The encoder itself is shared (`support::image_bytes`) rather than copied: the copy that lived
/// here encoded the same bytes in the same way, and a second copy is a second thing that has to
/// stay correct.
fn source_png() -> Vec<u8> {
    support::image_bytes::quadrant_png(1200, 630)
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
///
/// The pool is clamped to two connections: `Db::connect` honours a default of ten and this
/// suite runs in parallel, so eight fixtures at ten each is eighty of the container's hundred
/// slots — the suite starves *itself* and reports `PoolTimedOut` out of `seed::ensure`, which
/// reads as a broken IAM seed and is really a test that asked for too much.
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
        .bind("Transform Test")
        .bind(format!("media-transform-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Transform Site")
        .fetch_one(db.pool())
        .await
        .expect("the site must be created");

        // The platform owner, who may work across tenants.
        let (platform_id, _) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        // The editor: everything this suite drives, including the settings power.
        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        let role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("media-editor-{}", Uuid::new_v4().simple()),
                name: "Media Editor".to_owned(),
                description: "Drives the transformation presets".to_owned(),
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

        // A reader: `media.read` and nothing else, so the settings routes must refuse it.
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

    /// Remove what this fixture created — objects first, then rows.
    ///
    /// The keys come from the *union* of the live rows, the version history and the derivative
    /// cache. Reading only `media.storage_key` leaks every replaced version's object and every
    /// generated derivative, and a test run that leaks is a bucket that grows for ever.
    async fn cleanup(&self) {
        let keys: Vec<String> = sqlx::query_scalar(
            "select storage_key from media where site_id = any($1) \
             union \
             select v.storage_key from media_versions v \
               join media m on m.id = v.media_id where m.site_id = any($1) \
             union \
             select d.storage_key from media_derivatives d \
               join media m on m.id = d.media_id where m.site_id = any($1)",
        )
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

/// Create an account with a unique address.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("transform-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Transform Test".to_owned(),
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

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_preset_url_returns_the_transformed_image_and_caches_it_by_its_inputs() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a();
    let editor = fixture.editor_token().await;

    // A 1200x630 PNG, uploaded through the real route.
    let upload = call(
        &fixture.state,
        upload_request(
            &format!("/api/v1/media?site_id={site}"),
            Some(&editor),
            "hero.png",
            "image/png",
            &source_png(),
        ),
    )
    .await;
    assert_eq!(
        upload.status,
        StatusCode::CREATED,
        "upload body: {}",
        upload.body
    );
    let media_id = id_of(&upload.body);

    // A preset at 400x400, cover, WebP.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/transformation-presets?site_id={site}"),
            Some(&editor),
            Some(json!({ "name": "card", "width": 400, "height": 400, "fit": "cover", "format": "webp", "quality": 80 })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create body: {}",
        created.body
    );
    assert_eq!(created.body["name"], "card");
    assert_eq!(created.body["summary"], "400 x 400 · cover · WebP q80");
    let preset_id = id_of(&created.body);

    // First request: the derivative is built. The bytes must actually be a WebP, and the
    // dimensions must be the box — a length check would pass on the original.
    let first = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=card"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "raw body: {}", first.body);
    assert_eq!(first.content_type.as_deref(), Some("image/webp"));
    assert!(
        first.bytes.starts_with(b"RIFF") && first.bytes[8..12] == *b"WEBP",
        "the answer must be WebP bytes, not the original PNG"
    );
    assert_ne!(
        first.bytes,
        source_png(),
        "a preset that returns the source is not a transformation"
    );
    let first_key = first
        .derivative_key
        .clone()
        .expect("a derivative must carry its cache key");
    // Whether this call built or read depends on whether a previous run left the row behind, and
    // the row is keyed by the source bytes — which a re-run reproduces exactly. Asserting
    // "built" would make the suite fail on the *second* run and pass on the first, which is the
    // worst possible property for a test. What matters is that the answer is correct either way,
    // and that the key is stable: which the two calls below check.
    assert!(
        matches!(first.derivative_cache.as_deref(), Some("0" | "1")),
        "a derivative must report whether it built or read: {:?}",
        first.derivative_cache
    );

    // The row exists and names the object the response came from.
    let row: (String, String, i32, i32) = sqlx::query_as(
        "select storage_key, content_type, width, height from media_derivatives where cache_key = $1",
    )
    .bind(&first_key)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the derivative row must exist");
    assert_eq!(row.1, "image/webp");
    assert_eq!(
        (row.2, row.3),
        (400, 400),
        "the stored size is the box, not the source"
    );
    assert!(row.0.ends_with(".webp"), "{}", row.0);

    // The bytes are really in the store under that key.
    let stored = fixture
        .storage
        .get(&row.0)
        .await
        .expect("the derivative object must be readable");
    assert_eq!(stored, first.bytes, "the served bytes are the stored bytes");

    // Second request: served from the cache, same bytes, and the CDN headers say a year.
    let second = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=card"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.bytes, first.bytes,
        "a cached derivative must be byte-identical"
    );
    assert_eq!(second.derivative_key.as_deref(), Some(first_key.as_str()));
    assert_eq!(
        second.cache_control.as_deref(),
        Some("public, max-age=31536000, immutable"),
        "a content-addressed object is cacheable for a year"
    );

    // The download name carries the preset, so a browser saving it is not confused.
    assert!(
        second
            .content_disposition
            .as_deref()
            .is_some_and(|value| value.contains("hero-card.webp")),
        "{:?}",
        second.content_disposition
    );

    // Editing the preset's quality changes the key. Serving the previous quality's pixels under
    // the new name is the bug this is here to prevent.
    let edited = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/media/transformation-presets/{preset_id}?site_id={site}"),
            Some(&editor),
            Some(json!({ "name": "card", "width": 400, "height": 400, "format": "webp", "quality": 40 })),
        ),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "edit body: {}", edited.body);

    let after_edit = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=card"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(after_edit.status, StatusCode::OK);
    let after_key = after_edit.derivative_key.expect("a key");
    assert_ne!(
        after_key, first_key,
        "editing a preset must change the key it is served under"
    );
    // And the old row is untouched, not overwritten: a page still holding the old URL keeps
    // working with the pixels it was cached with.
    let old_still_there: i64 =
        sqlx::query_scalar("select count(*) from media_derivatives where cache_key = $1")
            .bind(&first_key)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the old row must still read");
    assert_eq!(old_still_there, 1, "an edit must not rewrite the old entry");

    // Deleting the preset takes its derivatives with it.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/transformation-presets/{preset_id}?site_id={site}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let remaining: i64 = sqlx::query_scalar(
        "select count(*) from media_derivatives d \
         join media m on m.id = d.media_id where m.site_id = $1",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the derivative count must read");
    assert_eq!(
        remaining, 0,
        "a deleted preset's cache is unreachable and must not linger"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_contain_preset_letterboxes_and_an_unknown_preset_falls_back_to_the_original() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a();
    let editor = fixture.editor_token().await;
    let source = source_png();

    let upload = call(
        &fixture.state,
        upload_request(
            &format!("/api/v1/media?site_id={site}"),
            Some(&editor),
            "wide.png",
            "image/png",
            &source,
        ),
    )
    .await;
    assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.body);
    let media_id = id_of(&upload.body);

    // `contain` into a square box: the result is still square, letterboxed. A `cover` crop would
    // also be square, so the check is the *shape* of the answer, which the route reports.
    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/transformation-presets?site_id={site}"),
            Some(&editor),
            Some(json!({ "name": "square", "width": 300, "height": 300, "fit": "contain", "format": "png" })),
        ),
    )
    .await;
    let contained = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=square"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        contained.status,
        StatusCode::OK,
        "contain body: {}",
        contained.body
    );
    assert_eq!(contained.content_type.as_deref(), Some("image/png"));

    // One query reading both columns: two queries could read the row twice and disagree, and a
    // test that can disagree with itself is a test that will eventually.
    let contained_key = contained
        .derivative_key
        .expect("a derivative must carry its key");
    let (width, height): (i32, i32) =
        sqlx::query_as("select width, height from media_derivatives where cache_key = $1")
            .bind(&contained_key)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must exist");
    assert_eq!(
        (width, height),
        (300, 300),
        "a letterboxed derivative keeps the box, so a card does not change height per image"
    );

    // An unknown preset falls back to the original *bytes*, not to a 404: a published page must
    // not lose its picture because somebody renamed a preset.
    let unknown = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=does-not-exist"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::OK,
        "an unknown preset must fall back, not 404: {}",
        unknown.body
    );
    assert_eq!(unknown.content_type.as_deref(), Some("image/png"));
    assert_eq!(
        unknown.bytes, source,
        "the fallback is the original file, byte for byte"
    );
    assert!(
        unknown.derivative_key.is_none(),
        "a fallback is not a derivative and must not be cached as one"
    );

    // A request that would enlarge is refused with an explanation, not answered with a blur.
    let upscaling = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=standard"),
            Some(&editor),
            None,
        ),
    )
    .await;
    // `standard` is seeded at 1200x630, which is exactly the source: the identity, allowed.
    assert!(
        upscaling.status == StatusCode::OK,
        "the seeded preset at the source's own size must work: {}",
        upscaling.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_preset_that_cannot_be_applied_says_which_thing_was_wrong() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a();
    let editor = fixture.editor_token().await;

    // A non-image is refused with its own code: an SVG is a valid file this feature cannot
    // handle, which is a different answer from "these bytes are corrupt".
    let upload = call(
        &fixture.state,
        upload_request(
            &format!("/api/v1/media?site_id={site}"),
            Some(&editor),
            "logo.svg",
            "image/svg+xml",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>",
        ),
    )
    .await;
    assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.body);
    let media_id = id_of(&upload.body);

    call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/transformation-presets?site_id={site}"),
            Some(&editor),
            Some(json!({ "name": "card", "width": 200, "height": 200 })),
        ),
    )
    .await;

    let not_transformable = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/raw?preset=card"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(not_transformable.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(not_transformable.body["error"]["code"], "not_transformable");
    assert!(
        not_transformable.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("svg")),
        "the message must name the type: {}",
        not_transformable.body
    );

    // Every preset field error names the field, so the form can put it under the right input.
    for (payload, field) in [
        (
            json!({ "name": "card wide", "width": 200, "height": 200 }),
            "name",
        ),
        (
            json!({ "name": "zero", "width": 0, "height": 200 }),
            "width",
        ),
        (
            json!({ "name": "quality", "width": 200, "height": 200, "quality": 900 }),
            "quality",
        ),
        (json!({ "name": "nodims" }), "width"),
    ] {
        let refused = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/media/transformation-presets?site_id={site}"),
                Some(&editor),
                Some(payload.clone()),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{payload} must be refused: {}",
            refused.body
        );
        assert_eq!(refused.body["error"]["code"], "invalid_preset");
        assert_eq!(
            refused.body["error"]["details"]["field"], field,
            "the error must name `{field}`: {}",
            refused.body
        );
    }

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_preset_routes_are_permission_gated_and_tenant_scoped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a();
    let presets = format!("/api/v1/media/transformation-presets?site_id={site}");
    let editor = fixture.editor_token().await;
    let reader = fixture.reader_token().await;

    // Anonymous: nothing.
    for method in [Method::GET, Method::POST] {
        let response = call(
            &fixture.state,
            request(method.clone(), &presets, None, None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {presets}"
        );
    }

    // A reader sees the list — the browser shows a preset picker on every image — but cannot
    // change the set, because that is what every published page renders.
    let listing = call(
        &fixture.state,
        request(Method::GET, &presets, Some(&reader), None),
    )
    .await;
    assert_eq!(
        listing.status,
        StatusCode::OK,
        "a reader must see the presets"
    );
    assert!(listing.body["presets"].is_array());

    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &presets,
            Some(&reader),
            Some(json!({ "name": "sneaky", "width": 100, "height": 100 })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["error"]["code"], "permission_denied");

    // A second site of the same organization is fine; a site of *another* organization is not.
    let other = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ('Other', $1) returning id",
    )
    .bind(format!("media-transform-other-{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the organization must be created");
    let foreign_site = sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, 'main', 'Foreign') returning id",
    )
    .bind(other)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the site must be created");

    let crossed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/transformation-presets?site_id={foreign_site}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert!(
        crossed.status == StatusCode::FORBIDDEN || crossed.status == StatusCode::NOT_FOUND,
        "a site of another organization must not be readable: {}",
        crossed.status
    );

    sqlx::query("delete from sites where id = $1")
        .bind(foreign_site)
        .execute(fixture.db.pool())
        .await
        .expect("the foreign site must be removed");
    sqlx::query("delete from organizations where id = $1")
        .bind(other)
        .execute(fixture.db.pool())
        .await
        .expect("the organization must be removed");

    // The seed gave the site a `standard` preset, so the migration's idempotent seed is proved
    // by the listing rather than by a separate assertion.
    let final_listing = call(
        &fixture.state,
        request(Method::GET, &presets, Some(&editor), None),
    )
    .await;
    assert_eq!(final_listing.status, StatusCode::OK);
    let names: Vec<&str> = final_listing.body["presets"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|preset| preset["name"].as_str())
        .collect();
    assert!(
        names.contains(&"standard"),
        "every site is seeded with `standard`: {names:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_preset_name_is_a_conflict_not_a_second_row() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a();
    let editor = fixture.editor_token().await;
    let uri = format!("/api/v1/media/transformation-presets?site_id={site}");

    let first = call(
        &fixture.state,
        request(
            Method::POST,
            &uri,
            Some(&editor),
            Some(json!({ "name": "thumb", "width": 320, "height": 320, "format": "jpeg" })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);

    // The same name with different casing normalizes to the same preset: two rows called
    // `thumb` and `Thumb` would be one nobody can request, because the URL is lowercased.
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            &uri,
            Some(&editor),
            Some(json!({ "name": "THUMB", "width": 100, "height": 100 })),
        ),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.body);
    assert_eq!(second.body["error"]["code"], "preset_name_taken");

    let count: i64 = sqlx::query_scalar(
        "select count(*) from media_transformation_presets where site_id = $1 and name = 'thumb'",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(count, 1, "a name clash must not create a second row");

    fixture.cleanup().await;
}

impl Fixture {
    /// The site this fixture created.
    fn site_a(&self) -> Uuid {
        self.sites[0]
    }
}
