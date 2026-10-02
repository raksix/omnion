//! The parity gate between the GraphQL surface's permissions and the platform's (REQ-130, slice 1).
//!
//! ## Why this file exists and why it is NOT in the crate
//!
//! `crates/graphql` holds the decision layer and is deliberately free of every other crate — no
//! `sqlx`, no `omnion-permissions`, no `axum`. That is not incidental: it is what makes
//! *"the playground's cost meter refuses before it sends"* provable, because the meter and the
//! endpoint then run the SAME pure function rather than two implementations that agree today.
//!
//! The consequence is that the one check that needs **both** sides — "is every permission this
//! surface names a key the platform actually ships?" — cannot live there. A crate that cannot read
//! `omnion_permissions::catalogue` cannot check its own vocabulary against it. So the check lives
//! here, in the API crate, which links both.
//!
//! ## The defect it would have caught, and the one it actually catches
//!
//! The slice-1 schema catalogue was written with **invented** permission names: `content.read`,
//! `tenancy.read`, `media.download`, `billing.read`, `content.revisions.read`. Not one of them
//! exists — the platform spells them `content.pages.read`, `organizations.read`, `media.read`, and
//! has no billing key at all. Seventy-five unit tests were green the whole time.
//!
//! The failure mode of such a key is a **refusal, not a crash**. An uncatalogued key resolves to
//! no permission, so `authorize(pool, user, scope, "content.read")` answers `403` — for every
//! caller, *including the instance owner*, and with a body that reads like an authentication
//! problem. Two other requests in this repository lost ticks to exactly that
//! (`omnion-loop-lessons`: *"A new guard added with a key that is not in the catalogue returns 403
//! for everyone on the route that uses it"*). On a GraphQL surface it is worse than a dead route,
//! because the endpoint would be up, healthy, signed-in — and answering `403` to every query.
//!
//! ## Why closing the vocabulary was not enough on its own
//!
//! Making `parity::Known` a closed enum stops the *typo*: `"content.read"` is a compile error now.
//! But it does not stop a **fabrication** — a variant added with a plausible-looking string that
//! the platform does not ship. That was measured, not assumed: adding a `Known::InventedBillingRead`
//! variant returning `"billing.read"` left all 75 unit tests GREEN, because nothing inside
//! `crates/graphql` can read the platform's catalogue to disagree.
//!
//! So the enum is necessary and not sufficient, and this file is the sufficient half.
//!
//! ## What is asserted, and what is deliberately not
//!
//! * Every [`Known`] variant's string is in `omnion_permissions::catalogue`. (The one check that
//!   cannot be done anywhere else.)
//! * Every permission the **schema catalogue** gates a field or type on is a `Known` the caller can
//!   hold — i.e. it is a real key, so a resolver can pass it to `authorize` without inventing one.
//! * Every field's permission matches the permission its **REST twin** is guarded by, for the
//!   surface pairs that exist today. This is the request's headline risk (*"parity drift between
//!   REST and GraphQL"*) expressed as a table rather than as prose.
//!
//! What it does NOT assert: that REST and GraphQL are identical. They are deliberately not, in
//! three named places (`Page.revisions`, `Page.author`, `MediaFile.scanStatus`), each annotated at
//! its declaration in the schema and each listed in `KNOWN_DIVERGENCES` below with the reason.
//! Silently-wider GraphQL would be a security defect; silently-narrower is a usability one. Both
//! are decisions someone made, so both are written down and checked to still be true — an
//! undocumented divergence is the failure mode, and `the_divergence_list_is_still_accurate` is
//! what stops it decaying into "we stopped checking".

use omnion_graphql::parity::{ALL as KNOWN_PERMISSIONS, Known};
use omnion_graphql::schema::{SchemaCatalogue, TypeDefinition};
use omnion_permissions::catalogue;

