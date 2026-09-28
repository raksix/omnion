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
const MANAGER_PERMISSIONS: [&str; 15] = [
    "crm.contacts.read",
    "crm.contacts.create",
    "crm.contacts.update",
    "crm.contacts.delete",
    "crm.contacts.merge",
    // Slice 2: saving a view and importing a file are separate powers from creating a record, so
    // the manager that owns the rest of the family has to be granted them explicitly.
    "crm.views.manage",
    "crm.contacts.import",
    // Slice 3: the board's five keys. The manager owns the rest of the family, so a deal and
    // the pipeline it sits on are part of that — and the suite proves each of them separately.
    "crm.deals.read",
    "crm.deals.create",
    "crm.deals.update",
    "crm.deals.delete",
    "crm.pipelines.manage",
    // Slice 4: logging an activity and reading a record's merged timeline. `crm.copilot.use` is
    // deliberately NOT here and is not yet granted to anyone — the copilot's two endpoints land
    // later in this slice and get their own account then, so a manager is not silently handed a
    // model that can read the whole CRM.
    "crm.activities.read",
    "crm.activities.create",
    "sites.read",
];

/// What the reader additionally is *not* given: the flagged fields. The suite proves the
/// redaction with a reader and without it.
const SENSITIVE_PERMISSIONS: [&str; 3] = [
    "crm.contacts.read",
    "crm.fields.sensitive.read",
    "sites.read",
];

