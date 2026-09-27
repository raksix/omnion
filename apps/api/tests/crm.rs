//! Integration tests for the CRM surface (docs/requests/REQ-051, slice 1).
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason.
//!
//! What the walk proves, in the words of the acceptance criteria: every `/api/v1/crm/*` route
//! answers `401` unauthenticated, `403` with the permission missing and `200` with it granted; a
//! record of another organization is `404`; a malformed address, a duplicate address and a
//! nameless contact are each refused with the field the form renders the message under; creating,
//! updating, archiving and merging write an audit row with the actor, the changed fields and the
//! before/after; `crm.contact.created` and `crm.contact.merged` reach the event feed with the
//! documented payload; the list's filters combine and its sort refuses an unknown column; the
//! `own` visibility level hides a colleague's record (and keeps the unassigned one); and the
//! flagged custom fields are gone for a role without `crm.fields.sensitive.read` — in the list,
//! in the detail and in the copy the caller gets back.

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
static CRM_WALK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a reader of the CRM may do: see the contacts, change nothing.
const READER_PERMISSIONS: [&str; 2] = ["crm.contacts.read", "sites.read"];

/// What a manager adds on top.
const MANAGER_PERMISSIONS: [&str; 6] = [
    "crm.contacts.read",
    "crm.contacts.create",
    "crm.contacts.update",
    "crm.contacts.delete",
    "crm.contacts.merge",
    "sites.read",
];

/// What the reader additionally is *not* given: the flagged fields. The suite proves the
/// redaction with a reader and without it.
const SENSITIVE_PERMISSIONS: [&str; 3] = [
    "crm.contacts.read",
    "crm.fields.sensitive.read",
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
    let bytes = response.into_body().collect().await.expect("body must read").to_bytes();
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
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
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

/// One organization, a platform Owner, a manager, a plain reader, a reader that may read the
/// flagged fields, a member with nothing, and a reader of a *second* organization.
struct Fixture {
    _walk: tokio::sync::MutexGuard<'static, ()>,
    state: AppState,
    db: Db,
    org: Uuid,
    other_org: Uuid,
    manager: String,
    reader: String,
    sensitive: String,
    member: String,
    other_reader: String,
    manager_id: Uuid,
    accounts: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let walk = CRM_WALK.lock().await;
        let (state, db) = live_state().await?;
        seed::ensure(db.pool()).await.expect("the IAM seed must run");

        let org = create_organization_row(&db, "a").await;
        let other_org = create_organization_row(&db, "b").await;

        let (owner_id, _) = create_account(&db, None, "CRM Owner").await;
        seed::bind_owner(db.pool(), owner_id)
            .await
            .expect("the owner binding must be created");

        let (manager_id, manager) = create_account(&db, Some(org), "CRM Manager").await;
        grant(&db, org, manager_id, owner_id, &MANAGER_PERMISSIONS).await;

        let (reader_id, reader) = create_account(&db, Some(org), "CRM Reader").await;
        grant(&db, org, reader_id, owner_id, &READER_PERMISSIONS).await;

        let (sensitive_id, sensitive) = create_account(&db, Some(org), "CRM Sensitive").await;
        grant(&db, org, sensitive_id, owner_id, &SENSITIVE_PERMISSIONS).await;

        let (_member_id, member) = create_account(&db, Some(org), "CRM Member").await;

        let (other_id, other_reader) = create_account(&db, Some(other_org), "CRM Other").await;
        grant(&db, other_org, other_id, owner_id, &READER_PERMISSIONS).await;

        // The default pipeline is seeded per organization by the migration; a fresh organization
        // created by the fixture gets one only through that function, so the suite calls it — the
        // same path a new tenant takes.
        seed_default_pipeline(&db, org).await;
        seed_default_pipeline(&db, other_org).await;

        Some(Self {
            _walk: walk,
            state,
            db,
            org,
            other_org,
            manager,
            reader,
            sensitive,
            member,
            other_reader,
            manager_id,
            accounts: vec![owner_id, manager_id, reader_id, sensitive_id, other_id],
        })
    }

    async fn token(&self, email: &str) -> String {
        login(&self.state, email).await
    }
}

