//! Integration walk for the stored-membership source of a group role rule (REQ-065, slice 4
//! part 6).
//!
//! The unit tests prove the evaluator against a hand-built `GroupContext`. This walk proves the
//! half only a real database and the real router can reach, and it is built around the one
//! scenario the whole feature exists for:
//!
//! * a **SCIM-provisioned account** — one the IdP asserts no group claim about, whose membership
//!   exists only because a connector wrote `group_members`;
//! * a **group rule** the operator wrote for exactly that account;
//! * and the **next sign-in**, which has to grant the mapped role.
//!
//! Before this slice that combination granted nothing, silently: the evaluator read the token's
//! claim, the claim was empty, and the audit said `no rule matched → default role`. The event that
//! announces such a change, `iam.group_membership_synced`, fired into that silence. The claim here
//! is that nothing did, so the test asserts the **binding** exists rather than that a function
//! returned a variant.
//!
//! Four things are pinned, and each is a way the obvious implementation lies:
//!
//! 1. **The grant happens** — read back out of `role_bindings`, not out of the response body.
//! 2. **The rule is not reached through the claim.** The dry run is driven twice, with and without
//!    a `subject`: the sample-only run must miss, the named-subject run must match. A dry run that
//!    quietly consulted the database for *any* pasted sample would pass a one-sided test, and it
//!    would also be reading rows the operator never named.
//! 3. **The dry run says which source fired.** A `Matched` that does not distinguish a claim from
//!    a stored row cannot answer "which clock revokes this role", and a rule that fires on the
//!    database must not read as a claim match.
//! 4. **The cross-tenant subject is refused**, and so is an unknown one, with the same answer —
//!    the dry run must not become a probe for which account ids exist in another organization.
//!
//! Fixtures carry a `Uuid` in every address, so the walk is re-runnable after a failure.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_identity::users::{self, NewUser};
use omnion_permissions::{groups, seed};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

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
        .expect("the body must be readable")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    TestResponse {
        status,
        set_cookie,
        body,
    }
}

fn request(
    method: Method,
    path: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header(header::COOKIE, format!("omnion_session={session}"));
    }
    builder
        .body(match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        })
        .expect("the request must build")
}

async fn live_state() -> Option<(AppState, Db)> {
    let config = Config::from_env().expect("environment must be valid");
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({error}) — this walk needs a database with \
                 every migration applied"
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
        omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
            .expect("the default storage configuration is valid"),
    );
    Some((state, db))
}

/// One organization with an owner, a directory provider, a role to grant, an account a connector
/// would have created, and a group it would have put them in.
struct Tenant {
    organization_id: Uuid,
    session: String,
    provider_id: Uuid,
    role_id: Uuid,
    user_id: Uuid,
    group_slug: String,
}

impl Tenant {
    async fn make(state: &AppState, db: &Db, index: usize) -> Tenant {
        seed::ensure(db.pool())
            .await
            .expect("the IAM seed must run");

        let slug = format!("membership-{index}-{}", Uuid::new_v4().simple());
        let organization_id: Uuid = sqlx::query_scalar(
            "insert into organizations (name, slug) values ($1, $2) returning id",
        )
        .bind("Group Membership Test Organization")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .expect("the organization must be created");

        let email = format!(
            "membership-owner-{index}-{}@omnion.test",
            Uuid::new_v4().simple()
        );
        let owner = users::create_user(
            db.pool(),
            NewUser {
                email: email.clone(),
                password: PASSWORD.to_owned(),
                display_name: "Membership Test Owner".to_owned(),
                organization_id: Some(organization_id),
            },
        )
        .await
        .expect("the owner must be created");
        seed::bind_owner(db.pool(), owner.id)
            .await
            .expect("the owner binding must be created");

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

        // The provisioned subject. The password column carries the JIT marker rather than a hash,
        // which is exactly what `/scim/v2/Users` leaves behind — the account exists to be signed
        // in by an IdP, not by a password.
        let user_id: Uuid = sqlx::query_scalar(
            "insert into users (email, password_hash, display_name, organization_id) \
             values ($1, $2, $3, $4) returning id",
        )
        .bind(format!(
            "provisioned-{index}-{}@omnion.test",
            Uuid::new_v4().simple()
        ))
        .bind("!jit:no-password")
        .bind("Provisioned Person")
        .bind(organization_id)
        .fetch_one(db.pool())
        .await
        .expect("the provisioned account must be created");

        // The group the connector wrote, and the membership row that is the entire reason this
        // slice exists. The name is "Engineering" so the slug is the same word; the walk's rule
        // is written against the slug, which is what a panel URL shows.
        let group = groups::create(
            db.pool(),
            groups::NewGroup {
                organization_id,
                name: "Engineering".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("the group must be created");
        assert!(
            groups::add_member(db.pool(), group.id, user_id, None)
                .await
                .expect("the membership must be written"),
            "a first add must report the row it inserted"
        );

        let provider_id: Uuid = sqlx::query_scalar(
            "insert into auth_providers (organization_id, slug, name, kind, enabled, config) \
             values ($1, $2, $3, 'oidc', false, '{}'::jsonb) returning id",
        )
        .bind(organization_id)
        .bind(format!("p-{}", Uuid::new_v4().simple()))
        .bind("Directory Provider")
        .fetch_one(db.pool())
        .await
        .expect("the provider must be created");

        let login = call(
            state,
            request(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "email": email, "password": PASSWORD })),
            ),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "login: {}", login.body);
        let session = login
            .set_cookie
            .as_deref()
            .and_then(|value| value.split(';').next())
            .and_then(|value| value.split_once('='))
            .map(|(_, value)| value.to_owned())
            .expect("login must hand back a session cookie");