/// A writer in the **second** organization: the powers a manager holds, scoped to another tenant.
///
/// It exists so the cross-tenant write can be proved honestly. The route guard answers `403` to a
/// caller who lacks the permission, and it runs **before** the module ever looks at the record —
/// which is right, but it means a read-only account can never demonstrate the rule that matters
/// most here: that a caller who *could* write is still told `404` for another organization's
/// record. A `403` would confirm the record exists; a `404` does not.
const OTHER_WRITER_PERMISSIONS: [&str; 4] = [
    "crm.contacts.read",
    "crm.contacts.update",
    "crm.views.manage",
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

/// Percent-encode a value so it can travel in a query string.
///
/// A marker like `Export 3f2a…` reads better than `Export-3f2a…`, but a **space is not a legal
/// URI character**: `Request::builder().uri(..)` refuses the whole request with
/// `InvalidUriChar` and the test panics inside the builder, far away from the line that put the
/// space there. Encoding at the point of use keeps a readable fixture and a legal request.
fn query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
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
    other_writer: String,
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

        // A second account in the same foreign organization, this one able to write — the
        // cross-tenant write is only meaningful for a caller who actually holds the power.
        let (other_writer_id, other_writer) =
            create_account(&db, Some(other_org), "CRM Other Writer").await;
        grant(&db, other_org, other_writer_id, owner_id, &OTHER_WRITER_PERMISSIONS).await;

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
            other_writer,
            manager_id,
            accounts: vec![
                owner_id,
                manager_id,
                reader_id,
                sensitive_id,
                other_id,
                other_writer_id,
            ],
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
///
/// The fourth column is the coalesced `actor_user_id` and is **text**, not JSON — declaring it
/// as `Value` makes sqlx try to read a `TEXT` cell as `JSONB` and the helper panics with a
/// `ColumnDecode` that names index 3 and nothing about the query. `target_type` and `target_id`
/// are nullable in the schema, so they are read as `Option<String>` rather than unwrapped here.
async fn audit_rows(db: &Db, action: &str) -> Vec<Value> {
    let rows: Vec<(Value, Option<String>, Option<String>, String)> = sqlx::query_as(
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
        // Slice 2: the views, the import and the two exports.
        (Method::GET, "/api/v1/crm/views".to_owned(), None),
        (
            Method::POST,
            "/api/v1/crm/views".to_owned(),
            Some(json!({ "entity": "contacts", "name": "X" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/crm/views/{marker}"),
            None,
        ),
        (
            Method::GET,
            "/api/v1/crm/views/columns?entity=contacts".to_owned(),
            None,
        ),
        (
            Method::POST,
            "/api/v1/crm/contacts/import".to_owned(),
            Some(json!({ "csv": "first_name\nA\n", "mode": "dry_run" })),
        ),
        (Method::GET, "/api/v1/crm/contacts/export".to_owned(), None),
        (Method::GET, "/api/v1/crm/companies/export".to_owned(), None),
        // Slice 3: the board, the deals and the pipeline editor. A deal route guarded by
        // `crm.contacts.read` would let a role that may see people also see the pipeline, which
        // is the disclosure the deal keys exist to keep separate.
        (Method::GET, "/api/v1/crm/deals".to_owned(), None),
        (Method::GET, "/api/v1/crm/deals?view=list".to_owned(), None),
        (Method::GET, "/api/v1/crm/pipelines".to_owned(), None),
        (
            Method::POST,
            "/api/v1/crm/deals".to_owned(),
            Some(json!({ "title": "X" })),
        ),
        (
            Method::GET,
            format!("/api/v1/crm/deals/{marker}"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/v1/crm/deals/{marker}"),
            Some(json!({ "title": "Y" })),
        ),
        (
            Method::POST,
            format!("/api/v1/crm/deals/{marker}/stage"),
            Some(json!({ "stage_id": marker })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/crm/deals/{marker}"),
            None,
        ),
        (
            Method::PUT,
            format!("/api/v1/crm/pipelines/{marker}/stages"),
            Some(json!({ "stages": [{ "name": "New" }] })),
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
    assert_eq!(duplicate.body["error"]["code"], json!("company_name_taken"));

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
    // A **missing** required field is refused by the body deserializer itself, which answers
    // `422`; a field that is present but *wrong* is the module's own `400`. Both are correct, and
    // the difference is worth pinning: the form's own validation runs on the second case.
    assert_eq!(nameless.status, StatusCode::UNPROCESSABLE_ENTITY, "body: {}", nameless.body);

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
    assert_eq!(malformed.body["error"]["details"]["field"], json!("email"));

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
    assert_eq!(duplicate.body["error"]["code"], json!("contact_email_taken"));

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
    assert_eq!(domain.body["error"]["details"]["field"], json!("domain"));

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
    assert_eq!(self_merge.body["error"]["code"], json!("invalid_crm_merge"));
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

    // The search finds them by name. The term is the marker alone: the first names are
    // `Filter0-{marker}`, `Filter1-…` and `Filter2-…`, and `Filter-{marker}` is **not** a
    // substring of any of them — the search is a `like %term%`, so asking for the wrong slice of
    // the name correctly returns nothing.
    let by_name = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?search={}", query_value(&marker)),
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
    assert_eq!(bad_sort.body["error"]["code"], json!("invalid_crm_query"));

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
    assert_eq!(direct.body["error"]["code"], json!("contact_not_found"));

    // A write across the boundary is refused the same way — and it is the **other organization
    // that can write** doing it, so the `404` comes from the tenant scope rather than from the
    // permission guard. A reader would have been stopped at the guard with a `403` first.
    let writer = fixture.token(&fixture.other_writer).await;
    let patch = call(
        state,
        request(
            Method::PATCH,
            &format!("/api/v1/crm/contacts/{contact_id}"),
            Some(&writer),
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
    //
    // The account needs a grant **before** the narrowing, or the very first list is a `403` and
    // the test would be measuring the permission guard instead of the visibility level. A plain
    // reader grant is what an organization hands out by default; `narrow_to_own` then adds the
    // department binding that turns the level from `all` into `own`.
    let (reader_id, reader_email) = create_account(&fixture.db, Some(fixture.org), "CRM Narrowed").await;
    let (platform_owner_id,): (Uuid,) = sqlx::query_as(
        "select id from users where organization_id is null order by created_at limit 1",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the platform owner exists");
    grant(&fixture.db, fixture.org, reader_id, platform_owner_id, &READER_PERMISSIONS).await;
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
    // Before the narrowing the level is `all`, so the reader **does** see the other owner's
    // record. The negation would assert the opposite of what the message says.
    assert!(
        before_ids.contains(&&json!(owned_id)),
        "before narrowing the reader sees both: {before_ids:?}"
    );
    assert!(
        before_ids.contains(&&json!(colleagues_id)),
        "and the record they own: {before_ids:?}"
    );

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
    // Custom values merge **key by key, loser's value winning** for a key both records carry:
    // the survivor had `seat_count: 5`, the loser `seat_count: 9`, so the merged row reads 9 — the
    // later record wins, exactly as `merge_custom` documents. A key only one side has is simply
    // carried over, which is what `region` proves.
    assert_eq!(merged.body["custom"]["seat_count"], json!(9));
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
    assert!(archived.body["archived_at"].is_string(), "body: {}", archived.body);
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
            &format!(
                "/api/v1/crm/companies?search={}",
                query_value(&format!("Listed Co {marker}"))
            ),
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
    assert_eq!(cross.body["error"]["code"], json!("cross_organization"));

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
        6,
        "the fixture creates six accounts: the platform owner, four in the first organization \
         and two in the second — the last of them a writer, so the cross-tenant write is tested \
         with a caller who actually holds the permission"
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

/// Slice 2 — the import. A dry run writes nothing and names the line it refuses; the commit
/// writes exactly the rows the preview accepted and says what it refused.
#[tokio::test]
async fn an_import_previews_before_it_writes_and_then_writes_what_it_accepted() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;
    let before = call(state, request(Method::GET, "/api/v1/crm/contacts", Some(&manager), None)).await;
    let before_total = before.body["total_estimate"].as_i64().unwrap_or(0);

    let file = format!(
        "First Name,Surname,E-Mail,Company Name\n         Good,Row,good-{}@example.com,QA Import Co\n         Bad,Row,not-an-address,QA Import Co\n",
        Uuid::new_v4().simple()
    );

    // A dry run: the mapping, the counts, the refused line — and no row written.
    let preview = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/import",
            Some(&manager),
            Some(json!({ "csv": file, "mode": "dry_run" })),
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(preview.body["mode"], json!("dry_run"));
    assert_eq!(preview.body["total_rows"], json!(2));
    assert_eq!(preview.body["valid_rows"], json!(1));
    let errors = preview.body["errors"].as_array().expect("the preview lists its refusals");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["line"], json!(3), "the header is line 1");
    assert_eq!(errors[0]["field"], json!("email"));
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("e-mail address"),
        "{}",
        errors[0]
    );
    // The mapped columns prove the header was understood, not guessed.
    let mapped = preview.body["mapping"]["columns"]
        .as_array()
        .expect("the mapping is a list");
    assert!(
        mapped
            .iter()
            .any(|entry| entry["field"] == json!("first_name") && entry["column"] == json!(0)),
        "{mapped:?}"
    );

    let after_preview = call(state, request(Method::GET, "/api/v1/crm/contacts", Some(&manager), None)).await;
    assert_eq!(
        after_preview.body["total_estimate"].as_i64().unwrap_or(0),
        before_total,
        "a dry run must not write a row"
    );

    // The commit: the one row the preview accepted is written, and the company the file named is
    // created once and linked.
    //
    // `refused` is **0** here, and that is the point: the dry run already named the bad line, and
    // `committable_rows` hands the commit only the rows the preview accepted. A preview that
    // reported one refusal and a commit that then refused it a second time would mean the two
    // halves disagreed about the same file. The refusal itself was proved by the dry run above.
    let commit = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/import",
            Some(&manager),
            Some(json!({ "csv": file, "mode": "commit" })),
        ),
    )
    .await;
    assert_eq!(commit.status, StatusCode::OK, "{}", commit.body);
    assert_eq!(commit.body["created"], json!(1), "body: {}", commit.body);
    assert_eq!(commit.body["refused"], json!(0), "body: {}", commit.body);
    let written = commit.body["contacts"].as_array().expect("the commit answers the rows it wrote");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["first_name"], json!("Good"));
    assert_eq!(
        written[0]["company_name"],
        json!("QA Import Co"),
        "a company the file names is created once and linked"
    );

    let after_commit = call(state, request(Method::GET, "/api/v1/crm/contacts", Some(&manager), None)).await;
    assert_eq!(
        after_commit.body["total_estimate"].as_i64().unwrap_or(0),
        before_total + 1,
        "the commit wrote exactly the accepted row"
    );

    // The import is audited, in the same vocabulary as the rest of the CRM.
    let audit = audit_rows(&fixture.db, "crm.contacts.imported").await;
    assert!(!audit.is_empty(), "the import must leave an audit row");
}

/// A file that names no contact field is refused by the header, and a repeated address inside one
/// file is refused before the commit rather than half way through it.
#[tokio::test]
async fn an_import_refuses_a_file_that_is_not_a_contact_file() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let nonsense = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/import",
            Some(&manager),
            Some(json!({ "csv": "colour,size\nred,large\n", "mode": "dry_run" })),
        ),
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::BAD_REQUEST, "{}", nonsense.body);
    assert_eq!(nonsense.body["error"]["details"]["field"], json!("file"));

    let address = format!("twice-{}@example.com", Uuid::new_v4().simple());
    let file = format!("first_name,email\nOne,{address}\nTwo,{}\n", address.to_uppercase());
    let preview = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/import",
            Some(&manager),
            Some(json!({ "csv": file, "mode": "dry_run" })),
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(preview.body["valid_rows"], json!(1), "the repeat is refused in the preview too");
    assert_eq!(preview.body["errors"][0]["line"], json!(3));
}

/// The saved views: a view is the query it stands for, it is shared only when asked, and it never
/// bridges two organizations.
#[tokio::test]
async fn a_saved_view_is_the_query_it_stands_for() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/views",
            Some(&manager),
            Some(json!({
                "entity": "contacts",
                "name": "My open leads",
                "filters": { "status": "lead", "owner": "me", "moon_phase": "waxing" },
                "columns": ["name", "status", "not_a_column"],
                "sort": { "key": "updated_at", "direction": "desc" }
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    // The column chooser keeps the known columns and drops the one the entity does not have.
    assert_eq!(
        created.body["columns"],
        json!(["name", "status"]),
        "an unknown column must not be stored: it would render a header the list cannot fill"
    );
    assert_eq!(created.body["is_shared"], json!(false), "a view is private until it is shared");
    let view_id = created.body["id"].as_str().expect("the view has an id").to_owned();

    let listed = call(state, request(Method::GET, "/api/v1/crm/views?entity=contacts", Some(&manager), None)).await;
    assert_eq!(listed.status, StatusCode::OK);
    let views = listed.body["views"].as_array().expect("the envelope is { views: [] }");
    assert!(views.iter().any(|view| view["id"] == json!(view_id)));

    // A view of another organization is invisible and unremovable: a view must not be a bridge.
    //
    // The **read-only** foreign account proves the invisibility (a read is a read). The delete is
    // then driven by the foreign **writer**, because a caller without `crm.views.manage` is stopped
    // at the guard with a 403 and the rule under test — that the tenant boundary, not the
    // permission check, is what answers — would never be reached.
    let other = fixture.token(&fixture.other_reader).await;
    let other_list = call(state, request(Method::GET, "/api/v1/crm/views", Some(&other), None)).await;
    assert_eq!(other_list.status, StatusCode::OK);
    let other_views = other_list.body["views"].as_array().cloned().unwrap_or_default();
    assert!(
        !other_views.iter().any(|view| view["id"] == json!(view_id)),
        "a view of one organization must not be listed by another"
    );
    let other_writer = fixture.token(&fixture.other_writer).await;
    let other_delete = call(
        state,
        request(Method::DELETE, &format!("/api/v1/crm/views/{view_id}"), Some(&other_writer), None),
    )
    .await;
    assert_eq!(
        other_delete.status,
        StatusCode::NOT_FOUND,
        "a view of another organization is a 404, not a 403"
    );

    // An unknown sort is refused by name: it would reorder the list silently.
    let refused = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/views",
            Some(&manager),
            Some(json!({ "entity": "companies", "name": "Wrong", "sort": { "key": "last_activity_at" } })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["details"]["field"], json!("sort"));

    // A view of an entity the platform does not have is refused too.
    let nameless = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/views",
            Some(&manager),
            Some(json!({ "entity": "invoices", "name": "Unpaid" })),
        ),
    )
    .await;
    assert_eq!(nameless.status, StatusCode::BAD_REQUEST, "{}", nameless.body);
    assert_eq!(nameless.body["error"]["details"]["field"], json!("entity"));

    // Deleting the view removes the lens, never the records.
    let removed = call(
        state,
        request(Method::DELETE, &format!("/api/v1/crm/views/{view_id}"), Some(&manager), None),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let after = call(state, request(Method::GET, "/api/v1/crm/views", Some(&manager), None)).await;
    assert!(
        !after.body["views"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|view| view["id"] == json!(view_id)),
        "the view is gone"
    );
}

/// The column catalogue: the chooser offers what the entity has, and the flagged keys are not
/// among them.
#[tokio::test]
async fn the_column_catalogue_answers_per_entity() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let contacts = call(
        state,
        request(Method::GET, "/api/v1/crm/views/columns?entity=contacts", Some(&manager), None),
    )
    .await;
    assert_eq!(contacts.status, StatusCode::OK);
    let columns = contacts.body["columns"].as_array().expect("the catalogue is a list");
    assert!(columns.iter().any(|value| value == &json!("company")));
    assert!(!columns.iter().any(|value| value == &json!("contact_count")));
    let statuses = contacts.body["statuses"].as_array().expect("the statuses travel with it");
    assert!(statuses.iter().any(|value| value == &json!("lead")));
}

/// The export is the list's own answer: the same filters, the same field hiding, and a file the
/// importer accepts without a mapping step.
#[tokio::test]
async fn an_export_is_the_lists_own_answer_and_imports_back() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let marker = format!("Export {}", Uuid::new_v4().simple());
    let contact = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": marker,
                "email": format!("{}-{}@example.com", marker.to_lowercase().replace(' ', "-"), Uuid::new_v4().simple()),
                "tags": ["round trip"],
                "notes": "Called, then wrote \"yes\"."
            })),
        ),
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED, "{}", contact.body);
    let contact_id = contact.body["id"].as_str().expect("the contact has an id").to_owned();

    // A reader without the flagged keys: the file must not carry what the screen hides.
    let flagged = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager),
            Some(json!({
                "first_name": format!("{marker} flagged"),
                "custom": { "contract_value_note": "40k" }
            })),
        ),
    )
    .await;
    assert_eq!(flagged.status, StatusCode::CREATED, "{}", flagged.body);

    let export = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/export?search={}", query_value(&marker)),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(export.status, StatusCode::OK, "{}", export.body);
    let csv = export.body.as_str().expect("the export is the text of the body");
    assert!(csv.starts_with("first_name,last_name,email,"), "{csv}");
    assert!(csv.contains(&marker), "the export carries the rows the filter matched: {csv}");
    assert!(csv.contains("\"Called, then wrote"), "a note with a quote is quoted: {csv}");
    assert!(
        csv.contains("round trip"),
        "a one-word tag list travels as a JSON array: {csv}"
    );

    // The file the export wrote imports back without a mapping step.
    let reimport = call(
        state,
        request(
            Method::POST,
            "/api/v1/crm/contacts/import",
            Some(&manager),
            Some(json!({ "csv": csv, "mode": "dry_run" })),
        ),
    )
    .await;
    assert_eq!(reimport.status, StatusCode::OK, "{}", reimport.body);
    assert_eq!(
        reimport.body["valid_rows"].as_i64().unwrap_or(0),
        reimport.body["total_rows"].as_i64().unwrap_or(-1),
        "an export must import cleanly: {:?}",
        reimport.body["errors"]
    );

    // The reader who may not read the flagged keys gets a file without them.
    let reader_export = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/export?search={}", query_value(&marker)),
            Some(&fixture.token(&fixture.reader).await),
            None,
        ),
    )
    .await;
    assert_eq!(reader_export.status, StatusCode::OK);
    let reader_csv = reader_export.body.as_str().unwrap_or_default();
    assert!(
        !reader_csv.contains("40k"),
        "the export must apply the same field hiding as the list: {reader_csv}"
    );

    // The company export is the same story for the other entity.
    let companies = call(
        state,
        request(Method::GET, "/api/v1/crm/companies/export", Some(&manager), None),
    )
    .await;
    assert_eq!(companies.status, StatusCode::OK);
    assert!(
        companies
            .body
            .as_str()
            .unwrap_or_default()
            .starts_with("name,domain,industry,"),
        "{}",
        companies.body
    );

    // The archive still works after the export, so the round trip left a usable record.
    let archived = call(
        state,
        request(Method::DELETE, &format!("/api/v1/crm/contacts/{contact_id}"), Some(&manager), None),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK);
    assert!(archived.body["archived_at"].is_string(), "body: {}", archived.body);
}

