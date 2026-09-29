//! Integration tests for a file's usage and its activity (REQ-010, slice 4).
//!
//! These walk the **real router**, because both reads are compositions and a composition is only
//! broken between its parts: a usage endpoint that resolves a referent's title from the wrong
//! revision, or an activity read whose target filter drops the very rows it exists for, both
//! answer `200` with a plausible body.
//!
//! Five things are walked here, and each exists because the shortcut produces a plausible wrong
//! answer:
//!
//! * **the two numbers are different numbers.** A page naming one hero in three fields is *one*
//!   record and three rows. A screen that reported "used in 3 places" beside one page is the
//!   number that makes somebody delete a page, so `records` and `rows` are asserted separately.
//! * **a reference whose record is gone is reported, not dropped.** It refuses a purge for ever,
//!   so the reader has to be able to see *which* row is stale and what to do about it. Dropping
//!   it would make the list shorten every week with no way to tell a quiet file from a broken one.
//! * **the referent's title comes from the published revision when there is one.** Taking the
//!   newest revision instead shows an editor's draft title on a list meant to answer "which
//!   published pages use this", and taking no revision at all drops a brand-new page from a list
//!   it has already been added to.
//! * **a file's story includes its access changes.** A grant is the most consequential thing
//!   this library records — it decides who may fetch the file at all — and it is written under
//!   `media_file` while the bytes are written under `media`. A filter naming only one of them
//!   answers "who could see this file in March" with a list of uploads.
//! * **a trashed file still has a story.** "What happened to this" is asked *after* the deletion,
//!   so a 404 there answers a blank page to the one person who needs the trail.

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
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// What the editor of this suite holds.
///
/// The three read/write keys are here only so the walks can *make* something happen. The claim
/// both screens rest on — that they are plain reads — is proved by the reader account below,
/// which holds `media.read` and nothing else. An editor holding the write keys is what makes
/// that a comparison rather than an assumption.
const EDITOR_PERMISSIONS: [&str; 5] = [
    "media.read",
    "media.upload",
    "media.share",
    // The repair scan is what the usage screen's stale-reference sentence promises, so the walk
    // that reads that sentence has to be able to run it — otherwise the promise is untested.
    "media.settings.manage",
    "media.delete",
];

/// The bytes a fixture file is made of. Distinct per test so a cross-test mix-up is visible.
const FILE_BYTES: &[u8] = b"omnion usage and activity walkthrough bytes";

/// The pieces of one in-process response the assertions need.
struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
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

// ---------------------------------------------------------------------------------------------
// URIs
// ---------------------------------------------------------------------------------------------

