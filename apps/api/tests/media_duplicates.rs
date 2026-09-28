//! Integration tests for duplicate detection and merge (REQ-010, slice 3).
//!
//! These walk the **real router**, because a merge's failure modes live between the route and
//! the rows, not inside either: a keeper chosen by the platform instead of the operator, a
//! repoint that collides with the keeper's own reference rows and rolls the whole merge back,
//! a report that claims the bytes are already reclaimed, and a cross-site mode a tenant account
//! can reach.
//!
//! Every assertion is against observable state — the response body, the rows, the audit log —
//! because a unit test on `MergeBody::build` cannot see whether the route calls it.

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

/// What the editor of this suite holds: it may organise a library and hand a file out. Merging is
/// `media.manage` rather than a new key, because a merge does not hand a file to the outside
/// world — it rewrites which row this site's own pages resolve to. `media.share` is here only
/// because one walk creates a link to prove the merge closes it.
const EDITOR_PERMISSIONS: [&str; 5] = [
    "media.read",
    "media.manage",
    "media.delete",
    "media.upload",
    "media.share",
];

/// What the reader holds: the report and nothing else. A team that may see what a site stores
/// twice may not merge it — reading a storage profile costs nothing, rewriting what a published
/// page points at is a different power.
const READER_PERMISSIONS: [&str; 1] = ["media.read"];

/// The bytes a fixture file is made of.
const FILE_BYTES: &[u8] = b"omnion duplicate walkthrough bytes";

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
    /// Kept for the shape of a refusal, which is sometimes a plain string rather than JSON.
    #[allow(dead_code)]
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

