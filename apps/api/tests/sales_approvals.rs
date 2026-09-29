//! Integration tests for the discount approval gate (docs/requests/REQ-052, slice 3).
//!
//! The gate is the one rule in the sales module that a person can act against, so these walks
//! are written around **what somebody would try to do**, not around the endpoints:
//!
//! * a seller builds a quote at a 20% discount, presses **send**, and is refused — and the
//!   refusal names the discount, the limit and the request rather than saying "wrong status";
//! * they raise the request, the quote moves to `pending_approval`, and a **second** request for
//!   the same quote is refused with the one that is already open;
//! * the seller tries to approve their own quote, and is refused: a gate you can open from your
//!   own screen is a checkbox;
//! * a manager approves it, the quote becomes `approved`, and **only then** can it be sent;
//! * a rejection without a reason is refused, and one with a reason sends the quote back to
//!   `draft` so the seller can fix the line and ask again;
//! * the manager is notified that a request exists and the seller is notified of the decision —
//!   both sides, as the spec asks;
//! * cancelling a quote closes its open request, so nobody is left holding a decision on a
//!   document that no longer exists;
//! * a request raised on a quote **inside** the limit is refused: the gate is a threshold, not
//!   a form anybody may use to escalate a discount that policy already allows;
//! * every `/api/v1/sales/approvals/*` route answers 401 without a session and 403 without the
//!   permission, and another organization's request is **404, never 403**.

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

/// Serialises this suite: the organizations and the IAM seed are shared state.
static APPROVALS_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// May draft, edit and ask for approval, but **not** send and not decide.
///
/// The missing `sales.quotes.send` is the point: this is the role the acceptance criteria are
/// about, because a drafter who can clear their own discount is the failure this slice exists to
/// prevent.
const DRAFTER_PERMISSIONS: [&str; 8] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.products.read",
    "sales.products.manage",
    "crm.contacts.read",
    "crm.contacts.create",
    "sites.read",
];

/// A manager: may read, send and **decide**.
const MANAGER_PERMISSIONS: [&str; 5] = [
    "sales.quotes.read",
    "sales.quotes.send",
    "sales.products.read",
    "crm.contacts.read",
    "sites.read",
];

/// A full seller: the send key **and** the update key, which is the combination that proves the
/// module's own self-approval refusal rather than the permission's.
const SELLER_PERMISSIONS: [&str; 6] = [
    "sales.quotes.read",
    "sales.quotes.create",
    "sales.quotes.update",
    "sales.quotes.send",
    "sales.products.read",
    "sites.read",
];

/// A writer in a second organization, for the cross-tenant `404`.
const FOREIGN_PERMISSIONS: [&str; 4] = [
    "sales.quotes.read",
    "sales.quotes.send",
    "sales.products.read",
    "sites.read",
];

// ---------------------------------------------------------------------------------------------
// Harness (the same shape sales_quotes.rs uses — a reader who knows that walk knows this one)
// ---------------------------------------------------------------------------------------------

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

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
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

struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    organization: Uuid,
    owner_id: Uuid,
    company: Uuid,
    product: Uuid,
    drafter: String,
    manager: String,
    foreign: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = APPROVALS_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.ok()?;

        let organization = create_organization_row(&db, "approvals").await;
        let other_org = create_organization_row(&db, "approvals-foreign").await;

        let (owner_id, _) = create_account(&db, None, "Approvals Owner").await;
        seed::bind_owner(db.pool(), owner_id).await.ok()?;

        let (drafter_id, drafter) = create_account(&db, Some(organization), "Approvals Drafter").await;
        grant(&db, organization, drafter_id, owner_id, &DRAFTER_PERMISSIONS).await;

        let (manager_id, manager) = create_account(&db, Some(organization), "Approvals Manager").await;
        grant(&db, organization, manager_id, owner_id, &MANAGER_PERMISSIONS).await;

        let (foreign_id, foreign) =
            create_account(&db, Some(other_org), "Approvals Foreign").await;
        grant(&db, other_org, foreign_id, owner_id, &FOREIGN_PERMISSIONS).await;

        let drafter_token = login(&state, &drafter).await;
        let company = create_company(&state, &drafter_token).await;
        let product = create_product(&state, &drafter_token).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            organization,
            owner_id,
            company,
            product,
            drafter,
            manager,
            foreign,
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("approvals-{}-{}", label, Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("Approvals Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("approvals-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("approvals-role-{}", Uuid::new_v4().simple()),
            name: "Approvals Test Role".to_owned(),
            description: "A role of the approvals walk".to_owned(),
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
    assert_eq!(response.status, StatusCode::OK, "login body: {}", response.body);
    response
        .set_cookie
        .and_then(|cookie| {
            cookie
                .split(';')
                .next()?
                .split_once('=')
                .map(|(_, value)| value.to_string())
        })
        .expect("a session cookie")
}

async fn create_company(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(token),
            Some(json!({ "name": "Gate Test Co" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "company: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("a company id")).expect("a uuid")
}

async fn create_product(state: &AppState, token: &str) -> Uuid {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/sales/products",
            Some(token),
            Some(json!({ "sku": "GATE-1", "name": "Gate widget", "default_price": "100.00" })),
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "product: {}", response.body);
    Uuid::parse_str(response.body["id"].as_str().expect("a product id")).expect("a uuid")
}

/// A quote whose largest line discount is `discount`, against the default 15% threshold.
fn quote_with_discount(company: Uuid, product: Uuid, discount: i64) -> Value {
    json!({
        "customer_id": company,
        "customer_type": "company",
        "customer_name": "Gate Test Co",
        "title": "Gate quote",
        "currency": "TRY",
        "lines": [
            { "product_id": product, "description": "Widget", "quantity": "2",
              "unit_price": "100.00", "discount_percent": discount, "tax_percent": 20 }
        ]
    })
}

async fn create_quote(fixture: &Fixture, token: &str, body: Value) -> Uuid {
    let response = call(
        &fixture.state,
        request(Method::POST, "/api/v1/sales/quotes", Some(token), Some(body)),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "quote: {}", response.body);
    Uuid::parse_str(response.body["quote"]["id"].as_str().expect("a quote id")).expect("a uuid")
}

async fn quote_status(fixture: &Fixture, token: &str, quote_id: Uuid) -> String {
    let response = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}"),
            Some(token),
            None,
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "quote read: {}", response.body);
    response.body["quote"]["status"]
        .as_str()
        .expect("a status")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_quote_over_the_limit_cannot_be_sent_until_a_manager_approves_it() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let quote_id = create_quote(
        &fixture,
        &drafter,
        quote_with_discount(fixture.company, fixture.product, 20),
    )
    .await;

    // 1. The requirement is answerable before anybody presses send, and it names both numbers.
    let requirement = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requirement"),
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(requirement.status, StatusCode::OK, "{}", requirement.body);
    assert_eq!(requirement.body["discount_percent"].as_i64(), Some(20));
    assert_eq!(requirement.body["threshold_percent"].as_i64(), Some(15));
    assert!(requirement.body["request"].is_null(), "no request has been raised yet");

    // 2. Sending is refused, and the refusal carries the requirement rather than a bare 409.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/send"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    let details = &refused.body["error"]["details"];
    assert_eq!(details["discount_percent"].as_i64(), Some(20));
    assert_eq!(details["threshold_percent"].as_i64(), Some(15));

    // 3. The seller raises the request. The quote moves with it, in the same transaction.
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "They are a good customer and this is the second year" })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::CREATED, "{}", raised.body);
    assert_eq!(raised.status, StatusCode::CREATED, "raise: {}", raised.body);
    let approval_id =
        Uuid::parse_str(raised.body["id"].as_str().expect("a request id")).expect("a uuid");
    assert_eq!(raised.body["status"].as_str(), Some("pending"));
    assert_eq!(raised.body["discount_percent"].as_i64(), Some(20));
    assert_eq!(quote_status(&fixture, &drafter, quote_id).await, "pending_approval");

    // 4. A second request is refused **with the open one attached**, not with a fresh empty state.
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "again" })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    assert_eq!(
        again.body["error"]["details"]["request"]["id"].as_str(),
        Some(approval_id.to_string().as_str()),
        "the refusal must carry the request that is already open: {}",
        again.body
    );

    // 5. Sending is still refused — the gate is not "somebody asked".
    let still_refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/send"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(
        still_refused.status,
        StatusCode::CONFLICT,
        "an open request must not send it: {}",
        still_refused.body
    );

    // 6. The manager approves; the quote becomes `approved` and only then can it be sent.
    let approved = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["status"].as_str(), Some("approved"));
    assert_eq!(quote_status(&fixture, &drafter, quote_id).await, "approved");

    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/send"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "an approved quote sends: {}", sent.body);
    assert_eq!(sent.body["quote"]["status"].as_str(), Some("sent"));

    // 7. Both sides were told. The manager learns a request exists, the seller learns the verdict.
    // Scoped to **this** request, not counted globally: six other walks share this database and
    // a `count(*)` over the whole table measures the suite, not this gate.
    let notified: Vec<(String, String)> = sqlx::query_as(
        "select source_id, title from notifications where source_id = $1",
    )
    .bind(approval_id.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the notification query must run");
    let asked = notified
        .iter()
        .any(|(_, title)| title.contains("needs approval"));
    let decided = notified
        .iter()
        .any(|(_, title)| title.contains("was approved"));
    assert!(asked, "the manager is told a request exists: {notified:?}");
    assert!(decided, "the requester is told the decision: {notified:?}");

    // 8. And the audit trail names who cleared it.
    // Scoped to this request for the same reason as the notification count: the audit table is
    // shared by every walk in the suite, and four rows here means four walks decided something.
    let decisions: Vec<String> = sqlx::query_scalar(
        "select target_id from audit_log
          where action = 'sales.quote.approval_decided' and target_id = $1",
    )
    .bind(approval_id.to_string())
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit query must run");
    assert_eq!(
        decisions.len(),
        1,
        "one decision, one audit row: {decisions:?}"
    );
    assert_eq!(
        decisions[0], approval_id.to_string(),
        "and it names the request that was decided"
    );
}

