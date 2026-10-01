//! The AI app builder's **review** surface (docs/requests/REQ-045, slice 2).
//!
//! Slice 1 proved the store's rules in a database and nothing else could reach them: the
//! plans table was written by test code and by no handler, so "a plan can be reviewed" was a
//! statement about a Rust API nobody could call. This suite is the proof that the review
//! screen has a wire behind it — the nine routes a reviewer presses, each driven through the
//! real router with a real session and a real tenant.
//!
//! The claims worth stating before the walks, because each is a way this half could have been
//! wrong and still looked right:
//!
//! * **A rejection with no reason is refused, and so is one whose reason is whitespace.**
//!   `0226` deliberately does *not* enforce this in SQL (a regeneration retires a predecessor
//!   with `rejected` and no reason), so the guarantee is the store's and it has to be proven
//!   on the wire: a bare `POST …/reject` answers `422` and the row stays `pending`.
//! * **A machine retirement is not a reviewer's rejection.** After a regeneration the previous
//!   row is `rejected` *with* `superseded by a regenerated version`, and that is what the
//!   tree renders — the two must be distinguishable, or "reject and try again" and "try again"
//!   are the same history.
//! * **Accept is refused for an artifact the validator refused, by name.** The store enforces
//!   it; what this file proves is that the refusal reaches the screen as a `422` carrying the
//!   finding, rather than as a silent no-op.
//! * **The tenancy check is the store's `where`, so it is `404` and not `403`.** A second
//!   tenant's plan id answers `404` on the detail and contributes **zero rows** to the other
//!   tenant's list — asserted against the list body rather than against a status code.
//! * **`appbuilder.apply` guards nothing yet, and that is visible.** The four keys are
//!   catalogued (the crate test proves it) while the apply runner is the next slice, so the
//!   suite proves the *other* three actually refuse an account that holds none of them: a
//!   caller with no app-builder keys gets `403` from the list, which is what makes the key
//!   a power rather than a label.
//! * **`generate` was the one route that answered with its own absence.** It used to spend
//!   zero provider calls and fail the plan with "the typed artifact generator is not wired
//!   yet" — a status code wearing the costume of a "coming soon" button. It now asks once
//!   and stores what came back, so the walks below assert the plan **exists** (nine rows,
//!   one per required kind) rather than asserting the shape of an apology. Three properties
//!   the walks pin because each was a way this could have looked finished and not been:
//!   the plan settles at `draft` and never `approved` (a generator must not accept its own
//!   work); the provider is called **once** (a repair loop would double the cost of a plan
//!   that was merely mis-spelled); and an answer missing a required kind keeps its
//!   artifacts and names the gap rather than reading as a finished generation.
//!
//! The harness — CSRF-aware credential, scratch database, mock provider — is the sibling
//! decision suite's, copied rather than reinvented, for the reason its own header states.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post as route_post;
use axum::{Json, Router};
use http_body_util::BodyExt;
mod support;
use omnion_api::rate_limit_middleware::RateLimiter;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_permissions::bindings;
use omnion_permissions::model::{NewBinding, Scope};
use omnion_security::RatePolicy;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// A scripted mock provider: the answers it will give, in order, and how many were taken.
///
/// The count is a **counter the provider increments**, not the remainder of the queue — the
/// remainder version reports "1 call" after the first pop and "0" after the second, so a test
/// asserting "exactly one regeneration call" would be asserting a number the fixture invented.
#[derive(Clone, Default)]
struct Script {
    queue: Arc<Mutex<VecDeque<String>>>,
    taken: Arc<Mutex<usize>>,
}

impl Script {
    fn of(answers: &[&str]) -> Self {
        Self {
            queue: Arc::new(Mutex::new(
                answers.iter().map(|a| (*a).to_owned()).collect(),
            )),
            taken: Arc::new(Mutex::new(0)),
        }
    }

    fn calls(&self) -> usize {
        *self.taken.lock().expect("the counter lock must hold")
    }

    fn next(&self) -> String {
        *self.taken.lock().expect("the counter lock must hold") += 1;
        self.queue
            .lock()
            .expect("the script lock must hold")
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "the provider was called {} times but the test scripted fewer",
                    self.calls()
                )
            })
    }
}

