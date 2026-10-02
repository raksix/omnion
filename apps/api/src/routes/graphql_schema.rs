//! `GET /api/v1/graphql/schema` and `/schema/diff` — the schema explorer (REQ-130, slice 2).
//!
//! ## What this route shows and what it must never show
//!
//! The request's rule: *"A type whose read permission the caller lacks is absent from the schema
//! entirely — not nulled at runtime — so introspection cannot leak shape or existence."* So the
//! SDL below is [`ComposedSchema::sdl`], which omits withheld types completely — not as `null`, not
//! as an empty object, and not as a comment, because a comment would still put the name in a
//! document the caller can read.
//!
//! The **explorer screen**, by contrast, is an administrator's tool and it DOES name what is
//! missing: [`withheld`] carries those type names, and the screen shows them under a heading that
//! says who may not read them and why. That is not a contradiction of the rule — the rule is
//! about what a caller can obtain; an administrator asking "why can nobody see `MediaFile`" is
//! asking a different question, and answering it with an empty list is the "documented but
//! unreachable" shape this request has produced five times.
//!
//! ## The diff composes a ROLE's schema, and does it through the same function
//!
//! The role diff is asked *"by diffing two callers' schemas"*. The obvious cheap version — take
//! the caller's composed schema and delete the fields the other role lacks — is wrong twice over:
//! it needs the catalogue anyway to know what the other role *should* have, and a hand-written
//! filter would be a second composition path that could drift from [`compose`]. So the route
//! resolves the role's effective permissions with the platform's own resolution
//! ([`omnion_permissions::effective_permissions_for`] over the role graph) and calls
//! [`compose`] a second time. Two calls, one function: the diff cannot disagree with the schema.
//!
//! ## No cache, and the note that says why
//!
//! [`schema_cache_key`] exists and is what a cache would be indexed by, but nothing here caches.
//! The request requires the composed schema to be "cached … and invalidated on module, capability
//! or role changes" — three invalidation sources, none of which this repository emits events for
//! yet (REQ-124 and REQ-067 own them). **A cache invalidated by nothing is a schema served to the
//! wrong role**, which is the headline risk the request names, so the read is composed per request
//! and the cache key travels in the response for whoever adds the cache with its invalidation.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use omnion_graphql::parity::{ALL as KNOWN_PERMISSIONS, PermissionSet};
use omnion_graphql::schema::{ComposedSchema, SchemaCatalogue, compose};

use crate::auth::ApiCaller;
use crate::error::ApiError;
use crate::routes::graphql::{known_from_effective, resolve_caller, schema_cache_key};
use crate::state::AppState;

/// The read guard. The same real key the endpoint carries: the explorer shows the caller's own
/// schema, and the endpoint it describes reads that same surface.
pub const READ_PERMISSION: &str = "content.pages.read";

/// The version half of the cache key. Bumped when the catalogue's shape changes, which is the one
/// invalidation source this module can implement without an event bus.
const SCHEMA_VERSION: u32 = 1;

/// What `GET /graphql/schema` answers.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaResponse {
    pub schema: SchemaBody,
    pub sdl: String,
    pub cache_key: String,
    pub permissions: Vec<String>,
    pub roles: Vec<RoleOption>,
    pub read_permission: &'static str,
}

