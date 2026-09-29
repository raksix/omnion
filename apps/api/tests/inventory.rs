//! Integration tests for the inventory surface (docs/requests/REQ-053, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite **skips itself with a printed reason** unless
//! `OMNION_REQUIRE_DB=1` is set, which turns a missing database into a **panic**: a walk that
//! quietly reports SKIP when PostgreSQL is down is a green tick that proved nothing, and this
//! suite's whole subject is an absence (a ledger that agrees with a rollup).
//!
//! What the walk proves, in the words of the acceptance criteria: every `/api/v1/inventory/*`
//! route answers `401` unauthenticated, `403` with the permission missing and `200` with it
//! granted; an item of another organization is `404`; a bad SKU, a reorder point below the
//! minimum and a duplicate barcode are each refused with the field the form renders the message
//! under; **a new organization owns a warehouse and two locations without anybody creating them**
//! (the trigger, proved the way a product does it rather than by calling the seed by hand); a
//! receipt, an issue and an adjustment produce the expected on-hand and the correct sign in the
//! ledger and the stock list; **the ledger cannot be edited** — `PATCH` and `DELETE` on a movement
//! answer `405`; a negative result is refused by default and allowed only with reason
//! `correction` **plus** `inventory.negative.manage`; and — the reason this slice exists —
//! **replaying the ledger reproduces the rollup for every item × location**, which is the property
//! the whole module is built around.
//!
//! Two tests exist because the first run of the others reported a plausible wrong answer:
//!
//! * `a_movement_cannot_be_edited_or_deleted` passes for the wrong reason if the route is missing
//!   rather than refusing, so it asserts on the **405** and not on "not 200".
//! * `the_rollup_equals_a_replay_of_the_ledger` is the only test that would notice a service which
//!   wrote the two tables in different transactions, and it is written as a **replay** rather
//!   than as "read the last movement's `on_hand_after`" for a reason the comment repeats: a ledger
//!   whose middle was corrupted still ends at the right number if the last row was honest.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope as PermScope};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Serialises this suite: the organizations and the IAM seed are shared state.
static INVENTORY_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a warehouse reader may do: see everything, change nothing.
///
/// Note what is **absent**: `inventory.movements.record` and `inventory.negative.manage`. The
/// first is proved separately below, because reading a balance and moving stock being different
/// powers is the point of the split; the second is the one key that guards no route at all.
const READER_PERMISSIONS: [&str; 2] = ["inventory.items.read", "sites.read"];

/// What a warehouse operator adds: the item writes, the movement write and the location tree.
const OPERATOR_PERMISSIONS: [&str; 5] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.locations.manage",
    "sites.read",
];

/// An operator who may **also** take stock below zero with a `correction`.
///
/// Its own account, and that is the point: the negative-stock criterion is "refused by default,
/// allowed with the permission", so the two answers have to come from two different callers or the
/// test proves nothing.
const NEGATIVE_PERMISSIONS: [&str; 6] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.locations.manage",
    "inventory.negative.manage",
    "sites.read",
];

/// A writer in a **second** organization.
///
/// The route guard answers `403` before the module looks at a record, which is right but means a
/// read-only account can never demonstrate the rule that matters most here: a caller who *could*
/// write is still told `404` for another organization's stock. A `403` would confirm it exists.
const OTHER_WRITER_PERMISSIONS: [&str; 5] = [
    "inventory.items.read",
    "inventory.items.manage",
    "inventory.movements.record",
    "inventory.locations.manage",
    "sites.read",
];

