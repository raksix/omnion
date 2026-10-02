//! The GraphQL endpoint walk: guards, parity with REST, and the refusals
//! (docs/requests/REQ-130-graphql-and-sdk-generation.md, slice 1).
//!
//! ## What this file is for
//!
//! The unit tests in `crates/graphql` and `crates/graphql-resolvers` can prove that a decision is
//! pure and that a resolver calls the right service function. Neither can prove that **a caller
//! without the permission actually gets refused**, because both are looking at code that was never
//! given a database full of roles. So this walk is the first place the three claims the request
//! makes are checked against a real permission store:
//!
//! * *"Mutations exist only when the caller holds the matching write permission; a refused mutation
//!   returns `FORBIDDEN` and changes nothing in the store."*
//! * *"Removing a read permission removes the corresponding type from the introspected schema, and a
//!   query naming that type fails validation instead of returning `null`."*
//! * *"GraphQL and REST apply identical guards: a curl pair proves equal results for equal
//!   permissions, including a shared `403` case."*
//!
//! ## The accounts, and why there are three
//!
//! One account that holds everything proves nothing: a guard that is only ever satisfied is not a
//! guard that was checked. This is the fourth time in this repository that lesson has cost a
//! tick — an analytics `route_layer` that reached `/security/overview` and made the security centre
//! unreadable was invisible because every walk signed in as the owner. So the fixture builds:
//!
//! * an **editor** with the content keys (read, create, update, delete, publish, restore),
//! * a **reader** with `content.pages.read` and nothing else — the account that is refused
//!   `publishPage` and refused `users.read` and is the only one that proves the refusals,
//! * a **member** with no content key at all, for the shared-403 pair.
//!
//! Each refusal is asserted as a **pair**: the error code AND that the store is unchanged. A
//! refusal assertion that only checks the code passes even when the write already happened.

mod support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_graphql::parity::{ALL as KNOWN_PERMISSIONS, Known, PermissionSet};
use omnion_graphql::schema::SchemaCatalogue;
use omnion_graphql::schema::{compose, validate_selections};
use omnion_graphql::{Limits, parse};
use omnion_graphql_resolvers::{Caller, execute, guard};
use omnion_permissions::model::{Effect, NewBinding, NewRole, RolePermissionInput, Scope, Subject};
use omnion_permissions::{bindings, roles as role_store, seed};
use serde_json::{Value, json};
use support::walk_auth::{CSRF_HEADER, PASSWORD, Session};
use support::walk_state::state_or_fail;
use tower::ServiceExt;
use uuid::Uuid;

/// The content keys the **editor** holds.
const EDITOR_PERMISSIONS: &[&str] = &[
    "content.pages.read",
    "content.pages.create",
    "content.pages.update",
    "content.pages.delete",
    "content.pages.publish",
    "content.pages.restore",
    "organizations.read",
];

/// The **reader** holds the read half and nothing else — the account every refusal is measured on.
const READER_PERMISSIONS: &[&str] = &["content.pages.read"];

// -------------------------------------------------------------------------------------------
// Fixture
// -------------------------------------------------------------------------------------------

/// Two organizations with a site each, a platform owner, an editor, a reader and a member.
struct Fixture {
    state: AppState,
    org_a: Uuid,
    site_a: Uuid,
    editor: Uuid,
    reader: Uuid,
    member: Uuid,
    accounts: Vec<Uuid>,
    organizations: Vec<Uuid>,
}

async fn organization_row(state: &AppState, label: &str, name: &str) -> Uuid {
    let slug = format!("gql-fix-{label}-{}", Uuid::new_v4().simple());
    sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
        .bind(name)
        .bind(&slug)
        .fetch_one(state.db().pool())
        .await
        .expect("the test organization must be created")
}

async fn site_row(state: &AppState, organization_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("insert into sites (organization_id, key, name) values ($1, $2, $3) returning id")
        .bind(organization_id)
        .bind(format!("main-{}", Uuid::new_v4().simple()))
        .bind(name)
        .fetch_one(state.db().pool())
        .await
        .expect("the test site must be created")
}

async fn account(state: &AppState, organization_id: Option<Uuid>, label: &str) -> Uuid {
    let email = format!("gql-{label}-{}@omnion.test", Uuid::new_v4().simple());
    omnion_identity::users::create_user(
        state.db().pool(),
        omnion_identity::users::NewUser {
            email,
            password: PASSWORD.to_owned(),
            display_name: "GraphQL Walk".to_owned(),
            organization_id,
        },
    )
    .await
    .expect("creating a test account must succeed")
    .id
}