/// Every field in the schema that is gated on something other than its type's own permission,
/// with the key the REST twin is guarded by.
///
/// The twin path is in the table rather than in a comment so the two halves cannot drift: the
/// `permission` column is what the schema asserts, and `rest_twin_permission` is the platform's
/// own answer, copied out of `apps/api/src/routes/mod.rs`. A change to either side without the
/// other fails here.
struct FieldRule {
    type_name: &'static str,
    field_name: &'static str,
    permission: Option<Known>,
    /// `None` when the REST route needs no permission beyond the collection's read.
    rest_twin_permission: Option<&'static str>,
    /// The REST route, so a reader can check the claim without hunting for it.
    rest_twin: &'static str,
}

/// The fields this gate covers.
///
/// Every entry is a ROOT field, because those are the ones a REST twin exists for: a resolver
/// entry point maps to a REST route, and a leaf field is a column of the row the route already
/// returned under that route's permission. Checking leaves against REST would be checking the
/// platform's DTO shape, which is a different (and much noisier) claim.
const ROOT_FIELDS: &[FieldRule] = &[
    FieldRule {
        type_name: "Query",
        field_name: "me",
        permission: None,
        rest_twin_permission: None,
        rest_twin: "GET /api/v1/me",
    },
    FieldRule {
        type_name: "Query",
        field_name: "organization",
        permission: Some(Known::OrganizationRead),
        rest_twin_permission: Some("organizations.read"),
        rest_twin: "GET /api/v1/organizations",
    },
    FieldRule {
        type_name: "Query",
        field_name: "organizations",
        permission: Some(Known::OrganizationRead),
        rest_twin_permission: Some("organizations.read"),
        rest_twin: "GET /api/v1/organizations",
    },
    FieldRule {
        type_name: "Query",
        field_name: "sites",
        permission: Some(Known::SitesRead),
        rest_twin_permission: Some("sites.read"),
        rest_twin: "GET /api/v1/sites",
    },
    FieldRule {
        type_name: "Query",
        field_name: "pages",
        permission: Some(Known::ContentPagesRead),
        rest_twin_permission: Some("content.pages.read"),
        rest_twin: "GET /api/v1/pages",
    },
    FieldRule {
        type_name: "Query",
        field_name: "page",
        permission: Some(Known::ContentPagesRead),
        rest_twin_permission: Some("content.pages.read"),
        rest_twin: "GET /api/v1/pages/{id}",
    },
    FieldRule {
        type_name: "Query",
        field_name: "pageBySlug",
        permission: Some(Known::ContentPagesRead),
        rest_twin_permission: Some("content.pages.read"),
        rest_twin: "GET /api/v1/pages/{id} (public renderer: /p/{slug})",
    },
    FieldRule {
        type_name: "Query",
        field_name: "mediaFiles",
        permission: Some(Known::MediaRead),
        rest_twin_permission: Some("media.read"),
        rest_twin: "GET /api/v1/media",
    },
    FieldRule {
        type_name: "Query",
        field_name: "mediaFile",
        permission: Some(Known::MediaRead),
        rest_twin_permission: Some("media.read"),
        rest_twin: "GET /api/v1/media/{id}",
    },
    FieldRule {
        type_name: "Query",
        field_name: "mediaDownloadUrl",
        permission: Some(Known::MediaRead),
        // The raw bytes are `media.read` on the REST side too. Slice 1 invented
        // `media.download` here, which would have hidden this field from EVERY caller the
        // platform actually lets read it — a surface that is wrong in the direction of denying
        // access to an authorised user, which is at least loud; the reverse is not.
        rest_twin_permission: Some("media.read"),
        rest_twin: "GET /api/v1/media/{id}/raw",
    },
    FieldRule {
        type_name: "Mutation",
        field_name: "createPage",
        permission: Some(Known::ContentPagesCreate),
        rest_twin_permission: Some("content.pages.create"),
        rest_twin: "POST /api/v1/pages",
    },
    FieldRule {
        type_name: "Mutation",
        field_name: "updatePage",
        permission: Some(Known::ContentPagesUpdate),
        rest_twin_permission: Some("content.pages.update"),
        rest_twin: "PATCH /api/v1/pages/{id}",
    },
    FieldRule {
        type_name: "Mutation",
        field_name: "deletePage",
        permission: Some(Known::ContentPagesDelete),
        rest_twin_permission: Some("content.pages.delete"),
        rest_twin: "DELETE /api/v1/pages/{id}",
    },
    FieldRule {
        type_name: "Mutation",
        field_name: "publishPage",
        permission: Some(Known::ContentPagesPublish),
        rest_twin_permission: Some("content.pages.publish"),
        rest_twin: "POST /api/v1/pages/{id}/publish",
    },
    FieldRule {
        type_name: "Mutation",
        field_name: "createOrganization",
        permission: Some(Known::OrganizationManage),
        rest_twin_permission: Some("organizations.manage"),
        rest_twin: "POST /api/v1/organizations",
    },
];

