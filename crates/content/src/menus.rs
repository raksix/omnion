//! Site menus (REQ-064, slice 1).
//!
//! A **menu** is a site-scoped, named tree of links with the theme locations it renders into.
//! Three decisions shape everything here, and each is a place where the obvious implementation
//! produces a menu that lies to the person editing it.
//!
//! 1. **A location holds one menu, and the conflict is refused at save time.** Not silently
//!    overwritten, not last-write-wins. Two editors assigning a menu to `header` at the same
//!    time is a real scenario, so the claim is taken inside the transaction and a refusal names
//!    the menu that already holds the location. A navigation that silently changed shape
//!    underneath two people editing it is how a site's header breaks and nobody knows why.
//!
//! 2. **Items are saved as a whole tree, and ids are the caller's.** A drag is a reorder, a drop
//!    is a reparent, and both are expressed by writing the tree the editor holds. Stable ids are
//!    what make that safe: the same item is the same row before and after, so `enabled` and
//!    `visibility` survive a reorder, and a save that changes nothing is a save that changed
//!    nothing. The alternative — a per-item PATCH — makes "reorder six rows" six requests that
//!    can half-apply.
//!
//! 3. **The depth rule is the editor's rule, refused by the store.** `MAX_DEPTH` is the same
//!    number the block tree uses and the same number the editor draws. A four-level menu is
//!    refused with a message naming the limit rather than stored and rendered as a column of
//!    columns that nobody can click.
//!
//! Items carry a `visibility` rule, and the *public* read is audience-aware: a `members` item is
//! absent for a signed-out visitor and present for a signed-in one. That filtering lives in
//! [`rendered_menu`], which is the only function the public surface calls — so a menu can never
//! be "filtered in the editor" but "unfiltered on the site".

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::validation::{validate_key, validate_text};

/// Deepest item nesting a menu may hold.
///
/// Three levels (a top row, a submenu, a sub-submenu) is the same bound the block editor applies
/// and for the same reason: past three levels a navigation is a sitemap with a hover menu bolted
/// on, and the people who use it are scrolling the page instead.
pub const MAX_DEPTH: usize = 3;

/// Most items one menu may hold, nested ones included.
pub const MAX_ITEMS: usize = 200;

/// Longest accepted menu name.
pub const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted item label.
pub const MAX_LABEL_LENGTH: usize = 120;

/// Longest accepted URL or CSS class on an item.
pub const MAX_URL_LENGTH: usize = 2_048;

/// The theme slots a menu can be assigned to.
///
/// Closed in code and repeated as a `check` constraint in `0124_cms_menus_publishing.sql`: a
/// location no renderer reads is a menu an editor assigned to a slot that never appears, and it
/// is invisible from the panel.
pub const LOCATIONS: [&str; 4] = ["header", "footer", "sidebar", "mobile"];

/// What an item links to.
pub const ITEM_TYPES: [&str; 4] = ["page", "url", "anchor", "index"];

/// Who may see an item.
pub const VISIBILITIES: [&str; 4] = ["everyone", "members", "logged_out", "roles"];

/// Who is asking. The public payload is audience-aware, and the *same* payload a visitor's
/// browser receives is the one the editor's preview renders — so a preview that disagrees with
/// the site is impossible by construction.
// Not `Copy`: the roles are a `Vec`, and a `Copy` derive here would be a lie the compiler
// catches — which is the right place to learn that a menu with roles is not a bit-copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Audience {
    /// Is the caller signed in as a site member (REQ-064 slice 4, `cms_members`)?
    pub signed_in: bool,
    /// Roles the member carries, checked only when an item asks for `roles`.
    pub roles: Vec<String>,
}

impl Audience {
    /// A signed-out visitor.
    #[must_use]
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A signed-in member with no roles.
    #[must_use]
    pub fn member(roles: Vec<String>) -> Self {
        Self {
            signed_in: true,
            roles,
        }
    }

    /// Whether an item with this visibility and role list is in the payload.
    #[must_use]
    pub fn may_see(&self, visibility: &str, roles: &[String]) -> bool {
        match visibility {
            "everyone" => true,
            "members" => self.signed_in,
            "logged_out" => !self.signed_in,
            // `roles` means "any of these", and an empty list means nobody rather than
            // everybody: a role-gated item with no roles is an item nobody may see, which is
            // almost certainly a mistake the editor wants surfaced, not a free pass.
            "roles" => {
                self.signed_in
                    && !roles.is_empty()
                    && roles.iter().any(|role| self.roles.contains(role))
            }
            _ => false,
        }
    }
}