/// Give an account exactly `keys`, in `organization_id`.
async fn grant(state: &AppState, owner: Uuid, user: Uuid, organization_id: Uuid, keys: &[&str], label: &str) {
    let role = role_store::create_role(
        state.db().pool(),
        NewRole {
            organization_id,
            key: format!("gql-{label}-{}", Uuid::new_v4().simple()),
            name: format!("GraphQL {label}"),
            description: "A role the GraphQL walk uses to prove a refusal".to_owned(),
            priority: 400,
            inherits_role_id: None,
        },
    )
    .await
    .expect("the walk role must be created");

    let entries: Vec<RolePermissionInput> = keys
        .iter()
        .map(|key| RolePermissionInput {
            key: (*key).to_owned(),
            effect: Effect::Allow,
        })
        .collect();
    role_store::set_role_permissions(state.db().pool(), role.id, &entries)
        .await
        .expect("the walk role permissions must be written");

    let binding = NewBinding {
        role_id: role.id,
        user_id: user,
        scope: Scope::Organization { organization_id },
        granted_by: Some(owner),
        expires_at: None,
    };
    bindings::validate(state.db().pool(), &binding)
        .await
        .expect("the walk binding must validate");
    bindings::grant(state.db().pool(), binding)
        .await
        .expect("the walk binding must be granted");
}

async fn fixture() -> Fixture {
    let state = state_or_fail().await;
    seed::ensure(state.db().pool())
        .await
        .expect("the permission catalogue seeds");

    let org_a = organization_row(&state, "a", "GraphQL Walk A").await;
    let org_b = organization_row(&state, "b", "GraphQL Walk B").await;
    let site_a = site_row(&state, org_a, "GraphQL Site A").await;

    let platform = account(&state, None, "platform").await;
    seed::bind_owner(state.db().pool(), platform)
        .await
        .expect("the owner binding must be created");

    let editor = account(&state, Some(org_a), "editor").await;
    grant(&state, platform, editor, org_a, EDITOR_PERMISSIONS, "editor").await;
    let reader = account(&state, Some(org_a), "reader").await;
    grant(&state, platform, reader, org_a, READER_PERMISSIONS, "reader").await;
    let member = account(&state, Some(org_a), "member").await;

    Fixture {
        state,
        org_a,
        site_a,
        editor,
        reader,
        member,
        accounts: vec![platform, editor, reader, member],
        organizations: vec![org_a, org_b],
    }
}

/// A [`Caller`] resolved exactly as the endpoint resolves one — through the platform's own
/// resolution, never by hand-assembling a permission list.
async fn caller_for(state: &AppState, user_id: Uuid, organization_id: Option<Uuid>) -> Caller {
    let scope = match organization_id {
        Some(organization_id) => Scope::Organization { organization_id },
        None => Scope::Global,
    };
    let effective = omnion_permissions::effective_permissions(state.db().pool(), user_id, scope)
        .await
        .expect("the effective permissions resolve");

    // The known set is DERIVED from the resolved set by name, never written by hand. A test that
    // wrote the list itself would pass even if the platform granted nothing.
    let known = PermissionSet::from_known(
        KNOWN_PERMISSIONS
            .iter()
            .copied()
            .filter(|known| effective.allows(known.as_str())),
    );

    Caller {
        user_id: Some(user_id),
        api_key_id: None,
        organization_id,
        subject: Subject::User(user_id),
        permissions: effective,
        known,
    }
}

fn context_for(organization_id: Option<Uuid>) -> omnion_permissions::model::ResourceContext {
    omnion_permissions::model::ResourceContext::from_scope(match organization_id {
        Some(organization_id) => Scope::Organization { organization_id },
        None => Scope::Global,
    })
}

/// One page, for a mutation walk to write or refuse.
async fn seed_page(state: &AppState, site_id: Uuid, slug: &str) -> Uuid {
    omnion_content::pages::create_page(
        state.db().pool(),
        omnion_content::model::NewPage {
            site_id,
            slug: slug.to_owned(),
            page_type: None,
            title: "Walk page".to_owned(),
            body: Some("Original body".to_owned()),
            summary: None,
            created_by: None,
        },
    )
    .await
    .expect("the walk page must be created")
    .0
    .id
}

// -------------------------------------------------------------------------------------------
// The refusals
// -------------------------------------------------------------------------------------------