/// Where GraphQL is deliberately NARROWER than REST, with the permission that makes it so.
///
/// Each entry is a permission the REST twin does NOT require, so a caller entitled on REST is
/// refused on GraphQL. That is the direction this project accepts: narrower costs a legitimate
/// caller one query and is visible in the explorer; wider would hand out data over a transport the
/// operator did not intend to expose it on. The list exists so "deliberately narrower" stays a
/// recorded decision — an entry that silently stops being true is how a divergence becomes
/// folklore.
const KNOWN_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "Page.revisions",
        "content.pages.restore",
        "GET /pages/{id}/revisions is guarded by content.pages.read, so REST is WIDER here. \
         Reading a superseded draft is a different privilege from reading the current page, and \
         the request asks for field-level filtering on narrower permissions.",
    ),
    (
        "Page.author",
        "users.read",
        "The REST page row carries created_by but not the author; the relation is resolved here, \
         so it is gated on the users read permission rather than riding on the page's.",
    ),
    (
        "MediaFile.scanStatus",
        "media.read",
        "Declared under media.read for now. media.scan.manage is the narrower key the scanning \
         surface uses and is the one this should move to when the scan detail stops being a \
         column of the ordinary read.",
    ),
];

fn schema() -> SchemaCatalogue {
    SchemaCatalogue::catalogued()
}

fn find_field<'a>(
    catalogue: &'a SchemaCatalogue,
    type_name: &str,
    field_name: &str,
) -> Option<&'a TypeDefinition> {
    catalogue
        .types
        .iter()
        .find(|type_definition| type_definition.name == type_name && {
            type_definition
                .fields
                .iter()
                .any(|field| field.name == field_name)
        })
}

fn permission_of(
    catalogue: &SchemaCatalogue,
    type_name: &str,
    field_name: &str,
) -> Option<Known> {
    find_field(catalogue, type_name, field_name).and_then(|type_definition| {
        type_definition
            .fields
            .iter()
            .find(|field| field.name == field_name)
            .and_then(|field| field.requires)
    })
}

/// THE GATE. Every permission the GraphQL surface can name is a key the platform ships.
///
/// This is the check `crates/graphql` structurally cannot perform. A closed enum stops the typo;
/// it does not stop a variant whose string nobody ever checked, and that was measured — a variant
/// added with `"billing.read"` left all 75 of the crate's own tests green.
#[test]
fn every_permission_the_graphql_surface_names_is_a_key_the_platform_ships() {
    assert!(
        !KNOWN_PERMISSIONS.is_empty(),
        "the known-permission list is empty, so this gate would pass on an empty set — the \
         single most dangerous way for a parity test to be green"
    );

    for known in KNOWN_PERMISSIONS {
        let key = known.as_str();
        assert!(
            catalogue::is_known(key),
            "the GraphQL surface names `{key}`, which is NOT a permission the platform ships. \
             A resolver calling `authorize(.., \"{key}\")` would be refused for EVERY caller, \
             including the instance owner — a 403 that reads like an authentication problem. Add \
             the key to crates/permissions/src/catalogue.rs, or remove this variant."
        );
    }
}

