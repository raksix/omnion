//! Integration tests for share links (REQ-010, slice 3).
//!
//! These walk the **real router**, because a share link's failure modes live between the route
//! and the row, not inside either: a token that is stored instead of hashed, a revocation that
//! leaves the link working, a counter that moves for a download that produced nothing, and a
//! link that keeps serving a file after the scanner flags it.
//!
//! Every assertion is against observable state — the response bytes, the row, the object store —
//! because a unit test on `servable` cannot see whether the route calls it.

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
/// `media.share` is here *beside* `media.manage` rather than inside it, and the reader below
/// holds neither — a team that may organise a library has no business handing files to the
/// outside world, and the reader is the account that proves it.
const EDITOR_PERMISSIONS: [&str; 6] = [
    "media.read",
    "media.upload",
    "media.delete",
    "media.update",
    "media.manage",
    "media.share",
];

/// The bytes a fixture file is made of. Distinct per test so a cross-test mix-up is visible.
const FILE_BYTES: &[u8] = b"omnion share link walkthrough bytes";

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    headers: Vec<(String, String)>,
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
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
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
        headers,
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
        .bind("Share Test")
        .bind(format!("media-share-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Share Site")
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
                key: format!("share-editor-{}", Uuid::new_v4().simple()),
                name: "Share Editor".to_owned(),
                description: "Drives the share links".to_owned(),
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

        // A reader: `media.read` and nothing else. It may see a file's links and may neither
        // create one nor revoke one — the power to hand a file to the outside world is not a
        // by-product of being able to open the file.
        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        let reader_role = role_store::create_role(
            db.pool(),
            NewRole {
                organization_id: org,
                key: format!("share-reader-{}", Uuid::new_v4().simple()),
                name: "Share Reader".to_owned(),
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

    /// Put real bytes in the library and return the media row's id.
    async fn upload(&self, site: Uuid, name: &str) -> Uuid {
        let media = omnion_media::insert_media(
            self.db.pool(),
            omnion_media::NewMedia {
                site_id: site,
                storage_key: format!("shares/{}/{name}", Uuid::new_v4().simple()),
                filename: name.to_owned(),
                content_type: "text/plain".to_owned(),
                size_bytes: FILE_BYTES.len() as i64,
                checksum: omnion_media::hash_token(name),
                created_by: None,
            },
        )
        .await
        .expect("the media row must be inserted");
        self.storage
            .put(&media.storage_key, FILE_BYTES, "text/plain")
            .await
            .expect("the object must be written");
        media.id
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
    let email = format!("share-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Share Test".to_owned(),
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

/// The public URL of a token, as a caller would reach it.
fn public_uri(token: &str) -> String {
    format!("/api/v1/public/media/shared/{token}")
}

/// A walk over one link: create it, use it, count it, revoke it, and watch it die.
#[tokio::test]
async fn a_link_serves_until_it_is_revoked_and_counts_only_real_downloads() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media_id = fixture.upload(site, "share-basic.txt").await;

    // Create. The response carries the token exactly once.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            Some(json!({ "expires_in_days": 7 })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let share_id = created.body["share"]["id"]
        .as_str()
        .expect("the create response names the share")
        .to_owned();
    let bearer = created.body["token"]
        .as_str()
        .expect("the create response carries the token once")
        .to_owned();
    assert_eq!(bearer.len(), 64, "32 random bytes, hex encoded");
    assert!(
        created.body["url"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&bearer),
        "the URL ends in the token: {:?}",
        created.body["url"]
    );

    // The row stores the *hash*, not the token. This is the property the whole feature rests
    // on, and it is checked against the database rather than against the response — a response
    // that hid the token would still leave a token in the row.
    let stored: String = sqlx::query_scalar("select token_hash from media_shares where id = $1")
        .bind(Uuid::parse_str(&share_id).expect("the share id is a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the share row must read");
    assert_eq!(stored, omnion_media::hash_token(&bearer));
    assert_ne!(stored, bearer, "the row never holds the token itself");

    // The list answers without the token: it is built from a type that has nowhere to put one.
    let listed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "body: {}", listed.body);
    let entry = &listed.body[0];
    assert_eq!(entry["id"], json!(share_id));
    assert_eq!(entry["state"], json!("live"));
    assert_eq!(entry["download_count"], json!(0));
    assert_eq!(entry["has_password"], json!(false));
    let raw = String::from_utf8_lossy(&listed.raw).to_lowercase();
    assert!(
        !raw.contains(&bearer.to_lowercase()),
        "the list must not carry the token"
    );
    assert!(
        !raw.contains("\"token\""),
        "the list has no field named token at all"
    );

    // It serves. The bytes are compared as bytes, not length-checked.
    let served = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(served.status, StatusCode::OK, "body: {}", served.body);
    assert_eq!(served.raw, FILE_BYTES, "the link returns the real bytes");
    let header_of = |name: &str| {
        served
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert!(
        header_of("cache-control").contains("no-store"),
        "a revoked link a proxy cached keeps working otherwise: {:?}",
        header_of("cache-control")
    );
    assert_eq!(header_of("x-content-type-options"), "nosniff");
    assert!(
        header_of("content-disposition").starts_with("attachment"),
        "a shared file is never rendered inline: {:?}",
        header_of("content-disposition")
    );

    // The counter moved, because bytes went out.
    let counted: i32 = sqlx::query_scalar("select download_count from media_shares where id = $1")
        .bind(Uuid::parse_str(&share_id).expect("uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the counter must read");
    assert_eq!(counted, 1, "one download that produced bytes");

    // A wrong token is refused, and answered as "gone" rather than 404 — the same answer as a
    // revoked link, so a caller cannot enumerate tokens by watching for a difference.
    let unknown = call(
        &fixture.state,
        request(Method::GET, &public_uri(&"f".repeat(64)), None, None),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::GONE, "body: {}", unknown.body);
    assert_eq!(unknown.body["error"]["code"], json!("unknown_token"));

    // Revoke.
    let revoked = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/{media_id}/shares/{share_id}"),
            Some(&token),
            Some(json!({ "reason": "sent to the wrong client" })),
        ),
    )
    .await;
    assert_eq!(
        revoked.status,
        StatusCode::NO_CONTENT,
        "body: {}",
        revoked.body
    );

    // Immediate: the very next request is refused, with no worker and no reaper in between.
    let after = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(after.status, StatusCode::GONE, "body: {}", after.body);
    assert_eq!(after.body["error"]["code"], json!("revoked"));

    // The row is *kept*, with the reason and the instant — a deleted row could not answer
    // "who held this link, and when did it stop working".
    let kept: (Option<time::OffsetDateTime>, String, i32) = sqlx::query_as(
        "select revoked_at, revoked_reason, download_count from media_shares where id = $1",
    )
    .bind(Uuid::parse_str(&share_id).expect("uuid"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("a revoked row must survive");
    assert!(kept.0.is_some(), "the revocation is recorded on the row");
    assert_eq!(kept.1, "sent to the wrong client");
    assert_eq!(kept.2, 1, "the download count survives the revocation");

    // The list shows it as revoked rather than hiding it: "this link was handed out and no
    // longer works" is a question the panel must be able to answer.
    let after_list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(after_list.body[0]["state"], json!("revoked"));
    assert_eq!(after_list.body[0]["download_count"], json!(1));

    // The audit trail carries both halves.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = any($1) and target_id = $2",
    )
    .bind(vec![
        "media.share_created".to_owned(),
        "media.share_revoked".to_owned(),
    ])
    .bind(media_id.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit log must read");
    assert_eq!(audited, 2, "creation and revocation are both audited");

    fixture.cleanup().await;
}

/// A password-protected link: the password is enforced, hashed, and the wrong one is refused
/// with the same answer as a missing one.
#[tokio::test]
async fn a_password_protected_link_checks_the_password_and_stores_its_hash() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media_id = fixture.upload(site, "share-locked.txt").await;
    let secret = "correct horse battery";

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            Some(json!({ "password": secret, "expires_in_days": 30 })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let share_id = created.body["share"]["id"].as_str().expect("id").to_owned();
    let bearer = created.body["token"].as_str().expect("token").to_owned();
    assert_eq!(created.body["share"]["has_password"], json!(true));

    // The password is stored as an Argon2id PHC string, never as what was typed.
    let stored: String = sqlx::query_scalar("select password_hash from media_shares where id = $1")
        .bind(Uuid::parse_str(&share_id).expect("uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the password hash must read");
    assert!(
        stored.starts_with("$argon2"),
        "a share password is low-entropy, so the work factor is the security: {stored}"
    );
    assert!(!stored.contains(secret), "the plaintext is never stored");

    // No password at all is refused.
    let bare = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(bare.status, StatusCode::FORBIDDEN, "body: {}", bare.body);
    assert_eq!(bare.body["error"]["code"], json!("password_required"));

    // A wrong password answers identically — a caller must not be able to tell "wrong" from
    // "none set", because the difference is only useful to somebody guessing the link.
    let wrong = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{}?password=not-the-password", public_uri(&bearer)),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(wrong.status, bare.status);
    assert_eq!(wrong.body["error"]["code"], bare.body["error"]["code"]);

    // The right one serves. The password is percent-encoded on the way in: a share password is
    // a human-chosen string and may contain anything, and a `+` or a space in a query string
    // is a real value the route has to receive correctly rather than a test artefact.
    let encoded: String = secret
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => ch.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect();
    let right = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("{}?password={encoded}", public_uri(&bearer)),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(right.status, StatusCode::OK, "body: {}", right.body);
    assert_eq!(right.raw, FILE_BYTES);

    fixture.cleanup().await;
}

/// An expired link, and the boundaries around the expiry field.
#[tokio::test]
async fn an_expiry_is_enforced_and_its_field_is_named_when_it_is_not() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media_id = fixture.upload(site, "share-expiry.txt").await;

    // A link that last zero days, and one that lasts a century, are both refused — and the
    // refusal names the field, because the message goes under that input on the screen.
    for days in [json!(0), json!(-3), json!(100_000)] {
        let refused = call(
            &fixture.state,
            request(
                Method::POST,
                &format!("/api/v1/media/{media_id}/shares"),
                Some(&token),
                Some(json!({ "expires_in_days": days })),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "days={days} body: {}",
            refused.body
        );
        assert_eq!(refused.body["error"]["code"], json!("expires_in_days"));
    }

    // A link created with a real expiry, then aged past it in the database — because waiting a
    // day for a test to expire is not a test.
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            Some(json!({ "expires_in_days": 1 })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let share_id = created.body["share"]["id"].as_str().expect("id").to_owned();
    let bearer = created.body["token"].as_str().expect("token").to_owned();

    let live = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK, "body: {}", live.body);

    // Age the link past its expiry. The `media_shares_expiry_sane` check refuses an expiry that
    // is not after `created_at`, so a past instant cannot be *stored* — the row ages by moving
    // `created_at` back instead, which is the only way to reach the same state and is why the
    // constraint is there: a link whose expiry precedes its own creation is nonsense, and a
    // test that reaches for one to simulate age is asking for a state the platform forbids.
    sqlx::query(
        "update media_shares set created_at = now() - interval '3 days', \
         expires_at = now() - interval '1 second' where id = $1",
    )
    .bind(Uuid::parse_str(&share_id).expect("uuid"))
    .execute(fixture.db.pool())
    .await
    .expect("the link must be aged past its expiry");

    let expired = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(expired.status, StatusCode::GONE, "body: {}", expired.body);
    assert_eq!(
        expired.body["error"]["code"],
        json!("expired"),
        "an expired link says so, so the holder asks for a new one rather than the owner"
    );

    fixture.cleanup().await;
}

/// A link reaches a file; it does not bypass what the file is.
#[tokio::test]
async fn a_link_stops_serving_when_its_file_stops_being_servable() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media_id = fixture.upload(site, "share-quarantine.txt").await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let bearer = created.body["token"].as_str().expect("token").to_owned();

    let live = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK);

    // The scanner flags the file *after* the link was made. The check lives at serve time for
    // exactly this reason: a link created yesterday must not keep serving a file the scanner
    // has since quarantined.
    sqlx::query("update media set scan_status = 'flagged' where id = $1")
        .bind(media_id)
        .execute(fixture.db.pool())
        .await
        .expect("the scan state must be written");

    let flagged = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(
        flagged.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        flagged.body
    );
    assert_eq!(flagged.body["error"]["code"], json!("file_unavailable"));

    // And the same for the trash.
    sqlx::query("update media set scan_status = 'clean', deleted_at = now() where id = $1")
        .bind(media_id)
        .execute(fixture.db.pool())
        .await
        .expect("the trash state must be written");
    let trashed = call(
        &fixture.state,
        request(Method::GET, &public_uri(&bearer), None, None),
    )
    .await;
    assert_eq!(
        trashed.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        trashed.body
    );
    assert_eq!(trashed.body["error"]["code"], json!("file_unavailable"));

    // A link over a file that is *already* in the trash is refused at creation, rather than
    // created and immediately useless.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "body: {}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], json!("file_in_trash"));

    fixture.cleanup().await;
}

/// The permission boundary and the tenant boundary, which are the same walk seen from two
/// sides: a reader may see a file's links and may not make one, and nobody reaches another
/// organization's file.
#[tokio::test]
async fn the_share_routes_are_permission_gated_and_tenant_scoped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let editor = fixture.editor_token().await;
    let reader = fixture.reader_token().await;
    let media_id = fixture.upload(site, "share-guarded.txt").await;

    // A reader may read the list (a file's links are part of the file) …
    let read = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);

    // … and may not create one, nor revoke one.
    let create = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        create.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        create.body
    );

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{media_id}/shares"),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.body
    );
    let share_id = created.body["share"]["id"].as_str().expect("id").to_owned();

    let revoke = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/{media_id}/shares/{share_id}"),
            Some(&reader),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        revoke.status,
        StatusCode::FORBIDDEN,
        "body: {}",
        revoke.body
    );

    // An anonymous caller may not list, and may not create.
    for (method, uri) in [
        (Method::GET, format!("/api/v1/media/{media_id}/shares")),
        (Method::POST, format!("/api/v1/media/{media_id}/shares")),
    ] {
        let anonymous = call(&fixture.state, request(method, &uri, None, Some(json!({})))).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{uri} body: {}",
            anonymous.body
        );
    }

    // A share id that belongs to a *different* file is a 404, not a 403: "it exists, but not
    // on this file" turns the route into an oracle for guessing ids.
    let other_file = fixture.upload(site, "share-other.txt").await;
    let crossed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/{other_file}/shares/{share_id}"),
            Some(&editor),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        crossed.status,
        StatusCode::NOT_FOUND,
        "body: {}",
        crossed.body
    );
    assert_eq!(crossed.body["error"]["code"], json!("share_not_found"));

    // A share of a file that does not exist at all is the same 404, and the token route is
    // the only route on this file that needs no session.
    let missing = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/media/{}/shares", Uuid::new_v4()),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    fixture.cleanup().await;
}
