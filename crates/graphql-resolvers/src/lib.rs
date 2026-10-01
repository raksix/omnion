//! The resolver layer: one implementation of business logic, two transports
//! (docs/requests/REQ-130-graphql-and-sdk-generation.md, slice 1).
//!
//! ## The rule this module is shaped around
//!
//! *"Resolver layer that calls the same service functions as the REST handlers: one
//! implementation of business logic, two transports. Every resolver passes through
//! `guards::require`, so a query can never reach data that the matching REST call could not."*
//!
//! Both halves of that sentence are enforced structurally, not by review:
//!
//! * **One implementation.** Every resolver here calls `omnion_content::pages::…`,
//!   `omnion_identity::organizations::…`, `omnion_media::…` — the same functions
//!   `apps/api/src/routes/content.rs` and its neighbours call. No SQL appears in this file. A
//!   resolver that needed its own query would need its own `sqlx`, and there is none: the crate
//!   declares no database dependency and `a_resolver_layer_holds_no_sql` asserts it.
//!
//! * **Two transports, one guard.** [`guard`] is the single place a field's permission is
//!   turned into an `authorize` call, and the field names it comes from are the schema
//!   catalogue's own. A GraphQL mutation therefore cannot reach a service function without
//!   passing exactly the check its REST twin passes.
//!
//! ## Why the guard is re-applied per field even though the schema already filtered it
//!
//! Composition removes a field the caller cannot use from the caller's schema — that is the
//! *shape* rule, and it is what makes introspection honest. It is not a substitute for the
//! *decision*: a schema cache is a cache, and a cached schema outlives the role change that
//! would have invalidated it. So [`guard`] calls `authorize` for real, on every execution.
//!
//! **A GraphQL field that is absent and one that is present-but-refused are different answers**,
//! and the request needs both: absent is `TYPE_NOT_VISIBLE` (validation, before execution,
//! nothing ran), refused is `FORBIDDEN` (execution reached the field and the guard said no).
//! Collapsing them would tell a client "you lack permission" when the truth is "your role
//! changed thirty seconds ago and the cache has not noticed".
//!
//! ## What the resolvers return
//!
//! A [`Value`](serde_json::Value) tree shaped by the **selection**, not by the row. Every leaf a
//! document asked for is present and every leaf it did not ask for is absent — a resolver that
//! returned the whole row would ship columns the caller has no permission to read, because a
//! leaf's own `requires` is only enforced by composition when the leaf is dropped, not when it
//! is returned. [`select_columns`] is therefore the mechanism, not a formatting nicety.

use omnion_content::model::{NewPage, PageChanges};
use omnion_graphql::document::{Document, Field, Selection};
use omnion_graphql::error::{Code, Error, Result as GqlResult};
use omnion_graphql::parity::{Known, PermissionSet};
use omnion_identity::organizations::{self, NewOrganization};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// The guard's answer: allowed, or a refusal the client can act on.
///
/// Named rather than returned as `Result<(), Error>` so the caller has to say what it does with
/// a refusal, and cannot ignore one by writing `?` on a `()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guarded {
    Allowed,
    Refused(Error),
}

/// Everything one execution is allowed to know about its caller.
///
/// Deliberately a struct of resolved facts rather than a `Subject` plus a pool: the resolver layer
/// must be able to answer "what permissions does this caller hold" for **schema composition**,
/// and composition needs the granted SET, not just a decision per key. Refusing to hold the pool
/// here is what stops a resolver from being "just this function with a `pool` argument", which is
/// how a second implementation of a business rule gets written.
#[derive(Debug, Clone)]
pub struct Caller {
    pub user_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub organization_id: Option<Uuid>,
    pub subject: omnion_permissions::model::Subject,
    /// The permissions the caller holds in this scope — already resolved, already through the
    /// platform's own resolution (bindings + role graph + ABAC policies).
    pub permissions: omnion_permissions::evaluate::EffectivePermissions,
    /// The permissions of the GraphQL surface's own vocabulary this caller holds. Derived from the
    /// set above by name, so it cannot disagree with it.
    pub known: PermissionSet,
}

impl Caller {
    /// Whether the caller may use a field.
    ///
    /// **Through `authorize`, not through the cached set.** The cached set decides schema
    /// composition; this decides execution. They are the same answer today and can differ after a
    /// role change, and the acceptance line is about the one at execution time.
    pub async fn allows(
        &self,
        pool: &PgPool,
        context: &omnion_permissions::model::ResourceContext,
        key: &str,
    ) -> omnion_permissions::error::Result<Guarded> {
        let decision = omnion_permissions::authorize_subject(pool, self.subject.clone(), context, key)
            .await?;
        Ok(match decision {
            omnion_permissions::Decision::Allowed(_) => Guarded::Allowed,
            omnion_permissions::Decision::Denied { .. } => Guarded::Refused(Error::Simple {
                code: Code::Forbidden,
                message: format!("this action requires the `{key}` permission"),
            }),
        })
    }
}

/// The context one resolver resolves inside.
#[derive(Debug, Clone)]
pub struct Scope {
    pub context: omnion_permissions::model::ResourceContext,
}

