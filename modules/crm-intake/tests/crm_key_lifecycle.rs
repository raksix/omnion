//! The endpoint key's lifecycle: which sources may hold one, and what "rotate" means.
//!
//! Run through `scripts/qa/run-crm-key-lifecycle.sh`.
//!
//! ## The defect this gate names
//!
//! `store::rotate_key`'s own doc comment has said, since the key surface shipped:
//!
//! > Refused for a source that has no key (a form-bound one), because "rotate" on a surface
//! > with no key is a button that appears to work and does nothing.
//!
//! **There was no refusal anywhere.** The function found the source, issued a key, and wrote
//! it — so `POST /api/v1/crm/intake/sources/{id}/rotate-key` turned a form-bound source into
//! a keyed endpoint. The panel hides the button (`source.kind === "endpoint"`, in
//! `intake-sources.tsx`), which is exactly the boundary the REQ says must be enforced by the
//! API and never by hiding buttons: `apps/api/src/routes/crm_intake.rs` documents its own
//! posture as "management attempts return 403, reads of invisible resources return 404" and
//! the intake module's risk note lists "the keyed endpoint accepts only the hashed key it was
//! issued" among the surface's promises.
//!
//! The direction of the defect is what makes it a finding rather than a nuisance. A form-bound
//! source is authenticated by the *form's own submission validation* — that is the reason
//! `create_source` deliberately issues it no key:
//!
//! ```ignore
//! let issued = (draft.kind == "endpoint").then(keys::issue_key);
//! ```
//!
//! Giving one a key does not merely store a stray digest. `find_source_by_key` looks a source
//! up by that digest **regardless of `kind`**, so after one rotate the source becomes
//! reachable at a public URL whose authentication is a 160-bit credential pasted into
//! somebody's own site — a second, undeclared capture path onto a source that was never
//! configured to have one, bypassing the form's own validation entirely.
//!
//! ## Why the existing gates were green
//!
//! The panel half was correct — the button is genuinely absent for a form source — and the
//! API half was untested, because every gate that touched rotation created an `endpoint`
//! source and asserted only that the digest changed. **A test that asserts the happy path
//! passes on an implementation with no guard at all.** The assertions below are the ones that
//! cannot pass on that implementation, plus the negative controls that must stay green so a
//! reader can see the gate names this defect rather than its neighbourhood.

use omnion_module_crm_intake::store::{self, SourcePatch};
// `NewIntakeSource` is re-exported from the crate root, not from `store` — a second path for
// the same type would be a second import to keep in step with the next re-export.
use omnion_module_crm_intake::{MappingEntry, NewIntakeSource};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (the shell script sets it)");
    PgPool::connect(&url).await.expect("the QA database")
}

async fn fresh_org(pool: &PgPool, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    // The slug is built from the label with every non-alphanumeric character removed, because
    // `organizations_slug_format` refuses anything else. The first draft interpolated the
    // label raw and all six tests failed with 23514 on the organization insert — a fixture
    // defect of mine, reported by the constraint rather than by a test, which is exactly what
    // a check constraint is for.
    let slug: String = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(org)
        .bind(format!("{label} {org}"))
        .bind(format!("{slug}-{}", org.simple()))
        .execute(pool)
        .await
        .expect("an organization for the test");
    org
}

async fn drop_org(pool: &PgPool, org: Uuid) {
    for sql in [
        "delete from crm_lead_events where lead_id in (select id from crm_leads where organization_id = $1)",
        "delete from crm_lead_submissions where source_id in (select id from crm_intake_sources where organization_id = $1)",
        "delete from crm_leads where organization_id = $1",
        "delete from crm_intake_sources where organization_id = $1",
        "delete from organizations where id = $1",
    ] {
        sqlx::query(sql)
            .bind(org)
            .execute(pool)
            .await
            .expect("the test's own rows");
    }
}

/// A **form-bound** source — the shape that must never hold a key.
async fn form_source(pool: &PgPool, org: Uuid) -> Uuid {
    let label = format!("key lifecycle {}", Uuid::new_v4().simple());
    let mut draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![MappingEntry::new("email", "email")],
        vec!["email".to_string()],
    );
    draft.kind = "form".to_string();
    draft.form_key = Some("contact".to_string());
    let (created, key) = store::create_source(pool, &draft).await.expect("a source");
    assert!(
        key.is_none(),
        "a form-bound source is issued no key at create — that is the state this gate protects",
    );
    created.id
}