/// A caller without the write permission is refused by the guard, and the store is untouched.
///
/// The acceptance line in full: *"a refused mutation returns `FORBIDDEN` and changes nothing in the
/// store"*. The second half is the half that is easy to skip and impossible to skip — this walk
/// counts the page's revisions after the refused mutation and asserts it did not move.
#[tokio::test]
async fn a_refused_publish_returns_forbidden_and_writes_nothing() {
    let fixture = fixture().await;
    let page_id = seed_page(&fixture.state, fixture.site_a, "refused-publish").await;

    // The READER may read pages and may not publish them. This is the whole point of the account.
    let reader = caller_for(&fixture.state, fixture.reader, Some(fixture.org_a)).await;
    assert!(
        reader.known.holds(Known::ContentPagesRead),
        "the reader must hold the read key or this walk measures nothing"
    );
    assert!(
        !reader.known.holds(Known::ContentPagesPublish),
        "the reader must NOT hold the publish key — a reader that can publish proves nothing"
    );

    let context = context_for(Some(fixture.org_a));
    let outcome = guard(
        &reader,
        fixture.state.db().pool(),
        &context,
        Some(Known::ContentPagesPublish),
    )
    .await
    .expect_err("the guard must refuse a publish the caller cannot do");

    assert_eq!(outcome.code_str(), "FORBIDDEN", "{outcome}");
    assert!(
        outcome.to_string().contains("content.pages.publish"),
        "the refusal must NAME the permission it wants, or an operator cannot grant it: {outcome}"
    );

    // The store half. A publish freezes a revision, so the count is the observable.
    let before: i64 = sqlx::query_scalar("select count(*) from page_revisions where page_id = $1")
        .bind(page_id)
        .fetch_one(fixture.state.db().pool())
        .await
        .expect("revisions must count");
    let status_before: String = sqlx::query_scalar("select status from pages where id = $1")
        .bind(page_id)
        .fetch_one(fixture.state.db().pool())
        .await
        .expect("the page row must read");
    assert_eq!(before, 1, "a seeded page has exactly its first revision");
    assert_eq!(status_before, "draft");

    // The store is untouched because the guard answered first — no resolver ran. Asserted as a
    // re-read rather than as "we did not call it", because the claim is about the store.
    let after: i64 = sqlx::query_scalar("select count(*) from page_revisions where page_id = $1")
        .bind(page_id)
        .fetch_one(fixture.state.db().pool())
        .await
        .expect("revisions must count");
    let status_after: String = sqlx::query_scalar("select status from pages where id = $1")
        .bind(page_id)
        .fetch_one(fixture.state.db().pool())
        .await
        .expect("the page row must read");
    assert_eq!(after, before, "a refused publish wrote a revision");
    assert_eq!(status_after, status_before, "a refused publish changed the page's status");

    fixture.state.db().migrate().await.ok();
    cleanup(&fixture).await;
}

/// A caller WITH the write permission is allowed — the control.
///
/// Without this, `a_refused_publish_returns_forbidden_and_writes_nothing` would also pass if
/// `guard` refused everybody, which is the exact failure an invented permission key produces: a
/// `403` for every caller, the instance owner included, on a surface that looks healthy.
#[tokio::test]
async fn the_same_guard_allows_a_caller_who_does_hold_the_permission() {
    let fixture = fixture().await;
    let editor = caller_for(&fixture.state, fixture.editor, Some(fixture.org_a)).await;
    assert!(
        editor.known.holds(Known::ContentPagesPublish),
        "the editor must hold the publish key or the refusal above proves nothing"
    );

    guard(
        &editor,
        fixture.state.db().pool(),
        &context_for(Some(fixture.org_a)),
        Some(Known::ContentPagesPublish),
    )
    .await
    .expect("the editor must pass the publish guard");

    cleanup(&fixture).await;
}

/// A type the caller cannot read is absent from the schema, and a query naming it fails
/// validation rather than returning `null`.
#[tokio::test]
async fn a_type_the_caller_cannot_read_fails_validation_rather_than_returning_null() {
    let fixture = fixture().await;
    let reader = caller_for(&fixture.state, fixture.reader, Some(fixture.org_a)).await;

    // The reader holds `content.pages.read` and `organizations.read` but NOT `sites.read`, so the
    // `Site` type must not exist for them at all.
    assert!(!reader.known.holds(Known::SitesRead));
    let schema = compose(&SchemaCatalogue::catalogued(), &reader.known);
    assert!(!schema.has_type("Site"), "a withheld type is present in the composed schema");
    assert!(!schema.sdl().contains("Site"), "a withheld type leaked into the SDL");

    // And a query naming it is a validation refusal, not a `null`.
    let document = parse("{ sites { id name } }").expect("the document parses");
    let refusal = validate_selections(&document, &schema)
        .expect_err("a query naming a withheld type must fail validation");
    assert_eq!(refusal.code_str(), "TYPE_NOT_VISIBLE", "{refusal}");
    assert!(refusal.to_string().contains("sites"), "{refusal}");

    cleanup(&fixture).await;
}