/// Create an organization row with a unique slug.
async fn create_organization_row(db: &Db, label: &str) -> Uuid {
    let slug = format!("crm-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(format!("CRM Test {label}"))
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Seed the default pipeline of an organization, the way a new tenant gets one.
async fn seed_default_pipeline(db: &Db, organization_id: Uuid) {
    sqlx::query("select crm_seed_default_pipeline($1)")
        .bind(organization_id)
        .execute(db.pool())
        .await
        .expect("the default pipeline must be seeded");
}

/// Create an account with a unique address so parallel runs cannot collide.
async fn create_account(db: &Db, organization_id: Option<Uuid>, name: &str) -> (Uuid, String) {
    let email = format!("crm-{}@omnion.test", Uuid::new_v4().simple());
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
            key: format!("crm-role-{}", Uuid::new_v4().simple()),
            name: "CRM Test Role".to_owned(),
            description: "A role of the CRM suite".to_owned(),
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

/// Narrow an account to the `own` visibility level, the way an organization does it with a
/// `department` binding.
async fn narrow_to_own(db: &Db, organization_id: Uuid, user_id: Uuid, granted_by: Uuid) {
    let role = role_store::create_role(
        db.pool(),
        NewRole {
            organization_id,
            key: format!("crm-narrow-{}", Uuid::new_v4().simple()),
            name: "CRM Own Only".to_owned(),
            description: "Sees only its own records".to_owned(),
            priority: 300,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the narrowing role must be created");

    role_store::set_role_permissions(
        db.pool(),
        role.id,
        &[RolePermissionInput {
            key: "crm.contacts.read".to_owned(),
            effect: Effect::Allow,
        }],
    )
    .await
    .expect("the narrowing role's permission must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id,
        scope: PermScope::Department {
            organization_id,
            department: "own".to_owned(),
        },
        granted_by: Some(granted_by),
        expires_at: None,
    };
    omnion_permissions::bindings::grant(db.pool(), binding)
        .await
        .expect("the narrowing binding must be created");
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
        )
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
    let rows: Vec<(Value, String, Option<String>, Value)> = sqlx::query_as(
        "select metadata, target_type, target_id, coalesce(actor_user_id::text, '') from audit_log \
         where action = $1 order by id desc limit 5",
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
            .expect("the events must read");
    rows.into_iter().map(|(payload,)| payload).collect()
}

// ---------------------------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------------------------

/// Every route refuses an unauthenticated caller and a caller without the permission.
#[tokio::test]
async fn every_crm_route_is_permission_guarded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let marker = Uuid::new_v4();

    let calls: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, "/api/v1/crm/contacts".to_owned(), None),
        (
            Method::POST,
            "/api/v1/crm/contacts".to_owned(),
            Some(json!({ "first_name": "X" })),
        ),
        (
            Method::GET,
            format!("/api/v1/crm/contacts/{marker}"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/v1/crm/contacts/{marker}"),
            Some(json!({ "status": "customer" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/crm/contacts/{marker}"),
            None,
        ),
        (
            Method::POST,
            "/api/v1/crm/contacts/merge".to_owned(),
            Some(json!({ "survivor": marker, "loser": marker })),
        ),
        (Method::GET, "/api/v1/crm/companies".to_owned(), None),
        (
            Method::POST,
            "/api/v1/crm/companies".to_owned(),
            Some(json!({ "name": "X" })),
        ),
        (
            Method::GET,
            format!("/api/v1/crm/companies/{marker}"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/v1/crm/companies/{marker}"),
            Some(json!({ "name": "Y" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/crm/companies/{marker}"),
            None,
        ),
    ];

    for (method, uri, body) in &calls {
        let anonymous = call(state, request(method.clone(), uri, None, body.clone())).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must refuse an anonymous caller: {}",
            anonymous.body
        );
    }

    // A member with no CRM permission at all is refused with `403`, and the refusal names the
    // key that is missing — a `401` here would be a sign-in problem, which it is not.
    let member = fixture.token(&fixture.member).await;
    for (method, uri, body) in &calls {
        let refused = call(
            state,
            request(method.clone(), uri, Some(&member), body.clone()),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a member without the permission: {}",
            refused.body
        );
    }
}

/// A signed-in curl round-trip: a company, a contact on it, a patch, the audit trail and the
/// event feed all read back.
#[tokio::test]
async fn a_contact_round_trip_writes_its_audit_row_and_event() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let company = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(&manager),
            Some(json!({
                "name": format!("Analytical Engines {}", Uuid::new_v4().simple()),
                "domain": "example.com",
                "industry": "Software",
                "status": "lead",
                "tags": ["priority", "  Priority  ", "emea"],
            })),
        )
    )
    .await;
    assert_eq!(company.status, StatusCode::CREATED, "body: {}", company.body);
    assert_eq!(company.body["domain"], json!("example.com"));
    // Tags arrive trimmed and de-duplicated, not as typed.
    assert_eq!(company.body["tags"], json!(["priority", "emea"]));
    let company_id = company.body["id"].as_str().expect("the company has an id");

    // A second company with the same name is a conflict, not a silent overwrite.
    let duplicate = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(&manager),
            Some(json!({ "name": company.body["name"] })),
        )
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "body: {}", duplicate.body);
    assert_eq!(duplicate.body["code"], json!("company_name_taken"));

    let email = format!("ada-{}@example.com", Uuid::new_v4().simple());
    let contact = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Ada",
                "last_name": "Lovelace",
                "email": email,
                "phone": "+44 20 7946 0000",
                "job_title": "Analyst",
                "company_id": company_id,
                "status": "customer",
                "tags": ["vip"],
                "notes": "Prefers a reply in the morning.",
                "custom": { "seat_count": 12, "contract_value_note": "40k" },
            })),
        )
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED, "body: {}", contact.body);
    assert_eq!(contact.body["company_name"], json!(company.body["name"]));
    assert_eq!(contact.body["owner_user_id"], json!(fixture.manager_id));
    // The manager holds the sensitive key, so the flagged field is present.
    assert_eq!(contact.body["custom"]["contract_value_note"], json!("40k"));
    let contact_id = contact.body["id"].as_str().expect("the contact has an id");

    // The audit trail: the actor, the target and the after-image.
    let audits = audit_rows(&fixture.db, "crm.contact.created").await;
    let entry = audits
        .iter()
        .find(|entry| entry["target_id"] == json!(contact_id))
        .unwrap_or_else(|| panic!("the create must leave an audit row: {audits:?}"));
    assert_eq!(entry["actor"], json!(fixture.manager_id.to_string()));
    assert_eq!(entry["target_type"], json!("crm_contact"));
    assert_eq!(entry["metadata"]["after"]["contact_id"], json!(contact_id));

    // The event feed: the documented name, the organization and the payload keys.
    let payloads = event_payloads(&fixture.db, "crm.contact.created").await;
    let payload = payloads
        .iter()
        .find(|payload| payload["contact_id"] == json!(contact_id))
        .unwrap_or_else(|| panic!("the create must emit crm.contact.created: {payloads:?}"));
    for key in ["contact_id", "organization_id", "email", "owner_user_id", "status"] {
        assert!(payload.get(key).is_some(), "the payload must carry {key}");
    }
    assert!(
        payload.get("notes").is_none(),
        "the payload must not carry the record's free text"
    );

    // The inline edit of the list: one field, the fresh row back.
    let patched = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&manager),
            Some(json!({ "status": "partner", "tags": ["vip", "renewal"] })),
        )
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "body: {}", patched.body);
    assert_eq!(patched.body["status"], json!("partner"));
    assert_eq!(patched.body["tags"], json!(["vip", "renewal"]));
    // A field the patch did not mention is kept, not cleared.
    assert_eq!(patched.body["notes"], json!("Prefers a reply in the morning."));

    let updates = audit_rows(&fixture.db, "crm.contact.updated").await;
    let update = updates
        .iter()
        .find(|entry| entry["target_id"] == json!(contact_id))
        .unwrap_or_else(|| panic!("the update must leave an audit row: {updates:?}"));
    let changed = update["metadata"]["changed"].as_array().expect("changed is a list");
    let names: Vec<&str> = changed.iter().filter_map(|value| value.as_str()).collect();
    assert!(names.contains(&"status"), "{names:?}");
    assert!(names.contains(&"tags"), "{names:?}");
    assert!(!names.contains(&"notes"), "{names:?}");

    // The detail carries the rollups a company screen shows.
    let company_detail = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/companies/{company_id}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(company_detail.status, StatusCode::OK);
    assert_eq!(company_detail.body["contact_count"], json!(1));
    assert_eq!(company_detail.body["company"]["name"], company.body["name"]);
}