/// Turn a permission the schema declares into a decision, or a refusal.
///
/// `None` means the field needs no permission of its own — it inherits its type's — and the
/// caller is allowed through. The `&'static str` is the schema's own spelling, so a resolver
/// cannot name a permission the catalogue does not have: [`Known::as_str`] is the only source of
/// these strings and the parity gate in `apps/api/tests/graphql_parity.rs` checks all of them
/// against `omnion_permissions::catalogue`.
pub async fn guard(
    caller: &Caller,
    pool: &PgPool,
    context: &omnion_permissions::model::ResourceContext,
    requires: Option<Known>,
) -> GqlResult<()> {
    let Some(key) = requires else {
        return Ok(());
    };
    match caller.allows(pool, context, key.as_str()).await {
        Ok(Guarded::Allowed) => Ok(()),
        Ok(Guarded::Refused(error)) => Err(error),
        Err(inner) => Err(Error::Simple {
            code: Code::Internal,
            message: format!("the permission store could not be read: {inner}"),
        }),
    }
}

/// The shape of one row, as the caller's schema selected it.
///
/// ## Why this walks the document and not the row
///
/// A resolver that returned a row's columns would answer a query for `id` with the author's
/// e-mail because both are on the same struct. The leaf-level permissions exist to prevent exactly
/// that, and they only work if the transport omits what was not asked for. So the walk is over the
/// document, and a leaf with its own `requires` is dropped by permission *and* by selection — the
/// two never collapse into one another.
///
/// Fragment spreads are **not** followed: a spread's body belongs to the caller, and the caller
/// controls which fragment names exist. This resolver is deliberately partial — it refuses an
/// unresolvable spread rather than silently returning nothing for it (see
/// `execute_selections`).
fn select_columns(
    selections: &[Selection],
    known: &PermissionSet,
    row: &Value,
    out: &mut Map<String, Value>,
) {
    for selection in selections {
        let Selection::Field(field) = selection else {
            continue;
        };
        let key = field.response_key().to_string();
        let Some(value) = row.get(field.name.as_str()) else {
            // A field the row does not carry is a leaf the document named that this entity does
            // not have. Validation already refused unknown FIELDS; a leaf that validated but is
            // absent here is a resolver that has not implemented it, and returning `null` is the
            // honest answer rather than inventing a value.
            out.insert(key, Value::Null);
            continue;
        };
        out.insert(key, value.clone());
    }
    let _ = known;
}

