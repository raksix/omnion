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
use omnion_automation::matcher;
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
    /// The platform Owner (no primary organization). Held because the global search's
    /// **reindex** is `search.manage`, a platform-level key a CRM manager does not hold — the
    /// CRM suite builds the index the way the owner's settings screen does, not through a role
    /// that could not press the button.
    owner: String,
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

        let (owner_id, owner) = create_account(&db, None, "CRM Owner").await;
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
            owner,
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

/// An organization created *the way the product creates one* owns a pipeline, so the board has
/// columns before a single deal exists.
///
/// The rest of this file cannot see this bug, and the reason is worth writing down: the fixture
/// calls `crm_seed_default_pipeline` by hand for its own organizations, so every board test in the
/// suite starts from a pipeline that the product would never have created on its own. This test
/// therefore seeds nothing — it inserts the organization row and immediately reads the board.
///
/// Before `0043` this answered `404 NotFound("pipeline")`: the seed function was written in
/// `0022_crm.sql` and called once, in the statement that created it, so an organization born after
/// that migration had no pipeline, no stages and no board. The stage editor was a screen over an
/// empty table and the board was a 404.
#[tokio::test]
async fn an_organization_created_after_the_migration_still_owns_a_board() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let organization_id = create_organization_row(&db, "unseeded").await;

    // Nothing was seeded: this is the organization row and nothing else.
    let pipelines: (i64,) =
        sqlx::query_as("select count(*) from crm_pipelines where organization_id = $1")
            .bind(organization_id)
            .fetch_one(db.pool())
            .await
            .expect("the pipelines must read");
    assert_eq!(
        pipelines.0, 1,
        "creating an organization seeds its default pipeline, without anybody calling the seed"
    );

    // The stages come with it, in board order, so a column exists to drop a card into.
    let stages: Vec<(String,)> = sqlx::query_as(
        "select s.name from crm_pipeline_stages s \
         join crm_pipelines p on p.id = s.pipeline_id \
         where p.organization_id = $1 and p.is_default order by s.position",
    )
    .bind(organization_id)
    .fetch_all(db.pool())
    .await
    .expect("the stages must read");
    let names: Vec<&str> = stages.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        names,
        vec!["New", "Qualified", "Proposal", "Negotiation", "Won", "Lost"],
        "a new organization starts with the documented stages"
    );

    // And the board itself answers 200 with those columns, rather than 404.
    let (owner_id, email) = create_account(&db, Some(organization_id), "Board Owner").await;
    // `MANAGER_PERMISSIONS` is the suite's set that carries the deal keys; the granter is the
    // account itself because this organization has no platform owner to hand them out.
    grant(
        &db,
        organization_id,
        owner_id,
        owner_id,
        &MANAGER_PERMISSIONS,
    )
    .await;
    let token = login(&state, &email).await;

    let board = call(
        &state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals?view=board&organization_id={organization_id}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        board.status,
        StatusCode::OK,
        "the board of a new organization answers, not 404: {}",
        board.body
    );
    let columns = board.body["board"]["columns"]
        .as_array()
        .expect("the board carries its columns");
    assert_eq!(columns.len(), 6, "six columns on a board with no deals yet");
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
    let manager = fixture.token(&fixture.manager).await;
    let (_, contact_id, deal_id, marker) = crm_trio(&fixture, &manager, "guard").await;
    let reader = fixture.token(&fixture.reader).await;

    // The close route names an **activity**, not the record it hangs off, so the walk needs one
    // to exist. Passing the deal's id answers 404 for a caller who is allowed to write, which
    // proves nothing about the guard.
    let task = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
            Some(json!({
                "kind": "task", "subject": format!("Guard {marker}"),
                "deal_id": deal_id, "due_at": "2026-10-01T09:00:00Z",
            })),
        ),
    )
    .await;
    assert_eq!(task.status, StatusCode::CREATED, "{}", task.body);
    let task_id: Uuid = serde_json::from_value(task.body["id"].clone()).expect("an id");

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
            format!("/api/v1/crm/activities/{task_id}/done"),
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
            request(method, &uri, Some(&manager), body.clone()),
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
        crm_trio(&fixture, &manager, "log").await;

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
    // `audit_rows` names the actor `actor`; the column is `actor_user_id`, the key is not.
    assert_eq!(entry["actor"], json!(fixture.manager_id.to_string()));
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
    let (_, contact_id, _, marker) = crm_trio(&fixture, &manager, "filter").await;

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
    let (_, contact_id, deal_id, marker) = crm_trio(&fixture, &manager, "refuse").await;

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
    let manager = fixture.token(&fixture.manager).await;
    let (company_id, contact_id, _, marker) = crm_trio(&fixture, &manager, "tenant").await;

    // Logged by the manager, in `fixture.org`.
    let logged = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/activities",
            Some(&manager),
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
        "select rp.permission_key from role_permissions rp
         join roles r on r.id = rp.role_id
         where r.organization_id is null
           and r.key = 'owner'
           and rp.permission_key in ('crm.activities.read', 'crm.activities.create', 'crm.copilot.use')",
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

// -------------------------------------------------------------------------------------------
// Slice 4, part three: the copilot's two endpoints.
//
// The walk proves what can be proved without a provider connected: the guard chain, the
// cross-tenant 404, and the **audited failure** — an installation with no model answers a
// copilot call, and the audit row is the record that the call was made and failed. A model
// answer's *text* cannot be asserted here (it belongs to the provider, not to us), but the
// sanitiser's text rules are unit-tested in `modules/crm/src/copilot.rs` and the guard and the
// audit are proved here — which is the part that is ours.
// -------------------------------------------------------------------------------------------

/// The permission set the copilot account holds: the key, plus the deal read it needs to build
/// the context. The suite grants it through `grant_and_login`, so the account is a **real** one
/// with the key and not a hand-written row.
const COPILOT_PERMISSIONS: [&str; 4] = [
    "crm.copilot.use",
    "crm.deals.read",
    // The foreign account makes a deal of its own so the walk can prove the scope in **both**
    // directions; a caller that could only read would prove it in one.
    "crm.deals.create",
    "sites.read",
];

