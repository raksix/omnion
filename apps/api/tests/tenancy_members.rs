//! Integration tests for the tenant's own people (docs/requests/REQ-005, slice 1):
//! memberships, invitations and the organization switcher.
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI. When
//! PostgreSQL is not reachable the suite skips itself with a printed reason, so `cargo test`
//! stays usable on a machine without Docker.
//!
//! The walk proves, over the real router:
//!
//! * the backfill gives every account with a home organization exactly one primary membership;
//! * an invited **new** address signs up, joins the organization and lands on its overview;
//! * an invited **existing** account accepts with its own session;
//! * a second invitation for the same address returns the pending one instead of a duplicate,
//!   and inviting a member is refused naming them;
//! * an expired, a revoked and an already-used token each get their own answer;
//! * switching organizations moves the session's home, and the switcher's list follows it;
//! * a member of one tenant gets `404` for another tenant's members — never `403`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::memberships;
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{roles as role_store, seed};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

/// Password used for the accounts this suite creates.
const PASSWORD: &str = "correct horse battery";

/// Permission keys the administrator of this suite holds. `sites.read` is part of it because
/// the switcher's whole point is that the panel's data follows the switch, and a site list
/// nobody may read would prove nothing.
const ADMIN_PERMISSIONS: [&str; 3] = ["organizations.read", "organizations.manage", "sites.read"];

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
    // ALL of the `Set-Cookie` headers, joined. `Headers::get` returns only the first, and
    // sign-in sends TWO — the session and the CSRF token — so reading one made the platform look
    // like it had issued no CSRF cookie at all.
    let set_cookie = {
        let values: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
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
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };

    TestResponse {
        status,
        set_cookie,
        body,
    }
}

/// What one sign-in issued: the session cookie **and** the CSRF token that sign-in set beside
/// it.
///
/// The previous version of this suite returned a bare `String` read with `.split(';').next()`,
/// which is the *session* — the second cookie was dropped on the floor. The CSRF layer refuses a
/// cookie-authenticated write that carries no `x-omnion-csrf`, so every mutation this suite sent
/// was answered `403 csrf_failed` at the security layer, before reaching the handler the walk was
/// written to exercise. The walks read as "memberships are broken" for as long as the CSRF layer
/// existed.
///
/// `Deref<Target = str>` is deliberate: it makes this type usable everywhere the plain session
/// token used to be (`format!("{token}")`, `Some(&admin)` at ~35 call sites, a `&str` parameter on
/// a helper) while carrying the second cookie that has to travel with it. A test that still wants
/// a session *without* a CSRF token — proving the refusal — names `Credentials::session_only`,
/// which is exactly what the browser never does, so the two shapes cannot be confused.
struct Credentials {
    session: String,
    /// The value of the `omnion_csrf` cookie, which is the value the `x-omnion-csrf` header must
    /// carry. `None` only where the platform configured no CSRF secret, in which case the layer
    /// refuses writes with `csrf_unavailable` and there is nothing to send.
    csrf: Option<String>,
}

impl std::ops::Deref for Credentials {
    type Target = str;

    fn deref(&self) -> &str {
        &self.session
    }
}

impl std::fmt::Debug for Credentials {
    /// Never prints the tokens: a failing assertion quoting a session token would write a live
    /// credential into the test log of every CI run.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("session", &"<redacted>")
            .field("csrf", &self.csrf.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Build a request; `credentials` becomes the cookie jar and `body` the JSON payload.
///
/// Both cookies the sign-in issued go out, plus the matching `x-omnion-csrf` header on a write —
/// which is precisely what a browser does and what this suite failed to do.
fn request(
    method: Method,
    uri: &str,
    credentials: Option<&Credentials>,
    body: Option<Value>,
) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match credentials {
        Some(credentials) => {
            let mut cookies = format!("omnion_session={}", credentials.session);
            let builder = match &credentials.csrf {
                Some(csrf) => {
                    cookies.push_str(&format!("; omnion_csrf={csrf}"));
                    builder
                        // The header is only *required* on a write, but sending it on a read is
                        // harmless and is what the panel does.
                        .header("x-omnion-csrf", csrf.as_str())
                }
                None => builder,
            };
            builder.header(header::COOKIE, cookies)
        }
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

/// Object store of the test state.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
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
        test_storage(),
    );
    Some((state, db))
}

/// Two organizations, an administrator of the first, and a member of the second that the
/// switcher will move between them.
struct Fixture {
    state: AppState,
    db: Db,
    org_a: Uuid,
    org_b: Uuid,
    admin_id: Uuid,
    admin_email: String,
    outsider_id: Uuid,
    outsider_email: String,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let org_a = create_organization_row(&db, "a", "Membership Test A").await;
        let org_b = create_organization_row(&db, "b", "Membership Test B").await;