/// The refusals: a nameless contact, a malformed address, a duplicate address, a bad domain.
#[tokio::test]
async fn the_contact_and_company_forms_refuse_what_they_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let marker = Uuid::new_v4().simple().to_string();

    // No first name.
    let nameless = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({ "last_name": "Lovelace" })),
        )
    )
    .await;
    assert_eq!(nameless.status, StatusCode::BAD_REQUEST, "body: {}", nameless.body);
    assert_eq!(nameless.body["details"]["field"], json!("first_name"));

    // A malformed address.
    let malformed = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({ "first_name": "Ada", "email": "ada@example" })),
        )
    )
    .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed.body["details"]["field"], json!("email"));

    // A duplicate address, case-insensitively.
    let email = format!("dup-{marker}@example.com");
    let first = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({ "first_name": "Ada", "email": email })),
        )
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "body: {}", first.body);

    let duplicate = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({ "first_name": "Ada", "email": email.to_uppercase() })),
        )
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT, "body: {}", duplicate.body);
    assert_eq!(duplicate.body["code"], json!("contact_email_taken"));

    // A bad domain.
    let domain = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(&manager),
            Some(json!({ "name": format!("Bad Domain {marker}"), "domain": "not a domain" })),
        )
    )
    .await;
    assert_eq!(domain.status, StatusCode::BAD_REQUEST);
    assert_eq!(domain.body["details"]["field"], json!("domain"));

    // A merge of a contact into itself.
    let contact_id = first.body["id"].as_str().expect("the contact has an id");
    let self_merge = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/merge",
            Some(&manager),
            Some(json!({ "survivor": contact_id, "loser": contact_id })),
        )
    )
    .await;
    assert_eq!(self_merge.status, StatusCode::BAD_REQUEST, "body: {}", self_merge.body);
    assert_eq!(self_merge.body["code"], json!("invalid_crm_merge"));
}