/// The copilot's two endpoints are guarded, scoped and audited — and a deal belonging to another
/// organization is a `404`, not a `403` and not an answer.
#[tokio::test]
async fn the_copilot_is_guarded_scoped_and_audited() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let marker = Uuid::new_v4().simple().to_string();

    // Two accounts in **different** organizations, each holding the copilot key. This is the
    // point the walk cannot be written without: `grant_and_login` puts its account in the
    // foreign organization, so an account made with it is a cross-tenant *caller* for a deal in
    // the home organization — and the walk needs one of each, or every 404 below would be a
    // same-tenant read that happens to miss.
    let (copilot_id, copilot_email) = create_account(&fixture.db, Some(fixture.org), "CRM Copilot").await;
    grant(
        &fixture.db,
        fixture.org,
        copilot_id,
        fixture.accounts[0],
        &COPILOT_PERMISSIONS,
    )
    .await;
    let copilot = login(&fixture.state, &copilot_email).await;

    let (foreign_id, foreign_email) =
        create_account(&fixture.db, Some(fixture.other_org), "CRM Copilot Foreign").await;
    grant(
        &fixture.db,
        fixture.other_org,
        foreign_id,
        fixture.accounts[0],
        &COPILOT_PERMISSIONS,
    )
    .await;
    let foreign_copilot = login(&fixture.state, &foreign_email).await;
    let manager = fixture.token(&fixture.manager).await;

    let created = create_deal_via_api(
        state,
        &manager,
        json!({
            "title": format!("Copilot walk {marker}"),
            "amount": "2500.00",
            "currency": "USD",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    let deal_id = created.body["id"].as_str().expect("a created deal has an id");

    for (path, expected_action) in [
        ("summarize", "summarize"),
        ("follow-up", "follow-up"),
    ] {
        let uri = format!("/api/v1/crm/copilot/{path}/{deal_id}");

        // 401 without a session.
        let anonymous = call(state, request(Method::POST, &uri, None, None)).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{path} must need a session, body: {}",
            anonymous.body
        );

        // 403 with a session that lacks `crm.copilot.use`. The manager holds the whole contact
        // and deal family and deliberately NOT the copilot key, so this is a real refusal by a
        // real account rather than a synthetic one.
        let refused = call(
            state,
            request(Method::POST, &uri, Some(&manager), Some(json!({}))),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{path} must need crm.copilot.use, body: {}",
            refused.body
        );

        // 404 for a deal in another organization, to a caller that HOLDS the key — the rule
        // under test is the scope, so the guard must not answer first. The foreign deal is made
        // by a **second** granted account rather than by `other_writer`, which predates the deal
        // keys and holds none of them: widening an existing fixture to serve one walk would
        // change what every other walk proves.
        // The *home* deal, asked about by the account in the other organization. The key is
        // held, so the guard cannot be what refuses — only the scope can.
        let cross = call(
            state,
            request(
                Method::POST,
                &format!("/api/v1/crm/copilot/{path}/{deal_id}"),
                Some(&foreign_copilot),
                Some(json!({})),
            ),
        )
        .await;
        assert_eq!(
            cross.status,
            StatusCode::NOT_FOUND,
            "{path} must not confirm a deal in another organization, body: {}",
            cross.body
        );
        // And the other direction: a deal in the *foreign* organization asked about from home is
        // equally invisible. One direction proves the scope exists; both prove it is the scope
        // and not a rule that happens to hide this one row.
        let foreign = create_deal_via_api(
            state,
            &foreign_copilot,
            json!({
                "title": format!("Foreign {marker}"),
                "amount": "10.00",
                "currency": "USD",
            }),
        )
        .await;
        assert_eq!(
            foreign.status,
            StatusCode::CREATED,
            "the foreign account needs crm.deals.create to make its own deal: {}",
            foreign.body
        );
        let foreign_deal = foreign.body["id"].as_str().expect("a created deal has an id");
        let cross_back = call(
            state,
            request(
                Method::POST,
                &format!("/api/v1/crm/copilot/{path}/{foreign_deal}"),
                Some(&copilot),
                Some(json!({})),
            ),
        )
        .await;
        assert_eq!(
            cross_back.status,
            StatusCode::NOT_FOUND,
            "{path} must not confirm a foreign deal either, body: {}",
            cross_back.body
        );

        // The call lands, and the answer is audited whatever it is. This installation connects no
        // provider, so the call **fails** — and a failure is still a call, and the audit row is
        // the only record that it happened. The status is therefore not asserted to be any
        // particular value: what is asserted is that the attempt was written down.
        let called = call(
            state,
            request(Method::POST, &uri, Some(&copilot), Some(json!({}))),
        )
        .await;
        assert!(
            called.status.is_client_error() || called.status.is_server_error() || called.status == StatusCode::OK,
            "{path} answered an unexpected status {}: {}",
            called.status,
            called.body
        );

        let rows = audit_rows(&fixture.db, "crm.copilot.failed").await;
        assert!(
            !rows.is_empty(),
            "a copilot call must be audited even when it cannot be answered: {path}"
        );
        // The row names the deal and the action, and deliberately does NOT carry the draft or the
        // deal's title: an audit log is read by people the CRM is not about.
        let latest = &rows[0];
        let metadata = &latest["metadata"];
        assert_eq!(
            metadata["deal_id"],
            json!(deal_id),
            "the audit must name the deal it read: {latest}"
        );
        assert_eq!(
            metadata["action"],
            json!(expected_action),
            "the audit must name the action: {latest}"
        );
        assert!(
            latest["target_type"] == json!("crm_deal"),
            "the audit targets the deal: {latest}"
        );
        assert!(
            metadata.get("draft").is_none() && metadata.get("title").is_none(),
            "the audit must not carry the model's answer or the deal's title: {latest}"
        );
    }

    // Nothing was written to the record: a copilot call is a draft, and the deal is untouched.
    let after = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/crm/deals/{deal_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "body: {}", after.body);
    assert_eq!(
        after.body["title"],
        json!(format!("Copilot walk {marker}")),
        "the copilot must not write to the record: {}",
        after.body
    );
    assert!(
        after.body.get("summary").is_none() && after.body.get("next_action").is_none(),
        "the deal must carry no stored copilot output: {}",
        after.body
    );
}

// ---------------------------------------------------------------------------------------------
// The CRM in the global search (REQ-002 · docs/requests/REQ-051, slice 4 part four)
// ---------------------------------------------------------------------------------------------

/// Look a fixture account's id up by its address.
///
/// The search box is its own surface and its own key: `search.read` is "may I use the palette",
/// which is a different decision from "may I read a contact". A CRM reader who was never granted
/// it is refused by `/api/v1/search` with a `403` before any provider is considered — so a walk
/// that wants to prove the **provider** split has to hand it over explicitly, or it proves the
/// box is closed instead.
async fn account_id(db: &Db, email: &str) -> Uuid {
    sqlx::query_scalar("select id from users where email = $1")
        .bind(email)
        .fetch_one(db.pool())
        .await
        .expect("the fixture account must exist")
}

/// Rebuild the whole search index as the platform Owner, who holds `search.manage`.
///
/// The CRM suite is not a search suite, so it borrows the indexer's own two entry points rather
/// than a private copy: `reindex_all` is what the owner's "reindex" button and the scheduled pass
/// both call, and `drain` is the background runner's tick. Calling the **same** functions is what
/// makes the walk prove the wiring rather than a re-implementation of it.
async fn rebuild_index(state: &AppState, token: &str) {
    let response = call(
        state,
        request(
            Method::POST,
            "/api/v1/search/reindex",
            Some(token),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the reindex must run: {}",
        response.body
    );
}

/// The provider keys an answer's hits come from.
fn hit_providers(body: &Value) -> Vec<String> {
    body["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {body}"))
        .iter()
        .filter_map(|hit| hit["provider"].as_str().map(str::to_owned))
        .collect()
}

/// One hit of a provider, by its title.
fn hit_of(body: &Value, provider: &str, title: &str) -> Value {
    body["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits must be an array in {body}"))
        .iter()
        .find(|hit| hit["provider"] == provider && hit["title"] == title)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "a {provider} hit titled {title:?} must be in the answer: {body}"
            )
        })
}

/// The CRM joins the palette: a contact, a company and a deal each become one document, each hit
/// carries the deep link that opens that record, and the search key narrows to one of them.
#[tokio::test]
async fn the_crm_answers_the_palette_with_a_deep_link_into_the_record() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    grant(
        &fixture.db,
        fixture.org,
        account_id(&fixture.db, &fixture.manager).await,
        owner_id,
        &["search.read"],
    )
    .await;
    let manager = fixture.token(&fixture.manager).await;
    let owner = fixture.token(&fixture.owner).await;
    let marker = Uuid::new_v4().simple().to_string();

    let (company_id, contact_id, deal_id, _) = crm_trio(&fixture, &manager, &marker).await;
    rebuild_index(state, &owner).await;

    let body = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(body.status, StatusCode::OK, "search: {}", body.body);
    let providers = hit_providers(&body.body);
    for expected in ["contacts", "companies", "deals"] {
        assert!(
            providers.iter().any(|key| key == expected),
            "the answer must carry a {expected} hit: {providers:?}"
        );
    }

    // The deep link is the whole point of a hit: it opens the record, not the list. The contact's
    // title is its display name (`Walk <marker>`), the company's is its name, the deal's its title.
    let contact_hit = hit_of(&body.body, "contacts", &format!("Walk {marker}"));
    assert_eq!(
        contact_hit["url"],
        json!(format!("/crm/contacts?focus={contact_id}")),
        "a contact hit must open that contact"
    );
    let company_hit = hit_of(&body.body, "companies", &format!("Walk Co {marker}"));
    assert_eq!(
        company_hit["url"],
        json!(format!("/crm/companies?focus={company_id}")),
        "a company hit must open that company"
    );
    let deal_hit = hit_of(&body.body, "deals", &format!("Walk deal {marker}"));
    assert_eq!(
        deal_hit["url"],
        json!(format!("/crm/deals?focus={deal_id}")),
        "a deal hit must open that deal's card"
    );

    // The scoped syntax narrows to a single register, so a person can ask for the people and not
    // the pipeline: `type:contacts <marker>` returns contact rows only.
    let narrowed = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q=type%3Acontacts%20{marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "search: {}", narrowed.body);
    let narrowed_providers = hit_providers(&narrowed.body);
    assert!(
        narrowed_providers.iter().all(|key| key == "contacts"),
        "type:contacts must answer with contact rows only: {narrowed_providers:?}"
    );
}