/// Everything one walk needs: an organization, two sites, an editor and a reader.
struct Fixture {
    state: AppState,
    db: Db,
    storage: Storage,
    editor_email: String,
    reader_email: String,
    platform_email: String,
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
        .bind("Duplicate Test")
        .bind(format!("media-dup-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let mut sites = Vec::new();
        for key in ["main", "second"] {
            let site = sqlx::query_scalar::<_, Uuid>(
                "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
            )
            .bind(org)
            .bind(key)
            .bind(format!("Duplicate Site {key}"))
            .fetch_one(db.pool())
            .await
            .expect("the site must be created");
            sites.push(site);
        }

        let (platform_id, platform_email) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        grant_role(&db, org, &EDITOR_PERMISSIONS, "dup-editor", editor_id, platform_id).await;

        let (reader_id, reader_email) = create_account(&db, Some(org)).await;
        grant_role(&db, org, &READER_PERMISSIONS, "dup-reader", reader_id, platform_id).await;

        Some(Self {
            state,
            db,
            storage,
            editor_email,
            reader_email,
            platform_email,
            accounts: vec![platform_id, editor_id, reader_id],
            organizations: vec![org],
            sites,
        })
    }

    async fn editor_token(&self) -> String {
        login(&self.state, &self.editor_email).await
    }

    async fn reader_token(&self) -> String {
        login(&self.state, &self.reader_email).await
    }

    async fn platform_token(&self) -> String {
        login(&self.state, &self.platform_email).await
    }

    /// Put real bytes in the library with *this* checksum, so a group is real rather than
    /// hand-written: the checksum is what the report groups by, and a fixture that lies about it
    /// would prove nothing about the grouping.
    async fn upload_with(
        &self,
        site: Uuid,
        name: &str,
        checksum: &str,
        size: i64,
    ) -> Uuid {
        let media = omnion_media::insert_media(
            self.db.pool(),
            omnion_media::NewMedia {
                site_id: site,
                storage_key: format!("dupes/{}/{name}", Uuid::new_v4().simple()),
                filename: name.to_owned(),
                content_type: "text/plain".to_owned(),
                size_bytes: size,
                checksum: checksum.to_owned(),
                created_by: None,
            },
        )
        .await
        .expect("the media row must be inserted");
        self.storage
            .put(
                &media.storage_key,
                &FILE_BYTES[..size.min(FILE_BYTES.len() as i64) as usize],
                "text/plain",
            )
            .await
            .expect("the object must be written");
        media.id
    }

    /// The checksums two "uploads of the same file" produce — the real SHA-256 of the bytes.
    fn checksum_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        digest.iter().map(|b| format!("{b:02x}")).collect()
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
    let email = format!("dup-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Duplicate Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Create a role with the given permissions and bind one account to it.
async fn grant_role(
    db: &Db,
    organization_id: Uuid,
    permissions: &[&str],
    key_prefix: &str,
    user_id: Uuid,
    granted_by: Uuid,
) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("{key_prefix}-{}", Uuid::new_v4().simple()),
            name: key_prefix.to_owned(),
            description: "Drives the duplicate report".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");
    let entries: Vec<RolePermissionInput> = permissions
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
            user_id,
            scope: Scope::Organization { organization_id },
            granted_by: Some(granted_by),
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
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

/// The report for one site.
async fn report(state: &AppState, token: &str, site: Uuid, expand: bool) -> TestResponse {
    let uri = if expand {
        format!("/api/v1/media/duplicates?site_id={site}&expand=1")
    } else {
        format!("/api/v1/media/duplicates?site_id={site}")
    };
    call(state, request(Method::GET, &uri, Some(token), None)).await
}

/// A group of two identical files, plus one file that is not a duplicate of anything.
async fn seed_pair(fixture: &Fixture, site: Uuid, bytes: &[u8]) -> (Uuid, Uuid, String) {
    let checksum = Fixture::checksum_of(bytes);
    let size = bytes.len() as i64;
    let a = fixture
        .upload_with(site, "dupe-a.txt", &checksum, size)
        .await;
    let b = fixture
        .upload_with(site, "dupe-b.txt", &checksum, size)
        .await;
    (a, b, checksum)
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The report: a group of two appears, a lone file does not, a trashed copy does not, and the
/// reclaimable column is the group minus one copy.
#[tokio::test]
async fn the_report_groups_by_checksum_and_counts_only_the_waste() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let bytes = b"identical bytes for a duplicate group";
    let (_, _, checksum) = seed_pair(&fixture, site, bytes).await;

    // A file that shares nothing, and a third copy that is in the trash. The trashed one is the
    // interesting case: it is the same bytes, and it is *not* in the report, because the report
    // is about what the site is holding, not about what the trash is about to release.
    let other = Fixture::checksum_of(b"a completely different file");
    fixture.upload_with(site, "unique.txt", &other, 7).await;
    let trashed = fixture.upload_with(site, "trashed.txt", &checksum, bytes.len() as i64).await;
    sqlx::query("update media set deleted_at = now() where id = $1")
        .bind(trashed)
        .execute(fixture.db.pool())
        .await
        .expect("the third copy must be trashed");

    let body = report(&fixture.state, &token, site, true).await;
    assert_eq!(body.status, StatusCode::OK, "body: {}", body.body);
    assert_eq!(body.body["cross_site"], json!(false));
    assert_eq!(
        body.body["group_count"],
        json!(1),
        "exactly one group: a unique file is not a group, and a trashed copy is not a duplicate. \
         body: {}",
        body.body
    );

    let group = &body.body["groups"][0];
    assert_eq!(group["checksum"], json!(checksum[..16].to_owned() + "…"));
    assert_eq!(group["full_checksum"], json!(checksum));
    assert_eq!(group["file_count"], json!(2), "the trashed copy is not counted");
    assert_eq!(
        group["total_bytes"],
        json!(bytes.len() as i64 * 2),
        "both copies' bytes"
    );
    assert_eq!(
        group["reclaimable_bytes"],
        json!(bytes.len() as i64),
        "the waste is the group minus one keeper, not the group"
    );

    let files = group["files"].as_array().expect("expand was asked for");
    assert_eq!(files.len(), 2, "the expansion lists the group's members");
    for file in files {
        assert_eq!(file["reference_count"], json!(0), "nothing uses them yet");
        assert!(file["raw_path"].as_str().expect("raw path").contains("raw"));
    }

    // The headline matches the rows: it is a sum, not a second computation.
    assert_eq!(
        body.body["reclaimable_bytes"],
        json!(group["reclaimable_bytes"].as_i64().expect("bytes")),
    );

    // A different site sees nothing: the group is per-site, because two tenants storing the same
    // bytes is not a duplicate any of them can act on.
    let other_site = report(&fixture.state, &token, fixture.sites[1], false).await;
    assert_eq!(other_site.body["group_count"], json!(0), "another site is clean");

    fixture.cleanup().await;
}

/// The merge: the caller names the keeper, references move, the copy is trashed and the group is
/// gone.
#[tokio::test]
async fn a_merge_keeps_the_chosen_file_and_repoints_every_reference() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let bytes = b"identical bytes for the merge walk";
    let (a, b, checksum) = seed_pair(&fixture, site, bytes).await;

    // Two records use the copies. One of them — page-1's hero — points at *both*, which is the
    // case the repoint has to get right: a plain `update` collides with the keeper's own row.
    for (media_id, resource) in [(a, "page-1"), (b, "page-1"), (b, "page-2")] {
        omnion_media::record_reference(
            fixture.db.pool(),
            &omnion_media::NewReference {
                media_id,
                resource_kind: "page".to_owned(),
                resource_id: resource.to_owned(),
                field: "hero_image_id".to_owned(),
            },
        )
        .await
        .expect("the reference must be recorded");
    }

    // Keep the *second* file, so the walk cannot pass because it happened to keep the older one.
    let merged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&token),
            Some(json!({
                "site_id": site,
                "checksum": checksum,
                "keep": b,
            })),
        ),
    )
    .await;
    assert_eq!(merged.status, StatusCode::OK, "body: {}", merged.body);
    assert_eq!(merged.body["kept"], json!(b.to_string()));
    assert_eq!(merged.body["trashed"], json!([a.to_string()]));
    assert_eq!(
        merged.body["bytes_pending_purge"],
        json!(bytes.len() as i64),
        "the copy's bytes are pending, not reclaimed"
    );
    assert!(
        merged.body["notice"]
            .as_str()
            .expect("a notice")
            .contains("only reclaimed when the trash is purged"),
        "the notice must not claim the space is back: {}",
        merged.body["notice"]
    );

    // The keeper is live, the copy is trashed — not deleted, so a restore reverses the whole
    // operation.
    let states: Vec<(String, bool)> = sqlx::query_as(
        "select filename, deleted_at is null from media where id = any($1) order by filename",
    )
    .bind([a, b])
    .fetch_all(fixture.db.pool())
    .await
    .expect("the rows must read");
    assert_eq!(states.len(), 2);
    assert!(
        states.iter().any(|(name, live)| name == "dupe-a.txt" && !live),
        "the copy is in the trash: {states:?}"
    );
    assert!(
        states.iter().any(|(name, live)| name == "dupe-b.txt" && *live),
        "the keeper is live: {states:?}"
    );

    // Every reference now resolves to the keeper, and page-1's hero appears *once* — the
    // duplicate row collapsed rather than tripping the unique index.
    let on_keeper: Vec<(String, String)> = sqlx::query_as(
        "select resource_id, field from media_references where media_id = $1 order by resource_id",
    )
    .bind(b)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the references must read");
    assert_eq!(
        on_keeper,
        vec![
            ("page-1".to_owned(), "hero_image_id".to_owned()),
            ("page-2".to_owned(), "hero_image_id".to_owned()),
        ],
        "both records now point at the keeper, and page-1 is listed once"
    );
    let on_copy: i64 =
        sqlx::query_scalar("select count(*) from media_references where media_id = $1")
            .bind(a)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(on_copy, 0, "nothing points at the trashed copy");

    // The report agrees: one live file is not a group.
    let after = report(&fixture.state, &token, site, false).await;
    assert_eq!(after.body["group_count"], json!(0), "body: {}", after.body);

    // The audit trail records the merge and both halves of it.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'media.duplicate_merged' and target_id = $1",
    )
    .bind(b.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit log must read");
    assert_eq!(audited, 1, "the merge is audited against the file it kept");

    fixture.cleanup().await;
}