/// A menu row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Menu {
    /// Primary key.
    pub id: Uuid,
    /// Organization the site belongs to.
    pub organization_id: Uuid,
    /// Site this menu navigates.
    pub site_id: Uuid,
    /// Stable key, unique inside the site.
    pub key: String,
    /// Name the list shows.
    pub name: String,
    /// Theme slots this menu renders into.
    pub locations: Vec<String>,
    /// Author, when a person created it.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: time::OffsetDateTime,
    /// Last change.
    pub updated_at: time::OffsetDateTime,
}

/// One link in a menu, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MenuItem {
    /// Primary key — the editor's stable handle across a reorder.
    pub id: Uuid,
    /// Menu the item belongs to.
    pub menu_id: Uuid,
    /// Parent item, or `None` for a top-level row.
    pub parent_id: Option<Uuid>,
    /// Order inside its parent.
    pub position: i32,
    /// Text of the link.
    pub label: String,
    /// `page`, `url`, `anchor` or `index`.
    pub item_type: String,
    /// Page the item points at, for `page` items.
    pub page_id: Option<Uuid>,
    /// Target URL or anchor, for `url` / `anchor` items.
    pub url: String,
    /// `_self` or `_blank`.
    pub target: String,
    /// `rel` attribute, for links that open elsewhere.
    pub rel: String,
    /// Extra class names for the theme.
    pub css_class: String,
    /// A disabled item is stored and never rendered.
    pub enabled: bool,
    /// `everyone`, `members`, `logged_out` or `roles`.
    pub visibility: String,
    /// Roles a `roles` item requires.
    pub visibility_roles: Vec<String>,
}

/// A menu item as the caller submits it.
///
/// The id is the caller's, which is the whole reason a save is a tree write: the editor holds
/// ids it minted, so reordering keeps each item's own settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMenuItem {
    /// Stable id, minted by the editor.
    pub id: Uuid,
    /// Parent item's id, or `None`.
    pub parent_id: Option<Uuid>,
    /// Order inside the parent.
    pub position: i32,
    /// Link text.
    pub label: String,
    /// Link kind.
    pub item_type: String,
    /// Page target.
    pub page_id: Option<Uuid>,
    /// URL or anchor target.
    pub url: String,
    /// Link target window.
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

impl Default for NewMenuItem {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            position: 0,
            label: String::new(),
            item_type: "url".to_owned(),
            page_id: None,
            url: String::new(),
            target: "_self".to_owned(),
            rel: String::new(),
            css_class: String::new(),
            enabled: true,
            visibility: "everyone".to_owned(),
            visibility_roles: Vec::new(),
        }
    }
}

/// A menu to create or replace.
///
/// `PUT /menus/{id}` saves the tree, so the item list is the whole edit. Key and name are not
/// part of it: renaming a menu should not be a full tree write, so those have their own
/// `MenuChanges` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuSave {
    /// The complete item tree, in any order — the store orders it by `position` per parent.
    pub items: Vec<NewMenuItem>,
    /// Locations this menu claims.
    pub locations: Vec<String>,
}

/// A menu's own fields, as an edit. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuChanges {
    /// New name.
    pub name: Option<String>,
    /// New key.
    pub key: Option<String>,
    /// Locations claimed; replaces the current set.
    pub locations: Option<Vec<String>>,
}

impl MenuChanges {
    /// `true` when the change set carries nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.key.is_none() && self.locations.is_none()
    }
}

/// The audience-filtered menu, nested, as the renderer receives it.
///
/// Filtering removes a whole *branch*: a submenu whose every child is hidden is not an empty
/// submenu the visitor can open, it is absent. A `Disclosure` pointing at nothing is a dead
/// control, and the REQ forbids dead controls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RenderedMenu {
    /// The menu's stable key, so the theme can style per menu.
    pub key: String,
    /// The menu's name.
    pub name: String,
    /// Top-level items, filtered for this audience.
    pub items: Vec<RenderedItem>,
}

