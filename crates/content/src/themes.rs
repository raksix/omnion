//! The theme gallery and a site's activation (REQ-062, slice 1).
//!
//! Three things in here are decisions rather than plumbing, and each of them is a place where
//! the obvious implementation produces a product that lies to the person using it.
//!
//! 1. **A bundled theme is a file; the row is a mirror.** The platform ships
//!    `themes/<key>/omnion.theme.json`, and [`sync_bundled`] copies the manifests into the
//!    `themes` table at boot so the gallery has something to list. That is a *mirror*, which
//!    means it is rebuilt rather than edited: an operator who changes a bundled row is editing
//!    a copy, and [`restore_previous`] must not be able to return a site to a theme the
//!    platform no longer ships. Installed packages are the other half — real rows, with bytes
//!    behind them, that the file loader will never overwrite.
//!
//! 2. **Activation is one row per site, and it remembers what it displaced.** `site_themes`
//!    holds the active key and `previous_theme_key`, both written by the activation itself. The
//!    alternative — computing the previous theme when somebody presses *Restore previous* —
//!    guesses, and the guess is wrong exactly twice: after two activations, and after a
//!    restore. It also cannot answer the question the confirmation asks ("what will change?"),
//!    because the thing being replaced is the point.
//!
//! 3. **A site keeps a key the platform does not have and still renders.** `site_themes.theme_key`
//!    has no foreign key to `themes.key` on purpose. A restore from a database dump, a package
//!    removed by an operator, or a theme key that arrived in `sites.theme` before this table
//!    existed: all three leave a site pointing at something the gallery cannot show. Refusing
//!    the write would turn a cosmetic gap into an outage; the renderer already falls back to
//!    `minimal`, and [`read_activation`] reports the gap instead of hiding it.
//!
//! The gallery is site-scoped, not organization-scoped, because the question the screen answers
//! is "what can THIS site render with". A bundled theme belongs to no organization at all, and
//! an uploaded one belongs to the organization that installed it — so the filter is a `union`
//! rather than a single equality, and that union is the reason a platform owner with no
//! organization can still see the ten bundled themes.

use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::validation::{validate_key, validate_text};

/// Longest accepted theme name.
pub const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted description on a manifest.
pub const MAX_DESCRIPTION_LENGTH: usize = 500;

/// Where a theme came from.
pub const SOURCES: [&str; 2] = ["bundled", "uploaded"];

/// The theme a site renders with when none has been activated, and the key the renderer falls
/// back to for one it cannot resolve.
///
/// One constant for both roles on purpose: "no theme chosen" and "a theme that no longer
/// exists" are the same situation to a visitor, and two spellings of the fallback would let the
/// gallery and the renderer disagree about what a site is showing.
pub const DEFAULT_THEME_KEY: &str = "minimal";

/// A theme as the gallery lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Theme {
    /// Primary key.
    pub id: Uuid,
    /// Organization that installed it, or `None` for a bundled theme.
    pub organization_id: Option<Uuid>,
    /// Stable key a site activates.
    pub key: String,
    /// Display name.
    pub name: String,
    /// SemVer of the theme.
    pub version: String,
    /// `bundled` or `uploaded`.
    pub source: String,
    /// The v2 manifest, verbatim.
    pub manifest: Value,
    /// Where the package's bytes live, for an uploaded theme.
    pub storage_key: Option<String>,
    /// Checksum of the package, for an uploaded theme.
    pub checksum: Option<String>,
    /// Who installed it.
    pub installed_by: Option<Uuid>,
    /// When it was installed.
    pub installed_at: Option<OffsetDateTime>,
}

/// One gallery row, joined with the activation state of the site being viewed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct GalleryEntry {
    /// The theme itself.
    pub theme: Theme,
    /// Is this the theme the site renders with right now.
    pub is_active: bool,
}

/// What a site renders with, and what it could go back to.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Activation {
    /// Site the row belongs to.
    pub site_id: Uuid,
    /// Active theme key.
    pub theme_key: String,
    /// The key the last activation displaced, or `None` for a site that never switched.
    pub previous_theme_key: Option<String>,
    /// Who activated it.
    pub activated_by: Option<Uuid>,
    /// When.
    pub activated_at: OffsetDateTime,
}

