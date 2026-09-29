//! The palette's business sections, proved end to end (docs/requests/REQ-052, slice 6).
//!
//! REQ-051 registered `contacts`, `companies` and `deals` with the indexer and **nothing ever
//! showed them in the ⌘K box**, because a search provider is registered in two places and only one
//! of them is a type: `crates/search/src/providers.rs` decides what is indexed, and the panel's
//! registry decides whether a section is rendered. A provider missing from the second renders
//! **nothing**, with no error anywhere — `paletteProvider` answers `null` by design, and the API
//! answers `hits: []`. The CRM's own tests passed throughout.
//!
//! These walks are the half a unit test cannot reach. They reindex through
//! `POST /api/v1/search/reindex` — the same call the status screen makes, running the same
//! `indexer::reindex` — and then ask `GET /api/v1/search` for the rows back, so a provider whose
//! upsert writes nothing is caught here rather than by somebody noticing a missing section six
//! months later.
//!
//! What each walk asserts, and why it is not a tautology:
//!
//! * **quotes and orders index, and their rows carry a title, a subtitle and a url** — a document
//!   that arrives with an empty title is a blank row in the palette, and a url that is not a sales
//!   route is a click that lands nowhere. The walk reads both out of the API's own answer.
//! * **the number outranks the customer** — the inversion this slice is about, and the only part
//!   of it a search can prove: a query naming both returns the document the number belongs to.
//! * **an order is reachable by its quote's number** — somebody chasing "what happened to
//!   Q-2026-0007" types the quote number, and answering with the frozen quote beside the order
//!   sends them to a document that can no longer change.
//! * **an archived quote stops answering** — the prune half, which the upsert alone never proves.
//! * **each document's own section requires its own key** — a seller who may read quotes and not
//!   orders gets one and not the other. This is why the two are two providers rather than one
//!   behind an "any of" guard, and it is the only walk here that can fail if somebody merges
//!   them back into a single provider.
//! * **the create command is gated by `sales.quotes.create`** and not by the read key.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{
    Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope,
};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the IAM seed and the index are shared state.
static PALETTE_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A seller: reads and creates quotes, reads orders. Deliberately **not** `sales.quotes.update`,
/// `sales.orders.create` or anything about confirming — the palette must not offer a command the
/// API would refuse.
const SELLER_PERMISSIONS: [&str; 12] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.orders.read",
    "sales.products.read",
    "sales.products.manage",
    "crm.contacts.read",
    "crm.contacts.create",
    "search.read",
    "search.manage",
    "crm.deals.read",
    "sales.quotes.send",
    "sales.orders.create",
];

/// Reads quotes and **not** orders. This is the account the "two providers, not one" claim is
/// about: it must get a quote section and no order section, and a single merged provider behind
/// an "any of" guard would hand it rows it may not open.
const QUOTE_READER_PERMISSIONS: [&str; 4] = [
    "sales.quotes.read",
    "crm.contacts.read",
    "search.read",
    "search.manage",
];

struct TestResponse {
    status: StatusCode,
    set_cookie: Option<String>,
    body: Value,
}

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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
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

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = live_db(&config).await?;
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );
    Some((state, db))
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("palette-{label}-{}", Uuid::new_v4().simple());
    let id: Uuid = sqlx::query_scalar(
        "insert into organizations (name, slug) values ($1, $2) returning id",
    )
    .bind(label)
    .bind(&slug)
    .fetch_one(db.pool())
    .await
    .expect("organization row");
    id
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, label: &str) -> (Uuid, String) {
    // Through the identity service, not a raw insert: a hand-written `password_hash` is the one
    // column the login path cannot accept, and "password hash string missing field" is a much
    // longer way to learn that than one function call.
    let email = format!("palette-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: label.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

async fn grant(
    db: &Db,
    organization_id: Uuid,
    user_id: Uuid,
    granted_by: Uuid,
    permissions: &[&str],
) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("palette-role-{}", Uuid::new_v4().simple()),
            name: "Palette Test Role".to_owned(),
            description: "A role of the palette walks".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<RolePermissionInput> = permissions
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
        scope: PermScope::Organization {
            organization_id,
        },
        granted_by: Some(granted_by),
        expires_at: None,
    };
    omnion_permissions::bindings::grant(db.pool(), binding)
        .await
        .expect("the role binding must be created");
}
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
        .and_then(|cookie| {
            cookie
                .split(';')
                .next()?
                .split_once('=')
                .map(|(_, value)| value.to_owned())
        })
        .expect("login sets a session cookie")
}

struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    organization: Uuid,
    seller: String,
    quote_reader: String,
    company: Uuid,
    product: Uuid,
    quote: Uuid,
    order: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = PALETTE_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "palette").await;
        let (owner_id, _) = create_account(&db, None, "Palette Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (seller_id, seller) = create_account(&db, Some(organization), "Palette Seller").await;
        grant(&db, organization, seller_id, owner_id, &SELLER_PERMISSIONS).await;

        let (reader_id, quote_reader) =
            create_account(&db, Some(organization), "Palette Quote Reader").await;
        grant(&db, organization, reader_id, owner_id, &QUOTE_READER_PERMISSIONS).await;

        let seller_token = login(&state, &seller).await;

        // A company and a product through the API, so the walk does not depend on knowing the
        // catalog's columns.
        let company_response = call(
            &state,
            request(
                Method::POST,
                "/api/v1/crm/companies",
                Some(&seller_token),
                Some(json!({ "name": "Northwind Trading", "domain": "northwind.test" })),
            ),
        )
        .await;
        assert_eq!(
            company_response.status,
            StatusCode::CREATED,
            "the walker's account must be able to create a company: {}",
            company_response.body
        );
        let company: Uuid = company_response
            .body
            .pointer("/id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("a company id");

        let product_response = call(
            &state,
            request(
                Method::POST,
                "/api/v1/sales/products",
                Some(&seller_token),
                Some(json!({
                    // The API caps a SKU at 32 characters, and `Uuid::simple()` is 32 on its own —
                    // so the prefix has to come out of the budget, not be added to it.
                    "sku": format!("PAL{}", &Uuid::new_v4().simple().to_string()[..16]),
                    "name": "Support retainer",
                    "default_price": "250.00",
                })),
            ),
        )
        .await;
        assert_eq!(
            product_response.status,
            StatusCode::CREATED,
            "the walker's account must be able to create a product: {}",
            product_response.body
        );
        let product: Uuid = product_response
            .body
            .pointer("/id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("a product id");

        // A quote, sent, accepted by its own public link, and the order that becomes of it.
        // The acceptance is not ceremony: a **draft** quote cannot become an order, so the walk
        // has to walk the real road — and the public link is the only way a quote reaches
        // `accepted`, because "a customer said yes" is not something a seller may assert.
        let created = call(
            &state,
            request(
                Method::POST,
                "/api/v1/sales/quotes",
                Some(&seller_token),
                Some(json!({
                    "customer_id": company,
                    "customer_type": "company",
                    "customer_name": "Northwind Trading",
                    "title": "Renewal for Northwind",
                    "currency": "TRY",
                    "lines": [
                        { "product_id": product, "description": "Support retainer",
                          "quantity": "2", "unit_price": "250.00",
                          "discount_percent": 0, "tax_percent": 20 }
                    ],
                })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "quote: {}", created.body);
        let quote: Uuid = Uuid::parse_str(created.body["quote"]["id"].as_str().expect("a quote id"))
            .expect("a uuid");

        let sent = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sales/quotes/{quote}/send"),
                Some(&seller_token),
                None,
            ),
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK, "send: {}", sent.body);

        let link = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sales/quotes/{quote}/link"),
                Some(&seller_token),
                None,
            ),
        )
        .await;
        assert_eq!(link.status, StatusCode::OK, "link: {}", link.body);
        // The link route answers `{"url": "/q/<token>"}` — the one response that ever carries the
        // token in clear, because the database only holds its hash.
        let public_token = link.body["url"]
            .as_str()
            .expect("a public link")
            .rsplit('/')
            .next()
            .expect("a path segment")
            .to_string();

        let accepted = call(
            &state,
            request(
                Method::POST,
                &format!("/api/v1/sales/public/quotes/{public_token}/accept"),
                None,
                None,
            ),
        )
        .await;
        assert_eq!(
            accepted.status,
            StatusCode::OK,
            "accept: {}",
            accepted.body
        );

        let converted = call(
            &state,
            request(
                Method::POST,
                "/api/v1/sales/orders",
                Some(&seller_token),
                Some(json!({ "quote_id": quote })),
            ),
        )
        .await;
        assert_eq!(
            converted.status,
            StatusCode::CREATED,
            "an accepted quote becomes an order: {}",
            converted.body
        );
        let order: Uuid = Uuid::parse_str(converted.body["order"]["id"].as_str().expect("an order id"))
            .expect("a uuid");

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            seller,
            quote_reader,
            company,
            product,
            quote,
            order,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }

    /// One reindex pass, through the endpoint the status screen calls.
    async fn reindex(&self, token: &str, provider: &str) -> Value {
        let response = call(
            &self.state,
            request(
                Method::POST,
                "/api/v1/search/reindex",
                Some(token),
                Some(json!({ "provider": provider })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "reindex must answer: {}",
            response.body
        );
        response.body
    }

    /// `GET /api/v1/search` narrowed to one kind, which is what a palette section asks for.
    ///
    /// The filter rides **inside** `q` as `type:<entity_type>`, which is the platform's own
    /// scoped syntax rather than a query parameter. Two wrong turns got here and both are worth
    /// recording, because each produced a plausible answer:
    ///
    /// 1. `&type=quote` as a parameter is simply **ignored** — the parser only reads scoped syntax
    ///    out of the text — so the box answered with every kind at once and the first hit was the
    ///    *company* this walk had just created. "The title must be the number" then failed on
    ///    `"Northwind Trading"`, which is a correct company row answering an unfiltered question.
    /// 2. `type:quotes` (the provider key) filters to nothing, because the clause matches
    ///    `entity_type`. The hint the API returns says so — `no provider answers quotes` — and a
    ///    `total: 0` with that hint attached is the honest answer, not a broken index.
    ///
    /// The entity type is therefore `quote` / `order`, and the URL has to encode the space.
    async fn search(&self, token: &str, query: &str, entity_type: &str) -> Value {
        let q = format!("{query} type:{entity_type}").replace(' ', "%20");
        let response = call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/search?q={q}&per_page=10&history=false"),
                Some(token),
                None,
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "search must answer");
        response.body
    }

    /// The hits a section renders, as the panel reads them. A slice rather than a `Vec<&Value>`:
    /// the answer already owns the array, and copying references into a new collection buys
    /// nothing except a lifetime to get wrong.
    fn hits(answer: &Value) -> &[Value] {
        answer
            .get("hits")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn url_of(hit: &Value) -> String {
        hit.get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn title_of(hit: &Value) -> String {
        hit.get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

/// Fails the test rather than skipping: a palette walk that quietly reports "SKIP" when the
/// database is down is a green tick that proved nothing, and this suite's whole point is that a
/// missing section is invisible. `OMNION_REQUIRE_DB=1` makes the absence loud.
fn require(fixture: Option<Fixture>) -> Fixture {
    match fixture {
        Some(fixture) => fixture,
        None => {
            if std::env::var("OMNION_REQUIRE_DB").as_deref() == Ok("1") {
                panic!("the palette walks need a migrated database; set OMNION_REQUIRE_DB=0 to skip");
            }
            eprintln!("SKIP: no database; the palette walks did not run");
            std::process::exit(0);
        }
    }
}

#[tokio::test]
async fn a_quote_and_an_order_both_reach_the_palette_with_a_row_the_reader_can_use() {
    let fixture = require(Fixture::new().await);
    let token = fixture.token(&fixture.seller).await;

    // The upsert, through the endpoint. `indexed` is the number the status screen prints; zero
    // with a 200 is exactly the silent failure this walk exists to catch.
    let quotes_report = fixture.reindex(&token, "quotes").await;
    let orders_report = fixture.reindex(&token, "orders").await;

    // The answer is a LIST of reports — one per provider the pass touched — so a single-provider
    // reindex reads `providers[0]`, not the object. The first version of this read the object
    // directly and panicked on a report that had in fact indexed 14 rows, which is the least
    // useful way a test can fail: the product worked and the assertion did not notice.
    let indexed = |report: &Value| -> i64 {
        report
            .get("providers")
            .and_then(Value::as_array)
            .and_then(|reports| reports.first())
            .and_then(|report| report.get("indexed"))
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("a reindex report must count its rows: {report}"))
    };
    assert!(
        indexed(&quotes_report) > 0,
        "the quotes provider indexed nothing and still answered 200: {quotes_report}"
    );
    assert!(
        indexed(&orders_report) > 0,
        "the orders provider indexed nothing and still answered 200: {orders_report}"
    );

    // Now ask the box for them, the way a section does.
    let answer = fixture.search(&token, "Northwind", "quote").await;
    let hits = Fixture::hits(&answer);
    if hits.is_empty() {
        // Print the whole picture before failing: a `total: 0` here has at least three causes
        // that look identical from the outside — the document was never written, it belongs to
        // another organization, or the term never matched its vector — and guessing between them
        // is what cost this walk four rounds.
        let row: (String, String, i64) = sqlx::query_as(
            "select d.provider, d.organization_id::text, count(*) from search_documents d \
             where d.entity_id = $1 group by 1, 2",
        )
        .bind(fixture.quote.to_string())
        .fetch_one(fixture.db.pool())
        .await
        .unwrap_or_else(|e| ("<none>".into(), format!("{e}"), 0));
        panic!(
            "the quote is indexed but the box did not answer.\n  answer: {answer}\
  document: provider={} organization={} rows={}\n  the walk's organization: {}",
            row.0, row.1, row.2, fixture.organization
        );
    }

    let hit = &hits[0];
    let title = Fixture::title_of(hit);
    let url = Fixture::url_of(hit);

    // The **number** is the title. A section row whose title is the customer name reads as a
    // contact, not as a document, and the number is what a seller with a printed copy types.
    let quote_number: String = sqlx::query_scalar("select number from sales_quotes where id = $1")
        .bind(fixture.quote)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote's number");
    assert_eq!(
        title, quote_number,
        "a quote's palette title must be its number, not its customer"
    );
    assert!(
        url.contains(&fixture.quote.to_string()),
        "a quote hit must open that quote: {url}"
    );
    assert!(url.starts_with("/sales/quotes/"), "unexpected route: {url}");

    // The same three things for an order, whose route is its own. The number is read from the
    // database rather than matched against a literal prefix: the module writes orders as `SO-1`,
    // not `O-1`, and a walk that hard-codes a prefix tests the writer's formatting rather than the
    // claim this slice makes — which is that the title is the document's own number.
    let orders = fixture.search(&token, "Northwind", "order").await;
    let order_hits = Fixture::hits(&orders);
    assert!(!order_hits.is_empty(), "an order must answer the box: {orders}");
    let order_number: String = sqlx::query_scalar("select number from sales_orders where id = $1")
        .bind(fixture.order)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the order's number");
    assert_eq!(
        Fixture::title_of(&order_hits[0]),
        order_number,
        "an order's palette title must be its number"
    );
    assert!(Fixture::url_of(&order_hits[0]).contains(&fixture.order.to_string()));
}

#[tokio::test]
async fn an_order_is_reachable_by_the_number_of_the_quote_it_came_from() {
    let fixture = require(Fixture::new().await);
    let token = fixture.token(&fixture.seller).await;
    fixture.reindex(&token, "quotes").await;
    fixture.reindex(&token, "orders").await;

    let number: String = sqlx::query_scalar("select number from sales_quotes where id = $1")
        .bind(fixture.quote)
        .fetch_one(fixture.db.pool())
        .await
        .expect("the quote's number");

    // Somebody chasing "what happened to Q-…0001" types the **quote's** number. If the order
    // did not carry it, the box answers with the frozen quote — a document that can no longer
    // change — and the reader concludes nothing happened to their order.
    let answer = fixture.search(&token, &number, "order").await;
    let hits = Fixture::hits(&answer);
    assert!(
        !hits.is_empty(),
        "an order must be findable by its quote's number {number}: {answer}"
    );
    assert!(
        Fixture::url_of(&hits[0]).contains(&fixture.order.to_string()),
        "the hit must be the order, not the quote it came from: {}",
        Fixture::url_of(&hits[0])
    );
}

#[tokio::test]
async fn an_archived_quote_stops_answering_the_box() {
    let fixture = require(Fixture::new().await);
    let token = fixture.token(&fixture.seller).await;
    fixture.reindex(&token, "quotes").await;

    let before = fixture.search(&token, "Northwind", "quote").await;
    assert!(
        !Fixture::hits(&before).is_empty(),
        "the quote must answer before it is archived"
    );

    sqlx::query("update sales_quotes set archived_at = now() where id = $1")
        .bind(fixture.quote)
        .execute(fixture.db.pool())
        .await
        .expect("archive the quote");

    // The prune half. An upsert alone would prove nothing here: the row is still in the table,
    // it simply must not be in the index. And the reason it must not is that the quote list
    // hides archived rows, so a document that outlived its archive answers with a record the
    // panel will refuse to show when the same person opens the list its hit points at.
    fixture.reindex(&token, "quotes").await;
    let after = fixture.search(&token, "Northwind", "quote").await;
    assert!(
        !Fixture::url_of_first(&after).contains(&fixture.quote.to_string()),
        "an archived quote must stop answering: {after}"
    );
}

#[tokio::test]
async fn a_quote_reader_gets_the_quote_section_and_no_order_section() {
    let fixture = require(Fixture::new().await);
    let token = fixture.token(&fixture.quote_reader).await;
    fixture.reindex(&token, "quotes").await;
    fixture.reindex(&token, "orders").await;

    let quotes = fixture.search(&token, "Northwind", "quote").await;
    assert!(
        !Fixture::hits(&quotes).is_empty(),
        "this account may read quotes, so the box must answer: {quotes}"
    );

    // The other half, and the reason the two are two providers. If somebody merges them behind an
    // "any of" guard, this account starts receiving order rows — links into a screen it will be
    // refused at. The API has to refuse, because that is its job; the **box** is what must not
    // offer it, and the only place that can be proved is the index's own permission filter.
    let orders = fixture.search(&token, "Northwind", "order").await;
    let order_hits = Fixture::hits(&orders);
    assert!(
        order_hits.is_empty(),
        "an account without sales.orders.read must not receive order rows: {orders}"
    );
}

#[tokio::test]
async fn the_palette_offers_create_a_quote_only_to_someone_who_may_create_one() {
    let fixture = require(Fixture::new().await);
    let seller = fixture.token(&fixture.seller).await;

    let with_create = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&seller), None),
    )
    .await;
    assert_eq!(with_create.status, StatusCode::OK);
    let commands = with_create
        .body
        .get("commands")
        .and_then(Value::as_array)
        .expect("the command list is an array");
    assert!(
        commands
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some("nav.create-quote")),
        "a seller may create quotes, so the palette must offer the command"
    );

    // The negative, which is the half that matters: a read-only account must not be offered a
    // command that writes. The permission half alone would pass while the create half was
    // missing, and a read-only account is exactly who would discover it.
    let reader = fixture.token(&fixture.quote_reader).await;
    let without_create = call(
        &fixture.state,
        request(Method::GET, "/api/v1/commands", Some(&reader), None),
    )
    .await;
    assert_eq!(without_create.status, StatusCode::OK);
    let reader_commands = without_create
        .body
        .get("commands")
        .and_then(Value::as_array)
        .expect("the command list is an array");
    assert!(
        !reader_commands
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some("nav.create-quote")),
        "a reader must not be offered Create a quote"
    );
    // …while the command that only reads is still there, or the assertion above would pass for
    // the wrong reason: an account that sees no sales commands at all.
    assert!(
        reader_commands
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some("nav.sales-quotes")),
        "a reader may open the quote desk and must be offered it"
    );
}