/// A keyed endpoint source, returning its id and the clear key it was issued once.
async fn endpoint_source(pool: &PgPool, org: Uuid) -> (Uuid, String) {
    let label = format!("key lifecycle {}", Uuid::new_v4().simple());
    let draft = NewIntakeSource::endpoint(org, &label, None).with_mapping(
        vec![MappingEntry::new("email", "email")],
        vec!["email".to_string()],
    );
    let (created, key) = store::create_source(pool, &draft).await.expect("a source");
    let issued = key.expect("an endpoint source is issued a key");
    (created.id, issued.clear)
}

/// Whether a source's stored row carries a digest at all.
///
/// `None` and `Some("")` are both "no credential", and reading the column as a string would
/// answer `Some("")` for the first and let a caller treat the second as a real key.
async fn stored_hash(pool: &PgPool, source_id: Uuid) -> Option<String> {
    sqlx::query_scalar("select endpoint_key_hash from crm_intake_sources where id = $1")
        .bind(source_id)
        .fetch_one(pool)
        .await
        .expect("the source's key column")
}

async fn hint_of(pool: &PgPool, source_id: Uuid) -> Option<String> {
    sqlx::query_scalar("select endpoint_key_hint from crm_intake_sources where id = $1")
        .bind(source_id)
        .fetch_one(pool)
        .await
        .expect("the source's hint column")
}

/// The public answer to "is this key live?" — the one lookup a caller actually makes.
async fn resolvable_by_key(pool: &PgPool, key: &str) -> Option<Uuid> {
    store::find_source_by_key(pool, key)
        .await
        .expect("the public key lookup")
        .map(|source| source.id)
}

// -------------------------------------------------------------------------------------------
// The defect
// -------------------------------------------------------------------------------------------

/// Rotating a form-bound source must be refused, and must leave the row untouched.
///
/// The assertion that matters is the last one. A guard that refused the call and still wrote
/// a digest would satisfy "the caller was told no" while leaving the source reachable at a
/// public URL — so the row is read back after the refusal rather than trusted.
#[tokio::test]
async fn rotating_a_form_bound_source_is_refused_and_writes_no_key() {
    let pool = pool().await;
    let org = fresh_org(&pool, "rotate form").await;
    let source_id = form_source(&pool, org).await;

    // The outcome is matched rather than inspected for its value, because the *shape* is the
    // assertion: the caller must not learn the refusal through the `Ok(None)` channel, which
    // the handler reads as a missing source. `the_refusal_does_not_report_a_missing_source`
    // then says the same thing with the three arms named, so a reader who wants the detail
    // has one test that reads as a list.
    let outcome = store::rotate_key(&pool, org, source_id).await;
    assert!(
        outcome.is_err(),
        "rotating a source with no key must be refused; the doc comment on `rotate_key` \
         promises exactly this and the refusal did not exist"
    );

    assert_eq!(
        stored_hash(&pool, source_id).await,
        None,
        "the refusal must not leave a digest behind: `find_source_by_key` matches on the \
         digest regardless of kind, so a half-applied refusal is a live public endpoint"
    );
    assert_eq!(
        hint_of(&pool, source_id).await,
        None,
        "a refused rotation must not leave a hint either — the panel would then render a \
         credential that does not exist"
    );

    drop_org(&pool, org).await;
}

/// The refusal is a *refusal*, not a silent success: the caller's two ways of learning are
/// both checked, because "returned `Ok(None)`" and "returned `Err`" are different answers and
/// only one of them reaches the handler's `404`.
///
/// This is a separate test from the one above on purpose. Merging them would let an
/// implementation that returns `Ok(None)` pass both, and `Ok(None)` is what the handler maps
/// to `not_found("intake source")` — an answer that names the *source* as missing when the
/// source is right there. A wrong reason for a refusal is still a wrong answer.
#[tokio::test]
async fn the_refusal_does_not_report_a_missing_source() {
    let pool = pool().await;
    let org = fresh_org(&pool, "rotate reason").await;
    let source_id = form_source(&pool, org).await;

    match store::rotate_key(&pool, org, source_id).await {
        Ok(None) => panic!(
            "`Ok(None)` is the handler's `not_found(\"intake source\")`: it tells an operator \
             the source is gone, so they go looking for a source that exists. The refusal must \
             be an error, and the message must name the reason."
        ),
        Ok(Some(_)) => panic!("a form-bound source must never be issued a key by rotation"),
        Err(error) => {
            let code = error.code();
            assert_ne!(
                code, "not_found",
                "the code must not be a not-found; the source exists",
            );
            // The message has to name the surface, because the two lines an operator can act
            // on are "this source has no key" and "this source is a form, not an endpoint".
            let message = error.to_string().to_lowercase();
            assert!(
                message.contains("key"),
                "the refusal must name the key — an operator who cannot tell what is missing \
                 cannot act; got: {error}"
            );
        }
    }

    drop_org(&pool, org).await;
}