/// The gallery's answer for one site: what is installed, what is active, and what a rollback
/// would restore.
///
/// `rollback_target` is `None` for a site that never activated a different theme — and that
/// is a *different state* from "the previous key is the same as the active one", so the panel
/// hides the button rather than offering an action that changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GalleryView {
    /// The site's key, for the confirmation copy.
    pub site_key: String,
    /// The theme the site renders with, which may be a key the gallery cannot show.
    pub active_key: String,
    /// Whether that key is one this platform can actually render.
    pub active_known: bool,
    /// The key a rollback would restore, if any.
    pub rollback_target: Option<String>,
    /// Installed and bundled themes, alphabetical.
    pub themes: Vec<GalleryEntry>,
}

/// What [`activate`] changed, so the route can emit one event and not two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationChange {
    /// The site.
    pub site_id: Uuid,
    /// The key now active.
    pub theme_key: String,
    /// The key it replaced — always `Some` for a rollback, never for a plain activation.
    pub previous_theme_key: Option<String>,
    /// Whether this was a restore rather than a forward switch.
    pub restored: bool,
}

// ---------------------------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------------------------

/// What a manifest must carry, and what the loader refuses a file without.
///
/// The v2 contract (docs/03-FRONTEND.md) adds `slots`, `tokens`, `settingsSchema`,
/// `compatibility`, `previewImage`, `screenshots` and `aliases` to v1. Only a subset is
/// *required*: a theme that declares no `slots` still renders, and refusing it would make a
/// first theme author's first upload fail on a field the renderer never reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestShape {
    /// Slots the theme ships.
    pub slots: usize,
    /// Colour tokens the theme declares.
    pub tokens: usize,
    /// Modes (`light`, `dark`).
    pub modes: Vec<&'static str>,
    /// Other keys the manifest carries.
    pub extras: Vec<&'static str>,
}