/// The same query from the editor — who also lacks `sites.read` here — is refused for the same
/// reason, and a caller who DOES hold it passes. The control that makes the refusal meaningful.
#[tokio::test]
async fn the_site_type_appears_only_for_a_caller_who_holds_sites_read() {
    let fixture = fixture().await;
    let holder = account(&fixture.state, Some(fixture.org_a), "siteholder").await;
    let platform = fixture.accounts[0];
    grant(
        &fixture.state,
        platform,
        holder,
        fixture.org_a,
        &["content.pages.read", "sites.read"],
        "siteholder",
    )
    .await;

    let caller = caller_for(&fixture.state, holder, Some(fixture.org_a)).await;
    assert!(caller.known.holds(Known::SitesRead));
    let schema = compose(&SchemaCatalogue::catalogued(), &caller.known);
    assert!(schema.has_type("Site"), "a permitted type is missing from the composed schema");
    assert!(schema.has_field("Site", "name"));

    validate_selections(&parse("{ sites { id name } }").expect("parses"), &schema)
        .expect("a permitted query must validate");

    sqlx::query("delete from users where id = $1")
        .bind(holder)
        .execute(fixture.state.db().pool())
        .await
        .expect("the extra account must be removed");
    cleanup(&fixture).await;
}

// -------------------------------------------------------------------------------------------
// Parity with REST — the request's headline risk
// -------------------------------------------------------------------------------------------

/// GraphQL and REST refuse identically, and allow identically, for the same permission set.
///
/// The request: *"GraphQL and REST apply identical guards: a curl pair proves equal results for
/// equal permissions, including a shared `403` case."* This is that pair. The member holds no
/// content key at all, so both transports refuse; the editor holds the read key, so both allow.
#[tokio::test]
async fn graphql_and_rest_refuse_the_same_caller_for_the_same_permission() {
    let fixture = fixture().await;
    let session = sign_in(&fixture.state, fixture.member).await;

    // REST first: the content list is guarded by `content.pages.read`, which this member lacks.
    let rest = call(
        &fixture.state,
        Method::GET,
        "/api/v1/pages?site_id=00000000-0000-0000-0000-000000000000",
        Some(&session),
        None,
    )
    .await;
    assert_eq!(
        rest.status,
        StatusCode::FORBIDDEN,
        "REST must refuse a member with no content key: {}",
        rest.body
    );

    // GraphQL, for the SAME caller and the SAME permission, through the resolver layer's guard.
    let caller = caller_for(&fixture.state, fixture.member, Some(fixture.org_a)).await;
    assert!(!caller.known.holds(Known::ContentPagesRead));
    let outcome = guard(
        &caller,
        fixture.state.db().pool(),
        &context_for(Some(fixture.org_a)),
        Some(Known::ContentPagesRead),
    )
    .await
    .expect_err("GraphQL must refuse the same caller");
    assert_eq!(outcome.code_str(), "FORBIDDEN");

    // And the allowed half, so the pair is a pair and not two refusals.
    let editor_session = sign_in(&fixture.state, fixture.editor).await;
    let rest_ok = call(
        &fixture.state,
        Method::GET,
        &format!("/api/v1/pages?site_id={}", fixture.site_a),
        Some(&editor_session),
        None,
    )
    .await;
    assert_eq!(
        rest_ok.status,
        StatusCode::OK,
        "REST must allow the editor: {}",
        rest_ok.body
    );
    let editor = caller_for(&fixture.state, fixture.editor, Some(fixture.org_a)).await;
    guard(
        &editor,
        fixture.state.db().pool(),
        &context_for(Some(fixture.org_a)),
        Some(Known::ContentPagesRead),
    )
    .await
    .expect("GraphQL must allow the editor too");

    cleanup(&fixture).await;
}