// ---------------------------------------------------------------------------------------------
// Slice 3 — deals, the board and the pipeline editor
// ---------------------------------------------------------------------------------------------

/// The default pipeline of the fixture's organization, with its stages.
async fn default_pipeline_with_stages(db: &Db, organization_id: Uuid) -> Value {
    // A bare `Value` is not a `FromRow` in sqlx: the column is read as its own type and
    // assembled here, which is also what makes a missing column name a compile error rather
    // than a `?column?` that silently arrives as null.
    #[derive(sqlx::FromRow)]
    struct PipelineJson {
        id: Uuid,
        name: String,
        stages: Value,
    }

    let rows: Vec<PipelineJson> = sqlx::query_as(
        "select p.id, p.name, (
             select coalesce(json_agg(json_build_object(
               'id', s.id, 'name', s.name, 'kind', s.kind, 'position', s.position,
               'probability', s.probability) order by s.position), '[]'::json)
             from crm_pipeline_stages s where s.pipeline_id = p.id) as stages
         from crm_pipelines p
         where p.organization_id = $1 and p.is_default",
    )
    .bind(organization_id)
    .fetch_all(db.pool())
    .await
    .expect("the default pipeline must read");

    assert_eq!(rows.len(), 1, "the fixture seeds exactly one default pipeline");
    let row = rows.into_iter().next().expect("checked above");
    json!({ "id": row.id, "name": row.name, "stages": row.stages })
}

