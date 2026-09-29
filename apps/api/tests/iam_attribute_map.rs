//! Integration walk for the attribute map (REQ-065, slice 2).
//!
//! The walk drives the **real router** and covers the halves the unit tests cannot reach:
//!
//! * the HTTP surface — read a map, replace it, read it back, and see the catalogue the editor
//!   renders its pickers from;
//! * the **refusals that keep a map safe**: a map with no email, a duplicate field, a transform
//!   missing its argument, a payload that is not an object, and a provider in another
//!   organization (reported as *absent*, not as forbidden, because an id is not a secret);
//! * the preview, which is the sign-in path's own projection — a complete payload produces the
//!   values a sign-in would write, and an incomplete one is **refused by name** rather than
//!   producing a partial account;
//! * the audit entry, asserted for what it must *not* contain. A person's department, title and
//!   employee number are the rows themselves: an audit log that collected a diff of them would be
//!   a second copy of the directory sitting somewhere nobody audits it.

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

/// Password for the accounts the fixture creates.
const PASSWORD: &str = "correct horse battery";

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
        builder = if credential.starts_with("omnion_session=") {
            builder.header(header::COOKIE, credential)
        } else {
            builder.header(header::AUTHORIZATION, format!("Bearer {credential}"))
        };
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
                "SKIP: PostgreSQL is not reachable ({error}) — the attribute-map walk needs a \
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

/// One organization, an Owner signed in, and a connected provider to hang the map off.
struct Fixture {
    state: AppState,
    db: Db,
    organization_id: Uuid,
    /// Every organization this walk created, so cleanup removes all of them.
    organizations: Vec<Uuid>,
    session: String,
    provider_id: Uuid,
    /// A provider in a *different* organization, owned by a different Owner.
    foreign_session: String,
    foreign_provider: Uuid,
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

        // Two organizations, because the cross-tenant case is a refusal that only a real second
        // owner can produce: a 403 would leak that the id exists, and a 404 proves it does not.
        for index in 0..2 {
            let slug = format!("attr-{index}-{}", Uuid::new_v4().simple());
            let organization_id: Uuid = sqlx::query_scalar(
                "insert into organizations (name, slug) values ($1, $2) returning id",
            )
            .bind("Attribute Map Test Organization")
            .bind(&slug)
            .fetch_one(db.pool())
            .await
            .expect("the organization must be created");

            let email = format!("attr-owner-{index}-{}@omnion.test", Uuid::new_v4().simple());
            let owner = users::create_user(
                db.pool(),
                NewUser {
                    email: email.clone(),
                    password: PASSWORD.to_owned(),
                    display_name: "Attribute Map Test Owner".to_owned(),
                    organization_id: Some(organization_id),
                },
            )
            .await
            .expect("the owner must be created");
            seed::bind_owner(db.pool(), owner.id)
                .await
                .expect("the owner binding must be created");

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
            let session = format!(
                "omnion_session={}",
                login
                    .set_cookie
                    .clone()
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
                    Some(&session),
                    Some(json!({
                        "slug": format!("okta-{index}"),
                        "name": "Attribute Map Fixture",
                        "kind": "oidc",
                        "config": {
                            "issuer": "https://idp.omnion.test",
                            "client_id": "qa-client",
                            "authorization_endpoint": "https://idp.omnion.test/authorize",
                            "token_endpoint": "https://idp.omnion.test/token",
                            "jwks_uri": "https://idp.omnion.test/jwks"
                        },
                        "jit_enabled": true
                    })),
                ),
            )
            .await;
            assert_eq!(
                connected.status,
                StatusCode::CREATED,
                "connect: {}",
                connected.body
            );
            let provider_id =
                Uuid::parse_str(connected.body["id"].as_str().expect("an id")).expect("a uuid");