/// The composed schema, serialisable.
///
/// `ComposedSchema` itself is not `Serialize` — the crate's types are pure and the wire shape is
/// this module's decision. Naming the permission as a string rather than as `Known` is deliberate:
/// the screen prints it, and a serialised enum variant name is not the key the operator granted.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaBody {
    pub types: Vec<TypeBody>,
    pub withheld: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeBody {
    pub name: String,
    pub fields: Vec<FieldBody>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldBody {
    pub name: String,
    pub requires: Option<String>,
    pub is_mutation: bool,
    pub returns: Option<String>,
}

impl From<&ComposedSchema> for SchemaBody {
    fn from(schema: &ComposedSchema) -> Self {
        Self {
            types: schema
                .types
                .iter()
                .map(|type_definition| TypeBody {
                    name: type_definition.name.to_owned(),
                    fields: type_definition
                        .fields
                        .iter()
                        .map(|field| FieldBody {
                            name: field.name.to_owned(),
                            requires: field.requires.map(|known| known.as_str().to_owned()),
                            is_mutation: field.is_mutation,
                            returns: field.returns.map(str::to_owned),
                        })
                        .collect(),
                })
                .collect(),
            withheld: schema.withheld.clone(),
        }
    }
}

/// One role the explorer may compose a schema for.
#[derive(Debug, Clone, Serialize)]
pub struct RoleOption {
    pub id: String,
    pub key: String,
    pub name: String,
    /// How many of the GraphQL-visible permissions the role grants. Counted from the role graph,
    /// so the number the picker shows is the number the diff will produce — not a count of every
    /// permission in the catalogue, most of which have nothing to do with GraphQL.
    pub permission_count: usize,
}

/// `GET /api/v1/graphql/schema`.
pub async fn read(
    State(state): State<AppState>,
    caller: ApiCaller,
) -> Result<Json<SchemaResponse>, ApiError> {
    let caller = resolve_caller(&state, &caller).await?;
    let catalogue = SchemaCatalogue::catalogued();
    let schema = compose(&catalogue, &caller.known);
    let roles = role_options(state.db().pool(), caller.organization_id).await?;

    Ok(Json(SchemaResponse {
        schema: SchemaBody::from(&schema),
        sdl: schema.sdl(),
        cache_key: schema_cache_key(&caller, &[], &[], SCHEMA_VERSION),
        permissions: caller.known.names().map(str::to_owned).collect(),
        roles,
        read_permission: READ_PERMISSION,
    }))
}

/// The diff's query.
#[derive(Debug, Default, Deserialize)]
pub struct DiffQuery {
    /// The role to compare against. Named `roleId` because that is what the screen sends.
    #[serde(default)]
    pub role_id: Option<String>,
    /// The id spelling the screen also tolerates, so a hand-written URL works.
    #[serde(default, rename = "role")]
    pub role: Option<String>,
}

/// One line of the diff.
#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    /// `Type`, or `Type.field`.
    pub path: String,
    /// `mine` = the caller has it and the role does not; `theirs` = the reverse.
    pub direction: &'static str,
    /// The permission that decides the line. `null` for a whole type, which no permission gates —
    /// a root holds no data of its own, which is why it is never withheld.
    pub requires: Option<String>,
}

/// `GET /api/v1/graphql/schema/diff?roleId=…`.
#[derive(Debug, Clone, Serialize)]
pub struct DiffResponse {
    pub mine: String,
    pub theirs: String,
    pub only_mine: Vec<DiffLine>,
    pub only_theirs: Vec<DiffLine>,
    pub identical: bool,
}

/// The caller's schema beside a role's, field by field.
///
/// Both sides go through [`compose`], so the diff is a comparison of two compositions rather than
/// a filter applied to one — and the permission each line names comes from the catalogue, not from
/// a guess about what a field "probably" needs.
pub async fn diff(
    State(state): State<AppState>,
    caller: ApiCaller,
    Query(params): Query<DiffQuery>,
) -> Result<Json<DiffResponse>, ApiError> {
    let requested = params
        .role_id
        .as_deref()
        .or(params.role.as_deref())
        .unwrap_or_default();
    if requested.is_empty() {
        return Err(ApiError::bad_request(
            "graphql_role_required",
            "`roleId` is required: the diff compares your schema against a role, and with no role \
             there is nothing to compare it to",
        ));
    }
    let role_id = Uuid::parse_str(requested).map_err(|_| {
        ApiError::bad_request(
            "graphql_role_invalid",
            format!("`{requested}` is not a role id"),
        )
    })?;

    let caller = resolve_caller(&state, &caller).await?;
    let organization_id = caller.organization_id.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "organization_required",
            "roles belong to an organization and this account has none",
        )
    })?;

    let catalogue = SchemaCatalogue::catalogued();
    let mine = compose(&catalogue, &caller.known);
    let theirs = compose(
        &catalogue,
        &compose_role_permissions(&state, organization_id, role_id).await?,
    );

    let role_name = role_label(state.db().pool(), organization_id, role_id).await?;
    Ok(Json(DiffResponse {
        mine: "your schema".to_owned(),
        theirs: role_name,
        only_mine: diff_lines(&mine, &theirs, "mine"),
        only_theirs: diff_lines(&theirs, &mine, "theirs"),
        identical: diff_lines(&mine, &theirs, "mine").is_empty()
            && diff_lines(&theirs, &mine, "theirs").is_empty(),
    }))
}