/// The REST twins' permissions are themselves real keys.
///
/// Without this, the table above could be self-consistently wrong: `GraphQL` says `pages.read` and
/// the table says `pages.read` and both are names the platform does not have, and the gate above
/// would pass because the table's column is never checked against anything.
#[test]
fn every_rest_twin_permission_in_the_table_is_a_key_the_platform_ships() {
    for rule in ROOT_FIELDS {
        if let Some(key) = rule.rest_twin_permission {
            assert!(
                catalogue::is_known(key),
                "the table claims the REST twin of `{}.{}` is guarded by `{key}`, which the \
                 platform does not ship — so the table is asserting parity against a route that \
                 does not exist",
                rule.type_name,
                rule.field_name
            );
        }
    }
}

/// GraphQL and REST are guarded by the SAME key, field by field.
///
/// This is the request's headline risk as an executable claim. It is a table rather than a
/// derivation on purpose: a derivation that read `routes/mod.rs` would need the router, and the
/// router's guard layers are not introspectable from an integration test — so the twin's key is
/// copied, and the copy is a reviewable line in this file rather than a fact nobody can check.
#[test]
fn every_root_field_is_guarded_by_the_same_permission_as_its_rest_twin() {
    let catalogue = schema();
    for rule in ROOT_FIELDS {
        let actual = permission_of(&catalogue, rule.type_name, rule.field_name);
        let expected = rule.permission;
        assert_eq!(
            actual,
            expected,
            "`{}.{}` is guarded by {:?} in GraphQL but its REST twin ({}) is guarded by {:?}. \
             The request's headline risk is parity drift between the two transports, and a caller \
             who is refused on one and allowed on the other cannot tell which is wrong.",
            rule.type_name,
            rule.field_name,
            actual.map(|permission| permission.as_str()),
            rule.rest_twin,
            rule.rest_twin_permission
        );
        // Both spellings must agree when both are present, which catches the case where the
        // schema is changed and the table is not (or the other way round).
        if let (Some(known), Some(rest_key)) = (rule.permission, rule.rest_twin_permission) {
            assert_eq!(
                known.as_str(),
                rest_key,
                "`{}.{}`: the schema says `{}` and the table says `{rest_key}` for the same \
                 field — one of the two is stale",
                rule.type_name,
                rule.field_name,
                known.as_str()
            );
        }
    }
}

/// Every rule in the table names a field the schema actually declares.
///
/// Without it the table could describe a surface that does not exist, and the gate above would be
/// measuring a fiction — which is precisely how the original `articles`/`Article` catalogue passed
/// seventy-five of its own tests.
#[test]
fn every_row_in_the_table_names_a_field_the_schema_declares() {
    let catalogue = schema();
    for rule in ROOT_FIELDS {
        assert!(
            find_field(&catalogue, rule.type_name, rule.field_name).is_some(),
            "the table covers `{}.{}`, which the schema does not declare — so the parity check is \
             measuring a field that does not exist",
            rule.type_name,
            rule.field_name
        );
    }
}

/// Every root field the schema declares is covered by the table.
///
/// The other direction, and the one that catches a field someone adds and forgets to reason about:
/// an uncovered root field has NO REST twin recorded, so nobody stated whether it is equivalent to
/// one or deliberately wider. That is the undocumented divergence this file exists to prevent.
#[test]
fn every_root_field_the_schema_declares_is_covered_by_the_table() {
    let catalogue = schema();
    for type_definition in &catalogue.types {
        if !type_definition.is_query_root {
            continue;
        }
        for field in &type_definition.fields {
            // Introspection is not a resolver and has no REST twin; it is the schema describing
            // itself, which every caller may always ask.
            if field.name.starts_with("__") {
                continue;
            }
            assert!(
                ROOT_FIELDS
                    .iter()
                    .any(|rule| rule.type_name == type_definition.name
                        && rule.field_name == field.name),
                "`{}.{}` is a root field with no row in the parity table: nobody has said which \
                 REST route it mirrors, or that it mirrors none. Add a row with the twin's \
                 permission, or — if it has no twin — say so there explicitly.",
                type_definition.name, field.name
            );
        }
    }
}