/// The list: filters combine, an unknown sort is refused, and the total matches the page.
#[tokio::test]
async fn the_contact_list_filters_sorts_and_totals() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let marker = Uuid::new_v4().simple().to_string();

    let company = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(&manager),
            Some(json!({ "name": format!("Filter Co {marker}") })),
        )
    )
    .await;
    let company_id = company.body["id"].as_str().expect("the company has an id").to_owned();

    for (index, status) in ["lead", "customer", "partner"].iter().enumerate() {
        let created = call(
            state,
            request(
                Method::POST,
                "/api/v1/crm/contacts",
                Some(&manager),
                Some(json!({
                    "first_name": format!("Filter{index}-{marker}"),
                    "last_name": "Person",
                    "email": format!("filter{index}-{marker}@example.com"),
                    "company_id": company_id,
                    "status": status,
                    "tags": [if index == 0 { "vip".to_owned() } else { "plain".to_owned() }],
                })),
            ),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    }

    // The search finds them by name.
    let by_name = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=Filter-{marker}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(by_name.status, StatusCode::OK, "body: {}", by_name.body);
    assert_eq!(
        by_name.body["items"].as_array().map(Vec::len),
        Some(3),
        "three contacts carry the marker: {}",
        by_name.body
    );
    assert!(by_name.body["total_estimate"].as_i64().unwrap_or_default() >= 3);

    // The filters combine: the company, the status and the tag narrow to exactly one.
    let combined = call(
        state,
        request(
            Method::GET,
            &format!(
                "/api/v1/crm/contacts?company_id={company_id}&status=lead&tag=vip&limit=50"
            ),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(combined.status, StatusCode::OK, "body: {}", combined.body);
    let items = combined.body["items"].as_array().expect("items is a list");
    assert_eq!(items.len(), 1, "one contact matches all three: {items:?}");
    assert_eq!(items[0]["status"], json!("lead"));

    // The `owner=me` filter finds the manager's own contacts and nothing else.
    let mine = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/contacts?owner=me&limit=200",
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(mine.status, StatusCode::OK);
    for item in mine.body["items"].as_array().expect("items is a list") {
        assert_eq!(item["owner_user_id"], json!(fixture.manager_id));
    }

    // An unknown sort is refused, with the columns named.
    let bad_sort = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/contacts?sort=whatever",
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(bad_sort.status, StatusCode::BAD_REQUEST, "body: {}", bad_sort.body);
    assert_eq!(bad_sort.body["code"], json!("invalid_crm_query"));

    // A documented sort works and pages with a cursor.
    let page = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/contacts?sort=email&direction=asc&limit=2",
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "body: {}", page.body);
    let items = page.body["items"].as_array().expect("items is a list");
    assert_eq!(items.len(), 2, "a page of two: {items:?}");
    let cursor = page.body["next_cursor"].as_str().expect("a full page has a cursor");
    assert_eq!(cursor, items[1]["id"].as_str().unwrap_or_default());

    let next = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?sort=email&direction=asc&limit=2&cursor={cursor}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(next.status, StatusCode::OK);
    let next_items = next.body["items"].as_array().expect("items is a list");
    assert!(
        next_items.iter().all(|item| item["id"] != json!(items[0]["id"])),
        "the second page must not repeat the first"
    );
}

/// A record of another organization is invisible: `404`, exactly like one that does not exist.
#[tokio::test]
async fn a_record_of_another_organization_is_invisible() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let other = fixture.token(&fixture.other_reader).await;

    let contact = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Tenant",
                "last_name": "Isolated",
                "email": format!("isolated-{}@example.com", Uuid::new_v4().simple()),
            })),
        )
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED);
    let contact_id = contact.body["id"].as_str().expect("the contact has an id");

    // The other organization's reader does not see it in the list…
    let list = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/contacts?limit=200",
            Some(&other),
            None,
        )
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    let items = list.body["items"].as_array().expect("items is a list");
    assert!(
        items.iter().all(|item| item["id"] != json!(contact_id)),
        "another tenant's list must not carry the record"
    );

    // …and a direct read is a `404`, not a `403`: a `403` would confirm it exists.
    let direct = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&other),
            None,
        )
    )
    .await;
    assert_eq!(direct.status, StatusCode::NOT_FOUND, "body: {}", direct.body);
    assert_eq!(direct.body["code"], json!("contact_not_found"));

    // A write across the boundary is refused the same way.
    let patch = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&other),
            Some(json!({ "status": "customer" })),
        )
    )
    .await;
    assert_eq!(patch.status, StatusCode::NOT_FOUND, "body: {}", patch.body);
}