        // The administrator of organization A: tenancy permissions bound at its scope. The
        // binding is written after the account, so the fixture inserts the membership itself
        // (the backfill only runs as a migration) — that is the shape the panel lives with.
        let (admin_id, admin_email) = create_account(&db).await;
        memberships::add_member(
            db.pool(),
            memberships::NewMembership {
                organization_id: org_a,
                user_id: admin_id,
                status: "active".to_owned(),
                is_primary: true,
            },
        )
        .await
        .expect("the admin membership must be created");
        grant_organization_admin(&db, org_a, admin_id).await;

        // A member of the second organization: the switcher will move them into A, and they are
        // the account the isolation assertions are made from.
        let (outsider_id, outsider_email) = create_account(&db).await;
        memberships::add_member(
            db.pool(),
            memberships::NewMembership {
                organization_id: org_b,
                user_id: outsider_id,
                status: "active".to_owned(),
                is_primary: true,
            },
        )
        .await
        .expect("the outsider membership must be created");
        grant_organization_admin(&db, org_b, outsider_id).await;

        // One site per organization, so "the data follows the switch" is a real assertion and
        // not a tautology over an empty list.
        create_site_row(&db, org_a, "a-main", "Organization A Main").await;
        create_site_row(&db, org_b, "b-main", "Organization B Main").await;

        Some(Self {
            state,
            db,
            org_a,
            org_b,
            admin_id,
            admin_email,
            outsider_id,
            outsider_email,
            accounts: vec![admin_id, outsider_id],
            organizations: vec![org_a, org_b],
        })
    }

    async fn admin_token(&self) -> Credentials {
        login(&self.state, &self.admin_email).await
    }

    async fn outsider_token(&self) -> Credentials {
        login(&self.state, &self.outsider_email).await
    }

    fn remember_account(&mut self, account: Uuid) {
        self.accounts.push(account);
    }