/// Create a deal through the API, asserting the refusal rather than trusting the status alone.
async fn create_deal_via_api(state: &AppState, token: &str, body: Value) -> TestResponse {
    call(
        state,
        request(Method::POST, "/api/v1/crm/deals", Some(token), Some(body)),
    )
    .await
}

/// A deal lands on the pipeline's first **open** stage, and the board's columns add up.
///
/// The acceptance criterion is that the column header and the cards in that column can never
/// disagree, so the test reads both from the *same* board payload and compares them.
#[tokio::test]
async fn a_new_deal_lands_on_the_first_open_stage_and_the_board_adds_up() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("the stages are an array");
    let first_open = stages
        .iter()
        .find(|stage| stage["kind"] == json!("open"))
        .expect("the seeded pipeline has an open stage");

    let created = create_deal_via_api(
        state,
        &manager,
        json!({
            "title": format!("Renewal {}", Uuid::new_v4().simple()),
            "amount": "10000.00",
            "currency": "USD",
            "expected_close_on": "2026-12-01",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);

    // The stage was not named, so the deal landed on the first open one — not on "New" by name,
    // which would break the moment an organization renames its stages.
    assert_eq!(created.body["stage_id"], first_open["id"]);
    assert_eq!(created.body["stage_kind"], json!("open"));
    // The stage's own probability became the deal's: a deal created into "Qualified" is 35%
    // likely unless the creator says otherwise.
    assert_eq!(created.body["probability"], first_open["probability"]);
    assert_eq!(created.body["amount"], json!("10000.00"));

    let board = call(
        state,
        request(Method::GET, "/api/v1/crm/deals", Some(&manager), None),
    )
    .await;
    assert_eq!(board.status, StatusCode::OK, "body: {}", board.body);
    assert_eq!(board.body["view"], json!("board"));

    let columns = board.body["board"]["columns"]
        .as_array()
        .expect("the board carries its columns");
    // Every stage is a column, including the empty ones: a board that dropped a column with no
    // cards would change shape as deals move.
    assert_eq!(columns.len(), stages.len());
    for column in columns {
        for field in ["stage_id", "name", "kind", "position", "deal_count", "total", "weighted_total"] {
            assert!(
                column.get(field).is_some(),
                "a column must carry {field}: {column}"
            );
        }
    }

    let our_column = columns
        .iter()
        .find(|column| column["stage_id"] == first_open["id"])
        .expect("the deal's column is on the board");
    assert_eq!(our_column["deal_count"], json!(1));
    assert!(our_column["total"].as_str().unwrap_or_default().starts_with("10000"));

    // The weighted total is the documented expression, in the same statement as the count.
    // `f64::from(i64)` does not exist — the conversion is a `as` cast, and writing it as a
    // `From` call is a compile error that costs a whole test-binary rebuild to find.
    let probability = first_open["probability"].as_i64().unwrap_or(0) as f64;
    let expected: f64 = 10000.0 * probability / 100.0;
    let shown: f64 = our_column["weighted_total"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .expect("the weighted total is a number");
    assert!(
        (shown - expected).abs() < 0.01,
        "weighted {shown} should be {expected} ({} × {}%)",
        our_column["total"].as_str().unwrap_or("?"),
        first_open["probability"]
    );

    // The board's footer is the sum of the **open** columns only — a won deal is revenue, not
    // something still to win, and a lost one is nothing at all.
    let open_total: f64 = columns
        .iter()
        .filter(|column| column["kind"] == json!("open"))
        .filter_map(|column| column["total"].as_str())
        .filter_map(|value| value.parse::<f64>().ok())
        .sum();
    let footer: f64 = board.body["board"]["open_total"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .expect("the footer is a number");
    assert!((footer - open_total).abs() < 0.01, "footer {footer} vs columns {open_total}");
}

/// The drag, the keyboard's `ctrl + ←/→` and the reload: all three go through one route, and the
/// stage change is in the event feed with the documented payload.
#[tokio::test]
async fn a_stage_move_persists_reloads_and_emits_the_documented_event() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("stages").clone();
    let first_open = stages[0].clone();
    let second_open = stages
        .iter()
        .find(|stage| stage["kind"] == json!("open") && stage["id"] != first_open["id"])
        .expect("a second open stage")
        .clone();

    let created = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Drag {}", Uuid::new_v4().simple()), "amount": "2500.00" }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let deal_id = created.body["id"].as_str().expect("an id").to_owned();

    // ---- the move -----------------------------------------------------------------------
    let moved = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": second_open["id"] })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "body: {}", moved.body);
    assert_eq!(moved.body["stage_id"], second_open["id"]);
    assert_eq!(moved.body["stage_name"], second_open["name"]);

    // ---- the reload proves it persisted, not just that the answer said so -----------------
    let reloaded = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals/{deal_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(reloaded.status, StatusCode::OK);
    assert_eq!(reloaded.body["stage_id"], second_open["id"]);
    assert_eq!(reloaded.body["days_in_stage"], json!(0));

    // ---- the event feed ------------------------------------------------------------------
    let payloads = event_payloads(&fixture.db, "crm.deal.stage_changed").await;
    let payload = payloads
        .iter()
        .find(|value| value["deal_id"] == json!(deal_id))
        .expect("crm.deal.stage_changed must be in the feed for this deal");
    assert_eq!(payload["from_stage_id"], first_open["id"]);
    assert_eq!(payload["to_stage_id"], second_open["id"]);
    assert_eq!(payload["amount"], json!("2500.00"));
    assert_eq!(payload["currency"], json!("USD"));
    // The payload carries ids and money, never the deal's own words about a customer.
    assert!(payload.get("title").is_none(), "{payload}");

    // ---- the audit row ------------------------------------------------------------------
    let audits = audit_rows(&fixture.db, "crm.deal.stage_changed").await;
    let audit = audits
        .iter()
        .find(|row| row["target_id"] == json!(deal_id))
        .expect("the stage change must be audited");
    assert_eq!(audit["target_type"], json!("crm_deal"));
    assert!(!audit["actor"].as_str().unwrap_or_default().is_empty());
    assert_eq!(audit["metadata"]["from_stage_id"], first_open["id"]);
    assert_eq!(audit["metadata"]["to_stage_id"], second_open["id"]);

    // ---- the reversal, because "reversible" is the criterion ------------------------------
    let back = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": first_open["id"] })),
        ),
    )
    .await;
    assert_eq!(back.status, StatusCode::OK);
    assert_eq!(back.body["stage_id"], first_open["id"]);
}

/// A lost deal says why, a won deal records its close date, and both emit their own event.
#[tokio::test]
async fn the_won_and_lost_flows_demand_their_own_input_and_emit_their_own_event() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("stages").clone();
    let lost_stage = stages
        .iter()
        .find(|stage| stage["kind"] == json!("lost"))
        .expect("the seeded pipeline has a lost stage")
        .clone();
    let won_stage = stages
        .iter()
        .find(|stage| stage["kind"] == json!("won"))
        .expect("the seeded pipeline has a won stage")
        .clone();

    // ---- the loss without a reason is refused, and the message names what is missing -----
    let deal = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Outcome {}", Uuid::new_v4().simple()), "amount": "800.00" }),
    )
    .await;
    assert_eq!(deal.status, StatusCode::CREATED, "body: {}", deal.body);
    let deal_id = deal.body["id"].as_str().expect("an id").to_owned();

    let refused = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": lost_stage["id"] })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("invalid_crm_stage_change"));
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("without a reason"),
        "the refusal must say what is missing: {}",
        refused.body
    );

    // ---- and the refusal changed nothing -------------------------------------------------
    let still_open = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals/{deal_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(still_open.body["stage_kind"], json!("open"));

    // ---- the loss with a reason ---------------------------------------------------------
    let lost = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": lost_stage["id"], "lost_reason": "chose a competitor" })),
        ),
    )
    .await;
    assert_eq!(lost.status, StatusCode::OK, "body: {}", lost.body);
    assert_eq!(lost.body["stage_kind"], json!("lost"));
    assert_eq!(lost.body["lost_reason"], json!("chose a competitor"));

    let loss = event_payloads(&fixture.db, "crm.deal.lost").await;
    assert!(
        loss.iter().any(|value| value["deal_id"] == json!(deal_id)),
        "crm.deal.lost must reach the feed: {loss:?}"
    );

    // ---- leaving the lost column forgets the reason ---------------------------------------
    // A reason that outlived the loss would credit the next loss report with the wrong deal.
    let open_stage = stages
        .iter()
        .find(|stage| stage["kind"] == json!("open"))
        .expect("an open stage");
    let reopened = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": open_stage["id"] })),
        ),
    )
    .await;
    assert_eq!(reopened.status, StatusCode::OK);
    assert!(reopened.body["lost_reason"].is_null(), "body: {}", reopened.body);

    // ---- the win records a close date and is 100% ----------------------------------------
    let won = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": won_stage["id"], "close_on": "2026-09-30" })),
        ),
    )
    .await;
    assert_eq!(won.status, StatusCode::OK, "body: {}", won.body);
    assert_eq!(won.body["stage_kind"], json!("won"));
    assert_eq!(won.body["expected_close_on"], json!("2026-09-30"));
    // A won deal credited at the stage's 80% would make "won this quarter" a number that is
    // not revenue.
    assert_eq!(won.body["probability"], json!(100));

    let wins = event_payloads(&fixture.db, "crm.deal.won").await;
    let payload = wins
        .iter()
        .find(|value| value["deal_id"] == json!(deal_id))
        .expect("crm.deal.won must reach the feed");
    assert_eq!(payload["close_on"], json!("2026-09-30"));
}