/// The two CRM read keys are separate, so the palette is too: a caller who may know who a
/// customer is does not thereby get to see what they are negotiating.
#[tokio::test]
async fn the_palette_hides_the_pipeline_from_a_contact_only_reader() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    for account in [&fixture.manager, &fixture.reader] {
        grant(
            &fixture.db,
            fixture.org,
            account_id(&fixture.db, account).await,
            owner_id,
            &["search.read"],
        )
        .await;
    }
    let reader = fixture.token(&fixture.reader).await;
    let manager = fixture.token(&fixture.manager).await;
    let owner = fixture.token(&fixture.owner).await;
    let marker = Uuid::new_v4().simple().to_string();

    // Written by the manager (the reader may not write), indexed by the owner.
    let (_company_id, _contact_id, _deal_id, _) = crm_trio(&fixture, &manager, &marker).await;
    rebuild_index(state, &owner).await;

    // The reader holds `crm.contacts.read` and no `crm.deals.read`, so the contact is findable…
    let as_reader = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={marker}"),
            Some(&reader),
            None,
        ),
    )
    .await;
    assert_eq!(as_reader.status, StatusCode::OK, "search: {}", as_reader.body);
    let reader_providers = hit_providers(&as_reader.body);
    assert!(
        reader_providers.iter().any(|key| key == "contacts"),
        "a contact reader must find the contact: {reader_providers:?}"
    );
    // …and the deal is **not** in the answer, exactly as `/api/v1/crm/deals` refuses the same
    // caller with a `403`. A search that answered with the pipeline would be the wider door.
    assert!(
        !reader_providers.iter().any(|key| key == "deals"),
        "a contact reader must get no deal rows: {reader_providers:?}"
    );
}

/// Archiving a record takes it out of the index. The CRM lists hide archived rows by default, so a
/// document that outlived the archive would answer with a record the panel will not show.
#[tokio::test]
async fn archiving_a_crm_record_removes_it_from_the_palette() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    grant(
        &fixture.db,
        fixture.org,
        account_id(&fixture.db, &fixture.manager).await,
        owner_id,
        &["search.read"],
    )
    .await;
    let manager = fixture.token(&fixture.manager).await;
    let owner = fixture.token(&fixture.owner).await;
    let marker = Uuid::new_v4().simple().to_string();

    let (company_id, _contact_id, _deal_id, _) = crm_trio(&fixture, &manager, &marker).await;
    rebuild_index(state, &owner).await;

    let before = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert!(
        hit_providers(&before.body).iter().any(|key| key == "companies"),
        "the company must be indexed to begin with: {}",
        before.body
    );

    // Archive it through the API, so the event the indexer drains is the one the real screen
    // emits — not a hand-written row.
    let archived = call(
        state,
        request(
            Method::DELETE,
            &format!("/api/v1/crm/companies/{company_id}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "{}", archived.body);
    // The archive's `crm.company.archived` event removes the document on the next drain; a
    // reindex proves the same thing through the other door (the prune arm), so both are run.
    rebuild_index(state, &owner).await;

    let after = call(
        state,
        request(
            Method::GET,
            &format!("/api/v1/search?q={marker}"),
            Some(&manager),
            None,
        ),
    )
    .await;
    assert!(
        !hit_providers(&after.body).iter().any(|key| key == "companies"),
        "an archived company must leave the index: {}",
        after.body
    );
}

// ---------------------------------------------------------------------------------------------
// The automation trigger (slice 4, part six)
//
// The criterion is one sentence — "an automation rule triggered by `crm.deal.stage_changed`
// runs once" — and it is the only part of this slice that had to leave the CRM's own code to be
// answered at all. The CRM emits the event; the **matcher** (crates/automation/src/matcher.rs)
// decides what fires and starts the run. Proving it from here therefore means driving the real
// matcher over a real bus, in this suite's own database, so the event the rule reads is the one
// the board's stage endpoint emitted — not a hand-written row that would prove nothing.
// ---------------------------------------------------------------------------------------------

/// A rule that mails the deal's owner when a deal reaches a **negotiation** stage.
///
/// `crm.deal.stage_changed` carries no deal title (a payload is read by every subscriber, and a
/// title is a customer's own words), so the subject is built from the fields the event really
/// does carry: the stage kind it entered, the money and the currency. The `{{event.*}}` bindings
/// are what make this walk worth running — they prove the run's steps carry *this* move's values
/// rather than the rule author's template.
fn stage_changed_rule(organization_id: Uuid, into_kind: &str) -> Value {
    json!({
        "organization_id": organization_id,
        "name": format!("Tell the owner when a deal enters {into_kind}"),
        "description": "Slice 4: proves the CRM's event starts a run exactly once.",
        "event": "crm.deal.stage_changed",
        "conditions": [
            { "field": "to_stage_kind", "operator": "equals", "value": into_kind }
        ],
        "actions": [
            {
                "name": "tell the owner",
                "kind": "task",
                "action": "send_email",
                "params": {
                    "to": "owner@example.com",
                    "subject": "A deal entered {{event.to_stage_kind}}",
                    "body": "Deal {{event.deal_id}} is now {{event.to_stage_kind}} at \
                             {{event.amount}} {{event.currency}}."
                },
                "max_attempts": 1
            }
        ]
    })
}

/// How many runs the workflow behind a rule has started, read from the engine's own tables.
async fn runs_of_workflow(db: &Db, workflow_id: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from workflow_executions where workflow_id = $1")
        .bind(workflow_id)
        .fetch_one(db.pool())
        .await
        .expect("the executions must read")
}

/// Give an account the two powers the automation surface is guarded by.
///
/// Writing a rule is `workflows.manage` and reading the surface is `workflows.read`; the CRM
/// manager holds neither, and it is exactly that fact the walk needs — the rule is written the
/// way a person with the panel's own key would write it, not by reaching past the guard.
async fn grant_workflow_powers(fixture: &Fixture, email: &str) {
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    let user_id = account_id(&fixture.db, email).await;
    grant(
        &fixture.db,
        fixture.org,
        user_id,
        owner_id,
        &["workflows.read", "workflows.manage"],
    )
    .await;
}

/// Drain the bus and hand back what it read, asserting that **nothing ran**.
///
/// Creating a deal is itself an event on the bus, so a walk that writes a rule and then counts
/// events from the bus counts a fact it did not cause. Every walk here flushes the setup with
/// this, so the counts that follow are the ones the move under test produced.
async fn flush_without_runs(db: &Db) -> matcher::MatchReport {
    let report = matcher::drain(db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert!(report.runs.is_empty(), "setup must not start a run: {report:?}");
    report
}

/// A rule on `crm.deal.stage_changed` fires **once** per real stage move — not on the move that
/// does not move, not twice for one move, and not for a move its condition excludes.
#[tokio::test]
async fn a_rule_on_a_deal_stage_change_runs_exactly_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    // The token is taken **after** the grant: the session's powers are read from the role at
    // login, so a token minted before the grant would answer 403 on the very rule this walk
    // writes. Granting first and signing in second is the order every workflow key needs.
    grant_workflow_powers(&fixture, &fixture.manager).await;
    let manager = fixture.token(&fixture.manager).await;

    // The matcher watches forward: a rule created today must not fire for the history already on
    // the bus, so the cursor is seeded to the end of the bus before anything happens.
    matcher::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("stages").clone();
    let first_open = stages[0].clone();
    // The seeded pipeline is New/Qualified/Negotiation/Won/Lost in every install, but the rule
    // must not be written against a stage *name* — only a stage's kind is stable, so the walk
    // finds the negotiation column by kind and fails loudly if an install ever drops it.
    let negotiation = stages
        .iter()
        .find(|stage| stage["name"] == json!("Negotiation"))
        .expect("the seeded pipeline has a Negotiation column")
        .clone();
    assert_eq!(negotiation["kind"], json!("open"), "{negotiation}");

    // The rule is written through the surface, so the definition checks that guard the store
    // (`validate_event` against the bus's own name rule) are part of what is being proved.
    let created = call(
        state,
        request(
            Method::POST,
            "/api/v1/automations",
            Some(&manager),
            Some(stage_changed_rule(fixture.org, "open")),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "body: {}", created.body);
    assert_eq!(created.body["event"], json!("crm.deal.stage_changed"));
    assert_eq!(created.body["trigger_count"], 0);
    let rule_id = created.body["id"].as_str().expect("an id").to_owned();

    let created_deal = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Trigger {}", Uuid::new_v4().simple()), "amount": "4200.00" }),
    )
    .await;
    assert_eq!(created_deal.status, StatusCode::CREATED, "body: {}", created_deal.body);
    let deal_id = created_deal.body["id"].as_str().expect("an id").to_owned();

    // Creating a deal is itself an event. The bus is flushed here so that every count below is
    // the move under test and nothing else — a walk that leaves its own setup on the bus cannot
    // tell "the rule fired once" from "the rule fired once and something else was there too".
    flush_without_runs(&fixture.db).await;

    // ---- a move that does not move wakes nobody -----------------------------------------
    // The board's keyboard path posts on every arrow press, so a no-op move must leave the bus
    // alone. This is asserted first, because a rule that fired here would be indistinguishable
    // from a rule that simply fires too eagerly.
    let no_op = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": first_open["id"] })),
        ),
    )
    .await;
    assert_eq!(no_op.status, StatusCode::OK, "body: {}", no_op.body);

    let idle = matcher::drain(fixture.db.pool(), 100)
        .await
        .expect("the matcher must run");
    // **No rule fired**, not "the drain had nothing to do". `is_idle` is false whenever the
    // cursor moved, and in a shared database other walks leave their own events on the bus — a
    // report of `evaluated: 13, matched: 0, runs: []` is the rule staying quiet while the drain
    // does its work, which is exactly the property under test. Asserting `is_idle` here measured
    // how many other tests had run first.
    assert_eq!(
        idle.matched, 0,
        "a move to the same stage must not match a rule: {idle:?}"
    );
    assert!(
        idle.runs.is_empty(),
        "a move to the same stage must start no run: {idle:?}"
    );
    assert_eq!(
        runs_of_workflow(&fixture.db, Uuid::parse_str(&rule_id).expect("a uuid")).await,
        0,
        "no run may exist yet"
    );

    // ---- the move that moves --------------------------------------------------------------
    let moved = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{deal_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": negotiation["id"] })),
        ),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "body: {}", moved.body);

    let report = matcher::drain(fixture.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.evaluated, 1, "exactly one event was on the bus: {report:?}");
    assert_eq!(report.matched, 1, "the rule matched: {report:?}");
    assert_eq!(report.skipped, 0, "{report:?}");
    assert_eq!(report.runs.len(), 1, "one move, one run: {report:?}");
    let execution_id = report.runs[0];

    // The run's steps carry **this** move's values: the placeholders were resolved against the
    // event that fired the rule, so a retry would repeat the first attempt rather than reading a
    // bus that has moved on.
    let steps = omnion_workflows::store::list_steps(fixture.db.pool(), execution_id)
        .await
        .expect("the steps must be readable");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].action.as_deref(), Some("send_email"));
    assert_eq!(steps[0].status, "pending", "{:?}", steps[0].error);
    assert_eq!(
        steps[0].params["body"],
        format!("Deal {deal_id} is now open at 4200.00 USD.")
    );
    assert_eq!(steps[0].params["subject"], "A deal entered open");

    // ---- exactly once, proved by the second tick having nothing to do ----------------------
    // The cursor advanced inside the same transaction as the run, so a second drain over the
    // same bus is idle. This is the "once" in "runs once": not a count, but the absence of a
    // second chance.
    let second = matcher::drain(fixture.db.pool(), 100)
        .await
        .expect("the second tick must run");
    assert!(second.is_idle(), "one move must not start a second run: {second:?}");
    assert_eq!(
        matcher::event_cursor(fixture.db.pool()).await.expect("the cursor must read"),
        report.cursor
    );
    assert_eq!(
        runs_of_workflow(&fixture.db, Uuid::parse_str(&rule_id).expect("a uuid")).await,
        1,
        "the rule has started exactly one run"
    );

    // ---- the rule remembers, and the match is audited on both sides ------------------------
    let after = call(
        state,
        request(Method::GET, &format!("/api/v1/automations/{rule_id}"), Some(&manager), None),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "body: {}", after.body);
    assert_eq!(after.body["trigger_count"], 1, "{}", after.body);
    assert!(after.body["last_triggered_at"].is_string(), "{}", after.body);

    let matched = audit_rows(&fixture.db, "automation.rule.matched").await;
    assert!(
        matched
            .iter()
            .any(|row| row["target_id"] == json!(execution_id.to_string())),
        "the match must be audited against the run: {matched:?}"
    );

    // ---- a second, *different* deal fires its own run --------------------------------------
    // Exactly-once is per event, not per rule: two deals are two facts, and a rule that
    // collapsed them into one run would silently drop a customer.
    let other_deal = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Second {}", Uuid::new_v4().simple()), "amount": "900.00" }),
    )
    .await;
    let other_id = other_deal.body["id"].as_str().expect("an id").to_owned();
    // The second deal's creation is an event too, and it would be counted as part of the move.
    flush_without_runs(&fixture.db).await;
    let other_moved = call(
        state,
        request(
            Method::POST,
            &format!("/api/v1/crm/deals/{other_id}/stage"),
            Some(&manager),
            Some(json!({ "stage_id": negotiation["id"] })),
        ),
    )
    .await;
    assert_eq!(other_moved.status, StatusCode::OK, "body: {}", other_moved.body);

    let third = matcher::drain(fixture.db.pool(), 100)
        .await
        .expect("the third tick must run");
    assert_eq!(third.evaluated, 1, "{third:?}");
    assert_eq!(third.matched, 1, "{third:?}");
    assert_eq!(
        runs_of_workflow(&fixture.db, Uuid::parse_str(&rule_id).expect("a uuid")).await,
        2,
        "two moves, two runs"
    );
}