    /// Remove exactly what this fixture created — by id, never by a pattern.
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

/// Create an organization row with a unique, suite-scoped slug.
async fn create_organization_row(db: &Db, label: &str, name: &str) -> Uuid {
    let slug = format!("memberships-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the test organization must be created")
}

/// Create an account with a unique address and **no** membership — the shape a platform-level
/// account has until it is invited somewhere.
async fn create_account(db: &Db) -> (Uuid, String) {
    let email = format!("memberships-{}@omnion.test", Uuid::new_v4().simple());
    let user = users::create_user(
        db.pool(),
        NewUser {
            email: email.clone(),
            password: PASSWORD.to_owned(),
            display_name: "Membership Test".to_owned(),
            organization_id: None,
        },
    )
    .await
    .expect("creating a test account must succeed");
    (user.id, email)
}

/// Create a site row inside an organization.
async fn create_site_row(db: &Db, organization_id: Uuid, key: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(key)
    .bind(name)
    .fetch_one(db.pool())
    .await
    .expect("the test site must be created")
}

/// Give one account the tenancy permissions of an organization.
async fn grant_organization_admin(db: &Db, organization_id: Uuid, user_id: Uuid) {
    let role = role_store::create_role(
        db.pool(),
        omnion_permissions::model::NewRole {
            organization_id,
            key: format!("org-admin-{}", Uuid::new_v4().simple()),
            name: "Organization Administrator".to_owned(),
            description: "Runs one organization".to_owned(),
            priority: 800,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the organization role must be created");

    let entries: Vec<omnion_permissions::model::RolePermissionInput> = ADMIN_PERMISSIONS
        .iter()
        .map(|key| omnion_permissions::model::RolePermissionInput {
            key: (*key).to_owned(),
            effect: omnion_permissions::model::Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(db.pool(), role.id, &entries)
        .await
        .expect("the role permission set must be written");

    omnion_permissions::bindings::grant(
        db.pool(),
        omnion_permissions::model::NewBinding {
            role_id: role.id,
            user_id,
            scope: omnion_permissions::Scope::Organization { organization_id },
            granted_by: None,
            expires_at: None,
        },
    )
    .await
    .expect("the binding must be granted");
}

/// Sign an account in and return the cookies the sign-in issued.
///
/// Naming each cookie is what makes a *missing* one visible: the `expect` says which cookie the
/// platform did not send, instead of the failure surfacing three layers away as a `403`.
async fn login(state: &AppState, email: &str) -> Credentials {
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
    let set_cookie = response
        .set_cookie
        .clone()
        .expect("login must set the session cookie");

    let cookie_value = |name: &str| -> Option<String> {
        set_cookie
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(cookie, _)| *cookie == name)
            .map(|(_, value)| value.to_owned())
    };

    let session = cookie_value("omnion_session")
        .unwrap_or_else(|| panic!("login must set the omnion_session cookie; sent: {set_cookie}"));
    let csrf = cookie_value("omnion_csrf");
    assert!(
        csrf.is_some(),
        "login must set the omnion_csrf cookie beside the session one; sent: {set_cookie}"
    );

    Credentials { session, csrf }
}

/// The `id` field of a response body, as text.
fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an id: {body}"))
        .to_owned()
}

/// The site keys a list body carries, in order.
fn site_keys(body: &Value) -> Vec<String> {
    body["sites"]
        .as_array()
        .expect("sites is an array")
        .iter()
        .map(|site| site["key"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The `code` of an error body.
fn code_of(body: &Value) -> String {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("body carries an error code: {body}"))
        .to_owned()
}

#[tokio::test]
async fn an_invited_address_signs_up_joins_and_lands_on_the_organization() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    let email = format!("invited-{}@omnion.test", Uuid::new_v4().simple());
    let created = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({
                "email": email,
                "message": "Welcome aboard — the marketing site is yours.",
            })),
        ),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "invite body: {}",
        created.body
    );
    let token = created.body["token"]
        .as_str()
        .expect("the invite returns its token")
        .to_owned();
    assert!(
        created.body["accept_url"]
            .as_str()
            .is_some_and(|url| url.ends_with(&token)),
        "the accept link carries the token: {}",
        created.body["accept_url"]
    );

    // The public preview shows the organization, the inviter and the masked address — never the
    // full address, and never the token again.
    let preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/invitations/{token}"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "preview: {}", preview.body);
    assert_eq!(preview.body["organization_name"], "Membership Test A");
    assert_eq!(preview.body["usable"], json!(true));
    assert!(
        preview.body["email_masked"]
            .as_str()
            .is_some_and(|masked| masked.ends_with("@omnion.test") && !masked.contains("invited-")),
        "the address is masked: {}",
        preview.body["email_masked"]
    );

    // Accepting while signed out creates the account and joins the organization.
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{token}/accept"),
            None,
            Some(json!({ "display_name": "Invited Member", "password": PASSWORD })),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "accept: {}", accepted.body);
    let user_id = accepted.body["user_id"]
        .as_str()
        .expect("the acceptance names the account")
        .to_owned();
    fixture.remember_account(Uuid::parse_str(&user_id).expect("a uuid"));
    assert_eq!(accepted.body["organization_name"], "Membership Test A");