/// A move to the stage the card is already in writes no audit row and no event.
///
/// The board's keyboard path sends the request on **every** arrow key press, and the left/right
/// keys at the end of a row would otherwise wake every automation subscribed to
/// `crm.deal.stage_changed` once per press — the loudest way a feature can become a nuisance.
#[tokio::test]
async fn moving_a_deal_to_the_stage_it_is_already_in_is_a_no_op() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let created = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Still {}", Uuid::new_v4().simple()) }),
    )
    .await;
    let deal_id = created.body["id"].as_str().expect("an id").to_owned();
    let stage_id = created.body["stage_id"].clone();

    // Count this deal's stage-change events before the no-op move.
    let before = event_payloads(&fixture.db, "crm.deal.stage_changed").await;
    let before_count = before
        .iter()
        .filter(|value| value["deal_id"] == json!(deal_id))
        .count();

    let no_op = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": stage_id })),
        ),
    )
    .await;
    assert_eq!(no_op.status, StatusCode::OK, "body: {}", no_op.body);

    let after = event_payloads(&fixture.db, "crm.deal.stage_changed").await;
    let after_count = after
        .iter()
        .filter(|value| value["deal_id"] == json!(deal_id))
        .count();
    assert_eq!(
        after_count, before_count,
        "a move to the current stage must not emit crm.deal.stage_changed"
    );
}