            organizations.push(organization_id);
            sessions.push(session);
            providers.push(provider_id);
        }

        Some(Self {
            state,
            db,
            organization_id: organizations[0],
            organizations,
            session: sessions[0].clone(),
            provider_id: providers[0],
            foreign_session: sessions[1].clone(),
            foreign_provider: providers[1],
        })
    }

    /// GET the map of a provider, as the editor's first call.
    async fn map_of(&self, id: Uuid, session: &str) -> TestResponse {
        call(
            &self.state,
            request(
                Method::GET,
                &format!("/api/v1/iam/providers/{id}/attribute-mappings"),
                Some(session),
                None,
            ),
        )
        .await
    }

    /// PUT a whole map.
    async fn put_map(&self, id: Uuid, session: &str, body: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::PUT,
                &format!("/api/v1/iam/providers/{id}/attribute-mappings"),
                Some(session),
                Some(body),
            ),
        )
        .await
    }

    /// Rehearse the map against a payload.
    async fn preview(&self, id: Uuid, session: &str, sample: Value) -> TestResponse {
        call(
            &self.state,
            request(
                Method::POST,
                &format!("/api/v1/iam/providers/{id}/attribute-mappings/preview"),
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
                .expect("organization cleanup must run");
        }
    }
}

/// A complete, valid map used by most of the walk.
fn valid_map() -> Value {
    json!({ "mappings": [
        { "source_attr": "mail", "target_field": "email", "transform": "lowercase", "required": true, "position": 0 },
        { "source_attr": "given_name", "target_field": "display_name", "transform": "trim", "required": false, "position": 1 },
        { "source_attr": "department", "target_field": "department", "transform": "none", "required": false, "position": 2 },
        { "source_attr": "employee_number", "target_field": "employee_id", "transform": "none", "required": false, "position": 3 }
    ]})
}