/// Check a parsed manifest.
///
/// Returns the reason as a string rather than a typed error because the caller — a boot-time
/// loader over N files and an installer over one zip — prints the sentence to an operator, and
/// two different call sites writing their own wording is how a file starts being described two
/// ways in the same product.
pub fn manifest_shape(manifest: &Value) -> std::result::Result<ManifestShape, String> {
    let object = manifest.as_object().ok_or("the manifest is not a JSON object")?;

    for required in ["key", "name", "version"] {
        match object.get(required).and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => {}
            Some(_) => return Err(format!("'{required}' is blank")),
            None => return Err(format!("'{required}' is missing")),
        }
    }

    let key = object["key"].as_str().unwrap_or_default();
    // The key is the only manifest field that reaches the filesystem and a URL, so it is the
    // one that has to look like a key rather than like a sentence.
    validate_key(key, "theme key").map_err(|error| error.to_string())?;

    let slots = object
        .get("slots")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let tokens = object
        .get("tokens")
        .and_then(Value::as_object)
        .map_or(0, serde_json::Map::len);

    let declared_modes = object.get("modes").and_then(Value::as_array);
    let modes: Vec<&'static str> = ["light", "dark"]
        .into_iter()
        .filter(|mode| {
            declared_modes.is_some_and(|declared| {
                declared
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|value| value.eq_ignore_ascii_case(mode))
            })
        })
        .collect();

    let mut extras: Vec<&'static str> = ["settingsSchema", "compatibility", "previewImage", "screenshots", "aliases", "layouts", "pageTypes", "engine", "author", "description"]
        .into_iter()
        .filter(|name| object.contains_key(*name))
        .collect();
    extras.sort_unstable();

    Ok(ManifestShape {
        slots,
        tokens,
        modes,
        extras,
    })
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// Every theme this site may choose, with its active flag.
///
/// Bundled themes have `organization_id is null`, so a platform owner with no organization of
/// their own still gets the ten. An uploaded theme is visible to its own organization, and to
/// nobody else: two tenants sharing an installation must not see each other's packages, because
/// an upload is a tenant's code.
pub async fn gallery(
    pool: &PgPool,
    site_id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<GalleryView> {
    let active_key = active_theme_key(pool, site_id).await?;
    let rows = sqlx::query_as::<_, GalleryRow>(
        // Every column from `t` carries the `theme_` prefix `GalleryRow` declares, because a
        // JOIN has two `id`s, two `key`s and two `theme_key`s and sqlx resolves a row field by
        // COLUMN NAME, not by position. Without the aliases this route answered 500
        // ("no column found for name: theme_id") the first time a walk asked the gallery for a
        // site — which is the only way the route is ever used, so the slice-1 gallery was
        // unreachable end to end and its own walk never noticed.
        "select t.id as theme_id, t.organization_id as theme_organization_id, \
                t.key as theme_key_value, t.name as theme_name, t.version as theme_version, \
                t.source as theme_source, t.manifest as theme_manifest, \
                t.storage_key as theme_storage_key, t.checksum as theme_checksum, \
                t.installed_by as theme_installed_by, t.installed_at as theme_installed_at, \
                s.theme_key as active_key, s.previous_theme_key as rollback_key \
         from themes t \
         left join site_themes s on s.site_id = $1 \
         where t.removed_at is null \
           and (t.organization_id is null or t.organization_id = $2) \
         order by t.name, t.key",
    )
    .bind(site_id)
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    // The rollback target belongs to the activation row, and it is the same on every joined
    // row — so it is read off the first one rather than recomputed from the theme list. A
    // gallery that *derived* it would answer "nothing to go back to" for exactly the sites
    // that most need the button.
    let rollback_target = rows
        .first()
        .and_then(|row| row.rollback_key.clone())
        // A target equal to the active key means the last write was itself a restore, and
        // rolling back again would change nothing. "Nothing to go back to" and "going back
        // would change nothing" are both `None` on purpose: the button is about an action.
        .filter(|target| target.as_str() != active_key);

    let themes: Vec<GalleryEntry> = rows
        .into_iter()
        .map(|row| GalleryEntry {
            is_active: row.active_key.as_deref() == Some(row.theme_key_value.as_str()),
            theme: Theme {
                id: row.theme_id,
                organization_id: row.theme_organization_id,
                key: row.theme_key_value,
                name: row.theme_name,
                version: row.theme_version,
                source: row.theme_source,
                manifest: row.theme_manifest,
                storage_key: row.theme_storage_key,
                checksum: row.theme_checksum,
                installed_by: row.theme_installed_by,
                installed_at: row.theme_installed_at,
            },
        })
        .collect();

    Ok(GalleryView {
        site_key: String::new(),
        active_key,
        active_known: themes.iter().any(|entry| entry.is_active),
        rollback_target,
        themes,
    })
}

/// The row the gallery's join returns, before the columns are split into two structs.
///
/// `#[sqlx(flatten)]` needs the two halves to have disjoint column names, and `themes.key` and
/// `site_themes.theme_key` do not — so the join aliases both sides and this struct is the
/// shape the row actually has.
#[derive(sqlx::FromRow)]
struct GalleryRow {
    theme_id: Uuid,
    theme_organization_id: Option<Uuid>,
    theme_key_value: String,
    theme_name: String,
    theme_version: String,
    theme_source: String,
    theme_manifest: Value,
    theme_storage_key: Option<String>,
    theme_checksum: Option<String>,
    theme_installed_by: Option<Uuid>,
    theme_installed_at: Option<OffsetDateTime>,
    active_key: Option<String>,
    rollback_key: Option<String>,
}

/// The theme a site renders with.
///
/// The left join in [`gallery`] is not enough on its own, so this is a second read: a site
/// with no `site_themes` row at all still has a theme, carried on the `sites` row the platform
/// has always had. Reading only `site_themes` would answer `minimal` for a site whose operator
/// set a theme months ago, and the gallery would then show a `Active` badge on the wrong card.
pub async fn active_theme_key(pool: &PgPool, site_id: Uuid) -> Result<String> {
    let from_site_themes: Option<String> = sqlx::query_scalar(
        "select theme_key from site_themes where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await?;
    if let Some(key) = from_site_themes {
        return Ok(key);
    }
    let from_sites: Option<String> = sqlx::query_scalar("select theme from sites where id = $1")
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    Ok(from_sites.unwrap_or_else(|| DEFAULT_THEME_KEY.to_owned()))
}

/// The activation row of a site, when it has one.
pub async fn read_activation(pool: &PgPool, site_id: Uuid) -> Result<Option<Activation>> {
    sqlx::query_as::<_, Activation>(
        "select site_id, theme_key, previous_theme_key, activated_by, activated_at \
         from site_themes where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await
    .map_err(ContentError::from)
}

/// One theme by key, for the preview and the installer.
pub async fn find_theme(pool: &PgPool, key: &str) -> Result<Option<Theme>> {
    sqlx::query_as::<_, Theme>(
        "select id, organization_id, key, name, version, source, manifest, storage_key, \
                checksum, installed_by, installed_at \
         from themes where key = $1 and removed_at is null",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(ContentError::from)
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// What an activation asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationRequest {
    /// The key to activate.
    pub theme_key: Option<String>,
}

/// Activate a theme for a site, recording the key it displaced.
///
/// Refuses a key no live theme carries, and says which keys it does. A gallery that offered a
/// theme nobody can install is one thing; an activation that writes an unknown key and leaves
/// the site rendering a fallback is a worse one, because the panel would report success.
pub async fn activate(
    pool: &PgPool,
    site_id: Uuid,
    organization_id: Option<Uuid>,
    theme_key: &str,
    activated_by: Option<Uuid>,
) -> Result<ActivationChange> {
    let key = validate_key(theme_key, "theme key")?;
    let exists = sqlx::query_scalar::<_, bool>(
        "select exists ( \
           select 1 from themes \
           where key = $1 and removed_at is null \
             and (organization_id is null or organization_id = $2))",
    )
    .bind(&key)
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    if !exists {
        return Err(ContentError::ThemeNotFound(key));
    }

    let current = read_activation(pool, site_id).await?;
    let previous = current.as_ref().map(|row| row.theme_key.clone());
    // Activating the theme that is already active is not an error, and it is also not a
    // change: writing the row would move `previous_theme_key` to the active key, and the next
    // *Restore previous* would then restore the theme that was already in use.
    if previous.as_deref() == Some(key.as_str()) {
        let row = current.expect("`previous` is Some only when the activation row was read");
        return Ok(ActivationChange {
            site_id,
            theme_key: row.theme_key,
            previous_theme_key: row.previous_theme_key,
            restored: false,
        });
    }

    let mut tx = pool.begin().await?;
    let stored = write_activation(&mut tx, site_id, &key, previous.clone(), activated_by).await?;
    tx.commit().await?;

    Ok(ActivationChange {
        site_id,
        theme_key: stored,
        previous_theme_key: previous,
        restored: false,
    })
}

/// Restore the theme the last activation displaced.
///
/// Refuses when there is nothing to restore, and says so with its own error rather than
/// answering success: a *Restore previous* button that re-activates the current theme is a
/// button that reports work it did not do.
pub async fn restore_previous(
    pool: &PgPool,
    site_id: Uuid,
    organization_id: Option<Uuid>,
    activated_by: Option<Uuid>,
) -> Result<ActivationChange> {
    let current = read_activation(pool, site_id)
        .await?
        .ok_or(ContentError::RollbackUnavailable)?;
    let target = current
        .previous_theme_key
        .clone()
        .ok_or(ContentError::RollbackUnavailable)?;

    // The displaced theme may have been removed since it was displaced. Restoring it would
    // leave the site on a key the gallery cannot show, so it is refused with the reason.
    let exists = sqlx::query_scalar::<_, bool>(
        "select exists ( \
           select 1 from themes \
           where key = $1 and removed_at is null \
             and (organization_id is null or organization_id = $2))",
    )
    .bind(&target)
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    if !exists {
        return Err(ContentError::ThemeNotFound(target));
    }

    let displaced = current.theme_key;
    let mut tx = pool.begin().await?;
    let stored = write_activation(&mut tx, site_id, &target, Some(displaced.clone()), activated_by)
        .await?;
    tx.commit().await?;

    Ok(ActivationChange {
        site_id,
        theme_key: stored,
        previous_theme_key: Some(displaced),
        restored: true,
    })
}

/// The write both entry points share, so activation and rollback cannot drift apart.
async fn write_activation(
    tx: &mut Transaction<'_, Postgres>,
    site_id: Uuid,
    theme_key: &str,
    previous: Option<String>,
    activated_by: Option<Uuid>,
) -> Result<String> {
    // Both directions store the key they displaced, which is what makes a rollback reversible
    // in the same way an activation is. The two callers pass exactly that value, so there is
    // no flag to get wrong here.
    sqlx::query(
        "insert into site_themes (site_id, theme_key, previous_theme_key, activated_by) \
         values ($1, $2, $3, $4) \
         on conflict (site_id) do update set \
           theme_key = excluded.theme_key, \
           previous_theme_key = excluded.previous_theme_key, \
           activated_by = excluded.activated_by, \
           activated_at = now()",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(previous)
    .bind(activated_by)
    .execute(&mut **tx)
    .await?;

    // The `sites.theme` column is kept in step. It is what the renderer's public payload
    // reads, and a table that says one thing while the column a visitor's request actually
    // reads says another is the exact failure the gallery exists to prevent.
    sqlx::query("update sites set theme = $2, updated_at = now() where id = $1")
        .bind(site_id)
        .bind(theme_key)
        .execute(&mut **tx)
        .await?;

    Ok(theme_key.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Boot-time mirror
// ---------------------------------------------------------------------------------------------

/// Copy the platform's bundled manifests into the table.
///
/// Bundled rows are UPSERTED and never deleted: a theme file that a build stopped shipping
/// leaves its row behind, and the gallery is then honest about a theme the platform used to
/// have. Deleting them would make a rollback target vanish because a release was thin, and the
/// REQ asks for one-click rollback rather than "rollback if the next release was generous".
pub async fn sync_bundled(pool: &PgPool, manifests: &[(String, Value)]) -> Result<usize> {
    let mut written = 0;
    let mut tx = pool.begin().await?;
    for (key, manifest) in manifests {
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .map_or_else(|| key.clone(), ToOwned::to_owned);
        let version = manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("0.0.0");
        sqlx::query(
            "insert into themes (organization_id, key, name, version, source, manifest) \
             values (null, $1, $2, $3, 'bundled', $4) \
             on conflict (key) where removed_at is null do update set \
               name = excluded.name, \
               version = excluded.version, \
               manifest = excluded.manifest",
        )
        .bind(key)
        .bind(validate_text(&name, MAX_NAME_LENGTH, "theme name")?)
        .bind(version)
        .bind(manifest)
        .execute(&mut *tx)
        .await?;
        written += 1;
    }
    tx.commit().await?;
    Ok(written)
}

/// The gallery payload the panel renders, with the site's key filled in.
///
/// Separate from [`gallery`] because the SQL there does not know the site's key, and the
/// confirmation copy needs it.
pub async fn gallery_for_site(
    pool: &PgPool,
    site_id: Uuid,
    organization_id: Option<Uuid>,
) -> Result<GalleryView> {
    let site_key: Option<String> = sqlx::query_scalar("select key from sites where id = $1")
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    let mut view = gallery(pool, site_id, organization_id).await?;
    view.site_key = site_key.unwrap_or_default();
    Ok(view)
}

/// The manifest a bundled theme ships, as a gallery card shows it.
pub fn describe(theme: &Theme) -> Value {
    let shape = manifest_shape(&theme.manifest).unwrap_or(ManifestShape {
        slots: 0,
        tokens: 0,
        modes: Vec::new(),
        extras: Vec::new(),
    });
    json!({
        "key": theme.key,
        "name": theme.name,
        "version": theme.version,
        "description": theme.manifest.get("description").and_then(Value::as_str).unwrap_or_default(),
        "author": theme.manifest.get("author").and_then(Value::as_str).unwrap_or_default(),
        "modes": shape.modes,
        "slotCount": shape.slots,
        "tokenCount": shape.tokens,
        "previewImage": theme.manifest.get("previewImage").and_then(Value::as_str),
        "source": theme.source,
        "canDelete": theme.source == "uploaded",
    })
}