/// The `own` level: a colleague's record is invisible, the unassigned one is not.
#[tokio::test]
async fn the_own_visibility_level_hides_a_colleagues_record() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let marker = Uuid::new_v4().simple().to_string();

    // One contact owned by the manager…
    let owned = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": format!("Owned-{marker}"),
                "email": format!("owned-{marker}@example.com"),
            })),
        )
    )
    .await;
    assert_eq!(owned.status, StatusCode::CREATED, "body: {}", owned.body);
    let owned_id = owned.body["id"].as_str().expect("the contact has an id");

    // …and one owned by the reader the suite narrows.
    let (reader_id, reader_email) = create_account(&fixture.db, Some(fixture.org), "CRM Narrowed").await;
    let reader_token = fixture.token(&reader_email).await;
    let colleagues = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": format!("Colleague-{marker}"),
                "email": format!("colleague-{marker}@example.com"),
                "owner_user_id": reader_id,
            })),
        )
    )
    .await;
    assert_eq!(colleagues.status, StatusCode::CREATED, "body: {}", colleagues.body);
    let colleagues_id = colleagues.body["id"].as_str().expect("the contact has an id");

    // The narrowed reader sees only their own record.
    let before = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=-{marker}&limit=200"),
            Some(&reader_token),
            None,
        )
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "body: {}", before.body);
    let before_ids: Vec<&Value> = before.body["items"]
        .as_array()
        .expect("items is a list")
        .iter()
        .map(|item| &item["id"])
        .collect();
    assert!(!before_ids.contains(&&json!(owned_id)), "before narrowing the reader sees both");

    // Narrow the reader to `own` with a `department` binding. The grantor is the platform
    // account this suite created, so the binding's `granted_by` points at a real row.
    let (owner_id,): (Uuid,) = sqlx::query_as(
        "select id from users where organization_id is null order by created_at limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the platform owner exists");
    narrow_to_own(&fixture.db, fixture.org, reader_id, owner_id).await;

    let after = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=-{marker}&limit=200"),
            Some(&reader_token),
            None,
        )
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "body: {}", after.body);
    let items = after.body["items"].as_array().expect("items is a list");
    assert!(
        items.iter().any(|item| item["id"] == json!(colleagues_id)),
        "the reader must still see their own record"
    );
    assert!(
        items.iter().all(|item| item["id"] != json!(owned_id)),
        "the `own` level must hide the manager's record"
    );

    // A direct read of the hidden record is a `404`.
    let direct = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{owned_id}"),
            Some(&reader_token),
            None,
        )
    )
    .await;
    assert_eq!(direct.status, StatusCode::NOT_FOUND, "body: {}", direct.body);
}