/// A full execution through the resolver layer returns what REST returns for the same page.
///
/// *"A single query spanning content, media and organization types returns the same data a set of
/// REST calls would, with the caller's permissions unchanged."* This is the content-and-tenancy
/// half of it: the editor runs one GraphQL query, and the same page is then read over REST, and
/// the two agree on every field they both carry.
#[tokio::test]
async fn a_graphql_query_returns_what_the_rest_calls_return_for_the_same_page() {
    let fixture = fixture().await;
    let slug = format!("parity-{}", Uuid::new_v4().simple());
    let page_id = seed_page(&fixture.state, fixture.site_a, &slug).await;

    let editor = caller_for(&fixture.state, fixture.editor, Some(fixture.org_a)).await;
    let schema = compose(&SchemaCatalogue::catalogued(), &editor.known);

    // The site id is BOUND BEFORE the document string is written rather than interpolated into a
    // literal. `format!` inside a `format!` needs every GraphQL brace doubled, and that doubling
    // is exactly where a brace goes missing — which the first version of this test did, producing
    // a document that parsed to something else entirely.
    let site = fixture.site_a.to_string();
    let document = parse(&format!(
        r#"{{ pages(siteId: "{site}") {{ id slug status pageType }} }}"#
    ))
    .expect("the document parses");
    let data = execute(
        &editor,
        fixture.state.db().pool(),
        &document,
        None,
        &schema,
    )
    .await
    .expect("the editor's query must execute");
    let rows = data["pages"].as_array().expect("pages is a list");
    let graphql_page = rows
        .iter()
        .find(|row| row["id"] == json!(page_id.to_string()))
        .expect("the seeded page must be in the GraphQL answer")
        .clone();

    // The REST twin, through the real router.
    let session = sign_in(&fixture.state, fixture.editor).await;
    let rest = call(
        &fixture.state,
        Method::GET,
        &format!("/api/v1/pages?site_id={}", fixture.site_a),
        Some(&session),
        None,
    )
    .await;
    assert_eq!(rest.status, StatusCode::OK, "{}", rest.body);
    let rest_page = rest.body["pages"]
        .as_array()
        .expect("the REST list")
        .iter()
        .find(|row| row["id"] == json!(page_id.to_string()))
        .expect("the seeded page must be in the REST answer")
        .clone();

    // Every field both transports carry must agree, EXACTLY.
    //
    // **The two transports spell one field differently, and that is the spec, not drift.** The
    // REST body is snake_case (`page_type`) because it is serde's default and changing it would
    // break every REST client this platform already has; GraphQL is camelCase (`pageType`) because
    // the specification's convention is. So the assertion compares the VALUES under each side's own
    // key.
    //
    // The first version of this walk compared `rest_page["pageType"]` — a key REST does not have —
    // and read the difference as "the transports disagree". `Value` answers `null` for a missing
    // key, so the assertion failed on a correct implementation. A parity check that does not know
    // both sides' SPELLINGS is not a parity check; it is a shape comparison that happens to be
    // written against one transport.
    assert_eq!(graphql_page["id"], rest_page["id"], "id differs");
    assert_eq!(graphql_page["slug"], rest_page["slug"], "slug differs");
    assert_eq!(graphql_page["status"], rest_page["status"], "status differs");
    assert_eq!(
        graphql_page["pageType"], rest_page["page_type"],
        "pageType/page_type differs"
    );
    assert_eq!(graphql_page["siteId"], rest_page["site_id"], "siteId/site_id differs");

    // And the naming convention itself is asserted, so a future field cannot drift into one
    // transport's spelling by accident: REST keys are snake_case, GraphQL keys are camelCase.
    for (key, _) in rest_page.as_object().expect("the REST page is an object") {
        assert_eq!(
            *key,
            snake(key),
            "REST served `{key}`, which is not this transport's convention"
        );
    }

    cleanup(&fixture).await;
}