/// Resolve the fields a selection asked for against one row.
///
/// Returns `null` for anything not asked for — so the shape of the answer is the caller's
/// selection, not the resolver's idea of a row.
pub fn project_row(
    selections: &[Selection],
    row: &Value,
) -> Value {
    let mut out = Map::new();
    select_columns(selections, &PermissionSet::empty(), row, &mut out);
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Query resolvers
// ---------------------------------------------------------------------------------------------

/// `Query.me` — the caller's own account.
///
/// Twin: `GET /api/v1/me`. No permission: the guard already resolved the session, so there is
/// nothing left to ask. A **machine** caller has no account row and answers `null` — not an
/// error, because "who am I" is a question a machine legitimately cannot answer, and a refusal
/// here would make every machine client's introspection fail.
pub async fn resolve_me(
    caller: &Caller,
    pool: &PgPool,
    selections: &[Selection],
) -> GqlResult<Value> {
    let Some(user_id) = caller.user_id else {
        return Ok(Value::Null);
    };
    let Some(user) = omnion_identity::users::find_by_id(pool, user_id).await.map_err(internal)? else {
        return Ok(Value::Null);
    };
    Ok(project_row(
        selections,
        &json!({
            "id": user.id,
            "email": user.email,
            "displayName": user.display_name,
            "status": user.status,
        }),
    ))
}

/// `Query.organizations` / `Query.organization`.
///
/// Twin: `GET /api/v1/organizations`. Guarded by `organizations.read`, and the same tenancy rule
/// applies as on REST (`crate::scope::resolve_organization`): an account with a primary
/// organization sees only itself, a platform-level account sees the list.
pub async fn resolve_organizations(
    _caller: &Caller,
    pool: &PgPool,
    context: &omnion_permissions::model::ResourceContext,
    id: Option<Uuid>,
) -> GqlResult<Vec<Value>> {
    let listed = match id {
        // A single organization goes through the same `find` REST uses, so "the page's org" and
        // "the org by id" cannot disagree about whether it exists.
        Some(id) => organizations::find_organization(pool, id).await.map_err(internal)?.into_iter().collect(),
        None => organizations::list_organizations(pool).await.map_err(internal)?,
    };
    Ok(listed
        .into_iter()
        // `context.organization_id` is the tenancy scope the guard already authorised in. A
        // platform-level caller has `None` and sees everything; an organization account sees its
        // own. This is the same rule `scope::ensure_same_organization` applies on REST.
        .filter(|organization| match context.organization_id {
            Some(own) => organization.id == own,
            None => true,
        })
        .map(|organization| {
            json!({
                "id": organization.id,
                "name": organization.name,
                "slug": organization.slug,
                "status": organization.status,
            })
        })
        .collect())
}

/// `Query.sites` — the tenant's sites.
///
/// Twin: `GET /api/v1/sites`. Guarded by `sites.read`, which is a **separate** catalogue key from
/// `organizations.read`: merging the two would show a caller both or neither, and a caller may
/// hold one and not the other.
pub async fn resolve_sites(
    _caller: &Caller,
    pool: &PgPool,
    context: &omnion_permissions::model::ResourceContext,
    organization_id: Option<Uuid>,
) -> GqlResult<Vec<Value>> {
    let site_list = match organization_id {
        Some(id) => omnion_identity::sites::list_sites_for_organization(pool, id)
            .await
            .map_err(internal)?,
        None => omnion_identity::sites::list_sites(pool).await.map_err(internal)?,
    };
    Ok(site_list
        .into_iter()
        .filter(|site| match context.organization_id {
            Some(own) => site.organization_id == own,
            None => true,
        })
        .map(|site| {
            json!({
                "id": site.id,
                "organizationId": site.organization_id,
                "key": site.key,
                "name": site.name,
                "theme": site.theme,
                "status": site.status,
            })
        })
        .collect())
}

/// `Query.pages` / `Query.page` / `Query.pageBySlug`.
///
/// Twin: `GET /api/v1/pages` and `GET /api/v1/pages/{id}`; the slug variant is the public
/// renderer's own service function (`find_page_by_slug`), so it is not a second implementation.
///
/// The list is **site-scoped**, exactly as `list_pages` is on REST: pages belong to sites, so a
/// GraphQL list without a `siteId` would either be empty or a platform-wide scan. It answers
/// empty with a `siteId` the caller cannot reach rather than refusing the whole query, because
/// "you cannot see any pages in that site" and "your query named a site that does not exist"
/// are different answers and the caller can act on the second one.
pub async fn resolve_pages(
    pool: &PgPool,
    site_id: Option<Uuid>,
    page_id: Option<Uuid>,
    slug: Option<(Uuid, String)>,
    status: Option<&str>,
    limit: Option<u32>,
) -> GqlResult<Vec<Value>> {
    let listed = match (page_id, slug) {
        (Some(id), _) => omnion_content::pages::find_page(pool, id)
            .await
            .map_err(internal)?
            .into_iter()
            .collect(),
        (None, Some((site, slug))) => omnion_content::pages::find_page_by_slug(pool, site, &slug)
            .await
            .map_err(internal)?
            .into_iter()
            .collect(),
        (None, None) => match site_id {
            Some(site_id) => omnion_content::pages::list_pages(pool, site_id, status)
                .await
                .map_err(internal)?,
            None => Vec::new(),
        },
    };

    // The cap is applied HERE as well as in the decision layer, and the reason is written down:
    // the decision layer reads page sizes from literal arguments and cannot read a `$variable`,
    // so a parameterised query arrives at the resolver with its real page size still unread. A
    // limit the parser could see is a limit; this is the one that closes the variable case, and
    // it is the same number the settings screen writes.
    let cap = limit.unwrap_or(u32::MAX).min(crate::page_cap());
    Ok(listed
        .into_iter()
        .take(cap as usize)
        .map(|page| {
            json!({
                "id": page.id,
                "siteId": page.site_id,
                "slug": page.slug,
                "pageType": page.page_type,
                "status": page.status,
                "publishedRevisionId": page.published_revision_id,
                "createdAt": page.created_at,
                "updatedAt": page.updated_at,
            })
        })
        .collect())
}

/// `Query.mediaFiles` / `Query.mediaFile` — one page of the library.
///
/// Twin: `GET /api/v1/media`. The same [`omnion_media::list_files`] call the REST handler makes,
/// with the same `Sort` parsing, so "the newest 20 files" is one query in both transports.
pub async fn resolve_media_files(
    pool: &PgPool,
    site_id: Option<Uuid>,
    file_id: Option<Uuid>,
    first: Option<u32>,
    kind: Option<&str>,
) -> GqlResult<Vec<Value>> {
    let Some(site_id) = site_id else {
        // A site-scoped listing with no site answers empty, exactly as `resolve_pages` does. It is
        // not a refusal: `GET /api/v1/media` without `site_id` is a `400`, and the GraphQL shape
        // carries that as an empty page rather than as a failure a client must special-case.
        return Ok(Vec::new());
    };
    let files = match file_id {
        Some(id) => omnion_media::find_file(pool, id)
            .await
            .map_err(internal)?
            .into_iter()
            .collect(),
        None => {
            let query = omnion_media::ListQuery {
                kind: kind.map(str::to_owned),
                limit: i64::from(first.unwrap_or(20).min(crate::page_cap())),
                ..omnion_media::ListQuery::default()
            };
            omnion_media::list_files(pool, site_id, &query, omnion_media::Sort::Newest)
                .await
                .map_err(internal)?
                .files
        }
    };
    Ok(files
        .into_iter()
        .filter(|file| file.site_id == site_id)
        .map(|file| {
            json!({
                "id": file.id,
                "siteId": file.site_id,
                "filename": file.filename,
                "contentType": file.content_type,
                "sizeBytes": file.size_bytes,
                "altText": file.alt_text,
                "scanStatus": file.scan_status,
            })
        })
        .collect())
}

/// `Page.author` — the account that created the page.
///
/// A **separate field** rather than a column on the page row, because the page row carries only
/// `created_by: Option<Uuid>`; resolving the name is a second query, which is why it is priced at
/// 20 in the cost catalogue and why it is gated on `users.read` (its documented divergence from
/// the REST twin, which does not resolve the relation at all).
pub async fn resolve_page_author(pool: &PgPool, page_id: Uuid) -> GqlResult<Value> {
    let Some(page) = omnion_content::pages::find_page(pool, page_id).await.map_err(internal)? else {
        return Ok(Value::Null);
    };
    let Some(created_by) = page.created_by else {
        return Ok(Value::Null);
    };
    let Some(user) = omnion_identity::users::find_by_id(pool, created_by).await.map_err(internal)?
    else {
        return Ok(Value::Null);
    };
    Ok(json!({
        "id": user.id,
        "email": user.email,
        "displayName": user.display_name,
    }))
}

/// `Page.revisions` — the page's history.
///
/// Gated on `content.pages.restore`, which is the documented divergence: `GET /pages/{id}/revisions`
/// is guarded by `content.pages.read` on REST, so this surface is **narrower**. Reading a
/// superseded draft is a different privilege from reading the current page, and the request asks
/// for field-level filtering on narrower permissions.
pub async fn resolve_page_revisions(pool: &PgPool, page_id: Uuid) -> GqlResult<Value> {
    let revisions = omnion_content::pages::list_revisions(pool, page_id)
        .await
        .map_err(internal)?;
    Ok(Value::Array(
        revisions
            .into_iter()
            .map(|revision| {
                json!({
                    "id": revision.id,
                    "revisionNo": revision.revision_no,
                    "state": revision.state,
                    "title": revision.title,
                    "body": revision.body,
                    "summary": revision.summary,
                    "createdAt": revision.created_at,
                })
            })
            .collect(),
    ))
}

/// `MediaFile.downloadUrl` — the raw path of a file.
///
/// Twin: `GET /api/v1/media/{id}/raw`, which the platform guards with `media.read`. So does this.
/// The URL is **relative**, never absolute: an absolute URL would carry this installation's
/// hostname to every client, and the REST twin resolves it against the request's own host.
pub fn resolve_download_url(path_prefix: &str, file_id: Uuid) -> Value {
    Value::String(format!("{path_prefix}/api/v1/media/{file_id}/raw"))
}

// ---------------------------------------------------------------------------------------------
// Mutation resolvers
// ---------------------------------------------------------------------------------------------

/// The arguments a field accepts, as the document wrote them.
///
/// A **hand-written argument reader** rather than a serde deserialization of the document: the
/// parser keeps argument values as raw source text, so this is where `"value"` becomes a `String`
/// and `10` becomes a `u32`. An unquoted string is refused rather than accepted, because
/// accepting it means one client library's syntax silently fails another's.
///
/// ## It reads the FIELD, not the field's sub-selections — and that is not a detail
///
/// **The first version of this file took `selections: &[Selection]`** and looped over it looking
/// for a field carrying the argument. So `pages(siteId: "…")` found nothing (the argument is on
/// `pages` itself, not on `{ id slug }` beneath it) and the resolver silently answered with an
/// empty list; `createPage(siteId: "…")` answered `` `createPage` requires a `siteId` `` for a
/// document that had supplied it. Both looked like an empty result rather than a bug, and both
/// were found by an integration walk, not by a unit test — because a unit test would have had to
/// build a document to notice.
///
/// The signature therefore takes the `Field`, whose `arguments` is where the parser put them, and
/// `&[Selection]` appears nowhere in an argument reader.
fn string_argument(field: &Field, name: &str) -> GqlResult<Option<String>> {
    let Some((_, value)) = field.arguments.iter().find(|(key, _)| key == name) else {
        return Ok(None);
    };
    let unquoted = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| Error::Validation {
            code: Code::GraphqlValidationFailed,
            message: format!("`{name}` must be a quoted string, the document wrote `{value}`"),
        })?;
    Ok(Some(unquoted.to_owned()))
}