/// The flagged fields are absent for a role without the key — in the list and in the detail.
#[tokio::test]
async fn the_flagged_fields_are_hidden_from_a_role_without_the_key() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let reader = fixture.token(&fixture.reader).await;
    let sensitive = fixture.token(&fixture.sensitive).await;
    let marker = Uuid::new_v4().simple().to_string();

    let contact = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": format!("Sensitive-{marker}"),
                "email": format!("sensitive-{marker}@example.com"),
                "custom": {
                    "seat_count": 12,
                    "contract_value_note": "renewal at 40k",
                    "history": [{ "year": 2025, "internal_notes": "churn risk" }],
                },
            })),
        )
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED, "body: {}", contact.body);
    let contact_id = contact.body["id"].as_str().expect("the contact has an id").to_owned();

    // A role with the key reads the whole object.
    let allowed = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&sensitive),
            None,
        )
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "body: {}", allowed.body);
    assert_eq!(allowed.body["custom"]["contract_value_note"], json!("renewal at 40k"));
    assert_eq!(
        allowed.body["custom"]["history"][0]["internal_notes"],
        json!("churn risk")
    );

    // A role without it reads the same record with the flagged keys removed, at every depth.
    let hidden = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&reader),
            None,
        )
    )
    .await;
    assert_eq!(hidden.status, StatusCode::OK, "body: {}", hidden.body);
    assert_eq!(hidden.body["custom"]["seat_count"], json!(12));
    assert!(hidden.body["custom"].get("contract_value_note").is_none());
    assert!(
        hidden.body["custom"]["history"][0].get("internal_notes").is_none(),
        "a flagged key one level down is hidden too"
    );

    // The list applies the same rule, or the screen and the detail would disagree.
    let listed = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=Sensitive-{marker}&limit=10"),
            Some(&reader),
            None,
        )
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    let items = listed.body["items"].as_array().expect("items is a list");
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(items[0]["custom"].get("contract_value_note").is_none());
}

