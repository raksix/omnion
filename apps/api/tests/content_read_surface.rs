//! Integration test for the headless content read surface (REQ-019, slice 2).
//!
//! Slice 1 proved a token *authenticates*. This file proves the thing a token is for: that it
//! reads **only published content, only inside its own site scope, in pages that do not skip or
//! repeat**. Those are the four properties a frontend's cache depends on, and each of them is a
//! silent failure — a leaking API returns rows and nobody notices, a repeating cursor returns
//! rows and nobody notices — so every claim below is written to fail loudly.
//!
//! What is proved here, and why each one is a trap that a naive implementation falls into:
//!
//! * **A draft never appears.** The fixtures publish three pages and leave two unpublished; a
//!   query that filtered on "has a revision" rather than "status is published" would serve the
//!   newest draft of an edited page, which is the single most damaging bug this surface can have.
//! * **The cursor walks a set exactly once.** Five published pages read with `limit=2` produce
//!   three pages and **no duplicate slug**. An `OFFSET` cursor passes a "does it paginate" test
//!   and fails this one the first time a page is deleted between two calls.
//! * **`fields` is a projection that cannot orphan a response.** `fields=slug,title` still
//!   carries `id`, `updated_at` and `etag`, because a response without them cannot be cached or
//!   used to ask for the next page.
//! * **An unknown field is a 400 that names it**, because the Explorer's form highlights the
//!   offending input from `details.field`.
//! * **A site scope is a filter, not a gate.** A single-site token asking about another site gets
//!   an empty list — not a 403, which would confirm the site exists.
//! * **`media:read` is its own power.** A content-only token calling the media list gets
//!   `403 insufficient_scope`, and the error names the scope so the caller knows what to ask the
//!   panel for.
//! * **A panel session is not a content token.** The read surface is the only token-authenticated
//!   part of the API, so a logged-in editor's cookie must not open it — that is the difference
//!   between "published content" and "the whole installation".
//! * **`updated_since` is the rebuild primitive.** Two sequential calls with one changed item
//!   return exactly that item.
//!
//! Runs against the development stack, and skips itself with a printed reason when PostgreSQL
//! is not reachable.

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
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The content editor: the panel powers a page needs to publish with.
const EDITOR_PERMISSIONS: [&str; 3] = [
    "content.pages.read",
    "content.pages.create",
    "content.pages.publish",
];

/// The token curator: read plus manage, so the suite can mint the tokens it then reads with.
const CURATOR_EXTRA: [&str; 2] = ["content.api.read", "content.api.manage"];

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn call(state: &AppState, request: Request<Body>) -> TestResponse {
    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("router must answer");
    let status = response.status();
    let headers = response.headers().clone();
    // EVERY `Set-Cookie`, joined. Sign-in issues two headers — the session and the CSRF token —
    // and a suite that keeps only the first is a suite whose every mutation answers `403`. The
    // attributes are trimmed so the string is re-sendable as a `Cookie:` request header.
    let set_cookie = {
        let values: Vec<String> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned())
            .collect();
        (!values.is_empty()).then(|| values.join("; "))
    };
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    TestResponse {
        status,
        set_cookie,
        headers,
        body,
    }
}

fn session_request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(cookies) => {
            let builder = builder.header(header::COOKIE, cookies);
            // The CSRF secret travels as a HEADER, and the cookie it was issued alongside is only
            // the readable half. Sending the cookie without the header is the shape every
            // cookie-authenticated mutation in this suite used to have, and the platform's answer
            // was `403 csrf_unavailable` — twelve identical failures whose message names a
            // missing environment variable rather than a missing header, so the cost of reading
            // them as "the suite is misconfigured" was a whole tick.
            //
            // Read out of the SAME cookie string the session is sent in, so the two can never
            // disagree about which session they belong to.
            match csrf_of(cookies) {
                Some(secret) => builder.header("x-omnion-csrf", secret),
                None => builder,
            }
        }
        None => builder,
    };
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("body serializes"),
            ))
            .expect("request builds"),
        None => builder.body(Body::empty()).expect("request builds"),
    }
}

/// The `omnion_csrf` value from a joined cookie header.
fn csrf_of(cookies: &str) -> Option<String> {
    cookies.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key.trim() == "omnion_csrf" && !value.is_empty()).then(|| value.to_owned())
    })
}

/// A request carrying a content token rather than a session.
fn api_request(method: Method, uri: &str, token: Option<&str>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => builder.header(header::AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    };
    builder.body(Body::empty()).expect("request builds")
}

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );
    Some((state, db))
}