/// A share over a copy stops reaching the file, and the merge says it closed it.
#[tokio::test]
async fn a_merge_closes_the_share_links_over_the_copies() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let bytes = b"a duplicated file that somebody shared";
    let (a, b, checksum) = seed_pair(&fixture, site, bytes).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/{a}/shares"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let bearer = created.body["token"].as_str().expect("token").to_owned();

    let merged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&token),
            Some(json!({ "site_id": site, "checksum": checksum, "keep": b })),
        ),
    )
    .await;
    assert_eq!(merged.status, StatusCode::OK, "body: {}", merged.body);
    assert_eq!(
        merged.body["shares_revoked"],
        json!(1),
        "the link somebody is still holding is closed"
    );

    // The link is gone, and it says why — the holder of a dead link is the person who needs the
    // reason most.
    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/public/media/shared/{bearer}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(after.status, StatusCode::GONE, "body: {}", after.body);
    assert_eq!(after.body["error"]["code"], json!("revoked"));

    fixture.cleanup().await;
}

/// The refusals: a keeper from outside the group, a group that no longer exists, a short
/// checksum, and a caller without `media.manage`.
#[tokio::test]
async fn a_merge_refuses_what_it_must_and_names_the_field() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let reader = fixture.reader_token().await;
    let bytes = b"identical bytes for the refusal walk";
    let (a, b, checksum) = seed_pair(&fixture, site, bytes).await;
    let stranger = fixture
        .upload_with(site, "stranger.txt", &Fixture::checksum_of(b"other"), 5)
        .await;

    // A keeper that is not in the group. Merging anyway would repoint every reference onto a
    // file with *different bytes*, which is the failure this rule exists to prevent.
    let outside = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&token),
            Some(json!({ "site_id": site, "checksum": checksum, "keep": stranger })),
        ),
    )
    .await;
    assert_eq!(outside.status, StatusCode::CONFLICT, "body: {}", outside.body);
    assert_eq!(
        outside.body["error"]["code"],
        json!("duplicate_merge_refused"),
        "a 409, not a 400: the request was legal and the library moved on"
    );
    assert!(
        outside.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not one of the files"),
        "{}",
        outside.body["error"]["message"]
    );

    // A short checksum: the report hands the full value, and a truncated one would silently
    // return an empty report that reads as "no duplicates".
    for short in ["abc", &checksum[..8]] {
        let refused = call(
            &fixture.state,
            request(
                Method::POST,
                "/api/v1/media/duplicates/merge",
                Some(&token),
                Some(json!({ "site_id": site, "checksum": short, "keep": a })),
            ),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "checksum={short} body: {}",
            refused.body
        );
        assert_eq!(refused.body["error"]["code"], json!("checksum"));
    }

    // A group that has been merged already: two live files are required, so a second click is a
    // conflict rather than a merge of one file with itself.
    let first = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&token),
            Some(json!({ "site_id": site, "checksum": checksum, "keep": a })),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "body: {}", first.body);
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&token),
            Some(json!({ "site_id": site, "checksum": checksum, "keep": b })),
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "the group is gone: {}",
        second.body
    );

    // A reader may read the report and may not merge it.
    let seen = report(&fixture.state, &reader, site, false).await;
    assert_eq!(seen.status, StatusCode::OK, "a reader may look: {}", seen.body);
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/media/duplicates/merge",
            Some(&reader),
            Some(json!({ "site_id": site, "checksum": checksum, "keep": a })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "body: {}", refused.body);

    // No session at all is refused on both halves.
    let anonymous = call(
        &fixture.state,
        request(Method::GET, &format!("/api/v1/media/duplicates?site_id={site}"), None, None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    fixture.cleanup().await;
}

/// The cross-site report is the platform's, and a tenant account cannot reach it — even though it
/// holds every permission the report needs.
#[tokio::test]
async fn the_cross_site_report_belongs_to_the_platform_and_nobody_else() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let [first, second] = [fixture.sites[0], fixture.sites[1]];
    let bytes = b"the same bytes in two tenants";
    let checksum = Fixture::checksum_of(bytes);

    // The same bytes in two sites. Two tenants storing one file is not a duplicate either of them
    // can merge — a merge would move one tenant's pages onto another tenant's row.
    for (site, name) in [(first, "tenant-a.txt"), (second, "tenant-b.txt")] {
        fixture
            .upload_with(site, name, &checksum, bytes.len() as i64)
            .await;
    }

    let editor = fixture.editor_token().await;
    let uri = format!("/api/v1/media/duplicates?sites={first},{second}");

    // The tenant account is refused. `platform_only` runs *before* any row is read, so this is
    // not "the rows were filtered" — it is "the question was not answered for you".
    let refused = call(
        &fixture.state,
        request(Method::GET, &uri, Some(&editor), None),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "body: {}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("platform_only"));

    // The platform owner sees both groups, named, each with one file — which is the honest
    // answer: "this installation stores these bytes in two places", not "merge them".
    let owner = fixture.platform_token().await;
    let seen = call(&fixture.state, request(Method::GET, &uri, Some(&owner), None)).await;
    assert_eq!(seen.status, StatusCode::OK, "body: {}", seen.body);
    assert_eq!(seen.body["cross_site"], json!(true));
    // One checksum, two sites — which is the whole point of the mode. The per-site reports above
    // showed *nothing*: each tenant holds exactly one copy, so neither of them has a duplicate
    // it can act on. Only the platform can see that the same bytes sit in two places at once.
    assert_eq!(
        seen.body["group_count"],
        json!(1),
        "the same bytes in two tenants is one group across two sites: {}",
        seen.body
    );
    let group = &seen.body["groups"][0];
    assert_eq!(group["full_checksum"], json!(checksum));
    assert_eq!(group["file_count"], json!(2));
    assert_eq!(group["site_count"], json!(2));
    assert_eq!(group["total_bytes"], json!(bytes.len() as i64 * 2));

    // Both sites are named, and neither is missing.
    let sites = group["sites"].as_array().expect("the copies are listed");
    assert_eq!(sites.len(), 2, "both copies are located: {sites:?}");
    for copy in sites {
        assert!(
            copy["site_name"].as_str().is_some_and(|name| !name.is_empty()),
            "a copy names its site: {copy}"
        );
        assert_eq!(copy["file_count"], json!(1));
    }

    // No "reclaimable" column at all, rather than a zero: there is no merge that can act on
    // this, and a number with no button behind it would be quoted as a saving.
    assert!(
        group.get("reclaimable_bytes").is_none(),
        "the cross-site report offers no reclaimable number: {group}"
    );
    assert!(
        seen.body["notice"]
            .as_str()
            .unwrap_or_default()
            .contains("per-site decision"),
        "the report says where the action is: {}",
        seen.body["notice"]
    );

    // A site list that is not site ids is a `400` naming the field, not an empty report.
    let bad = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/media/duplicates?sites=not-a-uuid",
            Some(&owner),
            None,
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "body: {}", bad.body);

    // And no account at all.
    let anonymous = call(&fixture.state, request(Method::GET, &uri, None, None)).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    fixture.cleanup().await;
}