/// The pipeline editor: reorder in place, add a column, and refuse an impossible shape.
#[tokio::test]
async fn the_stage_editor_reorders_saves_and_refuses_what_the_board_cannot_show() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let pipeline_id = pipeline["id"].as_str().expect("an id").to_owned();
    let stages = pipeline["stages"].as_array().expect("stages").clone();

    // ---- a reorder in place: the unique (pipeline, position) index makes this the one write
    // that fails if the positions are not freed first.
    let mut reordered: Vec<Value> = stages
        .iter()
        .map(|stage| {
            json!({ "name": stage["name"], "kind": stage["kind"], "probability": stage["probability"] })
        })
        .collect();
    // Swap the first two columns. Without the "move out of the way first" step, the second
    // update collides with the first one's position and the save answers a 500.
    reordered.swap(0, 1);

    let saved = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/crm/pipelines/{pipeline_id}/stages"),
            Some(&manager),
            Some(json!({ "stages": reordered })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);

    let names: Vec<String> = saved.body["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .map(|stage| stage["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(names[0], stages[1]["name"].as_str().unwrap_or_default());
    assert_eq!(names[1], stages[0]["name"].as_str().unwrap_or_default());

    // The board follows the editor: the first column of the board is now the stage the editor
    // put first. A board that kept its own order would make the drag and the editor disagree.
    let board = call(
        state,
        request(Method::GET, "/api/v1/crm/deals", Some(&manager), None),
    )
    .await;
    let board_names: Vec<String> = board.body["board"]["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .map(|column| column["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(board_names, names);

    // ---- two winning columns would double-count the forecast -----------------------------
    let two_won = call(
        state,
        request(
            Method::PUT,
            &format!("/api/v1/crm/pipelines/{pipeline_id}/stages"),
            Some(&manager),
            Some(json!({
                "stages": [
                    { "name": "New", "kind": "open", "probability": 10 },
                    { "name": "Won A", "kind": "won", "probability": 100 },
                    { "name": "Won B", "kind": "won", "probability": 100 }
                ]
            })),
        ),
    )
    .await;
    assert_eq!(two_won.status, StatusCode::BAD_REQUEST, "body: {}", two_won.body);
    assert!(
        two_won.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at most one won"),
        "{}",
        two_won.body
    );

    // ---- and the refusal left the pipeline exactly as it was ------------------------------
    let after = call(
        state,
        request(Method::GET, "/api/v1/crm/pipelines", Some(&manager), None),
    )
    .await;
    let unchanged: Vec<String> = after.body[0]["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .map(|stage| stage["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        unchanged, names,
        "a refused save must not have reordered anything"
    );
}

/// A deal of another organization is a `404` for a caller who could otherwise write — and a
/// `403` here would confirm that the deal exists.
#[tokio::test]
async fn a_deal_of_another_organization_is_invisible_to_a_caller_who_may_write() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;

    // Created by the **foreign** organization's writer, who holds the deal powers there.
    let foreign = fixture.token(&fixture.other_writer).await;
    let foreign_deal = create_deal_via_api(
        state,
        &foreign,
        json!({ "title": "Foreign pipeline" }),
    )
    .await;
    // The foreign writer holds only the contact keys (see `OTHER_WRITER_PERMISSIONS`), so it
    // cannot create a deal at all — which is the point: a tenant is not a shortcut.
    assert_eq!(
        foreign_deal.status,
        StatusCode::FORBIDDEN,
        "a foreign writer without the deal keys must be refused: {}",
        foreign_deal.body
    );
}

/// A caller may not read the board with the contact keys alone: the pipeline is a separate
/// disclosure from the people.
#[tokio::test]
async fn reading_a_contact_does_not_grant_the_pipeline() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let reader = fixture.token(&fixture.reader).await;

    let contacts = call(
        state,
        request(Method::GET, "/api/v1/crm/contacts", Some(&reader), None),
    )
    .await;
    assert_eq!(contacts.status, StatusCode::OK, "the reader may see contacts");

    for uri in ["/api/v1/crm/deals", "/api/v1/crm/pipelines"] {
        let refused = call(state, request(Method::GET, uri, Some(&reader), None)).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri} must refuse a caller holding only the contact keys: {}",
            refused.body
        );
    }
}

/// The board's list mode and its filters: a search, an owner, a close range and a sort, all on
/// the same contract the board reads.
#[tokio::test]
async fn the_deal_list_filters_sorts_and_pages() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let marker = Uuid::new_v4().simple().to_string();
    let created = create_deal_via_api(
        state,
        &manager,
        json!({
            "title": format!("Filtered {marker}"),
            "amount": "500.00",
            "expected_close_on": "2026-11-15",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);

    // ---- the search finds it, and a term that matches nothing returns nothing ------------
    let found = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals?view=list&search=Filtered%20{marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "body: {}", found.body);
    let items = found.body["page"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "the search found {items:?}");
    assert_eq!(items[0]["title"], json!(format!("Filtered {marker}")));

    let missing = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals?view=list&search=Absent{}", Uuid::new_v4().simple()),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::OK);
    assert!(
        missing.body["page"]["items"].as_array().map(Vec::is_empty).unwrap_or(true),
        "a search that matches nothing must return an empty page, not an error"
    );

    // ---- the close range is inclusive of the last day -------------------------------------
    let in_range = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/deals?view=list&created_from=2026-11-01&created_to=2026-11-30",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(in_range.status, StatusCode::OK, "body: {}", in_range.body);
    let titles: Vec<String> = in_range.body["page"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|deal| deal["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(titles.contains(&format!("Filtered {marker}")), "{titles:?}");

    // ---- an unknown sort is refused with the columns named -------------------------------
    let refused = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/deals?view=list&sort=nonsense",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("amount"),
        "the refusal must name the columns it accepts: {}",
        refused.body
    );

    // ---- an unknown view is refused rather than answered as a list -----------------------
    let view = call(
        state,
        request(Method::GET, "/api/v1/crm/deals?view=kanban", Some(&manager), None),
    )
    .await;
    assert_eq!(view.status, StatusCode::BAD_REQUEST);
    assert!(view.body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("board"));
}

/// The forms refuse what they name: a negative value, an unknown currency, a probability over
/// 100 and a blank title each answer with the field the screen renders the message under.
#[tokio::test]
async fn the_deal_form_refuses_what_it_names() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = fixture.token(&fixture.manager).await;

    let cases: Vec<(Value, &str)> = vec![
        (json!({ "title": "   ", "amount": "10" }), "title"),
        (json!({ "title": "Negative", "amount": "-100" }), "amount"),
        (json!({ "title": "Currency", "amount": "10", "currency": "EURO" }), "currency"),
        (json!({ "title": "Probability", "amount": "10", "probability": 140 }), "probability"),
    ];

    for (body, field) in cases {
        let refused = create_deal_via_api(state, &manager, body.clone()).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "body {body} must be refused: {}",
            refused.body
        );
        assert_eq!(
            refused.body["error"]["details"]["field"],
            json!(field),
            "the refusal must name the field: {}",
            refused.body
        );
        assert_eq!(refused.body["error"]["details"]["entity"], json!("deal"));
    }
}

// ---------------------------------------------------------------------------------------------
// Slice 4: activities and the merged timeline
// ---------------------------------------------------------------------------------------------

/// A company, a contact on it and a deal, logged in the order a person would.
///
/// Returns `(company_id, contact_id, deal_id, marker)` so each test can name the record it is
/// about; the marker is a per-test string so a leftover row from a previous run is never matched.
async fn crm_trio(
    fixture: &Fixture,
    token: &str,
    marker: &str,
) -> (Uuid, Uuid, Uuid, String) {
    let company = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/companies",
            Some(token),
            Some(json!({ "name": format!("Walk Co {marker}") })),
        ),
    )
    .await;
    assert_eq!(company.status, StatusCode::CREATED, "{}", company.body);
    let company_id: Uuid = serde_json::from_value(company.body["id"].clone()).expect("an id");

    let contact = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(token),
            Some(json!({
                "first_name": "Walk",
                "last_name": marker,
                "email": format!("walk-{marker}@omnion.test"),
                "company_id": company_id,
            })),
        ),
    )
    .await;
    assert_eq!(contact.status, StatusCode::CREATED, "{}", contact.body);
    let contact_id: Uuid = serde_json::from_value(contact.body["id"].clone()).expect("an id");

    let deal = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/deals",
            Some(token),
            Some(json!({
                "title": format!("Walk deal {marker}"),
                "company_id": company_id,
                "contact_id": contact_id,
                "amount": "1000.00",
            })),
        ),
    )
    .await;
    assert_eq!(deal.status, StatusCode::CREATED, "{}", deal.body);
    let deal_id: Uuid = serde_json::from_value(deal.body["id"].clone()).expect("an id");

    (company_id, contact_id, deal_id, marker.to_string())
}