fn references_uri(file: Uuid) -> String {
    format!("/api/v1/media/{file}/references")
}
fn activity_uri(file: Uuid) -> String {
    format!("/api/v1/media/{file}/activity")
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

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
        .bind("Usage Test")
        .bind(format!("media-usage-{}", Uuid::new_v4().simple()))
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let site = sqlx::query_scalar::<_, Uuid>(
            "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
        )
        .bind(org)
        .bind("Usage Site")
        .fetch_one(db.pool())
        .await
        .expect("the site must be created");

        let (platform_id, _) = create_account(&db, None).await;
        seed::bind_owner(db.pool(), platform_id)
            .await
            .expect("the owner binding must be created");

        let (editor_id, editor_email) =
            bind_role(&db, org, platform_id, "Usage Editor", &EDITOR_PERMISSIONS).await;

        // A reader: `media.read` and nothing else. Both of these screens are reads, so the
        // reader must be able to open both — and the editor has no key the reader lacks that
        // either screen needs, which is what makes that a claim rather than a coincidence.
        let (reader_id, reader_email) =
            bind_role(&db, org, platform_id, "Usage Reader", &["media.read"]).await;

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
                storage_key: format!("usage/{}/{name}", Uuid::new_v4().simple()),
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

    /// Create a page with one published revision, and return its id.
    async fn page(&self, site: Uuid, slug: &str, title: &str) -> Uuid {
        let id: Uuid = sqlx::query_scalar(
            "insert into pages (site_id, slug, page_type, status) \
             values ($1, $2, 'page', 'published') returning id",
        )
        .bind(site)
        .bind(slug)
        .fetch_one(self.db.pool())
        .await
        .expect("the page must be created");
        sqlx::query(
            "insert into page_revisions (page_id, revision_no, state, title, body, published_at) \
             values ($1, 1, 'published', $2, '', now())",
        )
        .bind(id)
        .bind(title)
        .execute(self.db.pool())
        .await
        .expect("the revision must be created");
        id
    }

    /// A second, newer, *draft* revision — the one that must not become the label.
    async fn draft_revision(&self, page: Uuid, title: &str) {
        sqlx::query(
            "insert into page_revisions (page_id, revision_no, state, title, body) \
             values ($1, 2, 'draft', $2, '')",
        )
        .bind(page)
        .bind(title)
        .execute(self.db.pool())
        .await
        .expect("the draft revision must be created");
    }

    async fn reference(&self, media: Uuid, kind: &str, resource: Uuid, field: &str) {
        omnion_media::record_reference(
            self.db.pool(),
            &omnion_media::NewReference {
                media_id: media,
                resource_kind: kind.to_owned(),
                resource_id: resource.to_string(),
                field: field.to_owned(),
            },
        )
        .await
        .expect("the reference must be recorded");
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

/// Read one row out of the audit trail, so a response cannot vouch for a write it did not make.
async fn audit_actions(fixture: &Fixture, file: Uuid) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "select action from audit_log where target_id = $1::text order by id",
    )
    .bind(file.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit log must read")
}

/// Create an account with a unique address.
async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("usage-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Usage Test".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

/// Create an account, give it its own role with exactly `keys`, and bind it to the org.
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

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// A reader can open both screens, and a file of another tenant is refused.
///
/// The permission is the claim; the refusal is what keeps the ids from being walked. Both are
/// asserted in the same walk because a read that 200s for a file of another tenant passes its
/// own permission test perfectly.
///
/// The refusal is `403 cross_organization`, **not** a `404` — and that is the platform-wide
/// media convention rather than a slip: `site_in_scope` is what every media route loads, so a
/// `404` here would make these two screens the only place in the library where a foreign file
/// is invisible rather than forbidden, and an id that is invisible on one route and forbidden
/// on its neighbours is a better oracle than either alone. This test pins the *existing*
/// behaviour so that a change to it is a deliberate one.
#[tokio::test]
async fn usage_and_activity_are_reads_and_a_file_of_another_tenant_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let media = fixture.upload(site, "scope.txt").await;
    let reader = fixture.reader_token().await;
    let editor = fixture.editor_token().await;

    for uri in [references_uri(media), activity_uri(media)] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&reader), None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a reader may ask this of a file: {uri} → {}",
            response.body
        );
    }

    // Another tenant's file. The site load is the scope check, so this is a 404 and not a 403 —
    // a 403 would confirm the id exists.
    let other_org = sqlx::query_scalar::<_, Uuid>(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind("Usage Other")
    .bind(format!("usage-other-{}", Uuid::new_v4().simple()))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the second organization must be created");
    let other_site = sqlx::query_scalar::<_, Uuid>(
        "insert into sites (organization_id, key, name) values ($1, 'main', $2) returning id",
    )
    .bind(other_org)
    .bind("Usage Other Site")
    .fetch_one(fixture.db.pool())
    .await
    .expect("the second site must be created");
    let other_file = fixture.upload(other_site, "foreign.txt").await;

    for uri in [references_uri(other_file), activity_uri(other_file)] {
        let response = call(
            &fixture.state,
            request(Method::GET, &uri, Some(&editor), None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "another tenant's file is not readable: {uri} → {}",
            response.body
        );
        assert_eq!(
            response.body["error"]["code"],
            json!("cross_organization"),
            "and it says which boundary was crossed, naming no site: {uri} → {}",
            response.body
        );
    }

    sqlx::query("delete from organizations where id = $1")
        .bind(other_org)
        .execute(fixture.db.pool())
        .await
        .expect("the second organization must be cleaned up");
    fixture.cleanup().await;
}

/// An unused file says it is safe to delete, and a file used by one page in three fields says
/// it is *one* page.
///
/// The two numbers disagree in ordinary use and the sentence has to pick one deliberately: the
/// record count is what a reader acts on, and the row count is an implementation detail of the
/// reference table.
#[tokio::test]
async fn the_summary_distinguishes_records_from_fields() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;

    // Nothing uses it.
    let unused = fixture.upload(site, "unused.txt").await;
    let response = call(
        &fixture.state,
        request(Method::GET, &references_uri(unused), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["records"], json!(0));
    assert_eq!(response.body["resolved"], json!(0));
    assert_eq!(response.body["rows"], json!(0));
    let usage = response.body["usage"].as_array().expect("an array");
    assert!(usage.is_empty(), "an unused file has no rows: {usage:?}");
    assert!(
        response.body["summary"]
            .as_str()
            .expect("a sentence")
            .contains("breaks nothing"),
        "the empty state has to say it is safe: {}",
        response.body["summary"]
    );

    // One page, three fields.
    let used = fixture.upload(site, "three-fields.txt").await;
    let page = fixture
        .page(site, "hero-everywhere", "Hero everywhere")
        .await;
    for field in ["hero_image_id", "social_image_id", "og_image_id"] {
        fixture.reference(used, "page", page, field).await;
    }

    let response = call(
        &fixture.state,
        request(Method::GET, &references_uri(used), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(
        response.body["records"],
        json!(1),
        "one page is one record however many of its fields point here"
    );
    assert_eq!(response.body["rows"], json!(3), "three fields, three rows");
    assert_eq!(response.body["resolved"], json!(1));
    let summary = response.body["summary"].as_str().expect("a sentence");
    assert!(
        summary.contains("not 3 different ones"),
        "the sentence must correct the number a reader would assume: {summary}"
    );

    fixture.cleanup().await;
}

/// The label is the *published* revision's title, and a page with no revision is still resolved.
///
/// Both halves are the same query's two failure modes. Taking the newest revision shows a
/// draft's title on a list meant to answer "which published pages use this"; requiring a
/// revision drops a brand-new page from a list it has already been added to.
#[tokio::test]
async fn a_usage_row_is_labelled_with_its_published_title() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media = fixture.upload(site, "titles.txt").await;

    let published = fixture
        .page(site, "published-page", "The published title")
        .await;
    fixture
        .draft_revision(published, "An unpublished draft title")
        .await;
    fixture
        .reference(media, "page", published, "hero_image_id")
        .await;

    let response = call(
        &fixture.state,
        request(Method::GET, &references_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let rows = response.body["usage"].as_array().expect("an array");
    assert_eq!(rows.len(), 1, "one reference: {rows:?}");
    assert_eq!(
        rows[0]["label"],
        json!("The published title"),
        "the draft's title is not what a reader of this list is asking for: {rows:?}"
    );
    assert_eq!(rows[0]["status"], json!("published"));
    assert_eq!(rows[0]["resolved"], json!(true));
    assert!(
        rows[0]["path"]
            .as_str()
            .unwrap_or_default()
            .contains(&published.to_string()),
        "a resolved row is clickable: {rows:?}"
    );

    fixture.cleanup().await;
}

/// A reference whose page is gone is reported, marked, and *counted apart* from the live ones.
///
/// This is the row that makes the screen worth having. It refuses a purge for ever, and the
/// repair scan exists because of it — so a usage list that dropped it would make the library's
/// own bookkeeping invisible exactly where somebody needs to act.
#[tokio::test]
async fn a_stale_reference_is_reported_and_counted_apart() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media = fixture.upload(site, "stale.txt").await;

    let alive = fixture.page(site, "alive-page", "Still here").await;
    let doomed = fixture.page(site, "doomed-page", "About to vanish").await;
    fixture
        .reference(media, "page", alive, "hero_image_id")
        .await;
    fixture
        .reference(media, "page", doomed, "hero_image_id")
        .await;

    sqlx::query("delete from pages where id = $1")
        .bind(doomed)
        .execute(fixture.db.pool())
        .await
        .expect("the page must be deleted");

    let response = call(
        &fixture.state,
        request(Method::GET, &references_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(
        response.body["records"],
        json!(2),
        "the stale row still counts"
    );
    assert_eq!(
        response.body["resolved"],
        json!(1),
        "and is counted apart from the live one — the difference is the whole point"
    );

    let rows = response.body["usage"].as_array().expect("an array");
    assert_eq!(
        rows.len(),
        2,
        "a stale row is reported, not dropped: {rows:?}"
    );
    let stale = rows
        .iter()
        .find(|row| row["resolved"] == json!(false))
        .expect("the stale row must be marked");
    assert_eq!(stale["resource_id"], json!(doomed.to_string()));
    assert!(
        stale["path"].is_null(),
        "a row that points at nothing cannot be a link: {stale}"
    );

    let summary = response.body["summary"].as_str().expect("a sentence");
    assert!(
        summary.contains("no longer exists") && summary.contains("repair scan"),
        "the sentence names the problem and the way out of it: {summary}"
    );

    // And the repair scan really does clear it, which is what makes the sentence a promise.
    let repaired = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/media/retention/repair?site_id={site}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(repaired.status, StatusCode::OK, "{}", repaired.body);
    assert!(
        repaired.body["references_removed"]
            .as_i64()
            .unwrap_or_default()
            >= 1,
        "the sentence promised a repair that does this: {}",
        repaired.body
    );

    let after = call(
        &fixture.state,
        request(Method::GET, &references_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(
        after.body["records"],
        json!(1),
        "after the repair the stale row is gone: {}",
        after.body
    );
    // The sentence has to *change with the state*. A summary that still names the repair scan
    // after the repair has run is telling the operator to do a thing that is already done — the
    // most expensive kind of stale text, because it sends them to the retention tab to look for
    // a problem that no longer exists.
    let after_summary = after.body["summary"].as_str().expect("a sentence");
    assert!(
        !after_summary.contains("no longer exists") && !after_summary.contains("repair scan"),
        "the sentence must follow the state it describes: {after_summary}"
    );
    assert!(
        after_summary.contains("uses this file"),
        "and say what is true instead: {after_summary}"
    );

    fixture.cleanup().await;
}

/// A file's story carries its uploads, its share links *and* its access changes.
///
/// The grant is written under `media_file` while the bytes are written under `media`, so a
/// filter naming only one of them answers "who could see this file in March" with a list of
/// uploads — the most reassuring possible wrong answer.
#[tokio::test]
async fn a_files_story_includes_its_access_changes() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media = fixture.upload(site, "story.txt").await;

    // One audited action against the file's *bytes*…
    omnion_audit::record(
        fixture.db.pool(),
        omnion_audit::NewAuditEntry::by_user(fixture.accounts[1], "media.share_created")
            .target("media", media.to_string())
            .metadata(json!({ "site_id": site, "filename": "story.txt" })),
    )
    .await
    .expect("the entry must be recorded");

    // …and one against its *access*, under the other target name.
    omnion_audit::record(
        fixture.db.pool(),
        omnion_audit::NewAuditEntry::by_user(fixture.accounts[1], "media.grant_changed")
            .target("media_file", media.to_string())
            .metadata(json!({ "site_id": site, "effect": "allow", "capabilities": ["read"] })),
    )
    .await
    .expect("the entry must be recorded");

    // An action against something else entirely must NOT appear: the folder rename is recorded
    // against the folder, and a tab that showed it would be a second audit screen with no name
    // for what it excludes.
    omnion_audit::record(
        fixture.db.pool(),
        omnion_audit::NewAuditEntry::by_user(fixture.accounts[1], "media.folder_created")
            .target("media_folder", media.to_string()),
    )
    .await
    .expect("the entry must be recorded");

    let response = call(
        &fixture.state,
        request(Method::GET, &activity_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);

    let actions: Vec<&str> = response.body["activity"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"media.share_created"),
        "the bytes' own actions are here: {actions:?}"
    );
    assert!(
        actions.contains(&"media.grant_changed"),
        "and so are the access changes, under the other target name: {actions:?}"
    );
    assert!(
        !actions.contains(&"media.folder_created"),
        "an action against the folder is not this file's story: {actions:?}"
    );

    // The newest first, so the reader is looking at the most recent thing that happened.
    let ids: Vec<i64> = response.body["activity"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["id"].as_i64())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        ids, sorted,
        "newest first, which is what a trail is read as"
    );

    // And every row reads as a sentence, never as its token.
    for row in response.body["activity"].as_array().expect("an array") {
        let summary = row["summary"].as_str().expect("a sentence");
        assert!(
            !summary.starts_with("media."),
            "an action token on screen is a database column: {summary}"
        );
        assert!(!summary.trim().is_empty(), "no row is left blank");
    }

    fixture.cleanup().await;
}

/// A trashed file still has a story — that is when it is asked for.
///
/// `file_in_scope` reads live files only, so the obvious reuse of it here answers a `404` to
/// the one person who deleted the file and wants to know who did it and why.
#[tokio::test]
async fn a_trashed_file_still_answers_both_reads() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let site = fixture.sites[0];
    let token = fixture.editor_token().await;
    let media = fixture.upload(site, "trashed.txt").await;
    let page = fixture
        .page(site, "still-points", "Points at a deleted file")
        .await;
    fixture
        .reference(media, "page", page, "hero_image_id")
        .await;

    // The **soft** delete, and the distinction matters to this walk: `/media/{id}` is the hard
    // one (it removes the row), while `/media/files/{id}` moves the file to the trash and leaves
    // the row for the restore to find. The trashed-file claim is only testable against the
    // second, so the URI is spelled out rather than assembled from the id alone.
    let deleted = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!("/api/v1/media/files/{media}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);

    // The row is in the trash…
    let (trashed_at,): (Option<OffsetDateTime>,) =
        sqlx::query_as("select deleted_at from media where id = $1")
            .bind(media)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the row must read");
    assert!(trashed_at.is_some(), "the file is really in the trash");

    // …and both reads still answer, because that is when they are asked.
    let usage = call(
        &fixture.state,
        request(Method::GET, &references_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(
        usage.status,
        StatusCode::OK,
        "a trashed file still has referrers: {}",
        usage.body
    );
    assert_eq!(usage.body["records"], json!(1));

    let activity = call(
        &fixture.state,
        request(Method::GET, &activity_uri(media), Some(&token), None),
    )
    .await;
    assert_eq!(
        activity.status,
        StatusCode::OK,
        "a trashed file still has a story: {}",
        activity.body
    );
    let actions = audit_actions(&fixture, media).await;
    assert!(
        actions.iter().any(|action| action == "media.deleted"),
        "and the deletion is on it: {actions:?}"
    );

    fixture.cleanup().await;
}