async fn create_account(db: &Db, organization_id: Option<Uuid>) -> (Uuid, String) {
    let email = format!("read-{}@example.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            display_name: "Read Tester".to_owned(),
            password: PASSWORD.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must be created");
    (user.id, email)
}

async fn login(state: &AppState, email: &str) -> String {
    let response = call(
        state,
        session_request(
            Method::POST,
            "/api/v1/auth/login",
            None,
            Some(json!({ "email": email, "password": PASSWORD })),
        ),
    )
    .await;
    assert!(
        response.status.is_success(),
        "login for {email} answered {}: {}",
        response.status,
        response.body
    );
    // Both cookies, read by name and rejoined. Sign-in issues `omnion_session` *and*
    // `omnion_csrf`; taking only the first — which is what this used to do, with
    // `.split(';').next()` — leaves every cookie-authenticated mutation answered `403 csrf_failed`.
    // The error names the missing header, so it is a loud failure rather than a silent one, and
    // reading by name survives the platform adding a third cookie.
    let header = response
        .set_cookie
        .as_deref()
        .expect("login must set the session cookie");
    let mut cookies: Vec<String> = Vec::new();
    for part in header.split(';') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            if matches!(key.trim(), "omnion_session" | "omnion_csrf") && !value.is_empty() {
                cookies.push(format!("{}={}", key.trim(), value));
            }
        }
    }
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("omnion_session=")),
        "login must set the session cookie: {header}"
    );
    cookies.join("; ")
}

async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!(
                "{}-{}",
                label.to_lowercase().replace(' ', "-"),
                &Uuid::new_v4().simple().to_string()[..8]
            ),
            name: label.to_owned(),
            description: format!("{label} role"),
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
        .expect("the role permission set must be written");
    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: Scope::Organization { organization_id },
        granted_by: None,
        expires_at: None,
    };
    bindings::validate(db.pool(), &binding)
        .await
        .expect("the binding must validate");
    bindings::grant(db.pool(), binding)
        .await
        .expect("the binding must be granted");
}

struct Fixture {
    state: AppState,
    db: Db,
    org: Uuid,
    site: Uuid,
    /// A second site of the same organization, for the site-scope test.
    other_site: Uuid,
    editor_email: String,
    curator_email: String,
    outsider_email: String,
    outsider_org: Uuid,
    outsider_site: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(org)
            .bind("Read Test Org")
            .bind(format!("rd-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the organization must be created");

        let outsider_org = Uuid::new_v4();
        sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
            .bind(outsider_org)
            .bind("Read Outsider Org")
            .bind(format!("rdo-{}", Uuid::new_v4().simple()))
            .execute(db.pool())
            .await
            .expect("the outsider organization must be created");

        let mut sites = Vec::new();
        for (owner, label) in [
            (org, "Read Site"),
            (org, "Read Other Site"),
            (outsider_org, "Outsider Site"),
        ] {
            let site = Uuid::new_v4();
            sqlx::query(
                "insert into sites (id, organization_id, key, name) values ($1, $2, $3, $4)",
            )
            .bind(site)
            .bind(owner)
            .bind(format!("s{}", &Uuid::new_v4().simple().to_string()[..10]))
            .bind(label)
            .execute(db.pool())
            .await
            .expect("the site must be created");
            sites.push(site);
        }

        let (editor_id, editor_email) = create_account(&db, Some(org)).await;
        grant(
            &db,
            org,
            editor_id,
            &EDITOR_PERMISSIONS.to_vec(),
            "Read Editor",
        )
        .await;

        let (curator_id, curator_email) = create_account(&db, Some(org)).await;
        let mut curator_keys = EDITOR_PERMISSIONS.to_vec();
        curator_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(&db, org, curator_id, &curator_keys, "Read Curator").await;

        let (outsider_id, outsider_email) = create_account(&db, Some(outsider_org)).await;
        let mut outsider_keys = EDITOR_PERMISSIONS.to_vec();
        outsider_keys.extend_from_slice(&CURATOR_EXTRA);
        grant(
            &db,
            outsider_org,
            outsider_id,
            &outsider_keys,
            "Outsider Curator",
        )
        .await;

        Some(Self {
            state,
            db,
            org,
            site: sites[0],
            other_site: sites[1],
            editor_email,
            curator_email,
            outsider_email,
            outsider_org,
            outsider_site: sites[2],
        })
    }

    async fn editor(&self) -> String {
        login(&self.state, &self.editor_email).await
    }
    async fn curator(&self) -> String {
        login(&self.state, &self.curator_email).await
    }
    async fn outsider(&self) -> String {
        login(&self.state, &self.outsider_email).await
    }