/// One audience-filtered item.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RenderedItem {
    /// The item's stable id.
    pub id: Uuid,
    /// Link text.
    pub label: String,
    /// Where the link goes, already resolved: a `page` item's slug became a path, an `anchor`
    /// item's `#section` kept its hash. The theme never has to know the item types.
    pub href: String,
    /// Whether the link opens a new window.
    pub external: bool,
    /// `rel` attribute, defaulted to `noopener` for new-window links.
    pub rel: String,
    /// Extra class names.
    pub css_class: String,
    /// Visible children.
    pub children: Vec<RenderedItem>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

fn validate_location(location: &str) -> Result<String> {
    let location = location.trim().to_lowercase();
    if LOCATIONS.contains(&location.as_str()) {
        Ok(location)
    } else {
        Err(ContentError::InvalidLocation(format!(
            "location {location:?} must be one of {}",
            LOCATIONS.join(", ")
        )))
    }
}

fn validate_locations(locations: &[String]) -> Result<Vec<String>> {
    let mut seen: Vec<String> = Vec::new();
    for location in locations {
        let location = validate_location(location)?;
        if !seen.contains(&location) {
            seen.push(location);
        }
    }
    Ok(seen)
}

fn validate_visibility(visibility: &str) -> Result<String> {
    let visibility = visibility.trim().to_lowercase();
    if VISIBILITIES.contains(&visibility.as_str()) {
        Ok(visibility)
    } else {
        Err(ContentError::InvalidVisibility(format!(
            "visibility {visibility:?} must be one of {}",
            VISIBILITIES.join(", ")
        )))
    }
}

fn validate_item_type(item_type: &str) -> Result<String> {
    let item_type = item_type.trim().to_lowercase();
    if ITEM_TYPES.contains(&item_type.as_str()) {
        Ok(item_type)
    } else {
        Err(ContentError::InvalidMenuItem(format!(
            "item type {item_type:?} must be one of {}",
            ITEM_TYPES.join(", ")
        )))
    }
}