/// The "used in" question the reference rows answer, and its own duplicate-suppression rule.
#[tokio::test]
async fn a_reference_is_counted_once_per_record_and_the_file_says_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let bytes = b"a file two pages point at";
    let media_id = fixture
        .upload_with(site, "shared.png", &Fixture::checksum_of(bytes), bytes.len() as i64)
        .await;

    // The same record naming the same file twice is one usage, not two — a page with a hero on
    // the list card and the same hero on the detail page is one page.
    for field in ["hero_image_id", "og_image_id"] {
        omnion_media::record_reference(
            fixture.db.pool(),
            &omnion_media::NewReference {
                media_id,
                resource_kind: "page".to_owned(),
                resource_id: "page-home".to_owned(),
                field: field.to_owned(),
            },
        )
        .await
        .expect("the reference must be recorded");
    }
    // And recording the identical row again must not raise or duplicate it.
    omnion_media::record_reference(
        fixture.db.pool(),
        &omnion_media::NewReference {
            media_id,
            resource_kind: "page".to_owned(),
            resource_id: "page-home".to_owned(),
            field: "hero_image_id".to_owned(),
        },
    )
    .await
    .expect("a repeated record is a no-op");

    let counted = omnion_media::count_references(fixture.db.pool(), media_id)
        .await
        .expect("the count must read");
    assert_eq!(
        counted, 1,
        "two fields of one page is one usage; a 'used in 2 places' line for one page makes \
         somebody delete a page"
    );

    // An empty kind or id is refused rather than stored as a blank row nobody can find again.
    for (kind, id) in [("", "page-1"), ("page", "  ")] {
        let refused = omnion_media::record_reference(
            fixture.db.pool(),
            &omnion_media::NewReference {
                media_id,
                resource_kind: kind.to_owned(),
                resource_id: id.to_owned(),
                field: "hero".to_owned(),
            },
        )
        .await;
        assert!(refused.is_err(), "kind={kind:?} id={id:?} must be refused");
    }

    let listed = omnion_media::list_references(fixture.db.pool(), media_id)
        .await
        .expect("the list must read");
    assert_eq!(listed.len(), 2, "both fields are still listed");

    fixture.cleanup().await;
}