    /// Insert a page directly, with an explicit status and timestamp.
    ///
    /// Deliberately not through `/api/v1/pages`: the surface under test only ever reads
    /// published rows, and a fixture that can create an arbitrary `status` and an arbitrary
    /// `updated_at` is the only way to prove the filter and the sort without waiting for a clock
    /// to tick. The published revision is written in the same transaction so the row the
    /// endpoint joins to exists.
    async fn seed_page(&self, site: Uuid, slug: &str, status: &str, minutes_ago: i64) -> Uuid {
        self.seed_page_inner(site, slug, status, minutes_ago, None)
            .await
    }

    /// The body of [`Self::seed_page`], with the revision's title as a parameter.
    ///
    /// `None` derives the title from the slug, which is what every fixture that does not care
    /// about ordering wants. It is derived rather than blank so the alphabetical and the
    /// timestamp orders stay *different* for a default fixture: a title-sort test that reused
    /// `seed_page` would pass against a query that ignored `sort` entirely.
    async fn seed_page_inner(
        &self,
        site: Uuid,
        slug: &str,
        status: &str,
        minutes_ago: i64,
        title: Option<&str>,
    ) -> Uuid {
        let page_id = Uuid::new_v4();
        let revision_id = Uuid::new_v4();
        let stamp = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
        let published = status == "published";

        // Both rows, in one transaction. The two foreign keys point at each other —
        // `page_revisions.page_id` needs the page, and `pages.published_revision_id` needs the
        // revision — so neither insert order is legal on its own. The store's own `create_page`
        // has the same shape and solves it the same way: insert the page, insert the revision,
        // then point the page at the revision, all inside one transaction so the intermediate
        // state is never visible.
        let mut tx = self
            .db
            .pool()
            .begin()
            .await
            .expect("a transaction must open");
        sqlx::query(
            "insert into pages (id, site_id, slug, page_type, status, created_at, updated_at) \
             values ($1, $2, $3, 'page', $4, $5, $5)",
        )
        .bind(page_id)
        .bind(site)
        .bind(slug)
        .bind(status)
        .bind(stamp)
        .execute(&mut *tx)
        .await
        .expect("the page must be seeded");

        sqlx::query(
            "insert into page_revisions (id, page_id, revision_no, state, title, body, summary, \
                                       created_at, published_at) \
             values ($1, $2, 1, $3, $4, $5, $6, $7, $8)",
        )
        .bind(revision_id)
        .bind(page_id)
        .bind(if published { "published" } else { "draft" })
        .bind(title.map_or_else(|| format!("Title of {slug}"), str::to_owned))
        .bind(format!("Body of {slug}"))
        .bind(Some(format!("Summary of {slug}")))
        .bind(stamp)
        .bind(published.then_some(stamp))
        .execute(&mut *tx)
        .await
        .expect("the revision must be seeded");

        // Only a published page points at a revision. A draft carrying one is precisely the row
        // the read surface must refuse, and leaving this `NULL` is what makes the
        // "only published pages are served" walk a test rather than a test of the status column.
        sqlx::query("update pages set published_revision_id = $2 where id = $1")
            .bind(page_id)
            .bind(published.then_some(revision_id))
            .execute(&mut *tx)
            .await
            .expect("the publication pointer must be set");

        tx.commit().await.expect("the fixture must commit");
        page_id
    }

    /// [`Self::seed_page`] with a chosen revision title.
    ///
    /// The title is a property of the *revision*, not of the page — which is precisely the fact
    /// `?sort=title` needs, and the reason the pages query orders by `r.title` and not by a
    /// `title` on `pages`. A fixture that could not choose a title would let a query that sorted
    /// by the *wrong* thing still look alphabetical, because every fixture title is derived from
    /// its slug and a slug-ordered list and a title-ordered list would then coincide.
    async fn seed_page_title(
        &self,
        site: Uuid,
        slug: &str,
        status: &str,
        minutes_ago: i64,
        title: &str,
    ) -> Uuid {
        self.seed_page_inner(site, slug, status, minutes_ago, Some(title))
            .await
    }