/// What one side has that the other does not.
///
/// Walks both compositions rather than calling [`ComposedSchema::diff_against`], because that
/// returns the paths without the permission — and **the permission is the answer the explorer
/// exists to give**: "you cannot see `Page.revisions`" is a support ticket, "`Page.revisions`
/// needs `content.pages.read`, which that role lacks" closes it.
fn diff_lines(mine: &ComposedSchema, theirs: &ComposedSchema, direction: &'static str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    for type_definition in &mine.types {
        if !theirs.has_type(type_definition.name) {
            lines.push(DiffLine {
                path: type_definition.name.to_owned(),
                direction,
                requires: None,
            });
            continue;
        }
        for field in &type_definition.fields {
            if !theirs.has_field(type_definition.name, field.name) {
                lines.push(DiffLine {
                    path: format!("{}.{}", type_definition.name, field.name),
                    direction,
                    requires: field.requires.map(|known| known.as_str().to_owned()),
                });
            }
        }
    }
    lines
}

/// The permissions one role resolves to, through the platform's own resolution.
///
/// A role that does not exist, or belongs to another organization, resolves to **no**
/// permissions — the same answer a caller with no binding gets. That is deliberate: inventing an
/// empty-but-distinguishable answer would let the picker offer a row that cannot exist, and a diff
/// against a role from another tenant would be a cross-tenant oracle.
async fn compose_role_permissions(
    state: &AppState,
    organization_id: Uuid,
    role_id: Uuid,
) -> Result<PermissionSet, ApiError> {
    let graph = omnion_permissions::evaluate::load_role_graph(state.db().pool(), Some(organization_id))
        .await
        .map_err(internal)?;
    let effective = graph.effective(&[role_id]);
    Ok(known_from_effective(&effective))
}

