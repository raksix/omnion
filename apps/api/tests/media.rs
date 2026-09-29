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
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, CsrfSecret};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_media::MAX_UPLOAD_BYTES;
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope};
use omnion_permissions::{bindings, roles as role_store, seed};
use omnion_security::RatePolicy;
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

/// Key material this suite's own state signs its CSRF tokens with.
///
/// A test-only value with a test-only name: it is the fixture's *own* secret, and nothing the
/// suite stores is protected by anything but the walls of the process. Naming it here is what
/// keeps the distinction legible — a real secret would be a credential in a public repository.
const CSRF_SECRET: &str = "csrf-media-walk-suite-key-material-not-a-real-secret";

/// The header a cookie-authenticated write has to carry its CSRF token in.
const CSRF_HEADER: &str = "x-omnion-csrf";

/// Result of one in-process HTTP call, in the pieces the assertions need.
struct TestResponse {
    status: StatusCode,
    /// The **first** `Set-Cookie`, which is what the pre-existing assertions read.
    set_cookie: Option<String>,
    /// Every `Set-Cookie` on the response, joined.
    ///
    /// Sign-in sets two: the session and the CSRF token beside it. `get` returns the first, so
    /// a helper that reads `set_cookie` alone sees a session with no token and concludes the
    /// deployment never issued one — which is the message the CSRF layer gives a deployment
    /// without a secret, so the two are indistinguishable from the call site.
    set_cookies: Vec<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    nosniff: bool,
    /// `Accept-Ranges` on the response — the claim that ranges are supported at all.
    accept_ranges: Option<String>,
    /// `Content-Range` on the response, when the answer was a window.
    content_range: Option<String>,
    body: Value,
    bytes: Vec<u8>,
}

/// Give this suite a rate-limit budget of its own, once per process.
///
/// The limiter is a process-wide cell that `router()` fills from the **stored** document, and the
/// stored `sign_in` scope is ten requests per five minutes. This suite signs in three accounts per
/// walk and runs fifteen walks, so the eleventh sign-in is refused with `429` and every walk after
/// it dies on a line that has nothing to do with media. The failure is worse than useless: it
/// names a *rate limit* on a suite that was never testing rate limits, and the obvious reading —
/// "the limiter is too strict" — is the opposite of the truth.
///
/// Raising the ceiling here does not weaken what the limiter suite proves, because that suite
/// installs and asserts its own numbers: this cell is process-wide, so whichever fixture installs
/// first wins, and a suite that needs the shipped policy is asserting the policy rather than
/// sharing a budget with other tests.
fn give_the_suite_its_own_rate_limit(state: &AppState) {
    let policies: Vec<RatePolicy> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            // Only the sign-in scope needs raising. The rest of the ceilings are the ones a
            // deployment ships, and leaving them alone keeps a suite from being the reason a
            // genuinely over-budget request stops being refused.
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    omnion_api::rate_limit_middleware::install(RateLimiter::new(state, policies));
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
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let content_type = header_text(header::CONTENT_TYPE);
    let content_disposition = header_text(header::CONTENT_DISPOSITION);
    let nosniff = header_text(header::X_CONTENT_TYPE_OPTIONS).as_deref() == Some("nosniff");
    let accept_ranges = header_text(header::ACCEPT_RANGES);
    let content_range = header_text(header::CONTENT_RANGE);

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
        set_cookies,
        content_type,
        content_disposition,
        nosniff,
        accept_ranges,
        content_range,
        body,
        bytes,
    }
}

/// Build a JSON request; `token` becomes the session cookie.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, cookie_header(token));
        if let Some(csrf) = csrf_token(token) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
    }

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

/// A multipart body carrying a `note` part beside the `file` part.
///
/// The two parts are written out in full rather than assembled by cutting the single-part
/// builder's output: a part appended *after* a closing boundary is not part of the body at all,
/// which is a silent no-op rather than an error, and the note would arrive empty.
fn multipart_body_with_note(
    boundary: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
    note: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 320);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"note\"\r\n\r\n");
    body.extend_from_slice(note.as_bytes());
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
    let mut builder = Request::builder().method(Method::POST).uri(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, cookie_header(token));
        // An upload is a write, so it carries the CSRF token in the **header** as well as in the
        // cookie. The cookie alone is a fallback the middleware accepts, but a token in the
        // cookie and no token in the header is precisely the state a stale page is in — the layer
        // reads the header first so that the explicit intention wins, and a suite that only set
        // the cookie was testing the fallback path while believing it tested the normal one.
        if let Some(csrf) = csrf_token(token) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
    }

    builder
        .body(Body::from(multipart_body(
            BOUNDARY,
            filename,
            content_type,
            bytes,
        )))
        .expect("request must build")
}

/// Build a `GET` carrying a `Range` header.
///
/// Its own builder rather than a parameter on [`request`], because a range is only meaningful
/// on a read: threading an `Option<&str>` through a builder that also signs people in, uploads
/// multipart bodies and deletes files would put a header where a body is expected, and the
/// mistake that follows is silent — a test that thinks it asked for a window and got the whole
/// object.
fn range_request(uri: &str, token: Option<&str>, range: &str) -> Request<Body> {
    let builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::RANGE, range);
    let builder = match token {
        Some(token) => builder.header(header::COOKIE, cookie_header(token)),
        None => builder,
    };
    builder.body(Body::empty()).expect("request must build")
}

/// The `Cookie` header for one caller, from whatever the fixture handed back.
///
/// A bare session id and a whole `name=value; name=value` header both have to work, because the
/// suite has readers (which need only the session) and writers (which need the CSRF token beside
/// it) and neither should have to know which kind it was handed. Wrapping a header that already
/// carries `=` would produce `omnion_session=a=…; b=…`, which is a cookie named `omnion_session`
/// with the value `"a"` and a stray pair the server ignores — a session that authenticates for
/// nothing and a test failure that reads as a permission problem.
fn cookie_header(token: &str) -> String {
    let session = session_of(token);
    let csrf = csrf_token(token);
    match csrf {
        Some(token) => format!("omnion_session={session}; omnion_csrf={token}"),
        None => format!("omnion_session={session}"),
    }
}