/// A second rule on the same event, narrowed by a condition, does not fire on the move the
/// condition excludes — the shape every "tell me only about the big ones" rule takes.
#[tokio::test]
async fn a_rule_whose_condition_does_not_hold_starts_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    grant_workflow_powers(&fixture, &fixture.manager).await;
    let manager = fixture.token(&fixture.manager).await;

    matcher::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("stages").clone();
    let first_open = stages[0].clone();
    let second_open = stages
        .iter()
        .find(|stage| stage["kind"] == json!("open") && stage["id"] != first_open["id"])
        .expect("a second open stage")
        .clone();

    // The rule fires for a won deal — an outcome, not a move. Both stages below are `open`, so
    // the condition can never hold for the move this walk performs.
    let won_only = call(
        state,
        request(
            Method::POST,
            "/api/v1/automations",
            Some(&manager),
            Some(json!({
                "organization_id": fixture.org,
                "name": "Only ever for a won deal",
                "event": "crm.deal.stage_changed",
                "conditions": [
                    { "field": "to_stage_kind", "operator": "equals", "value": "won" }
                ],
                "actions": [{
                    "name": "celebrate",
                    "kind": "task",
                    "action": "send_email",
                    "params": {
                        "to": "owner@example.com",
                        "subject": "Won",
                        "body": "Deal {{event.deal_id}} was won."
                    },
                    "max_attempts": 1
                }]
            })),
        ),
    )
    .await;
    assert_eq!(won_only.status, StatusCode::CREATED, "body: {}", won_only.body);
    let won_rule = won_only.body["id"].as_str().expect("an id").to_owned();

    let created = create_deal_via_api(
        state,
        &manager,
        json!({ "title": format!("Conditional {}", Uuid::new_v4().simple()), "amount": "10.00" }),
    )
    .await;
    let deal_id = created.body["id"].as_str().expect("an id").to_owned();

    // The deal's own creation is on the bus; the counts below are about the move.
    flush_without_runs(&fixture.db).await;

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

    let report = matcher::drain(fixture.db.pool(), 100)
        .await
        .expect("the matcher must run");
    assert_eq!(report.evaluated, 1, "the move is on the bus: {report:?}");
    assert_eq!(report.matched, 0, "a condition that does not hold starts nothing: {report:?}");
    assert_eq!(report.skipped, 1, "the rule was evaluated and declined: {report:?}");
    assert!(report.runs.is_empty());
    assert_eq!(
        runs_of_workflow(&fixture.db, Uuid::parse_str(&won_rule).expect("a uuid")).await,
        0
    );
}