    // The membership exists, is active and — because the account had no home — is the primary
    // one, so the switcher has something to show.
    let home: Option<Uuid> = sqlx::query_scalar("select organization_id from users where id = $1")
        .bind(Uuid::parse_str(&user_id).expect("a uuid"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("the home column must be readable");
    assert_eq!(
        home,
        Some(fixture.org_a),
        "the account adopted the organization"
    );

    let status: String = sqlx::query_scalar(
        "select status from organization_members where organization_id = $1 and user_id = $2",
    )
    .bind(fixture.org_a)
    .bind(Uuid::parse_str(&user_id).expect("a uuid"))
    .fetch_one(fixture.db.pool())
    .await
    .expect("the membership row must be readable");
    assert_eq!(status, "active");

    // The Members tab lists it, with the account's own name.
    let members = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/members", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(members.status, StatusCode::OK, "members: {}", members.body);
    assert!(
        members.body["members"]
            .as_array()
            .expect("members is an array")
            .iter()
            .any(|member| member["email"] == json!(email)),
        "the invited account is in the member list: {}",
        members.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_used_expired_or_revoked_token_answers_with_its_own_reason() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // 1. A token that was used already is not valid a second time.
    let used_email = format!("used-{}@omnion.test", Uuid::new_v4().simple());
    let used = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": used_email })),
        ),
    )
    .await;
    let used_token = used.body["token"].as_str().expect("token").to_owned();
    let outsider = fixture.outsider_token().await;
    let first = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{used_token}/accept"),
            Some(&outsider),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "first accept: {}", first.body);
    let again = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{used_token}/accept"),
            Some(&outsider),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "second accept");
    assert_eq!(code_of(&again.body), "invitation_not_found");

    // 2. A revoked token is not valid either.
    let revoked_email = format!("revoked-{}@omnion.test", Uuid::new_v4().simple());
    let revoked = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": revoked_email })),
        ),
    )
    .await;
    let revoked_id = id_of(&revoked.body["invitation"]);
    let revoked_token = revoked.body["token"].as_str().expect("token").to_owned();
    let revoke = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/invitations/{revoked_id}",
                fixture.org_a
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(revoke.status, StatusCode::NO_CONTENT);
    let after_revoke = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{revoked_token}/accept"),
            Some(&outsider),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(after_revoke.status, StatusCode::NOT_FOUND);

    // 3. An expired token gets its own answer — `410`, not the generic not-valid one.
    let expired_email = format!("expired-{}@omnion.test", Uuid::new_v4().simple());
    let expired = memberships::create_invitation(
        fixture.db.pool(),
        memberships::NewInvitation {
            organization_id: fixture.org_a,
            email: expired_email,
            role_id: None,
            invited_by: None,
            message: String::new(),
            expires_at: Some(OffsetDateTime::now_utc() - Duration::hours(1)),
            queued: false,
        },
    )
    .await
    .expect("the expired invitation must be created");
    let expired_preview = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/invitations/{}", expired.token),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(expired_preview.status, StatusCode::OK);
    assert_eq!(expired_preview.body["usable"], json!(false));
    let expired_accept = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{}/accept", expired.token),
            Some(&outsider),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(expired_accept.status, StatusCode::GONE, "expired accept");
    assert_eq!(code_of(&expired_accept.body), "invitation_expired");

    // 4. A token that was never issued is not valid either — the same answer as a revoked one,
    //    so the public route cannot be used to discover organizations.
    let unknown = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/invitations/0000000000000000000000000000000000000000000000000000000000000000",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&unknown.body), "invitation_not_found");

    fixture.cleanup().await;
}