/// The session id inside a caller's credential.
fn session_of(token: &str) -> &str {
    match token.split_once('\u{1f}') {
        Some((session, _)) => session,
        // A bare session id, which is what a caller with no token passes.
        None => token,
    }
}

/// The CSRF token inside a caller's credential, when it carries one.
///
/// The credential the fixture hands out is `<session>\x1f<token>`: two things that must travel
/// together on a write, packed into the one `String` the helpers already return. The separator is
/// a unit separator rather than `;` or `=` because neither can appear in a session id or a
/// derived token, so a helper that guesses wrong cannot silently read half a value as a whole one.
fn csrf_token(token: &str) -> Option<String> {
    let (_, csrf) = token.split_once('\u{1f}')?;
    (!csrf.is_empty()).then(|| csrf.to_owned())
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
    let mut config = Config::from_env().expect("environment must be valid");
    // The CSRF secret is set on the **config**, not through the environment. Sign-in only issues
    // a token when the running state carries one, and a suite that relies on `OMNION_CSRF_SECRET`
    // being in the shell is a suite that silently stops testing writes the moment it is not —
    // which is exactly what happened here: every upload answered `403 csrf_unavailable`, and the
    // message names the server's configuration rather than the suite's own missing token.
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
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
    give_the_suite_its_own_rate_limit(&state);
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
    ///
    /// The keys come from the *union* of the live rows and the version history. Reading only
    /// `media.storage_key` leaves every replaced version's object in the bucket, because a
    /// replace moves that column to the new key and the old one is named only by the history —
    /// a cleanup that misses them turns a test run into a slow leak.
    async fn cleanup(&self) {
        let keys: Vec<String> = sqlx::query_scalar(
            "select storage_key from media where site_id = any($1) \
             union \
             select v.storage_key from media_versions v \
               join media m on m.id = v.media_id where m.site_id = any($1)",
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
    session_cookie(state, email).await
}

/// Sign in and return the **whole cookie header**, session and CSRF token together.
///
/// Every `Set-Cookie` is kept rather than only the first one. The CSRF layer (tick 59) makes the
/// session cookie *ambient* authority — anything a browser sends along on its own — so a
/// cookie-authenticated write now has to present a token as well, and sign-in is where the token
/// is issued. The previous helper took `.split(';').next()`, which is correct for one cookie and
/// silently drops every cookie after it: the walks then failed on `csrf_unavailable` with a
/// message that names the server's configuration rather than the suite's own loss of the token,
/// and the failure looked like a broken deployment instead of a broken helper.
async fn session_cookie(state: &AppState, email: &str) -> String {
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
    let set_cookie = response.set_cookies.join("; ");
    assert!(
        set_cookie.contains("omnion_session="),
        "sign-in must set the session cookie: {set_cookie}"
    );
    // A sign-in that issued no CSRF token is a deployment without `OMNION_CSRF_SECRET`, and this
    // suite is not the place to discover that: every write below would fail identically.
    assert!(
        set_cookie.contains("omnion_csrf="),
        "sign-in must issue a CSRF token, or every cookie-authenticated write is refused: \
         {set_cookie}"
    );
    // The session id is the only part of the credential the *header* cannot express, so the two
    // are handed to the builders separately: a cookie header and the token to echo in
    // `x-omnion-csrf`.
    let session = set_cookie
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("omnion_session="))
        .expect("the joined header carries the session");
    let token = set_cookie
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("omnion_csrf="))
        .map(str::to_owned);
    format!("{session}\u{1f}{}", token.unwrap_or_default())
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
        .header(header::COOKIE, cookie_header(&editor))
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
    assert_eq!(
        campaigns.status,
        StatusCode::CREATED,
        "body: {}",
        campaigns.body
    );
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
    assert_eq!(
        archive.status,
        StatusCode::CREATED,
        "body: {}",
        archive.body
    );

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
    assert_eq!(
        reparented.status,
        StatusCode::OK,
        "body: {}",
        reparented.body
    );
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
    assert_eq!(
        deep_folder.status,
        StatusCode::OK,
        "body: {}",
        deep_folder.body
    );
    let crumbs: Vec<String> = deep_folder.body["breadcrumb"]
        .as_array()
        .expect("the breadcrumb is an array")
        .iter()
        .map(|crumb| crumb["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        crumbs,
        vec!["Media", "Archive", "Campaigns", "2026 Launch"],
        "{crumbs:?}"
    );

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
    assert_eq!(
        scratch.status,
        StatusCode::CREATED,
        "body: {}",
        scratch.body
    );
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
                &format!(
                    "/api/v1/media/files?site_id={site}&folder_id={}",
                    id_of(&scratch.body)
                ),
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
    let key_before: String =
        sqlx::query_scalar::<_, String>("select storage_key from media where id = $1")
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
        fixture.storage.get(&key_after_delete).await.is_ok(),
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
    let entries = trash.body["entries"]
        .as_array()
        .expect("entries is an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], json!(first_id));
    assert!(
        entries[0]["purges_at"].as_str().is_some(),
        "the countdown is real"
    );

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
    assert_eq!(
        tags,
        json!(["summer"]),
        "tags are trimmed, lower-cased and unique"
    );

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
    assert_eq!(
        purged.status,
        StatusCode::NO_CONTENT,
        "body: {}",
        purged.body
    );
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

// ---------------------------------------------------------------------------------------------
// The version history (REQ-010, slice 2)
// ---------------------------------------------------------------------------------------------

/// A PNG of the given size, built header-first — the probe only ever reads the header.
fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&0u32.to_be_bytes());
    // A unique tail so two "same size, different bytes" versions really do differ.
    bytes.extend_from_slice(b"omnion-version-walk");
    bytes
}

/// Post a replacement to a file's version route, with a note beside the file.
fn replace_request(
    uri: &str,
    token: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
    note: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::COOKIE, cookie_header(token));
    if let Some(csrf) = csrf_token(token) {
        builder = builder.header(CSRF_HEADER, csrf);
    }
    builder
        .body(Body::from(multipart_body_with_note(
            BOUNDARY,
            filename,
            content_type,
            bytes,
            note,
        )))
        .expect("request must build")
}