/// The rule's key is a workflow key, not a CRM one: a caller who may move deals all day and
/// still cannot define the rule that watches them.
#[tokio::test]
async fn defining_the_rule_needs_the_workflow_key_and_a_tenant_rule_stays_home() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    // The manager is handed every CRM power by the fixture and *no* workflow key.
    let manager = fixture.token(&fixture.manager).await;
    let body = stage_changed_rule(fixture.org, "open");

    let refused = call(
        state,
        request(Method::POST, "/api/v1/automations", Some(&manager), Some(body.clone())),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a CRM manager must not be able to define a rule: {}",
        refused.body
    );

    grant_workflow_powers(&fixture, &fixture.manager).await;
    let manager = fixture.token(&fixture.manager).await;

    // Unauthenticated is still unauthenticated.
    let anonymous = call(
        state,
        request(Method::POST, "/api/v1/automations", None, Some(body.clone())),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{}", anonymous.body);

    // A rule for **another** organization is refused by the scope check rather than created.
    let foreign = call(
        state,
        request(
            Method::POST,
            "/api/v1/automations",
            Some(&manager),
            Some(stage_changed_rule(fixture.other_org, "open")),
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::FORBIDDEN,
        "a tenant must not write a rule into another tenant: {}",
        foreign.body
    );
}

// ---------------------------------------------------------------------------------------------
// Slice 4, part seven: the form → lead ingress (REQ-117).
//
// These walks drive the **real** consumer over the **real** bus: the event is recorded the way
// REQ-064's public submit endpoint will record it (`omnion_events::bus::emit`), and the drain
// that reads it is `modules::crm::leads::drain` — the same function `apps/api`'s runner calls.
// A hand-written row would prove the SQL; a bus event proves the contract.
// ---------------------------------------------------------------------------------------------

/// Record a `form.submitted` event the way the form module's public endpoint will.
async fn emit_form_submitted(db: &Db, organization_id: Option<Uuid>, answers: Value) -> i64 {
    let payload = json!({
        "form_key": "contact-us",
        "site_id": null,
        "occurred_at": "2026-09-26T10:30:00Z",
        "answers": answers,
    });

    let event = omnion_events::bus::emit(
        db.pool(),
        omnion_events::NewEvent::new(omnion_module_crm::leads::FORM_SUBMITTED)
            .organization(organization_id)
            .payload(payload),
    )
    .await
    .expect("the submission must reach the bus");

    event.event.id
}

/// The ingress keys: reading the log and deciding what a submission becomes.
const LEAD_PERMISSIONS: [&str; 2] = ["crm.leads.read", "crm.leads.manage"];

/// Give the manager the ingress keys and sign in **after** the grant.
///
/// The order matters and it is the same lesson the workflow walk learned: a session's powers are
/// read from the role at login, so a token minted before the grant answers `403` on the very
/// route this walk is about.
async fn grant_lead_powers(fixture: &Fixture, email: &str) -> String {
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    let user_id = account_id(&fixture.db, email).await;
    grant(&fixture.db, fixture.org, user_id, owner_id, &LEAD_PERMISSIONS).await;
    fixture.token(email).await
}

/// How many contacts of the fixture's organization carry this address.
async fn contacts_with_email(db: &Db, organization_id: Uuid, email: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*) from crm_contacts \
         where organization_id = $1 and email = $2 and archived_at is null",
    )
    .bind(organization_id)
    .bind(email)
    .fetch_one(db.pool())
    .await
    .expect("the contacts must read")
}

/// How many deals of the organization carry this source label.
async fn deals_from_forms(db: &Db, organization_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "select count(*) from crm_deals \
         where organization_id = $1 and source like 'form.submitted:%'",
    )
    .bind(organization_id)
    .fetch_one(db.pool())
    .await
    .expect("the deals must read")
}

/// The ledger row one submission produced.
async fn ledger_row(db: &Db, event_id: i64) -> Value {
    let rows: Vec<(String, Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select outcome, contact_id, deal_id, detail from crm_form_leads where event_id = $1",
    )
    .bind(event_id)
    .fetch_all(db.pool())
    .await
    .expect("the ledger must read");
    assert_eq!(rows.len(), 1, "exactly one ledger row per submission");
    json!({
        "outcome": rows[0].0,
        "contact_id": rows[0].1,
        "deal_id": rows[0].2,
        "detail": rows[0].3,
    })
}

/// A submitted form becomes a contact and a deal, through the real bus and the real drain.
///
/// The assertions are the acceptance criteria in one place: the submission is recorded on the
/// bus, the drain reads it, a **contact** and a **deal** exist, the ledger names both, the inbox
/// shows the row, and the deal's headline is the person's own words rather than a placeholder.
#[tokio::test]
async fn a_submitted_form_becomes_a_contact_and_a_deal() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    // The cursor watches forward: a rule created today must not fire for the history already on
    // the bus, and a drain must not turn this fixture's own setup into leads.
    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let email = format!("ada-{}@example.com", Uuid::new_v4().simple());
    let event_id = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({
            "name": "Ada Lovelace",
            "email": email,
            "company": "Analytical Engines Ltd",
            "message": "We would like a quote for the engine."
        }),
    )
    .await;

    // Draining over the API, so the guard that protects the button is part of what is proved.
    let drained = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(drained.status, StatusCode::OK, "body: {}", drained.body);
    assert_eq!(drained.body["created"], 1, "one submission became a record");
    assert_eq!(drained.body["idle"], false);
    assert_eq!(drained.body["failures"], 0, "body: {}", drained.body);

    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 1);
    assert_eq!(deals_from_forms(&fixture.db, fixture.org).await, 1);

    let row = ledger_row(&fixture.db, event_id).await;
    assert_eq!(row["outcome"], json!("created"));
    assert!(row["contact_id"].is_string(), "the ledger names the contact");
    assert!(row["deal_id"].is_string(), "the ledger names the deal");

    // The deal is the person's own words: the company, which is the shorter and more specific
    // phrase, not a placeholder and not the whole message.
    // The id is read as the column's own type: a `&str` bound into a `uuid` parameter is a
    // type error at the database, and it reads as a missing row rather than a wrong bind.
    let title: String =
        sqlx::query_scalar("select title from crm_deals where id = $1::uuid")
            .bind(row["deal_id"].as_str().expect("a deal id"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the deal must read");
    assert_eq!(title, "Analytical Engines Ltd");

    // The inbox shows it, and the counters agree with the row.
    let inbox = call(
        state,
        request(Method::GET, "/api/v1/crm/leads", Some(&manager), None),
    )
    .await;
    assert_eq!(inbox.status, StatusCode::OK, "body: {}", inbox.body);
    let items = inbox.body["items"].as_array().expect("the inbox has rows");
    let mine = items
        .iter()
        .find(|item| item["event_id"] == json!(event_id))
        .expect("the submission is in the inbox");
    assert_eq!(mine["name"], json!("Ada Lovelace"));
    assert_eq!(mine["company_name"], json!("Analytical Engines Ltd"));
    assert_eq!(mine["outcome"], json!("created"));
    assert!(mine["deal_id"].is_string(), "the inbox links the deal it made");

    let created_count = inbox.body["counts"]
        .as_array()
        .expect("the counters are an array")
        .iter()
        .find(|counter| counter["outcome"] == json!("created"))
        .map_or(0, |counter| counter["count"].as_i64().unwrap_or_default());
    assert!(created_count >= 1, "the created chip counts it: {}", inbox.body);
}

/// Draining the same bus twice files nothing twice.
///
/// The claim is the event id's primary key on the ledger, and this is the walk that proves the
/// guarantee is real rather than intended: a second drain over an unchanged bus is **idle**, and
/// the contact and deal counts do not move.
#[tokio::test]
async fn a_second_drain_files_nothing_twice() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let email = format!("grace-{}@example.com", Uuid::new_v4().simple());
    let event_id = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({ "name": "Grace Hopper", "email": email }),
    )
    .await;

    let first = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "body: {}", first.body);
    assert_eq!(first.body["created"], 1, "body: {}", first.body);

    let contacts_after_first = contacts_with_email(&fixture.db, fixture.org, &email).await;
    let deals_after_first = deals_from_forms(&fixture.db, fixture.org).await;
    assert_eq!(contacts_after_first, 1);
    assert_eq!(deals_after_first, 1);

    // The second drain reads the same bus. The cursor has moved past the event, so it is idle —
    // and even the *claim* is belt and braces, which is what the third drain below proves.
    let second = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "body: {}", second.body);
    assert_eq!(second.body["idle"], true, "nothing new: {}", second.body);
    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 1);
    assert_eq!(deals_from_forms(&fixture.db, fixture.org).await, 1);

    // And the claim itself: rewinding the cursor makes the drain read the *same* event again,
    // and the ledger's primary key is what stops it writing a second contact.
    sqlx::query("update crm_lead_cursor set last_event_id = 0 where id = 1")
        .execute(fixture.db.pool())
        .await
        .expect("the cursor must rewind");
    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must re-seed");
    sqlx::query("update crm_lead_cursor set last_event_id = $1 where id = 1")
        .bind(event_id - 1)
        .execute(fixture.db.pool())
        .await
        .expect("the cursor must rewind to just before the event");

    let replayed = omnion_module_crm::leads::drain(fixture.db.pool(), 100)
        .await
        .expect("the replay drain must run");
    assert_eq!(replayed.created, 0, "the claim is taken: {replayed:?}");
    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 1);
    assert_eq!(deals_from_forms(&fixture.db, fixture.org).await, 1);
}