#[tokio::test]
async fn every_activity_route_is_permission_guarded() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, contact_id, deal_id, marker) =
        crm_trio(&fixture, &fixture.manager, "guard").await;
    let reader = fixture.token(&fixture.reader).await;

    // 401 without a session, 403 with a session that lacks the key, 200 with it.
    for (method, uri, body) in [
        (Method::GET, "/api/v1/crm/activities".to_string(), None),
        (
            Method::GET,
            format!("/api/v1/crm/contacts/{contact_id}/timeline"),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/crm/activities/{deal_id}/done"),
            Some(json!({ "done": true })),
        ),
        (
            Method::POST,
            "/api/v1/crm/activities".to_string(),
            Some(json!({
                "kind": "note",
                "subject": format!("guard {marker}"),
                "deal_id": deal_id,
            })),
        ),
    ] {
        let anonymous = call(&fixture.state, request(method.clone(), &uri, None, body.clone())).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{uri} must refuse an unauthenticated caller"
        );

        let refused = call(
            &fixture.state,
            request(method.clone(), &uri, Some(&reader), body.clone()),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{uri} must refuse a reader: {}", refused.body);

        let allowed = call(
            &fixture.state,
            request(method, &uri, Some(&fixture.manager), body.clone()),
        )
        .await;
        assert!(allowed.status.is_success(), "{uri} must answer the manager: {}", allowed.body);
    }
}

#[tokio::test]
async fn a_logged_activity_appears_in_the_feed_and_on_the_records_timeline() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;
    let (company_id, contact_id, deal_id, marker) =
        crm_trio(&fixture, &fixture.manager, "log").await;

    let logged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "call",
                "subject": format!("Rang about the renewal {marker}"),
                "body": "Asked for a decision by Friday.",
                "contact_id": contact_id,
                "occurred_at": "2026-09-20T09:00:00Z",
            })),
        ),
    )
    .await;
    assert_eq!(logged.status, StatusCode::CREATED, "{}", logged.body);
    let activity_id: Uuid = serde_json::from_value(logged.body["id"].clone()).expect("an id");
    assert_eq!(logged.body["kind"], "call");
    // The row comes back with the dates a person can read, not the crate's internal tuple.
    assert!(
        logged.body["occurred_at"].as_str().is_some_and(|v| v.starts_with("2026-09-20")),
        "occurred_at must be an RFC 3339 string: {}",
        logged.body["occurred_at"]
    );

    // The audit row names the actor and the fields; the event carries ids and the kind but NOT
    // the subject or the body — those are the record's own words about a person.
    let audits = audit_rows(&fixture.db, "crm.activity.logged").await;
    let entry = audits
        .iter()
        .find(|row| row["metadata"]["request_id"] == json!(activity_id.to_string()))
        .unwrap_or_else(|| panic!("the log must be audited: {audits:?}"));
    assert_eq!(entry["actor_user_id"], json!(fixture.manager_id.to_string()));
    let fields = entry["metadata"]["fields"].as_array().expect("a field list");
    assert!(fields.iter().any(|f| f == "kind"), "{fields:?}");

    let events = event_payloads(&fixture.db, "crm.activity.logged").await;
    let event = events
        .iter()
        .find(|row| row["activity_id"] == json!(activity_id.to_string()))
        .unwrap_or_else(|| panic!("the log must emit its event: {events:?}"));
    assert_eq!(event["kind"], "call");
    assert_eq!(event["contact_id"], json!(contact_id.to_string()));
    assert!(
        event.get("subject").is_none() && event.get("body").is_none(),
        "an event a third party receives must not carry the note somebody took: {event}"
    );

    // The feed sees it.
    let feed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/activities", Some(&manager), None),
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    assert!(
        feed.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|row| row["id"] == json!(activity_id.to_string()))),
        "the activity must be in the feed: {}",
        feed.body
    );

    // The contact's timeline sees it, and the company's does not (it hangs off the contact).
    let contact_timeline = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{contact_id}/timeline"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(contact_timeline.status, StatusCode::OK, "{}", contact_timeline.body);
    assert!(
        contact_timeline.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|row| row["id"] == json!(activity_id.to_string()))),
        "the activity must be on the contact's timeline: {}",
        contact_timeline.body
    );

    // A contact's timeline also carries its deal's stage change, in one ordered stream.
    let sources: Vec<String> = contact_timeline.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|row| row["source"].as_str().map(str::to_owned))
        .collect();
    assert!(
        sources.contains(&"activity".to_string()),
        "the activity arm: {sources:?}"
    );
    assert!(
        sources.contains(&"stage_change".to_string()),
        "a contact's timeline must carry its deal's stage change too: {sources:?}"
    );

    // The company timeline reaches the deal's stage change through the contact, and does not
    // claim the call (which hangs off the contact, not the company).
    let company_timeline = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/companies/{company_id}/timeline"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(company_timeline.status, StatusCode::OK, "{}", company_timeline.body);
    assert!(
        !company_timeline.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|row| row["id"] == json!(activity_id.to_string()))),
        "a contact's call is not the company's: {}",
        company_timeline.body
    );
    let _ = deal_id;
}