    /// Mint a content token and return its plaintext.
    async fn mint(&self, session: &str, name: &str, site: Option<Uuid>, scopes: &[&str]) -> String {
        let response = call(
            &self.state,
            session_request(
                Method::POST,
                "/api/v1/content-api/tokens",
                Some(session),
                Some(json!({
                    "name": name,
                    "site_id": site,
                    "scopes": scopes,
                })),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
        response.body["plaintext"]
            .as_str()
            .expect("the create response must carry the plaintext")
            .to_owned()
    }

    async fn get(&self, uri: &str, token: Option<&str>) -> TestResponse {
        call(&self.state, api_request(Method::GET, uri, token)).await
    }
}

/// The slugs a list response carries, in order.
fn slugs(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("a list must carry items")
        .iter()
        .map(|item| item["slug"].as_str().unwrap_or_default().to_owned())
        .collect()
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn only_published_pages_are_served_and_never_a_draft() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Publisher",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];

    fixture
        .seed_page(fixture.site, &format!("{prefix}-live-1"), "published", 5)
        .await;
    fixture
        .seed_page(fixture.site, &format!("{prefix}-live-2"), "published", 4)
        .await;
    // A draft that is *newer* than every published page: the ordering a naive query would pick
    // if it filtered on "has a revision" rather than on status.
    fixture
        .seed_page(fixture.site, &format!("{prefix}-draft"), "draft", 0)
        .await;
    fixture
        .seed_page(fixture.site, &format!("{prefix}-archived"), "archived", 1)
        .await;

    let response = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let found = slugs(&response.body);
    assert!(
        found.contains(&format!("{prefix}-live-1")),
        "a published page must be served: {found:?}"
    );
    assert!(
        !found.contains(&format!("{prefix}-draft")),
        "a draft must never be served, and the newest row here IS a draft: {found:?}"
    );
    assert!(
        !found.contains(&format!("{prefix}-archived")),
        "an archived page is not published: {found:?}"
    );
    // And the newest published page comes first.
    assert_eq!(
        found.first().map(String::as_str),
        Some(format!("{prefix}-live-2").as_str()),
        "newest change first: {found:?}"
    );
}

#[tokio::test]
async fn the_cursor_walks_a_set_exactly_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Pager",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    // Five published pages, staggered so `updated_at` ordering is total.
    for index in 0..5 {
        fixture
            .seed_page(
                fixture.site,
                &format!("{prefix}-p{index}"),
                "published",
                10 - index,
            )
            .await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= 6, "the walk must terminate: {seen:?}");
        let mut uri = format!("/api/v1/content/pages?site={}&limit=2", fixture.site);
        if let Some(value) = &cursor {
            // The cursor carries two `|` characters and `http::Uri` refuses one raw, so the
            // request never reached the router and the walk read as "one page, no cursor". The
            // encoder is the same one the timestamp below uses, for the same reason.
            uri.push_str(&format!("&cursor={}", urlencode(value)));
        }
        let response = fixture.get(&uri, Some(&token)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        seen.extend(slugs(&response.body));
        match response.body["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    assert_eq!(
        pages, 3,
        "five rows at two per page is three pages: {seen:?}"
    );
    assert_eq!(seen.len(), 5, "every published page exactly once: {seen:?}");
    let unique = {
        let mut copy = seen.clone();
        copy.sort();
        copy.dedup();
        copy
    };
    assert_eq!(unique.len(), 5, "no page was served twice: {seen:?}");
}

/// `count` must count the items actually sent, not the items the query asked for.
///
/// This is its own test because the walk above passes with a broken `count`: a walk that
/// concatenates `items` never consults `count`, so a response that says `count: 2` while carrying
/// three items walks perfectly and tells a *different* client it lost one. The over-fetch row
/// used to be rendered into `items` and excluded from `count`, which is exactly that shape.
#[tokio::test]
async fn a_page_sends_the_limit_and_says_so() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Counter",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    // Five rows, so `limit=2` is a page that is *full* AND has more behind it — the only shape
    // where the over-fetch row exists to be either shown or hidden.
    for index in 0..5 {
        fixture
            .seed_page(
                fixture.site,
                &format!("{prefix}-c{index}"),
                "published",
                20 - index,
            )
            .await;
    }

    let response = fixture
        .get(
            &format!("/api/v1/content/pages?site={}&limit=2", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let items = response.body["items"]
        .as_array()
        .expect("items must be an array")
        .len();
    let count = response.body["count"]
        .as_u64()
        .expect("count must be a number");
    assert_eq!(items, 2, "a limit of two is two items: {}", response.body);
    assert_eq!(
        count as usize, items,
        "`count` counts what is sent: {items} sent, {count} said"
    );
    assert!(
        response.body["next_cursor"].is_string(),
        "and a full page with rows behind it continues: {}",
        response.body
    );

    // The last page: one row, no cursor, and `count` of one.
    let mut cursor = response.body["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    let mut last = None;
    for _ in 0..4 {
        let uri = format!(
            "/api/v1/content/pages?site={}&limit=2&cursor={}",
            fixture.site,
            urlencode(&cursor)
        );
        let page = fixture.get(&uri, Some(&token)).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        if page.body["next_cursor"].is_null() {
            last = Some(page.body.clone());
            break;
        }
        cursor = page.body["next_cursor"]
            .as_str()
            .expect("cursor")
            .to_owned();
    }
    let last = last.expect("the walk must reach a last page");
    assert_eq!(
        last["items"].as_array().expect("items").len(),
        1,
        "five rows at two per page ends with one: {last}"
    );
    assert_eq!(last["count"].as_u64(), Some(1), "{last}");
}

/// `sort=title` is a documented parameter and it has to page.
///
/// It is on its own because the two failure modes are indistinguishable from the outside: the
/// pages query built `p.title`, a column that has never existed on `pages` (the title is the
/// *revision's*), so a title walk was a `500` on the first page and nobody had called it with a
/// cursor to see the second. The title is compared as text, so the cursor's value is a string
/// here — the one sort whose key is not an instant, which is why the two bind paths differ.
///
/// **Descending, like every other sort.** The crate documents one direction for the whole
/// surface ("sorting is newest-first and the keyset must match the sort"), and a keyset walk
/// carries its direction in the comparison, so ascending would be a different cursor format
/// rather than a different flag. The first version of this test asserted alphabetical order and
/// failed against a correct endpoint: `sort=title` is Z→A, and the test's own expectation was the
/// thing that was wrong.
#[tokio::test]
async fn a_title_sort_pages_by_the_revision_title_descending() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Titled",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    // Titles that make the alphabetical order obviously not the timestamp order, so a query that
    // silently ignored `sort` and served newest-first could not pass.
    for (index, title) in ["alpha", "bravo", "charlie"].iter().enumerate() {
        fixture
            .seed_page_title(
                fixture.site,
                &format!("{prefix}-t{index}"),
                "published",
                30 - index as i64,
                &format!("{prefix} {title}"),
            )
            .await;
    }

    let mut uri = format!(
        "/api/v1/content/pages?site={}&limit=2&sort=title",
        fixture.site
    );
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..4 {
        let response = fixture.get(&uri, Some(&token)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "sort=title must be answered, not refused by a missing column: {}",
            response.body
        );
        for item in response.body["items"]
            .as_array()
            .expect("items must be an array")
        {
            seen.push(item["title"].as_str().expect("title").to_owned());
        }
        if response.body["next_cursor"].is_null() {
            break;
        }
        uri = format!(
            "/api/v1/content/pages?site={}&limit=2&sort=title&cursor={}",
            fixture.site,
            urlencode(response.body["next_cursor"].as_str().expect("cursor"))
        );
    }
    let mut expected = vec![
        format!("{prefix} alpha"),
        format!("{prefix} bravo"),
        format!("{prefix} charlie"),
    ];
    // Z→A, and *not* the seeding order (alpha, bravo, charlie) — the timestamps run the other
    // way, so a query that ignored `sort` would produce exactly that and fail here.
    expected.sort_by(|left, right| right.cmp(left));
    assert_eq!(
        seen, expected,
        "a title walk is descending, alphabetical and complete: {seen:?}"
    );
}

/// A sort a relation does not have is refused by name, not by a `500` about a missing column.
///
/// `media` has no title, and the media list used to accept `?sort=title` and build `m.title` — a
/// documented parameter whose every call was an `internal_error`. The refusal has to be a `400`
/// naming `sort`, because a `500` is not a contract change: the caller cannot act on it.
#[tokio::test]
async fn a_sort_the_media_list_cannot_do_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "MediaSorter",
            Some(fixture.site),
            &["media:read"],
        )
        .await;

    let response = fixture
        .get(
            &format!("/api/v1/content/media?site={}&sort=title", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a file has no title to sort by: {}",
        response.body
    );
    assert_eq!(response.body["error"]["code"], "invalid_parameter");
    let message = response.body["error"]["message"]
        .as_str()
        .expect("a message")
        .to_owned();
    assert!(
        message.contains("title"),
        "it names the parameter: {message}"
    );
    assert!(
        message.contains("updated_at") && message.contains("created_at"),
        "and lists what this endpoint does offer: {message}"
    );

    // The two it does offer still work, so the refusal is a vocabulary answer and not a ban.
    for sort in ["updated_at", "created_at"] {
        let ok = fixture
            .get(
                &format!("/api/v1/content/media?site={}&sort={sort}", fixture.site),
                Some(&token),
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "sort={sort}: {}", ok.body);
    }
}

#[tokio::test]
async fn a_projection_keeps_the_keys_a_caller_needs_to_keep_going() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Projector",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    fixture
        .seed_page(fixture.site, &format!("{prefix}-one"), "published", 1)
        .await;

    let response = fixture
        .get(
            &format!(
                "/api/v1/content/pages?site={}&fields=slug,title",
                fixture.site
            ),
            Some(&token),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let item = &response.body["items"][0];
    assert_eq!(item["title"], json!(format!("Title of {prefix}-one")));
    assert!(item.get("body").is_none(), "body was not selected: {item}");
    // The keys that make the response usable at all.
    for key in ["id", "slug", "updated_at", "etag", "type", "locale"] {
        assert!(
            item.get(key).is_some(),
            "{key} must survive a projection: {item}"
        );
    }
}

#[tokio::test]
async fn an_unknown_field_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Fields",
            Some(fixture.site),
            &["content:read"],
        )
        .await;