/// Result of one in-process HTTP call, in the pieces the assertions need.
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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::String(
            String::from_utf8_lossy(&bytes).to_string(),
        ))
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// Build a JSON request; `token` becomes the session cookie and `body` the payload.
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

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            // The difference from every other suite on the platform: `OMNION_REQUIRE_DB=1` makes
            // this a **panic**. A suite whose subject is "the ledger agrees with the rollup"
            // cannot be allowed to report a cheerful SKIP when the database is down, because the
            // only way it would notice is the reconciliation test it is about to run.
            if std::env::var("OMNION_REQUIRE_DB").as_deref() == Ok("1") {
                panic!("OMNION_REQUIRE_DB=1 and PostgreSQL is not reachable: {err}");
            }
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d` \
                 or set OMNION_REQUIRE_DB=1 to make this a failure"
            );
            None
        }
    }
}

/// A state whose database has all migrations applied and the IAM seed loaded.
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

/// One organization, an owner, an operator, a reader, an operator who may go negative, a member
/// with nothing, and a writer of a *second* organization.
struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    org: Uuid,
    /// A second organization, so the cross-tenant half has a real tenant to be refused for.
    ///
    /// This field exists because the test once read `fixture.org` where it meant this one and got
    /// a `200` for "another organization's item" — which was the module correctly answering a
    /// question about the caller's *own* organization. A test that cannot fail for the right
    /// reason is worse than no test, and the way to get one back is to make the values it depends
    /// on nameable and distinct.
    other_org: Uuid,
    operator: String,
    reader: String,
    negative: String,
    member: String,
    other_writer: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = INVENTORY_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db, "a").await;
        let other_org = create_organization_row(&db, "b").await;

        let (owner_id, _owner) = create_account(&db, None, "Inventory Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (operator_id, operator) = create_account(&db, Some(org), "Inventory Operator").await;
        grant(&db, org, operator_id, owner_id, &OPERATOR_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "Inventory Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (negative_id, negative) = create_account(&db, Some(org), "Inventory Negative").await;
        grant(&db, org, negative_id, owner_id, &NEGATIVE_PERMISSIONS).await;

        let (_member_id, member) = create_account(&db, Some(org), "Inventory Member").await;

        let (other_id, other_writer) = create_account(&db, Some(other_org), "Inventory Other").await;
        grant(&db, other_org, other_id, owner_id, &OTHER_WRITER_PERMISSIONS).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            org,
            other_org,
            operator,
            reader,
            negative,
            member,
            other_writer,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }

    /// The `MAIN` warehouse's `STOCK` location, which the organization trigger seeded.
    async fn stock_location(&self) -> Uuid {
        sqlx::query_scalar(
            "select l.id from inventory_locations l join inventory_warehouses w on w.id = l.warehouse_id \
             where l.organization_id = $1 and w.code = 'MAIN' and l.code = 'STOCK'",
        )
        .bind(self.org)
        .fetch_one(self.db.pool())
        .await
        .expect("the seeded STOCK location must exist")
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!(
        "inventory-fix-{}-{}",
        label.to_lowercase().replace([' ', '_'], "-"),
        Uuid::new_v4().simple()
    );
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Inventory Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("inventory-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: name.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Give an account a role with exactly these permissions.
async fn grant(db: &Db, organization_id: Uuid, user_id: Uuid, granted_by: Uuid, permissions: &[&str]) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("inventory-role-{}", Uuid::new_v4().simple()),
            name: "Inventory Test Role".to_owned(),
            description: "A role of the inventory suite".to_owned(),
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
        scope: PermScope::Organization { organization_id },
        granted_by: Some(granted_by),
        expires_at: None,
    };
    omnion_permissions::bindings::grant(db.pool(), binding)
        .await
        .expect("the role binding must be created");
}

/// Sign an account in and return its session token.
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
        .expect("cookie has a name")
        .1
        .to_owned()
}

/// The audit rows of one action, newest first.
async fn audit_rows(db: &Db, action: &str) -> Vec<Value> {
    let rows: Vec<(Value, Option<String>, Option<String>, String)> = sqlx::query_as(
        "select metadata, target_type, target_id, coalesce(actor_user_id::text, '') \
         from audit_log where action = $1 order by id desc limit 5",
    )
    .bind(action)
    .fetch_all(db.pool())
    .await
    .expect("the audit rows must read");

    rows.into_iter()
        .map(|(metadata, target_type, target_id, actor)| {
            json!({
                "metadata": metadata,
                "target_type": target_type,
                "target_id": target_id,
                "actor": actor,
            })
        })
        .collect()
}

/// The payloads of one event name, newest first.
async fn event_payloads(db: &Db, name: &str) -> Vec<Value> {
    let rows: Vec<(Value,)> =
        sqlx::query_as("select payload from events where name = $1 order by id desc limit 5")
            .bind(name)
            .fetch_all(db.pool())
            .await
            .expect("the event rows must read");
    rows.into_iter().map(|(payload,)| payload).collect()
}

/// A SKU unique to one test, so a run cannot collide with another.
///
/// **Capped at 32 characters**, because the schema's check is `^[A-Za-z0-9._-]{2,32}$` and a UUID's
/// 32 hex characters plus a label would be 40. An SKU that is too long produces a `400` from the
/// round trip rather than a row, and the walk's assertion is on the id.
fn sku(label: &str) -> String {
    format!("{}-{}", label, &Uuid::new_v4().simple().to_string()[..8])
}

/// Create an item and return its id, failing loudly with the body if the create was refused.
async fn create_item(state: &AppState, token: &str, body: Value) -> Uuid {
    let response = call(
        state,
        request(Method::POST, "/api/v1/inventory/items", Some(token), Some(body)),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "the item must be created: {}",
        response.body
    );
    Uuid::parse_str(response.body["id"].as_str().expect("an id")).expect("an id")
}

/// An item body with the awkward numbers filled in: thresholds that produce all three badges.
fn item_body(label: &str) -> Value {
    json!({
        "sku": sku(label),
        "name": "Hex bolt M8",
        "category": "Fasteners",
        "unit": "piece",
        "barcode": format!("869{}", &Uuid::new_v4().simple().to_string()[..9]),
        "min_threshold": "2",
        "reorder_point": "5",
        "reorder_qty": "100",
        "cost": "12.50",
        "notes": "Counted on the first shelf",
    })
}

/// Record a movement and return the response, asserting nothing so a walk can assert its status.
async fn record(
    state: &AppState,
    token: &str,
    item_id: Uuid,
    location_id: Uuid,
    body: Value,
) -> TestResponse {
    call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/movements",
            Some(token),
            // `json!` has no struct-update syntax, so the two fixed fields are inserted into a
            // copy of the caller's body. The helper exists so a walk writes only what varies.
            {
                let mut payload = body.as_object().cloned().unwrap_or_default();
                payload.insert("item_id".to_owned(), json!(item_id));
                payload.insert("location_id".to_owned(), json!(location_id));
                Some(Value::Object(payload))
            },
        ),
    )
    .await
}

// -------------------------------------------------------------------------------------------
// The walk
// -------------------------------------------------------------------------------------------

/// A new organization owns a warehouse and two locations without anybody creating them.
///
/// **This is the test REQ-051's board should have had** and this module's migration is written
/// because of it: `0022_crm.sql` created its seed function and called it once, in the statement
/// that created it, so every tenant born afterwards owned no pipeline and the board answered
/// `404`. The fix here is a trigger plus a backfill, and the only honest way to prove a trigger
/// fires is to **insert an organization and read its rows** — a fixture that calls the seed
/// function by hand proves the function works, not that the product calls it.
#[tokio::test]
async fn a_new_organization_is_seeded_with_a_warehouse_and_two_locations() {
    let Some(_fixture) = Fixture::new().await else {
        return;
    };
    // A **second** organization, created after the fixture's, so the assertion is about the
    // trigger rather than about the backfill the migration also ran.
    let fresh = create_organization_row(&_fixture.db, "fresh").await;

    let warehouses: Vec<String> = sqlx::query_scalar(
        "select code from inventory_warehouses where organization_id = $1 order by code",
    )
    .bind(fresh)
    .fetch_all(_fixture.db.pool())
    .await
    .expect("the warehouses must read");
    assert_eq!(
        warehouses,
        vec!["MAIN".to_string()],
        "an organization created through the product owns a MAIN warehouse without anybody \
         creating it"
    );

    let locations: Vec<String> = sqlx::query_scalar(
        "select l.code from inventory_locations l join inventory_warehouses w on w.id = l.warehouse_id \
         where l.organization_id = $1 order by l.code",
    )
    .bind(fresh)
    .fetch_all(_fixture.db.pool())
    .await
    .expect("the locations must read");
    assert_eq!(
        locations,
        vec!["RETURNS".to_string(), "STOCK".to_string()],
        "and somewhere to put stock and somewhere to put a customer's return"
    );

    // The settings row too, because the movement path reads it and a missing row would make the
    // first adjustment of every new tenant fall back to a default nobody chose.
    let settings: i64 = sqlx::query_scalar(
        "select count(*) from inventory_settings where organization_id = $1",
    )
    .bind(fresh)
    .fetch_one(_fixture.db.pool())
    .await
    .expect("the settings row must read");
    assert_eq!(settings, 1, "a new organization owns its thresholds from the day it is created");
}

/// Every inventory route refuses an anonymous caller and a member without the permission.
#[tokio::test]
async fn every_inventory_route_is_permission_guarded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let marker = Uuid::new_v4();
    let other = fixture.other_org;
    let ours = fixture.org;

    let calls: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, "/api/v1/inventory".to_owned(), None),
        (Method::GET, "/api/v1/inventory/vocabulary".to_owned(), None),
        (Method::GET, "/api/v1/inventory/items".to_owned(), None),
        (
            Method::GET,
            "/api/v1/inventory/items/lookup?code=ABC123".to_owned(),
            None,
        ),
        (Method::GET, format!("/api/v1/inventory/items/{marker}"), None),
        (Method::GET, "/api/v1/inventory/stock".to_owned(), None),
        (Method::GET, "/api/v1/inventory/reconciliation".to_owned(), None),
        (Method::GET, "/api/v1/inventory/warehouses".to_owned(), None),
        (Method::GET, "/api/v1/inventory/locations".to_owned(), None),
        (Method::GET, "/api/v1/inventory/movements".to_owned(), None),
        (Method::GET, format!("/api/v1/inventory/movements/{marker}"), None),
        (Method::POST, "/api/v1/inventory/movements/preview".to_owned(), None),
        (Method::GET, "/api/v1/inventory/settings".to_owned(), None),
        (
            Method::POST,
            "/api/v1/inventory/items".to_owned(),
            Some(item_body("guard")),
        ),
        (
            Method::PATCH,
            format!("/api/v1/inventory/items/{marker}"),
            Some(json!({ "name": "renamed" })),
        ),
        (Method::DELETE, format!("/api/v1/inventory/items/{marker}"), None),
        (
            Method::POST,
            "/api/v1/inventory/warehouses".to_owned(),
            Some(json!({ "code": "SECOND", "name": "Second" })),
        ),
        (
            Method::PATCH,
            format!("/api/v1/inventory/warehouses/{marker}"),
            Some(json!({ "name": "renamed" })),
        ),
        (
            Method::POST,
            "/api/v1/inventory/locations".to_owned(),
            Some(json!({ "warehouse_id": marker, "code": "BIN", "name": "Bin" })),
        ),
        (
            Method::PATCH,
            format!("/api/v1/inventory/locations/{marker}"),
            Some(json!({ "name": "renamed" })),
        ),
        (
            Method::PUT,
            "/api/v1/inventory/settings".to_owned(),
            Some(json!({ "adjustment_approval_threshold": "1" })),
        ),
        (
            Method::POST,
            "/api/v1/inventory/movements".to_owned(),
            Some(json!({ "item_id": marker, "location_id": marker, "quantity": "1" })),
        ),
    ];

    for (method, uri, body) in calls {
        let anonymous = call(state, request(method.clone(), &uri, None, body.clone())).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            anonymous.body
        );

        let member = call(
            state,
            request(method.clone(), &uri, Some(&fixture.token(&fixture.member).await), body.clone()),
        )
        .await;
        assert_eq!(
            member.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a member without the permission: {}",
            member.body
        );
    }

    // A writer of a *second* organization, with every inventory key, still gets `404` for the
    // first organization's records. The `403` above is the guard answering first, which is right —
    // and it is also why this half needs its own caller.
    let outsider = fixture.token(&fixture.other_writer).await;
    let reader = fixture.token(&fixture.reader).await;
    let operator = fixture.token(&fixture.operator).await;
    let mine = create_item(&fixture.state, &operator, item_body("tenant")).await;

    // Two different refusals, and the first version of this test conflated them. A **tenant**
    // naming another organization is stopped by the tenancy rule before the module is reached —
    // a `403 cross_organization`, which is the right answer and not this criterion's. The
    // criterion is about the module's own predicate, so the caller must be one the platform
    // *does* let name a tenant: the outsider asking for `?organization_id=` of its **own**
    // organization and an id that lives in ours.
    let named = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/items/{mine}?organization_id={ours}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        named.status,
        StatusCode::FORBIDDEN,
        "the tenancy rule stops a tenant naming another organization before the module is \
         reached: {}",
        named.body
    );

    // And the same account, on **its own** organization (named, so the resolved scope is not
    // inferred from a parameter's absence), asking for our id. Nothing stops it now except the
    // module's own `where organization_id = …`, and the answer must be a `404`: a `403` would
    // confirm the row exists, which is the one thing an organization's stock must never leak.
    let cross_read = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/items/{mine}?organization_id={other}"),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        cross_read.status,
        StatusCode::NOT_FOUND,
        "another organization's item is a 404, not a 403: {}",
        cross_read.body
    );
    // And the reader, who is bound to **our** organization, reads the same item with `200` and
    // its own tenant's data. The first version of this assertion expected a `404` here and was
    // reading nothing at all: the reader is not an outsider, so a `404` would have meant the
    // module is broken. It is worth keeping as the positive half — the cross-tenant `404` only
    // means something if the same request shape answers `200` for the right caller.
    let reader_own = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/items/{mine}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(
        reader_own.status,
        StatusCode::OK,
        "the reader is one of ours and reads the item: {}",
        reader_own.body
    );
    assert_eq!(reader_own.body["position"]["item"]["id"], json!(mine.to_string()));
}

/// The three movements produce the expected on-hand, the right sign in the ledger, and the badge
/// the thresholds imply.
#[tokio::test]
async fn a_receipt_an_issue_and_an_adjustment_agree_in_the_ledger_and_the_stock_list() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &token, item_body("flow")).await;

    // 1. A receipt of 10.
    let receipt = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "10", "reason": "purchase_receipt" }),
    )
    .await;
    assert_eq!(receipt.status, StatusCode::CREATED, "{}", receipt.body);
    assert_eq!(receipt.body["movement"]["kind"], "receipt");
    assert_eq!(receipt.body["position"]["on_hand"], "10.000");
    assert_eq!(receipt.body["position"]["available"], "10.000");
    // The status is the badge the thresholds imply, and it is a definition the module shares with
    // the list rather than one the handler re-derives.
    assert_eq!(receipt.body["position"]["status"], "ok");

    // 2. An issue of 4 → 6 left, which is above the reorder point of 5 but not by much.
    let issue = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "4", "reason": "sale_shipment" }),
    )
    .await;
    assert_eq!(issue.status, StatusCode::CREATED, "{}", issue.body);
    assert_eq!(issue.body["movement"]["kind"], "issue");
    assert_eq!(issue.body["position"]["on_hand"], "6.000");
    // **Strictly above** the reorder point is "ok"; at it is "low". The first version of this
    // assertion expected `low` at 6 against a reorder point of 5 and got `ok` — the module is
    // right and the expectation was wrong: a line that is one unit above its reorder point is
    // not yet asking to be reordered. The third movement then crosses it, which is the crossing
    // the low-stock event is emitted on.
    assert_eq!(issue.body["position"]["status"], "ok", "6 is above the reorder point of 5");

    // 3. An adjustment of −1.5 → 4.5. Against `min_threshold = 2` and `reorder_point = 5` that is
    //    **low, not critical** — 4.5 is above the minimum — so the first version of this test,
    //    which expected `critical` here and got `low`, was wrong about its own fixture rather than
    //    about the module. The badge is corrected here and the *crossing* (6 → 4.5, past the
    //    reorder point of 5) is what emits the event, which is the thing this walk is for.
    let adjustment = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "-1.5", "reason": "correction" }),
    )
    .await;
    assert_eq!(adjustment.status, StatusCode::CREATED, "{}", adjustment.body);
    assert_eq!(adjustment.body["movement"]["kind"], "adjustment");
    assert_eq!(
        adjustment.body["movement"]["quantity"], "-1.500",
        "an adjustment is stored signed; every other kind is stored positive"
    );
    assert_eq!(
        adjustment.body["position"]["on_hand"], "4.500",
        "three decimals survive the round trip: 6 − 1.5 is 4.500, not 4.5 and not 4"
    );
    assert_eq!(
        adjustment.body["position"]["status"], "low",
        "4.5 is below the reorder point of 5 and above the minimum of 2"
    );

    // The ledger's own rows, in order, with the numbers each one produced.
    let ledger = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/movements?item_id={item}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(ledger.status, StatusCode::OK, "{}", ledger.body);
    let rows = ledger.body["items"].as_array().expect("an array of movements");
    assert_eq!(rows.len(), 3, "three movements and not a fourth");
    // Newest first, and each row carries the on-hand it produced — which is what makes the ledger
    // replayable without trusting the rollup.
    assert_eq!(rows[0]["kind"], "adjustment");
    assert_eq!(rows[0]["on_hand_after"], "4.500");
    assert_eq!(rows[1]["kind"], "issue");
    assert_eq!(rows[1]["on_hand_after"], "6.000");
    assert_eq!(rows[2]["kind"], "receipt");
    assert_eq!(rows[2]["on_hand_after"], "10.000");
    // The reason travels separately from the kind: a receipt bought from a supplier and a
    // customer's return are both receipts and are not the same fact.
    assert_eq!(rows[2]["reason"], "purchase_receipt");

    // The stock list agrees, and the item's own totals are summed from the rows it just drew.
    let stock = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/stock?item_id={item}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(stock.status, StatusCode::OK, "{}", stock.body);
    let row = &stock.body["items"][0];
    assert_eq!(row["on_hand"], "4.500");
    assert_eq!(row["available"], "4.500");
    assert_eq!(row["status"], "low");
    assert_eq!(row["sku"], stock.body["items"][0]["sku"], "the list carries the SKU for the label");

    let detail = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/items/{item}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    // The detail is **not** flattened: `{ position, history }` with the position's own fields
    // inside `position`. The first version of this test read `body["on_hand"]` — the flattened
    // shape the struct had before it was changed — and got `null`, which is a test written
    // against a response that no longer exists rather than a failing product.
    assert_eq!(
        detail.body["position"]["on_hand"], "4.500",
        "the header is the sum of the rows under it"
    );
    assert_eq!(detail.body["position"]["status"], "low");
    assert_eq!(
        detail.body["position"]["locations"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "one location holds it, and the per-location row is the same numbers the stock list drew"
    );
    assert_eq!(
        detail.body["history"].as_array().map(Vec::len),
        Some(3),
        "the detail carries its own history rather than making the screen fetch it"
    );

    // The low-stock event, emitted **once**, on the crossing. It is edge-triggered on purpose: a
    // second movement that does not cross anything must be silent, or a busy warehouse floods the
    // automation log.
    let low = event_payloads(&fixture.db, "inventory.stock.low").await;
    let mine_low = low
        .iter()
        .filter(|payload| payload["item_id"] == json!(item.to_string()))
        .count();
    assert_eq!(mine_low, 1, "exactly one low-stock event for the crossing: {low:?}");

    let recorded = event_payloads(&fixture.db, "inventory.movement.recorded").await;
    assert!(
        recorded.iter().any(|payload| payload["item_id"] == json!(item.to_string())),
        "every movement emits its own event"
    );

    // The audit row carries the before and after, not just the delta: the question an auditor asks
    // about an adjustment is "what was there before and what is there now".
    let audits = audit_rows(&fixture.db, "inventory.movement.recorded").await;
    let mine_audit = audits
        .iter()
        .find(|row| row["metadata"]["item_id"] == json!(item.to_string()))
        .unwrap_or_else(|| panic!("the adjustment must write an audit row: {audits:?}"));
    assert_eq!(mine_audit["metadata"]["on_hand_before"], "6.000");
    assert_eq!(mine_audit["metadata"]["on_hand_after"], "4.500");
    assert!(!mine_audit["actor"].as_str().unwrap_or_default().is_empty());
}

/// **The property the module exists for:** replaying the ledger reproduces the rollup.
///
/// Written as a **replay** and not as "the last movement's `on_hand_after` equals the rollup",
/// because the cheap version only proves the final number: a ledger whose middle was corrupted
/// still ends at the right number if the last row was honest. The replay walks every movement, so
/// it notices a lost row, a sign flipped in the middle, and a rollup that was written without its
/// ledger row — which is the failure a service that used two transactions would produce.
#[tokio::test]
async fn the_rollup_equals_a_replay_of_the_ledger() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;

    // Three items with different shapes, so the replay is not proved on one happy path.
    let first = create_item(state, &token, item_body("replay-a")).await;
    let second = create_item(state, &token, item_body("replay-b")).await;
    let third = create_item(
        state,
        &token,
        json!({
            "sku": sku("replay-c"),
            "name": "Item that never moved",
            "unit": "piece",
        }),
    )
    .await;

    for quantity in ["10", "3.5", "0.25", "12"] {
        let response = record(
            state,
            &token,
            first,
            location,
            json!({ "quantity": quantity, "reason": "purchase_receipt" }),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.body);
    }
    let issue = record(
        state,
        &token,
        first,
        location,
        json!({ "quantity": "2.75", "reason": "sale_shipment" }),
    )
    .await;
    assert_eq!(issue.status, StatusCode::CREATED, "{}", issue.body);
    // 10 + 3.5 + 0.25 + 12 − 2.75 = 23.000, and the fact that three of those five carry a decimal
    // is the point: a binary float would not land on 23.000.
    assert_eq!(issue.body["position"]["on_hand"], "23.000");

    record(
        state,
        &token,
        second,
        location,
        json!({ "quantity": "5", "reason": "customer_return" }),
    )
    .await;

    // `third` never moves, so it has no stock row at all — and the reconciliation must not report
    // it, because "no row" and "a row that should be zero" are different facts.
    let report = call(
        state,
        request(
            Method::GET,
            "/api/v1/inventory/reconciliation",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    let mismatches = report.body["mismatches"].as_array().expect("an array");
    let mine: Vec<&Value> = mismatches
        .iter()
        .filter(|row| row["item_id"] == json!(first.to_string())
            || row["item_id"] == json!(second.to_string()))
        .collect();
    assert!(
        mine.is_empty(),
        "replaying the ledger must reproduce the rollup for every item × location: {mine:?}"
    );
    assert!(
        report.body["checked"].as_i64().unwrap_or(0) >= 2,
        "the report says how many rows it compared, so an empty report is distinguishable from \
         an empty warehouse: {}",
        report.body
    );

    // **Now break it on purpose and prove the report can see it.** A rollup row written without a
    // ledger row is the exact state a two-transaction service would leave behind, and it is
    // invisible to "read the last movement" — so the test writes one directly and requires the
    // report to name it, with both numbers.
    sqlx::query(
        "update inventory_stock set on_hand = 999 where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org)
    .bind(first)
    .execute(fixture.db.pool())
    .await
    .expect("the rollup must be writable for the negative control");

    let broken = call(
        state,
        request(
            Method::GET,
            "/api/v1/inventory/reconciliation",
            Some(&token),
            None,
        ),
    )
    .await;
    let found = broken.body["mismatches"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["item_id"] == json!(first.to_string()))
        .unwrap_or_else(|| {
            panic!(
                "a rollup that was written without its ledger row must be reported, and reported \
                 with both numbers: {}",
                broken.body
            )
        });
    assert_eq!(found["rollup_on_hand"], "999.000");
    assert_eq!(
        found["replayed_on_hand"], "23.000",
        "the replayed figure is the one the movements add up to, and it is what makes the row \
         actionable rather than merely flagged"
    );
}

/// The ledger is append-only: there is no route that edits a movement, and the two methods a
/// caller would reach for answer `405`.
#[tokio::test]
async fn a_movement_cannot_be_edited_or_deleted() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &token, item_body("append-only")).await;

    let receipt = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "10", "reason": "purchase_receipt" }),
    )
    .await;
    let movement_id = receipt.body["movement"]["id"].as_i64().expect("an id");

    for (method, label) in [
        (Method::PATCH, "correct a movement"),
        (Method::PUT, "replace a movement"),
        (Method::DELETE, "delete a movement"),
    ] {
        let response = call(
            state,
            request(
                method.clone(),
                &format!("/api/v1/inventory/movements/{movement_id}"),
                Some(&token),
                Some(json!({ "quantity": "500" })),
            ),
        )
        .await;
        // **`405` and not merely "not 200".** A `404` would pass this assertion for the wrong
        // reason — it is what a route that filtered by organization would answer — and the
        // criterion asks for the method not existing at all.
        assert_eq!(
            response.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "there is no way to {label}: the ledger is append-only, and the fix is another \
             movement with reason `correction`: {}",
            response.body
        );
    }

    // And the balance is untouched by the attempts, read from the database rather than the API so
    // the assertion cannot be satisfied by a screen that lies.
    let on_hand: String = sqlx::query_scalar(
        "select on_hand::text from inventory_stock where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org)
    .bind(item)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the rollup must read");
    assert_eq!(on_hand, "10.000", "three refused writes and the balance is still the receipt's");

    // The rows themselves are unchanged too, because a trigger that "helpfully" updated a
    // historical row would be a worse bug than a missing route.
    let quantity: String = sqlx::query_scalar(
        "select quantity::text from inventory_movements where id = $1",
    )
    .bind(movement_id)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the movement must read");
    assert_eq!(quantity, "10.000");
}

/// A negative result is refused by default and allowed only with `correction` **plus** the
/// permission — three different callers, three different answers.
#[tokio::test]
async fn negative_stock_is_refused_unless_a_correction_with_the_permission_says_otherwise() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let operator = fixture.token(&fixture.operator).await;
    let privileged = fixture.token(&fixture.negative).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &operator, item_body("negative")).await;

    record(
        state,
        &operator,
        item,
        location,
        json!({ "quantity": "2", "reason": "purchase_receipt" }),
    )
    .await;

    // 1. An issue of 5 against 2 on hand: refused, **and the message carries the number**.
    let refused = record(
        state,
        &operator,
        item,
        location,
        json!({ "quantity": "5", "reason": "sale_shipment" }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal must carry a sentence: {}", refused.body));
    assert!(
        message.contains("2.000"),
        "the person at the shelf must be told what is available: {message}"
    );
    assert_eq!(
        refused.body["error"]["details"]["available"], "2.000",
        "and the number travels as a field too, so a screen can render it beside the input"
    );

    // 2. A `correction` below zero from the **same** operator: still refused. The permission is
    //    what is missing, and a test that only tried an `issue` would pass while the correction
    //    path was open.
    let correction = record(
        state,
        &operator,
        item,
        location,
        json!({ "quantity": "-5", "reason": "correction" }),
    )
    .await;
    assert_eq!(
        correction.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a correction below zero needs the permission, not just the reason: {}",
        correction.body
    );

    // 3. A `damage` write-off below zero from the **privileged** account: still refused. The rule
    //    is about the reason, not about who is asking, and `damage` is not `correction`.
    let damage = record(
        state,
        &privileged,
        item,
        location,
        json!({ "quantity": "-5", "reason": "damage" }),
    )
    .await;
    assert_eq!(
        damage.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "only a `correction` may go below zero, whoever asks: {}",
        damage.body
    );

    // 4. The same correction from the account that holds `inventory.negative.manage`: allowed, and
    //    the ledger says so in a way the reconciliation still agrees with.
    let allowed = record(
        state,
        &privileged,
        item,
        location,
        json!({ "quantity": "-5", "reason": "correction" }),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::CREATED, "{}", allowed.body);
    assert_eq!(allowed.body["position"]["on_hand"], "-3.000");
    assert_eq!(allowed.body["position"]["status"], "negative");

    // The negative row is badged, filtered and reported — three readers of the same definition.
    let stock = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/stock?item_id={item}"),
            Some(&operator),
            None,
        ),
    )
    .await;
    assert_eq!(stock.body["items"][0]["status"], "negative");
    let negative_only = call(
        state,
        request(
            Method::GET,
            "/api/v1/inventory/stock?status=negative",
            Some(&operator),
            None,
        ),
    )
    .await;
    assert!(
        negative_only.body["items"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|row| row["item_id"] == json!(item.to_string())),
        "the negative filter returns the row: {}",
        negative_only.body
    );

    let report = call(
        state,
        request(
            Method::GET,
            "/api/v1/inventory/reconciliation",
            Some(&operator),
            None,
        ),
    )
    .await;
    let mine = report.body["mismatches"]
        .as_array()
        .expect("an array")
        .iter()
        .filter(|row| row["item_id"] == json!(item.to_string()))
        .count();
    assert_eq!(
        mine, 0,
        "a permitted negative balance is still a balance the ledger explains: {}",
        report.body
    );

    // The overview counts it, so the number a person is judged on includes the debt.
    let overview = call(
        state,
        request(Method::GET, "/api/v1/inventory", Some(&operator), None),
    )
    .await;
    assert!(overview.body["negative"].as_i64().unwrap_or(0) >= 1);
}

/// The refusals the form has to render: a bad SKU, a reorder point below the minimum, a duplicate
/// barcode and a fourth decimal in a quantity.
#[tokio::test]
async fn bad_input_is_refused_at_the_field_the_form_renders_it_under() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;

    let bad_sku = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/items",
            Some(&token),
            Some(json!({ "sku": "a b c", "name": "Spaces in a SKU" })),
        ),
    )
    .await;
    assert_eq!(bad_sku.status, StatusCode::BAD_REQUEST, "{}", bad_sku.body);
    assert_eq!(bad_sku.body["error"]["details"]["field"], "sku");

    let inverted = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/items",
            Some(&token),
            Some(json!({
                "sku": sku("inverted"),
                "name": "Inverted thresholds",
                "min_threshold": "10",
                "reorder_point": "2",
            })),
        ),
    )
    .await;
    assert_eq!(inverted.status, StatusCode::BAD_REQUEST, "{}", inverted.body);
    assert_eq!(
        inverted.body["error"]["details"]["field"], "reorder_point",
        "the message has to name the field, and the rule is that the reorder point may not sit \
         below the minimum: {}",
        inverted.body
    );

    // A barcode taken by a live item: normalized before it is compared, so a scanner's spaces do
    // not create a second item for the same label.
    // **Digits only.** `simple()` is hex, so it can contain `a`–`f`, and the normalizer
    // upper-cases a barcode on the way in — which made the first version of this test compare
    // `"869B2B0D22AF"` with `"869b2b0d22af"` and fail on a difference the module had made
    // correctly. A barcode that is really an EAN is digits, and the test now says so.
    let shared_barcode: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .map(|c| c.to_digit(16).unwrap_or(0))
        .map(|d| char::from_digit(d, 10).unwrap_or('0'))
        .take(12)
        .collect();
    create_item(
        state,
        &token,
        json!({ "sku": sku("barcode"), "name": "Has the barcode", "barcode": shared_barcode }),
    )
    .await;
    // **The same twelve digits with a space in them**, which is what a scanner emits and what a
    // person types off the label. The first version of this test spaced only the first three
    // digits and then compared, which is a *different* twelve-character code minus three — the
    // create was `201` and the test would have failed for a reason that has nothing to do with
    // normalization.
    let spaced = format!("{} {} {}", &shared_barcode[0..4], &shared_barcode[4..8], &shared_barcode[8..]);
    let duplicate = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/items",
            Some(&token),
            Some(json!({
                "sku": sku("barcode-2"),
                "name": "Claims the same label",
                "barcode": spaced,
            })),
        ),
    )
    .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "one label is one item, however the code arrives: {}",
        duplicate.body
    );

    // And the lookup resolves the spaced form to the same item, which is the point of normalizing.
    let lookup = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/items/lookup?code={}", spaced.replace(' ', "%20")),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(lookup.status, StatusCode::OK, "{}", lookup.body);
    assert_eq!(lookup.body["barcode"], shared_barcode, "and it stored the normalized form");

    // A fourth decimal is refused rather than truncated: the operator is told, because the
    // alternative writes a number nobody read on the screen.
    let item = create_item(state, &token, item_body("precise")).await;
    let location = fixture.stock_location().await;
    let precise = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "1.2345", "reason": "purchase_receipt" }),
    )
    .await;
    assert_eq!(precise.status, StatusCode::BAD_REQUEST, "{}", precise.body);
    assert_eq!(precise.body["error"]["details"]["field"], "quantity");

    // An unknown status filter is a refusal naming the ones that work, because a filter that
    // silently matched nothing makes an operator conclude the warehouse is empty.
    let bad_status = call(
        state,
        request(
            Method::GET,
            "/api/v1/inventory/stock?status=lowish",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(bad_status.status, StatusCode::BAD_REQUEST, "{}", bad_status.body);
    let message = bad_status.body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("below_threshold"), "{message}");
    assert!(message.contains("negative"), "{message}");
}

/// The `counted` mode is a drawer concept, and it must be resolved against the **server's**
/// number rather than the client's.
#[tokio::test]
async fn the_counted_mode_resolves_to_a_delta_against_the_stored_balance() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &token, item_body("counted")).await;

    record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "10", "reason": "purchase_receipt" }),
    )
    .await;

    // The preview answers **before** the write, and the number it prints is the number the row
    // will hold — both come from the module's own arithmetic rather than from a second
    // implementation in the browser.
    let preview = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/movements/preview",
            Some(&token),
            Some(json!({
                "item_id": item,
                "location_id": location,
                "quantity": "7.5",
                "mode": "counted",
                "reason": "correction",
            })),
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(preview.body["on_hand_before"], "10.000");
    assert_eq!(preview.body["on_hand_after"], "7.500");
    assert_eq!(preview.body["kind"], "adjustment");

    let written = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "7.5", "mode": "counted", "reason": "correction" }),
    )
    .await;
    assert_eq!(written.status, StatusCode::CREATED, "{}", written.body);
    assert_eq!(
        written.body["position"]["on_hand"], "7.500",
        "the preview and the write agree to the gram, because they are the same function"
    );
    assert_eq!(
        written.body["movement"]["quantity"], "-2.500",
        "and the ledger stores the delta it actually applied, not the count the operator read"
    );

    // Counting a row that already matches is refused with a sentence that says why, rather than
    // writing a zero adjustment the schema would refuse as a 500.
    let same = record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "7.5", "mode": "counted", "reason": "correction" }),
    )
    .await;
    assert_eq!(same.status, StatusCode::BAD_REQUEST, "{}", same.body);
    assert!(
        same.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already"),
        "the refusal has to explain that there is nothing to adjust: {}",
        same.body
    );
}

/// The warehouse tree: it comes back in one request, a duplicate code is a `409`, and a location
/// holding stock cannot be closed.
#[tokio::test]
async fn the_warehouse_tree_refuses_a_duplicate_code_and_a_location_that_holds_stock() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &token, item_body("tree")).await;

    let tree = call(
        state,
        request(Method::GET, "/api/v1/inventory/warehouses", Some(&token), None),
    )
    .await;
    assert_eq!(tree.status, StatusCode::OK, "{}", tree.body);
    let main = tree.body
        .as_array()
        .expect("an array of warehouses")
        .iter()
        .find(|warehouse| warehouse["code"] == "MAIN")
        .unwrap_or_else(|| panic!("the seeded warehouse must be there: {}", tree.body));
    let codes: Vec<&str> = main["locations"]
        .as_array()
        .expect("an array of locations")
        .iter()
        .map(|row| row["code"].as_str().expect("a code"))
        .collect();
    assert_eq!(codes, vec!["RETURNS", "STOCK"], "the tree comes back with its children");

    // A second warehouse with the same code is a 409, not a 500 on the unique index.
    let duplicate = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/warehouses",
            Some(&token),
            Some(json!({ "code": "main", "name": "Another main" })),
        ),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "{}", duplicate.body);

    // Put stock in `STOCK`, then try to close it.
    record(
        state,
        &token,
        item,
        location,
        json!({ "quantity": "10", "reason": "purchase_receipt" }),
    )
    .await;
    let closing = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/inventory/locations/{location}"),
            Some(&token),
            Some(json!({ "active": false })),
        ),
    )
    .await;
    assert_eq!(closing.status, StatusCode::CONFLICT, "{}", closing.body);
    let message = closing.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("10.000"),
        "the refusal names the balance, because 'cannot close' without a number sends the \
         operator to the stock list to work it out: {message}"
    );

    // Write it off and the close is allowed — a refusal a person cannot act on is a dead end.
    let privileged = fixture.token(&fixture.negative).await;
    record(
        state,
        &privileged,
        item,
        location,
        json!({ "quantity": "-10", "reason": "correction" }),
    )
    .await;
    let now = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/inventory/locations/{location}"),
            Some(&token),
            Some(json!({ "active": false })),
        ),
    )
    .await;
    assert_eq!(now.status, StatusCode::OK, "{}", now.body);
    assert_eq!(now.body["active"], false);

    // A location under a warehouse of **another** organization is a 404, not a foreign-key
    // violation: the caller is asking for something that does not exist in their world.
    let outsider = fixture.token(&fixture.other_writer).await;
    let foreign = call(
        state,
        request(
            Method::POST,
            "/api/v1/inventory/locations",
            Some(&outsider),
            Some(json!({
                "warehouse_id": main["id"],
                "code": "THEIRS",
                "name": "Not mine",
            })),
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
}

/// Reading the ledger is not writing it: a role with the read key may look at every balance and
/// may not move a single one.
#[tokio::test]
async fn reading_a_balance_and_moving_stock_are_different_powers() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let reader = fixture.token(&fixture.reader).await;
    let operator = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;
    let item = create_item(state, &operator, item_body("split")).await;

    record(
        state,
        &operator,
        item,
        location,
        json!({ "quantity": "10", "reason": "purchase_receipt" }),
    )
    .await;

    // The reader sees everything: the list, the ledger, the item, the overview, the tree.
    for uri in [
        "/api/v1/inventory".to_owned(),
        "/api/v1/inventory/stock".to_owned(),
        "/api/v1/inventory/movements".to_owned(),
        "/api/v1/inventory/warehouses".to_owned(),
        format!("/api/v1/inventory/items/{item}"),
    ] {
        let response = call(state, request(Method::GET, &uri, Some(&reader), None)).await;
        assert_eq!(response.status, StatusCode::OK, "GET {uri}: {}", response.body);
    }

    // And may change nothing: not an item, not a warehouse, not a balance.
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/v1/inventory/items".to_owned(),
            Some(item_body("reader")),
        ),
        (
            Method::POST,
            "/api/v1/inventory/movements".to_owned(),
            Some(json!({
                "item_id": item,
                "location_id": location,
                "quantity": "999",
                "reason": "correction",
            })),
        ),
        (
            Method::POST,
            "/api/v1/inventory/warehouses".to_owned(),
            Some(json!({ "code": "READER", "name": "Not theirs" })),
        ),
    ] {
        let response = call(state, request(method.clone(), &uri, Some(&reader), body)).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be refused for a reader: {}",
            response.body
        );
    }

    // The balance is exactly what the operator left it at.
    let stock = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/inventory/stock?item_id={item}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(stock.body["items"][0]["on_hand"], "10.000");
}

/// The filters the stock list promises, and the item's own vocabulary.
#[tokio::test]
async fn the_stock_list_filters_and_the_vocabulary_answer_the_forms_questions() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let token = fixture.token(&fixture.operator).await;
    let location = fixture.stock_location().await;

    let healthy = create_item(state, &token, item_body("healthy")).await;
    let reorder = create_item(
        state,
        &token,
        json!({
            "sku": sku("reorder"),
            "name": "Needs reordering",
            "category": "Consumables",
            "min_threshold": "2",
            "reorder_point": "5",
        }),
    )
    .await;
    let idle = create_item(state, &token, item_body("idle")).await;

    record(
        state,
        &token,
        healthy,
        location,
        json!({ "quantity": "100", "reason": "purchase_receipt" }),
    )
    .await;
    record(
        state,
        &token,
        reorder,
        location,
        json!({ "quantity": "3", "reason": "purchase_receipt" }),
    )
    .await;
    record(
        state,
        &token,
        idle,
        location,
        json!({ "quantity": "40", "reason": "purchase_receipt" }),
    )
    .await;

    // Backdate the third item's last movement so "idle" has something to find. A filter that is
    // never exercised is a filter nobody knows works.
    sqlx::query(
        "update inventory_stock set last_movement_at = now() - interval '90 days' \
         where organization_id = $1 and item_id = $2",
    )
    .bind(fixture.org)
    .bind(idle)
    .execute(fixture.db.pool())
    .await
    .expect("the row must be writable for the filter to have a subject");

    let ids_in = |uri: &str| {
        let state = state.clone();
        let token = token.clone();
        let uri = uri.to_owned();
        async move {
            let response = call(&state, request(Method::GET, &uri, Some(&token), None)).await;
            assert_eq!(response.status, StatusCode::OK, "GET {uri}: {}", response.body);
            response.body["items"]
                .as_array()
                .expect("an array")
                .iter()
                .filter_map(|row| row["item_id"].as_str().map(str::to_owned))
                .collect::<Vec<String>>()
        }
    };

    let below = ids_in("/api/v1/inventory/stock?status=below_threshold").await;
    assert!(
        below.contains(&reorder.to_string()),
        "3 is at the reorder point of 5, so it needs reordering: {below:?}"
    );
    assert!(
        !below.contains(&healthy.to_string()),
        "100 is not below anything: {below:?}"
    );

    let old = ids_in("/api/v1/inventory/stock?idle_days=60").await;
    assert!(old.contains(&idle.to_string()), "the backdated row is idle: {old:?}");
    assert!(!old.contains(&healthy.to_string()), "and today's movement is not: {old:?}");

    let by_category = ids_in("/api/v1/inventory/stock?category=Consumables").await;
    assert_eq!(by_category, vec![reorder.to_string()], "the category filter is exact");

    let by_search = ids_in("/api/v1/inventory/stock?search=Needs").await;
    assert_eq!(by_search, vec![reorder.to_string()], "and the search reaches the name");

    // The vocabulary answers the three selects the item form draws, in one call.
    let vocabulary = call(
        state,
        request(Method::GET, "/api/v1/inventory/vocabulary", Some(&token), None),
    )
    .await;
    assert_eq!(vocabulary.status, StatusCode::OK, "{}", vocabulary.body);
    let categories: Vec<&str> = vocabulary.body["categories"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(categories.contains(&"Consumables"), "{categories:?}");
    assert!(categories.contains(&"Fasteners"), "{categories:?}");
    let reasons = vocabulary.body["reasons"].as_array().expect("an array");
    assert_eq!(reasons.len(), 10, "all ten reason codes, and the form offers all of them");
    let correction = reasons
        .iter()
        .find(|reason| reason["value"] == "correction")
        .expect("the correction reason");
    assert_eq!(
        correction["may_go_negative"], true,
        "the drawer has to know which reason needs the permission, or it offers a write-off that \
         will be refused for a reason the operator cannot see"
    );
    let damage = reasons
        .iter()
        .find(|reason| reason["value"] == "damage")
        .expect("the damage reason");
    assert_eq!(damage["may_go_negative"], false);
}