        Tenant {
            organization_id,
            session,
            provider_id,
            role_id,
            user_id,
            group_slug: group.slug,
        }
    }

    /// Install the one rule under test: `when_group` on the provisioned account's group.
    async fn install_group_rule(&self, state: &AppState) {
        let response = call(
            state,
            request(
                Method::PUT,
                &format!("/api/v1/iam/providers/{}/role-rules", self.provider_id),
                Some(&self.session),
                Some(json!({
                    "rules": [{
                        "position": 0,
                        "when_kind": "group",
                        "when_key": "groups",
                        "when_operator": "equals",
                        "when_value": self.group_slug,
                        "role_id": self.role_id,
                        "scope_type": "organization",
                    }]
                })),
            ),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "the rule must save: {}",
            response.body
        );
    }

    async fn dry_run(
        &self,
        state: &AppState,
        sample: Value,
        subject: Option<Uuid>,
    ) -> TestResponse {
        call(
            state,
            request(
                Method::POST,
                &format!(
                    "/api/v1/iam/providers/{}/role-rules/preview",
                    self.provider_id
                ),
                Some(&self.session),
                Some(json!({ "sample": sample, "subject": subject })),
            ),
        )
        .await
    }
}

/// A sample that a provisioned person's IdP would really send: the account, and no group claim.
fn provisioned_sample() -> Value {
    json!({ "email": "provisioned-0@omnion.test", "sub": "connector-1" })
}

#[tokio::test]
async fn a_group_rule_grants_from_stored_membership_the_claim_never_carried() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let tenant = Tenant::make(&state, &db, 0).await;
    tenant.install_group_rule(&state).await;

    // 1. The reader the sign-in path uses sees the connector's row.
    let membership =
        groups::membership_groups(db.pool(), tenant.user_id, Some(tenant.organization_id))
            .await
            .expect("the stored membership must be readable");
    assert_eq!(
        membership.len(),
        1,
        "the connector wrote one membership row and the reader must see exactly it"
    );
    assert_eq!(
        membership[0].slug, tenant.group_slug,
        "and it must be the group the walk created"
    );

    // 2. Without a named subject the preview is exactly what it always was: a claims document
    //    evaluated against its own groups. A sample that carries none must still miss, which is
    //    the "the rule is not reached through the database by default" half.
    let without = tenant.dry_run(&state, provisioned_sample(), None).await;
    assert_eq!(without.status, StatusCode::OK, "{}", without.body);
    assert_eq!(
        without.body["matched_rule_index"],
        Value::Null,
        "a pasted sample with no group claim must not match: {}",
        without.body
    );
    assert_eq!(
        without.body["group_source"], "none",
        "and the run must say there was nothing to read, rather than leaving the panel to guess"
    );

    // 3. With the provisioned account named, the same rules and the same sample now match, and
    //    the trace says the value came from stored membership.
    let with = tenant
        .dry_run(&state, provisioned_sample(), Some(tenant.user_id))
        .await;
    assert_eq!(with.status, StatusCode::OK, "{}", with.body);
    assert_eq!(
        with.body["role_id"],
        json!(tenant.role_id),
        "the named subject's stored group must grant the mapped role: {}",
        with.body
    );
    assert_eq!(
        with.body["group_source"], "membership_only",
        "and the run must name the source, or an operator cannot tell which clock revokes it"
    );

    let trace = with.body["trace"].as_array().expect("the trace is a list");
    let matched = trace
        .iter()
        .find(|row| row["matched"] == json!(true))
        .expect("exactly one rule must be marked matched");
    assert_eq!(
        matched["group_source"], "membership",
        "a rule that fired on the stored row must not read as a claim match"
    );
    assert_eq!(
        matched["read"],
        json!([tenant.group_slug]),
        "and the trace must show the value the evaluator actually compared"
    );
}

#[tokio::test]
async fn the_dry_run_refuses_a_subject_from_another_organization() {
    let Some((state, db)) = live_state().await else {
        return;
    };
    let mine = Tenant::make(&state, &db, 1).await;
    let theirs = Tenant::make(&state, &db, 2).await;
    mine.install_group_rule(&state).await;

    // A cross-tenant subject is refused rather than quietly evaluated, because a trace that
    // folded another organization's groups into this one's answer would tell an operator their
    // rule fires when it will not.
    let cross = mine
        .dry_run(&state, provisioned_sample(), Some(theirs.user_id))
        .await;
    assert_eq!(
        cross.status,
        StatusCode::NOT_FOUND,
        "a subject in another organization must not be answered: {}",
        cross.body
    );
    assert_eq!(
        cross.body["error"]["code"], "subject_not_in_organization",
        "and the refusal is named, so the panel can say why: {}",
        cross.body
    );

    // An id that belongs to nobody is answered the same way. The two must not be
    // distinguishable: the difference would turn the dry run into a probe for which account ids
    // exist in other tenants.
    let unknown = mine
        .dry_run(&state, provisioned_sample(), Some(Uuid::new_v4()))
        .await;
    assert_eq!(
        unknown.status, cross.status,
        "an unknown subject and a foreign one must be indistinguishable"
    );
    assert_eq!(
        unknown.body["error"]["code"], cross.body["error"]["code"],
        "the two refusals must be the same sentence, not merely the same status"
    );
}
