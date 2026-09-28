//! Integration tests for per-site storage settings (REQ-010, slice 3).
//!
//! These walk the **real router**, because the interesting failures in this feature live between
//! the validator and the row: a value the form refuses and the database accepts, a partial save
//! that silently resets the fields it did not send, a connection test that reports the *saved*
//! configuration rather than the one on screen, and a settings response that leaks a credential.
//!
//! Every assertion is against observable state — the response body, the row, the object store —
//! because a unit test on `validate_new` cannot see whether the route calls it.

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
///
/// Clamped to two connections for the same reason the transformation suite clamps them: this
/// suite runs beside it, and a suite that starves its own pool reports `PoolTimedOut` out of
/// `seed::ensure`, which reads as a broken IAM seed.
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
        .bind("Storage Test")
        .bind(format!("media-storage-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Storage Site")
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
                description: "Drives the storage settings".to_owned(),
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

        // A reader: `media.read` and nothing else, so the write and the test must refuse it.
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
    let email = format!("storage-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Storage Test".to_owned(),
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

/// The settings route for a site, with its scope query.
fn settings_uri(site: Uuid) -> String {
    format!("/api/v1/media/settings?site_id={site}")
}

/// A walk over the storage settings: read, save, refuse, prove.
#[tokio::test]
async fn storage_settings_round_trip_and_refuse() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // A brand-new site has a row, thanks to the trigger — not a row the GET invents. This is
    // the same gap the preset seed had (`0028`), and the reason it is checked here rather than
    // trusted: a site with no row gets an answer the form can edit, and the edit silently does
    // nothing.
    let seeded: Option<(String, String, i32)> = sqlx::query_as(
        "select driver, bucket, max_upload_mb from media_storage_settings where site_id = $1",
    )
    .bind(site)
    .fetch_optional(fixture.db.pool())
    .await
    .expect("the settings row must read");
    let (driver, bucket, max_upload_mb) =
        seeded.expect("a site created now must have a storage settings row");
    assert_eq!(driver, "s3", "the seeded driver is the platform's own");
    assert!(!bucket.is_empty());
    assert_eq!(max_upload_mb, 25, "the seeded ceiling is the platform default");

    // Reading it answers the platform defaults and says it has never been configured.
    let read = call(
        &fixture.state,
        request(Method::GET, &settings_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);
    assert_eq!(read.body["bucket"], json!(bucket));
    assert_eq!(read.body["default_visibility"], json!("private"));
    assert_eq!(read.body["configured"], json!(false));
    assert_eq!(read.body["max_upload_mb"], json!(25));

    // A response that carried a credential would be a settings screen rendering one. The check
    // is over the *raw bytes*, not over a list of fields somebody remembered to check.
    let raw = String::from_utf8_lossy(&read.raw).to_lowercase();
    for forbidden in [
        "secret",
        "access_key",
        "password",
        "credential",
        "session",
    ] {
        assert!(
            !raw.contains(forbidden),
            "the settings response must never carry a `{forbidden}` field, got: {raw}"
        );
    }

    // A save with real values, and the row must read back as written.
    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(site),
            Some(&token),
            Some(json!({
                "path_prefix": "tenant-a",
                "public_base_url": "https://cdn.example.com/media/",
                "signed_url_ttl_seconds": 1800,
                "max_upload_mb": 64,
                "default_visibility": "private",
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    // The trailing slash is dropped on the way in, because the URL is built by joining a path
    // onto it and a doubled slash is a 404 on somebody else's CDN.
    assert_eq!(saved.body["public_base_url"], json!("https://cdn.example.com/media"));
    assert_eq!(saved.body["path_prefix"], json!("tenant-a"));
    assert_eq!(saved.body["signed_url_ttl_seconds"], json!(1800));
    assert_eq!(saved.body["max_upload_mb"], json!(64));

    // A *partial* save must not reset what it did not send. This is the bug a form with a
    // hidden input causes, and it is invisible until a site stops serving its files.
    let partial = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(site),
            Some(&token),
            Some(json!({ "max_upload_mb": 32 })),
        ),
    )
    .await;
    assert_eq!(partial.status, StatusCode::OK, "body: {}", partial.body);
    assert_eq!(partial.body["max_upload_mb"], json!(32));
    assert_eq!(
        partial.body["public_base_url"],
        json!("https://cdn.example.com/media"),
        "a partial save must not reset the fields it did not send"
    );
    assert_eq!(partial.body["path_prefix"], json!("tenant-a"));
    assert_eq!(partial.body["signed_url_ttl_seconds"], json!(1800));

    // Every out-of-range value names its own field, and a save and a connection test of the
    // same bad value agree about which field is wrong.
    let cases: [(Value, &str); 5] = [
        (json!({ "signed_url_ttl_seconds": 30 }), "signed_url_ttl_seconds"),
        (json!({ "max_upload_mb": 0 }), "max_upload_mb"),
        (json!({ "max_upload_mb": 2000 }), "max_upload_mb"),
        (json!({ "bucket": "UPPERCASE" }), "bucket"),
        (
            json!({ "endpoint": "https://s3.example.com/bucket" }),
            "endpoint",
        ),
    ];
    for (payload, field) in cases {
        let save = call(
            &fixture.state,
            request(Method::PUT, &settings_uri(site), Some(&token), Some(payload.clone())),
        )
        .await;
        assert_eq!(
            save.status,
            StatusCode::BAD_REQUEST,
            "the save must refuse {payload}"
        );
        assert_eq!(
            save.body["error"]["details"]["field"],
            json!(field),
            "the save must name `{field}` for {payload}, got: {}",
            save.body
        );

        let test = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/media/settings/test-connection?site_id={site}"),
                Some(&token),
                Some(payload.clone()),
            ),
        )
        .await;
        assert_eq!(
            test.status,
            StatusCode::BAD_REQUEST,
            "the connection test must refuse {payload} too"
        );
        assert_eq!(
            test.body["error"]["details"]["field"],
            json!(field),
            "the test and the save must agree about `{field}`, got: {}",
            test.body
        );
    }

    // A refused value must not have been written: the row still holds the last good save.
    let after_refusals = call(
        &fixture.state,
        request(Method::GET, &settings_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(after_refusals.body["max_upload_mb"], json!(32));
    assert_eq!(after_refusals.body["bucket"], json!(bucket));

    // The connection test writes. It is the only claim here that reaches the network, and it
    // must clean up after itself: a probe that leaves a marker behind fills a bucket with one
    // small object per click.
    let probe = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/settings/test-connection?site_id={site}"),
            Some(&token),
            // The *candidate* is posted, so the answer describes the form rather than the row —
            // here the stored bucket, pointed at a store the test process can actually reach.
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(probe.status, StatusCode::OK, "body: {}", probe.body);
    let ok = probe.body["ok"].as_bool().expect("the probe answers a boolean");
    let detail = probe.body["detail"].as_str().expect("the probe says what it proved");
    if ok {
        assert!(
            detail.contains("wrote"),
            "a passing probe must say it wrote, not merely that it read: got `{detail}`"
        );
        assert!(
            !detail.contains("connection-test"),
            "the detail must not be the marker key: got `{detail}`"
        );
        // The marker is gone. The prefix is part of the posted candidate, so the key is built
        // the same way a real file's key would be — which is what makes this a real check
        // rather than a lookup of a key nobody would ever use.
        let marker = format!("tenant-a/sites/{site}/connection-test");
        assert_eq!(
            fixture.storage.get(&marker).await.err().is_some(),
            true,
            "the probe object must be removed, `{marker}` is still there"
        );
    } else {
        // A store that is not writable is a legitimate answer, and it must say *which* part
        // failed — "reached but could not write" and "could not open" are different problems.
        assert!(
            detail.contains("could not") || detail.contains("reach"),
            "a failing probe must say what failed, got `{detail}`"
        );
        eprintln!("note: the connection probe could not reach the store ({detail})");
    }

    // A reader may read the settings and may not write them: knowing the upload ceiling is not
    // a secret, and repointing the bucket is not a reader's power.
    let reader = fixture.reader_token().await;
    let reader_read = call(
        &fixture.state,
        request(Method::GET, &settings_uri(site), Some(&reader), None),
    )
    .await;
    assert_eq!(reader_read.status, StatusCode::OK);

    for (method, uri) in [
        (Method::PUT, settings_uri(site)),
        (
            Method::POST,
            format!("/api/v1/media/settings/test-connection?site_id={site}"),
        ),
    ] {
        let refused = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&reader), Some(json!({}))),
        )
        .await;
        assert!(
            refused.status == StatusCode::FORBIDDEN || refused.status == StatusCode::UNAUTHORIZED,
            "a reader must not {method} the storage settings, got {}",
            refused.status
        );
    }

    // And nobody at all is refused both.
    for (method, uri) in [
        (Method::GET, settings_uri(site)),
        (Method::PUT, settings_uri(site)),
        (
            Method::POST,
            format!("/api/v1/media/settings/test-connection?site_id={site}"),
        ),
    ] {
        let anonymous = call(&fixture.state, request(method.clone(), &uri, None, None)).await;
        assert!(
            anonymous.status == StatusCode::UNAUTHORIZED,
            "an anonymous {method} must be refused, got {}",
            anonymous.status
        );
    }

    // The save is audited, naming the fields that changed rather than repeating the whole body.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log \
         where action = 'media.storage_updated' \
           and metadata ->> 'site_id' = $1",
    )
    .bind(site.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit log must read");
    assert!(
        audited >= 2,
        "both the full save and the partial one are settings changes, found {audited}"
    );

    fixture.cleanup().await;
}

/// The settings row a site is given must be editable on the same record the form reads.
///
/// A `GET` that builds a default in Rust when no row is found is a *pleasant* answer and a
/// useless one: the form then saves to a row the `GET` never read, and an operator who typed
/// one field and saved would find the other nine back at their defaults.
#[tokio::test]
async fn a_site_created_after_the_migration_is_editable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // The trigger fires on the site's own insert, so this row exists without anybody asking.
    let exists: bool = sqlx::query_scalar(
        "select exists(select 1 from media_storage_settings where site_id = $1)",
    )
    .bind(site)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the existence check must read");
    assert!(exists, "a site created after the migration must have a settings row");

    let saved = call(
        &fixture.state,
        request(
            Method::PUT,
            &settings_uri(site),
            Some(&token),
            Some(json!({ "max_upload_mb": 128 })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);

    let read = call(
        &fixture.state,
        request(Method::GET, &settings_uri(site), Some(&token), None),
    )
    .await;
    assert_eq!(
        read.body["max_upload_mb"],
        json!(128),
        "a value written to the row must come back out of the row"
    );
    assert_eq!(read.body["configured"], json!(true));

    fixture.cleanup().await;
}