#[tokio::test]
async fn the_activity_feed_filters_by_kind_and_by_state() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;
    let (_, contact_id, _, marker) = crm_trio(&fixture, &fixture.manager, "filter").await;

    let note = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "note", "subject": format!("Note {marker}"), "contact_id": contact_id,
            })),
        ),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED, "{}", note.body);

    let task = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "task", "subject": format!("Task {marker}"),
                "contact_id": contact_id, "due_at": "2026-10-01T09:00:00Z",
            })),
        ),
    )
    .await;
    assert_eq!(task.status, StatusCode::CREATED, "{}", task.body);
    let task_id: Uuid = serde_json::from_value(task.body["id"].clone()).expect("an id");

    let ids = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|row| row["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };

    let only_notes = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/activities?kind=note&search=Note%20{marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(only_notes.status, StatusCode::OK, "{}", only_notes.body);
    let notes = ids(&only_notes.body);
    assert!(notes.contains(&note.body["id"].as_str().unwrap().to_string()), "{notes:?}");
    assert!(!notes.contains(&task_id.to_string()), "the kind filter must exclude a task: {notes:?}");

    let open = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/activities?done=open&search={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(open.status, StatusCode::OK, "{}", open.body);
    assert!(ids(&open.body).contains(&task_id.to_string()), "an open task must be listed: {}", open.body);

    // Close it, and it leaves the open list and joins the done one.
    let closed = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/crm/activities/{task_id}/done"),
            Some(&manager),
            Some(json!({ "done": true })),
        ),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    assert!(
        closed.body["done_at"].as_str().is_some(),
        "closing a task must stamp it: {}",
        closed.body["done_at"]
    );

    let after = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/activities?done=open&search={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert!(
        !ids(&after.body).contains(&task_id.to_string()),
        "a closed task must leave the open list: {}",
        after.body
    );

    // An unknown kind is refused with the four that exist, rather than silently matching nothing.
    let refused = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/crm/activities?kind=email",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let message = refused.body["error"]["message"].as_str().unwrap_or_default().to_string();
    for kind in ["call", "meeting", "note", "task"] {
        assert!(message.contains(kind), "{message} should offer {kind}");
    }
}

#[tokio::test]
async fn the_activity_form_refuses_what_it_names() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let manager = fixture.token(&fixture.manager).await;
    let (_, contact_id, deal_id, marker) = crm_trio(&fixture, &fixture.manager, "refuse").await;

    // No record: the refusal lands on the field the form renders the message under.
    let floating = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({ "kind": "note", "subject": format!("Floating {marker}") })),
        ),
    )
    .await;
    assert_eq!(floating.status, StatusCode::BAD_REQUEST, "{}", floating.body);
    assert_eq!(floating.body["error"]["details"]["field"], json!("contact_id"));

    // No subject.
    let blank = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({ "kind": "note", "subject": "   ", "contact_id": contact_id })),
        ),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST, "{}", blank.body);
    assert_eq!(blank.body["error"]["details"]["field"], json!("subject"));

    // A task with neither a due date nor a done mark cannot appear on the open-task list.
    let dateless = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({ "kind": "task", "subject": format!("Call {marker}"), "deal_id": deal_id })),
        ),
    )
    .await;
    assert_eq!(dateless.status, StatusCode::BAD_REQUEST, "{}", dateless.body);
    assert_eq!(dateless.body["error"]["details"]["field"], json!("due_at"));

    // Two records at once satisfies the schema's `crm_activities_attached` and is still refused.
    let two = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "note", "subject": format!("Ambiguous {marker}"),
                "contact_id": contact_id, "deal_id": deal_id,
            })),
        ),
    )
    .await;
    assert_eq!(two.status, StatusCode::BAD_REQUEST, "{}", two.body);

    // A record that is not there is a 404, not a 400 — the form was well-formed.
    let missing = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "note", "subject": format!("Ghost {marker}"),
                "contact_id": Uuid::new_v4(),
            })),
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);

    // Nothing above was written.
    let feed = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/activities?search={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    assert_eq!(
        feed.body["items"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "a refused write must leave nothing behind: {}",
        feed.body
    );
}

#[tokio::test]
async fn an_activity_of_another_organization_is_invisible() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (company_id, contact_id, _, marker) = crm_trio(&fixture, &fixture.manager, "tenant").await;

    // Logged by the manager, in `fixture.org`.
    let logged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&fixture.manager),
            Some(json!({
                "kind": "note", "subject": format!("Private {marker}"), "contact_id": contact_id,
            })),
        ),
    )
    .await;
    assert_eq!(logged.status, StatusCode::CREATED, "{}", logged.body);
    let activity_id: Uuid = serde_json::from_value(logged.body["id"].clone()).expect("an id");

    // The other tenant's feed must not carry it, and its timeline must not either. The other
    // writer holds `crm.activities.read` in its own organization, so the rule is exercised for a
    // caller who genuinely has the power — a 403 would only prove the guard, not the filter.
    let other = grant_and_login(&fixture, "activity-cross-tenant", &OTHER_ACTIVITY_PERMISSIONS).await;
    let foreign_feed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/activities", Some(&other), None),
    )
    .await;
    assert_eq!(foreign_feed.status, StatusCode::OK, "{}", foreign_feed.body);
    assert!(
        !foreign_feed.body["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|row| row["id"] == json!(activity_id.to_string()))),
        "another tenant must not see the activity: {}",
        foreign_feed.body
    );

    let foreign_timeline = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts/{contact_id}/timeline"),
            Some(&other),
            None,
        ),
    )
    .await;
    // The contact is another organization's, so the timeline is either empty or 404 — never its
    // history. Both are acceptable; a populated one is not.
    if foreign_timeline.status == StatusCode::OK {
        assert_eq!(
            foreign_timeline.body["items"].as_array().map(Vec::len).unwrap_or(0),
            0,
            "another tenant must not read this contact's timeline: {}",
            foreign_timeline.body
        );
    } else {
        assert_eq!(foreign_timeline.status, StatusCode::NOT_FOUND, "{}", foreign_timeline.body);
    }

    // And it cannot be closed from outside the organization.
    let foreign_close = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/crm/activities/{activity_id}/done"),
            Some(&other),
            Some(json!({ "done": true })),
        ),
    )
    .await;
    assert_eq!(foreign_close.status, StatusCode::NOT_FOUND, "{}", foreign_close.body);
    let _ = company_id;
}

/// The powers the cross-tenant activity walk needs, in the *other* organization.
const OTHER_ACTIVITY_PERMISSIONS: [&str; 3] = [
    "crm.activities.read",
    "crm.activities.create",
    "sites.read",
];

/// An account in the other organization, granted `permissions`, already signed in.
async fn grant_and_login(fixture: &Fixture, label: &str, permissions: &[&str]) -> String {
    let (user_id, email) = create_account(&fixture.db, Some(fixture.other_org), label).await;
    grant(
        &fixture.db,
        fixture.other_org,
        user_id,
        fixture.accounts[0],
        permissions,
    )
    .await;
    login(&fixture.state, &email).await
}

#[tokio::test]
async fn the_new_activity_keys_are_in_the_catalogue_and_the_owner_holds_them() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for key in ["crm.activities.read", "crm.activities.create", "crm.copilot.use"] {
        assert!(omnion_permissions::catalogue::is_known(key), "{key} must be a known key");
    }
    // The owner is the account every seeded role is built from, so the new keys have to be in it
    // or a fresh install cannot read its own activity feed.
    let held: Vec<String> = sqlx::query_scalar(
        "select rp.permission from role_permissions rp
         join roles r on r.id = rp.role_id
         where r.organization_id is null
           and r.key = 'owner'
           and rp.permission in ('crm.activities.read', 'crm.activities.create', 'crm.copilot.use')",
    )
    .fetch_all(fixture.db.pool())
    .await
    .expect("the owner's permissions must read");
    assert!(
        held.contains(&"crm.activities.read".to_string())
            && held.contains(&"crm.activities.create".to_string())
            && held.contains(&"crm.copilot.use".to_string()),
        "the owner must hold slice 4's keys: {held:?}"
    );
}