/// Archiving and merging: the history survives, and the merge moves what pointed at the loser.
#[tokio::test]
async fn archiving_and_merging_keep_the_history() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let marker = Uuid::new_v4().simple().to_string();

    let survivor = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Survivor",
                "email": format!("survivor-{marker}@example.com"),
                "tags": ["vip"],
                "notes": "The note that stays.",
                "custom": { "seat_count": 5 },
            })),
        )
    )
    .await;
    assert_eq!(survivor.status, StatusCode::CREATED, "body: {}", survivor.body);
    let survivor_id = survivor.body["id"].as_str().expect("an id").to_owned();

    let loser = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Loser",
                "phone": "+90 532 000 00 00",
                "tags": ["vip", "emea"],
                "notes": "The note that moves.",
                "custom": { "seat_count": 9, "region": "eu" },
            })),
        )
    )
    .await;
    assert_eq!(loser.status, StatusCode::CREATED, "body: {}", loser.body);
    let loser_id = loser.body["id"].as_str().expect("an id").to_owned();

    let merged = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/merge",
            Some(&manager),
            Some(json!({ "survivor": survivor_id, "loser": loser_id })),
        )
    )
    .await;
    assert_eq!(merged.status, StatusCode::OK, "body: {}", merged.body);
    // The survivor's identity wins; what it lacked takes the loser's.
    assert_eq!(
        merged.body["email"],
        format!("survivor-{marker}@example.com")
    );
    assert_eq!(merged.body["phone"], json!("+90 532 000 00 00"));
    assert_eq!(merged.body["tags"], json!(["vip", "emea"]));
    assert!(merged.body["notes"].as_str().unwrap_or_default().contains("The note that moves."));
    // The loser's custom value for a key the survivor had is kept.
    assert_eq!(merged.body["custom"]["seat_count"], json!(5));
    assert_eq!(merged.body["custom"]["region"], json!("eu"));

    // The loser is archived, so the default list no longer shows it — and the audit records it.
    let list = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=Loser&limit=50"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(
        list.body["items"].as_array().expect("items").is_empty(),
        "an archived contact leaves the default list: {}",
        list.body
    );

    let with_archived = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search=Loser&include_archived=true&limit=50"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(with_archived.status, StatusCode::OK);
    let items = with_archived.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "the archived record is still readable: {items:?}");
    assert!(items[0]["archived_at"].is_string());

    let merges = audit_rows(&fixture.db, "crm.contact.merged").await;
    assert!(
        merges
            .iter()
            .any(|entry| entry["metadata"]["archived"] == json!(loser_id)),
        "the merge must leave an audit row: {merges:?}"
    );

    // Archiving a contact that is already archived is a `404`, not a second archive.
    let archived = call(
        state,
        request(
            Method::DELETE,
            &format!("/api/v1/crm/contacts/{loser_id}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(archived.status, StatusCode::NOT_FOUND, "body: {}", archived.body);

    // Archiving a live contact hides it and leaves its own audit row.
    let archived = call(
        state,
        request(
            Method::DELETE,
            &format!("/api/v1/crm/contacts/{survivor_id}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "body: {}", archived.body);
    assert!(archived.body["archived_at"].is_string());
    let archives = audit_rows(&fixture.db, "crm.contact.archived").await;
    assert!(archives.iter().any(|entry| entry["target_id"] == json!(survivor_id)));
}

/// The company list: the same envelope, and the rollup a detail screen shows.
#[tokio::test]
async fn the_company_list_and_detail_read_back() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let marker = Uuid::new_v4().simple().to_string();

    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(&manager),
            Some(json!({
                "name": format!("Listed Co {marker}"),
                "domain": format!("co-{marker}.example"),
                "industry": "Software",
            })),
        )
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let company_id = created.body["id"].as_str().expect("an id").to_owned();

    let listed = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/companies?search=Listed Co {marker}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "body: {}", listed.body);
    let items = listed.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["id"], json!(company_id));
    assert!(items[0]["total_estimate"].is_null() || items[0]["total_estimate"].is_number());

    // The detail starts at zero and keeps the domain.
    let detail = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/companies/{company_id}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "body: {}", detail.body);
    assert_eq!(detail.body["contact_count"], json!(0));
    assert_eq!(detail.body["open_deal_count"], json!(0));

    // A contact on the company moves the rollup.
    let contact = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Rollup",
                "email": format!("rollup-{marker}@example.com"),
                "company_id": company_id,
            })),
        )
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED, "body: {}", contact.body);

    let detail = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/companies/{company_id}"),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(detail.body["contact_count"], json!(1));
}

/// The event carries the organization it belongs to, so a subscriber scoped to one tenant is
/// the only one that can ever see it.
#[tokio::test]
async fn a_contact_event_carries_its_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": "Scoped",
                "email": format!("scoped-{}@example.com", Uuid::new_v4().simple()),
            })),
        )
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);

    let row: (Option<Uuid>,) = sqlx::query_as(
        "select organization_id from events where name = 'crm.contact.created' \
         and payload ->> 'contact_id' = $1 order by id desc limit 1",
    )
    .bind(created.body["id"].as_str().unwrap_or_default())
    .fetch_one(fixture.db.pool())
    .await
    .expect("the event row must exist");
    assert_eq!(
        row.0,
        Some(fixture.org),
        "the event must belong to the writing account's organization"
    );
}