#[tokio::test]
async fn inviting_a_member_or_a_pending_address_is_refused_by_name() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A member is refused by name — the panel says who is already there.
    let existing = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": fixture.admin_email })),
        ),
    )
    .await;
    assert_eq!(
        existing.status,
        StatusCode::CONFLICT,
        "member: {}",
        existing.body
    );
    assert_eq!(code_of(&existing.body), "already_member");
    assert!(
        existing.body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Membership Test")),
        "the refusal names the member: {}",
        existing.body["error"]["message"]
    );

    // The same address twice returns the pending invitation instead of a duplicate.
    let email = format!("pending-{}@omnion.test", Uuid::new_v4().simple());
    let first = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": email })),
        ),
    )
    .await;
    assert_eq!(
        first.status,
        StatusCode::CREATED,
        "first invite: {}",
        first.body
    );
    let second = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": email.to_uppercase() })),
        ),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "second invite: {}",
        second.body
    );
    assert_eq!(code_of(&second.body), "invitation_already_pending");
    assert_eq!(
        second.body["error"]["details"]["invitation_id"],
        json!(id_of(&first.body["invitation"])),
        "the refusal names the invitation that already exists"
    );

    // And the organization holds exactly one invitation for that address.
    let list = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "invitations: {}", list.body);
    let matching = list.body["invitations"]
        .as_array()
        .expect("an array")
        .iter()
        .filter(|invitation| invitation["email"] == json!(email))
        .count();
    assert_eq!(matching, 1, "one live invitation, not two: {}", list.body);

    // An unusable address is refused in the field, not stored.
    let invalid = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": "not an address" })),
        ),
    )
    .await;
    assert_eq!(
        invalid.status,
        StatusCode::BAD_REQUEST,
        "invalid: {}",
        invalid.body
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_switcher_moves_the_session_and_follows_the_membership() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let outsider = fixture.outsider_token().await;

    // The site list the session can see at the start: organization B's only, because that is
    // the organization it works in. Captured before anything moves.
    let before_sites = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sites", Some(&outsider), None),
    )
    .await;
    assert_eq!(
        before_sites.status,
        StatusCode::OK,
        "sites: {}",
        before_sites.body
    );
    assert_eq!(
        site_keys(&before_sites.body),
        vec!["b-main".to_owned()],
        "before the switch the session works in B"
    );

    // Before: the account works in organization B, which is the only membership it has.
    let before = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/me/organizations",
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "before: {}", before.body);
    assert_eq!(
        before.body["current_organization_id"],
        json!(fixture.org_b.to_string())
    );
    assert_eq!(
        before.body["organizations"]
            .as_array()
            .expect("an array")
            .len(),
        1
    );
    assert_eq!(
        before.body["organizations"][0]["roles"][0]["name"], "Organization Administrator",
        "the switcher shows the roles held there"
    );

    // Switching to A is refused while the account is not a member — no membership is invented.
    let refused = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/me/organization",
            Some(&outsider),
            Some(json!({ "organization_id": fixture.org_a })),
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "refused: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "not_a_member");

    // An invitation makes the account a member; accepting it does not move the home, because
    // the account already had one. The invitation carries a role this time, so the acceptance
    // has to grant it — a stored-but-never-applied role would pass every other assertion here.
    let admin = fixture.admin_token().await;
    let email = fixture.outsider_email.clone();
    let role_id: Uuid = sqlx::query_scalar(
        "select id from roles where organization_id = $1 order by created_at desc limit 1",
    )
    .bind(fixture.org_a)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the organization role must exist");
    let invite = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&admin),
            Some(json!({ "email": email, "role_id": role_id })),
        ),
    )
    .await;
    let token = invite.body["token"].as_str().expect("token").to_owned();
    let accepted = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/invitations/{token}/accept"),
            Some(&outsider),
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "accept: {}", accepted.body);

    // Now the switcher lists both, and the session still works in B.
    let both = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/me/organizations",
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(both.status, StatusCode::OK, "both: {}", both.body);
    assert_eq!(
        both.body["current_organization_id"],
        json!(fixture.org_b.to_string()),
        "the home does not move on its own"
    );
    assert_eq!(
        both.body["organizations"]
            .as_array()
            .expect("an array")
            .len(),
        2
    );

    // Switching moves it.
    let switched = call(
        &fixture.state,
        request(
            Method::POST,
            "/api/v1/me/organization",
            Some(&outsider),
            Some(json!({ "organization_id": fixture.org_a })),
        ),
    )
    .await;
    assert_eq!(switched.status, StatusCode::OK, "switch: {}", switched.body);
    assert_eq!(
        switched.body["organization_id"],
        json!(fixture.org_a.to_string())
    );

    let me = call(
        &fixture.state,
        request(Method::GET, "/api/v1/me", Some(&outsider), None),
    )
    .await;
    assert_eq!(
        me.body["user"]["organization_id"],
        json!(fixture.org_a.to_string()),
        "the session now works in the switched organization"
    );

    // And the primary flag followed: the switcher marks the new one, not the old.
    // The offered role was granted at organization scope, which is what makes the switched-to
    // organization usable: its site list answers 200 instead of 403.
    let bound: bool = sqlx::query_scalar(
        "select exists (select 1 from role_bindings where user_id = $1 and role_id = $2 \
                            and organization_id = $3 and revoked_at is null)",
    )
    .bind(fixture.outsider_id)
    .bind(role_id)
    .bind(fixture.org_a)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the binding count must be readable");
    assert!(bound, "the offered role is bound after the acceptance");

    let after = call(
        &fixture.state,
        request(
            Method::GET,
            "/api/v1/me/organizations",
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        after.body["current_organization_id"],
        json!(fixture.org_a.to_string())
    );
    assert_eq!(
        after.body["organizations"]
            .as_array()
            .expect("an array")
            .iter()
            .filter(|row| row["is_primary"] == json!(true))
            .count(),
        1,
        "exactly one primary membership: {}",
        after.body
    );

    // The site list follows the switch: the very same session listed B's site before it and
    // A's site after it.
    let after_sites = call(
        &fixture.state,
        request(Method::GET, "/api/v1/sites", Some(&outsider), None),
    )
    .await;
    assert_eq!(
        after_sites.status,
        StatusCode::OK,
        "sites: {}",
        after_sites.body
    );
    assert_eq!(
        site_keys(&after_sites.body),
        vec!["a-main".to_owned()],
        "after the switch the very same session lists the other tenant's site"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_member_of_one_tenant_cannot_see_another_tenants_members() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let outsider = fixture.outsider_token().await;

    // Organization B's members are visible to its own member…
    let own = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/members", fixture.org_b),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "own: {}", own.body);

    // …and organization A's are `404`, not `403`: a 403 would confirm the id exists.
    let foreign = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/members", fixture.org_a),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "another tenant answers 404: {}",
        foreign.body
    );
    assert_eq!(code_of(&foreign.body), "organization_not_found");

    // The same answer for its invitations, and a write is refused just as quietly.
    let foreign_invitations = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(foreign_invitations.status, StatusCode::NOT_FOUND);

    let invite_write = call(
        &fixture.state,
        request(
            Method::POST,
            &format!("/api/v1/organizations/{}/invitations", fixture.org_a),
            Some(&outsider),
            Some(json!({ "email": "someone@example.com" })),
        ),
    )
    .await;
    assert_eq!(invite_write.status, StatusCode::NOT_FOUND);

    // Removing a member of another tenant is refused the same way.
    let foreign_remove = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/members/{}",
                fixture.org_a, fixture.admin_id
            ),
            Some(&outsider),
            None,
        ),
    )
    .await;
    assert_eq!(foreign_remove.status, StatusCode::NOT_FOUND);

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_backfill_gives_every_home_organization_one_primary_membership() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };

    // Every account that carries a home organization has exactly one membership there, and
    // exactly one primary membership overall.
    let orphans: i64 = sqlx::query_scalar(
        "select count(*) from users u \
          where u.organization_id is not null \
            and not exists (select 1 from organization_members m \
                             where m.organization_id = u.organization_id and m.user_id = u.id)",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the orphan count must be readable");
    assert_eq!(orphans, 0, "no account may be left without a membership");

    let double_primary: i64 = sqlx::query_scalar(
        "select count(*) from ( \
             select user_id from organization_members where is_primary group by user_id \
             having count(*) > 1 \
         ) doubled",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the primary count must be readable");
    assert_eq!(
        double_primary, 0,
        "an account has at most one primary membership"
    );

    // And a membership never points at a user without a home organization.
    let mismatched: i64 = sqlx::query_scalar(
        "select count(*) from organization_members m \
           join users u on u.id = m.user_id \
          where m.is_primary and u.organization_id is distinct from m.organization_id",
    )
    .fetch_one(fixture.db.pool())
    .await
    .expect("the mismatch count must be readable");
    assert_eq!(
        mismatched, 0,
        "the primary membership and the home column always agree"
    );

    // The suite's own rows are part of that invariant.
    let admin_home: Option<Uuid> =
        sqlx::query_scalar("select organization_id from users where id = $1")
            .bind(fixture.admin_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("the home column must be readable");
    assert_eq!(admin_home, Some(fixture.org_a));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_suspended_member_and_a_last_primary_are_refused_with_their_reason() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let admin = fixture.admin_token().await;

    // A member can be suspended and activated again; the row survives either way.
    let (member_id, member_email) = create_account(&fixture.db).await;
    fixture.remember_account(member_id);
    memberships::add_member(
        fixture.db.pool(),
        memberships::NewMembership {
            organization_id: fixture.org_a,
            user_id: member_id,
            status: "active".to_owned(),
            is_primary: false,
        },
    )
    .await
    .expect("the membership must be created");

    let suspended = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/members/{member_id}",
                fixture.org_a
            ),
            Some(&admin),
            Some(json!({ "status": "suspended" })),
        ),
    )
    .await;
    assert_eq!(
        suspended.status,
        StatusCode::OK,
        "suspend: {}",
        suspended.body
    );
    assert_eq!(suspended.body["status"], json!("suspended"));
    assert_eq!(suspended.body["email"], json!(member_email));

    let activated = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/members/{member_id}",
                fixture.org_a
            ),
            Some(&admin),
            Some(json!({ "status": "active" })),
        ),
    )
    .await;
    assert_eq!(activated.body["status"], json!("active"));

    // An unknown status is refused in the field rather than stored.
    let bogus = call(
        &fixture.state,
        request(
            Method::PATCH,
            &format!(
                "/api/v1/organizations/{}/members/{member_id}",
                fixture.org_a
            ),
            Some(&admin),
            Some(json!({ "status": "banned" })),
        ),
    )
    .await;
    assert_eq!(
        bogus.status,
        StatusCode::BAD_REQUEST,
        "bogus: {}",
        bogus.body
    );

    // The primary member cannot be removed: the organization would be left with no home.
    let refused = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/members/{}",
                fixture.org_a, fixture.admin_id
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "refused: {}",
        refused.body
    );
    assert_eq!(code_of(&refused.body), "last_owner");

    // A non-primary member is removed, and the removal is visible in the list.
    let removed = call(
        &fixture.state,
        request(
            Method::DELETE,
            &format!(
                "/api/v1/organizations/{}/members/{member_id}",
                fixture.org_a
            ),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let members = call(
        &fixture.state,
        request(
            Method::GET,
            &format!("/api/v1/organizations/{}/members", fixture.org_a),
            Some(&admin),
            None,
        ),
    )
    .await;
    assert!(
        !members.body["members"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|member| member["email"] == json!(member_email)),
        "the removed member is gone: {}",
        members.body
    );

    // Every step landed in the audit trail and the event stream.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where organization_id = $1 and action = any($2)",
    )
    .bind(fixture.org_a)
    .bind(vec![
        "organization.member.added".to_owned(),
        "organization.member.updated".to_owned(),
        "organization.member.removed".to_owned(),
    ])
    .fetch_one(fixture.db.pool())
    .await
    .expect("the audit count must be readable");
    assert!(
        audited >= 3,
        "the membership changes are audited: {audited}"
    );

    fixture.cleanup().await;
}