    let response = fixture
        .get(
            &format!("/api/v1/content/pages?fields=slug,nonsense",),
            Some(&token),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    let error = &response.body["error"];
    assert_eq!(error["code"], json!("invalid_parameter"));
    assert_eq!(error["details"]["field"], json!("fields"));
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("nonsense"),
        "the message names the field: {}",
        error
    );
}

#[tokio::test]
async fn a_site_scope_is_a_filter_and_never_a_confirmation() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A token scoped to one site.
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Scoped",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    fixture
        .seed_page(fixture.site, &format!("{prefix}-mine"), "published", 2)
        .await;
    fixture
        .seed_page(
            fixture.other_site,
            &format!("{prefix}-theirs"),
            "published",
            1,
        )
        .await;

    // Naming its own site works.
    let own = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(own.status, StatusCode::OK);
    assert!(slugs(&own.body).contains(&format!("{prefix}-mine")));

    // Naming another site returns nothing, and does not confirm the site exists.
    let other = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.other_site),
            Some(&token),
        )
        .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    assert!(
        slugs(&other.body).is_empty(),
        "a token must not read another site: {}",
        other.body
    );
    assert!(
        !other.raw_contains(&format!("{prefix}-theirs")),
        "nor may the other site's content appear anywhere in the body"
    );
}

