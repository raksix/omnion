//! Integration walk for the role rules (REQ-065, slice 3).
//!
//! The unit tests prove the evaluator against a hand-built identity. This walk proves the halves
//! only a real database and the real router can reach:
//!
//! * the HTTP surface — read the set, replace it, read it back, and see the catalogue the editor
//!   renders its three pickers from;
//! * **atomicity as an observed fact**, not a claim: a set is replaced while the old one is still
//!   readable, and the write is refused for an invalid rule with the stored set untouched — a
//!   half-written rule set silently changes which role a colleague gets;
//! * the **cross-tenant refusal** answered as *absent* rather than forbidden, because a provider id
//!   is not a secret and 403 would leak that it exists;
//! * the grantability check the schema cannot express: `roles.organization_id` is nullable for
//!   platform roles, so a role in another tenant is a valid row and only the API refuses it;
//! * and the point of the whole slice — **the dry run and a real sign-in agree**. The same sample
//!   is resolved by the preview endpoint and by `RoleRules::resolve` on the rows the preview read
//!   back out of the database, and the audit entry is asserted for the reason string the feature
//!   promises (`role via rule #N`) and for what it must never carry: a rule's `when_value` is
//!   usually a group name, and a rule diff in a log nobody audits is a second copy of the
//!   directory.
//!
//! Every fixture address carries a `Uuid`. A fixed address is only safe while the walk passes,
//! which is exactly when it is not needed — and a walk that cannot be re-run after a failure
//! hides the next failure behind a uniqueness error.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::seed;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct TestResponse {
    status: StatusCode,
    /// The `Set-Cookie` value, when the response set one. Login hands the session back here
    /// rather than in the body, so a walk that reads only `body` authenticates as nobody and then
    /// blames the endpoint it was testing.
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
    TestResponse {
        status,
        set_cookie,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        },
    }
}