// -------------------------------------------------------------------------------------------
// Negative controls — these must stay green on any correct implementation
// -------------------------------------------------------------------------------------------

/// The happy path still works: rotation on a keyed endpoint replaces the digest and kills
/// the old key.
///
/// If this one ever goes red the guard has overreached, which is why it lives here and not in
/// the defect's own test.
#[tokio::test]
async fn rotating_a_keyed_endpoint_replaces_the_digest_and_kills_the_old_key() {
    let pool = pool().await;
    let org = fresh_org(&pool, "rotate endpoint").await;
    let (source_id, first) = endpoint_source(&pool, org).await;

    let issued = store::rotate_key(&pool, org, source_id)
        .await
        .expect("a keyed endpoint rotates")
        .expect("a fresh key");

    assert_ne!(issued.clear, first, "rotation issues a *new* key");
    assert_eq!(
        resolvable_by_key(&pool, &first).await,
        None,
        "the old key must stop working the instant the new one is saved — a window with two \
         live keys is the one the module's doc promises does not exist"
    );
    assert_eq!(
        resolvable_by_key(&pool, &issued.clear).await,
        Some(source_id),
        "the new key must resolve to the source it was issued for"
    );

    drop_org(&pool, org).await;
}

/// A source in another organization is not reachable by rotation at all.
///
/// This is the tenancy control, and it is asserted on the *refusal* rather than on the row:
/// the existing code returns `Ok(None)` for a source that is not the caller's, and changing
/// that to an error would leak the existence of another tenant's row through the difference.
#[tokio::test]
async fn a_source_in_another_organization_is_not_rotatable() {
    let pool = pool().await;
    let mine = fresh_org(&pool, "rotate tenant mine").await;
    let theirs = fresh_org(&pool, "rotate tenant theirs").await;
    let (theirs_source, theirs_key) = endpoint_source(&pool, theirs).await;

    let before = stored_hash(&pool, theirs_source).await;

    assert_eq!(
        store::rotate_key(&pool, mine, theirs_source).await.ok().flatten(),
        None,
        "another tenant's source must not be rotatable"
    );
    assert_eq!(
        stored_hash(&pool, theirs_source).await,
        before,
        "a refused cross-tenant rotation must not rewrite the row"
    );
    assert_eq!(
        resolvable_by_key(&pool, &theirs_key).await,
        Some(theirs_source),
        "the other tenant's key must survive a cross-tenant rotation attempt untouched"
    );

    drop_org(&pool, mine).await;
    drop_org(&pool, theirs).await;
}

/// A form-bound source stays invisible to the public key path — which is the property the
/// refusal exists to protect, asserted directly rather than through the rotation call.
///
/// If this ever goes red the *lookup* has widened, which is the deeper half of the defect and
/// the one a guard in `rotate_key` alone would not cover.
#[tokio::test]
async fn a_form_bound_source_is_not_reachable_by_any_key() {
    let pool = pool().await;
    let org = fresh_org(&pool, "form unreachable").await;
    let source_id = form_source(&pool, org).await;

    let source = store::find_source(&pool, org, source_id)
        .await
        .expect("the source row")
        .expect("the source exists");

    assert_eq!(
        source.endpoint_key_hash, None,
        "a form-bound source stores no digest",
    );
    // Even a key forged against nothing must not resolve: the lookup is by digest, so with no
    // digest stored there is nothing to match.
    assert_eq!(
        resolvable_by_key(&pool, "any-key-at-all").await,
        None,
        "a source with no digest must not be resolvable by the public key path"
    );

    drop_org(&pool, org).await;
}