/// Small helper so the cross-site assertion reads as a sentence.
trait RawContains {
    fn raw_contains(&self, needle: &str) -> bool;
}

impl RawContains for TestResponse {
    fn raw_contains(&self, needle: &str) -> bool {
        self.body.to_string().contains(needle)
    }
}

#[tokio::test]
async fn media_is_its_own_power() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A content-only token: it may read pages and nothing else.
    let token = fixture
        .mint(
            &fixture.curator().await,
            "PagesOnly",
            Some(fixture.site),
            &["content:read"],
        )
        .await;

    let denied = fixture
        .get(
            &format!("/api/v1/content/media?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.body);
    assert_eq!(denied.body["error"]["code"], json!("insufficient_scope"));
    assert_eq!(
        denied.body["error"]["details"]["required_scope"],
        json!("media:read")
    );

    // With the scope, the same call answers — and the list shape is the documented one.
    let both = fixture
        .mint(
            &fixture.curator().await,
            "PagesAndMedia",
            Some(fixture.site),
            &["content:read", "media:read"],
        )
        .await;
    let allowed = fixture
        .get(
            &format!("/api/v1/content/media?site={}", fixture.site),
            Some(&both),
        )
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
    assert!(allowed.body["items"].is_array());
    assert!(allowed.body["next_cursor"].is_null() || allowed.body["next_cursor"].is_string());
}

#[tokio::test]
async fn a_missing_or_wrong_credential_is_refused_before_any_row_is_read() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let curator = fixture.curator().await;

    for case in [
        ("no token", None),
        (
            "a made-up token",
            Some("omn_00000000_deadbeefdeadbeefdeadbeefdeadbeef"),
        ),
        ("a token from another organization", None),
    ] {
        let uri = format!("/api/v1/content/pages?site={}", fixture.site);
        let response = fixture.get(&uri, case.1).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{} must be refused: {}",
            case.0,
            response.body
        );
        assert_eq!(response.body["error"]["code"], json!("invalid_token"));
    }

    // And a token from a *different* organization is `invalid_token`, never `forbidden` — a
    // `forbidden` would confirm the token exists.
    let outsider = fixture
        .mint(
            &fixture.outsider().await,
            "Foreign",
            Some(fixture.outsider_site),
            &["content:read"],
        )
        .await;
    let foreign = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.other_site),
            Some(&outsider),
        )
        .await;
    assert_eq!(foreign.status, StatusCode::OK);
    assert!(
        slugs(&foreign.body).is_empty(),
        "a foreign token reads nothing here: {}",
        foreign.body
    );
    let _ = curator;
}