#[tokio::test]
async fn replacing_a_file_keeps_the_old_bytes_as_a_version() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    // A 640×360 PNG. The header is what the probe reads, so the row must come back with the
    // dimensions filled in rather than null — the preview's aspect ratio depends on them.
    let original = png_bytes(640, 360);
    let uploaded = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "hero.png", "image/png", &original),
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::CREATED,
        "body: {}",
        uploaded.body
    );
    let file_id = id_of(&uploaded.body);
    let media_id = Uuid::parse_str(&file_id).expect("the uploaded id is a uuid");

    let detail = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files/{file_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.body["width"], 640, "the header states the width");
    assert_eq!(detail.body["height"], 360, "the header states the height");
    assert_eq!(detail.body["version_count"], 1, "an upload is version 1");

    // The upload wrote a version 1, so the history is never empty on a file that exists.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK, "body: {}", history.body);
    assert_eq!(history.body["version_total"], 1);
    assert_eq!(history.body["versions"][0]["version"], 1);
    assert_eq!(
        history.body["versions"][0]["is_current"], true,
        "the only version is the current one"
    );

    // Replace the bytes. The file keeps its name, its id and its folder.
    let replacement = png_bytes(1920, 1080);
    let replaced = call(
        &fixture.state,
        replace_request(
            &format!("/api/v1/media/{file_id}/versions"),
            &editor,
            "hero.png",
            "image/png",
            &replacement,
            "the campaign crop",
        ),
    )
    .await;
    assert_eq!(
        replaced.status,
        StatusCode::CREATED,
        "body: {}",
        replaced.body
    );
    assert_eq!(
        replaced.body["version"]["version"], 2,
        "a replace appends the next number"
    );
    assert_eq!(
        replaced.body["file"]["filename"], "hero.png",
        "a replace never renames the file"
    );
    assert_eq!(replaced.body["file"]["width"], 1920);
    assert_eq!(
        replaced.body["version"]["note"], "the campaign crop",
        "the note rides as its own multipart part"
    );

    // The row now points at the new bytes and says two versions.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/files/{file_id}"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(after.body["version_count"], 2);
    assert_eq!(after.body["size_bytes"], replacement.len() as i64);

    // Version 1 is still downloadable, and it still serves the OLD bytes. This is the whole
    // point of the slice: if the old key had been overwritten, this read would return the
    // 1920×1080 payload and the assertion on the size would pass by accident on a length
    // check — so the bytes are compared, not just the status.
    let old_bytes = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions/1/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(old_bytes.status, StatusCode::OK, "body: {}", old_bytes.body);
    assert_eq!(
        old_bytes.bytes, original,
        "version 1 must still be the bytes it was"
    );

    // The current version serves the new bytes through the panel's own read path. The path is
    // `/media/{id}/raw` — the *file manager* list is `/media/files`, and a raw read addressed at
    // `/media/files/{id}/raw` has no route, so it would 404 and read as an empty body here.
    let current_bytes = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        current_bytes.status,
        StatusCode::OK,
        "body: {}",
        current_bytes.body
    );
    assert_eq!(current_bytes.bytes, replacement);

    // Two versions, two storage keys, and the current key is the newer one.
    let keys: Vec<String> = sqlx::query_scalar(
        "select storage_key from media_versions where media_id = $1 order by version",
    )
    .bind(media_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the version keys must read");
    assert_eq!(keys.len(), 2, "one key per version: {keys:?}");
    assert_ne!(
        keys[0], keys[1],
        "two versions may never share one object key"
    );
    assert!(
        keys[1].ends_with("/v2.png"),
        "the key carries the version number: {}",
        keys[1]
    );
    let current_key: String = sqlx::query_scalar("select storage_key from media where id = $1")
        .bind(media_id)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the current key must read");
    assert_eq!(
        current_key, keys[1],
        "the row points at the version it serves"
    );

    // A download of an old version is an attachment named after *that* version, so two
    // versions do not collide on one name in a download folder.
    let downloaded = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions/1/download"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(downloaded.status, StatusCode::OK);
    let disposition = downloaded
        .content_disposition
        .as_deref()
        .expect("a download carries a disposition");
    assert!(
        disposition.contains("attachment") && disposition.contains("hero-v1.png"),
        "the download is an attachment named for the version: {disposition}"
    );
    assert_eq!(downloaded.bytes, original);

    // A version that does not exist is named as missing, not answered with the current bytes.
    let missing = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions/9/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body["error"]["code"], "version_not_found");

    // And the replace is audited.
    assert!(
        audit_rows(&fixture.db, "media.version_created", &file_id).await > 0,
        "a replace leaves an audit row"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn restoring_an_old_version_appends_instead_of_rewriting() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    let first = png_bytes(800, 600);
    let uploaded = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "banner.png", "image/png", &first),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::CREATED);
    let file_id = id_of(&uploaded.body);
    let media_id = Uuid::parse_str(&file_id).expect("the uploaded id is a uuid");

    let second = png_bytes(1024, 768);
    call(
        &fixture.state,
        replace_request(
            &format!("/api/v1/media/{file_id}/versions"),
            &editor,
            "banner.png",
            "image/png",
            &second,
            "v2",
        ),
    )
    .await;

    // Restore version 1. It comes back as version 3 — a *new* version — and version 1 itself is
    // not touched, so the history stays a straight line of appends.
    let restored = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{file_id}/versions/1/restore"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(restored.status, StatusCode::OK, "body: {}", restored.body);
    assert_eq!(
        restored.body["version"]["version"], 3,
        "a restore appends; it does not renumber"
    );
    assert_eq!(
        restored.body["file"]["width"], 800,
        "the restored bytes are the old ones"
    );

    // The three rows, with their checksums: version 1 and version 3 carry the same checksum
    // (same bytes), version 2 differs, and the numbers are 1, 2, 3 with no reuse.
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "select version, checksum, storage_key from media_versions where media_id = $1 order by version",
    )
    .bind(media_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the versions must read");
    assert_eq!(
        rows.iter().map(|row| row.0).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "three versions, in order"
    );
    assert_eq!(
        rows[0].1, rows[2].1,
        "the restored copy is byte-identical to the version it came from"
    );
    assert_ne!(rows[1].1, rows[0].1, "version 2 is a different upload");
    assert_eq!(
        rows.iter()
            .map(|row| row.2.clone())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3,
        "three versions, three keys — a restore copies, it does not point at the old object"
    );

    // The current bytes are version 1's bytes again, read through the panel's own path.
    let current = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(current.status, StatusCode::OK, "body: {}", current.body);
    assert_eq!(current.bytes, first);

    // And version 2 — the one that was replaced — is still downloadable after the restore.
    let second_still_there = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions/2/raw"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        second_still_there.bytes, second,
        "a restore does not remove the version that was current"
    );

    // The history says three versions, with exactly one marked current.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{file_id}/versions"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(history.body["version_total"], 3);
    let current_flags: Vec<bool> = history.body["versions"]
        .as_array()
        .expect("versions is an array")
        .iter()
        .map(|entry| entry["is_current"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        current_flags.iter().filter(|flag| **flag).count(),
        1,
        "exactly one version is current"
    );
    // The listing is newest first, so the current one is the *first* entry — a check written
    // against an assumed oldest-first order fails on a correct response.
    assert!(
        current_flags[0] && !current_flags[1] && !current_flags[2],
        "and it is the newest, which the listing puts first: {current_flags:?}"
    );
    let listed: Vec<i64> = history.body["versions"]
        .as_array()
        .expect("versions is an array")
        .iter()
        .filter_map(|entry| entry["version"].as_i64())
        .collect();
    assert_eq!(listed, vec![3, 2, 1], "the history reads newest first");

    // Restoring a version that does not exist is refused by name.
    let missing = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{file_id}/versions/7/restore"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body["error"]["code"], "version_not_found");

    assert!(
        audit_rows(&fixture.db, "media.version_restored", &file_id).await > 0,
        "a restore leaves an audit row"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_version_routes_are_permission_gated_and_scoped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let member = fixture.member_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    let uploaded = call(
        &fixture.state,
        upload_request(
            &library,
            Some(&editor),
            "notes.txt",
            "text/plain",
            b"the original text",
        ),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::CREATED);
    let file_id = id_of(&uploaded.body);
    let history_uri = format!("/api/v1/media/{file_id}/versions");

    // Without a session, the history and the bytes of a version are both closed.
    for uri in [
        history_uri.clone(),
        format!("/api/v1/media/{file_id}/versions/1/raw"),
    ] {
        let response = call(&fixture.state, request(Method::GET, &uri, None, None)).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{uri}");
    }

    // A member without a media permission sees none of it either.
    for uri in [
        history_uri.clone(),
        format!("/api/v1/media/{file_id}/versions/1/raw"),
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&member), None),
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{uri}");
    }

    // The platform Owner, whose scope crosses tenants, may read the history — the gate is the
    // permission, not the organization, so the same route answers for a different caller.
    let platform = fixture.platform_token().await;
    let owner_reads = call(
        &fixture.state,
        request(Method::GET, &history_uri, Some(&platform), None),
    )
    .await;
    assert_eq!(
        owner_reads.status,
        StatusCode::OK,
        "body: {}",
        owner_reads.body
    );

    // A file that does not exist is a 404 by id, naming what is missing — a stale deep link
    // must not answer with somebody else's history.
    let stale = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{}/versions", Uuid::new_v4()),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(stale.status, StatusCode::NOT_FOUND);
    assert_eq!(
        stale.body["error"]["code"], "media_not_found",
        "a stale id is named as a missing media row"
    );

    // An upload with no bytes is refused on the version route as well.
    let empty = call(
        &fixture.state,
        replace_request(&history_uri, &editor, "x.txt", "text/plain", b"", ""),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    // The media crate answers an empty upload as a single `invalid_request`; the walk asserts
    // the name the surface actually uses rather than one invented for the test.
    assert_eq!(empty.body["error"]["code"], "invalid_request");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// The camera record (REQ-010, slice 3)
// ---------------------------------------------------------------------------------------------

/// A SHORT as the four inline bytes of an entry: the value, then two zero bytes.
fn inline_short(value: u16) -> [u8; 4] {
    let mut inline = [0u8; 4];
    inline[..2].copy_from_slice(&value.to_le_bytes());
    inline
}

/// One IFD entry: tag, type, count, and the four bytes that sit inside the entry.
fn exif_entry(out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, inline: [u8; 4]) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&inline);
}

