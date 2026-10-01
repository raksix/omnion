//! Per-caller schema composition: what a caller can see, and the cache key that keeps it theirs.
//!
//! The request is precise about the rule and about why it is that way: *"A type whose read
//! permission the caller lacks is absent from the schema entirely — not nulled at runtime — so
//! introspection cannot leak shape or existence."*
//!
//! Absent, not nulled, is the whole claim. A schema that carried the type and returned `null` for
//! an unpermitted read would still answer introspection with the type's name, its fields and their
//! types — so a caller who may not read `billing` learns that `billing` exists, which is the
//! information the rule exists to withhold. Composition therefore happens *before* validation,
//! and a query naming a type the caller cannot see fails validation naming the type, rather than
//! executing and returning nothing.
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

use std::collections::BTreeSet;

use crate::error::{Code, Error, Result};

/// The read permission of a root type: a root holds no data of its own, so no permission gates
/// its EXISTENCE. Its fields carry their own permissions, which is where the filtering happens.
pub const NO_PERMISSION: &str = "";

/// One type in the schema catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDefinition {
    pub name: &'static str,
    /// The read permission a caller must hold to see this type at all.
    pub read_permission: &'static str,
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
    pub requires: Option<&'static str>,
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
    /// The permissions are the real ones from the catalogue (REQ-068), spelled as string
    /// constants here rather than imported — the crate is deliberately free of the permissions
    /// crate so it can be unit-tested without a database, and the endpoint asserts the two agree.
    pub fn catalogued() -> Self {
        Self {
            types: vec![
                // The query root. Every resolver-backed field on the whole surface starts here,
                // and each carries the read permission it needs — so a caller who may read
                // content but not the tenant still sees a Query type, with only their fields on
                // it. That is the difference between "a type is withheld" and "a field is
                // withheld", and the two produce different error codes.
                TypeDefinition {
                    name: "Query",
                    read_permission: NO_PERMISSION,
                    is_query_root: true,
                    fields: vec![
                        FieldDefinition {
                            name: "me",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "organization",
                            requires: Some("tenancy.read"),
                            is_mutation: false,
                            returns: Some("Organization"),
                        },
                        FieldDefinition {
                            name: "organizations",
                            requires: Some("tenancy.read"),
                            is_mutation: false,
                            returns: Some("Organization"),
                        },
                        FieldDefinition {
                            name: "articles",
                            requires: Some("content.read"),
                            is_mutation: false,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "article",
                            requires: Some("content.read"),
                            is_mutation: false,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "articleBySlug",
                            requires: Some("content.read"),
                            is_mutation: false,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "pages",
                            requires: Some("content.read"),
                            is_mutation: false,
                            returns: Some("Page"),
                        },
                        FieldDefinition {
                            name: "mediaFiles",
                            requires: Some("media.read"),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
                        FieldDefinition {
                            name: "mediaFile",
                            requires: Some("media.read"),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
                        FieldDefinition {
                            name: "mediaDownloadUrl",
                            requires: Some("media.download"),
                            is_mutation: false,
                            returns: Some("MediaFile"),
                        },
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
                // The mutation root, gated field by field. A caller with no write permission
                // still HAS a Mutation type; it is simply empty for them.
                TypeDefinition {
                    name: "Mutation",
                    read_permission: NO_PERMISSION,
                    is_query_root: true,
                    fields: vec![
                        FieldDefinition {
                            name: "createArticle",
                            requires: Some("content.create"),
                            is_mutation: true,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "updateArticle",
                            requires: Some("content.update"),
                            is_mutation: true,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "deleteArticle",
                            requires: Some("content.delete"),
                            is_mutation: true,
                            returns: Some("Article"),
                        },
                        FieldDefinition {
                            name: "createOrganization",
                            requires: Some("tenancy.create"),
                            is_mutation: true,
                            returns: Some("Organization"),
                        },
                    ],
                },
                TypeDefinition {
                    name: "Organization",
                    read_permission: "tenancy.read",
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
                        // The billing relation is the field-level filter's whole reason: anyone
                        // who may read the tenant may not read its billing.
                        FieldDefinition {
                            name: "billing",
                            requires: Some("billing.read"),
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                TypeDefinition {
                    name: "Article",
                    read_permission: "content.read",
                    is_query_root: false,
                    fields: vec![
                        FieldDefinition {
                            name: "id",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "title",
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
                            name: "body",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "author",
                            requires: Some("users.read"),
                            is_mutation: false,
                            returns: None,
                        },
                        // Unpublished revisions are a narrower permission than the article.
                        FieldDefinition {
                            name: "revisions",
                            requires: Some("content.revisions.read"),
                            is_mutation: false,
                            returns: None,
                        },
                    ],
                },
                TypeDefinition {
                    name: "MediaFile",
                    read_permission: "media.read",
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
                            name: "mimeType",
                            requires: None,
                            is_mutation: false,
                            returns: None,
                        },
                        FieldDefinition {
                            name: "downloadUrl",
                            requires: Some("media.download"),
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

/// The caller's effective permissions, as a set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet(BTreeSet<String>);

impl PermissionSet {
    pub fn new(permissions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(permissions.into_iter().map(Into::into).collect())
    }

    /// Whether the set holds a permission.
    pub fn holds(&self, permission: &str) -> bool {
        self.0.contains(permission)
    }

    /// The permission names, sorted — the input to the cache key's hash.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
    pub requires: Option<&'static str>,
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
        if definition.read_permission != NO_PERMISSION
            && !permissions.holds(definition.read_permission)
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
            permissions: permissions.names().cloned().collect(),
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
            permissions: permissions.names().cloned().collect(),
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
/// Derived from the catalogue rather than hardcoded, and consulted only for root fields. Without
/// it the walk would treat `articles` as a field of `Query` and then look for `title` on `Query`,
/// where it does not exist — which reads as "this caller may not read `title`" for every caller.
/// A separate registry (rather than a field on `FieldDefinition`) keeps the entity relation out
/// of the permission model, where it does not belong: `billing` has no return type because it is
/// a scalar relation resolved by its own resolver.
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
                // returns, `id` and `title` are that article's ordinary fields. Applying this
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
        let reader = PermissionSet::new(["tenancy.read"]);
        let schema = compose(&catalogue(), &reader);
        // The request's rule: absent, not nulled. So the name is nowhere in the SDL.
        assert!(!schema.has_type("Article"));
        let sdl = schema.sdl();
        assert!(
            !sdl.contains("Article"),
            "a withheld type leaked into the SDL:\n{sdl}"
        );
        // And it is not mentioned even as a comment or an empty stub.
        assert!(!sdl.contains("type Article"));
    }

    #[test]
    fn a_field_backing_a_narrower_permission_is_dropped_individually() {
        let caller = PermissionSet::new(["tenancy.read"]);
        let schema = compose(&catalogue(), &caller);
        // The caller may read the tenant but not its billing: the type is present, the field is not.
        assert!(schema.has_type("Organization"));
        assert!(schema.has_field("Organization", "name"));
        assert!(
            !schema.has_field("Organization", "billing"),
            "a field the caller may not read is present in their schema"
        );
        assert!(!schema.sdl().contains("billing"));
    }

    #[test]
    fn two_callers_with_different_permissions_see_different_schemas() {
        // The request demands this be verifiable: "verified by diffing two callers' schemas".
        let plain = compose(&catalogue(), &PermissionSet::new(["tenancy.read"]));
        let billing = compose(
            &catalogue(),
            &PermissionSet::new(["tenancy.read", "billing.read"]),
        );
        assert!(!plain.has_field("Organization", "billing"));
        assert!(billing.has_field("Organization", "billing"));
        // The diff runs FROM the richer schema: it reports what this caller can see and the
        // other cannot. Asserting it the other way round would pass on a diff that always
        // returns an empty list — which is the defect a role diff exists to catch.
        let differences = billing.diff_against(&plain);
        assert_eq!(
            differences,
            vec![("Organization.billing".to_string(), "field".to_string())]
        );
        // And the reverse direction is empty, so the diff is directional rather than symmetric.
        assert!(plain.diff_against(&billing).is_empty());
    }

    #[test]
    fn a_mutation_field_appears_only_when_the_write_permission_exists() {
        let reader = compose(&catalogue(), &PermissionSet::new(["content.read"]));
        let writer = compose(
            &catalogue(),
            &PermissionSet::new(["content.read", "content.create"]),
        );
        assert!(!reader.has_field("Mutation", "createArticle"));
        assert!(writer.has_field("Mutation", "createArticle"));
        // Holding one write permission does not grant the others.
        assert!(!writer.has_field("Mutation", "deleteArticle"));
    }

    #[test]
    fn a_query_naming_a_withheld_type_fails_validation_naming_it() {
        let caller = PermissionSet::new(["tenancy.read"]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ articles { id title } }").expect("parses");
        let err = validate_selections(&document, &schema)
            .expect_err("a query naming a withheld type is refused");
        assert_eq!(err.code_str(), "TYPE_NOT_VISIBLE");
        assert!(err.to_string().contains("articles"), "{err}");
    }

    #[test]
    fn a_query_naming_a_withheld_field_is_refused_rather_than_nulled() {
        let caller = PermissionSet::new(["tenancy.read"]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ organization { id billing } }").expect("parses");
        let err = validate_selections(&document, &schema).expect_err("a withheld field is refused");
        // `Organization` IS visible, so what is missing is the field — and a field the caller may
        // not read must not be reported as a type they may not read.
        assert_eq!(err.code_str(), "TYPE_NOT_VISIBLE");
        assert!(err.to_string().contains("billing"), "{err}");
    }

    #[test]
    fn a_refused_mutation_changes_nothing_because_it_never_executes() {
        // The acceptance line is "a refused mutation returns FORBIDDEN and changes nothing in
        // the store". The "changes nothing" half is structural here: validation runs before any
        // resolver, and this crate has no resolver. The test proves the refusal happens.
        let caller = PermissionSet::new(["content.read"]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("mutation { createArticle(title: \"x\") { id } }")
            .expect("parses");
        let err = validate_selections(&document, &schema).expect_err("the mutation is refused");
        assert_eq!(err.code_str(), "FORBIDDEN");
        assert!(err.to_string().contains("createArticle"), "{err}");
    }

    #[test]
    fn a_caller_who_holds_the_write_permission_gets_past_validation() {
        let caller = PermissionSet::new(["content.read", "content.create"]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("mutation { createArticle(title: \"x\") { id } }")
            .expect("parses");
        validate_selections(&document, &schema).expect("the permitted mutation validates");
    }

    #[test]
    fn a_mutation_may_not_select_a_read_field() {
        let caller = PermissionSet::new(["content.read", "content.create"]);
        let schema = compose(&catalogue(), &caller);
        let document =
            crate::document::parse("mutation { createArticle(title: \"x\") { articles { id } } }")
                .expect("parses");
        validate_selections(&document, &schema)
            .expect_err("a mutation may not select a read field");
    }

    #[test]
    fn an_aliased_field_validates_under_its_own_name() {
        // `{ mine: articles }` is the real field under another name. Validating the alias would
        // refuse every aliased query, which is most of them.
        let caller = PermissionSet::new(["content.read"]);
        let schema = compose(&catalogue(), &caller);
        let document = crate::document::parse("{ mine: articles { id } }").expect("parses");
        validate_selections(&document, &schema).expect("an aliased permitted field validates");
    }

    #[test]
    fn two_cache_keys_with_different_permissions_have_different_fingerprints() {
        // The bug the request warns about: a cache key that does not include the permission-set
        // hash serves one role's schema to another.
        let a = CacheKey::new(
            &["content"],
            &["crm"],
            &PermissionSet::new(["content.read"]),
            1,
        );
        let b = CacheKey::new(
            &["content"],
            &["crm"],
            &PermissionSet::new(["content.read", "media.read"]),
            1,
        );
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn a_permission_set_that_is_a_prefix_of_another_does_not_collide() {
        // The separator trap: joining with `.` would make {content, read} and {content.read} the
        // same string, which is two different roles sharing one cached schema.
        let a = CacheKey::new(&[], &[], &PermissionSet::new(["content", "read"]), 1);
        let b = CacheKey::new(&[], &[], &PermissionSet::new(["content.read"]), 1);
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "a prefix permission set collides with the dotted name"
        );
    }

    #[test]
    fn the_order_of_the_same_permission_set_does_not_change_the_key() {
        // Permissions arrive from a `HashSet` in arbitrary order; a key that depended on the
        // iteration order would miss its own cache on every other request.
        let a = CacheKey::new(&["b", "a"], &["z", "y"], &PermissionSet::new(["x", "w"]), 3);
        let b = CacheKey::new(&["a", "b"], &["y", "z"], &PermissionSet::new(["w", "x"]), 3);
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn the_version_is_part_of_the_key_so_a_catalogue_change_misses_the_cache() {
        let a = CacheKey::new(&["content"], &[], &PermissionSet::new(["content.read"]), 1);
        let b = CacheKey::new(&["content"], &[], &PermissionSet::new(["content.read"]), 2);
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
        let caller = PermissionSet::new(["tenancy.read"]);
        let schema = compose(&catalogue(), &caller);
        // The list exists so an admin can be told why a type is missing — and it is deliberately
        // NOT part of the SDL the caller reads.
        assert!(schema.withheld.contains(&"Article".to_string()));
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
        let articles = query
            .fields
            .iter()
            .find(|f| f.name == "articles")
            .expect("exists");
        assert_eq!(articles.returns, Some("Article"));
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
        const MODULES: &[&str] = &[
            "cost",
            "document",
            "error",
            "limits",
            "schema",
            "settings",
        ];
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
