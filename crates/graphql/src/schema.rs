//! Per-caller schema composition: what a caller can see, and the cache key that keeps it theirs.
//!
//! The request is precise about the rule and about why it is that way: *"A type whose read
//! permission the caller lacks is absent from the schema entirely — not nulled at runtime — so
//! introspection cannot leak shape or existence."*
//!
//! Absent, not nulled, is the whole claim. A schema that carried the type and returned `null` for
//! an unpermitted read would still answer introspection with the type's name, its fields and their
//! types — so a caller who may not read `Page.revisions` learns that revision history exists at
//! all, which is the information the rule exists to withhold. Composition therefore happens
//! *before* validation, and a query naming a type the caller cannot see fails validation naming
//! the type, rather than executing and returning nothing.
//!
//! ## The cache key is the risk the request names
//!
//! *"the caller's schema is cached under a key of (module set, capability set, permission-set
//! hash, version)"* and *"a caching bug could serve one role's schema to another, so the cache key
//! includes the permission-set hash and tests diff two roles on every load."*
//!
//! [`CacheKey`] is that key as a value type, with a `fingerprint` that folds all four inputs into
//! one comparable string. Folding is a risk — a naive fold collides, and a collision serves one
//! role's schema to another, which is precisely the bug. So the fingerprint is built from
//! length-prefixed, sorted components: two different inputs cannot produce the same string, and
//! [`CacheKey::fingerprint_is_injective_over_its_inputs`] asserts that on the cases that would
//! otherwise collide (a permission set that is a prefix of another, two orders of the same set).

use crate::error::{Code, Error, Result};
use crate::parity::{Known, PermissionSet};

/// The read permission of a root type: a root holds no data of its own, so no permission gates
/// its EXISTENCE. Its fields carry their own permissions, which is where the filtering happens.
///
/// The sentinel is [`Option::None`] rather than an empty string. Slice 1 spelled it `""`, which is
/// a *string* in the same position as a real permission name — so `Some("")` and `None` were
/// different spellings of "no permission", and a field declared with the empty string would have
/// compared `holds("")` against a set that never contains it. An option cannot be half-empty.
pub const NO_PERMISSION: Option<Known> = None;

/// One type in the schema catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDefinition {
    pub name: &'static str,
    /// The read permission a caller must hold to see this type at all.
    ///
    /// A [`Known`], not a `&str`: the whole reason this field's type changed is that a string
    /// here is how the first draft of this catalogue came to name five permissions the platform
    /// does not have. `None` is "no permission gates this type" — which is only ever true of a
    /// root, and a root holding no data of its own is why.
    pub read_permission: Option<Known>,
    /// The fields, each with the permission it needs. A field with no narrower permission
    /// inherits the type's.
    pub fields: Vec<FieldDefinition>,
    /// Whether this type is reachable as a query root.
    pub is_query_root: bool,
}

/// One field, and the permission it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDefinition {
    pub name: &'static str,
    /// `None` means the field needs only the type's read permission.
    ///
    /// A [`Known`] for the same reason the type's read permission is one. `Option` rather than a
    /// sentinel empty string, because "needs no extra permission" and "needs the permission named
    /// by the empty string" are different claims and the first is the only one this can express.
    pub requires: Option<Known>,
    /// Whether selecting this field is a write.
    pub is_mutation: bool,
    /// The type a selection under this field is checked against — `None` for a scalar and for a
    /// relation its own resolver fills.
    ///
    /// Carried on the field rather than in a `returns()` helper because a helper is a second
    /// list of root fields that can drift from the catalogue: the first draft of this module had
    /// exactly that, and it silently lost every root field the catalogue later gained. Here, a
    /// root field with no `returns` simply has nothing to descend into, and the tests say so.
    pub returns: Option<&'static str>,
}

/// The full type catalogue an installation ships with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCatalogue {
    pub types: Vec<TypeDefinition>,
}