/// A digest that reached a form-bound row by a route *other* than rotation still does not open
/// the public path.
///
/// This is the half `rotate_key`'s guard cannot cover, and it is the test that would go red if
/// only the guard existed. A digest is the *fact that a key was written*, not the boundary: a
/// restored dump, a hand-written insert, an import tool and a future kind's own migration all
/// put one on a row without asking `rotate_key`. So the row is given a real digest for a real
/// issued key, by raw SQL — the way those paths would — and the lookup must still refuse it.
///
/// The contrast is the point: *the digest is stored* passes on its own, and only the lookup can
/// tell an unkeyed row from an unaddressable one. Both are asserted, in that order, so a reader
/// sees which one the fix moved.
#[tokio::test]
async fn a_digest_written_onto_a_form_source_still_does_not_open_the_public_path() {
    let pool = pool().await;
    let org = fresh_org(&pool, "smuggled digest").await;
    let source_id = form_source(&pool, org).await;
    // A key that belongs to a real endpoint source, copied onto the form row by hand — the
    // exact state a restored dump or an import tool would leave behind.
    let (endpoint_id, borrowed) = endpoint_source(&pool, org).await;

    // Write a real digest onto the form-bound row by hand — a path that never calls rotate.
    sqlx::query(
        "update crm_intake_sources set endpoint_key_hash = $2, endpoint_key_hint = 'smug' \
         where id = $1",
    )
    .bind(source_id)
    .bind(omnion_module_crm_intake::hash_key(&borrowed))
    .execute(&pool)
    .await
    .expect("the hand-written digest");

    let digest_present: Option<String> =
        sqlx::query_scalar("select endpoint_key_hash from crm_intake_sources where id = $1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .expect("the source's key column");
    assert!(
        digest_present.is_some(),
        "the premise of this test: the digest really is stored, so a lookup that matched on \
         the digest alone would have served this row"
    );

    // The digest resolves to the *endpoint* row, never to the form-bound one that carries the
    // same value — and the id is what makes that readable. The first draft of this assertion
    // demanded `None` and failed with `Some(<endpoint id>)`: the two rows share a digest, and
    // `fetch_optional` returns the row that is legitimately addressable. That is the correct
    // product answer and the assertion was the thing that was wrong — a test that names the
    // wrong expected value sends the next reader to fix the product instead of the test.
    assert_eq!(
        resolvable_by_key(&pool, &borrowed).await,
        Some(endpoint_id),
        "the digest resolves to the key-bearing source, never to the form-bound row that \
         carries the same value — a key is not an address, the kind is"
    );

    // Now the control that keeps the assertion above honest: deactivate the *endpoint* row and
    // the same digest must resolve to nothing at all. Without it, the assertion would also pass
    // on a build where `find_source_by_key` answered `None` unconditionally — the same trap as
    // a test that measures its own fixture.
    sqlx::query("update crm_intake_sources set active = false where id = $1")
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .expect("deactivate the key-bearing source");
    assert_eq!(
        resolvable_by_key(&pool, &borrowed).await,
        None,
        "with no key-bearing row left, the same digest resolves to nothing — so the assertion \
         above was answering about the kind and not about the digest being unmatchable"
    );

    drop_org(&pool, org).await;
}

/// The patch path cannot be used to smuggle a key in either.
///
/// `update_source`'s `SourcePatch` has no key field and this test pins that: a key can only
/// enter through `create_source`'s one-time issue and `rotate_key`, both of which are covered
/// above. A patch field added later would be a second way to mint a credential, and the
/// panel's `SourcePatch` body would have to carry it.
#[tokio::test]
async fn an_update_does_not_mint_or_change_a_key() {
    let pool = pool().await;
    let org = fresh_org(&pool, "patch no key").await;
    let (source_id, issued) = endpoint_source(&pool, org).await;
    let before = stored_hash(&pool, source_id).await;

    let updated = store::update_source(
        &pool,
        org,
        source_id,
        &SourcePatch {
            name: Some("renamed".to_string()),
            ..SourcePatch::default()
        },
    )
    .await
    .expect("the update")
    .expect("the source still exists");

    assert_eq!(updated.name, "renamed", "the rename itself is what this test exercises");
    assert_eq!(
        stored_hash(&pool, source_id).await,
        before,
        "an ordinary edit must not touch the digest — the only way a key changes is rotation"
    );
    assert_eq!(
        resolvable_by_key(&pool, &issued).await,
        Some(source_id),
        "the key must still work after an ordinary edit"
    );

    drop_org(&pool, org).await;
}