/// The byte offset of IFD0's `index`-th entry, in the layout [`jpeg_with_exif`] builds.
fn ifd0_entry_at(index: usize) -> usize {
    // 8 (the TIFF header) + 2 (the entry count) + index * 12.
    10 + index * 12
}

/// The byte offset of the sub-directory's `index`-th entry.
///
/// Every byte of the prefix is really there: the TIFF header, IFD0's count field, IFD0's four
/// entries, **IFD0's four-byte next-directory pointer**, and the sub-directory's own count
/// field. Forgetting the next-pointer — which the first version of this function did — put
/// every sub-directory entry four bytes early, which is invisible for five of them and fatal
/// for the sixth: the lens pointer then landed inside the value area and overwrote the start of
/// the camera make.
fn sub_entry_at(index: usize) -> usize {
    const BEFORE_SUB: usize = 8 + 2 + 4 * 12 + 4 + 2;
    BEFORE_SUB + index * 12
}

/// The value bytes of a JPEG carrying a real EXIF block.
///
/// `orientation` and the frame size are the two things a test moves to change what the reader
/// concludes, and both are arguments here — a committed JPEG would be a binary blob nobody can
/// review, and a hand-built one shows *which* byte each assertion depends on.
///
/// The layout is the format's: a TIFF header, an IFD0 with the values that fit inline, then an
/// Exif sub-directory, then every value too wide for its entry, in the order the entries appear.
fn jpeg_with_exif(orientation: u16, width: u16, height: u16) -> Vec<u8> {
    // IFD0 holds make, model, orientation and the sub-directory pointer.
    const IFD0_ENTRIES: usize = 4;
    // The sub-directory holds ISO, exposure, aperture, focal length, the date and the lens.
    const SUB_ENTRIES: usize = 6;

    let mut block: Vec<u8> = Vec::new();
    block.extend_from_slice(b"II");
    block.extend_from_slice(&42u16.to_le_bytes());
    block.extend_from_slice(&8u32.to_le_bytes());

    // IFD0 at offset 8, so its value area starts after its own table.
    block.extend_from_slice(&(IFD0_ENTRIES as u16).to_le_bytes());
    exif_entry(&mut block, 0x010f, 2, 6, [0; 4]);
    exif_entry(&mut block, 0x0110, 2, 14, [0; 4]);
    // A SHORT lives in the first two bytes of the entry's four, high bytes zero.
    exif_entry(&mut block, 0x0112, 3, 1, inline_short(orientation));
    let subdir_pointer_at = block.len() + 8;
    exif_entry(&mut block, 0x8769, 4, 1, [0; 4]);
    block.extend_from_slice(&0u32.to_le_bytes());
    let subdir_at = block.len();

    block.extend_from_slice(&(SUB_ENTRIES as u16).to_le_bytes());
    exif_entry(&mut block, 0x8827, 3, 1, inline_short(400));
    exif_entry(&mut block, 0x829a, 5, 1, [0; 4]);
    exif_entry(&mut block, 0x829d, 5, 1, [0; 4]);
    exif_entry(&mut block, 0x920a, 5, 1, [0; 4]);
    exif_entry(&mut block, 0x9003, 2, 20, [0; 4]);
    exif_entry(&mut block, 0xa434, 2, 25, [0; 4]);
    block.extend_from_slice(&0u32.to_le_bytes());

    // Now the value area. An offset is measured from the **start of the block**, and the area
    // sits after the last table rather than after any one of them — so every value is appended
    // here and the entry that declared it is patched with wherever the bytes actually landed.
    //
    // The first version computed each table's value area as `block.len() + entries * 12` at the
    // moment that table finished. That is right for the *last* table and wrong for every other
    // one, and it was wrong twice over: IFD0's values were addressed before the sub-directory
    // existed at all, and `sub_wide_at` was taken before the sub-directory's own next-pointer.
    // The result was the lens pointer landing inside the value area and overwriting the first
    // two bytes of the camera make — so the reader parsed a block whose make began mid-word and
    // reported *no camera at all*, which the walk read as a null column rather than as a
    // broken fixture. Silent, total, and pointing at the code under test rather than at the
    // code that built the input.
    //
    // One allocator, shared, is the fix: an offset is wherever the bytes are, and the reader
    // is the one thing that has to agree.
    // The value area is written first and the pointers second, because a pointer is only known
    // once its value has landed. The first version fused the two into one `place` closure —
    // which wrote the pointer into the entry and *then* appended the value, so a value longer
    // than four bytes overwrote the pointer that had just named it. IFD0's `make` and `model`
    // are both longer than four bytes, and both came back as no camera at all.
    let mut value_at = block.len();
    //
    // Named pairs, because the whole bug was positional: two parallel lists that happened to
    // line up, and a reader has to count to check whether they still do. Here each value sits
    // beside the entry that declares it, so "is the lens pointed at the lens" is a glance.
    //
    // ISO is a SHORT and lives *inside* its entry, so it has no pointer and is not here.
    let wide: Vec<(usize, Vec<u8>)> = vec![
        (ifd0_entry_at(0) + 8, b"Canon\0".to_vec()),
        (ifd0_entry_at(1) + 8, b"Canon EOS R5\0".to_vec()),
        // 0x829a exposure: a RATIONAL is numerator *then* denominator, and `1/200 s` is the
        // case the reader's zero-denominator guard exists for.
        (sub_entry_at(1) + 8, {
            let mut value = 1u32.to_le_bytes().to_vec();
            value.extend_from_slice(&200u32.to_le_bytes());
            value
        }),
        // 0x829d aperture: f/1.8.
        (sub_entry_at(2) + 8, {
            let mut value = 180u32.to_le_bytes().to_vec();
            value.extend_from_slice(&100u32.to_le_bytes());
            value
        }),
        // 0x920a focal length in 35mm: 50mm, a whole number, so denominator one.
        (sub_entry_at(3) + 8, {
            let mut value = 50u32.to_le_bytes().to_vec();
            value.extend_from_slice(&1u32.to_le_bytes());
            value
        }),
        // 0x9003 the date, then 0xa434 the lens.
        (sub_entry_at(4) + 8, b"2019:07:04 12:34:56\0".to_vec()),
        (sub_entry_at(5) + 8, b"RF 24-70mm F2.8 L IS USM\0".to_vec()),
    ];
    for (_, bytes) in &wide {
        block.extend_from_slice(bytes);
    }
    for (slot, bytes) in wide.iter() {
        block[*slot..*slot + 4].copy_from_slice(&(value_at as u32).to_le_bytes());
        value_at += bytes.len();
    }
    block[subdir_pointer_at..subdir_pointer_at + 4]
        .copy_from_slice(&(subdir_at as u32).to_le_bytes());

    // The JPEG around it: SOI, the `APP1` segment, then the frame the geometry probe reads.
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&block);
    let mut jpeg = vec![0xff, 0xd8];
    let length = u16::try_from(payload.len() + 2).expect("a test block is small");
    jpeg.extend_from_slice(&[0xff, 0xe1]);
    jpeg.extend_from_slice(&length.to_be_bytes());
    jpeg.extend_from_slice(&payload);
    jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
    jpeg.extend_from_slice(&height.to_be_bytes());
    jpeg.extend_from_slice(&width.to_be_bytes());
    jpeg.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
    jpeg
}