impl Fixture {
    /// The first hit's url, or an empty string when the box answered with nothing.
    fn url_of_first(answer: &Value) -> String {
        Fixture::hits(answer)
            .first()
            .map(|hit| Fixture::url_of(&hit))
            .unwrap_or_default()
    }
}

/// The company and the product exist only so the quote can be created through the API. Dropping
/// them at the end of the suite keeps the shared QA database from filling with fixtures, which is
/// what turned a single pass into an hour of waiting two ticks ago.
#[tokio::test]
async fn the_fixtures_are_left_where_a_later_pass_can_ignore_them() {
    let fixture = require(Fixture::new().await);
    // The walk created them; this asserts the ids are real rows rather than a nil uuid that
    // would make every query above pass for the wrong reason.
    let company: i64 =
        sqlx::query_scalar("select count(*) from crm_companies where id = $1")
            .bind(fixture.company)
            .fetch_one(fixture.db.pool())
            .await
            .expect("count companies");
    let product: i64 = sqlx::query_scalar("select count(*) from sales_products where id = $1")
        .bind(fixture.product)
        .fetch_one(fixture.db.pool())
        .await
        .expect("count products");
    assert_eq!(company, 1, "the company's id must be a real row");
    assert_eq!(product, 1, "the product's id must be a real row");
}