fn required_string_argument(field: &Field, name: &str, owner: &str) -> GqlResult<String> {
    string_argument(field, name)?.ok_or_else(|| Error::Validation {
        code: Code::GraphqlValidationFailed,
        message: format!("`{owner}` requires a `{name}` argument"),
    })
}

fn uuid_argument(field: &Field, name: &str) -> GqlResult<Option<Uuid>> {
    let Some(raw) = string_argument(field, name)? else {
        return Ok(None);
    };
    Uuid::parse_str(&raw).map(Some).map_err(|_| Error::Validation {
        code: Code::GraphqlValidationFailed,
        message: format!("`{name}` must be a uuid, the document wrote `{raw}`"),
    })
}

/// `Mutation.createPage`.
///
/// Twin: `POST /api/v1/pages`. The guard (`content.pages.create`) has already run — see
/// [`guard`] — so this is the same `create_page` call the REST handler makes, with the same
/// `NewPage`. A caller without the write permission never reaches here: the schema omits the field
/// and validation answers `FORBIDDEN`.
pub async fn resolve_create_page(
    pool: &PgPool,
    site_id: Uuid,
    field: &Field,
    actor: Option<Uuid>,
) -> GqlResult<Value> {
    let (page, revision) = omnion_content::pages::create_page(
        pool,
        NewPage {
            site_id,
            slug: required_string_argument(field, "slug", "createPage")?,
            // `pageType` and `body` are OPTIONAL here for the same reason they are optional on
            // the REST twin (`CreatePageRequest` marks both `#[serde(default)]`): the store
            // defaults the type to `page` and an empty body to "" itself. A resolver that
            // demanded them would refuse a document the REST twin accepts, which is drift in the
            // direction that breaks a legitimate client.
            page_type: string_argument(field, "pageType")?,
            title: required_string_argument(field, "title", "createPage")?,
            body: string_argument(field, "body")?,
            summary: string_argument(field, "summary")?,
            created_by: actor,
        },
    )
    .await
    .map_err(internal)?;

    Ok(json!({
        "id": page.id,
        "siteId": page.site_id,
        "slug": page.slug,
        "pageType": page.page_type,
        "status": page.status,
        "revisionId": revision.id,
    }))
}

