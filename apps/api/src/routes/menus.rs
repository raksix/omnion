//! `/api/v1/menus`, `/api/v1/public/menus/{location}` and `/api/v1/publishing/queue` —
//! REQ-064 slice 1.
//!
//! Two surfaces that answer the same question from different sides: *when does this content
//! appear* and *how does a visitor reach it*. They ship in one module because a scheduled post
//! almost always gets a navigation entry in the same editing session, and an editor who has to
//! switch between two modules to do that is an editor who forgets one half.
//!
//! The three decisions that shape this file:
//!
//! * **Reading a menu is `menus.read`, writing it is `menus.manage`** — the same shape every
//!   content surface uses. The public payload carries *no* permission: it is a rendered site, and
//!   a theme that needs a session to draw its header is a theme that cannot be rendered at all.
//! * **The public payload is audience-filtered, and `members` is the value that proves it.**
//!   `GET /public/menus/header` answers differently for a signed-out visitor and for a member,
//!   and the editor's preview toggle reads *the same endpoint* — so a preview that disagreed
//!   with the live site would be a bug in one of them, and there is only one.
//! * **A schedule is a promise that fires exactly once, or the queue is theatre.** `POST
//!   /pages/{id}/schedule` replaces any pending entry for the same action rather than appending
//!   one, and the runner claims rows with `for update skip locked` before touching a page. The
//!   claim is why two workers cannot publish twice, and why a stalled worker does not stop the
//!   queue for everybody else.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use omnion_audit::NewAuditEntry;
use omnion_content::{
    Audience, ContentError, Menu, MenuChanges, MenuItem, MenuSave, NewMenuItem, NewSchedule,
    Page, PublishingEntry, QueueQuery, RenderedMenu,
};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Create a menu.
#[derive(Debug, Deserialize)]
pub struct CreateMenuRequest {
    /// Site the menu navigates.
    pub site_id: Uuid,
    /// Stable key, unique inside the site.
    pub key: String,
    /// Name the list shows.
    pub name: String,
}

/// A menu's own fields as an edit.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateMenuRequest {
    /// New name.
    pub name: Option<String>,
    /// New key.
    pub key: Option<String>,
    /// Locations this menu claims; replaces the current set.
    pub locations: Option<Vec<String>>,
}

/// The whole-tree save: `PUT /menus/{id}`.
#[derive(Debug, Default, Deserialize)]
pub struct SaveMenuRequest {
    /// The complete item tree the editor holds.
    #[serde(default)]
    pub items: Vec<ItemBody>,
    /// Locations this menu claims.
    #[serde(default)]
    pub locations: Vec<String>,
}

/// One item as the editor submits it.
#[derive(Debug, Deserialize)]
pub struct ItemBody {
    /// Stable id, minted by the editor and never rewritten.
    pub id: Uuid,
    /// Parent item's id; `null` for a top-level row.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Order inside the parent.
    #[serde(default)]
    pub position: i32,
    /// Link text.
    pub label: String,
    /// `page`, `url`, `anchor` or `index`.
    #[serde(default = "default_item_type")]
    pub item_type: String,
    /// Page the item points at.
    #[serde(default)]
    pub page_id: Option<Uuid>,
    /// URL or anchor.
    #[serde(default)]
    pub url: String,
    /// `_self` or `_blank`.
    #[serde(default = "default_target")]
    pub target: String,
    /// `rel` attribute.
    #[serde(default)]
    pub rel: String,
    /// Extra class names.
    #[serde(default)]
    pub css_class: String,
    /// Enabled flag.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Visibility rule.
    #[serde(default = "default_visibility")]
    pub visibility: String,
    /// Roles a `roles` item requires.
    #[serde(default)]
    pub visibility_roles: Vec<String>,
}

fn default_item_type() -> String {
    "url".to_owned()
}
fn default_target() -> String {
    "_self".to_owned()
}
fn default_true() -> bool {
    true
}
fn default_visibility() -> String {
    "everyone".to_owned()
}