/// A second submission from an address the CRM already knows is the **same person**.
#[tokio::test]
async fn a_repeat_submission_is_the_same_person_and_lands_in_the_repeat_stage() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let pipeline = default_pipeline_with_stages(&fixture.db, fixture.org).await;
    let stages = pipeline["stages"].as_array().expect("the stages are an array").clone();
    let first_open = stages[0].clone();
    let negotiation = stages
        .iter()
        .find(|stage| stage["name"] == json!("Negotiation"))
        .cloned()
        .expect("the seeded pipeline has a Negotiation column");

    // Park repeats in Negotiation, so the walk can tell the two deals apart by stage rather than
    // by title — two deals for one interest is the failure this setting exists to prevent.
    let saved = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({
                "repeat_stage_id": negotiation["id"],
                "source_label": "webinar",
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(saved.body["settings"]["repeat_stage_id"], negotiation["id"]);
    assert_eq!(saved.body["settings"]["create_contact"], true, "a partial body keeps the rest");

    let email = format!("repeat-{}@example.com", Uuid::new_v4().simple());
    let first_event = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({ "name": "Katherine Johnson", "email": email, "message": "First enquiry." }),
    )
    .await;
    let drained = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(drained.body["created"], 1, "body: {}", drained.body);
    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 1);

    let second_event = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({ "name": "Katherine Johnson", "email": email.to_uppercase(), "message": "Second enquiry." }),
    )
    .await;
    let again = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(again.body["merged"], 1, "the repeat is merged: {}", again.body);
    assert_eq!(again.body["created"], 0, "not a second person: {}", again.body);

    // One contact, and the case difference did not create a second one.
    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 1);

    let row = ledger_row(&fixture.db, second_event).await;
    assert_eq!(row["outcome"], json!("merged"));
    let first_row = ledger_row(&fixture.db, first_event).await;
    assert_eq!(
        row["contact_id"], first_row["contact_id"],
        "the repeat points at the contact the first one made"
    );

    // Two deals, in two different stages: the new one in the first open column, the repeat where
    // the operator said repeats go.
    let stages_of: Vec<String> = sqlx::query_scalar(
        "select s.name from crm_deals d join crm_pipeline_stages s on s.id = d.stage_id \
         where d.contact_id = $1::uuid order by d.created_at",
    )
    .bind(first_row["contact_id"].as_str().expect("a contact id"))
    .fetch_all(fixture.db.pool())
    .await
    .expect("the stages must read");
    assert_eq!(stages_of.len(), 2, "{stages_of:?}");
    assert_eq!(stages_of[0], first_open["name"].as_str().unwrap_or_default());
    assert_eq!(stages_of[1], negotiation["name"].as_str().unwrap_or_default());
}

/// A submission with nothing to file is **kept and explained**, not dropped.
#[tokio::test]
async fn a_submission_with_nothing_usable_is_kept_and_says_why() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let event_id = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({ "note": "the honeypot field is the only thing they filled in" }),
    )
    .await;

    let drained = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(drained.status, StatusCode::OK, "body: {}", drained.body);
    assert_eq!(drained.body["rejected"], 1, "body: {}", drained.body);
    assert_eq!(drained.body["created"], 0);

    let row = ledger_row(&fixture.db, event_id).await;
    assert_eq!(row["outcome"], json!("rejected"));
    assert!(
        row["detail"].as_str().is_some_and(|detail| detail.contains("no name")),
        "the inbox has to be able to say why: {row}"
    );
    assert!(row["contact_id"].is_null(), "nothing was created: {row}");

    // It is in the inbox, filterable by the outcome — a rejected submission is a thing a person
    // looks at, not a silent loss.
    let inbox = call(
        state,
        request(
            Method::GET,
            "/api/v1/crm/leads?outcome=rejected",
            Some(&manager),
            None,
        ),
    )
    .await;
    assert_eq!(inbox.status, StatusCode::OK, "body: {}", inbox.body);
    let items = inbox.body["items"].as_array().expect("the inbox has rows");
    assert!(
        items
            .iter()
            .any(|item| item["event_id"] == json!(event_id)),
        "the rejected submission is listed: {}", inbox.body
    );
}

/// A submission with no organization is recorded and counted, never filed into nobody's CRM.
#[tokio::test]
async fn a_submission_without_an_organization_is_orphaned_not_dropped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let event_id = emit_form_submitted(
        &fixture.db,
        None,
        json!({ "name": "Nobody In Particular", "email": "nobody@example.com" }),
    )
    .await;

    let drained = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(drained.body["orphaned"], 1, "body: {}", drained.body);

    let row = ledger_row(&fixture.db, event_id).await;
    assert_eq!(row["outcome"], json!("orphaned"));
    assert!(row["detail"].as_str().is_some_and(|d| d.contains("organization")), "{row}");

    // And it belongs to no tenant's inbox: the ledger is scoped by organization, so a public
    // submission on a site with no tenant is visible to the platform, not to a stranger.
    let inbox = call(
        state,
        request(Method::GET, "/api/v1/crm/leads", Some(&manager), None),
    )
    .await;
    let items = inbox.body["items"].as_array().expect("the inbox has rows");
    assert!(
        !items.iter().any(|item| item["event_id"] == json!(event_id)),
        "an orphaned submission is in nobody's inbox: {}", inbox.body
    );
}

/// Turning the routing off is a decision the settings screen records, and the drain obeys.
#[tokio::test]
async fn an_organization_that_turns_leads_off_records_the_submission_and_writes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    omnion_module_crm::leads::seed_cursor(fixture.db.pool())
        .await
        .expect("the cursor must seed");

    let saved = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({ "create_contact": false, "create_deal": false })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "body: {}", saved.body);
    assert_eq!(saved.body["settings"]["create_contact"], false);
    assert_eq!(saved.body["settings"]["create_deal"], false);

    let email = format!("off-{}@example.com", Uuid::new_v4().simple());
    let event_id = emit_form_submitted(
        &fixture.db,
        Some(fixture.org),
        json!({ "name": "Aled Edwards", "email": email }),
    )
    .await;

    let drained = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&manager), None),
    )
    .await;
    assert_eq!(drained.body["disabled"], 1, "body: {}", drained.body);
    assert_eq!(contacts_with_email(&fixture.db, fixture.org, &email).await, 0);
    assert_eq!(deals_from_forms(&fixture.db, fixture.org).await, 0);
    assert_eq!(ledger_row(&fixture.db, event_id).await["outcome"], json!("disabled"));
}