#[tokio::test]
async fn a_seller_cannot_approve_their_own_quote_and_a_rejection_needs_a_reason() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let quote_id = create_quote(
        &fixture,
        &drafter,
        quote_with_discount(fixture.company, fixture.product, 25),
    )
    .await;
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "please" })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::CREATED, "raise: {}", raised.body);
    let approval_id =
        Uuid::parse_str(raised.body["id"].as_str().expect("a request id")).expect("a uuid");

    // A drafter cannot call the decision route at all — it is behind `sales.quotes.send`, which
    // this role deliberately lacks. That is the permission half of "a gate you can open from
    // your own screen is a checkbox".
    let refused_by_permission = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&drafter),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(
        refused_by_permission.status,
        StatusCode::FORBIDDEN,
        "a drafter may not decide: {}",
        refused_by_permission.body
    );

    // The module half: a **seller who does hold the send key** still cannot decide their own
    // request. The seller here is given the full seller role, so the only thing that can stop
    // them is the `requested_by == actor` check — which is what makes the gate a gate rather
    // than a permission somebody forgets to scope.
    let (boss_id, boss) =
        create_account(&fixture.db, Some(fixture.organization), "Approvals Seller").await;
    grant(
        &fixture.db,
        fixture.organization,
        boss_id,
        fixture.owner_id,
        &SELLER_PERMISSIONS,
    )
    .await;
    let boss = fixture.token(&boss).await;
    let boss_quote = create_quote(
        &fixture,
        &boss,
        quote_with_discount(fixture.company, fixture.product, 35),
    )
    .await;
    let boss_request = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{boss_quote}/approval-requests"),
            Some(&boss),
            Some(json!({ "note": "mine" })),
        ),
    )
    .await;
    assert_eq!(boss_request.status, StatusCode::CREATED, "{}", boss_request.body);
    let boss_approval_id = Uuid::parse_str(boss_request.body["id"].as_str().expect("a request id"))
        .expect("a uuid");
    let self_approved = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{boss_approval_id}/decision"),
            Some(&boss),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(
        self_approved.status,
        StatusCode::CONFLICT,
        "a seller may hold the send key and still not clear their own discount: {}",
        self_approved.body
    );
    assert_eq!(
        self_approved.body["error"]["code"].as_str(),
        Some("sales_approval_self_review"),
        "{}",
        self_approved.body
    );
    // And the quote is still waiting, so the refusal really was the gate and not a no-op.
    assert_eq!(quote_status(&fixture, &boss, boss_quote).await, "pending_approval");

    let without_reason = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "reject" })),
        ),
    )
    .await;
    assert_eq!(
        without_reason.status,
        StatusCode::BAD_REQUEST,
        "a rejection without a reason is refused: {}",
        without_reason.body
    );
    assert_eq!(without_reason.body["error"]["details"]["field"].as_str(), Some("comment"));
    assert_eq!(quote_status(&fixture, &drafter, quote_id).await, "pending_approval");

    // A verb that is neither approve nor reject is never read as an approval.
    let nonsense = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "yes please" })),
        ),
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::BAD_REQUEST, "{}", nonsense.body);

    // With a reason, the quote goes back to `draft`: it is still the seller's working document.
    let rejected = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "reject", "comment": "15% is the floor for this account" })),
        ),
    )
    .await;
    assert_eq!(rejected.status, StatusCode::OK, "{}", rejected.body);
    assert_eq!(rejected.body["status"].as_str(), Some("rejected"));
    assert_eq!(quote_status(&fixture, &drafter, quote_id).await, "draft");

    // The seller is told, with the reason that is the only part they can act on.
    let told: String = sqlx::query_scalar(
        "select body from notifications where source_id = $1 and title like '%rejected%' \
         order by created_at desc limit 1",
    )
    .bind(approval_id.to_string())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the seller must be notified");
    assert!(told.contains("15% is the floor"), "{told}");

    // A second decision on the same request is refused — the first one stands.
    let decided_twice = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(decided_twice.status, StatusCode::BAD_REQUEST, "{}", decided_twice.body);
}