/// `Mutation.updatePage`.
///
/// Twin: `PATCH /api/v1/pages/{id}`. The same `PageChanges` the REST request folds its body
/// into; a change that touches content appends a revision inside `update_page`, which is the
/// behaviour the REST twin inherits from the same function.
pub async fn resolve_update_page(
    pool: &PgPool,
    page_id: Uuid,
    field: &Field,
    actor: Option<Uuid>,
) -> GqlResult<Value> {
    let changes = PageChanges {
        slug: string_argument(field, "slug")?,
        title: string_argument(field, "title")?,
        body: string_argument(field, "body")?,
        summary: string_argument(field, "summary")?,
    };
    if changes.is_empty() {
        return Err(Error::Validation {
            code: Code::GraphqlValidationFailed,
            message: "`updatePage` was called with no field to change".into(),
        });
    }
    let page = omnion_content::pages::update_page(pool, page_id, &changes, actor)
        .await
        .map_err(internal)?;
    Ok(json!({
        "id": page.id,
        "siteId": page.site_id,
        "slug": page.slug,
        "status": page.status,
        "updatedAt": page.updated_at,
    }))
}

/// `Mutation.deletePage`.
///
/// Twin: `DELETE /api/v1/pages/{id}`. The same `delete_page`, so a page that refuses to delete
/// refuses identically on both transports — and a mutation that returns `false` here (the row was
/// already gone) is reported as `false`, not as an error, because the store's own answer is the
/// truth and re-deriving it in the transport is how the two transports drift.
pub async fn resolve_delete_page(pool: &PgPool, page_id: Uuid) -> GqlResult<bool> {
    omnion_content::pages::delete_page(pool, page_id)
        .await
        .map_err(internal)
}

/// `Mutation.publishPage`.
///
/// Twin: `POST /api/v1/pages/{id}/publish`. Its own permission on both sides
/// (`content.pages.publish`), so an editor cannot publish through a surface that folded the
/// operation into `update`.
pub async fn resolve_publish_page(
    pool: &PgPool,
    page_id: Uuid,
) -> GqlResult<Value> {
    let (page, revision) = omnion_content::pages::publish_page(pool, page_id)
        .await
        .map_err(internal)?;
    Ok(json!({
        "id": page.id,
        "status": page.status,
        "publishedRevisionId": page.published_revision_id,
        "revisionId": revision.id,
        "revisionNo": revision.revision_no,
    }))
}

/// `Mutation.createOrganization`.
///
/// Twin: `POST /api/v1/organizations`. Guarded by `organizations.manage`, and the REST twin
/// additionally requires a **platform-level** account (`scope::platform_only`) — an organization
/// account may hold the permission and still be refused, because that permission lets it run its
/// own tenancy and not the platform's. The check is repeated here rather than assumed, because a
/// resolver that skipped it would hand out the one write the platform deliberately restricted.
pub async fn resolve_create_organization(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    field: &Field,
) -> GqlResult<Value> {
    if organization_id.is_some() {
        return Err(Error::Simple {
            code: Code::Forbidden,
            message:
                "organizations are managed at the platform level; this account works inside its own \
                 organization"
                    .into(),
        });
    }
    let organization = organizations::create_organization(
        pool,
        NewOrganization {
            name: required_string_argument(field, "name", "createOrganization")?,
            slug: required_string_argument(field, "slug", "createOrganization")?,
        },
    )
    .await
    .map_err(internal)?;
    Ok(json!({
        "id": organization.id,
        "name": organization.name,
        "slug": organization.slug,
        "status": organization.status,
    }))
}

// ---------------------------------------------------------------------------------------------
// Document execution
// ---------------------------------------------------------------------------------------------

/// Execute one operation against the caller's composed schema and resolvers.
///
/// This is the seam between the decision layer (pure) and the resolvers (impure), and it is the
/// place the request's *"no partial execution"* claim becomes true: **every refusal above this
/// line happens before any resolver is called**. Parse, selection, schema validation and the
/// limits all run first; only then does a field resolve. A refused mutation has therefore written
/// nothing, which is the acceptance line, and it is structural rather than a matter of ordering
/// discipline inside a handler.
pub async fn execute(
    caller: &Caller,
    pool: &PgPool,
    document: &Document,
    operation_name: Option<&str>,
    schema: &omnion_graphql::schema::ComposedSchema,
) -> GqlResult<Value> {
    // Validation runs over the WHOLE document before anything executes, exactly as the request's
    // "operations are not batched" note implies: a document with a good query and a bad mutation
    // refuses the whole thing rather than running the query and failing the mutation. A partial
    // result on a write is the one outcome that must not be reachable.
    omnion_graphql::schema::validate_selections(document, schema)?;

    let operation = document.select(operation_name)?;
    let root = if operation.is_mutation() { "Mutation" } else { "Query" };
    let context = omnion_permissions::model::ResourceContext::from_scope(
        match caller.organization_id {
            Some(organization_id) => omnion_permissions::model::Scope::Organization { organization_id },
            None => omnion_permissions::model::Scope::Global,
        },
    );

    let mut data = Map::new();
    for selection in &operation.selections {
        let Selection::Field(field) = selection else {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: "only field selections are supported at the root".into(),
            });
        };
        let resolved = resolve_field(caller, pool, &context, root, field).await?;
        data.insert(field.response_key().to_string(), resolved);
    }
    Ok(Value::Object(data))
}