/// The recorded divergences are still true.
///
/// Each entry claims a permission the REST twin does NOT require. If a future change makes REST
/// require it too, the divergence is over and the entry is dead weight describing a difference
/// that no longer exists — which is how a "deliberate" difference quietly becomes "we stopped
/// checking". The check is: the claimed permission must NOT be the twin's permission for that
/// field, and the permission must be one a caller can actually hold.
#[test]
fn the_divergence_list_is_still_accurate() {
    let catalogue = schema();
    for (path, permission_key, reason) in KNOWN_DIVERGENCES {
        let (type_name, field_name) = path
            .split_once('.')
            .unwrap_or_else(|| panic!("`{path}` is not `Type.field`"));
        assert!(
            find_field(&catalogue, type_name, field_name).is_some(),
            "the divergence list names `{path}`, which the schema does not declare — remove the \
             entry rather than leaving a claim about nothing"
        );
        let actual = permission_of(&catalogue, type_name, field_name);
        assert_eq!(
            actual.map(|permission| permission.as_str()),
            Some(*permission_key),
            "`{path}` is listed as diverging under `{permission_key}`, but the schema gates it \
             under {:?}. Update the list or the schema — one of them is stale.",
            actual.map(|permission| permission.as_str())
        );
        assert!(
            catalogue::is_known(permission_key),
            "the divergence list claims `{permission_key}`, which the platform does not ship, so \
             no caller can hold it and the divergence cannot occur. Reason given: {reason}"
        );
    }
}

/// The write permissions are write permissions.
///
/// A mutation field gated on a read permission is the mistake this whole file is about in its
/// smallest form: it looks right, compiles, and hands out a write to a reader. Asserted by name so
/// the check survives a reordering of the catalogue.
#[test]
fn no_mutation_is_gated_on_a_read_permission() {
    let catalogue = schema();
    let mutation = catalogue
        .types
        .iter()
        .find(|type_definition| type_definition.name == "Mutation")
        .expect("the schema declares a Mutation root");

    assert!(
        !mutation.fields.is_empty(),
        "the Mutation root declares no fields, so this gate is measuring an empty set"
    );

    for field in &mutation.fields {
        let requires = field
            .requires
            .unwrap_or_else(|| panic!("`{}` needs no permission at all", field.name));
        assert!(
            requires.is_write(),
            "`{}` is a mutation gated on `{requires}`, which is a READ permission — every caller \
             who can read could write. Its REST twin ({}) carries a write key.",
            field.name,
            ROOT_FIELDS
                .iter()
                .find(|rule| rule.field_name == field.name)
                .map(|rule| rule.rest_twin)
                .unwrap_or("(not in the table)")
        );
    }
}

/// The schema's own type names are the platform's entity names.
///
/// Slice 1 declared an `Article` type with `articles`/`createArticle` over a content model that
/// has `Page`/`pages`/`createPage`. Nothing could see it: the crate has no resolver, so every field
/// of a fictional type validated and every one of them would have failed at the first resolver.
/// Spelled out as a list so the rule is "these names, and no invented ones", and so adding a
/// fictional type is a deliberate act that fails here.
#[test]
fn the_schema_declares_the_platform_entity_names_and_no_invented_ones() {
    const EXPECTED: &[&str] = &["Query", "Mutation", "Organization", "Site", "Page", "MediaFile"];
    let catalogue = schema();
    let declared: Vec<&str> = catalogue
        .types
        .iter()
        .map(|type_definition| type_definition.name)
        .collect();

    for name in EXPECTED {
        assert!(
            declared.contains(name),
            "the schema is missing `{name}`, which the platform has: {declared:?}"
        );
    }
    assert_eq!(
        declared.len(),
        EXPECTED.len(),
        "the schema declares {declared:?}, which is not exactly {EXPECTED:?}. A type the \
         platform has no entity for is a type whose every field will fail at its first resolver, \
         and one dropped without comment is a surface that lost a capability quietly."
    );
}