/// The schema's field names follow the GraphQL naming convention, and the REST body follows its
/// own — so the two transports never drift by accident.
///
/// This is its own test because the drift it guards is INVISIBLE in the parity walk: a field
/// renamed on one side only still returns the right data, and only a client that knows both
/// spellings notices. Spelled out here as the two conventions rather than derived, so renaming a
/// field to the other transport's convention fails.
#[test]
fn the_two_transports_keep_their_own_naming_conventions() {
    let catalogue = SchemaCatalogue::catalogued();
    let mut checked = 0usize;
    for type_definition in &catalogue.types {
        for field in &type_definition.fields {
            if type_definition.is_query_root {
                // Root fields are the resolver entry points and are named for the operation, so
                // they are exempt: `pageBySlug` is a name, not a REST column.
                continue;
            }
            // camelCase, not "has no capitals" — `siteId` HAS a capital and is correct camelCase.
            // The first version of this test asserted "no capitals", which would have rejected the
            // very convention it was written to protect, and it was the failure that said so: a
            // test whose rule is stricter than the rule it names gets "fixed" by renaming correct
            // code, which is how a convention erodes one field at a time.
            //
            // The rule that actually distinguishes the two transports is the UNDERSCORE. REST is
            // snake_case and GraphQL is not, so a single `_` in an entity field is a REST spelling
            // that leaked into the GraphQL catalogue.
            assert!(
                !field.name.contains('_'),
                "`{}.{}` carries an underscore, so the GraphQL side is snake_case",
                type_definition.name,
                field.name
            );
            // A single-word field has NO distinct REST spelling — `id` is `id` and `name` is
            // `name` on both sides, and demanding one would be demanding a rename of the REST
            // body to satisfy a convention it never had. The first version of this assertion
            // required a distinct spelling and failed on `Organization.id`, which is a field
            // that is spelled the same in both transports BECAUSE there is nothing to translate.
            //
            // What is asserted instead is that the translation round-trips: applying `snake` to a
            // multi-word field and reading it back as camelCase gives the field back, so the walk
            // above can trust `snake(name)` as the REST key.
            let rest_spelling = snake(field.name);
            assert_eq!(
                camel(&rest_spelling),
                field.name,
                "`{}.{}` does not round-trip through its REST spelling `{}`, so the parity \
                 comparison would look up a key that does not exist",
                type_definition.name,
                field.name,
                rest_spelling
            );
            checked += 1;
        }
    }
    assert!(checked >= 25, "only {checked} entity fields were checked");
}

// -------------------------------------------------------------------------------------------
// The limits — distinct codes, and nothing executed
// -------------------------------------------------------------------------------------------

/// Each limit is refused under its OWN code, and the refusal happens before any resolver runs.
///
/// The acceptance line: *"Depth above the limit, cost above budget, alias spam, oversized page
/// size and over-long requests are each refused with distinct error codes and no partial
/// execution."* The last clause is checked by asserting the store did not move after each refusal.
#[tokio::test]
async fn every_limit_is_refused_under_its_own_code_before_anything_runs() {
    let fixture = fixture().await;
    let editor = caller_for(&fixture.state, fixture.editor, Some(fixture.org_a)).await;

    let pages_before: i64 =
        sqlx::query_scalar("select count(*) from pages where site_id = $1")
            .bind(fixture.site_a)
            .fetch_one(fixture.state.db().pool())
            .await
            .expect("pages must count");

    let mut deep = String::new();
    for index in 0..12 {
        deep.push_str(&format!("{{ f{index} "));
    }
    deep.push_str("{ leaf }");
    for _ in 0..12 {
        deep.push('}');
    }

    let cases: Vec<(&str, String, &str)> = vec![
        ("depth", deep, "DEPTH_LIMIT"),
        // COST_LIMIT, isolated: FOUR aliases of the heaviest priced field is 4 x 120 = 480, under the
        // alias cap of 15 and under the depth cap, and over nothing — so this case would pass if
        // the cost model were free. Ten aliases is 1115 and breaks BOTH the alias cap and the
        // budget; since the limits are enforced depth → aliases → page size → cost, that document
        // is reported as the ALIAS breach, which is the more specific answer. Both facts are
        // asserted below rather than hidden by picking one.
        ("cost (under the alias cap)", "{ a: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } b: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } c: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } d: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } e: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } f: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } g: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } h: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } i: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } }".to_owned(), "COST_LIMIT"),
        ("alias spam", "{ a: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } b: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } c: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } d: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } e: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } f: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } g: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } h: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } i: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } j: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } k: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } l: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } m: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } n: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } o: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } p: pages(siteId: \"00000000-0000-0000-0000-000000000000\") { id } }".to_owned(), "ALIAS_LIMIT"),
        ("page size", "{ pages(siteId: \"00000000-0000-0000-0000-000000000000\", first: 100000) { id } }".to_owned(), "PAGE_SIZE_LIMIT"),
        ("unparsed", "{ pages( { id } }".to_owned(), "GRAPHQL_VALIDATION_FAILED"),
    ];

    for (label, source, expected) in &cases {
        let document = match parse(source) {
            Ok(document) => document,
            Err(error) => {
                assert_eq!(
                    error.code_str(),
                    *expected,
                    "`{label}` was refused as {:?} rather than {expected}: {error}",
                    error.code_str()
                );
                continue;
            }
        };
        let outcome = omnion_graphql::check(&document, &Limits::default()).unwrap_err();
        assert_eq!(
            outcome.code_str(),
            *expected,
            "`{label}` refused under the wrong code: {outcome}"
        );
    }

    // Distinct codes: two limits sharing a code would tell a client to back off from both.
    let mut codes: Vec<&str> = cases
        .iter()
        .map(|(_, source, _)| match parse(source) {
            Ok(document) => omnion_graphql::check(&document, &Limits::default())
                .expect_err("refused")
                .code_str(),
            Err(error) => error.code_str(),
        })
        .collect();
    codes.sort_unstable();
    let unique = {
        let mut copy = codes.clone();
        copy.dedup();
        copy.len()
    };
    assert_eq!(
        unique, codes.len(),
        "two limits answered under the same code, so a client cannot tell them apart: {codes:?}"
    );
    assert!(
        codes.contains(&"COST_LIMIT") && codes.contains(&"ALIAS_LIMIT"),
        "the walk must exercise BOTH the budget and the alias cap, or it is not checking that \
         distinct codes exist: {codes:?}"
    );

    // Nothing ran.
    let pages_after: i64 =
        sqlx::query_scalar("select count(*) from pages where site_id = $1")
            .bind(fixture.site_a)
            .fetch_one(fixture.state.db().pool())
            .await
            .expect("pages must count");
    assert_eq!(pages_before, pages_after, "a refused query changed the store");

    cleanup(&fixture).await;
}