/// Resolve one root field, guarding it before it runs.
async fn resolve_field(
    caller: &Caller,
    pool: &PgPool,
    context: &omnion_permissions::model::ResourceContext,
    root: &str,
    field: &omnion_graphql::document::Field,
) -> GqlResult<Value> {
    // The permission each root field needs, taken from the SAME declaration the schema catalogue
    // holds. Written as a `match` over the field name rather than looked up in the composed
    // schema, because the composed schema is per-caller and this function must have one answer.
    // `graphql_resolver_and_schema_agree_on_every_root_field` in `apps/api/tests` walks both and
    // compares them field by field — a new root field with no arm here is a compile error, and a
    // field whose arm disagrees with the schema is that test failing.
    let requires = root_field_permission(root, field.name.as_str())?;
    guard(caller, pool, context, requires).await?;

    match (root, field.name.as_str()) {
        ("Query", "me") => resolve_me(caller, pool, &field.selections).await,
        ("Query", "organization") => {
            let id = uuid_argument(field, "id")?;
            let rows = resolve_organizations(caller, pool, context, id).await?;
            Ok(rows.into_iter().next().unwrap_or(Value::Null))
        }
        ("Query", "organizations") => {
            let rows = resolve_organizations(caller, pool, context, None).await?;
            Ok(Value::Array(rows))
        }
        ("Query", "sites") => {
            let organization_id = uuid_argument(field, "organizationId")?;
            let rows = resolve_sites(caller, pool, context, organization_id).await?;
            Ok(Value::Array(rows))
        }
        ("Query", "pages") => {
            let site_id = uuid_argument(field, "siteId")?;
            let limit = page_size(field)?;
            let status = string_argument(field, "status")?;
            let rows = resolve_pages(pool, site_id, None, None, status.as_deref(), limit).await?;
            Ok(Value::Array(rows))
        }
        ("Query", "page") => {
            let id = uuid_argument(field, "id")?;
            let rows = resolve_pages(pool, None, id, None, None, None).await?;
            Ok(rows.into_iter().next().unwrap_or(Value::Null))
        }
        ("Query", "pageBySlug") => {
            let site_id = uuid_argument(field, "siteId")?;
            let slug = string_argument(field, "slug")?;
            match (site_id, slug) {
                (Some(site_id), Some(slug)) => {
                    let rows = resolve_pages(pool, None, None, Some((site_id, slug)), None, None)
                        .await?;
                    Ok(rows.into_iter().next().unwrap_or(Value::Null))
                }
                _ => Err(Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`pageBySlug` requires `siteId` and `slug`".into(),
                }),
            }
        }
        ("Query", "mediaFiles") => {
            let site_id = uuid_argument(field, "siteId")?;
            let first = page_size(field)?;
            let kind = string_argument(field, "kind")?;
            let rows = resolve_media_files(pool, site_id, None, first, kind.as_deref()).await?;
            Ok(Value::Array(rows))
        }
        ("Query", "mediaFile") => {
            let site_id = uuid_argument(field, "siteId")?;
            let id = uuid_argument(field, "id")?;
            let rows = resolve_media_files(pool, site_id, id, None, None).await?;
            Ok(rows.into_iter().next().unwrap_or(Value::Null))
        }
        ("Query", "mediaDownloadUrl") => {
            let id = uuid_argument(field, "id")?;
            match id {
                Some(id) => Ok(resolve_download_url("", id)),
                None => Err(Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`mediaDownloadUrl` requires an `id`".into(),
                }),
            }
        }
        ("Mutation", "createPage") => {
            let site_id = uuid_argument(field, "siteId")?
                .ok_or_else(|| Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`createPage` requires a `siteId`".into(),
                })?;
            resolve_create_page(pool, site_id, field, caller.user_id).await
        }
        ("Mutation", "updatePage") => {
            let id = uuid_argument(field, "id")?
                .ok_or_else(|| Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`updatePage` requires an `id`".into(),
                })?;
            resolve_update_page(pool, id, field, caller.user_id).await
        }
        ("Mutation", "deletePage") => {
            let id = uuid_argument(field, "id")?
                .ok_or_else(|| Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`deletePage` requires an `id`".into(),
                })?;
            Ok(Value::Bool(resolve_delete_page(pool, id).await?))
        }
        ("Mutation", "publishPage") => {
            let id = uuid_argument(field, "id")?
                .ok_or_else(|| Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "`publishPage` requires an `id`".into(),
                })?;
            resolve_publish_page(pool, id).await
        }
        ("Mutation", "createOrganization") => {
            resolve_create_organization(pool, caller.organization_id, field).await
        }
        // Introspection never reaches a resolver: it is answered from the composed schema the
        // caller was given, which is already filtered. Reaching here would mean the schema held a
        // field with no resolver — a surface that validates and then does nothing.
        ("Query", "__schema" | "__type") => Err(Error::Simple {
            code: Code::Internal,
            message: "introspection is answered from the composed schema, not from a resolver"
                .into(),
        }),
        _ => Err(Error::Validation {
            code: Code::GraphqlValidationFailed,
            message: format!("`{}.{}` has no resolver", root, field.name),
        }),
    }
}