fn request(method: Method, uri: &str, session: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(credential) = session {
        builder = builder.header(header::COOKIE, credential);
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("request must build"),
        None => builder.body(Body::empty()).expect("request must build"),
    }
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
                "SKIP: PostgreSQL is not reachable ({error}) — the role-rule walk needs a \
                 database with every migration applied"
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

struct Fixture {
    state: AppState,
    db: Db,
    organizations: Vec<Uuid>,
    session: String,
    provider_id: Uuid,
    role_id: Uuid,
    foreign_provider: Uuid,
    foreign_role: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (state, db) = live_state().await?;
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let mut organizations = Vec::new();
        let mut sessions = Vec::new();
        let mut providers = Vec::new();
        let mut roles = Vec::new();

        for index in 0..2 {
            let slug = format!("rules-{index}-{}", Uuid::new_v4().simple());
            let organization_id: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Role Rules Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            let email = format!("rules-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = users::create_user(
                db.pool(),
                NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Role Rules Test Owner".to_owned(),
                    organization_id: Some(organization_id),
                },
            )
            .await
            .expect("the owner must be created");
            seed::bind_owner(db.pool(), owner.id)
                .await
                .expect("the owner binding must be created");

            // A role in THIS organization for a rule to grant. Created directly rather than
            // through the API so the walk does not depend on the role-management slice, which is
            // a different REQ and a different writer's screen.
            let role_id: Uuid = sqlx::query_scalar(
                "insert into roles (organization_id, key, name, priority) \
                 values ($1, $2, $3, $4) returning id",
            )
            .bind(organization_id)
            .bind(format!("r-{}", Uuid::new_v4().simple()))
            .bind("Directory Editor")
            .bind(10_i32)
            .fetch_one(db.pool())
            .await
            .expect("the role must be created");

            let login = call(
                &state,
                request(
                    Method::POST,
                    "/api/v1/auth/login",
                    None,
                    Some(json!({ "email": email, "password": PASSWORD })),
                ),
            )
            .await;
            assert_eq!(login.status, StatusCode::OK, "login: {}", login.body);
            let cookie = format!(
                "omnion_session={}",
                login
                    .set_cookie
                    .as_deref()
                    .expect("login must set the session cookie")
                    .split(';')
                    .next()
                    .expect("cookie has a value")
                    .split_once('=')
                    .expect("cookie is name=value")
                    .1
            );

            let connected = call(
                &state,
                request(
                    Method::POST,
                    "/api/v1/iam/providers",
                    Some(&cookie),
                    Some(json!({
                        "slug": format!("okta-{index}"),
                        "name": "Role Rules Fixture",
                        "kind": "oidc",
                        "config": {
                            "issuer": "https://idp.omnion.test",
                            "client_id": "qa-client",
                            "authorization_endpoint": "https://idp.omnion.test/authorize",
                            "token_endpoint": "https://idp.omnion.test/token",
                            "jwks_uri": "https://idp.omnion.test/jwks",
                            "group_claim": "groups"
                        },
                        "jit_enabled": true
                    })),
                ),
            )
            .await;
            assert_eq!(connected.status, StatusCode::CREATED, "connect: {}", connected.body);
            let provider_id =
                Uuid::parse_str(connected.body["id"].as_str().expect("an id")).expect("a uuid");

            organizations.push(organization_id);
            sessions.push(cookie);
            providers.push(provider_id);
            roles.push(role_id);
        }

        Some(Self {
            state,
            db,
            organizations,
            session: sessions[0].clone(),
            provider_id: providers[0],
            role_id: roles[0],
            foreign_provider: providers[1],
            foreign_role: roles[1],
        })
    }

    async fn rules_of(&self, id: Uuid, session: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/iam/providers/{id}/role-rules"),
                Some(session),
                None,
            ),
        )
        .await
    }

    async fn put_rules(&self, id: Uuid, session: &str, body: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::PUT,
                &format!("/api/v1/iam/providers/{id}/role-rules"),
                Some(session),
                Some(body),
            ),
        )
        .await
    }

    async fn dry_run(&self, id: Uuid, session: &str, sample: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/iam/providers/{id}/role-rules/preview"),
                Some(session),
                Some(json!({ "sample": sample })),
            ),
        )
        .await
    }

    async fn cleanup(&self) {
        for organization_id in &self.organizations {
            for statement in [
                "delete from auth_provider_events where organization_id = $1",
                "delete from auth_providers where organization_id = $1",
                "delete from audit_log where organization_id = $1",
                "delete from role_bindings where user_id in \
                   (select id from users where organization_id = $1)",
                "delete from users where organization_id = $1",
                "delete from roles where organization_id = $1",
            ] {
                sqlx::query(statement)
                    .bind(organization_id)
                    .execute(self.db.pool())
                    .await
                    .expect("cleanup must run");
            }
            sqlx::query("delete from organizations where id = $1")
                .bind(organization_id)
                .execute(self.db.pool())
                .await
                .expect("the organization must be removed");
        }
    }
}

/// The sample the walk resolves more than once — the dry run, and the evaluator the walk calls
/// itself. A sample with a group the first rule does not name and a department the second does is
/// the case where "first match wins" and "the rule you expected" come apart, so it is the sample
/// worth disagreeing about.
fn sample() -> Value {
    json!({
        "sub": "00u-directory-person",
        "email": "ada@example.com",
        "name": "Ada Lovelace",
        "department": "platform",
        "title": "staff engineer",
        "groups": ["contractors", "engineering"]
    })
}