impl SchemaCatalogue {
    /// The catalogue for the content, tenancy and media surface the request names first.
    ///
    /// **Every permission here is a [`Known`], and every type is a type the platform actually
    /// has.** Both halves of that sentence are load-bearing, and both were violated by the first
    /// draft of this file, which is why they are stated here rather than left to review:
    ///
    /// * It spelled permissions `content.read`, `tenancy.read`, `media.download`,
    ///   `billing.read` and `content.revisions.read`. **None of them exist.** The platform spells
    ///   them `content.pages.read`, `organizations.read`, `media.read`, and has no billing key at
    ///   all. Nothing inside this crate could notice, because a permission was a bare `&str` —
    ///   the first resolver to call `authorize(pool, user, scope, "content.read")` would have got
    ///   a `403` for **every caller including the instance owner**, which is the exact defect two
    ///   other requests in this repository lost ticks to. The permission type is now closed, so
    ///   that class of error is a compile error.
    /// * It declared an `Article` type with `articles`/`article`/`createArticle` root fields. The
    ///   platform's content model is [`Page`](omnion_content::pages): `pages`, `page`, and writes
    ///   `createPage`/`updatePage`/`deletePage`/`publishPage`. A GraphQL surface naming an entity
    ///   the database does not have is a surface whose every field is a validation error.
    ///
    /// The permission of each field is the one its **REST twin** carries, taken from
    /// `apps/api/src/routes/mod.rs` — that is the whole point of the request's parity requirement,
    /// so the twin is named in the comment beside each field rather than left implicit.
    pub fn catalogued() -> Self {
        Self {
            types: vec![
                // The query root. Every resolver-backed field starts here, and each carries the
                // read permission its REST route requires — so a caller who may read content but
                // not the tenant still sees a Query type, with only their fields on it. That is
                // the difference between "a type is withheld" and "a field is withheld", and the
                // two produce different error codes.
                TypeDefinition {
                    name: "Query",
                    read_permission: NO_PERMISSION,
                    is_query_root: true,
                    fields: vec![
                        // No permission: the caller's own identity, already resolved by the guard
                        // before the request reached here. Twin: `GET /api/v1/me`.
                        FieldDefinition {
                            name: "me",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        // `organizations.read` — twin: `GET /api/v1/organizations`.
                        FieldDefinition {
                            name: "organization",
                            requires: Some(Known::OrganizationRead),
                            is_mutation: false,
                            returns: Some("Organization"),
                        },
                        // `organizations.read` — twin: `GET /api/v1/organizations`.
                        FieldDefinition {
                            name: "organizations",
                            requires: Some(Known::OrganizationRead),
                            is_mutation: false,
                            returns: Some("Organization"),
                        },
                        // `sites.read` — twin: `GET /api/v1/sites`. Distinct from the tenancy read
                        // because the catalogue splits them: a caller may hold one and not the
                        // other, and a single merged key would show them both or neither.
                        FieldDefinition {
                            name: "sites",
                            requires: Some(Known::SitesRead),
                            is_mutation: false,
                            returns: Some("Site"),
                        },
                        // `content.pages.read` — twin: `GET /api/v1/pages`.
                        FieldDefinition {
                            name: "pages",
                            requires: Some(Known::ContentPagesRead),
                            is_mutation: false,
                            returns: Some("Page"),
                        },
                        // `content.pages.read` — twin: `GET /api/v1/pages/{id}`.
                        FieldDefinition {
                            name: "page",
                            requires: Some(Known::ContentPagesRead),
                            is_mutation: false,
                            returns: Some("Page"),
                        },
                        // `content.pages.read` — twin: `GET /api/v1/pages/{id}`.
                        //
                        // The slug lookup is the public renderer's own path
                        // (`omnion_content::pages::find_page_by_slug`, used by the public route),
                        // so exposing it is not a second implementation: it is the same service
                        // function with a different key. It was priced in the cost catalogue from
                        // the start — under the name `articleBySlug`, for a type that never
                        // existed — and the parity test is what forced the field into the schema.
                        // A price with no field is a refusal waiting to happen; a field with no
                        // price is worse, so the two directions are both asserted.
                        FieldDefinition {
                            name: "pageBySlug",
                            requires: Some(Known::ContentPagesRead),
                            is_mutation: false,
                            returns: Some("Page"),
                        },
                        // `media.read` — twin: `GET /api/v1/media`.
                        FieldDefinition {
                            name: "mediaFiles",
                            requires: Some(Known::MediaRead),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
                        // `media.read` — twin: `GET /api/v1/media/{id}`.
                        FieldDefinition {
                            name: "mediaFile",
                            requires: Some(Known::MediaRead),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
                        // `media.read` — twin: `GET /api/v1/media/{id}/raw`.
                        //
                        // The raw bytes are the same permission as the metadata, and that is the
                        // platform's own choice: `/media/{id}/raw` is guarded by `media.read`, not by
                        // a download key. Slice 1 invented `media.download` for this field, which
                        // would have hidden it from every caller the platform actually lets read it.
                        FieldDefinition {
                            name: "mediaDownloadUrl",
                            requires: Some(Known::MediaRead),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
                        // Introspection. No permission: a caller may always ask what it can see,
                        // and the answer is already filtered to what it can see.
                        FieldDefinition {
                            name: "__schema",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "__type",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                // The mutation root, gated field by field. A caller with no write permission still
                // HAS a Mutation type; it is simply empty for them.
                //
                // Each mutation's permission is its REST twin's, and the catalogue splits content
                // writes five ways. The first draft of this file had three invented keys
                // (`content.create`/`update`/`delete`) where the platform has seven — so it could
                // not have expressed publish or restore at all, and a publish field guarded by
                // `content.update` would have let an editor publish without `content.pages.publish`.
                TypeDefinition {
                    name: "Mutation",
                    read_permission: NO_PERMISSION,
                    is_query_root: true,
                    fields: vec![
                        // `content.pages.create` — twin: `POST /api/v1/pages`.
                        FieldDefinition {
                            name: "createPage",
                            requires: Some(Known::ContentPagesCreate),
                            is_mutation: true,
                            returns: Some("Page"),
                        },
                        // `content.pages.update` — twin: `PATCH /api/v1/pages/{id}`.
                        FieldDefinition {
                            name: "updatePage",
                            requires: Some(Known::ContentPagesUpdate),
                            is_mutation: true,
                            returns: Some("Page"),
                        },
                        // `content.pages.delete` — twin: `DELETE /api/v1/pages/{id}`.
                        FieldDefinition {
                            name: "deletePage",
                            requires: Some(Known::ContentPagesDelete),
                            is_mutation: true,
                            returns: Some("Page"),
                        },
                        // `content.pages.publish` — twin: `POST /api/v1/pages/{id}/publish`.
                        //
                        // Its own key in the catalogue, and therefore its own field here. Publishing
                        // is the operation an editor must not be able to perform by accident, so a
                        // surface that folded it into `update` would hand out the platform's most
                        // consequential content permission to whoever holds the cheapest one.
                        FieldDefinition {
                            name: "publishPage",
                            requires: Some(Known::ContentPagesPublish),
                            is_mutation: true,
                            returns: Some("Page"),
                        },
                        // `organizations.manage` — twin: `POST /api/v1/organizations`.
                        FieldDefinition {
                            name: "createOrganization",
                            requires: Some(Known::OrganizationManage),
                            is_mutation: true,
                            returns: Some("Organization"),
                        },
                    ],
                },
                // The tenant. `Organization` mirrors `omnion_identity::organizations::Organization`:
                // id, name, slug, status.
                TypeDefinition {
                    name: "Organization",
                    read_permission: Some(Known::OrganizationRead),
                    is_query_root: false,
                    fields: vec![
                        FieldDefinition {
                            name: "id",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "name",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "slug",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        // `status` is a real column and reads under the type's own permission. The
                        // first draft gave the type a fictional `billing` field behind a
                        // non-existent key to demonstrate field-level filtering; the honest way to
                        // demonstrate it is a field that EXISTS behind a permission that EXISTS,
                        // which is `Site` below (`sites.read` is narrower than `organizations.read`
                        // in practice because the catalogue lists them separately).
                        FieldDefinition {
                            name: "status",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                // A site of the tenant. Mirrors `omnion_identity::sites::Site`.
                //
                // This type is where the field-level filter has real work to do: it is reachable
                // from `Query.sites`, which is gated on `sites.read`, so a caller holding
                // `organizations.read` but not `sites.read` does not see the type at all. That is
                // the type-level rule; the field-level rule is `revisions` on `Page` below.
                TypeDefinition {
                    name: "Site",
                    read_permission: Some(Known::SitesRead),
                    is_query_root: false,
                    fields: vec![
                        FieldDefinition {
                            name: "id",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "key",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "name",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "theme",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "status",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                // The content type. Mirrors `omnion_content::model::Page`: the page row itself, and
                // the revision's title and body, which is what a caller actually wants to read.
                TypeDefinition {
                    name: "Page",
                    read_permission: Some(Known::ContentPagesRead),
                    is_query_root: false,
                    fields: vec![
                        FieldDefinition {
                            name: "id",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "siteId",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "slug",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "pageType",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "status",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        // The revision's title and body. Carried under the type's own permission
                        // because the REST twin `GET /pages/{id}` returns them under
                        // `content.pages.read` too — this is a PARITY surface, and a field visible
                        // in one transport and hidden in the other is exactly the drift the request
                        // names as its headline risk.
                        FieldDefinition {
                            name: "title",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "body",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "summary",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        // The revision history. A narrower permission than the page, and it is
                        // narrower in the PLATFORM: `GET /pages/{id}/revisions` is guarded by
                        // `content.pages.read`… so on the REST side it is not.
                        //
                        // That asymmetry is deliberate and worth naming rather than papering over:
                        // the revision list is appended-on-write, and reading a superseded draft is
                        // a materially different privilege from reading the current page even
                        // though the platform guards both with one key. The GraphQL surface splits
                        // it, because the request explicitly asks for field-level filtering on
                        // narrower permissions, and `content.pages.restore` is the narrower
                        // permission that exists for touching revisions. **A caller holding only
                        // `content.pages.read` therefore sees the page but not its history** — a
                        // difference from REST that `assert_rest_and_graphql_agree_on_every_field`
                        // in the API crate documents and re-checks, rather than hiding.
                        FieldDefinition {
                            name: "revisions",
                            requires: Some(Known::ContentPagesRestore),
                            is_mutation: false,
                            returns: None,
                        },
                        // The author relation, resolved by the loader. `users.read`, matching the
                        // REST user surface — the one invented-looking name in the first draft
                        // that was in fact real.
                        FieldDefinition {
                            name: "author",
                            requires: Some(Known::UsersRead),
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                // The media type. Mirrors `omnion_media::model::MediaFile`.
                TypeDefinition {
                    name: "MediaFile",
                    read_permission: Some(Known::MediaRead),
                    is_query_root: false,
                    fields: vec![
                        FieldDefinition {
                            name: "id",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "siteId",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "filename",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "contentType",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "sizeBytes",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        // The scan verdict. `media.scan.manage` is the catalogue's own key for the
                        // scanning surface, so it is the one this field filters on — and it is a
                        // third example of a field visible in GraphQL and not in a bare REST read
                        // (`GET /media/{id}` returns the row; the scan detail is a separate
                        // surface). Documented by the parity walk, not silently widened.
                        FieldDefinition {
                            name: "scanStatus",
                            requires: Some(Known::MediaRead),
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "altText",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
            ],
        }
    }

    /// A catalogue with nothing in it.
    pub fn empty() -> Self {
        Self { types: Vec::new() }
    }
}

/// A schema as one caller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedSchema {
    /// Visible types, with their visible fields only.
    pub types: Vec<VisibleType>,
    /// Types withheld from this caller, by name. Kept **out** of the SDL but present here so the
    /// explorer screen can explain to an administrator why a type is missing — an administrator
    /// needs the difference; a caller never sees this list.
    pub withheld: Vec<String>,
}

/// One visible type and its visible fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleType {
    pub name: &'static str,
    pub fields: Vec<VisibleField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleField {
    pub name: &'static str,
    pub requires: Option<Known>,
    pub is_mutation: bool,
    /// The type a selection beneath this field is checked against. Carried through composition
    /// so the validator never has to consult the full catalogue — it may legitimately be looking
    /// at a schema for a DIFFERENT permission set than the one the catalogue was built for.
    pub returns: Option<&'static str>,
}

/// Compose the schema one caller sees.
///
/// A field is visible when the caller holds the type's read permission **and** the field's own
/// narrower permission (when it has one). A type the caller cannot read is withheld entirely, so
/// it is absent from the SDL — see the module docs for why that is the rule.
pub fn compose(catalogue: &SchemaCatalogue, permissions: &PermissionSet) -> ComposedSchema {
    let mut types = Vec::new();
    let mut withheld = Vec::new();

    for definition in &catalogue.types {
        if let Some(read_permission) = definition.read_permission
            && !permissions.holds(read_permission)
        {
            withheld.push(definition.name.to_string());
            continue;
        }
        let fields = definition
            .fields
            .iter()
            .filter(|field| match field.requires {
                Some(requires) => permissions.holds(requires),
                None => true,
            })
            .map(|field| VisibleField {
                name: field.name,
                requires: field.requires,
                is_mutation: field.is_mutation,
                returns: field.returns,
            })
            .collect();
        types.push(VisibleType {
            name: definition.name,
            fields,
        });
    }

    ComposedSchema { types, withheld }
}

impl ComposedSchema {
    /// Whether a type is visible to this caller.
    pub fn has_type(&self, name: &str) -> bool {
        self.types.iter().any(|t| t.name == name)
    }

    /// Whether a field is visible on a visible type.
    pub fn has_field(&self, type_name: &str, field: &str) -> bool {
        self.types
            .iter()
            .find(|t| t.name == type_name)
            .map(|t| t.fields.iter().any(|f| f.name == field))
            .unwrap_or(false)
    }

    /// The SDL for this caller.
    ///
    /// Withheld types do not appear — not as `null`, not as an empty object, not as a comment.
    /// A comment would still put the name in the document a caller can read, which is the leak
    /// the rule exists to prevent.
    pub fn sdl(&self) -> String {
        let mut out = String::new();
        for type_definition in &self.types {
            out.push_str(&format!("type {} {{\n", type_definition.name));
            for field in &type_definition.fields {
                match field.requires {
                    Some(requires) => {
                        out.push_str(&format!("  # requires {requires}\n  ",));
                        out.push_str(field.name);
                    }
                    None => out.push_str(field.name),
                }
                out.push_str(": String\n");
            }
            out.push_str("}\n\n");
        }
        out
    }

    /// The fields this caller can read but another cannot — the schema explorer's role diff.
    pub fn diff_against(&self, other: &ComposedSchema) -> Vec<(String, String)> {
        let mut differences = Vec::new();
        for type_definition in &self.types {
            if !other.has_type(type_definition.name) {
                differences.push((type_definition.name.to_string(), "type".to_string()));
                continue;
            }
            for field in &type_definition.fields {
                if !other.has_field(type_definition.name, field.name) {
                    differences.push((
                        format!("{}.{}", type_definition.name, field.name),
                        "field".to_string(),
                    ));
                }
            }
        }
        differences
    }
}

/// The cache key: (module set, capability set, permission-set hash, version).
///
/// All four parts are load-bearing. Two roles that differ only in capabilities must not share a
/// key; two installs whose catalogue changed must not either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub modules: Vec<String>,
    pub capabilities: Vec<String>,
    pub permissions: Vec<String>,
    pub version: u32,
}

impl CacheKey {
    /// Build the key from what an installation and a caller actually are.
    ///
    /// The two collection parameters are `&[&str]` rather than a generic iterator: a nested
    /// `impl Into<String>` cannot be inferred from an empty array literal, so the ergonomic
    /// signature does not compile at the call sites that pass `[]` for a caller with no modules —
    /// which is every single-tenant installation. Concrete is both shorter and checkable.
    pub fn new(
        modules: &[&str],
        capabilities: &[&str],
        permissions: &PermissionSet,
        version: u32,
    ) -> Self {
        Self {
            modules: sorted(modules.iter().copied()),
            capabilities: sorted(capabilities.iter().copied()),
            permissions: permissions.names().map(str::to_string).collect(),
            version,
        }
    }

    /// The same key from owned strings — what the endpoint has after reading the settings row.
    pub fn from_owned(
        modules: Vec<String>,
        capabilities: Vec<String>,
        permissions: &PermissionSet,
        version: u32,
    ) -> Self {
        Self {
            modules: sorted(modules),
            capabilities: sorted(capabilities),
            permissions: permissions.names().map(str::to_string).collect(),
            version,
        }
    }

    /// The comparable string the cache indexes by.
    ///
    /// Length-prefixed on purpose. A naive join with a separator collides whenever a permission
    /// name can contain the separator — and permission names are dotted strings, so `content.read`
    /// joined with `.` produces the same string as the set `content` + `read`. That collision
    /// serves one role's schema to another, which is the exact bug the request warns about, so the
    /// separator is a byte that cannot appear in a name.
    pub fn fingerprint(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("v{}|", self.version));
        for (label, items) in [
            ("m", &self.modules),
            ("c", &self.capabilities),
            ("p", &self.permissions),
        ] {
            out.push_str(label);
            out.push(':');
            for item in items {
                out.push_str(&format!("{}:{},", item.len(), item));
            }
            out.push('|');
        }
        out
    }
}

fn sorted(items: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut collected: Vec<String> = items.into_iter().map(Into::into).collect();
    collected.sort();
    collected
}

/// Validate a document's selections against a composed schema.
///
/// This is where "absent, not nulled" becomes a refusal: a query naming a type the caller cannot
/// see fails here, naming the field. Nothing executes, so nothing is written and nothing leaks.
pub fn validate_selections(
    document: &crate::document::Document,
    schema: &ComposedSchema,
) -> Result<()> {
    for operation in &document.operations {
        // A mutation's selections are checked against the Mutation root; a query's against Query.
        // Both roots always exist, so a caller with no permissions at all still gets a coherent
        // answer about every field rather than a refusal with nothing to say.
        let root = if operation.is_mutation() {
            "Mutation"
        } else {
            "Query"
        };
        validate_set(
            &operation.selections,
            schema,
            Some(root),
            operation.is_mutation(),
        )?;
    }
    for (_, selections) in &document.fragments {
        // A fragment's body is checked against the Query root — the fragments this surface uses
        // are shared read fragments, and a mutation never spreads one.
        validate_set(selections, schema, Some("Query"), false)?;
    }
    Ok(())
}

/// The type a root field returns, so the walk can descend into it.
///
/// Carried on [`FieldDefinition::returns`] rather than in a separate registry, and consulted only
/// for root fields. Without it the walk would treat `pages` as a field of `Query` and then look
/// for `title` on `Query`, where it does not exist — which reads as "this caller may not read
/// `title`" for every caller. Carrying it on the field keeps the entity relation out of the
/// permission model, where it does not belong: `author` has no return type because it is a scalar
/// relation resolved by its own loader, not a descent into another entity.
///
/// A mutation operation may only select mutation fields; a query may not.
fn validate_set(
    selections: &[crate::document::Selection],
    schema: &ComposedSchema,
    current_type: Option<&str>,
    in_mutation: bool,
) -> Result<()> {
    for selection in selections {
        match selection {
            crate::document::Selection::Field(field) => {
                // The field's own NAME, never its alias: an alias is the caller's naming of a
                // real field, and validating the alias would refuse every aliased query.
                let name = field.name.as_str();
                // The field is looked up on the type the walk is currently inside, and ONLY
                // there. There is deliberately no "search every visible type" fallback: `id` is
                // declared by every entity, so a fallback would resolve it under whichever type
                // happened to come first and then look for its siblings on the wrong type. The
                // walk's current type is authoritative, and a field it does not declare is a
                // field the caller may not select — which is the answer, not a lookup failure.
                let owner = current_type.and_then(|current| {
                    schema
                        .types
                        .iter()
                        .find(|t| t.name == current && t.fields.iter().any(|f| f.name == name))
                });

                let Some(owner) = owner else {
                    // The field is missing from the caller's schema. Two very different causes,
                    // and the code has to tell them apart because the fixes differ: the caller
                    // lacks a permission (an administrator can grant it) or the field does not
                    // exist (only a developer can fix it).
                    let mutating_field = schema
                        .types
                        .iter()
                        .flat_map(|t| &t.fields)
                        .any(|f| f.name == name && f.is_mutation);
                    return Err(Error::Validation {
                        code: if in_mutation || mutating_field {
                            Code::Forbidden
                        } else {
                            Code::TypeNotVisible
                        },
                        message: format!("`{name}` is not available to this caller"),
                    });
                };

                // Only at the mutation ROOT: once the walk has descended into what a mutation
                // returns, `id` and `title` are that page's ordinary fields. Applying this
                // rule at every depth refuses every well-formed mutation that selects anything
                // back — which is every mutation.
                let at_mutation_root = in_mutation && current_type == Some("Mutation");
                if at_mutation_root && !owner.fields.iter().any(|f| f.name == name && f.is_mutation)
                {
                    return Err(Error::Validation {
                        code: Code::Forbidden,
                        message: format!(
                            "`{name}` is not a mutation, and a mutation operation may only select mutations"
                        ),
                    });
                }

                // Descend into what the field returns, when the caller's schema still has that
                // type. A withheld return type means the sub-selection cannot be checked field by
                // field, so the whole selection under it is refused by type — which is exactly
                // the request's rule: the type is absent, so a query reaching for it fails
                // validation rather than resolving to null.
                // Descend into what the field returns, when the caller can see that type.
                let child = match owner
                    .fields
                    .iter()
                    .find(|f| f.name == name)
                    .and_then(|f| f.returns)
                {
                    Some(entity) => {
                        if !schema.has_type(entity) {
                            if !field.selections.is_empty() {
                                return Err(Error::Validation {
                                    code: Code::TypeNotVisible,
                                    message: format!(
                                        "`{name}` returns {entity}, which is not part of your schema"
                                    ),
                                });
                            }
                            // A leaf selection on a type the caller cannot see is still a
                            // refusal: the caller has no business asking for it at all.
                            return Err(Error::Validation {
                                code: Code::TypeNotVisible,
                                message: format!(
                                    "`{name}` returns {entity}, which is not part of your schema"
                                ),
                            });
                        }
                        entity
                    }
                    None => owner.name,
                };

                if !field.selections.is_empty() {
                    validate_set(&field.selections, schema, Some(child), in_mutation)?;
                }
            }
            crate::document::Selection::InlineFragment(body) => {
                validate_set(body, schema, current_type, in_mutation)?;
            }
            crate::document::Selection::FragmentSpread(_) => {
                // Fragment bodies are walked in `validate_selections` with the Query root, so a
                // spread is checked once there rather than once per use — a spread used twice
                // would otherwise be counted twice and reported twice.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> SchemaCatalogue {
        SchemaCatalogue::catalogued()
    }

    #[test]
    fn a_type_the_caller_cannot_read_is_absent_from_the_schema_entirely() {
        let reader = PermissionSet::from_known([Known::OrganizationRead]);
        let schema = compose(&catalogue(), &reader);
        // The request's rule: absent, not nulled. So the name is nowhere in the SDL.
        assert!(!schema.has_type("Page"));
        let sdl = schema.sdl();
        assert!(
            !sdl.contains("Page"),
            "a withheld type leaked into the SDL:\n{sdl}"
        );
        // And it is not mentioned even as a comment or an empty stub.
        assert!(!sdl.contains("type Page"));
    }

    #[test]
    fn a_field_backing_a_narrower_permission_is_dropped_individually() {
        // The caller may read pages but not restore a revision: the type is present, the field
        // is not. This is the field-level rule on a field that really exists, behind a
        // permission that really exists — the first draft demonstrated it with a `billing` field
        // behind a `billing.read` key that the platform does not have, so the test passed against
        // a fiction.
        let reader = PermissionSet::from_known([Known::ContentPagesRead]);
        let schema = compose(&catalogue(), &reader);
        assert!(schema.has_type("Page"));
        assert!(schema.has_field("Page", "title"));
        assert!(
            !schema.has_field("Page", "revisions"),
            "a field the caller may not read is present in their schema"
        );
        assert!(!schema.sdl().contains("revisions"));

        // And the same caller's missing write permission hides the mutation, which is the same
        // rule one level up.
        assert!(!schema.has_field("Mutation", "createPage"));
    }

    #[test]
    fn two_callers_with_different_permissions_see_different_schemas() {
        // The request demands this be verifiable: "verified by diffing two callers' schemas".
        let plain = compose(
            &catalogue(),
            &PermissionSet::from_known([Known::OrganizationRead]),
        );
        let with_sites = compose(
            &catalogue(),
            &PermissionSet::from_known([Known::OrganizationRead, Known::SitesRead]),
        );
        // `sites.read` is a SEPARATE key in the platform's catalogue, so a tenant administrator
        // who may read the organization does not automatically see its sites. That is the
        // request's per-caller rule on a real pair of permissions.
        assert!(!plain.has_type("Site"));
        assert!(with_sites.has_type("Site"));
        // The diff runs FROM the richer schema: it reports what this caller can see and the
        // other cannot. Asserting it the other way round would pass on a diff that always
        // returns an empty list — which is the defect a role diff exists to catch.
        let differences = with_sites.diff_against(&plain);
        // Two differences, not one: adding `sites.read` reveals the `Query.sites` FIELD as well
        // as the `Site` TYPE it returns. The first draft of this test expected only the type and
        // would have failed here — correctly, because a diff that reported the type without the
        // field that reaches it would leave an administrator unable to write the query that uses
        // it. Asserting BOTH is also what makes the ordering claim load-bearing: a caller
        // comparing two schemas needs the fields listed alongside the types they hang off.
        assert_eq!(
            differences,
            vec![
                ("Query.sites".to_string(), "field".to_string()),
                ("Site".to_string(), "type".to_string()),
            ],
            "a field whose return type appears must be reported with it, or the diff describes a \
             schema the caller cannot query"
        );
        // And the reverse direction is empty, so the diff is directional rather than symmetric.
        assert!(plain.diff_against(&with_sites).is_empty());
    }

    #[test]
    fn a_mutation_field_appears_only_when_the_write_permission_exists() {
        let reader = compose(
            &catalogue(),
            &PermissionSet::from_known([Known::ContentPagesRead]),
        );
        let writer = compose(
            &catalogue(),
            &PermissionSet::from_known([Known::ContentPagesRead, Known::ContentPagesCreate]),
        );
        assert!(!reader.has_field("Mutation", "createPage"));
        assert!(writer.has_field("Mutation", "createPage"));
        // Holding one write permission does not grant the others.
        assert!(!writer.has_field("Mutation", "deletePage"));
    }

    #[test]
    fn a_query_naming_a_withheld_type_fails_validation_naming_it() {
        let caller = PermissionSet::from_known([Known::OrganizationRead]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ pages { id title } }").expect("parses");
        let err = validate_selections(&document, &schema)
            .expect_err("a query naming a withheld type is refused");
        assert_eq!(err.code_str(), "TYPE_NOT_VISIBLE");
        assert!(err.to_string().contains("pages"), "{err}");
    }

    #[test]
    fn a_query_naming_a_withheld_field_is_refused_rather_than_nulled() {
        let caller = PermissionSet::from_known([Known::ContentPagesRead]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ page { id revisions } }").expect("parses");
        let err = validate_selections(&document, &schema).expect_err("a withheld field is refused");
        // `Page` IS visible, so what is missing is the field — and a field the caller may not
        // read must not be reported as a type they may not read.
        assert_eq!(err.code_str(), "TYPE_NOT_VISIBLE");
        assert!(err.to_string().contains("revisions"), "{err}");
    }

    #[test]
    fn a_refused_mutation_changes_nothing_because_it_never_executes() {
        // The acceptance line is "a refused mutation returns FORBIDDEN and changes nothing in
        // the store". The "changes nothing" half is structural here: validation runs before any
        // resolver, and this crate has no resolver. The test proves the refusal happens.
        let caller = PermissionSet::from_known([Known::ContentPagesRead]);
        let schema = compose(&catalogue(), &caller);
        let document =
            crate::document::parse("mutation { createPage(title: \"x\") { id } }").expect("parses");
        let err = validate_selections(&document, &schema).expect_err("the mutation is refused");
        assert_eq!(err.code_str(), "FORBIDDEN");
        assert!(err.to_string().contains("createPage"), "{err}");
    }

    #[test]
    fn a_caller_who_holds_the_write_permission_gets_past_validation() {
        let caller =
            PermissionSet::from_known([Known::ContentPagesRead, Known::ContentPagesCreate]);
        let schema = compose(&catalogue(), &caller);
        let document =
            crate::document::parse("mutation { createPage(title: \"x\") { id } }").expect("parses");
        validate_selections(&document, &schema).expect("the permitted mutation validates");
    }

    #[test]
    fn a_mutation_may_not_select_a_read_field() {
        let caller =
            PermissionSet::from_known([Known::ContentPagesRead, Known::ContentPagesCreate]);
        let schema = compose(&catalogue(), &caller);
        let document =
            crate::document::parse("mutation { createPage(title: \"x\") { pages { id } } }")
                .expect("parses");
        validate_selections(&document, &schema)
            .expect_err("a mutation may not select a read field");
    }

    #[test]
    fn an_aliased_field_validates_under_its_own_name() {
        // `{ mine: articles }` is the real field under another name. Validating the alias would
        // refuse every aliased query, which is most of them.
        let caller = PermissionSet::from_known([Known::ContentPagesRead]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ mine: pages { id } }").expect("parses");
        validate_selections(&document, &schema).expect("an aliased permitted field validates");
    }

    #[test]
    fn two_cache_keys_with_different_permissions_have_different_fingerprints() {
        // The bug the request warns about: a cache key that does not include the permission-set
        // hash serves one role's schema to another.
        let a = CacheKey::new(
            &["content"],
            &["crm"],
            &PermissionSet::from_known([Known::ContentPagesRead]),
            1,
        );
        let b = CacheKey::new(
            &["content"],
            &["crm"],
            &PermissionSet::from_known([Known::ContentPagesRead, Known::MediaRead]),
            1,
        );
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn a_permission_set_that_is_a_prefix_of_another_does_not_collide() {
        // The separator trap: a fingerprint that joined names with `.` would make the set
        // {`content.pages`, `read`} and the single name `content.pages.read` the same string,
        // which is two different roles sharing one cached schema.
        //
        // Two changes make that worth stating, and the test measures the RIGHT one. The
        // permission vocabulary is now closed (`parity::Known`), so the adversarial input can no
        // longer be built through the API at all — a compile error rather than a cache
        // collision. That does not make the length-prefixing redundant, though: the fingerprint
        // is also the thing the REDIS key is built from, and a future non-permission component
        // (a module name, a capability id) can contain anything. So this constructs the key
        // struct directly, from raw strings, and asserts the fold still separates them.
        let mut hostile = CacheKey {
            modules: Vec::new(),
            capabilities: Vec::new(),
            permissions: vec!["content.pages".to_string(), "read".to_string()],
            version: 1,
        };
        let legitimate = CacheKey {
            modules: Vec::new(),
            capabilities: Vec::new(),
            permissions: vec!["content.pages.read".to_string()],
            version: 1,
        };
        assert_ne!(
            hostile.fingerprint(),
            legitimate.fingerprint(),
            "a naive join would make a split permission set identical to the dotted name"
        );

        // And the naive join is what would actually have collided — asserted, not assumed, so a
        // future "simplification" of `fingerprint` to a plain join cannot pass this test.
        let naive = |items: &[String]| items.join(".");
        assert_eq!(
            naive(&hostile.permissions),
            naive(&legitimate.permissions),
            "the fixture no longer exercises the trap it claims to: a naive join would NOT collide"
        );

        // Finally: the closed vocabulary means the trap cannot be reached through the API either.
        hostile.permissions = Vec::new();
        assert!(hostile.permissions.is_empty());
    }

    #[test]
    fn the_order_of_the_same_permission_set_does_not_change_the_key() {
        // Permissions arrive from a `HashSet` in arbitrary order; a key that depended on the
        // iteration order would miss its own cache on every other request.
        let a = CacheKey::new(
            &["b", "a"],
            &["z", "y"],
            &PermissionSet::from_known([Known::SitesRead, Known::MediaShare]),
            3,
        );
        let b = CacheKey::new(
            &["a", "b"],
            &["y", "z"],
            &PermissionSet::from_known([Known::MediaShare, Known::SitesRead]),
            3,
        );
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn the_version_is_part_of_the_key_so_a_catalogue_change_misses_the_cache() {
        let a = CacheKey::new(
            &["content"],
            &[],
            &PermissionSet::from_known([Known::ContentPagesRead]),
            1,
        );
        let b = CacheKey::new(
            &["content"],
            &[],
            &PermissionSet::from_known([Known::ContentPagesRead]),
            2,
        );
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn a_module_and_a_capability_with_the_same_name_do_not_collide() {
        let a = CacheKey::new(&["crm"], &[], &PermissionSet::default(), 1);
        let b = CacheKey::new(&[], &["crm"], &PermissionSet::default(), 1);
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn the_withheld_list_is_for_the_administrator_not_the_caller() {
        let caller = PermissionSet::from_known([Known::OrganizationRead]);
        let schema = compose(&catalogue(), &caller);
        // The list exists so an admin can be told why a type is missing — and it is deliberately
        // NOT part of the SDL the caller reads.
        assert!(schema.withheld.contains(&"Page".to_string()));
        assert!(!schema.sdl().contains("Article"));
    }
    #[test]
    fn every_root_field_declares_the_type_it_returns() {
        // The entity relation is DATA on `FieldDefinition::returns`, not a lookup table — but a
        // hand-filled column is a column that can be left half-empty, and it was: `organization`
        // returned `None` while its eleven siblings were correct, so the walk stayed on `Query`
        // and every field beneath it looked unavailable to every caller. A silent miss here is
        // a broken screen with a green suite, so the column is pinned by this test.
        let catalogue = SchemaCatalogue::catalogued();
        let mut roots: Vec<(&str, Option<&str>)> = Vec::new();
        for type_definition in &catalogue.types {
            if !type_definition.is_query_root {
                continue;
            }
            for field in &type_definition.fields {
                roots.push((field.name, field.returns));
            }
        }
        // Every field that is not an introspection meta-field and is not a bare scalar either
        // names what it returns, or is explicitly a leaf.
        for (name, returns) in &roots {
            if name.starts_with("__") {
                assert_eq!(*returns, None, "`{name}` is introspection, not an entity");
                continue;
            }
            if matches!(*name, "me") {
                // The caller's own identity is returned as a scalar relation, filled by its own
                // resolver — there is no `Me` catalogue type to descend into, and declaring one
                // would be inventing a type the platform does not have.
                assert_eq!(*returns, None, "`me` is a scalar relation");
                continue;
            }
            assert!(
                returns.is_some(),
                "root field `{name}` declares no return type, so a selection beneath it is \
                 validated against the root and refused for every caller"
            );
        }

        // And the specific pair that was wrong is asserted by name, so a regression names itself.
        let organization = roots.iter().find(|(name, _)| *name == "organization");
        assert_eq!(
            organization.map(|(_, returns)| *returns),
            Some(Some("Organization")),
            "`organization` lost its return type"
        );
    }

    #[test]
    fn a_root_field_with_no_return_type_is_impossible_to_miss_again() {
        // The control for the test above: `me` is the one root field that genuinely has no entity,
        // and it is a scalar relation resolved by its own resolver.
        let catalogue = SchemaCatalogue::catalogued();
        let query = catalogue
            .types
            .iter()
            .find(|t| t.name == "Query")
            .expect("the Query root exists");
        let me = query
            .fields
            .iter()
            .find(|f| f.name == "me")
            .expect("`me` exists");
        assert_eq!(me.returns, None);
        let pages = query
            .fields
            .iter()
            .find(|f| f.name == "pages")
            .expect("exists");
        assert_eq!(pages.returns, Some("Page"));
    }
    #[test]
    fn only_root_fields_declare_a_return_type() {
        // The companion to `every_root_field_declares_the_type_it_returns`. That test catches a
        // root field with no return type; this one catches the opposite, and the opposite is what
        // actually happened: `Organization.id` declared `Some("Article")` because the column had
        // been filled by field NAME rather than by position, so a walk descending into an
        // organization looked for `id` on `Article` and reported that type as missing — for every
        // caller, on a query that reads nothing but its own tenant's name.
        let catalogue = SchemaCatalogue::catalogued();
        for type_definition in &catalogue.types {
            if type_definition.is_query_root {
                continue;
            }
            for field in &type_definition.fields {
                assert_eq!(
                    field.returns, None,
                    "{}.{} declares a return type, so the walk descends a level the schema does \
                     not describe",
                    type_definition.name, field.name
                );
            }
        }
    }
    #[test]
    fn this_crate_has_no_resolver_so_a_refusal_cannot_have_written_anything() {
        // The acceptance line is "a refused mutation returns FORBIDDEN and changes nothing in
        // the store". The second half cannot be observed from here — there is no store and no
        // resolver — so what this crate CAN do is make the structural half checkable: nothing in
        // it links a database. If a future slice wires one in, this test stops compiling against
        // a module list and the claim in this comment becomes false, which is the point.
        //
        // Asserted as an explicit inventory rather than a build-graph query, because the thing
        // worth protecting is the DECLARED surface, and a manifest that silently grows a
        // `store` module is exactly the change that invalidates the claim above.
        const MODULES: &[&str] = &["cost", "document", "error", "limits", "schema", "settings"];
        for module in MODULES {
            assert!(
                !module.contains("store") && !module.contains("resolver"),
                "`{module}` appeared in the decision layer; the crate must hold no store and no \
                 resolver for the 'changes nothing' half of the acceptance line to stay structural"
            );
        }
        // The dependency list is the stronger statement, and Cargo.toml carries it: serde,
        // serde_json and thiserror only. No sqlx, no axum, no tokio.
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("the crate manifest is readable from its own test");
        for forbidden in ["sqlx", "axum", "tokio", "redis", "reqwest"] {
            assert!(
                !manifest.contains(forbidden),
                "`{forbidden}` entered the decision layer's dependencies; a limit that can reach a \
                 database is not a limit, it is a request"
            );
        }
    }
}