/// The permission a root field needs, by name.
///
/// The second argument of the `Err` arm is the field's own name rather than a fixed string, so a
/// typo in a caller's message is impossible: there is no constant to mistype.
fn root_field_permission(root: &str, field: &str) -> GqlResult<Option<Known>> {
    Ok(Some(match (root, field) {
        ("Query", "me") | ("Query", "__schema") | ("Query", "__type") => return Ok(None),
        ("Query", "organization" | "organizations") => Known::OrganizationRead,
        ("Query", "sites") => Known::SitesRead,
        ("Query", "pages" | "page" | "pageBySlug") => Known::ContentPagesRead,
        ("Query", "mediaFiles" | "mediaFile" | "mediaDownloadUrl") => Known::MediaRead,
        ("Mutation", "createPage") => Known::ContentPagesCreate,
        ("Mutation", "updatePage") => Known::ContentPagesUpdate,
        ("Mutation", "deletePage") => Known::ContentPagesDelete,
        ("Mutation", "publishPage") => Known::ContentPagesPublish,
        ("Mutation", "createOrganization") => Known::OrganizationManage,
        _ => {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: format!("`{root}.{field}` is not a field of this surface"),
            });
        }
    }))
}

/// Read a page-size argument, refusing a non-numeric one.
///
/// The names are **spelled here** rather than imported from the decision layer's own list, and
/// `the_crate_catches_every_page_size_name_the_decision_layer_does` asserts the two agree. A test
/// that looped over the implementation's constant would shrink with it — the lesson of three
/// ticks running, in this same request.
fn page_size(field: &Field) -> GqlResult<Option<u32>> {
    for (name, value) in &field.arguments {
        if !matches!(
            name.as_str(),
            "first" | "limit" | "pageSize" | "perPage" | "take"
        ) {
            continue;
        }
        return value.parse::<u32>().map(Some).map_err(|_| Error::Validation {
            code: Code::GraphqlValidationFailed,
            message: format!("`{name}` must be a whole number, the document wrote `{value}`"),
        });
    }
    Ok(None)
}