/// Check a submitted tree and return the order it must be written in.
///
/// Three things are checked here rather than in the handler, because all three are properties of
/// the *tree* and not of one row: the depth bound, that every `parent_id` names an item in this
/// submission (a parent from another menu is a cycle the store cannot see), and that no item is
/// its own ancestor.
fn check_tree(items: &[NewMenuItem]) -> Result<()> {
    if items.len() > MAX_ITEMS {
        return Err(ContentError::InvalidMenuItem(format!(
            "a menu may hold at most {MAX_ITEMS} items; this one has {}",
            items.len()
        )));
    }

    let known: Vec<Uuid> = items.iter().map(|item| item.id).collect();
    for item in items {
        if let Some(parent) = item.parent_id {
            if parent == item.id {
                return Err(ContentError::InvalidMenuItem(
                    "an item cannot be its own parent".to_owned(),
                ));
            }
            if !known.contains(&parent) {
                return Err(ContentError::InvalidMenuItem(format!(
                    "item {:?} names parent {parent}, which is not part of this menu",
                    item.label
                )));
            }
        }
    }

    // Depth is resolved per item with a memo table, and the bound is checked on the *final*
    // value — not inside the walk. Checking inside the walk looks equivalent and is not: an item
    // whose parent is already in the memo leaves the walk through the reuse branch, so the check
    // after the walk is the only one every item passes, and the branch that skips it is exactly
    // the branch a four-level tree takes at its deepest level.
    let mut depths: Vec<(Uuid, usize)> = Vec::new();
    for item in items {
        // `chain` is this item plus the ancestors that are not in the memo yet; the walk either
        // reaches a memoized ancestor (whose depth plus the chain length is the answer) or a
        // top-level item (whose depth is the chain's own length).
        let mut chain: Vec<Uuid> = Vec::new();
        let mut current = item.id;
        let depth = loop {
            if let Some((_, memoized)) = depths.iter().find(|(id, _)| *id == current) {
                break memoized + chain.len();
            }
            chain.push(current);
            match items
                .iter()
                .find(|candidate| candidate.id == current)
                .and_then(|candidate| candidate.parent_id)
            {
                Some(parent) => current = parent,
                None => break chain.len(),
            }
        };
        if depth > MAX_DEPTH {
            return Err(ContentError::TooDeep(format!(
                "menu items nest at most {MAX_DEPTH} levels deep; {label:?} reaches {depth}",
                label = item.label
            )));
        }
        depths.push((item.id, depth));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------------------------

const MENU_COLUMNS: &str =
    "id, organization_id, site_id, key, name, locations, created_by, created_at, updated_at";

const ITEM_COLUMNS: &str = "id, menu_id, parent_id, position, label, item_type, page_id, url, \
     target, rel, css_class, enabled, visibility, visibility_roles";

/// A site's menus, name order.
pub async fn list_menus(pool: &PgPool, site_id: Uuid) -> Result<Vec<Menu>> {
    let sql = format!(
        "select {MENU_COLUMNS} from cms_menus where site_id = $1 order by name asc, key asc"
    );
    sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// One menu of a site, with its items in tree order.
pub async fn find_menu(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<Option<(Menu, Vec<MenuItem>)>> {
    let sql = format!("select {MENU_COLUMNS} from cms_menus where site_id = $1 and id = $2");
    let menu = sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(menu) = menu else {
        return Ok(None);
    };
    let items = list_items(pool, menu.id).await?;
    Ok(Some((menu, items)))
}

/// A menu of a site by key — the theme addresses a location, the editor addresses a key.
pub async fn find_menu_by_key(
    pool: &PgPool,
    site_id: Uuid,
    key: &str,
) -> Result<Option<Menu>> {
    let key = validate_key(key, "menu key")?;
    let sql = format!("select {MENU_COLUMNS} from cms_menus where site_id = $1 and key = $2");
    sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(&key)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// A menu's items, parents before children and ordered inside each parent.
///
/// The ordering is what makes "position 3" mean the same thing on reload as it did on the drag —
/// the store does not return a set and leave the editor to sort it, because a client that sorts
/// client-side and a client that does not produce two different menus.
pub async fn list_items(pool: &PgPool, menu_id: Uuid) -> Result<Vec<MenuItem>> {
    let sql = format!(
        "select {ITEM_COLUMNS} from cms_menu_items \
         where menu_id = $1 order by position asc, created_at asc, id asc"
    );
    sqlx::query_as::<_, MenuItem>(&sql)
        .bind(menu_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Create a menu.
pub async fn create_menu(
    pool: &PgPool,
    organization_id: Uuid,
    site_id: Uuid,
    key: &str,
    name: &str,
    created_by: Option<Uuid>,
) -> Result<Menu> {
    let key = validate_key(key, "menu key")?;
    let name = validate_text(name, MAX_NAME_LENGTH, "name")?;
    let sql = format!(
        "insert into cms_menus (organization_id, site_id, key, name, created_by) \
         values ($1, $2, $3, $4, $5) returning {MENU_COLUMNS}"
    );
    sqlx::query_as::<_, Menu>(&sql)
        .bind(organization_id)
        .bind(site_id)
        .bind(&key)
        .bind(&name)
        .bind(created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| map_menu_write_error(error, &key))
}

/// Apply a menu's own field changes. Locations are claimed transactionally against the other
/// menus of the same site, so two editors cannot both believe they hold `header`.
pub async fn update_menu(
    pool: &PgPool,
    site_id: Uuid,
    id: Uuid,
    changes: &MenuChanges,
) -> Result<Menu> {
    let name = match changes.name.as_deref() {
        Some(name) => Some(validate_text(name, MAX_NAME_LENGTH, "name")?),
        None => None,
    };
    let key = match changes.key.as_deref() {
        Some(key) => Some(validate_key(key, "menu key")?),
        None => None,
    };
    let locations = match &changes.locations {
        Some(locations) => Some(validate_locations(locations)?),
        None => None,
    };

    if changes.is_empty() {
        return read_menu(pool, site_id, id).await?.ok_or(ContentError::MenuNotFound);
    }

    let mut tx = pool.begin().await?;
    if let Some(locations) = &locations {
        claim_locations(&mut tx, site_id, id, locations).await?;
    }
    // The error mapper names the key that collided, so it is read after the binds rather than
    // moved by them — `key` is a `String` and the bind takes it by value.
    let conflicting_key = key.clone().unwrap_or_default();
    let sql = format!(
        "update cms_menus set \
           name = coalesce($2, name), \
           key = coalesce($3, key), \
           locations = coalesce($4, locations), \
           updated_at = now() \
         where site_id = $1 and id = $5 \
         returning {MENU_COLUMNS}"
    );
    let updated = sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(name)
        .bind(key)
        .bind(locations)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| map_menu_write_error(error, &conflicting_key))?
        .ok_or(ContentError::MenuNotFound)?;
    tx.commit().await?;
    Ok(updated)
}

/// Replace a menu's whole item tree and its locations in one transaction.
///
/// The write is a delete-and-reinsert of the rows the caller submitted, which is deliberately
/// blunt: a diffing writer would have to reason about which items the caller removed, and a
/// caller that removed an item by forgetting it would silently keep it. The tree the editor
/// holds is the tree that gets written, ids included.
pub async fn save_menu(
    pool: &PgPool,
    site_id: Uuid,
    id: Uuid,
    save: &MenuSave,
) -> Result<(Menu, Vec<MenuItem>)> {
    let locations = validate_locations(&save.locations)?;
    let mut items = save.items.clone();
    for item in &mut items {
        item.id = if item.id.is_nil() { Uuid::new_v4() } else { item.id };
        item.item_type = validate_item_type(&item.item_type)?;
        item.visibility = validate_visibility(&item.visibility)?;
        item.label = validate_text(&item.label, MAX_LABEL_LENGTH, "item label")?;
        if item.target != "_blank" {
            item.target = "_self".to_owned();
        }
        if item.url.len() > MAX_URL_LENGTH {
            return Err(ContentError::InvalidMenuItem(format!(
                "a menu item URL stays under {MAX_URL_LENGTH} characters"
            )));
        }
        // A `page` item without a page (or a `url` item with a stray one) would render a link
        // that goes nowhere; refuse it here rather than at the theme, where the symptom is a
        // dead navigation and the cause is a stored row.
        match item.item_type.as_str() {
            "page" if item.page_id.is_none() => {
                return Err(ContentError::InvalidMenuItem(format!(
                    "item {:?} is a page link with no page selected",
                    item.label
                )));
            }
            "url" | "anchor" if item.url.trim().is_empty() => {
                return Err(ContentError::InvalidMenuItem(format!(
                    "item {:?} is a link with no URL",
                    item.label
                )));
            }
            _ => {}
        }
    }
    check_tree(&items)?;

    let mut tx = pool.begin().await?;
    claim_locations(&mut tx, site_id, id, &locations).await?;

    // One delete-and-reinsert, not a per-item upsert: a parent that changed means a foreign key
    // would point at a row this transaction is about to remove, so the rows go in parent-first
    // order after a single `delete`.
    sqlx::query("delete from cms_menu_items where menu_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    let mut ordered = items.clone();
    ordered.sort_by_key(|item| depth_of(&items, item.id));
    for (index, item) in ordered.iter().enumerate() {
        // `created_at` is stamped with an explicit per-row offset rather than left to `now()`.
        // `now()` in PostgreSQL is the *transaction* timestamp, so every row written by one save
        // shares it to the microsecond — the ordering column existed and did nothing, and two
        // items saved with the same `position` came back in UUID order, which is random. The
        // offset is what makes "the order the editor built the tree in" survive the round trip;
        // it is monotonic in the write order and nobody reads it.
        let sql = format!(
            "insert into cms_menu_items \
             (id, menu_id, parent_id, position, label, item_type, page_id, url, target, rel, \
              css_class, enabled, visibility, visibility_roles, created_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                     now() + make_interval(secs => $15::double precision / 1000000.0)) \
             returning {ITEM_COLUMNS}"
        );
        // `fetch_one` rather than `execute`: the store hands back the *stored* rows, so a
        // database default (a timestamp, a normalised label) is what the editor receives rather
        // than what it happened to send.
        let _: (Uuid,) = sqlx::query_as(&sql)
            .bind(item.id)
            .bind(id)
            .bind(item.parent_id)
            .bind(item.position)
            .bind(&item.label)
            .bind(&item.item_type)
            .bind(item.page_id)
            .bind(item.url.trim())
            .bind(&item.target)
            .bind(&item.rel)
            .bind(&item.css_class)
            .bind(item.enabled)
            .bind(&item.visibility)
            .bind(&item.visibility_roles)
            .bind(index as f64)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| map_menu_write_error(error, "item"))?;
    }

    let sql = format!(
        "update cms_menus set locations = $2, updated_at = now() \
         where site_id = $1 and id = $3 returning {MENU_COLUMNS}"
    );
    let menu = sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(&locations)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ContentError::MenuNotFound)?;
    tx.commit().await?;
    // Re-read rather than return the caller's own structs: the answer is what the database
    // holds, which is what the next `GET` will report. Returning the submission would let a
    // save and a reload disagree about the very tree that was just written.
    let items = list_items(pool, id).await?;
    Ok((menu, items))
}

fn depth_of(items: &[NewMenuItem], id: Uuid) -> usize {
    let mut depth = 1;
    let mut current = id;
    while let Some(parent) = items
        .iter()
        .find(|item| item.id == current)
        .and_then(|item| item.parent_id)
    {
        depth += 1;
        current = parent;
        if depth > items.len() + 2 {
            break;
        }
    }
    depth
}

/// Claim every location of this menu, refusing one another menu already holds.
async fn claim_locations(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    site_id: Uuid,
    menu_id: Uuid,
    locations: &[String],
) -> Result<()> {
    if locations.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query_as::<_, (String, Uuid, String)>(
        "select unnest(locations) as location, id, key from cms_menus \
         where site_id = $1 and id <> $2 and locations && $3::text[]",
    )
    .bind(site_id)
    .bind(menu_id)
    .bind(locations)
    .fetch_all(&mut **tx)
    .await?;
    if let Some((location, holder, holder_key)) = rows.first() {
        return Err(ContentError::MenuLocationTaken {
            location: location.clone(),
            holder: *holder,
            holder_key: holder_key.clone(),
        });
    }
    Ok(())
}

/// Read a menu row without its items.
pub async fn read_menu(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<Option<Menu>> {
    let sql = format!("select {MENU_COLUMNS} from cms_menus where site_id = $1 and id = $2");
    sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Remove a menu and its items. `false` when the site has no such menu.
pub async fn delete_menu(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from cms_menus where site_id = $1 and id = $2")
        .bind(site_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// The menu a theme renders for a location, filtered for this audience.
///
/// Two failures are prevented by the shape of this function rather than by a check inside it:
/// a disabled item is absent (not `display: none` in a theme), and a submenu with no visible
/// children is absent with its parent, because a disclosure pointing at nothing is a dead
/// control.
pub async fn rendered_menu(
    pool: &PgPool,
    site_id: Uuid,
    location: &str,
    audience: Audience,
) -> Result<Option<RenderedMenu>> {
    let location = validate_location(location)?;
    let Some(menu) = find_menu_by_location(pool, site_id, &location).await? else {
        return Ok(None);
    };
    let items = list_items(pool, menu.id).await?;
    let slugs = page_slugs(pool, &items).await?;

    // The *unfiltered* parent set, taken before the audience filter runs. This ordering is the
    // whole bug: measured after the filter, a branch whose only child is members-gated has no
    // parent rows left in `visible`, so it looks like a leaf and survives as an empty disclosure
    // — which is exactly the case the rule exists for. "Had children" is a fact about the tree,
    // and the tree is the tree before anybody's audience is applied to it.
    let ever_parents: Vec<Uuid> = items.iter().filter_map(|item| item.parent_id).collect();

    let mut visible: Vec<MenuItem> = items
        .into_iter()
        .filter(|item| item.enabled && audience.may_see(&item.visibility, &item.visibility_roles))
        .collect();

    // A submenu whose every child is hidden is absent, not an empty disclosure.
    //
    // Two things this gets wrong when written the obvious way, and both were:
    //
    // * **"has no visible child" also matches every leaf.** A leaf is a normal item. The rule is
    //   "had children and has none left", so the original test pruned every leaf in the menu and
    //   left a member's payload with no submenu at all.
    // * **One pass is not enough.** Whether a branch is empty depends on its children, and
    //   whether a child survives depends on *its* children, so a three-level branch needs its
    //   deepest leaf removed before its middle level can be seen as empty. Sorting by depth
    //   looks like the fix and is not: a leaf and its parent come out of an unstable sort in
    //   either order. So the pass repeats until the count settles, and `MAX_DEPTH` bounds it at
    //   three iterations for any legal tree.
    //
    // The parent sets are materialised per pass because `retain` holds the vector immutably
    // while walking it — a closure cannot also read the list it is filtering.
    loop {
        let parents_now: Vec<Uuid> = visible.iter().filter_map(|item| item.parent_id).collect();
        let before = visible.len();
        visible.retain(|item| {
            item.parent_id.is_none()
                || !ever_parents.contains(&item.id)
                || parents_now.contains(&item.id)
        });
        if visible.len() == before {
            break;
        }
    }

    let rendered: Vec<RenderedItem> = visible
        .iter()
        .filter(|item| item.parent_id.is_none())
        .filter_map(|item| render_item(item, &visible, &slugs, &audience))
        .collect();

    if rendered.is_empty() {
        // A menu that exists but renders to nothing is not a navigation — the theme would draw
        // an empty bar. Answering `None` lets the theme draw nothing at all, which is honest.
        return Ok(None);
    }

    Ok(Some(RenderedMenu {
        key: menu.key,
        name: menu.name,
        items: rendered,
    }))
}

/// The menu that holds a location, if any.
pub async fn find_menu_by_location(
    pool: &PgPool,
    site_id: Uuid,
    location: &str,
) -> Result<Option<Menu>> {
    let sql = format!(
        "select {MENU_COLUMNS} from cms_menus where site_id = $1 and $2 = any(locations) \
         order by name asc limit 1"
    );
    sqlx::query_as::<_, Menu>(&sql)
        .bind(site_id)
        .bind(location)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Render one item and its visible descendants.
///
/// The audience is **borrowed**, not moved: the recursion is inside a closure, and an owned
/// `Audience` in a `FnMut` would be moved into the first call and refused at compile time — which
/// is the right answer, because a render pass has one audience for the whole menu, not one per
/// item.
fn render_item(
    item: &MenuItem,
    visible: &[MenuItem],
    slugs: &[(Uuid, String)],
    audience: &Audience,
) -> Option<RenderedItem> {
    let children: Vec<RenderedItem> = visible
        .iter()
        .filter(|child| child.parent_id == Some(item.id))
        .filter_map(|child| render_item(child, visible, slugs, audience))
        .collect();

    let href = match item.item_type.as_str() {
        "page" => {
            let page = item.page_id?;
            let slug = slugs.iter().find(|(id, _)| *id == page)?.1.clone();
            format!("/{slug}")
        }
        "anchor" | "url" => item.url.clone(),
        // The site index: the home path, which is the root rather than a page's slug.
        "index" => "/".to_owned(),
        _ => return None,
    };

    let external = item.target == "_blank";
    let rel = if external && item.rel.trim().is_empty() {
        "noopener".to_owned()
    } else {
        item.rel.clone()
    };

    Some(RenderedItem {
        id: item.id,
        label: item.label.clone(),
        href,
        external,
        rel,
        css_class: item.css_class.clone(),
        children,
    })
}

/// Slugs of the pages the items point at, in one query.
///
/// Resolving a page link must not become a query per item: a menu of twenty items is the site's
/// header on every page load, and twenty round trips is a header that arrives late.
async fn page_slugs(pool: &PgPool, items: &[MenuItem]) -> Result<Vec<(Uuid, String)>> {
    let ids: Vec<Uuid> = items.iter().filter_map(|item| item.page_id).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, String)>("select id, slug from pages where id = any($1)")
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn map_menu_write_error(error: sqlx::Error, key: &str) -> ContentError {
    let message = error.to_string();
    if let Some(constraint) = message
        .split("constraint ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
    {
        return match constraint {
            "cms_menus_site_key_key" => {
                ContentError::MenuKeyTaken(key.to_owned())
            }
            "cms_menu_items_menu_page_key" => ContentError::InvalidMenuItem(
                "this menu already links to one of those pages".to_owned(),
            ),
            "cms_menu_items_visibility_check" => ContentError::InvalidVisibility(format!(
                "visibility must be one of {}",
                VISIBILITIES.join(", ")
            )),
            "cms_menu_items_type_check" => ContentError::InvalidMenuItem(format!(
                "item type must be one of {}",
                ITEM_TYPES.join(", ")
            )),
            other => ContentError::Database(sqlx::Error::Protocol(
                format!("menu write refused by {other}").into(),
            )),
        };
    }
    ContentError::Database(error)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn item(parent: Option<Uuid>, label: &str) -> NewMenuItem {
        NewMenuItem {
            id: Uuid::new_v4(),
            parent_id: parent,
            position: 0,
            label: label.to_owned(),
            item_type: "url".to_owned(),
            url: format!("/{label}"),
            ..NewMenuItem::default()
        }
    }

    #[test]
    fn a_signed_out_visitor_sees_only_what_is_for_everyone() {
        let audience = Audience::anonymous();
        assert!(audience.may_see("everyone", &[]));
        assert!(!audience.may_see("members", &[]));
        assert!(audience.may_see("logged_out", &[]));
        assert!(!audience.may_see("roles", &["editor".to_owned()]));
    }

    #[test]
    fn a_member_sees_members_items_and_matching_roles_only() {
        let audience = Audience::member(vec!["editor".to_owned()]);
        assert!(audience.may_see("members", &[]));
        assert!(!audience.may_see("logged_out", &[]));
        assert!(audience.may_see("roles", &["editor".to_owned()]));
        assert!(!audience.may_see("roles", &["admin".to_owned()]));
    }

    #[test]
    fn a_role_gated_item_with_no_roles_is_visible_to_nobody() {
        // The dangerous reading of an empty list is "anybody with an account"; this is the test
        // that says it is not.
        let audience = Audience::member(vec!["editor".to_owned()]);
        assert!(!audience.may_see("roles", &[]));
        assert!(!Audience::anonymous().may_see("roles", &[]));
    }

    #[test]
    fn three_levels_are_accepted_and_four_are_refused() {
        let root = item(None, "root");
        let child = item(Some(root.id), "child");
        let grandchild = item(Some(child.id), "grandchild");
        assert!(check_tree(&[root.clone(), child.clone(), grandchild.clone()]).is_ok());

        let too_deep = item(Some(grandchild.id), "great-grandchild");
        let error = check_tree(&[root, child, grandchild, too_deep]).unwrap_err();
        assert_eq!(error.code(), "menu_too_deep");
    }

    #[test]
    fn a_parent_from_another_menu_is_refused() {
        let stranger = Uuid::new_v4();
        let orphan = item(Some(stranger), "orphan");
        let error = check_tree(&[orphan]).unwrap_err();
        assert_eq!(error.code(), "invalid_menu_item");
    }

    #[test]
    fn an_item_cannot_be_its_own_parent() {
        let mut node = item(None, "loop");
        node.parent_id = Some(node.id);
        assert_eq!(
            check_tree(&[node]).unwrap_err().code(),
            "invalid_menu_item"
        );
    }

    #[test]
    fn locations_are_normalized_deduplicated_and_closed() {
        let claimed = validate_locations(&[
            "Header".to_owned(),
            "header".to_owned(),
            "footer".to_owned(),
        ])
        .unwrap();
        assert_eq!(claimed, vec!["header".to_owned(), "footer".to_owned()]);
        assert_eq!(
            validate_locations(&["sidebar-nav".to_owned()]).unwrap_err().code(),
            "invalid_location"
        );
    }

    #[test]
    fn the_migration_agrees_with_the_vocabulary() {
        // SQL cannot import a Rust constant, so the closed lists are written twice. A location
        // the editor offers but the database refuses is a menu that cannot be saved; one the
        // database accepts but the editor cannot name is a menu in a slot no theme reads. The
        // file is read from the repository rather than pasted here, so this assertion cannot
        // itself drift out of date.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../database/migrations/0124_cms_menus_publishing.sql"
        );
        let sql = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("cannot read 0124_cms_menus_publishing.sql ({error}); the closed lists are duplicated in it")
        });

        for value in LOCATIONS {
            assert!(
                sql.contains(&format!("'{value}'")),
                "the migration does not know the {value} location"
            );
        }
        for value in ITEM_TYPES {
            assert!(
                sql.contains(&format!("'{value}'")),
                "the migration does not know the {value} item type"
            );
        }
        for value in VISIBILITIES {
            assert!(
                sql.contains(&format!("'{value}'")),
                "the migration does not know the {value} visibility"
            );
        }
    }
}