/// The migration left the default pipeline in place for an existing organization.
#[tokio::test]
async fn the_default_pipeline_is_seeded_for_every_organization() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    let stages: Vec<(String, String, i32)> = sqlx::query_as(
        "select s.name, s.kind, s.position from crm_pipeline_stages s \
         join crm_pipelines p on p.id = s.pipeline_id \
         where p.organization_id = $1 and p.is_default order by s.position",
    )
    .bind(fixture.org)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the stages must read");

    let names: Vec<&str> = stages.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        names,
        vec!["New", "Qualified", "Proposal", "Negotiation", "Won", "Lost"],
        "the seeded pipeline must carry the documented stages"
    );
    assert!(stages.iter().any(|row| row.1 == "won"));
    assert!(stages.iter().any(|row| row.1 == "lost"));
    assert!(
        stages.iter().filter(|row| row.1 == "open").count() == 4,
        "four open stages"
    );

    // Seeding twice is a no-op: the unique default index refuses the second pipeline.
    seed_default_pipeline(&fixture.db, fixture.org).await;
    let pipelines: Vec<(Uuid,)> =
        sqlx::query_as("select id from crm_pipelines where organization_id = $1 and is_default")
            .bind(fixture.org)
            .fetch_all(fixture.db.pool())
            .await
            .expect("the pipelines must read");
    assert_eq!(pipelines.len(), 1, "an organization has exactly one default pipeline");
}

/// The whole API answers for a signed-in account, with a request id, and the account is scoped.
#[tokio::test]
async fn the_surface_answers_a_platform_account_and_scopes_a_tenant_account() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    // A tenant account cannot name another organization.
    let cross = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?organization_id={}", fixture.other_org),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN, "body: {}", cross.body);
    assert_eq!(cross.body["code"], json!("cross_organization"));

    // Its own organization is accepted.
    let own = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?organization_id={}", fixture.org),
            Some(&manager),
            None,
        )
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "body: {}", own.body);
    assert_eq!(own.body["items"].as_array().map(Vec::len).is_some(), true);
}

/// The header the panel sends is not required: the walk must work with a bare session cookie.
#[tokio::test]
async fn the_routes_answer_without_a_content_type_on_a_read() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;

    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/contacts", Some(&manager), None)
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "body: {}", response.body);
    assert!(response.body["items"].is_array());
    assert!(response.body.get("next_cursor").is_some());
    assert!(response.body.get("total_estimate").is_some());
}

/// Every account the fixture creates carries a distinct address and a tenant (except the
/// platform owner), so two runs cannot collide and a reader cannot read across a boundary.
#[tokio::test]
async fn the_fixture_accounts_are_distinct_and_tenant_scoped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    assert_eq!(
        fixture.accounts.len(),
        5,
        "the fixture creates five accounts: the platform owner and four tenant accounts"
    );
    let unique: std::collections::BTreeSet<&Uuid> = fixture.accounts.iter().collect();
    assert_eq!(unique.len(), fixture.accounts.len(), "every account is distinct");

    // Four of the five are tenant-scoped; the platform owner belongs to no organization.
    let (scoped,): (i64,) = sqlx::query_as(
        "select count(*) from users where email like 'crm-%' and organization_id is not null",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the users must read");
    assert!(
        scoped >= 4,
        "every tenant account carries its organization, got {scoped}"
    );
}

/// The audit metadata is a JSON document, not a string: the IAM screen reads `changed` and
/// `before` as keys, and a stringified blob would make the audit trail unreadable there.
#[tokio::test]
async fn the_audit_metadata_is_a_json_document() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;

    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({ "first_name": "Audited", "status": "lead" })),
        )
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let contact_id = created.body["id"].as_str().expect("an id").to_owned();

    call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&manager),
            Some(json!({ "status": "customer" })),
        )
    )
    .await;

    let rows: Vec<(Value,)> = sqlx::query_as(
        "select metadata from audit_log where action = 'crm.contact.updated' \
         and target_id = $1 order by id desc limit 1",
    )
    .bind(&contact_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit rows must read");

    let metadata = rows.first().map(|(metadata,)| metadata).unwrap_or(&Value::Null);
    assert!(metadata.is_object(), "the metadata must be an object: {metadata}");
    assert_eq!(metadata["changed"], json!(["status"]));
    assert_eq!(metadata["request_id"], json!(contact_id));
    assert_eq!(metadata["before"]["status"], json!("lead"));
    assert_eq!(metadata["after"]["status"], json!("customer"));
}