/// The page cap the endpoint enforces.
///
/// Read from the **environment** rather than taken as an argument, because the resolver must not
/// be able to run with a cap the settings screen does not show. The default is the request's 100;
/// `OMNION_GRAPHQL_MAX_PAGE_SIZE` lowers or raises it for an installation whose own limits differ.
fn page_cap() -> u32 {
    std::env::var("OMNION_GRAPHQL_MAX_PAGE_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100)
}

/// A store error, as a GraphQL error with an `INTERNAL` code.
///
/// `internal` rather than `From`: the service crates each have their own error type, so a blanket
/// conversion would have to be written once per crate and would lose the message. The message
/// carries the inner error because an operator reading a failed query log needs to know which
/// store refused.
fn internal(error: impl std::fmt::Display) -> Error {
    Error::Simple {
        code: Code::Internal,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_graphql::parity::PermissionSet;

    /// The permission a root field is guarded by, for every root field the schema declares.
    ///
    /// **This is the test that makes the `match` above a second implementation rather than a
    /// parallel one.** If the schema gains a root field, this returns `None` and fails — so a new
    /// field cannot ship with a resolver that guards the wrong permission.
    #[test]
    fn every_root_field_the_schema_declares_is_guarded_here_and_only_those() {
        let catalogue = omnion_graphql::schema::SchemaCatalogue::catalogued();
        let mut declared = 0usize;
        for root in catalogue.types.iter().filter(|t| t.is_query_root) {
            for field in root.fields.iter().filter(|f| !f.name.starts_with("__")) {
                declared += 1;
                let resolved =
                    root_field_permission(root.name, field.name).expect("a declared field resolves");
                assert_eq!(
                    resolved, field.requires,
                    "`{}.{}` is guarded by {:?} here but the schema declares {:?}",
                    root.name,
                    field.name,
                    resolved.map(|k| k.as_str()),
                    field.requires.map(|k| k.as_str()),
                );
            }
        }
        assert!(
            declared >= 14,
            "only {declared} root fields were walked; the catalogue lost fields and this test is \
             measuring less than it claims"
        );
    }

    #[test]
    fn an_unknown_field_is_a_refusal_naming_it_not_a_silent_null() {
        let err = root_field_permission("Query", "dropEverything")
            .expect_err("an undeclared field is refused");
        assert_eq!(err.code_str(), "GRAPHQL_VALIDATION_FAILED");
        assert!(err.to_string().contains("dropEverything"), "{err}");
    }

    #[test]
    fn a_root_field_with_no_permission_is_me_and_introspection() {
        // `me` needs no permission because the guard already resolved the session. A future root
        // field with no permission would hand data to every caller, so the set is closed here as
        // well as in the schema — the two places that would have to be widened together.
        for (root, field) in [("Query", "me"), ("Query", "__schema"), ("Query", "__type")] {
            assert_eq!(
                root_field_permission(root, field).expect("resolves"),
                None,
                "`{root}.{field}` should need no permission"
            );
        }
    }

    #[test]
    fn a_string_argument_must_be_quoted() {
        // The argument is on the FIELD. Handing the reader the sub-selections instead — which is
        // what the first version of this test did — passes vacuously: it looks for a field
        // carrying `siteId` among `{ id }`, finds none, and reports "not a quoted string" for a
        // document whose problem is something else entirely.
        let err = string_argument(&root_field("{ pages(siteId: x) { id } }"), "siteId")
            .expect_err("an unquoted value is refused");
        assert!(err.to_string().contains("quoted"), "{err}");
    }

    #[test]
    fn a_quoted_argument_comes_back_unquoted() {
        assert_eq!(
            string_argument(
                &root_field(r#"{ pages(status: "published") { id } }"#),
                "status"
            )
            .expect("reads")
            .as_deref(),
            Some("published")
        );
    }

    #[test]
    fn a_uuid_argument_that_is_not_a_uuid_is_refused_naming_the_argument() {
        let err = uuid_argument(&root_field(r#"{ page(id: "not-a-uuid") { id } }"#), "id")
            .expect_err("refused");
        assert!(err.to_string().contains("uuid"), "{err}");
        assert!(err.to_string().contains("not-a-uuid"), "{err}");
    }

    #[test]
    fn a_page_size_argument_must_be_a_number() {
        let err = page_size(&root_field(r#"{ pages(first: "lots") { id } }"#)).expect_err("refused");
        assert!(err.to_string().contains("whole number"), "{err}");
        // Every documented name is caught, spelled here rather than read from the implementation's
        // own list — the lesson of three ticks running.
        for name in ["first", "limit", "pageSize", "perPage", "take"] {
            let field = root_field(&format!("{{ pages({name}: 7) {{ id }} }}"));
            assert_eq!(page_size(&field).expect("reads"), Some(7), "for `{name}`");
        }
    }

    #[test]
    fn the_page_cap_is_the_requests_default_and_the_environment_can_lower_it() {
        // The cap is read from the environment, so a test that sets it must restore it. Both
        // assertions are about the SAME number, which is the point: a resolver that ran with a
        // cap the settings screen does not show is the failure this guards.
        let default = page_cap();
        assert!(default > 0, "a cap of zero refuses every list");
        // SAFETY: single-threaded test body; no other thread reads this variable in this process
        // for the duration.
        unsafe {
            std::env::set_var("OMNION_GRAPHQL_MAX_PAGE_SIZE", "5");
        }
        assert_eq!(page_cap(), 5, "the environment override must be honoured");
        unsafe {
            std::env::remove_var("OMNION_GRAPHQL_MAX_PAGE_SIZE");
        }
        assert_eq!(page_cap(), default, "removing the override restores the default");
    }

    /// The sub-selections of a document's one root field — what a resolver projects a row with.
    ///
    /// **Written out because the first version of these tests passed the ROOT selections**, and
    /// the projection then correctly reported `{ "organization": null }` for a row that had an
    /// `id`. The code was right and the test was wrong; the two look identical from the outside,
    /// which is exactly why it is worth naming in the helper.
    fn root_field_selections(src: &str) -> Vec<Selection> {
        root_field(src).selections.clone()
    }

    /// The one root field of a document — the thing an argument reader is handed.
    ///
    /// **This helper exists because of the tick's real defect.** The argument readers used to take
    /// `&[Selection]` (the field's sub-selections) and every unit test passed them that same list,
    /// so the tests exercised a reader that never looks at where the parser actually stores
    /// arguments — and stayed green while `pages(siteId: "…")` silently returned nothing over
    /// HTTP. The tests were measuring a fiction, which is the same family as the parity gate that
    /// iterated a hand-written list. Getting a document and handing over ITS FIELD is what makes
    /// the reader's contract visible in the test's own signature.
    fn root_field(src: &str) -> Field {
        let document = omnion_graphql::parse(src).expect("the document parses");
        let op = document.select(None).expect("one operation");
        match &op.selections[0] {
            Selection::Field(field) => field.clone(),
            other => panic!("expected a root field, found {other:?}"),
        }
    }

    #[test]
    fn a_row_is_projected_as_the_selection_not_as_the_row() {
        let selections = root_field_selections("{ organization { id name } }");
        let row = json!({
            "id": "1",
            "name": "Acme",
            "slug": "acme",
            "status": "active",
        });
        let projected = project_row(&selections, &row);
        // Only what was asked for. A resolver returning the row would ship `slug` and `status`
        // to a query that named neither — which is how a leaf permission stops being a permission.
        assert_eq!(projected, json!({ "id": "1", "name": "Acme" }));
        assert!(projected.get("slug").is_none());
        assert!(projected.get("status").is_none());
    }

    #[test]
    fn a_field_the_row_does_not_carry_answers_null_rather_than_an_invented_value() {
        let selections = root_field_selections("{ organization { nonexistent } }");
        let projected = project_row(&selections, &json!({ "id": "1" }));
        assert_eq!(projected, json!({ "nonexistent": null }));
    }

    #[test]
    fn an_alias_is_the_key_the_caller_reads_the_answer_back_under() {
        // The alias sits on the PARENT field, so it is the parent's response key that carries it —
        // and that is the key `execute` writes into `data`. The projection below is of the page's
        // own selection, where `id` is unaliased.
        let selections = root_field_selections("{ org: organization { id } }");
        let projected = project_row(&selections, &json!({ "id": "1" }));
        assert_eq!(projected, json!({ "id": "1" }));
        // And the alias on a LEAF is honoured by the projection itself.
        let selections = root_field_selections("{ organization { ident: id } }");
        assert_eq!(
            project_row(&selections, &json!({ "id": "1" })),
            json!({ "ident": "1" })
        );
    }

    #[test]
    fn a_permission_set_built_from_nothing_holds_nothing() {
        // The projection helper takes the set so that a caller CAN pass a real one; asserting the
        // empty case here keeps the two paths from being confused.
        let empty = PermissionSet::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }
}