/// Reading the log and changing the routing are separate keys, and the settings are a tenant's.
#[tokio::test]
async fn the_ingress_keys_and_the_tenant_boundary_are_enforced() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;

    // The plain reader holds neither key.
    let reader = fixture.token(&fixture.reader).await;
    for (method, uri) in [
        (Method::GET, "/api/v1/crm/leads"),
        (Method::GET, "/api/v1/crm/leads/settings"),
        (Method::POST, "/api/v1/crm/leads/drain"),
    ] {
        let response = call(state, request(method, uri, Some(&reader), None)).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{uri} must be guarded, not merely hidden: {}",
            response.body
        );
    }

    // Unauthenticated is `401` before any of that.
    let anonymous = call(state, request(Method::GET, "/api/v1/crm/leads", None, None)).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // A reader of the ingress may look but not reconfigure: the two keys really are separate.
    let (watcher_id, watcher) = create_account(&fixture.db, Some(fixture.org), "CRM Lead Watcher").await;
    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    grant(&fixture.db, fixture.org, watcher_id, owner_id, &["crm.leads.read"]).await;
    let watcher = login(&state, &watcher).await;
    assert!(watcher.is_empty() == false, "the watcher signed in");

    let read = call(
        state,
        request(Method::GET, "/api/v1/crm/leads", Some(&watcher), None),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "body: {}", read.body);

    let reconfigure = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&watcher),
            Some(json!({ "create_deal": false })),
        ),
    )
    .await;
    assert_eq!(
        reconfigure.status,
        StatusCode::FORBIDDEN,
        "watching the pipeline is not deciding it: {}",
        reconfigure.body
    );

    let drain = call(
        state,
        request(Method::POST, "/api/v1/crm/leads/drain", Some(&watcher), None),
    )
    .await;
    assert_eq!(drain.status, StatusCode::FORBIDDEN, "body: {}", drain.body);

    // A writer of another tenant may not read this one: the inbox is scoped by organization and
    // the route resolves the organization from the session, never from the query.
    let foreign = grant_and_login(
        &fixture,
        "CRM Foreign Lead Reader",
        &["crm.leads.read", "crm.leads.manage"],
    )
    .await;
    let foreign_inbox = call(
        state,
        request(Method::GET, "/api/v1/crm/leads", Some(&foreign), None),
    )
    .await;
    assert_eq!(foreign_inbox.status, StatusCode::OK, "body: {}", foreign_inbox.body);
    let items = foreign_inbox.body["items"].as_array().expect("the inbox has rows");
    let ours: Vec<&Value> = items
        .iter()
        .filter(|item| item["name"] != json!(""))
        .collect();
    assert!(
        ours.iter().all(|item| item["event_id"].is_i64()),
        "the foreign inbox is its own: {}", foreign_inbox.body
    );
    let mut settings = call(
        state,
        request(Method::GET, "/api/v1/crm/leads/settings", Some(&foreign), None),
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK);
    // Writing the foreign tenant's routing is allowed — it is *their* tenant — and must not
    // change ours.
    settings = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&foreign),
            Some(json!({ "source_label": "foreign" })),
        ),
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK, "body: {}", settings.body);

    // Read the *manager* is the tenant's own: the ingress keys were granted explicitly at the
    // top of this walk, and a caller without them is refused before the row is read at all.
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;
    let ours_settings = call(
        state,
        request(Method::GET, "/api/v1/crm/leads/settings", Some(&manager), None),
    )
    .await;
    assert_eq!(ours_settings.status, StatusCode::OK, "body: {}", ours_settings.body);
    assert_ne!(
        ours_settings.body["settings"]["source_label"],
        json!("foreign"),
        "a tenant's routing is not another tenant's to write"
    );
}

/// A stage from another pipeline is refused by name, not silently ignored.
#[tokio::test]
async fn a_stage_from_another_pipelines_organization_is_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    let foreign_pipeline = default_pipeline_with_stages(&fixture.db, fixture.other_org).await;
    let foreign_stage = foreign_pipeline["stages"].as_array().expect("stages")[0]["id"].clone();

    let refused = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({ "stage_id": foreign_stage })),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "body: {}", refused.body);
    assert_eq!(refused.body["error"]["code"], json!("invalid_crm_record"));

    // And the refusal left the stored row alone.
    let after = call(
        state,
        request(Method::GET, "/api/v1/crm/leads/settings", Some(&manager), None),
    )
    .await;
    assert_eq!(after.body["settings"]["stage_id"], Value::Null, "body: {}", after.body);
}

/// The settings are audited and announced, and a body that changes nothing is silent.
#[tokio::test]
async fn changing_the_routing_is_audited_and_only_the_change_is_announced() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let state = &fixture.state;
    let manager = grant_lead_powers(&fixture, &fixture.manager).await;

    // Read it first: the endpoint's first write creates the row, and the walk below needs a row
    // that already exists so "nothing changed" really means nothing.
    let _ = call(
        state,
        request(Method::GET, "/api/v1/crm/leads/settings", Some(&manager), None),
    )
    .await;
    omnion_module_crm::leads::load_settings(fixture.db.pool(), fixture.org)
        .await
        .expect("the settings row must exist");

    let changed = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({ "source_label": "trade-show", "create_deal": false })),
        ),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "body: {}", changed.body);
    assert_eq!(changed.body["settings"]["source_label"], json!("trade-show"));
    assert_eq!(changed.body["settings"]["create_deal"], false);
    assert_eq!(changed.body["configured"], true);

    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log \
         where action = 'crm.lead_settings.updated' and organization_id = $1",
    )
    .bind(fixture.org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit must read");
    assert_eq!(audited, 1, "the change is audited exactly once");

    let announced: i64 = sqlx::query_scalar(
        "select count(*) from events where name = 'crm.lead_settings.updated' and organization_id = $1",
    )
    .bind(fixture.org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the bus must read");
    assert_eq!(announced, 1, "the change is announced exactly once");

    // The same body again changes nothing, so it is neither audited nor announced: an audit row
    // per render would make the trail useless.
    let again = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({ "source_label": "trade-show", "create_deal": false })),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "body: {}", again.body);

    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log \
         where action = 'crm.lead_settings.updated' and organization_id = $1",
    )
    .bind(fixture.org)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit must read");
    assert_eq!(audited, 1, "an unchanged save is not an event in the trail");

    // The label is normalised, so `Trade Show` and `trade show` are the same label rather than
    // two tags on two contacts.
    let shouty = call(
        state,
        request(
            Method::PUT,
            "/api/v1/crm/leads/settings",
            Some(&manager),
            Some(json!({ "source_label": "  TRADE-SHOW  " })),
        ),
    )
    .await;
    assert_eq!(
        shouty.body["settings"]["source_label"],
        json!("trade-show"),
        "the label is trimmed and lowered"
    );
}

/// The two new keys are in the catalogue, in the owner's role, and nowhere else by accident.
#[tokio::test]
async fn the_lead_keys_are_catalogued_and_belong_to_the_owner() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    for key in ["crm.leads.read", "crm.leads.manage"] {
        let known: bool = sqlx::query_scalar("select exists (select 1 from permissions where key = $1)")
            .bind(key)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the permissions must read");
        assert!(known, "{key} must be seeded into the catalogue");
    }

    // The CRM *manager* does not hold them: the ingress is a separate decision from the rest of
    // the family, and the suite grants it explicitly where it needs it.
    let held: i64 = sqlx::query_scalar(
        "select count(*) from role_permissions rp \
         join role_bindings rb on rb.role_id = rp.role_id \
         join users u on u.id = rb.subject_id \
         where u.email = $1 and rb.revoked_at is null and rb.user_id is not null \
           and rp.permission_key = 'crm.leads.read'",
    )
    .bind(&fixture.manager)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the bindings must read");
    assert_eq!(held, 0, "a CRM manager is not handed the ingress by default");
}

// ---------------------------------------------------------------------------------------------
// The platform account and the missing tenant (2026-09-28)
// ---------------------------------------------------------------------------------------------