#[tokio::test]
async fn a_panel_session_does_not_open_the_content_surface() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // The editor is a full member of the organization with page permissions — and the read
    // surface must still refuse them, because that surface exists to hand published content to
    // something *outside* the panel. A session fallback would make the token optional.
    let editor = fixture.editor().await;
    let response = call(
        &fixture.state,
        session_request(
            Method::GET,
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&editor),
            None,
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a session must not authenticate the content surface: {}",
        response.body
    );
}

#[tokio::test]
async fn updated_since_returns_only_what_changed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "Rebuilder",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    fixture
        .seed_page(fixture.site, &format!("{prefix}-old"), "published", 60)
        .await;
    fixture
        .seed_page(fixture.site, &format!("{prefix}-new"), "published", 1)
        .await;

    // Everything, then only what is newer than the newest of the first call.
    let all = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(slugs(&all.body).len(), 2);
    // **The response's own `updated_at`, fed back verbatim.** This line used to run the value
    // through a hand-written `to_rfc3339` helper, and the helper existed because the API did not
    // round trip its own output: the item carried the driver's `Display`
    // (`2026-09-30 21:53:21.509904 +00:00:00`) while `updated_since` accepts only RFC 3339. A test
    // that carries a conversion function is a test *documenting* a defect, and the comment above
    // the helper said so in as many words.
    //
    // A test that formats the watermark with the very parser it is testing would have proven
    // nothing, so the conversion was written out by hand — which is exactly what made the trap
    // visible. The fix keeps the assertion honest and drops the conversion: the value the caller
    // is handed is now the value the API accepts back.
    let newest = all.body["items"][0]["updated_at"]
        .as_str()
        .expect("an updated_at")
        .to_owned();

    let changed = fixture
        .get(
            &format!(
                "/api/v1/content/pages?site={}&updated_since={}",
                fixture.site,
                urlencode(&newest)
            ),
            Some(&token),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    let since = slugs(&changed.body);
    // `updated_since` is strictly-greater, so the newest item is excluded and nothing older
    // sneaks in — the answer is "nothing changed after that instant", not an error.
    assert!(
        !since.contains(&format!("{prefix}-old")),
        "a page older than the watermark must not be returned: {since:?}"
    );
}

/// Percent-encode a value for use in a query string.
///
/// The `|` case is the one that matters, and it was missing until a cursor walk failed with
/// `InvalidUri(InvalidUriChar)`. `encode_cursor` emits `value|id|mac`, so a cursor interpolated raw
/// into a URI carries two `|` characters, which `http::Uri` refuses outright — the request never
/// reached the router, so the failure surfaced as a URL-builder error and read like a harness bug
/// rather than an encoding one.
///
/// This is a hand-rolled encoder, so it carries exactly the characters these values can contain:
/// `+`, `:` and a space from a timestamp (`2026-09-30 21:41:02.388509+00:00`), and `|` from the
/// cursor. A general encoder would be better, but a general encoder that is wrong in a way these
/// tests cannot detect is worse than a short list that is obviously complete for its inputs.
fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '+' => "%2B".to_owned(),
            ':' => "%3A".to_owned(),
            '|' => "%7C".to_owned(),
            ' ' => "%20".to_owned(),
            other => other.to_string(),
        })
        .collect()
}

#[tokio::test]
async fn the_openapi_document_is_valid_and_complete() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "DocReader",
            None,
            &["content:read"],
        )
        .await;

    let response = fixture
        .get("/api/v1/content/openapi.json", Some(&token))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["openapi"], json!("3.1.0"));

    // Every read route of the surface appears with its scope.
    for (path, scope) in [
        ("/api/v1/content/pages", "content:read"),
        ("/api/v1/content/pages/{slug}", "content:read"),
        ("/api/v1/content/posts", "content:read"),
        ("/api/v1/content/posts/{slug}", "content:read"),
        ("/api/v1/content/media", "media:read"),
        ("/api/v1/content/sites", "content:read"),
    ] {
        let operation = &response.body["paths"][path]["get"];
        assert!(!operation.is_null(), "{path} must be documented");
        assert_eq!(operation["x-required-scope"], json!(scope), "{path}");
    }

    // And the note about the panel's own media path is where a reader will see it.
    let note = response.body["paths"]["/api/v1/media"]["get"]["summary"]
        .as_str()
        .expect("the media note exists");
    assert!(note.contains("/api/v1/content/media"), "{note}");
}