/// Read one `jsonb` column of a `media` row straight out of the database.
///
/// The API's own response is not enough: the question is what was *stored*, and a response that
/// omits a field is indistinguishable from one that stored it and chose not to say so.
async fn media_column(state: &AppState, id: Uuid, column: &str) -> Option<Value> {
    let sql = format!("select {column} from media where id = $1");
    sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_one(state.db().pool())
        .await
        .unwrap_or_else(|error| panic!("the {column} column must be readable: {error}"))
}

/// Read one **integer** column, by its own name.
///
/// A second helper rather than one that returns `Value` for everything, because a `jsonb`
/// decode of an `int4` is a type error rather than a wrong number: the column came back as
/// `mismatched types; Value (as JSONB) is not compatible with INT4`, which reads as a schema
/// problem and is really a helper that was only ever written for the two jsonb columns beside
/// it. The geometry assertions were passing through the API response and a null `exif` was
/// pointing the other way, so the helper hid the fact that the two kinds of column were being
/// read with one tool.
async fn media_int(state: &AppState, id: Uuid, column: &str) -> i32 {
    let sql = format!("select {column} from media where id = $1");
    sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_one(state.db().pool())
        .await
        .unwrap_or_else(|error| panic!("the {column} column must be readable: {error}"))
}

/// The builder and the reader agree, proved with no database and no walk.
///
/// The fixture was wrong twice and the walks could not tell which. The first failure was an
/// out-of-range slice *inside the builder*; the second was a `make` that quietly stopped
/// parsing, so `stored["make"]` was `null` — and a null column is a perfectly legal answer to
/// "what did the camera say", so it reads as a fact about the file rather than a fact about the
/// fixture. This test is the one that would have caught the second in a second, with the answer
/// printed next to the bytes.
#[test]
fn the_camera_builder_produces_a_block_the_reader_accepts() {
    let exif = omnion_media::read_exif("image/jpeg", &jpeg_with_exif(6, 4000, 3000));
    assert_eq!(exif.make.as_deref(), Some("Canon"), "make");
    assert_eq!(exif.model.as_deref(), Some("Canon EOS R5"), "model");
    assert_eq!(
        exif.lens.as_deref(),
        Some("RF 24-70mm F2.8 L IS USM"),
        "lens"
    );
    assert_eq!(exif.iso, Some(400), "iso");
    assert_eq!(exif.exposure_ms, Some(5), "1/200 s is five milliseconds");
    assert_eq!(exif.aperture_x100, Some(180), "f/1.8 is 180 hundredths");
    assert_eq!(exif.focal_length_mm, Some(50), "focal length in 35mm");
    assert_eq!(exif.orientation, Some(6), "orientation");
    assert!(!exif.gps, "no fix means no flag");
    assert_eq!(exif.captured_at.as_deref(), Some("2019-07-04T12:34:56"));
}