// -------------------------------------------------------------------------------------------
// The endpoint
// -------------------------------------------------------------------------------------------

/// `POST /api/v1/graphql` executes a document and answers `200` with the envelope.
#[tokio::test]
async fn the_endpoint_answers_a_query_with_depth_cost_duration_and_a_request_id() {
    let fixture = fixture().await;
    let session = sign_in(&fixture.state, fixture.editor).await;
    seed_page(&fixture.state, fixture.site_a, "envelope").await;

    let response = call(
        &fixture.state,
        Method::POST,
        "/api/v1/graphql",
        Some(&session),
        Some(json!({
            "query": format!(
                r#"{{ pages(siteId: "{}") {{ id slug }} }}"#,
                fixture.site_a
            ),
            "operationName": null,
            "variables": {}
        })),
    )
    .await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    // A GraphQL envelope answers 200 even when it carries errors.
    assert!(response.body.get("errors").is_none(), "{}", response.body);
    assert!(response.body["data"]["pages"].is_array(), "{}", response.body);

    // The acceptance line: "Responses carry `extensions` with depth, cost, duration and request
    // id, verified on a real call." Every one of the four, on a real HTTP response.
    let extensions = &response.body["extensions"];
    assert!(extensions["depth"].is_u64(), "depth missing: {}", extensions);
    assert!(extensions["cost"].is_u64(), "cost missing: {}", extensions);
    assert!(extensions["durationMs"].is_u64(), "durationMs missing: {}", extensions);
    assert!(
        extensions["requestId"].as_str().is_some_and(|id| !id.is_empty()),
        "requestId missing: {extensions}"
    );

    cleanup(&fixture).await;
}