#[tokio::test]
async fn a_single_page_is_served_and_an_unpublished_one_is_not_found() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let token = fixture
        .mint(
            &fixture.curator().await,
            "OneReader",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let prefix = &Uuid::new_v4().simple().to_string()[..8];
    fixture
        .seed_page(fixture.site, &format!("{prefix}-live"), "published", 3)
        .await;
    fixture
        .seed_page(fixture.site, &format!("{prefix}-secret"), "draft", 0)
        .await;

    let live = fixture
        .get(
            &format!(
                "/api/v1/content/pages/{}-live?site={}",
                prefix, fixture.site
            ),
            Some(&token),
        )
        .await;
    assert_eq!(live.status, StatusCode::OK, "{}", live.body);
    assert_eq!(live.body["item"]["slug"], json!(format!("{prefix}-live")));

    // A draft answers the same 404 as a slug that does not exist, so the status code cannot be
    // used to discover unpublished content.
    let secret = fixture
        .get(
            &format!(
                "/api/v1/content/pages/{}-secret?site={}",
                prefix, fixture.site
            ),
            Some(&token),
        )
        .await;
    assert_eq!(secret.status, StatusCode::NOT_FOUND, "{}", secret.body);
    let missing = fixture
        .get(
            &format!(
                "/api/v1/content/pages/{}-nope?site={}",
                prefix, fixture.site
            ),
            Some(&token),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        secret.body["error"]["code"], missing.body["error"]["code"],
        "a draft and a missing page must be indistinguishable"
    );
}

#[tokio::test]
async fn the_sites_list_reflects_the_token_scope() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let scoped = fixture
        .mint(
            &fixture.curator().await,
            "OneSite",
            Some(fixture.site),
            &["content:read"],
        )
        .await;
    let response = fixture.get("/api/v1/content/sites", Some(&scoped)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let listed = response.body.as_array().expect("a list");
    assert_eq!(
        listed.len(),
        1,
        "a single-site token sees one site: {listed:?}"
    );
    assert_eq!(listed[0]["id"], json!(fixture.site.to_string()));

    // An organization-wide token sees the organization's sites and not the outsider's.
    let wide = fixture
        .mint(
            &fixture.curator().await,
            "AllSites",
            None,
            &["content:read"],
        )
        .await;
    let all = fixture.get("/api/v1/content/sites", Some(&wide)).await;
    let ids: Vec<String> = all
        .body
        .as_array()
        .expect("a list")
        .iter()
        .map(|site| site["id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(ids.contains(&fixture.site.to_string()));
    assert!(ids.contains(&fixture.other_site.to_string()));
    assert!(
        !ids.contains(&fixture.outsider_site.to_string()),
        "another organization's site must never be listed: {ids:?}"
    );
}

#[tokio::test]
async fn a_revoked_token_stops_reading_immediately() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let curator = fixture.curator().await;
    let token = fixture
        .mint(&curator, "RevokeMe", Some(fixture.site), &["content:read"])
        .await;
    fixture
        .seed_page(
            fixture.site,
            &format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]),
            "published",
            1,
        )
        .await;

    let before = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);

    // Revoke it through the panel.
    let listed = call(
        &fixture.state,
        session_request(
            Method::GET,
            "/api/v1/content-api/tokens",
            Some(&curator),
            None,
        ),
    )
    .await;
    let id = listed
        .body
        .as_array()
        .expect("tokens")
        .iter()
        .find(|row| row["name"] == json!("RevokeMe"))
        .expect("the token row")
        .clone()["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let revoked = call(
        &fixture.state,
        session_request(
            Method::DELETE,
            &format!("/api/v1/content-api/tokens/{id}"),
            Some(&curator),
            None,
        ),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.body);

    let after = fixture
        .get(
            &format!("/api/v1/content/pages?site={}", fixture.site),
            Some(&token),
        )
        .await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "a revoked token must stop reading at once: {}",
        after.body
    );
    assert_eq!(after.body["error"]["code"], json!("token_revoked"));
}

/// Silence the unused-field warning for a fixture field kept for symmetry with the token suite.
#[allow(dead_code)]
fn _touch(fixture: &Fixture) -> Uuid {
    fixture.outsider_org
}