/// A running mock provider.
struct MockProvider {
    base_url: String,
    script: Script,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");
        let app = Router::new()
            .route("/v1/chat/completions", route_post(mock_chat))
            .with_state(script.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{address}/v1"),
            script,
            task,
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `POST /v1/chat/completions` — one scripted answer.
async fn mock_chat(State(script): State<Script>, Json(body): Json<Value>) -> Response {
    let answer = script.next();
    Json(json!({
        "choices": [{ "message": { "role": "assistant", "content": answer },
                      "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 4, "total_tokens": 11 }
    }))
    .into_response()
}

/// One in-process HTTP call.
struct TestResponse {
    status: StatusCode,
    /// **Every** `Set-Cookie`, not the first: a walkthrough credential is two cookies (the
    /// session and the CSRF token) and reading only the first gives every write a session and
    /// no token, which the double-submit check correctly refuses.
    set_cookies: Vec<String>,
    body: Value,
    text: String,
}

/// A throwaway database with every migration applied.
struct Harness {
    state: AppState,
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let mut config = Config::from_env().expect("environment must be valid");
        // Every walk here writes, and with no CSRF secret configured every one of them is
        // refused with `csrf_unavailable` — the product working, not a defect. The fixture
        // sets the secret on the **config** rather than relying on `OMNION_CSRF_SECRET` being
        // in the shell: a test process does not have it, and the day it stops having it every
        // write in this file fails for a reason that has nothing to do with the console.
        support::walk_auth::with_csrf_secret(&mut config);

        let database = format!("omnion_abuilderr_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
        let state = AppState::new(
            BuildInfo::new("omnion-api", "0.1.0-test"),
            config,
            db.clone(),
            redis,
            omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
                .expect("the default storage configuration is valid"),
        );
        give_the_suite_its_own_sign_in_budget(&state);

        Some(Self {
            state,
            db,
            maintenance,
            database,
        })
    }

    async fn call(&self, request: Request<Body>) -> TestResponse {
        let response = routes::router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must answer");
        let status = response.status();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body must read")
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        TestResponse {
            status,
            set_cookies,
            body,
            text,
        }
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

fn token_of(response: &TestResponse) -> String {
    support::walk_auth::Session::from_set_cookies(&response.set_cookies).pack()
}

fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match token {
        Some(token) => support::walk_auth::apply_credential(token, builder),
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::GET, uri, token, None)
}

fn post(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::POST, uri, token, Some(body))
}

fn patch(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    request(Method::PATCH, uri, token, Some(body))
}

fn delete(uri: &str, token: Option<&str>) -> Request<Body> {
    request(Method::DELETE, uri, token, None)
}

/// One harness per test, dropped on a **current-thread** runtime.
///
/// The drop is the reason this is a macro returning early rather than a lazy static: a
/// `#[tokio::test]` that spawns cleanup in a `Drop` on a current-thread runtime never runs
/// it, and every suite that has done that has leaked a database per run until the server ran
/// out of room.
macro_rules! harness {
    () => {
        match Harness::fresh().await {
            Some(harness) => harness,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Tenants
// ---------------------------------------------------------------------------------------------

/// Raise **only** the `sign_in` ceiling for this process.
///
/// Nine walks that each sign in an owner (and one that signs in two) exceed the shipped
/// `sign_in` budget (10 per 5 minutes) inside a single run, and the limiter is a process-wide
/// cell — so the suite would die on `429 rate_limited` at some walk's sign-in, on a line that
/// has nothing to do with what it was testing. The failure names a rate limit on a suite that
/// was never testing rate limits, and the obvious reading ("the limiter is too strict") is the
/// opposite of the truth. A shared, process-wide budget means test N+1 is the one that reports
/// the problem.
///
/// The other ceilings are left exactly as a deployment ships them, so nothing here can be the
/// reason a genuinely over-budget request stops being refused.
fn give_the_suite_its_own_sign_in_budget(state: &AppState) {
    let policies: Vec<RatePolicy> = RatePolicy::defaults()
        .into_iter()
        .map(|mut policy| {
            if policy.scope == "sign_in" {
                policy.limit = 10_000;
            }
            policy
        })
        .collect();
    omnion_api::rate_limit_middleware::install(RateLimiter::new(state, policies));
}

/// One signed-in account with a tenant, its id, and its credential.
struct Tenant {
    token: String,
    user_id: Uuid,
    organization_id: Uuid,
}

/// Sign in a fresh installation and give the account a tenant to work in.
/// Sign in a fresh installation and give the account a tenant to work in.
///
/// **One owner per installation, and that is a product rule rather than a fixture limit:**
/// `POST /onboarding/owner` answers `409 already_installed` once any account exists. A second
/// tenant is therefore built the way an administrator builds one — [`other_tenant_with_tenant`]
/// — and the first-run helper is only ever called once per database. Getting this wrong reads
/// as "the wizard refuses a second owner", which is a real rule and the wrong thing to be
/// testing when what the walk needs is two *organizations*.
async fn tenant_with_tenant(harness: &Harness, label: &str) -> Tenant {
    let owner = harness
        .call(post(
            "/api/v1/onboarding/owner",
            json!({
                "display_name": "Ada Lovelace",
                "email": format!("owner-{label}-{}@omnion.test", Uuid::new_v4().simple()),
                "password": PASSWORD,
            }),
            None,
        ))
        .await;
    assert_eq!(owner.status, StatusCode::CREATED, "{:?}", owner.body);
    let token = token_of(&owner);
    let user_id = Uuid::parse_str(owner.body["user"]["id"].as_str().expect("user.id"))
        .expect("user.id is a uuid");

    // The wizard's organization step answers a **status body**, not the tenant it created, so
    // the id is read from the account it attached — reading it from the response would be a
    // `None` that panics several assertions later with a message about plans. The slug is
    // per-call unique because a slug is unique platform-wide.
    let created = harness
        .call(post(
            "/api/v1/onboarding/organization",
            json!({
                "name": "QA Organization",
                "slug": format!("qa-org-{label}-{}", Uuid::new_v4().simple()),
            }),
            Some(&token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.body);

    let organization_id: Uuid =
        sqlx::query_scalar("select organization_id from users where id = $1")
            .bind(user_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the wizard must attach a tenant");

    Tenant {
        token,
        user_id,
        organization_id,
    }
}

/// A **second** tenant in the same installation, with its own signed-in account.
///
/// The tenancy walk needs two organizations that cannot see each other's plans, and the
/// first-run wizard is deliberately a once-per-installation path — so the second account is
/// created through `omnion_identity::users` and attached to its own organization the way an
/// administrator would, then signed in over the real login route so the credential is one the
/// platform issued rather than one the fixture forged.
async fn other_tenant_with_tenant(harness: &Harness, label: &str) -> Tenant {
    let slug = format!("qa-org-{label}-{}", Uuid::new_v4().simple());
    let organization_id: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind("Other QA Organization")
            .bind(&slug)
            .fetch_one(harness.db.pool())
            .await
            .expect("the second organization must be created");

    let email = format!("owner-{label}-{}@omnion.test", Uuid::new_v4().simple());
    let user = omnion_identity::users::create_user(
        harness.db.pool(),
        omnion_identity::users::NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Grace Hopper".to_owned(),
            organization_id: Some(organization_id),
        },
    )
    .await
    .expect("the second account must be created");

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::OK,
        "the second tenant's account must be able to sign in: {:?}",
        signed_in.body
    );

    Tenant {
        token: token_of(&signed_in),
        user_id: user.id,
        organization_id,
    }
}

/// A signed-in **member** of `owner`'s tenant, holding no role of its own.
///
/// This exists because the Owner role carries `BasePermissions::All` — an owner-based "this
/// account holds no app-builder key" walk is impossible to write, and the version that tries
/// it measures the seed rather than the guard. A member's rights are exactly what the fixture
/// grants it, so a refusal here is the guard's answer and not the seed's.
async fn member_without_keys(harness: &Harness, owner: &Tenant) -> Tenant {
    let email = format!("member-{}@omnion.test", Uuid::new_v4().simple());
    let user = omnion_identity::users::create_user(
        harness.db.pool(),
        omnion_identity::users::NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Member".to_owned(),
            organization_id: Some(owner.organization_id),
        },
    )
    .await
    .expect("the member account must be created");

    let signed_in = harness
        .call(post(
            "/api/v1/auth/login",
            json!({ "email": email, "password": PASSWORD }),
            None,
        ))
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::OK,
        "the member must be able to sign in: {:?}",
        signed_in.body
    );

    Tenant {
        token: token_of(&signed_in),
        user_id: user.id,
        organization_id: owner.organization_id,
    }
}

/// Give an account exactly `keys` and nothing else.
///
/// A **role** rather than a direct permission write, because that is the platform's shape: a
/// binding names a role and the role names its permissions, so a hand-written "grant one
/// permission" insert would be a second way to say something the store already says. The
/// sibling decision suite's `member_with` is the proven spelling of exactly this.
///
/// A fresh role per call is what makes "read alone must not carry review" testable: a second
/// call adds a role rather than widening the first, so the walk can grant `read`, see the
/// list open, and then find the decisions still refused.
async fn grant_keys(harness: &Harness, user_id: Uuid, organization_id: Uuid, keys: &[&str]) {
    let role = omnion_permissions::roles::create_role(
        harness.db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("qa-role-{}", Uuid::new_v4().simple()),
            name: "QA role".to_owned(),
            description: "Built by the AI app builder review walk".to_owned(),
            priority: 10,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the role must be created");

    let permissions = keys
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect::<Vec<_>>();
    omnion_permissions::roles::set_role_permissions(harness.db.pool(), role.id, &permissions)
        .await
        .expect("the permission set must be written");

    bindings::grant_if_missing(
        harness.db.pool(),
        NewBinding {
            role_id: role.id,
            user_id,
            scope: Scope::Global,
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be written");
}

/// Grant the three keys the review surface uses. `appbuilder.apply` is deliberately absent —
/// the apply runner is slice 3, so no route here is behind it.
async fn grant_reviewer(harness: &Harness, tenant: &Tenant) {
    grant_keys(
        harness,
        tenant.user_id,
        tenant.organization_id,
        &[
            "appbuilder.read",
            "appbuilder.generate",
            "appbuilder.review",
        ],
    )
    .await;
}

/// A complete plan's worth of artifacts: one entity, its fields, and every required kind.
///
/// Written through the **store**, not through a route, because the typed generator is slice
/// 3: these walks are about the review surface, and seeding a plan is the fixture's job.
async fn seed_plan(harness: &Harness, organization_id: Uuid, created_by: Uuid) -> Uuid {
    use omnion_module_app_builder::{NewArtifact, NewPlan, PlanStore};

    let plan = PlanStore::new(harness.db.pool())
        .begin(NewPlan {
            organization_id: Some(organization_id),
            site_id: None,
            prompt: "Create an app to manage employees' leave requests".into(),
            title: None,
            model_label: "qa/mock-model".into(),
            created_by: Some(created_by),
            supersedes_id: None,
        })
        .await
        .expect("the plan row must be written");

    let drafts: Vec<NewArtifact> = vec![
        artifact(
            "entity",
            "leave_request",
            None,
            0,
            json!({
                "key": "leave_request", "label": "Leave request", "plural_label": "Leave requests"
            }),
            "Leave requests are what the app is for.",
        ),
        artifact(
            "field",
            "start_date",
            Some("leave_request"),
            1,
            json!({
                "key": "start_date", "label": "Start date", "type": "date"
            }),
            "A request has a start.",
        ),
        artifact(
            "field",
            "days",
            Some("leave_request"),
            2,
            json!({
                "key": "days", "label": "Days", "type": "integer", "required": true
            }),
            "The approver needs the length.",
        ),
        artifact(
            "ui",
            "leave_request_list",
            Some("leave_request"),
            3,
            json!({
                "key": "leave_request_list", "label": "Leave requests", "entity": "leave_request",
                "columns": ["start_date", "days"]
            }),
            "Operators list what is pending.",
        ),
        artifact(
            "permission",
            "leave.approve",
            None,
            4,
            json!({
                "key": "leave.approve", "description": "Approve a leave request"
            }),
            "Approving is a separate power from reading.",
        ),
        artifact(
            "role",
            "leave_manager",
            None,
            5,
            json!({
                "key": "leave_manager", "label": "Leave manager",
                "permissions": ["leave.approve"]
            }),
            "One role for the approvers.",
        ),
        artifact(
            "workflow",
            "leave_approval",
            None,
            6,
            json!({
                "key": "leave_approval", "trigger": "leave_request.created",
                // `action` is REQUIRED on every step — the validator refuses a step the engine has
                // nothing to run, and an `approval` step is an action like any other. The fixture
                // shipped without it and the walk caught it by naming the offender, which is the
                // one thing a bare count ("8 pending, not 9") could never have told us.
                "steps": [{ "name": "approve", "kind": "task", "action": "leave.approve",
                            "params": { "entity": "leave_request" } }]
            }),
            "Every request needs a human decision.",
        ),
        artifact(
            "notification",
            "leave_decided",
            None,
            7,
            json!({
                "key": "leave_decided", "channel": "email", "event": "leave_request.decided"
            }),
            "The requester learns the outcome.",
        ),
        artifact(
            "report",
            "leave_summary",
            None,
            8,
            json!({
                "key": "leave_summary", "group_by": "days"
            }),
            "Managers count days, not rows.",
        ),
    ];

    let store = PlanStore::new(harness.db.pool());
    for draft in &drafts {
        let findings = omnion_module_app_builder::validate_artifact(draft);
        store
            .artifact(plan.id, draft, &findings)
            .await
            .expect("the artifact row must be written");
    }

    // The plan leaves `generating` exactly as a real answer does.
    store
        .answer(
            plan.id,
            "draft",
            omnion_module_app_builder::PlanUsage {
                input: Some(120),
                output: Some(340),
                cost_cents: 1,
            },
            Some("Leave request management"),
        )
        .await
        .expect("the plan must be answered")
        .expect("the answer must move the row");

    plan.id
}

fn artifact(
    kind: &str,
    key: &str,
    parent: Option<&str>,
    ordinal: i32,
    spec: Value,
    rationale: &str,
) -> omnion_module_app_builder::NewArtifact {
    omnion_module_app_builder::NewArtifact {
        kind: kind.into(),
        key: key.into(),
        parent_key: parent.map(str::to_owned),
        ordinal,
        spec,
        rationale: rationale.into(),
        validation: json!([]),
    }
}

/// The id of one artifact of a plan, read from the database.
async fn artifact_id(harness: &Harness, plan_id: Uuid, kind: &str, key: &str) -> Uuid {
    sqlx::query_scalar(
        "select id from app_builder_artifacts where plan_id = $1 and kind = $2 and key = $3",
    )
    .bind(plan_id)
    .bind(kind)
    .bind(key)
    .fetch_one(harness.db.pool())
    .await
    .expect("the artifact row must exist")
}

/// The status one artifact is stored at.
async fn status_of(harness: &Harness, artifact_id: Uuid) -> String {
    sqlx::query_scalar("select status from app_builder_artifacts where id = $1")
        .bind(artifact_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the artifact row must exist")
}

/// Connect a provider to the mock and make one model the default.
async fn connect(harness: &Harness, tenant: &Tenant, mock: &MockProvider) {
    let created = harness
        .call(post(
            "/api/v1/ai/providers",
            json!({
                "name": "Mock AI",
                "base_url": mock.base_url,
                "models": [{ "key": "mock-small", "is_default": true }],
            }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

/// The three keys are real powers: an account with none of them is refused, and an account
/// with them is not.
#[tokio::test]
async fn an_account_without_an_app_builder_key_is_refused_the_whole_surface() {
    let harness = harness!();
    // **Not the wizard's owner.** The Owner role carries `BasePermissions::All` by design
    // ("Full control of the platform"), so its accounts hold every catalogue key including the
    // four added this slice — an owner-based "no keys" walk measures the seed, not the guard,
    // and passes for the wrong reason. A **member** account is the one whose rights are
    // exactly what the fixture grants it.
    let owner = tenant_with_tenant(&harness, "keys-owner").await;
    let tenant = member_without_keys(&harness, &owner).await;

    for (method, uri) in [
        (Method::GET, "/api/v1/app-builder/plans"),
        (Method::GET, "/api/v1/app-builder/examples"),
    ] {
        let response = harness
            .call(request(method, uri, Some(&tenant.token), None))
            .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{uri} must be refused for an account with no app-builder key: {:?}",
            response.body
        );
        // `permission_denied` is the platform's own code for a session that simply does not carry
        // the key; `forbidden_missing_permission` is a sibling surface's spelling and guessing
        // it here would have made the walk assert a code this guard has never produced.
        assert_eq!(
            response.body["error"]["code"], "permission_denied",
            "the refusal names the power that is missing"
        );
    }

    // Granting the read key alone must open the list and **not** the decisions, which is the
    // whole point of four keys rather than one.
    grant_keys(
        &harness,
        tenant.user_id,
        tenant.organization_id,
        &["appbuilder.read"],
    )
    .await;
    let allowed = harness
        .call(get("/api/v1/app-builder/plans", Some(&tenant.token)))
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;
    let refused = harness
        .call(patch(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}"),
            json!({ "spec": { "key": "leave_summary", "group_by": "days" } }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "read must not carry review: a reader who can rewrite a proposal is a reviewer who \
         never applied for the job"
    );
}

/// A reviewer accepts an artifact and the plan's counters and blockers move with it.
#[tokio::test]
async fn accepting_an_artifact_moves_the_counters_and_shrinks_the_blockers() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "accept").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;

    let before = harness
        .call(get(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(before.status, StatusCode::OK, "{:?}", before.body);
    assert_eq!(
        before.body["counts"]["artifacts"], 9,
        "the fixture wrote nine artifacts"
    );
    // The fixture must produce a plan where **every** artifact validates, because the rest of
    // this walk counts on it: an `invalid` row is one the reviewer cannot accept, so a stray
    // one would make "nine pending" a lie and hide which row is at fault. Naming the offenders
    // rather than the number is the whole assertion — a count on its own says nothing about
    // *which* artifact needs fixing.
    let offenders: Vec<String> = before.body["artifacts"]
        .as_array()
        .expect("the detail must carry its artifacts")
        .iter()
        .filter(|artifact| artifact["status"] != "pending")
        .map(|artifact| {
            format!(
                "{} `{}` is {}: {}",
                artifact["kind"].as_str().unwrap_or_default(),
                artifact["key"].as_str().unwrap_or_default(),
                artifact["status"].as_str().unwrap_or_default(),
                artifact["validation"].to_string()
            )
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "every seeded artifact must pass its own validator, otherwise the counts below are \
         meaningless: {offenders:#?}"
    );
    assert_eq!(
        before.body["counts"]["pending"], 9,
        "nothing is decided before the reviewer starts"
    );
    assert_eq!(
        before.body["applicable"], false,
        "a plan of unresolved artifacts is not applicable"
    );
    assert_eq!(
        before.body["blockers"].as_array().map(Vec::len),
        Some(9),
        "every pending artifact is a named blocker"
    );

    let accepted = harness
        .call(post(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}/accept"),
            json!({}),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "{:?}", accepted.body);
    assert_eq!(
        accepted.body["artifact"]["status"], "accepted",
        "the accepted row answers as accepted, so the client never re-fetches to draw it"
    );
    assert_eq!(
        accepted.body["counts"]["accepted"], 1,
        "the counter moves in the same answer"
    );
    assert_eq!(
        accepted.body["blockers"].as_array().map(Vec::len),
        Some(8),
        "accepting one blocker removes exactly one"
    );
    let stored_status = status_of(&harness, report).await;
    assert_eq!(stored_status, "accepted");
}

/// A rejection with no reason is refused, and the artifact stays where it was.
#[tokio::test]
async fn a_rejection_without_a_reason_is_refused_and_the_row_stays_pending() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "reason").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;
    let uri = format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}/reject");

    // Missing body entirely, then a body whose reason is whitespace: two different ways of
    // saying nothing, and both must be refused before the write rather than after it.
    let no_body = harness
        .call(post(&uri, json!({}), Some(&tenant.token)))
        .await;
    assert_eq!(
        no_body.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a rejection with no reason is a decision nobody can learn from: {:?}",
        no_body.body
    );

    let blank = harness
        .call(post(&uri, json!({ "reason": "   " }), Some(&tenant.token)))
        .await;
    assert_eq!(
        blank.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "whitespace is not a reason: {:?}",
        blank.body
    );
    let stored = status_of(&harness, report).await;
    assert_eq!(
        stored, "pending",
        "a refused rejection must leave the artifact exactly where it was"
    );

    let refused = harness
        .call(post(
            &uri,
            json!({ "reason": "managers count days, not rows" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(refused.status, StatusCode::OK, "{:?}", refused.body);
    assert_eq!(refused.body["artifact"]["status"], "rejected");
    assert_eq!(
        refused.body["artifact"]["rejected_reason"], "managers count days, not rows",
        "the reason is stored beside the row, not only in the audit log"
    );
}

/// A regeneration retires the previous version with a **machine** reason, so the tree can
/// tell "the reviewer refused this" from "a newer version overtook this".
#[tokio::test]
async fn a_regeneration_keeps_the_previous_version_and_says_which_kind_of_rejection_it_was() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "regen").await;
    grant_reviewer(&harness, &tenant).await;

    let mock = MockProvider::start(Script::of(&[r#"{"artifact":{"key":"leave_summary","group_by":"status"},"rationale":"Counting status answers the question managers actually ask."}"#]))
        .await;
    connect(&harness, &tenant, &mock).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;

    let regenerated = harness
        .call(post(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}/regenerate"),
            json!({ "feedback": "group by status, not by days" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(regenerated.status, StatusCode::OK, "{:?}", regenerated.body);
    assert_eq!(
        mock.script.calls(),
        1,
        "one regeneration spends exactly one provider call"
    );

    let replacement = regenerated.body["artifact"]["id"]
        .as_str()
        .expect("the answer carries the replacement's id");
    assert_ne!(
        replacement,
        report.to_string(),
        "the regeneration writes a new row rather than overwriting"
    );
    assert_eq!(
        regenerated.body["artifact"]["status"], "pending",
        "a regenerated artifact comes back for review — the model cannot accept its own work"
    );

    let stored = status_of(&harness, report).await;
    assert_eq!(
        stored, "rejected",
        "the previous version is retired, not deleted"
    );
    let retired_reason: Option<String> =
        sqlx::query_scalar("select rejected_reason from app_builder_artifacts where id = $1")
            .bind(report)
            .fetch_one(harness.db.pool())
            .await
            .expect("the retired row must exist");
    assert_eq!(
        retired_reason.as_deref(),
        Some("superseded by a regenerated version"),
        "a machine retirement is distinguishable from a reviewer's refusal"
    );

    let rows: i64 =
        sqlx::query_scalar("select count(*) from app_builder_artifacts where plan_id = $1")
            .bind(plan_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the count must read");
    assert_eq!(
        rows, 10,
        "the plan keeps both versions, which is what the request asks for"
    );
}

/// An artifact the validator refused cannot be accepted, and the refusal names the finding.
#[tokio::test]
async fn an_invalid_artifact_cannot_be_accepted_and_the_refusal_names_the_finding() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "invalid").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;

    // Edit the report so it lands `invalid` — a key that is not a legal storage key.
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;
    let edited = harness
        .call(patch(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}"),
            json!({ "spec": { "key": "Leave Summary", "group_by": "days" } }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
    assert_eq!(
        edited.body["artifact"]["status"], "invalid",
        "an edit that still has findings is invalid, not edited — 'edited' reads as fixed"
    );
    assert_eq!(
        edited.body["artifact"]["validation"][0]["path"], "spec.key",
        "the finding names the field path the reviewer has to fix"
    );

    let accepted = harness
        .call(post(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}/accept"),
            json!({}),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        accepted.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "accepting a broken artifact is the one write that would make apply unsafe: {:?}",
        accepted.body
    );
    assert_eq!(
        accepted.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("leave_summary"),
        true,
        "the refusal names the artifact by name, not by id"
    );
    let stored = status_of(&harness, report).await;
    assert_eq!(stored, "invalid", "a refused acceptance changes nothing");

    // And the plan is still blocked, by name, because of it.
    let detail = harness
        .call(get(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&tenant.token),
        ))
        .await;
    let blockers = detail.body["blockers"]
        .as_array()
        .expect("blockers must be a list");
    let invalid_blocker = blockers
        .iter()
        .find(|blocker| blocker["key"] == "leave_summary")
        .expect("the invalid artifact is named in the blockers");
    assert_eq!(invalid_blocker["status"], "invalid");
    assert!(
        invalid_blocker["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "a blocker that is invalid carries the finding beside it"
    );
}

/// One tenant's plan is `404` to another, and contributes nothing to the other tenant's list.
#[tokio::test]
async fn another_tenants_plan_is_absent_rather_than_forbidden() {
    let harness = harness!();
    let first = tenant_with_tenant(&harness, "tenant-a").await;
    let second = other_tenant_with_tenant(&harness, "tenant-b").await;
    grant_reviewer(&harness, &first).await;
    grant_reviewer(&harness, &second).await;

    let plan_id = seed_plan(&harness, first.organization_id, first.user_id).await;

    let denied = harness
        .call(get(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&second.token),
        ))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::NOT_FOUND,
        "a plan of another tenant is absent, so 403 would tell the caller the id exists"
    );

    // Asserted on the LIST BODY rather than on a status code: "zero rows" is the property,
    // and a list that leaked one row would still answer 200.
    let list = harness
        .call(get("/api/v1/app-builder/plans", Some(&second.token)))
        .await;
    assert_eq!(list.status, StatusCode::OK, "{:?}", list.body);
    assert_eq!(
        list.body["plans"].as_array().map(Vec::len),
        Some(0),
        "the other tenant's plans do not appear in the list"
    );
    assert_eq!(list.body["total"], 0);

    // And a decision through the other tenant's id changes nothing.
    let report = artifact_id(&harness, plan_id, "report", "leave_summary").await;
    let attack = harness
        .call(post(
            &format!("/api/v1/app-builder/plans/{plan_id}/artifacts/{report}/accept"),
            json!({}),
            Some(&second.token),
        ))
        .await;
    assert_eq!(attack.status, StatusCode::NOT_FOUND);
    let stored = status_of(&harness, report).await;
    assert_eq!(
        stored, "pending",
        "an out-of-scope acceptance must leave the row untouched"
    );
}

/// A plan can be rejected whole, and the refusal names the status it could not be in.
#[tokio::test]
async fn a_plan_is_rejected_whole_with_its_reason_and_a_second_rejection_names_the_status() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "reject").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let uri = format!("/api/v1/app-builder/plans/{plan_id}/reject");

    let blank = harness
        .call(post(&uri, json!({ "reason": " " }), Some(&tenant.token)))
        .await;
    assert_eq!(blank.status, StatusCode::UNPROCESSABLE_ENTITY);

    let rejected = harness
        .call(post(
            &uri,
            json!({ "reason": "we already have an approval flow" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(rejected.status, StatusCode::OK, "{:?}", rejected.body);
    assert_eq!(rejected.body["plan"]["status"], "rejected");

    let stored: Option<String> =
        sqlx::query_scalar("select decision_reason from app_builder_plans where id = $1")
            .bind(plan_id)
            .fetch_one(harness.db.pool())
            .await
            .expect("the plan row must exist");
    assert_eq!(
        stored.as_deref(),
        Some("we already have an approval flow"),
        "the reason is on the row, so the list can show it without an audit reader"
    );

    // A second rejection finds no row: the status it is really in is the whole message,
    // because the operator's next move differs for each.
    let again = harness
        .call(post(
            &uri,
            json!({ "reason": "changed my mind" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);
    assert_eq!(again.body["error"]["code"], "plan_not_open");
    assert!(
        again.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("rejected")),
        "the conflict names the status the plan is in: {:?}",
        again.body
    );
}

/// A draft plan deletes; an applied one does not, and the two answers are told apart.
#[tokio::test]
async fn an_applied_plan_is_not_deletable_and_the_two_refusals_are_different() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "delete").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    let deleted = harness
        .call(delete(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{:?}", deleted.body);

    let gone = harness
        .call(delete(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        gone.status,
        StatusCode::NOT_FOUND,
        "a plan that is not there is a 404, not the applied-plan conflict"
    );

    // Now an applied plan. `applied_at` and `status` move together — the migration's check
    // refuses one without the other, so this is the only shape an applied row can take.
    let applied_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;
    sqlx::query(
        "update app_builder_plans set status = 'applied', applied_at = now() where id = $1",
    )
    .bind(applied_id)
    .execute(harness.db.pool())
    .await
    .expect("the plan must become applied");

    let refused = harness
        .call(delete(
            &format!("/api/v1/app-builder/plans/{applied_id}"),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an applied plan's artifacts are what the live app was built from: {:?}",
        refused.body
    );
    assert_eq!(refused.body["error"]["code"], "plan_applied");
}

/// The list's filters and the `examples` vocabulary are the landing page's own inputs.
#[tokio::test]
async fn the_list_filters_and_the_vocabulary_endpoint_answer_the_composer() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "list").await;
    grant_reviewer(&harness, &tenant).await;

    let plan_id = seed_plan(&harness, tenant.organization_id, tenant.user_id).await;

    let all = harness
        .call(get("/api/v1/app-builder/plans", Some(&tenant.token)))
        .await;
    assert_eq!(all.status, StatusCode::OK, "{:?}", all.body);
    assert_eq!(all.body["plans"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        all.body["plans"][0]["artifact_count"], 9,
        "the counts ride the list row rather than costing a query per plan"
    );
    assert_eq!(all.body["plans"][0]["title"], "Leave request management");

    let by_text = harness
        .call(get(
            "/api/v1/app-builder/plans?q=leave%20request",
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(by_text.body["total"], 1);

    let no_match = harness
        .call(get(
            "/api/v1/app-builder/plans?q=supplier%20contracts",
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        no_match.body["plans"].as_array().map(Vec::len),
        Some(0),
        "a filter that matches nothing answers an empty page, not every row"
    );

    let by_status = harness
        .call(get(
            "/api/v1/app-builder/plans?status=draft",
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(by_status.body["total"], 1);

    let mine = harness
        .call(get(
            "/api/v1/app-builder/plans?mine=true",
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(mine.body["total"], 1);

    // An unknown status is a `422` naming the vocabulary rather than an empty page that looks
    // like "you have no plans".
    let unknown = harness
        .call(get(
            "/api/v1/app-builder/plans?status=nonsense",
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);

    let vocabulary = harness
        .call(get("/api/v1/app-builder/examples", Some(&tenant.token)))
        .await;
    assert_eq!(vocabulary.status, StatusCode::OK, "{:?}", vocabulary.body);
    let examples = vocabulary.body["examples"]
        .as_array()
        .expect("the examples must be a list");
    assert_eq!(
        examples.len(),
        3,
        "the request names three sample prompts and the empty state offers all three"
    );
    assert!(
        examples.iter().all(|example| {
            !example["prompt"].as_str().unwrap_or_default().is_empty()
                && !example["title"].as_str().unwrap_or_default().is_empty()
        }),
        "no chip is a button that fills the composer with nothing"
    );
    assert_eq!(
        vocabulary.body["kinds"].as_array().map(Vec::len),
        Some(8),
        "all eight artifact kinds, in the order the tree groups them"
    );

    // The plan the list shows is the one the detail opens.
    let detail = harness
        .call(get(
            &format!("/api/v1/app-builder/plans/{plan_id}"),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(detail.body["plan"]["id"], plan_id.to_string());
    assert_eq!(
        detail.body["artifacts"].as_array().map(Vec::len),
        Some(9),
        "the tree is drawn from the detail's artifacts, not from a second request"
    );
}

/// `generate` writes the plan **before** the provider is asked, so a failed attempt is a row
/// the reviewer can read rather than a spinner that never ends.
/// A complete, well-formed answer: one artifact of every required kind, keys already in the
/// platform's spelling, so the walk measures the **generator** rather than its repairs.
const COMPLETE_PLAN: &str = r#"{
  "title": "Leave requests",
  "artifacts": [
    {"kind": "entity", "key": "leave_request",
     "spec": {"label": "Leave request", "plural_label": "Leave requests"},
     "rationale": "The request is about one kind of record: an employee's leave request."},
    {"kind": "field", "key": "leave_type", "parent_key": "leave_request",
     "spec": {"key": "leave_type", "label": "Leave type", "type": "enum",
              "options": ["annual", "sick"]},
     "rationale": "A request is classified by the kind of leave it asks for."},
    {"kind": "field", "key": "days", "parent_key": "leave_request",
     "spec": {"key": "days", "label": "Days", "type": "integer"},
     "rationale": "How many days are taken decides whether a manager must approve it."},
    {"kind": "ui", "key": "leave_request_list", "parent_key": "leave_request",
     "spec": {"screen": "list", "columns": ["leave_type", "days"]},
     "rationale": "A manager approves from a list, so the list is the entry point."},
    {"kind": "permission", "key": "leave_request.read", "parent_key": "leave_request",
     "spec": {"key": "leave_request.read", "description": "Read leave requests"},
     "rationale": "Reading the app is the base capability and nothing works without it."},
    {"kind": "role", "key": "leave_approver",
     "spec": {"name": "Leave approver", "permissions": ["leave_request.read"]},
     "rationale": "Somebody decides, and that somebody is a role rather than every user."},
    {"kind": "workflow", "key": "leave_approval", "parent_key": "leave_request",
     "spec": {"trigger": "record.created",
              "steps": [{"name": "Notify manager", "action": "notify"}]},
     "rationale": "A request nobody is told about is a request nobody approves."},
    {"kind": "notification", "key": "leave_requested", "parent_key": "leave_approval",
     "spec": {"title": "Leave requested", "body": "A leave request needs approval",
              "channel": "in_app"},
     "rationale": "The workflow fires an event; the template is what a person reads."},
    {"kind": "report", "key": "leave_request_summary", "parent_key": "leave_request",
     "spec": {"title": "Leave requests", "group_by": "leave_type", "metric": "count"},
     "rationale": "HR asks how much leave was taken, by type."}
  ]
}"#;

#[tokio::test]
async fn a_prompt_becomes_a_plan_of_stored_artifacts_and_the_stream_names_each_one() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "generate").await;
    grant_reviewer(&harness, &tenant).await;

    let mock = MockProvider::start(Script::of(&[COMPLETE_PLAN])).await;
    connect(&harness, &tenant, &mock).await;

    let response = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "Create an app to manage employees' leave requests" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "generation answers a stream, not a 201: {:?}",
        response.body
    );

    // The provider is asked **once**. A second call would double the cost of a plan that was
    // merely mis-spelled, and every repair this platform makes is visible in the artifact's
    // rationale — so a walk that counts calls is also the walk that proves there is no
    // silent repair loop.
    assert_eq!(mock.script.calls(), 1, "one prompt, one provider call");

    // Every artifact landed, and it landed in the database — not just on the stream.
    let plan_id: Uuid = sqlx::query_scalar("select id from app_builder_plans limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the plan row must exist");
    let stored: Vec<(String, String, String)> = sqlx::query_as(
        "select kind, key, status from app_builder_artifacts
          where plan_id = $1 order by ordinal, kind",
    )
    .bind(plan_id)
    .fetch_all(harness.db.pool())
    .await
    .expect("the artifacts must read back");
    assert_eq!(
        stored.len(),
        9,
        "every artifact the answer proposed is a row a reviewer can open: {stored:?}"
    );
    for kind in [
        "entity",
        "field",
        "ui",
        "permission",
        "workflow",
        "notification",
        "report",
    ] {
        assert!(
            stored.iter().any(|(k, _, _)| k == kind),
            "the `{kind}` artifact is stored"
        );
    }

    // The plan settled as a **draft**: validated, not reviewed. `approved` here would let a
    // generator approve its own work, which the whole two-act design exists to prevent.
    let status: String = sqlx::query_scalar("select status from app_builder_plans where id = $1")
        .bind(plan_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the plan row must exist");
    assert_eq!(
        status, "draft",
        "a generated plan is a draft; nothing is approved without a person"
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "select error from app_builder_plans where id = $1"
        )
        .bind(plan_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the error column must read"),
        None,
        "a successful generation leaves no error on the plan"
    );

    // The model named the plan, and the name is the model's own.
    let title: String = sqlx::query_scalar("select title from app_builder_plans where id = $1")
        .bind(plan_id)
        .fetch_one(harness.db.pool())
        .await
        .expect("the title must read");
    assert_eq!(title, "Leave requests");

    // The stream told the reviewer what landed, one frame per artifact, and ended with the
    // plan id — the acceptance criterion "artifacts stream into the tree", measured on the
    // wire rather than inferred from the row count.
    assert!(
        response.text.contains("event: artifact"),
        "each artifact is announced while the stream is open: {}",
        response.text
    );
    assert!(
        response.text.contains("event: done"),
        "the stream ends with a terminal frame, never a spinner that cannot be ended: {}",
        response.text
    );
    assert!(
        !response.text.contains("event: error"),
        "a complete answer produces no error frame: {}",
        response.text
    );
}

#[tokio::test]
async fn a_plan_that_is_missing_a_required_kind_still_lands_and_names_what_is_absent() {
    // The interesting half. A plan with no report is a plan a reviewer must be *told* about,
    // and the difference between "the generator produced something" and "the generator
    // produced a complete application" is exactly the missing-kind list.
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "partial").await;
    grant_reviewer(&harness, &tenant).await;

    let partial = r#"{
      "title": "Half an app",
      "artifacts": [
        {"kind": "entity", "key": "vehicle", "spec": {"label": "Vehicle"},
         "rationale": "The request is about vehicles."},
        {"kind": "field", "key": "plate", "parent_key": "vehicle",
         "spec": {"key": "plate", "label": "Plate", "type": "text"},
         "rationale": "A vehicle is identified by its plate."}
      ]
    }"#;
    let mock = MockProvider::start(Script::of(&[partial])).await;
    connect(&harness, &tenant, &mock).await;

    let response = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "Track company vehicles" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    // The two artifacts that were proposed are kept: they are inert drafts a reviewer can
    // keep working on, and dropping them would throw away the answer for being incomplete.
    let count: i64 = sqlx::query_scalar("select count(*) from app_builder_artifacts")
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(count, 2, "the artifacts that were proposed are still there");

    // And the gap is named on the wire, kind by kind — not as a count.
    assert!(
        response.text.contains("report") && response.text.contains("workflow"),
        "the terminal frame names the kinds the answer left out: {}",
        response.text
    );

    // The plan is a draft, and apply is still blocked: the store's own answer, read from the
    // database rather than from the screen that drew it.
    let plan_id: Uuid = sqlx::query_scalar("select id from app_builder_plans limit 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("the plan row must exist");
    let applicable: bool = sqlx::query_scalar(
        "select exists (
             select 1 from unnest($1::text[]) required
              where not exists (
                    select 1 from app_builder_artifacts
                     where plan_id = $2 and kind = required
              )
         ) is not true",
    )
    .bind(
        [
            "entity",
            "field",
            "ui",
            "permission",
            "workflow",
            "notification",
            "report",
        ]
        .map(str::to_owned),
    )
    .bind(plan_id)
    .fetch_one(harness.db.pool())
    .await
    .expect("the applicability must read");
    assert!(
        !applicable,
        "a plan missing required kinds is not applicable — this is the same question the \
         blockers list answers"
    );
}

#[tokio::test]
async fn a_mis_spelled_key_is_repaired_onto_the_artifact_and_the_repair_is_readable() {
    // The repair the generator performs, measured from the database. A silently corrected
    // key is a plan the reviewer approved under a name they never saw.
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "repair").await;
    grant_reviewer(&harness, &tenant).await;

    let sloppy = r#"{
      "title": "Leave requests",
      "artifacts": [
        {"kind": "entity", "key": "Leave Request", "spec": {"label": "Leave request"},
         "rationale": "One kind of record."},
        {"kind": "field", "key": "Days", "parent_key": "Leave Request",
         "spec": {"key": "Days", "label": "Days", "type": "Int"},
         "rationale": "How many days are taken."}
      ]
    }"#;
    let mock = MockProvider::start(Script::of(&[sloppy])).await;
    connect(&harness, &tenant, &mock).await;

    let response = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "Create an app to manage employees' leave requests" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let rows: Vec<(String, String, String)> =
        sqlx::query_as("select kind, key, rationale from app_builder_artifacts order by kind")
            .fetch_all(harness.db.pool())
            .await
            .expect("the artifacts must read back");
    let entity = rows
        .iter()
        .find(|(kind, _, _)| kind == "entity")
        .expect("the entity is stored");
    assert_eq!(
        entity.1, "leave_request",
        "the key is repaired into the platform's spelling"
    );
    assert!(
        entity
            .2
            .contains("`Leave Request` was read as `leave_request`"),
        "and the repair is in the rationale the reviewer reads: {}",
        entity.2
    );

    let field = rows
        .iter()
        .find(|(kind, _, _)| kind == "field")
        .expect("the field is stored");
    assert!(
        field.2.contains("`Int` was read as `integer`"),
        "a changed field type is the repair that changes behaviour, so it is stated: {}",
        field.2
    );
    // The parent was repaired too, or the field would hang off a parent that does not exist.
    let parent: Option<String> = sqlx::query_scalar(
        "select parent_key from app_builder_artifacts where kind = 'field' limit 1",
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("the parent must read");
    assert_eq!(
        parent.as_deref(),
        Some("leave_request"),
        "a repaired key and its repaired parent agree — otherwise the field hangs off \
         nothing"
    );
}

#[tokio::test]
async fn an_answer_that_is_not_a_plan_fails_the_plan_with_the_reason_on_the_row() {
    // A model that answers in prose is not a plan, and the reviewer's console must say so
    // on the plan row — not just as a frame that scrolled past.
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "unreadable").await;
    grant_reviewer(&harness, &tenant).await;

    let mock = MockProvider::start(Script::of(&[
        "Sure! Here is an application you could build. First, create an entity called ...",
    ]))
    .await;
    connect(&harness, &tenant, &mock).await;

    let response = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "Create an app to manage employees' leave requests" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK, "a stream, still");

    let (status, error): (String, Option<String>) =
        sqlx::query_as("select status, error from app_builder_plans limit 1")
            .fetch_one(harness.db.pool())
            .await
            .expect("the plan row must exist");
    assert_eq!(
        status, "failed",
        "an unreadable answer is a failed plan, never a draft that reviews as empty"
    );
    let error = error.unwrap_or_default();
    assert!(
        error.contains("did not answer with a JSON object"),
        "and the row names the reason: {error}"
    );
    assert!(
        response.text.contains("ai_provider_unreadable_answer"),
        "the stream carries the same stable code: {}",
        response.text
    );
}

#[tokio::test]
async fn a_refused_prompt_writes_no_plan_and_a_missing_provider_is_a_409() {
    let harness = harness!();
    let tenant = tenant_with_tenant(&harness, "refusals").await;
    grant_reviewer(&harness, &tenant).await;

    let mock = MockProvider::start(Script::of(&[])).await;
    connect(&harness, &tenant, &mock).await;

    // A prompt too short is refused **before** the row is written and before the provider is
    // asked: a refused request must not cost a call or litter the console with attempts.
    let before: i64 = sqlx::query_scalar("select count(*) from app_builder_plans")
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must read");
    let short = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "x" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST, "{:?}", short.body);
    let after: i64 = sqlx::query_scalar("select count(*) from app_builder_plans")
        .fetch_one(harness.db.pool())
        .await
        .expect("the count must read");
    assert_eq!(
        before, after,
        "a refused prompt writes no plan, so the list is not littered with empty attempts"
    );
    assert_eq!(
        mock.script.calls(),
        0,
        "and it spends no provider call: the refusal is before the model is asked"
    );

    // And with no provider connected at all, the composer gets a `409` it can render.
    sqlx::query("update ai_providers set enabled = false")
        .execute(harness.db.pool())
        .await
        .expect("the provider must be switchable off");
    let no_provider = harness
        .call(post(
            "/api/v1/app-builder/generate",
            json!({ "prompt": "Track supplier contracts with renewal reminders" }),
            Some(&tenant.token),
        ))
        .await;
    assert_eq!(
        no_provider.status,
        StatusCode::CONFLICT,
        "'no model is configured' is a 409 with a remedy, not a 500: {:?}",
        no_provider.body
    );
    assert_eq!(no_provider.body["error"]["code"], "ai_no_model_available");
}