#[tokio::test]
async fn a_camera_record_is_read_from_the_bytes_and_never_holds_a_coordinate() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");
    let files = format!("/api/v1/media/files?site_id={site}");

    // A 4000×3000 photograph stored sideways (orientation 6): every browser draws it as a
    // 3000×4000 portrait, so the stored columns have to say 3000 and 4000.
    let shot = jpeg_with_exif(6, 4000, 3000);
    let uploaded = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "shoot.jpg", "image/jpeg", &shot),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::CREATED);
    let id: Uuid = uploaded.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    // The record comes out of the bytes, not out of the file name.
    let stored = media_column(&fixture.state, id, "exif")
        .await
        .expect("a photograph with a camera block must store one");
    assert_eq!(stored["make"], "Canon", "the maker is read from the block");
    assert_eq!(stored["model"], "Canon EOS R5");
    assert_eq!(stored["lens"], "RF 24-70mm F2.8 L IS USM");
    assert_eq!(stored["iso"], 400);
    assert_eq!(stored["exposure_ms"], 5, "1/200 s is five milliseconds");
    assert_eq!(stored["aperture_x100"], 180, "f/1.8 is 180 hundredths");
    assert_eq!(stored["focal_length_mm"], 50);
    assert_eq!(stored["captured_at"], "2019-07-04T12:34:56");
    assert_eq!(stored["orientation"], 6);
    // No fix means no key at all — a `false` would be a value somebody could filter on, and the
    // difference between "not read" and "read nothing" is the column's nullability.
    assert!(
        stored.get("gps").is_none(),
        "a camera with no fix must leave no flag: {stored}"
    );
    let serialised = stored.to_string();
    for forbidden in ["lat", "lon", "GPSLatitude", "GPSLongitude", "altitude"] {
        assert!(
            !serialised.contains(forbidden),
            "the record must not carry {forbidden}: {serialised}"
        );
    }

    // The geometry the row stores is the geometry a reader sees.
    assert_eq!(media_int(&fixture.state, id, "width").await, 3000);
    assert_eq!(media_int(&fixture.state, id, "height").await, 4000);

    // The listing sends both readings, so a grid reserves the right box and a version list can
    // still show what the camera stored.
    let listed = call(
        &fixture.state,
        request(Method::GET, &files, Some(&editor), None),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    let row = listed.body["files"]
        .as_array()
        .expect("a file array")
        .iter()
        .find(|entry| entry["id"] == uploaded.body["id"])
        .expect("the uploaded file is listed");
    assert_eq!(row["display_width"], 3000);
    assert_eq!(row["display_height"], 4000);
    assert_eq!(row["exif"]["model"], "Canon EOS R5");

    // A file with no camera block has no record at all — not an empty one, because "we never read
    // a block" and "the camera said nothing" are different rows in a report.
    let plain = call(
        &fixture.state,
        upload_request(
            &library,
            Some(&editor),
            "note.txt",
            "text/plain",
            b"no camera here",
        ),
    )
    .await;
    assert_eq!(plain.status, StatusCode::CREATED);
    let plain_id: Uuid = plain.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");
    assert!(
        media_column(&fixture.state, plain_id, "exif")
            .await
            .is_none(),
        "a text file must not grow a camera record"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_replacement_replaces_the_camera_record_rather_than_inheriting_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    let shot = jpeg_with_exif(1, 4000, 3000);
    let uploaded = call(
        &fixture.state,
        upload_request(&library, Some(&editor), "shoot.jpg", "image/jpeg", &shot),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::CREATED);
    let id: Uuid = uploaded.body["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");
    let versions = format!("/api/v1/media/{id}/versions");
    assert_eq!(
        media_column(&fixture.state, id, "exif")
            .await
            .expect("the restored version brings its camera record back")["model"],
        "Canon EOS R5"
    );
    // Orientation 1 is upright, so the stored columns are the frame's own.
    assert_eq!(media_int(&fixture.state, id, "width").await, 4000);

    // A replacement that is a *different* photograph: same body, a portrait crop stored sideways.
    let reshot = jpeg_with_exif(8, 4000, 3000);
    let replaced = call(
        &fixture.state,
        replace_request(
            &versions,
            &editor,
            "shoot.jpg",
            "image/jpeg",
            &reshot,
            "cropped",
        ),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::CREATED);

    // The orientation moved to the new version, so the geometry moved with it: a 4000×3000 frame
    // stored at orientation 8 is drawn as 3000 wide by 4000 high.
    assert_eq!(
        media_column(&fixture.state, id, "exif")
            .await
            .expect("the replaced file carries its own record")["orientation"],
        8
    );
    assert_eq!(media_int(&fixture.state, id, "width").await, 3000);
    assert_eq!(media_int(&fixture.state, id, "height").await, 4000);

    // A replacement in a format with no camera block *clears* the record. Keeping the previous
    // body's lens on a screenshot is a wrong fact, not a stale cache — and the geometry falls
    // back to what the replacement's own header says rather than the rotation it no longer has.
    let flattened = call(
        &fixture.state,
        replace_request(
            &versions,
            &editor,
            "shoot.png",
            "image/png",
            &png(1200, 630),
            "flattened",
        ),
    )
    .await;
    assert_eq!(flattened.status, StatusCode::CREATED);
    assert!(
        media_column(&fixture.state, id, "exif").await.is_none(),
        "a replacement with no camera block must clear the record"
    );
    assert_eq!(
        media_int(&fixture.state, id, "width").await,
        1200,
        "the rotation is gone, so the frame's own width stands"
    );
    assert_eq!(media_int(&fixture.state, id, "height").await, 630);

    // Restoring the first version brings its record back, read from its own bytes.
    let restored = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("{versions}/1/restore"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(restored.status, StatusCode::OK);
    let back = media_column(&fixture.state, id, "exif")
        .await
        .expect("a restore brings version 1's record back");
    assert_eq!(
        back["orientation"], 1,
        "the restored bytes carry their own record"
    );
    assert_eq!(media_int(&fixture.state, id, "width").await, 4000);

    fixture.cleanup().await;
}

/// A PNG of the given size, built header-only — the geometry probe reads the `IHDR` chunk.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes
}

/// The serve path answers a window, and the three answers a client can be given are all
/// distinguishable from the outside.
///
/// This is the layer that proves REQ-010's "video plays with range requests": the parser has
/// nineteen unit tests and would pass with the route never reading the header at all, which is
/// exactly the shape of a green test list around a feature that is not on the request path.
///
/// The body is compared as **bytes** against the object that was uploaded, not by length: a
/// length check passes by accident on an off-by-one, and the last byte of an object is precisely
/// where a window implementation loses one.
#[tokio::test]
async fn a_range_request_answers_a_window_and_says_what_it_sent() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.site_a;
    let editor = fixture.editor_token().await;
    let library = format!("/api/v1/media?site_id={site}");

    // 300 bytes of recognisable content, so a window can be checked against the source rather
    // than against a length: every byte has a known value at a known offset.
    let mut body = Vec::with_capacity(300);
    for index in 0..300u32 {
        body.push((index % 251) as u8);
    }
    let filename = "ranged-video.mp4";
    let upload = call(
        &fixture.state,
        upload_request(&library, Some(&editor), filename, "video/mp4", &body),
    )
    .await;
    assert_eq!(upload.status, StatusCode::CREATED, "body: {}", upload.body);
    let media_id = id_of(&upload.body);
    // `/media/{id}/raw` and not `/media/files/{id}/raw`: the file-manager read path is the preset
    // route, which falls through to the original bytes when no `?preset=` is named. The route
    // named `files` is the metadata route, and a 404 from it reads exactly like a broken serve
    // path — the failure would have named the media id rather than the path that was wrong.
    let raw = format!("/api/v1/media/{media_id}/raw");

    // No header at all: the whole object, and the platform still says ranges are supported.
    let whole = call(
        &fixture.state,
        request(Method::GET, &raw, Some(&editor), None),
    )
    .await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(
        whole.bytes, body,
        "no range is the whole object, byte for byte"
    );
    assert_eq!(
        whole.accept_ranges.as_deref(),
        Some("bytes"),
        "a client learns ranges work from the first response, not from a failed second one"
    );
    assert_eq!(
        whole.content_range, None,
        "a 200 has no Content-Range: it is not a range"
    );

    // A closed window in the middle: the bytes, the status and the header all agree.
    let middle = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=100-149"),
    )
    .await;
    assert_eq!(middle.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        middle.bytes,
        body[100..=149].to_vec(),
        "the window is the bytes at those offsets"
    );
    assert_eq!(
        middle.content_range.as_deref(),
        Some("bytes 100-149/300"),
        "the reported total is the object, the end is the last byte sent"
    );
    assert_eq!(middle.content_type.as_deref(), Some("video/mp4"));
    assert!(
        middle.nosniff,
        "a windowed answer is still a nosniff answer"
    );

    // An open window: everything from that offset to the end.
    let open = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=290-"),
    )
    .await;
    assert_eq!(open.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        open.bytes,
        body[290..].to_vec(),
        "the last ten bytes, compared as bytes"
    );
    assert_eq!(open.content_range.as_deref(), Some("bytes 290-299/300"));

    // A suffix window: the tail, counted from the end rather than named.
    let suffix = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=-10"),
    )
    .await;
    assert_eq!(suffix.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(suffix.bytes, body[290..].to_vec());
    assert_eq!(suffix.content_range.as_deref(), Some("bytes 290-299/300"));

    // A single byte at each end — the first request a player makes, and the last frame of a
    // video. An off-by-one loses one of them and neither is visible in a length check.
    let first = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=0-0"),
    )
    .await;
    assert_eq!(first.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(first.bytes, vec![body[0]]);
    assert_eq!(first.content_range.as_deref(), Some("bytes 0-0/300"));

    let last = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=299-299"),
    )
    .await;
    assert_eq!(last.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        last.bytes,
        vec![body[299]],
        "the final byte is inside the object"
    );
    assert_eq!(last.content_range.as_deref(), Some("bytes 299-299/300"));

    // An end past the object is clamped, not refused: that is a client that believes the file is
    // longer than it is, and answering 416 to it makes a player give up on a file it could play.
    let clamped = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=295-99999"),
    )
    .await;
    assert_eq!(clamped.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(clamped.bytes, body[295..].to_vec());
    assert_eq!(
        clamped.content_range.as_deref(),
        Some("bytes 295-299/300"),
        "the header reports the bytes that arrived, not the ones asked for"
    );

    // A window that starts past the end names nothing: a 416 with the real total, and no body.
    let past = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=5000-6000"),
    )
    .await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(
        past.bytes.is_empty(),
        "a 416 sends no body; its header is the whole answer"
    );
    assert_eq!(
        past.content_range.as_deref(),
        Some("bytes */300"),
        "a 416 tells the client how long the object really is"
    );
    assert_eq!(past.accept_ranges.as_deref(), Some("bytes"));

    // An unreadable range is *ignored*: the whole object, not a refusal. RFC 9110 §14.2, and the
    // only answer a client recovers from — a 416 here teaches a player that the file is broken.
    for unusable in ["items=0-9", "bytes=abc-def", "bytes=5-1", "bytes=-"] {
        let ignored = call(&fixture.state, range_request(&raw, Some(&editor), unusable)).await;
        assert_eq!(
            ignored.status,
            StatusCode::OK,
            "{unusable:?} must be ignored, not refused"
        );
        assert_eq!(ignored.bytes, body, "{unusable:?} is served whole");
        assert_eq!(
            ignored.content_range, None,
            "{unusable:?} produced a 200, which carries no Content-Range"
        );
    }

    // A multi-range request gets the whole object. Answering 416 to a legal request teaches the
    // client to stop asking; there is no multipart writer in this codebase to do it properly.
    let multi = call(
        &fixture.state,
        range_request(&raw, Some(&editor), "bytes=0-9,20-29"),
    )
    .await;
    assert_eq!(multi.status, StatusCode::OK);
    assert_eq!(
        multi.bytes, body,
        "a multi-range request is answered in full"
    );

    // The public path windows too, with no session at all: a published page's own video has to
    // seek, and the anonymous visitor is exactly who the read path exists for.
    let public = call(
        &fixture.state,
        range_request(
            &format!("/api/v1/public/media/{media_id}"),
            None,
            "bytes=10-19",
        ),
    )
    .await;
    assert_eq!(public.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(public.bytes, body[10..=19].to_vec());
    assert_eq!(public.content_range.as_deref(), Some("bytes 10-19/300"));

    // The old raw route answers the same way — the two read paths must not drift apart, and the
    // one the panel's own preview uses is the one nobody would notice breaking.
    let legacy = call(
        &fixture.state,
        range_request(
            &format!("/api/v1/media/{media_id}/raw"),
            Some(&editor),
            "bytes=0-9",
        ),
    )
    .await;
    assert_eq!(legacy.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(legacy.bytes, body[0..=9].to_vec());
    assert_eq!(legacy.content_range.as_deref(), Some("bytes 0-9/300"));

    // A range against the **preset** route answers the same way, because the derivative and the
    // original are the same file to a client: a page that asked for `?preset=card` and got the
    // original back still has to be able to seek in it.
    //
    // The preset named here is one that does **not** exist, which is the case that matters: the
    // route falls back to the original, and a fallback that dropped the range would leave a
    // client that renamed a preset unable to seek in the file it was already served. (Asking for
    // a preset that *does* exist on a video answers `422 not_transformable` — the transform runs
    // before the range is applied, which is the transform's own documented refusal and not this
    // criterion's subject.)
    let preset_raw = call(
        &fixture.state,
        range_request(
            &format!("/api/v1/media/{media_id}/raw?preset=no-such-preset"),
            Some(&editor),
            "bytes=0-9",
        ),
    )
    .await;
    assert_eq!(preset_raw.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        preset_raw.bytes,
        body[0..=9].to_vec(),
        "a window through the preset route's fallback is the same window"
    );
    assert_eq!(preset_raw.content_range.as_deref(), Some("bytes 0-9/300"));

    // And a range does not become a way around the serve gates: a reader without `media.read`
    // is refused exactly as before, window or not.
    let member = fixture.member_token().await;
    let refused = call(
        &fixture.state,
        range_request(&raw, Some(&member), "bytes=0-9"),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a range is not a second read path around the permission guard"
    );

    let anonymous = call(&fixture.state, range_request(&raw, None, "bytes=0-9")).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}