#[tokio::test]
async fn the_attribute_map_replaces_atomically_previews_and_refuses() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let provider = fixture.provider_id;
    let session = fixture.session.clone();

    // --- 1. An empty provider has an empty map, and the catalogue the editor needs ------------
    let empty = fixture.map_of(provider, &session).await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.body);
    assert_eq!(
        empty.body["mappings"].as_array().map(Vec::len),
        Some(0),
        "a provider with no map has no rows, not an error"
    );
    // The pickers are built from the server, so the catalogue has to be here — eight fields and
    // six transforms, or a field added to the crate never appears in the panel.
    assert_eq!(
        empty.body["target_fields"].as_array().map(Vec::len),
        Some(8),
        "the map must ship the field catalogue"
    );
    let transforms = empty.body["transforms"].as_array().expect("transforms");
    assert_eq!(transforms.len(), 6, "six transforms, no more");
    // The two that need an argument must say so, or the editor renders an argument box that
    // silently does nothing for every other transform.
    for item in transforms {
        let needs = item["needs_argument"].as_bool().expect("a boolean");
        let name = item["name"].as_str().expect("a name");
        assert_eq!(
            needs,
            name == "prefix" || name == "static",
            "{name} reports the wrong argument rule"
        );
    }

    // --- 2. Replace the whole map, and read it back -------------------------------------------
    let saved = fixture.put_map(provider, &session, valid_map()).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["mappings"].as_array().map(Vec::len), Some(4));

    let reread = fixture.map_of(provider, &session).await;
    assert_eq!(reread.status, StatusCode::OK);
    let rows = reread.body["mappings"].as_array().expect("rows");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["source_attr"], "mail");
    assert_eq!(rows[0]["target_field"], "email");
    assert_eq!(rows[0]["transform"], "lowercase");
    assert_eq!(rows[0]["needs_argument"], json!(false));
    // The stored order is contiguous, which is what makes "row 3" mean the same thing in the
    // editor, the preview and the database.
    let positions: Vec<i64> = rows
        .iter()
        .map(|row| row["position"].as_i64().expect("a position"))
        .collect();
    assert_eq!(
        positions,
        vec![0, 1, 2, 3],
        "positions are renumbered on write"
    );

    // --- 3. A replacement is a replacement, not an append --------------------------------------
    let shorter = json!({ "mappings": [
        { "source_attr": "upn", "target_field": "email", "transform": "lowercase", "required": true }
    ]});
    let replaced = fixture.put_map(provider, &session, shorter).await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    assert_eq!(
        replaced.body["mappings"].as_array().map(Vec::len),
        Some(1),
        "a PUT replaces the map; the old rows must be gone"
    );
    let after = fixture.map_of(provider, &session).await;
    assert_eq!(after.body["mappings"].as_array().map(Vec::len), Some(1));
    // And the old email row is really gone from the table, not just from the response.
    let stored: i64 = sqlx::query_scalar(
        "select count(*) from provider_attribute_mappings where provider_id = $1",
    )
    .bind(provider)
    .fetch_one(fixture.db.pool())
    .await
    .expect("the count must read");
    assert_eq!(stored, 1, "the table holds exactly the rows that were sent");

    // Put the full map back for the preview half of the walk.
    let restored = fixture.put_map(provider, &session, valid_map()).await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);

    // --- 4. The preview is the sign-in projection ---------------------------------------------
    let sample = json!({
        "sub": "2481",
        "mail": "  Furkan@Example.COM ",
        "given_name": "  Furkan  ",
        "department": ["Platform", "QA"],
        "employee_number": 4812,
        "unmapped_thing": "ignored"
    });
    let preview = fixture.preview(provider, &session, sample).await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    assert_eq!(preview.body["ok"], json!(true), "{}", preview.body);

    // Values, not the raw claims: the transform actually ran, and the address came back folded
    // and trimmed. A preview that echoed the input would prove nothing about the map.
    let values: Vec<(String, String)> = preview.body["values"]
        .as_array()
        .expect("values")
        .iter()
        .map(|entry| {
            (
                entry["field"].as_str().unwrap_or_default().to_owned(),
                entry["value"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    // The claim is `  Furkan@Example.COM  ` and the transform is `lowercase`, so the projected
    // value is all-lowercase. An assertion written with a capital F tests the author's memory
    // of the sample rather than the projection, and it fails for a reason that looks like a
    // product bug while being entirely the test's.
    assert!(
        values.contains(&("email".to_owned(), "furkan@example.com".to_owned())),
        "{values:?}"
    );
    assert!(
        values.contains(&("display_name".to_owned(), "Furkan".to_owned())),
        "{values:?}"
    );
    // A multi-valued attribute reads its first usable member, and a number is a valid id.
    assert!(
        values.contains(&("department".to_owned(), "Platform".to_owned())),
        "{values:?}"
    );
    assert!(
        values.contains(&("employee_id".to_owned(), "4812".to_owned())),
        "{values:?}"
    );

    // --- 5. A missing required field refuses by name -----------------------------------------
    let incomplete = fixture
        .preview(
            provider,
            &session,
            json!({ "sub": "1", "given_name": "Nobody" }),
        )
        .await;
    assert_eq!(incomplete.status, StatusCode::OK, "{}", incomplete.body);
    assert_eq!(
        incomplete.body["ok"],
        json!(false),
        "a payload with no email would be refused by a real sign-in"
    );
    let missing: Vec<&str> = incomplete.body["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .map(|item| item.as_str().unwrap_or_default())
        .collect();
    assert_eq!(missing, vec!["email"], "the refusal names the field");
    assert_eq!(
        incomplete.body["values"].as_array().map(Vec::len),
        Some(0),
        "a refused projection withholds its values rather than returning a half account"
    );

    // --- 6. The refusals that keep a saved map safe -------------------------------------------
    // No email at all: the panel must not be able to save a map that provisions nobody.
    let no_email = fixture
        .put_map(
            provider,
            &session,
            json!({ "mappings": [
                { "source_attr": "given_name", "target_field": "display_name" }
            ]}),
        )
        .await;
    assert_eq!(
        no_email.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        no_email.body
    );
    // The refusal names the missing field. Errors arrive in the `{error:{code,message}}`
    // envelope every other walk reads, so the message is two levels down — reading
    // `body["message"]` here would have tested `Null`, which is a passing assertion about
    // nothing.
    assert!(
        no_email.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("email"),
        "the refusal names the missing field: {}",
        no_email.body
    );

    // Two rows writing one field: refused at save, not resolved by luck at sign-in.
    let duplicate = fixture
        .put_map(
            provider,
            &session,
            json!({ "mappings": [
                { "source_attr": "mail", "target_field": "email", "required": true },
                { "source_attr": "other_mail", "target_field": "email", "required": true }
            ]}),
        )
        .await;
    assert_eq!(
        duplicate.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        duplicate.body
    );
    assert!(
        duplicate.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already mapped"),
        "{}",
        duplicate.body
    );

    // A transform that needs an argument and has none.
    let no_argument = fixture
        .put_map(
            provider,
            &session,
            json!({ "mappings": [
                { "source_attr": "mail", "target_field": "email", "transform": "prefix", "required": true }
            ]}),
        )
        .await;
    assert_eq!(
        no_argument.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        no_argument.body
    );

    // An unknown field, and a field the platform does not have.
    let unknown_field = fixture
        .put_map(
            provider,
            &session,
            json!({ "mappings": [
                { "source_attr": "mail", "target_field": "favorite_colour", "required": true }
            ]}),
        )
        .await;
    assert_eq!(
        unknown_field.status,
        StatusCode::BAD_REQUEST,
        "{}",
        unknown_field.body
    );

    let unknown_transform = fixture
        .put_map(
            provider,
            &session,
            json!({ "mappings": [
                { "source_attr": "mail", "target_field": "email", "transform": "eval", "required": true }
            ]}),
        )
        .await;
    assert_eq!(
        unknown_transform.status,
        StatusCode::BAD_REQUEST,
        "{}",
        unknown_transform.body
    );

    // The map is still the one that was saved: a refused write must not half-apply.
    let survived = fixture.map_of(provider, &session).await;
    assert_eq!(
        survived.body["mappings"].as_array().map(Vec::len),
        Some(4),
        "a refused write leaves the stored map untouched"
    );

    // --- 7. A cleared map is a legitimate action ---------------------------------------------
    let cleared = fixture
        .put_map(provider, &session, json!({ "mappings": [] }))
        .await;
    assert_eq!(
        cleared.status,
        StatusCode::OK,
        "clearing a map is a real action, not a validation error: {}",
        cleared.body
    );
    let after_clear = fixture.map_of(provider, &session).await;
    assert_eq!(
        after_clear.body["mappings"].as_array().map(Vec::len),
        Some(0)
    );

    // --- 8. The cross-tenant case: absent, not forbidden --------------------------------------
    // A provider id is not a secret, so the answer must not distinguish "exists" from "not mine".
    let foreign = fixture.map_of(fixture.foreign_provider, &session).await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "another organization's provider reads as absent, never as forbidden: {}",
        foreign.body
    );
    // And the same rule for the other direction: the second owner may read their own provider.
    let own = fixture
        .map_of(fixture.foreign_provider, &fixture.foreign_session)
        .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);

    // --- 9. The preview refuses a payload that is not an object -------------------------------
    let not_an_object = fixture
        .preview(provider, &session, json!("a string, not a claims object"))
        .await;
    assert_eq!(
        not_an_object.status,
        StatusCode::BAD_REQUEST,
        "{}",
        not_an_object.body
    );

    // --- 10. The audit entry records the shape, not the rows -----------------------------------
    // The fields that changed, and nothing else: a department and an employee number are personal
    // data, and a mapping change is not the place to copy them into a log.
    let entries: Vec<String> = sqlx::query_scalar(
        "select metadata::text from audit_log where organization_id = $1 \
         and action = 'iam.provider_attribute_mappings_replaced'",
    )
    .bind(fixture.organization_id)
    .fetch_all(fixture.db.pool())
    .await
    .expect("the audit query must read");
    assert!(!entries.is_empty(), "replacing a map writes an audit entry");
    let joined = entries.join(" ");
    assert!(
        joined.contains("fields_added"),
        "the entry names the fields that were mapped: {joined}"
    );
    for secret_ish in ["Platform", "4812", "Furkan"] {
        assert!(
            !joined.contains(secret_ish),
            "the audit entry must not carry row values ({secret_ish}): {joined}"
        );
    }

    fixture.cleanup().await;
}