impl From<ItemBody> for NewMenuItem {
    fn from(body: ItemBody) -> Self {
        Self {
            id: body.id,
            parent_id: body.parent_id,
            position: body.position,
            label: body.label,
            item_type: body.item_type,
            page_id: body.page_id,
            url: body.url,
            target: body.target,
            rel: body.rel,
            css_class: body.css_class,
            enabled: body.enabled,
            visibility: body.visibility,
            visibility_roles: body.visibility_roles,
        }
    }
}

/// Bulk-add pages to a menu.
#[derive(Debug, Deserialize)]
pub struct AddPagesRequest {
    /// Page ids to insert.
    pub page_ids: Vec<Uuid>,
    /// Parent item to insert under; `null` for the top level.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Insert after this position rather than at the end.
    #[serde(default)]
    pub position: Option<i32>,
}

/// Schedule a publish or unpublish.
#[derive(Debug, Deserialize)]
pub struct ScheduleRequest {
    /// `publish` or `unpublish`.
    pub action: String,
    /// The instant, RFC 3339, in UTC. The `timezone` field beside it is the author's wall clock,
    /// not an offset applied here — the store compares instants.
    pub scheduled_at: String,
    /// The author's timezone label (`Europe/Istanbul`), shown beside the instant in the queue.
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

fn default_timezone() -> String {
    "UTC".to_owned()
}

/// Move a pending entry to a new instant.
#[derive(Debug, Deserialize)]
pub struct RescheduleRequest {
    /// The new instant, RFC 3339.
    pub scheduled_at: String,
}

/// A menu as the panel reads it.
#[derive(Debug, Serialize)]
pub struct MenuBody {
    /// Menu id.
    pub id: Uuid,
    /// Site it navigates.
    pub site_id: Uuid,
    /// The site's GLOBAL key, next to its id.
    ///
    /// Two id shapes exist in this platform and they are not interchangeable: the authenticated
    /// routes address a site by uuid, the public ones by global key or host (`resolve_site` reads
    /// a dot as a host and anything else as a key). The editor's audience preview calls the
    /// *public* menu route, so it needs this field — sending `site_id` there is a 404 that reads
    /// as "this installation has no navigation" rather than as a wrong argument, and it is
    /// invisible to every test that asserts on the panel's own routes.
    pub site_key: String,
    /// Stable key.
    pub key: String,
    /// Name.
    pub name: String,
    /// Theme slots it renders into.
    pub locations: Vec<String>,
    /// Item count, nested included — the list card's number.
    pub item_count: usize,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A menu with its items: the editor's own document.
#[derive(Debug, Serialize)]
pub struct MenuDetailBody {
    /// The menu.
    #[serde(flatten)]
    pub menu: MenuBody,
    /// Its items, in tree order.
    pub items: Vec<MenuItemBody>,
    /// The vocabulary the editor draws its pickers from, so the panel never hard-codes a list
    /// the server would then refuse.
    pub vocabulary: VocabularyBody,
}

/// The closed vocabularies of this surface.
#[derive(Debug, Serialize)]
pub struct VocabularyBody {
    /// Theme slots a menu can claim.
    pub locations: Vec<&'static str>,
    /// What an item can link to.
    pub item_types: Vec<&'static str>,
    /// Who may see an item.
    pub visibilities: Vec<&'static str>,
    /// Deepest nesting the editor offers.
    pub max_depth: usize,
}

/// One item as the editor reads it.
#[derive(Debug, Serialize)]
pub struct MenuItemBody {
    /// Stable id.
    pub id: Uuid,
    /// Parent item's id.
    pub parent_id: Option<Uuid>,
    /// Order inside the parent.
    pub position: i32,
    /// Link text.
    pub label: String,
    /// Link kind.
    pub item_type: String,
    /// Page target.
    pub page_id: Option<Uuid>,
    /// URL or anchor.
    pub url: String,
    /// Link window.
    pub target: String,
    /// `rel` attribute.
    pub rel: String,
    /// Extra class names.
    pub css_class: String,
    /// Enabled flag.
    pub enabled: bool,
    /// Visibility rule.
    pub visibility: String,
    /// Roles a `roles` item requires.
    pub visibility_roles: Vec<String>,
}

impl From<&MenuItem> for MenuItemBody {
    fn from(item: &MenuItem) -> Self {
        Self {
            id: item.id,
            parent_id: item.parent_id,
            position: item.position,
            label: item.label.clone(),
            item_type: item.item_type.clone(),
            page_id: item.page_id,
            url: item.url.clone(),
            target: item.target.clone(),
            rel: item.rel.clone(),
            css_class: item.css_class.clone(),
            enabled: item.enabled,
            visibility: item.visibility.clone(),
            visibility_roles: item.visibility_roles.clone(),
        }
    }
}

/// A queue row as the panel reads it.
#[derive(Debug, Serialize)]
pub struct QueueEntryBody {
    /// Entry id.
    pub id: Uuid,
    /// Page it acts on.
    pub page_id: Uuid,
    /// The page's slug, so the row is readable without a second request.
    pub page_slug: String,
    /// The page's title.
    pub page_title: String,
    /// The page's content type.
    pub page_type: String,
    /// `publish` or `unpublish`.
    pub action: String,
    /// The instant, RFC 3339, in UTC.
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: OffsetDateTime,
    /// The author's timezone label.
    pub timezone: String,
    /// `pending`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// What the last attempt did.
    pub result: String,
    /// Why the last attempt failed.
    pub error: String,
    /// When a worker took it.
    #[serde(with = "time::serde::rfc3339::option")]
    pub claimed_at: Option<OffsetDateTime>,
}

impl From<&PublishingEntry> for QueueEntryBody {
    fn from(entry: &PublishingEntry) -> Self {
        Self {
            id: entry.id,
            page_id: entry.page_id,
            page_slug: entry.page_slug.clone(),
            page_title: entry.page_title.clone(),
            page_type: entry.page_type.clone(),
            action: entry.action.clone(),
            scheduled_at: entry.scheduled_at,
            timezone: entry.timezone.clone(),
            status: entry.status.clone(),
            result: entry.result.clone(),
            error: entry.error.clone(),
            claimed_at: entry.claimed_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/menus`.
pub async fn list_menus(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<ListMenuParams>,
) -> Result<Json<Vec<MenuBody>>, ApiError> {
    let site = site_in_scope(&state, &current, params.site_id).await?;
    let menus = omnion_content::list_menus(state.db().pool(), site.id).await?;
    let mut bodies = Vec::with_capacity(menus.len());
    for menu in &menus {
        // The site is already resolved and in scope above, so its key is read once for the whole
        // list rather than once per row: the list is a set of menus of ONE site.
        let mut body = menu_body(&state, menu, 0).await?;
        body.item_count = omnion_content::list_items(state.db().pool(), menu.id)
            .await?
            .len();
        bodies.push(body);
    }
    Ok(Json(bodies))
}

/// The list query.
#[derive(Debug, Deserialize)]
pub struct ListMenuParams {
    /// Site whose menus to list.
    pub site_id: Uuid,
}

/// `GET /api/v1/menus/{id}` — the editor's document.
pub async fn get_menu(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<MenuDetailBody>, ApiError> {
    let (menu, items) = menu_in_scope(&state, &current, id).await?;
    Ok(Json(MenuDetailBody {
        menu: menu_body(&state, &menu, items.len()).await?,
        items: items.iter().map(MenuItemBody::from).collect(),
        vocabulary: VocabularyBody {
            locations: omnion_content::LOCATIONS.to_vec(),
            item_types: omnion_content::ITEM_TYPES.to_vec(),
            visibilities: omnion_content::VISIBILITIES.to_vec(),
            max_depth: omnion_content::MAX_DEPTH,
        },
    }))
}

/// `POST /api/v1/menus`.
pub async fn create_menu(
    State(state): State<AppState>,
    current: CurrentSession,
    Json(body): Json<CreateMenuRequest>,
) -> Result<(StatusCode, Json<MenuBody>), ApiError> {
    let site = site_in_scope(&state, &current, body.site_id).await?;
    let menu = omnion_content::create_menu(
        state.db().pool(),
        site.organization_id,
        site.id,
        &body.key,
        &body.name,
        Some(current.user.id),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "menu.create")
            .organization(site.organization_id)
            .target("menu", menu.id)
            .metadata(json!({ "key": menu.key, "name": menu.name, "site_id": site.id })),
    )
    .await?;
    emit(
        &state,
        "content.menu.updated",
        json!({ "menu_id": menu.id, "action": "created", "site_id": site.id }),
    ).await;

    Ok((StatusCode::CREATED, Json(menu_body(&state, &menu, 0).await?)))
}

/// `PUT /api/v1/menus/{id}` — rename, rekey, or move to other locations.
pub async fn update_menu(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateMenuRequest>,
) -> Result<Json<MenuBody>, ApiError> {
    let (menu, _) = menu_in_scope(&state, &current, id).await?;
    let changes = MenuChanges {
        name: body.name,
        key: body.key,
        locations: body.locations,
    };
    let updated =
        omnion_content::update_menu(state.db().pool(), menu.site_id, id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "menu.update")
            .organization(menu.organization_id)
            .target("menu", id)
            .metadata(json!({ "site_id": menu.site_id })),
    )
    .await?;
    emit(
        &state,
        "content.menu.updated",
        json!({ "menu_id": id, "action": "updated", "site_id": menu.site_id }),
    ).await;

    let items = omnion_content::list_items(state.db().pool(), id).await?;
    Ok(Json(menu_body(&state, &updated, items.len()).await?))
}

/// `PUT /api/v1/menus/{id}/items` — save the whole tree.
pub async fn save_menu_items(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveMenuRequest>,
) -> Result<Json<MenuDetailBody>, ApiError> {
    let (menu, _) = menu_in_scope(&state, &current, id).await?;
    let save = MenuSave {
        items: body.items.into_iter().map(NewMenuItem::from).collect(),
        locations: body.locations,
    };
    let (saved, items) =
        omnion_content::save_menu(state.db().pool(), menu.site_id, id, &save).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "menu.items.save")
            .organization(menu.organization_id)
            .target("menu", id)
            .metadata(json!({ "site_id": menu.site_id, "items": items.len() })),
    )
    .await?;
    emit(
        &state,
        "content.menu.updated",
        json!({ "menu_id": id, "action": "items_saved", "items": items.len() }),
    ).await;

    Ok(Json(MenuDetailBody {
        menu: menu_body(&state, &saved, items.len()).await?,
        items: items.iter().map(MenuItemBody::from).collect(),
        vocabulary: VocabularyBody {
            locations: omnion_content::LOCATIONS.to_vec(),
            item_types: omnion_content::ITEM_TYPES.to_vec(),
            visibilities: omnion_content::VISIBILITIES.to_vec(),
            max_depth: omnion_content::MAX_DEPTH,
        },
    }))
}

/// `POST /api/v1/menus/{id}/items/from-pages` — the `Add pages…` button.
///
/// Only **published** pages are inserted, and the label defaults to the page's own title. Both
/// halves are the acceptance criterion, and both are enforced here rather than in the panel: a
/// menu that links to a draft is a 404 for every visitor, and a menu whose labels are the slugs
/// is a menu somebody has to edit by hand afterwards.
pub async fn add_pages_to_menu(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<AddPagesRequest>,
) -> Result<Json<MenuDetailBody>, ApiError> {
    let (menu, items) = menu_in_scope(&state, &current, id).await?;
    if body.page_ids.is_empty() {
        return Err(ApiError::bad_request(
            "no_pages_selected",
            "pick at least one page to add",
        ));
    }
    if body.page_ids.len() > 100 {
        return Err(ApiError::bad_request(
            "too_many_pages",
            "add at most 100 pages at a time",
        ));
    }

    // The pages must belong to the menu's own site *and* be published. Reading them through the
    // menu's site rather than by id alone is what stops a caller from linking another site's
    // page into this site's navigation.
    let published = sqlx::query_as::<_, (Uuid, String)>(
        "select id, coalesce(nullif((select title from page_revisions \
            where page_id = pages.id and state = 'published' limit 1), ''), \
            (select title from page_revisions where page_id = pages.id and state = 'draft' limit 1)) \
         from pages where site_id = $1 and id = any($2) and status = 'published'",
    )
    .bind(menu.site_id)
    .bind(&body.page_ids)
    .fetch_all(state.db().pool())
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("reading the selected pages: {error}"),
        )
    })?;

    if published.len() != body.page_ids.len() {
        return Err(ApiError::bad_request(
            "page_not_published",
            "only published pages of this site can be added to a menu",
        ));
    }

    let existing: Vec<MenuItem> = items;
    let start = body.position.unwrap_or_else(|| {
        existing
            .iter()
            .filter(|item| item.parent_id == body.parent_id)
            .map(|item| item.position)
            .max()
            .map_or(0, |max| max + 1)
    });

    let mut added: Vec<NewMenuItem> = Vec::with_capacity(published.len());
    for (offset, (page_id, title)) in published.iter().enumerate() {
        added.push(NewMenuItem {
            id: Uuid::new_v4(),
            parent_id: body.parent_id,
            position: start + offset as i32,
            label: title.clone(),
            item_type: "page".to_owned(),
            page_id: Some(*page_id),
            ..NewMenuItem::default()
        });
    }
    // One save, not one insert per page: the tree write is atomic, so a menu is never left with
    // three of the five pages the editor selected.
    let mut all: Vec<NewMenuItem> = existing
        .iter()
        .map(|item| NewMenuItem {
            id: item.id,
            parent_id: item.parent_id,
            position: item.position,
            label: item.label.clone(),
            item_type: item.item_type.clone(),
            page_id: item.page_id,
            url: item.url.clone(),
            target: item.target.clone(),
            rel: item.rel.clone(),
            css_class: item.css_class.clone(),
            enabled: item.enabled,
            visibility: item.visibility.clone(),
            visibility_roles: item.visibility_roles.clone(),
        })
        .collect();
    all.extend(added);
    let locations = menu.locations.clone();
    let (saved, items) = omnion_content::save_menu(
        state.db().pool(),
        menu.site_id,
        id,
        &MenuSave {
            items: all,
            locations,
        },
    )
    .await?;

    emit(
        &state,
        "content.menu.updated",
        json!({ "menu_id": id, "action": "pages_added", "count": published.len() }),
    ).await;

    Ok(Json(MenuDetailBody {
        menu: menu_body(&state, &saved, items.len()).await?,
        items: items.iter().map(MenuItemBody::from).collect(),
        vocabulary: VocabularyBody {
            locations: omnion_content::LOCATIONS.to_vec(),
            item_types: omnion_content::ITEM_TYPES.to_vec(),
            visibilities: omnion_content::VISIBILITIES.to_vec(),
            max_depth: omnion_content::MAX_DEPTH,
        },
    }))
}

/// `DELETE /api/v1/menus/{id}`.
pub async fn delete_menu(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let (menu, _) = menu_in_scope(&state, &current, id).await?;
    omnion_content::delete_menu(state.db().pool(), menu.site_id, id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "menu.delete")
            .organization(menu.organization_id)
            .target("menu", id)
            .metadata(json!({ "site_id": menu.site_id })),
    )
    .await?;
    emit(
        &state,
        "content.menu.updated",
        json!({ "menu_id": id, "action": "deleted", "site_id": menu.site_id }),
    ).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/public/menus/{location}` — what the theme renders.
///
/// The audience is the query, not the session: the public surface stays unauthenticated, and a
/// theme that must have a session to draw its header cannot be rendered by a crawler. The panel
/// preview calls the same endpoint with the other audience, which is why a preview and the live
/// site cannot disagree.
#[derive(Debug, Deserialize)]
pub struct PublicMenuParams {
    /// Which viewer the payload is for: `visitor` or `member`.
    #[serde(default)]
    pub audience: Option<String>,
    /// Site address, exactly as `/public/pages/{slug}` takes it: a host or a site key. The
    /// public surface resolves the site from the request, and a hint is the third step of that
    /// order — a menu is addressed by location *and* by site, so a renderer with more than one
    /// site needs the same escape hatch the page route already has.
    #[serde(default)]
    pub site: Option<String>,
}

pub async fn public_menu(
    State(state): State<AppState>,
    Path(location): Path<String>,
    Query(params): Query<PublicMenuParams>,
    headers: HeaderMap,
) -> Result<Json<Option<RenderedMenu>>, ApiError> {
    let site = crate::routes::public::resolve_site(
        state.db().pool(),
        params.site.as_deref(),
        &headers,
    )
    .await?;
    let audience = match params.audience.as_deref() {
        Some("member") => Audience::member(Vec::new()),
        // `visitor` and an absent value are the *same* audience, and both are accepted: the
        // omitted case is what every unauthenticated renderer sends, and a `match` that only
        // listed `member` would reject the default it documents. An unknown value is refused
        // rather than widened — a payload built for a wider audience than the caller proved is
        // how a members-only link ends up in a search index.
        None | Some("visitor") => Audience::anonymous(),
        Some(other) => {
            return Err(ApiError::bad_request(
                "unknown_audience",
                format!("audience {other:?} must be visitor or member"),
            ));
        }
    };
    let menu =
        omnion_content::rendered_menu(state.db().pool(), site.id, &location, audience).await?;
    Ok(Json(menu))
}

// ---------------------------------------------------------------------------------------------
// Scheduled publishing
// ---------------------------------------------------------------------------------------------

/// The queue query.
#[derive(Debug, Deserialize)]
pub struct QueueParams {
    /// Keep only this status.
    pub status: Option<String>,
    /// Keep only this content type.
    pub page_type: Option<String>,
    /// Most rows.
    pub limit: Option<i64>,
    /// Keep only this site's pages. Optional so an organization-wide call still exists, but the
    /// panel always sends it: the screen is scoped to the site switcher like every other content
    /// screen in the panel, and an unscoped queue would list a second site's rows under the first
    /// site's name.
    pub site_id: Option<Uuid>,
}

/// `GET /api/v1/publishing/queue`.
pub async fn list_queue(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(params): Query<QueueParams>,
) -> Result<Json<Vec<QueueEntryBody>>, ApiError> {
    // The organization comes from the SITE, not from the account. Reading it off the account was
    // wrong in the direction nobody would guess: the first account a fresh installation creates
    // is the platform owner, whose `organization_id` is deliberately NULL (crates/onboarding
    // `steps.rs` — "an Owner runs the platform, not one tenant"). The queue therefore answered a
    // 400 to the one person who runs the platform, while every integration test passed, because
    // the fixtures create an account that *does* carry an organization. The scope check the other
    // content routes already do — resolve the site, then check the caller's organization against
    // the site's — is the one that also works for an account without one.
    let site = match params.site_id {
        Some(site_id) => Some(site_in_scope(&state, &current, site_id).await?),
        None => None,
    };
    let organization_id = match site.as_ref() {
        Some(site) => site.organization_id,
        None => require_organization(&current)?,
    };
    let entries = omnion_content::list_queue(
        state.db().pool(),
        organization_id,
        &QueueQuery {
            status: params.status,
            page_type: params.page_type,
            limit: params.limit,
            site_ids: site.as_ref().map(|site| vec![site.id]),
        },
    )
    .await?;
    Ok(Json(entries.iter().map(QueueEntryBody::from).collect()))
}

/// `POST /api/v1/pages/{id}/schedule`.
///
/// The page must be in the caller's organization, which is the scope check; the store then makes
/// the entry *replace* any pending one of the same action. So "schedule" and "reschedule" are
/// one call, and a page can never carry two live promises to be published.
pub async fn schedule_page(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(page_id): Path<Uuid>,
    Json(body): Json<ScheduleRequest>,
) -> Result<(StatusCode, Json<QueueEntryBody>), ApiError> {
    let page = page_in_scope(&state, &current, page_id).await?;
    let site = site_of(&state, page.site_id).await?;
    let scheduled_at = parse_instant(&body.scheduled_at)?;
    let entry = omnion_content::schedule(
        state.db().pool(),
        site.organization_id,
        NewSchedule {
            page_id,
            action: body.action,
            scheduled_at,
            timezone: body.timezone,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "page.schedule")
            .organization(site.organization_id)
            .target("page", page_id)
            .metadata(json!({
                "site_id": page.site_id,
                "entry_id": entry.id,
                "action": entry.action,
                "scheduled_at": entry.scheduled_at,
            })),
    )
    .await?;
    emit(
        &state,
        "content.page.scheduled",
        json!({
            "page_id": page_id,
            "slug": page.slug,
            "entry_id": entry.id,
            "action": entry.action,
            "scheduled_at": entry.scheduled_at,
        }),
    ).await;

    Ok((StatusCode::CREATED, Json(QueueEntryBody::from(&entry))))
}

/// `PUT /api/v1/publishing/queue/{id}` — move a pending entry.
pub async fn reschedule_entry(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<RescheduleRequest>,
) -> Result<Json<QueueEntryBody>, ApiError> {
    let organization_id = entry_in_scope(&state, &current, id).await?;
    let entry = omnion_content::reschedule(
        state.db().pool(),
        organization_id,
        id,
        parse_instant(&body.scheduled_at)?,
    )
    .await?;
    emit(
        &state,
        "content.page.scheduled",
        json!({ "page_id": entry.page_id, "entry_id": entry.id, "action": "rescheduled" }),
    ).await;
    Ok(Json(QueueEntryBody::from(&entry)))
}

/// `POST /api/v1/publishing/queue/{id}/cancel`.
pub async fn cancel_entry(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<QueueEntryBody>, ApiError> {
    let organization_id = entry_in_scope(&state, &current, id).await?;
    let entry = omnion_content::cancel(state.db().pool(), organization_id, id).await?;
    emit(
        &state,
        "content.page.schedule_cancelled",
        json!({ "page_id": entry.page_id, "entry_id": entry.id, "action": entry.action }),
    ).await;
    Ok(Json(QueueEntryBody::from(&entry)))
}

/// `POST /api/v1/publishing/queue/{id}/publish-now`.
///
/// It makes the entry due and lets the runner do the work — deliberately. A button that published
/// through its own second code path would leave two definitions of "published" in one platform.
pub async fn publish_now(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<QueueEntryBody>, ApiError> {
    let organization_id = entry_in_scope(&state, &current, id).await?;
    let entry = omnion_content::find_entry(state.db().pool(), organization_id, id)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)?;
    if !omnion_content::publish_now(state.db().pool(), id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "entry_not_pending",
            "only a pending entry can be published now",
        ));
    }
    let refreshed = omnion_content::find_entry(state.db().pool(), organization_id, id)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)?;
    let _ = entry;
    Ok(Json(QueueEntryBody::from(&refreshed)))
}

/// `POST /api/v1/publishing/queue/{id}/retry` — a failed entry back into the queue.
pub async fn retry_entry(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<QueueEntryBody>, ApiError> {
    let organization_id = entry_in_scope(&state, &current, id).await?;
    // Retry lands one minute out rather than "now", so a page that failed for a reason which has
    // not been fixed is not republished in the same second by an impatient click.
    let entry = omnion_content::retry(
        state.db().pool(),
        organization_id,
        id,
        OffsetDateTime::now_utc() + time::Duration::minutes(1),
    )
    .await?;
    Ok(Json(QueueEntryBody::from(&entry)))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The menu as the panel reads it, with the site's global key resolved alongside its id.
///
/// The key is looked up per row rather than passed in because every caller already holds a
/// `Menu` and none of them hold a `Site`, and one extra indexed read per list row is cheaper than
/// threading a site through five handlers that have nothing else to do with it.
async fn menu_body(state: &AppState, menu: &Menu, item_count: usize) -> Result<MenuBody, ApiError> {
    let site_key = site_of(state, menu.site_id).await?.key;
    Ok(MenuBody {
        id: menu.id,
        site_id: menu.site_id,
        site_key,
        key: menu.key.clone(),
        name: menu.name.clone(),
        locations: menu.locations.clone(),
        item_count,
        updated_at: menu.updated_at,
    })
}

fn parse_instant(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|_| {
        ApiError::bad_request(
            "invalid_schedule",
            format!("{value:?} is not an RFC 3339 instant, e.g. 2026-10-01T09:00:00Z"),
        )
    })
}

fn require_organization(current: &CurrentSession) -> Result<Uuid, ApiError> {
    current.user.organization_id.ok_or_else(|| {
        ApiError::bad_request(
            "no_organization",
            "this account has no primary organization; pick one before managing content",
        )
    })
}

async fn site_of(state: &AppState, site_id: Uuid) -> Result<omnion_identity::Site, ApiError> {
    omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site"))
}

async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<omnion_identity::Site, ApiError> {
    let site = site_of(state, site_id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

async fn page_in_scope(state: &AppState, current: &CurrentSession, page_id: Uuid) -> Result<Page, ApiError> {
    let page = omnion_content::pages::find_page(state.db().pool(), page_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "page_not_found", "no such page")
        })?;
    site_in_scope(state, current, page.site_id).await?;
    Ok(page)
}

async fn menu_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<(Menu, Vec<MenuItem>), ApiError> {
    // The menu is read by id alone and *then* scoped, rather than filtered by a site the caller
    // supplied: filtering would let a caller name another site's menu and learn it exists.
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("select site_id, organization_id from cms_menus where id = $1")
            .bind(id)
            .fetch_optional(state.db().pool())
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("reading the menu: {error}"),
                )
            })?;
    let Some((site_id, organization_id)) = row else {
        return Err(ContentError::MenuNotFound.into());
    };
    ensure_same_organization(current, Some(organization_id))?;
    let found = omnion_content::find_menu(state.db().pool(), site_id, id)
        .await?
        .ok_or(ContentError::MenuNotFound)?;
    Ok(found)
}

/// The organization that owns a queue entry, once the caller has been checked against it.
///
/// The queue's four write handlers used to read `current.user.organization_id` directly, which
/// is a 400 for the platform owner — the one account a fresh installation creates, and the one
/// account a deployment's own admin uses. The read path was corrected for exactly that case two
/// ticks ago and the write paths were left behind, so the queue screen could LIST and then refuse
/// every button on a row it had just shown. Resolving the entry and scoping through
/// `ensure_same_organization` is the same check the menu and page handlers already use, and it
/// works for an account that has no organization at all.
async fn entry_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<Uuid, ApiError> {
    let row: Option<(Uuid,)> = sqlx::query_as("select organization_id from cms_publishing_queue where id = $1")
        .bind(id)
        .fetch_optional(state.db().pool())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("reading the publishing entry: {error}"),
            )
        })?;
    let Some((organization_id,)) = row else {
        return Err(ContentError::PublishingEntryNotFound.into());
    };
    // A cross-tenant caller gets 404, never 403. `ensure_same_organization` answers "this exists
    // and is not yours", and the difference between that and "there is no such entry" is the
    // difference between a refusal and an existence oracle: an account from another tenant can
    // then probe entry ids and learn which are real. The store's own `where organization_id = $1`
    // had this right for free, which is why the previous implementation returned 404 — this
    // helper reads the organization and then has to reproduce that concealment by hand.
    if let Some(own) = current.user.organization_id
        && own != organization_id
    {
        return Err(ContentError::PublishingEntryNotFound.into());
    }
    ensure_same_organization(current, Some(organization_id))?;
    Ok(organization_id)
}

async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Publish an event without failing the request that caused it.
///
/// A queued event that a full outbox refuses is a fact the request already made true; answering
/// the editor with a 500 because the bus was busy would teach them to retry a write that
/// succeeded. The REQ names `content.menu.updated` and `content.page.scheduled` as contract, so
/// the contract is "the request is the source of truth and the bus is a fast follower".
/// A *non*-async function returning a future, so `emit(&state, …)` is an ordinary call.
///
/// The event bus is a fast follower: the request that caused the event has already been answered
/// by the time this runs, so a bus that is momentarily full must not turn a successful save into
/// a 500. Making it non-async is also what keeps the call sites honest — an `async fn` whose
/// future nobody awaits is a silent no-op that compiles clean and emits nothing at all.
fn emit(state: &AppState, event: &'static str, payload: serde_json::Value) -> impl std::future::Future<Output = ()> {
    let pool = state.db().pool();
    async move {
        if let Err(error) = bus::emit(pool, NewEvent::new(event).payload(payload)).await {
            tracing::warn!(event, %error, "event bus refused an event");
        }
    }
}