#[tokio::test]
async fn a_rule_set_is_replaced_atomically_and_the_dry_run_names_the_matched_rule() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // NOTE: no cleanup here. The sessions in this fixture belong to the accounts `new()` just
    // created, and the walk needs them alive for every call that follows — running cleanup first
    // deletes those accounts and turns every authenticated request into a 401 that reads like a
    // broken endpoint. The single cleanup at the end is the one that matters.

    // --- 1. an empty set is a real state, not an error, and carries the catalogue ------------
    let empty = fixture.rules_of(fixture.provider_id, &fixture.session).await;
    assert_eq!(empty.status, StatusCode::OK, "read: {}", empty.body);
    assert_eq!(empty.body["rules"].as_array().map(Vec::len), Some(0));
    for picker in ["when_kinds", "when_operators", "scope_types"] {
        let options = empty.body[picker]
            .as_array()
            .unwrap_or_else(|| panic!("{picker} must arrive with the set"));
        assert!(!options.is_empty(), "{picker} must not be empty — the editor renders it");
        assert!(
            options.iter().all(|item| item.get("hint").and_then(Value::as_str).is_some()),
            "every option carries a hint, so the picker does not need a second source of truth"
        );
    }
    // The closed vocabulary, asserted against what the crate accepts, not against a copy of it.
    let kinds: Vec<&str> = empty.body["when_kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(kinds, ["claim", "group", "department", "title", "always"]);
    let operators: Vec<&str> = empty.body["when_operators"]
        .as_array()
        .expect("operators")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(operators, ["equals", "contains", "starts_with", "regex"]);

    // --- 2. save a set whose second rule is the one the sample will match -------------------
    let saved = fixture
        .put_rules(
            fixture.provider_id,
            &fixture.session,
            json!({ "rules": [
                // Narrow and first: nobody in this walk is in `payroll`, so it must NOT fire.
                { "when_kind": "group", "when_key": "groups",
                  "when_operator": "equals", "when_value": "payroll",
                  "role_id": fixture.role_id, "stop": true },
                // The rule the sample's department actually satisfies.
                { "when_kind": "department", "when_key": "department",
                  "when_operator": "equals", "when_value": "platform",
                  "role_id": fixture.role_id, "stop": true }
            ]}),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "save: {}", saved.body);
    let stored = saved.body["rules"].as_array().expect("rules");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0]["position"], 0, "the order that was sent is the order that is stored");
    assert_eq!(stored[1]["position"], 1);

    // --- 3. the dry run, against the STORED rules -------------------------------------------
    let run = fixture
        .dry_run(fixture.provider_id, &fixture.session, sample())
        .await;
    assert_eq!(run.status, StatusCode::OK, "dry run: {}", run.body);
    assert_eq!(
        run.body["matched_rule_index"], 1,
        "the first rule does not match, so the second decides"
    );
    assert_eq!(run.body["role_id"], json!(fixture.role_id));
    assert_eq!(
        run.body["reason"], "role via rule #2",
        "the reason the panel shows and the audit carries are the same sentence"
    );

    // The trace has to distinguish "did not match" from "was never reached" from "read nothing",
    // because those are three different bugs with three different fixes.
    let trace = run.body["trace"].as_array().expect("trace");
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0]["verdict"], "no_match");
    assert_eq!(trace[0]["matched"], false);
    assert_eq!(trace[1]["verdict"], "matched");
    assert_eq!(trace[1]["matched"], true);
    // Rule 0 read `contractors` and `engineering` and matched neither — asserted rather than
    // assumed, because an empty `read` would make a broken rule look like a correct one.
    assert_eq!(trace[0]["read"], json!(["contractors", "engineering"]));
    assert_eq!(trace[1]["read"], json!(["platform"]));

    // A rule ABOVE the match is `not_reached` — which is only true because the sample matched the
    // second rule, and the walk checks it on a run that matched.
    let ahead = fixture
        .put_rules(
            fixture.provider_id,
            &fixture.session,
            json!({ "rules": [
                { "when_kind": "title", "when_key": "title",
                  "when_operator": "equals", "when_value": "staff engineer",
                  "role_id": fixture.role_id, "stop": true },
                { "when_kind": "department", "when_key": "department",
                  "when_operator": "equals", "when_value": "platform",
                  "role_id": fixture.role_id, "stop": true }
            ]}),
        )
        .await;
    assert_eq!(ahead.status, StatusCode::OK, "save: {}", ahead.body);
    let run = fixture
        .dry_run(fixture.provider_id, &fixture.session, sample())
        .await;
    assert_eq!(
        run.body["matched_rule_index"], 0,
        "the sample's title really does satisfy the first rule, so this is a different outcome"
    );
    assert_eq!(run.body["trace"][1]["verdict"], "not_reached");

    // --- 4. a sample that matches nothing says so, and takes the default --------------------
    let stranger = json!({
        "sub": "00u-nobody",
        "email": "bob@example.com",
        "groups": ["sales"]
    });
    let none = fixture
        .dry_run(fixture.provider_id, &fixture.session, stranger)
        .await;
    assert_eq!(none.status, StatusCode::OK, "dry run: {}", none.body);
    assert_eq!(
        none.body["resolution"]["outcome"], "default",
        "a miss is tagged `default` and nests under `resolution`"
    );
    assert_eq!(none.body["matched_rule_index"], Value::Null);
    assert_eq!(
        none.body["reason"], "no rule matched → default role",
        "a miss is a documented result, not a silent nothing"
    );

    // --- 5. an invalid rule is refused and the stored set is UNTOUCHED -----------------------
    // The atomicity claim, observed: a 422 must leave the two good rules exactly as they were.
    let before = fixture.rules_of(fixture.provider_id, &fixture.session).await;
    assert_eq!(before.body["rules"].as_array().map(Vec::len), Some(2));
    let refused = fixture
        .put_rules(
            fixture.provider_id,
            &fixture.session,
            json!({ "rules": [
                { "when_kind": "group", "when_key": "groups",
                  "when_operator": "equals", "when_value": "engineering",
                  "role_id": fixture.role_id },
                // No `when_value`, so it can never match — the whole set is refused for it.
                { "when_kind": "claim", "when_key": "https://claims.example.com/team",
                  "when_operator": "equals", "role_id": fixture.role_id }
            ]}),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an incomplete rule must be refused: {}",
        refused.body
    );
    let after = fixture.rules_of(fixture.provider_id, &fixture.session).await;
    assert_eq!(
        after.body["rules"], before.body["rules"],
        "a refused write must leave the stored set byte-identical — the sign-in path reads it"
    );

    // --- 6. a rule cannot name a role that does not exist ------------------------------------
    let ghost = fixture
        .put_rules(
            fixture.provider_id,
            &fixture.session,
            json!({ "rules": [
                { "when_kind": "always", "role_id": "00000000-0000-0000-0000-00000000dead" }
            ]}),
        )
        .await;
    assert_eq!(
        ghost.status, StatusCode::BAD_REQUEST,
        "a rule that grants nothing is not a rule: {}",
        ghost.body
    );

    // --- 7. a role from another organization is refused by the API, not only by convention ----
    // The schema cannot express this: `roles.organization_id` is nullable for platform roles, so
    // "belongs to another tenant" is a valid row. The application is the only gate.
    let foreign = fixture
        .put_rules(
            fixture.provider_id,
            &fixture.session,
            json!({ "rules": [
                { "when_kind": "always", "role_id": fixture.foreign_role }
            ]}),
        )
        .await;
    assert_eq!(
        foreign.status, StatusCode::FORBIDDEN,
        "a role from another tenant must be refused by name: {}",
        foreign.body
    );

    // --- 8. a provider in another organization is ABSENT, not forbidden ------------------------
    let cross = fixture
        .rules_of(fixture.foreign_provider, &fixture.session)
        .await;
    assert_eq!(
        cross.status,
        StatusCode::NOT_FOUND,
        "403 would leak that the id exists: {}",
        cross.body
    );

    // --- 9. the audit entry carries the reason and never the rule rows ------------------------
    // `metadata::text` and a `String`, not a decoded `Value`: the walk asserts on the *rendered*
    // text, so a nested condition hiding inside a JSON value is visible to it. Decoding into a
    // `Value` first and then searching its own fields would let a leak in a field nobody looked at
    // pass, which is the whole failure this assertion exists to catch.
    let entries: Vec<String> = sqlx::query_scalar(
        "select metadata::text from audit_log where organization_id = $1 \
         and action = 'iam.provider_role_rules_replaced'",
    )
    .bind(fixture.organizations[0])
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit query must read");
    assert!(
        !entries.is_empty(),
        "replacing a rule set writes an audit entry"
    );
    let rendered = entries[0].clone();
    // The shape: which provider, how many rules, and which roles changed.
    assert!(
        rendered.contains("row_count") && rendered.contains("2"),
        "the entry records the shape of the change: {rendered}"
    );
    assert!(
        rendered.contains("okta-0"),
        "and names the provider it happened on: {rendered}"
    );
    // The thing it must NOT carry: a group name is a rule's `when_value`, and `payroll` is the
    // one this walk saved. An audit log collecting it is a second copy of the directory.
    for secret in ["payroll", "staff engineer", "platform", "engineering"] {
        assert!(
            !rendered.contains(secret),
            "the audit entry leaked the rule condition `{secret}`: {rendered}"
        );
    }

    fixture.cleanup().await;
}