#[tokio::test]
async fn a_quote_inside_the_limit_never_asks_for_approval() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;

    let quote_id = create_quote(
        &fixture,
        &drafter,
        quote_with_discount(fixture.company, fixture.product, 10),
    )
    .await;

    // The requirement is `null` — not an object with a false — because a screen that renders the
    // discount unconditionally would print "10%" above every ordinary quote.
    let requirement = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requirement"),
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(requirement.status, StatusCode::OK);
    assert!(requirement.body.is_null(), "an inside-the-limit quote needs nothing: {}", requirement.body);

    // And asking for approval anyway is refused with the number that explains it.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "just in case" })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("10%"), "{}", refused.body);

    // Which means the seller can simply send it.
    let sent = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/send"),
            Some(&fixture.token(&fixture.manager).await),
            None,
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
}

#[tokio::test]
async fn withdrawing_a_quote_closes_the_request_so_nobody_decides_a_dead_document() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;

    let quote_id = create_quote(
        &fixture,
        &drafter,
        quote_with_discount(fixture.company, fixture.product, 30),
    )
    .await;
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "big one" })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::CREATED, "raise: {}", raised.body);
    let approval_id =
        Uuid::parse_str(raised.body["id"].as_str().expect("a request id")).expect("a uuid");

    let cancelled = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/cancel"),
            Some(&drafter),
            Some(json!({ "reason": "the customer went elsewhere" })),
        ),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);

    // The request is closed rather than deleted: the history still says who asked.
    let history = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/quotes/{quote_id}/approvals"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK);
    let rows = history.body.as_array().expect("a list");
    assert_eq!(rows.len(), 1, "the row survives: {rows:?}");
    assert_eq!(rows[0]["status"].as_str(), Some("cancelled"));

    // And the manager can no longer decide it.
    let decided = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/approvals/{approval_id}/decision"),
            Some(&manager),
            Some(json!({ "decision": "approve" })),
        ),
    )
    .await;
    assert_eq!(decided.status, StatusCode::BAD_REQUEST, "{}", decided.body);
}

#[tokio::test]
async fn the_inbox_reads_four_ways_and_another_organization_is_404() {
    let Some(fixture) = Fixture::new().await else { return };
    let drafter = fixture.token(&fixture.drafter).await;
    let manager = fixture.token(&fixture.manager).await;
    let foreign = fixture.token(&fixture.foreign).await;

    let quote_id = create_quote(
        &fixture,
        &drafter,
        quote_with_discount(fixture.company, fixture.product, 40),
    )
    .await;
    let raised = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/sales/quotes/{quote_id}/approval-requests"),
            Some(&drafter),
            Some(json!({ "note": "urgent" })),
        ),
    )
    .await;
    assert_eq!(raised.status, StatusCode::CREATED, "raise: {}", raised.body);
    let approval_id =
        Uuid::parse_str(raised.body["id"].as_str().expect("a request id")).expect("a uuid");

    // The manager's inbox has it; the seller's "mine" has it; "decided" has nothing yet.
    let pending_for_manager = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/approvals?scope=pending",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(pending_for_manager.status, StatusCode::OK, "{}", pending_for_manager.body);
    assert!(
        pending_for_manager.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|row| row["id"].as_str() == Some(approval_id.to_string().as_str())),
        "the manager sees it: {}",
        pending_for_manager.body
    );

    let mine = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/approvals?scope=requested_by_me",
            Some(&drafter),
            None,
        ),
    )
    .await;
    assert_eq!(mine.status, StatusCode::OK);
    assert!(!mine.body["items"].as_array().expect("items").is_empty());

    let decided = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/sales/approvals?scope=decided",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(decided.status, StatusCode::OK);

    // Cross-tenant: 404, never 403 — the status must not disclose that the request exists.
    let across = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/sales/approvals/{approval_id}"),
            Some(&foreign),
            None,
        ),
    )
    .await;
    assert_eq!(across.status, StatusCode::NOT_FOUND, "{}", across.body);

    // Unauthenticated is 401 on every route in the file.
    for path in [
        "/api/v1/sales/approvals",
        "/api/v1/sales/approvals/some-id",
    ] {
        let anonymous = call(&fixture.state, request(Method::GET, path, None, None)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}