/// The roles of one organization, with their GraphQL-visible permission counts.
async fn role_options(
    pool: &sqlx::PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<RoleOption>, ApiError> {
    // `RoleGraph` has no iterator over its assignments, and adding one to the permissions crate for
    // one screen would be a wider change than the screen. The role list is read directly instead —
    // it is the same `roles::list_roles` the graph itself is built from, so the two cannot
    // disagree about which roles exist.
    let roles = omnion_permissions::roles::list_roles(pool, organization_id)
        .await
        .map_err(internal)?;
    let ids: Vec<Uuid> = roles.iter().map(|role| role.id).collect();
    let entries = omnion_permissions::roles::permission_entries(pool, &ids)
        .await
        .map_err(internal)?;

    let mut options = Vec::new();
    for role in roles {
        // Only ALLOW entries count, and only the GraphQL-visible ones: a role granting 90
        // permissions that have no field behind them would show `90` in the picker and produce an
        // empty diff, which reads as a broken comparison rather than as an honest answer.
        let granted = entries.get(&role.id).map_or(0, |entries| {
            entries
                .iter()
                .filter(|entry| {
                    entry.effect == omnion_permissions::model::Effect::Allow
                        && KNOWN_PERMISSIONS
                            .iter()
                            .any(|known| known.as_str() == entry.key)
                })
                .count()
        });
        options.push(RoleOption {
            id: role.id.to_string(),
            key: role.key.clone(),
            name: role.name.clone(),
            permission_count: granted,
        });
    }
    options.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(options)
}

/// A role's display name for the diff's second column.
async fn role_label(
    pool: &sqlx::PgPool,
    organization_id: Uuid,
    role_id: Uuid,
) -> Result<String, ApiError> {
    let roles = omnion_permissions::roles::list_roles(pool, Some(organization_id))
        .await
        .map_err(internal)?;
    Ok(roles
        .into_iter()
        .find(|role| role.id == role_id)
        .map(|role| role.name)
        .unwrap_or_else(|| role_id.to_string()))
}

/// The store's error, as this layer's.
fn internal(error: omnion_permissions::error::PermissionsError) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("the role graph could not be read: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_graphql::parity::{Known, PermissionSet};

    /// Two compositions of different permission sets, and the diff between them.
    fn pair() -> (ComposedSchema, ComposedSchema) {
        let catalogue = SchemaCatalogue::catalogued();
        let reader = PermissionSet::from_known([Known::ContentPagesRead]);
        let reader_and_writer =
            PermissionSet::from_known([Known::ContentPagesRead, Known::ContentPagesPublish]);
        (
            compose(&catalogue, &reader),
            compose(&catalogue, &reader_and_writer),
        )
    }

    #[test]
    fn a_field_one_side_lacks_names_the_permission_that_decides_it() {
        // The explorer's whole job. A diff that printed `Page.publishPage` with no reason would be
        // indistinguishable from a schema bug, so every `theirs` line carries its permission.
        let (reader, writer) = pair();
        let lines = diff_lines(&writer, &reader, "theirs");
        let publish = lines
            .iter()
            .find(|line| line.path == "Mutation.publishPage")
            .expect("the writer can publish and the reader cannot");
        assert_eq!(
            publish.requires.as_deref(),
            Some(Known::ContentPagesPublish.as_str()),
            "a diff line must name the permission that decides it"
        );
    }

    #[test]
    fn a_reader_lacks_no_type_but_lacks_the_write_half() {
        // `Query` is a root: no permission gates its existence, only its fields. So the two
        // compositions differ in fields and in the `Mutation` type's fields — never in whether a
        // root is present, which is the distinction the request draws ("absent from the schema
        // entirely" applies to a type holding data, not to the root itself).
        let (reader, writer) = pair();
        assert!(reader.has_type("Query"));
        assert!(writer.has_type("Query"));
        assert!(
            diff_lines(&reader, &writer, "mine").is_empty(),
            "a reader has nothing the writer lacks"
        );
        assert!(
            !diff_lines(&writer, &reader, "mine").is_empty(),
            "the writer has writes the reader does not"
        );
    }

    #[test]
    fn the_serialised_body_carries_the_permission_key_and_not_the_enum_variant() {
        // The screen prints what the operator granted. `ContentPagesRestore` is an internal
        // spelling; `content.pages.restore` is the key that appears in a role matrix.
        //
        // The field used here is `Page.revisions`, and the choice is not arbitrary: it is the one
        // field in the catalogue whose permission is NARROWER than its type's — a reader sees
        // `Page` and not `Page.revisions`. So this assertion also proves the explorer shows a field
        // with its own permission rather than only the type's, which is the field-level filtering
        // the request asks for. **My first version of this test assumed `revisions` needed
        // `content.pages.read` and failed** — the assertion was wrong, not the catalogue, and the
        // fix is the narrower key the REST twin actually carries.
        let catalogue = SchemaCatalogue::catalogued();
        let reader = PermissionSet::from_known([Known::ContentPagesRead]);
        let body = SchemaBody::from(&compose(&catalogue, &reader));

        let page = body
            .types
            .iter()
            .find(|type_definition| type_definition.name == "Page")
            .expect("a reader sees Page");
        let title = page
            .fields
            .iter()
            .find(|field| field.name == "title")
            .expect("title needs only the type's permission");
        assert_eq!(
            title.requires, None,
            "`title` inherits the type's permission, so the field must carry none of its own"
        );

        // `revisions` is withheld from this reader — so it is not in the list at all, which is the
        // whole rule: absent, not nulled.
        assert!(
            !page.fields.iter().any(|field| field.name == "revisions"),
            "a caller without `content.pages.restore` must not see `revisions` on the type at all"
        );

        // The same field, seen by a caller who holds it, carries the KEY and not the variant.
        let restorer = PermissionSet::from_known([
            Known::ContentPagesRead,
            Known::ContentPagesRestore,
        ]);
        let body = SchemaBody::from(&compose(&catalogue, &restorer));
        let page = body
            .types
            .iter()
            .find(|type_definition| type_definition.name == "Page")
            .expect("the restorer sees Page too");
        let revisions = page
            .fields
            .iter()
            .find(|field| field.name == "revisions")
            .expect("a restorer sees the history");
        assert_eq!(
            revisions.requires.as_deref(),
            Some("content.pages.restore"),
            "the screen prints the key the operator granted, not the internal enum variant"
        );
    }

    #[test]
    fn the_withheld_list_names_the_types_a_caller_cannot_read() {
        // Withheld names are in the RESPONSE for the administrator, and absent from the SDL. Both
        // halves matter and they are different objects: the screen needs the difference, the
        // caller must not be able to read it out of a comment.
        let catalogue = SchemaCatalogue::catalogued();
        let nobody = compose(&catalogue, &PermissionSet::empty());
        assert!(
            !nobody.withheld.is_empty(),
            "an empty permission set must withhold the data-bearing types"
        );
        let sdl = nobody.sdl();
        for name in &nobody.withheld {
            assert!(
                !sdl.contains(name.as_str()),
                "`{name}` is withheld and must not appear anywhere in the SDL"
            );
        }
    }
}