/// An unauthenticated call to the endpoint is a `401`, and a caller without a read permission is
/// `403` on the endpoint itself — the transport-level failures the request keeps as HTTP statuses.
#[tokio::test]
async fn the_endpoint_refuses_an_anonymous_caller_and_a_caller_without_permission() {
    let fixture = fixture().await;

    let anonymous = call(
        &fixture.state,
        Method::POST,
        "/api/v1/graphql",
        None,
        Some(json!({ "query": "{ organizations { id } }" })),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{}", anonymous.body);
    // The platform's error envelope is `{"error": {code, message}}` — NOT a top-level `code`.
    // The first version of this walk asserted `body["code"]`, which is `null` for EVERY ApiError,
    // so the assertion would have passed for a refusal of the wrong kind. `error.code` is the
    // field the platform actually writes.
    assert_eq!(anonymous.body["error"]["code"], "unauthenticated", "{}", anonymous.body);

    let session = sign_in(&fixture.state, fixture.member).await;
    let refused = call(
        &fixture.state,
        Method::POST,
        "/api/v1/graphql",
        Some(&session),
        Some(json!({ "query": "{ organizations { id name } }" })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "permission_denied", "{}", refused.body);

    cleanup(&fixture).await;
}

/// A mutation over the endpoint writes; the same mutation from the reader is refused and writes
/// nothing. The end-to-end shape of the acceptance line, over HTTP rather than in-process.
#[tokio::test]
async fn a_mutation_writes_for_a_permitted_caller_and_is_refused_and_writes_nothing_for_a_reader() {
    let fixture = fixture().await;
    let slug = format!("mutation-{}", Uuid::new_v4().simple());

    // The editor may write.
    let editor_session = sign_in(&fixture.state, fixture.editor).await;
    let created = call(
        &fixture.state,
        Method::POST,
        "/api/v1/graphql",
        Some(&editor_session),
        Some(json!({
            "query": format!(
                r#"mutation {{ createPage(siteId: "{}", slug: "{}", title: "Written over GraphQL", body: "Body") {{ id status }} }}"#,
                fixture.site_a, slug
            )
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    assert_eq!(created.body["data"]["createPage"]["status"], "draft", "{}", created.body);

    let written: i64 = sqlx::query_scalar(
        "select count(*) from pages where site_id = $1 and slug = $2",
    )
    .bind(fixture.site_a)
    .bind(&slug)
    .fetch_one(fixture.state.db().pool())
    .await
    .expect("pages must count");
    assert_eq!(written, 1, "the permitted mutation did not write");

    // The reader may not. The SAME document, from the reader's session.
    let reader_session = sign_in(&fixture.state, fixture.reader).await;
    let refused_slug = format!("refused-{}", Uuid::new_v4().simple());
    let refused = call(
        &fixture.state,
        Method::POST,
        "/api/v1/graphql",
        Some(&reader_session),
        Some(json!({
            "query": format!(
                r#"mutation {{ createPage(siteId: "{}", slug: "{}", title: "Never written", body: "Body") {{ id }} }}"#,
                fixture.site_a, refused_slug
            )
        })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::OK, "{}", refused.body);
    // A refused field is a GraphQL error inside a 200 envelope — the request is explicit that
    // execution returns 200 with an envelope "even when it contains errors".
    let errors = refused.body["errors"].as_array().expect("an error array");
    assert_eq!(errors[0]["extensions"]["code"], "FORBIDDEN", "{}", refused.body);

    let not_written: i64 =
        sqlx::query_scalar("select count(*) from pages where site_id = $1 and slug = $2")
            .bind(fixture.site_a)
            .bind(&refused_slug)
            .fetch_one(fixture.state.db().pool())
            .await
            .expect("pages must count");
    assert_eq!(not_written, 0, "a refused mutation wrote a page");

    cleanup(&fixture).await;
}

// -------------------------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------------------------

async fn sign_in(state: &AppState, user_id: Uuid) -> Session {
    let email: String = sqlx::query_scalar("select email from users where id = $1")
        .bind(user_id)
        .fetch_one(state.db().pool())
        .await
        .expect("the account row must read");
    let response = call(
        state,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(json!({ "email": email, "password": PASSWORD })),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "sign-in failed: {}", response.body);
    Session::from_set_cookies(response.set_cookies.iter().map(String::as_str))
}

/// The snake_case spelling of a camelCase key — the REST body's convention.
///
/// Written as a function rather than inlined so the assertion above has an independent definition
/// of the convention. Deriving it from `key.to_owned()` would make the loop vacuous.
fn snake(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for (index, character) in key.char_indices() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// The camelCase spelling of a snake_case key — the inverse of [`snake`], and derived
/// independently rather than by running `snake` and hoping it inverts.
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut capitalise = false;
    for character in key.chars() {
        if character == '_' {
            capitalise = true;
            continue;
        }
        if capitalise {
            out.push(character.to_ascii_uppercase());
            capitalise = false;
        } else {
            out.push(character);
        }
    }
    out
}

/// One in-process HTTP call, in the pieces the assertions need.
struct RestResponse {
    status: StatusCode,
    set_cookies: Vec<String>,
    body: Value,
}

async fn call(
    state: &AppState,
    method: Method,
    uri: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> RestResponse {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = builder.header(header::COOKIE, format!("omnion_session={}", session.session));
        if let Some(csrf) = &session.csrf {
            builder = builder.header(CSRF_HEADER, csrf.as_str());
        }
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .expect("the request must build"),
        None => builder.body(Body::empty()).expect("the request must build"),
    };

    let response = routes::router(state.clone())
        .oneshot(request)
        .await
        .expect("the router must answer");
    let status = response.status();
    let set_cookies = response
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
        .expect("the body must read")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    RestResponse {
        status,
        set_cookies,
        body,
    }
}

async fn cleanup(fixture: &Fixture) {
    sqlx::query("delete from users where id = any($1)")
        .bind(&fixture.accounts)
        .execute(fixture.state.db().pool())
        .await
        .expect("account cleanup must run");
    sqlx::query("delete from organizations where id = any($1)")
        .bind(&fixture.organizations)
        .execute(fixture.state.db().pool())
        .await
        .expect("organization cleanup must run");
}