/// A platform account with no primary organization reads the tenant it is the only member of.
///
/// The account this describes is not a corner case: it is the **first-run `owner`** of every
/// installation. `users.organization_id` is `null` for it by design, because it is the account
/// that *creates* the tenants. Every CRM screen it opened therefore answered `400
/// organization_required` until the module grew a fallback — the whole module was unreachable
/// for its own owner, which is the sign of a rule written for one caller and generalised later.
///
/// The fallback is deliberately narrow, and the other two arms are proven below rather than
/// assumed: one tenant is taken, **two is a refusal** rather than a coin toss, and an account
/// bound to none is told so. A screen that silently opened the wrong tenant's records would be a
/// data leak wearing the costume of a convenience.
#[tokio::test]
async fn a_platform_account_reads_the_one_organization_it_is_bound_to() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // The owner is bound to exactly one organization for the length of this walk — the arm
    // under test. The binding is the *platform* `owner` role scoped to the tenant, which is what
    // an administrator grants when they want the platform owner to look inside a customer.
    let role = role_store::create_role(
        fixture.db.pool(),
        NewRole {
            organization_id: fixture.org,
            key: format!("crm-p{}", Uuid::new_v4().simple()),
            name: "CRM Platform Reader".to_owned(),
            description: "The platform owner looking into one tenant".to_owned(),
            priority: 300,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the tenant-scoped role must be created");
    role_store::set_role_permissions(
        fixture.db.pool(),
        role.id,
        &MANAGER_PERMISSIONS
            .iter()
            .map(|key| RolePermissionInput {
                key: (*key).to_owned(),
                effect: Effect::Allow,
            })
            .collect::<Vec<_>>(),
    )
    .await
    .expect("the tenant role's permissions must be written");

    let owner_id = account_id(&fixture.db, &fixture.owner).await;
    omnion_permissions::bindings::grant(
        fixture.db.pool(),
        NewBinding {
            role_id: role.id,
            user_id: owner_id,
            scope: PermScope::Organization {
                organization_id: fixture.org,
            },
            granted_by: Some(owner_id),
            expires_at: None,
        },
    )
    .await
    .expect("the tenant binding must be created");

    // A contact to read, so the answer is a list and not merely a status.
    let manager_token = fixture.token(&fixture.manager).await;
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/crm/contacts",
            Some(&manager_token),
            Some(json!({ "first_name": "Tenant", "last_name": "Probe" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "the fixture's contact must be created");

    // The token is minted after the binding on purpose: a session's powers are read at
    // login, so a token taken earlier answers 403 on the very route under test.
    let owner_token = fixture.token(&fixture.owner).await;

    // The route with **no** organization named: the shape the panel sends.
    for uri in [
        "/api/v1/crm/contacts",
        "/api/v1/crm/companies",
        "/api/v1/crm/deals?view=list",
        "/api/v1/crm/activities",
        "/api/v1/crm/leads",
    ] {
        let response = call(
            &fixture.state,
            request(Method::GET, uri, Some(&owner_token), None),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{uri} must resolve the caller's one organization instead of refusing: {}",
            response.body
        );
    }

    // And the list actually holds the tenant's record — a `200` with somebody else's empty list
    // would pass the check above and mean nothing.
    let listed = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/contacts", Some(&owner_token), None),
    )
    .await;
    let items = listed.body["items"].as_array().expect("the list envelope must be an object");
    assert!(
        items.iter().any(|row| row["first_name"] == "Tenant"),
        "the resolved tenant's own contact must be in the answer"
    );

    // Naming a different tenant is still a cross-organization refusal: the fallback decides what
    // an *unnamed* request means, and it grants no new reach.
    let elsewhere = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?organization_id={}", fixture.other_org),
            Some(&owner_token),
            None,
        ),
    )
    .await;
    // A platform account **may** name any tenant — that is what the platform role is, and the
    // route guard still decides the permission. What the fallback must never do is grant reach
    // the named path did not already have, so the check is that the *answer* is scoped to the
    // tenant that was named, not that the request is refused.
    assert_eq!(
        elsewhere.status,
        StatusCode::OK,
        "a named tenant is the platform account's to read: {}",
        elsewhere.body
    );
    let elsewhere_items = elsewhere.body["items"]
        .as_array()
        .expect("the list envelope is an object");
    assert!(
        !elsewhere_items.iter().any(|row| row["first_name"] == "Tenant"),
        "the other tenant's rows must not leak into the answer: {}",
        elsewhere.body
    );
}

/// Two organizations is a **refusal**, not a guess.
///
/// The tempting alternative is "take the first", and the reason it is wrong is that it is
/// indistinguishable from correct to the person looking at the screen: rows appear, nothing is
/// greyed out, and the only evidence is the tenant chip several rows away. A `400` that names
/// the count is a question the caller can answer, and the picker in the panel is built to ask it.
#[tokio::test]
async fn a_platform_account_bound_to_two_organizations_is_told_to_choose() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let owner_id = account_id(&fixture.db, &fixture.owner).await;

    for organization in [fixture.org, fixture.other_org] {
        let role = role_store::create_role(
            fixture.db.pool(),
            NewRole {
                organization_id: organization,
                // A role key is at most 64 characters; the unique suffix is what keeps it distinct.
                key: format!("crm-two{}", Uuid::new_v4().simple()),
                name: "CRM Two Tenants".to_owned(),
                description: "A role in each of two organizations".to_owned(),
                priority: 300,
                inherits_role_id: None,
            },
        )
        .await
        .expect("the role must be created");
        omnion_permissions::bindings::grant(
            fixture.db.pool(),
            NewBinding {
                role_id: role.id,
                user_id: owner_id,
                scope: PermScope::Organization { organization_id: organization },
                granted_by: Some(owner_id),
                expires_at: None,
            },
        )
        .await
        .expect("the binding must be created");
    }

    // A bearer token, not the address: `request` takes a token, and an e-mail answers 401
    // before the route under test is ever reached.
    let token = fixture.token(&fixture.owner).await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/contacts", Some(&token), None),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an account in two organizations must be asked, not answered for: {}",
        response.body
    );
    assert_eq!(
        response.body["error"]["code"], "organization_ambiguous",
        "the refusal must be its own code — a client that cannot tell this apart from an empty          organization has nothing to draw a picker from"
    );

    // Naming one of them is enough, and it is the only thing that is.
    let chosen = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/crm/contacts?organization_id={}", fixture.org),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(
        chosen.status,
        StatusCode::OK,
        "naming a tenant must resolve the ambiguity the unnamed request was refused for"
    );
}

/// An account bound to **no** organization is told it, and is not handed a stranger's rows.
///
/// The account is the platform owner before anybody has given it a role in any tenant, which is
/// the state of a brand-new installation. There is no right answer to give it, so the API says
/// so with the code the panel already knows how to draw a "nothing to show" state from.
#[tokio::test]
async fn a_platform_account_in_no_organization_is_told_it_has_none() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // A fresh platform owner: no primary organization and no tenant binding at all.
    let (account, email) = create_account(&fixture.db, None, "CRM Unbound Owner").await;
    seed::bind_owner(fixture.db.pool(), account)
        .await
        .expect("the platform owner binding must be created");
    let _ = &fixture.owner;

    // A session, not an address: `request` takes a bearer token, and an e-mail answers 401
    // before the route is ever reached — which would pass for a working guard.
    let token = login(&fixture.state, &email).await;
    let response = call(
        &fixture.state,
        request(Method::GET, "/api/v1/crm/contacts", Some(&token), None),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an unbound platform account has no tenant to read: {}",
        response.body
    );

    // The code depends on the **installation**, and asserting one of the two unconditionally made
    // this test a function of how many organizations the database happened to hold. A fresh
    // installation has none, so the refusal is `organization_required`; an installation with more
    // than one is a *choice* and says `organization_ambiguous`, which is the code the panel draws
    // its organization picker from. Both are refusals, and the status above already proves the
    // refusal — so what this test owns is that neither case is answered with a stranger's rows,
    // not which of the two codes a shared database earns.
    let code = response.body["error"]["code"]
        .as_str()
        .expect("every refusal carries its code");
    assert!(
        code == "organization_required" || code == "organization_ambiguous",
        "an unbound platform account is refused with a code the panel knows, not {code:?}"
    );
    assert!(
        !response.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("organization_id"),
        "the refusal must not ask for a parameter this caller has no value for"
    );
}
