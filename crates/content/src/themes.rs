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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
    /// The settings revision a rollback republished, when it republished one.
    ///
    /// `None` for a forward activation (a switch restores nothing) and for a rollback whose
    /// displaced state had published no settings — the two are different facts and both are
    /// absent for the same type, which is why this is a number the caller can print rather
    /// than a boolean that could be read either way.
    pub restored_settings_revision_no: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------------------------

/// One problem with a manifest's v2 fields, addressed by the path that would carry it.
///
/// A path and a message rather than a typed error, for the same reason [`manifest_shape`]
/// returns a sentence: the boot loader prints it to an operator's log and the upload screen
/// renders it as a row, and two call sites writing their own wording is how one file starts
/// being described two ways inside one product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestIssue {
    /// Where the problem is, in the operator's own terms (`tokens.accent.dark`).
    pub path: String,
    /// The sentence an operator reads.
    pub message: String,
}

impl ManifestIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Check the v2 fields a manifest declares are *usable*, and report every problem.
///
/// [`manifest_shape`] answers "does this file parse as a manifest" — it counts `slots` and
/// `tokens` and notes which v2 fields are present. This answers the question the count cannot:
/// whether what the theme declares can actually be rendered. The two are deliberately different
/// in one direction only. A v1 manifest with no v2 fields at all is **fine** (an author's first
/// upload must not fail on a field the renderer never reads), but a manifest that *declares* a
/// field has to declare it correctly, because a declared field is read: `sync_bundled` mirrors
/// it into the gallery, the renderer writes its tokens into CSS custom properties, and the
/// customize screen offers its settings to an operator who will then type a value the schema
/// said was impossible.
///
/// Nothing here short-circuits. A manifest with three bad slots, a token of the wrong shape and
/// a default outside its own range gets three issues, not the first one — the same rule the
/// package validator follows, because an author fixing a manifest one error per upload is an
/// author who gives up.
pub fn validate_v2_fields(manifest: &Value) -> Vec<ManifestIssue> {
    let mut issues = Vec::new();
    let Some(object) = manifest.as_object() else {
        return issues;
    };

    // `slots` — every name must be a slot the builder's picker can actually hold. A manifest
    // claiming a ninth slot is not forward-compatible here: there is no ninth slot to render,
    // so the claim can only be wrong.
    if let Some(slots) = object.get("slots") {
        match slots.as_array() {
            Some(list) => {
                for (index, entry) in list.iter().enumerate() {
                    let Some(name) = entry.as_str() else {
                        issues.push(ManifestIssue::new(
                            format!("slots[{index}]"),
                            "A slot entry is not a string.",
                        ));
                        continue;
                    };
                    if !crate::theme_layouts::is_slot(name) {
                        issues.push(ManifestIssue::new(
                            format!("slots[{index}]"),
                            format!(
                                "`{name}` is not a slot the platform renders. Known slots: {}.",
                                crate::theme_layouts::SLOTS.join(", ")
                            ),
                        ));
                    }
                }
            }
            None => issues.push(ManifestIssue::new("slots", "`slots` is not a list.")),
        }
    }

    // `tokens` — the renderer writes every one of these into a CSS custom property, so a
    // token is either a plain string or a `{light, dark}` pair. One more level of nesting is
    // not a future format, it is a value no style rule can read.
    if let Some(tokens) = object.get("tokens") {
        match tokens.as_object() {
            Some(map) => {
                for (name, value) in map {
                    if !token_value_is_readable(value) {
                        issues.push(ManifestIssue::new(
                            format!("tokens.{name}"),
                            format!("`{name}` is neither a value nor a `{{light, dark}}` pair."),
                        ));
                    }
                }
            }
            None => issues.push(ManifestIssue::new("tokens", "`tokens` is not an object.")),
        }
    }

    // `settingsSchema` — the customize screen reads `type`, `default`, `min` and `max` to
    // describe and bound a setting, so a declaration whose own default is outside its own range
    // is a manifest that would hand an operator a value the schema forbids.
    if let Some(schema) = object.get("settingsSchema") {
        match schema.as_object() {
            Some(map) => {
                for (name, spec) in map {
                    check_setting_spec(name, spec, &mut issues);
                }
            }
            None => issues.push(ManifestIssue::new(
                "settingsSchema",
                "`settingsSchema` is not an object of setting name to specification.",
            )),
        }
    }

    // `compatibility` — the engine is the one field with a value the platform compares itself
    // to, so an unrecognised engine is a theme that will never load rather than a theme that
    // degrades.
    if let Some(compatibility) = object.get("compatibility") {
        match compatibility.as_object() {
            Some(map) => match map.get("engine").and_then(Value::as_str) {
                Some(engine) if engine.trim().is_empty() => issues.push(ManifestIssue::new(
                    "compatibility.engine",
                    "'engine' is blank; a theme with no engine cannot be checked for compatibility.",
                )),
                Some(engine) if !SUPPORTED_ENGINES.contains(&engine) => issues.push(ManifestIssue::new(
                    "compatibility.engine",
                    format!(
                        "`{engine}` is not an engine this platform renders. Supported: {}.",
                        SUPPORTED_ENGINES.join(", ")
                    ),
                )),
                _ => {}
            },
            None => issues.push(ManifestIssue::new(
                "compatibility",
                "`compatibility` is not an object.",
            )),
        }
    }

    // `aliases` — an alias is a promise that an installation pinned to an older key resolves,
    // so an alias that is not a usable key is a key nobody can be pinned to.
    if let Some(aliases) = object.get("aliases") {
        match aliases.as_array() {
            Some(list) => {
                for (index, entry) in list.iter().enumerate() {
                    let Some(alias) = entry.as_str() else {
                        issues.push(ManifestIssue::new(
                            format!("aliases[{index}]"),
                            "An alias is not a string.",
                        ));
                        continue;
                    };
                    if validate_key(alias, "theme alias").is_err() {
                        issues.push(ManifestIssue::new(
                            format!("aliases[{index}]"),
                            format!("`{alias}` is not a usable theme key."),
                        ));
                    }
                }
            }
            None => issues.push(ManifestIssue::new("aliases", "`aliases` is not a list.")),
        }
    }

    // `previewImage` — the gallery draws this, and it is a path the manifest names. The one
    // thing that is always wrong is a path that walks: a manifest is a file an author ships
    // and the loader mirrors into every gallery, so `../` in it is a traversal waiting for a
    // reader that joins it. Whether the named file *exists* is not this function's question —
    // the gallery answers that by falling back to a drawn swatch, and an absent image is a
    // missing file, not a malformed manifest.
    if let Some(image) = object.get("previewImage") {
        match image.as_str() {
            Some(value) if value.trim().is_empty() => issues.push(ManifestIssue::new(
                "previewImage",
                "'previewImage' is blank; omit it rather than declaring an empty path.",
            )),
            Some(value) if value.split(['/', '\\']).any(|part| part == "..") => {
                issues.push(ManifestIssue::new(
                    "previewImage",
                    format!("`{value}` walks out of the theme's own directory."),
                ));
            }
            Some(_) => {}
            None => issues.push(ManifestIssue::new(
                "previewImage",
                "'previewImage' is a path, not an object.",
            )),
        }
    }

    issues
}

/// The URL the gallery card uses to draw a theme's preview image.
///
/// `None` when the manifest names nothing, or names a file the asset route will not serve —
/// and the caller then draws a generated swatch, which is honest where an image that 404s is
/// not. This lives beside the manifest reader rather than in the route because the card
/// payload and the route have to agree on which URLs exist, and a rule computed twice is a
/// rule that will be computed two ways.
pub fn preview_url(key: &str, preview_image: Option<&str>) -> Option<String> {
    let name = preview_image?;
    // The leaf, because a manifest may name the file on its own (`preview.svg`) or under a
    // directory (`styles/preview.png`, which is what the scaffolding CLI writes).
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    // The one name the platform serves for a bundled theme. An uploaded package's bytes live
    // in object storage and have no route here, so the card falls back for them.
    if leaf != PREVIEW_ASSET_NAME {
        return None;
    }
    // The key is checked with the platform's own key rule: it becomes a path segment in the
    // URL the card requests, and a card must never be handed a URL the route would refuse.
    // `validate_key` lowercases what it accepts, so the check is "accepted AND unchanged" —
    // a card asking for `/themes/UPPER/assets/preview.svg` would be served a 404 on a
    // case-sensitive filesystem while the gallery believed it had offered an image.
    match validate_key(key, "theme key") {
        Ok(normalised) if normalised == key => {}
        _ => return None,
    }
    Some(format!("/api/v1/themes/{key}/assets/{leaf}"))
}

/// The one file name a bundled theme's preview image has, across all ten themes.
pub const PREVIEW_ASSET_NAME: &str = "preview.svg";

/// The engine a manifest may name. One value today; a list rather than a string because the
/// comparison is "is this engine one of ours", and adding a second engine must not mean
/// rewriting the comparison.
const SUPPORTED_ENGINES: [&str; 1] = ["omnion-web"];

/// Whether a token value is something the renderer can put on an element.
///
/// Mirrors the rule the package validator applies to a package's `tokens` map, and says the
/// same thing for the same reason: the value reaches `style` as a CSS custom property, so a
/// shape the stylesheet cannot read is a token that silently does nothing.
fn token_value_is_readable(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Object(map) => map
            .get("light")
            .or_else(|| map.get("dark"))
            .is_some_and(Value::is_string),
        _ => false,
    }
}

/// Check one `settingsSchema` entry, reporting every problem it carries.
fn check_setting_spec(name: &str, spec: &Value, issues: &mut Vec<ManifestIssue>) {
    let Some(map) = spec.as_object() else {
        issues.push(ManifestIssue::new(
            format!("settingsSchema.{name}"),
            "A setting is not an object with a type, a default and its bounds.",
        ));
        return;
    };

    let kind = match map.get("type").and_then(Value::as_str) {
        Some("number") | Some("text") | Some("color") | Some("select") => map
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some(other) => {
            issues.push(ManifestIssue::new(
                format!("settingsSchema.{name}.type"),
                format!("`{other}` is not a setting type. Known types: number, text, color, select."),
            ));
            String::new()
        }
        None => {
            issues.push(ManifestIssue::new(
                format!("settingsSchema.{name}.type"),
                "A setting declares no type; the customize screen would have nothing to render.",
            ));
            String::new()
        }
    };

    // Bounds only make sense for a number, and a bound on a text setting is a number the
    // validator would silently never apply.
    let min = map.get("min").and_then(Value::as_f64);
    let max = map.get("max").and_then(Value::as_f64);
    if kind == "number" {
        if min.is_some() != max.is_some() {
            issues.push(ManifestIssue::new(
                format!("settingsSchema.{name}"),
                "A numeric setting bounds one end and not the other; declare both or neither.",
            ));
        }
        if let (Some(min), Some(max)) = (min, max) {
            if min > max {
                issues.push(ManifestIssue::new(
                    format!("settingsSchema.{name}"),
                    format!("The minimum ({min}) is above the maximum ({max})."),
                ));
            }
        }
    } else if min.is_some() || max.is_some() {
        issues.push(ManifestIssue::new(
            format!("settingsSchema.{name}"),
            format!("A {kind} setting declares a numeric bound, which applies to nothing."),
        ));
    }

    // The default is what an operator sees before they type anything, so a default the schema
    // itself forbids is a manifest that opens the screen already in violation.
    let path = format!("settingsSchema.{name}.default");
    match map.get("default") {
        None | Some(Value::Null) => {}
        Some(value) => match kind.as_str() {
            "number" => match value.as_f64() {
                None => issues.push(ManifestIssue::new(
                    &path,
                    "A numeric setting's default is not a number.",
                )),
                Some(number)
                    if min.is_some_and(|min| number < min)
                        || max.is_some_and(|max| number > max) =>
                {
                    issues.push(ManifestIssue::new(
                        &path,
                        format!(
                            "The default ({number}) is outside the declared range {}..{}.",
                            min.map_or_else(String::new, |v| v.to_string()),
                            max.map_or_else(String::new, |v| v.to_string()),
                        ),
                    ));
                }
                Some(_) => {}
            },
            "text" | "select" => {
                if !value.is_string() {
                    issues.push(ManifestIssue::new(
                        &path,
                        format!("A {kind} setting's default is not a string."),
                    ));
                } else if kind == "select" {
                    // `select` promises a closed list; a default outside it is a value the
                    // operator cannot choose, which is worse than no default at all.
                    if let Some(options) = map.get("options").and_then(Value::as_array) {
                        let chosen = value.as_str().unwrap_or_default();
                        if !options.iter().filter_map(Value::as_str).any(|o| o == chosen) {
                            issues.push(ManifestIssue::new(
                                &path,
                                format!("`{chosen}` is not one of the options this setting offers."),
                            ));
                        }
                    }
                }
            }
            "color" => {
                if !value
                    .as_str()
                    .is_some_and(crate::theme_settings::is_hex_colour)
                {
                    issues.push(ManifestIssue::new(
                        &path,
                        "A colour setting's default is not a hex colour.",
                    ));
                }
            }
            _ => {}
        },
    }
}

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

    // `is_active` is compared against `active_key` — the site's RESOLVED theme — and not
    // against `row.active_key`, which is `site_themes.theme_key` and is therefore `null` for
    // every site that has never run an activation. That made a fresh site's gallery show the
    // `Active` badge on no card at all, while `activeKey` above it correctly said `minimal`
    // and the renderer correctly drew `minimal`: a screen that names the active theme and
    // then badges nothing, with nothing red anywhere. `active_theme_key` is the second read
    // this module already makes for exactly this reason, and the join cannot stand in for
    // it — a site has a theme long before it has an activation row.
    let themes: Vec<GalleryEntry> = rows
        .into_iter()
        .map(|row| GalleryEntry {
            is_active: row.theme_key_value == active_key,
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

/// The published settings revision a rollback would bring back, or `None` for a site whose
/// last activation displaced a state that had published nothing.
///
/// A separate read from [`read_activation`] rather than a field on [`Activation`], because the
/// two answer different questions and are used differently: `read_activation` is the rollback's
/// *precondition* (there must be a previous key at all), while this is an *extra* thing it does
/// when it can. Folding the id onto the activation struct would make "no previous key" and
/// "no previous settings" indistinguishable at the type level, and both are `None`.
pub async fn pending_rollback_settings_revision(
    pool: &PgPool,
    site_id: Uuid,
) -> Result<Option<Uuid>> {
    sqlx::query_scalar(
        "select previous_settings_revision_id from site_themes \
         where site_id = $1 and previous_theme_key is not null",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await
    .map(|row| row.flatten())
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

    // The theme being displaced is the site's RESOLVED theme, not the activation row's — and
    // those are different things for every site that has never run an activation.
    //
    // `site_themes` is written by this very function, so a site that has never been activated
    // has no row, and reading only it made `previous` `None` on the FIRST activation of a
    // site's life. That is not a harmless null: it is the theme the site is rendering RIGHT
    // NOW, so the row recorded no rollback target at all, and the operator's
    // *Restore previous* button stayed hidden while the only switch they had ever made was
    // fully reversible. `active_theme_key` is the second read this module already makes for
    // this exact reason — a site has a theme long before it has an activation row, and the
    // `sites.theme` column plus the bundled default are what it renders in the meantime.
    let previous = active_theme_key(pool, site_id).await?;
    let current = read_activation(pool, site_id).await?;
    // Activating the theme that is already active is not an error, and it is also not a
    // change: writing the row would move `previous_theme_key` to the active key, and the next
    // *Restore previous* would then restore the theme that was already in use.
    if previous == key {
        // The activation row is guaranteed here: `previous` came from the RESOLVED theme, and
        // a resolved theme equal to the requested key means the site is already on it — which
        // on a site with no activation row cannot happen, because the resolved key would then
        // be `sites.theme` or the bundled default and the requested key is the theme the
        // operator picked. The row is therefore read for its CURRENT key, which is the same
        // string either way.
        let row = current.expect(
            "the resolved key equals the requested key only on a site that has been activated",
        );
        // The row is NOT touched. A re-activation displaces nothing, and the rollback target
        // it already carries is a real, spendable target: the operator is on `corporate`,
        // they asked for `corporate` again, and `minimal` is still what a rollback would
        // return the site to. Clearing the column here would spend that target on a call that
        // changed no theme — and my own walk
        // (`re_activating_the_active_theme_leaves_the_rollback_settings_target_alone`) failed
        // for exactly that reason, which is why the pointer is left alone here rather than
        // defended by a comment.
        //
        // The RESPONSE reports nothing displaced, because this call displaced nothing. Those
        // are two different questions and conflating them is what made the gallery offer a
        // rollback whose target the response claimed did not exist.
        return Ok(ActivationChange {
            site_id,
            theme_key: row.theme_key,
            previous_theme_key: None,
            restored: false,
            // Nothing was republished, so there is no revision to name.
            restored_settings_revision_no: None,
        });
    }

    let mut tx = pool.begin().await?;
    let stored = write_activation(&mut tx, site_id, &key, Some(previous.clone()), activated_by).await?;
    tx.commit().await?;

    Ok(ActivationChange {
        site_id,
        theme_key: stored,
        // Always `Some` now: the resolved theme exists for every site, so a forward
        // activation displaces something real. The type stays `Option` because
        // `rollback_target` filters a self-referential target to `None` — "going back would
        // change nothing" is still a distinct answer from "there was something".
        previous_theme_key: Some(previous),
        restored: false,
        restored_settings_revision_no: None,
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

    // The settings the displaced state was live with, captured by the activation that
    // displaced it. Read BEFORE the write below, because that write overwrites the column.
    //
    // Restoring it goes through `theme_settings::restore_revision` rather than moving the
    // published pointer backwards: that function writes a NEW revision with a number, an
    // author and a place in the history, which is what "the rollback is itself recorded"
    // means everywhere else in this module. Moving the pointer would produce a history
    // listing revisions in an order the site never had.
    //
    // `null` is a real answer (the site had published nothing when it was displaced) and is
    // not an error: the theme still comes back, it just renders with the theme's defaults.
    let previous_settings = pending_rollback_settings_revision(pool, site_id).await?;

    let mut tx = pool.begin().await?;
    let stored = write_activation(&mut tx, site_id, &target, Some(displaced.clone()), activated_by)
        .await?;
    tx.commit().await?;

    let restored_settings_revision_no = match previous_settings {
        // `revision_no()` is itself an `Option` because a draft save publishes nothing —
        // and this path can only ever produce a `Restored`, so the inner `None` is
        // unreachable in practice. Flattened rather than `Some(Some(..))` because a nested
        // `Option` here would need the caller to unwrap twice to learn whether the rollback
        // republished anything, and the one caller reads it to print a number.
        Some(id) => crate::theme_settings::restore_revision_by_id(pool, site_id, id)
            .await?
            .and_then(|change| change.revision_no()),
        None => None,
    };

    Ok(ActivationChange {
        site_id,
        theme_key: stored,
        previous_theme_key: Some(displaced),
        restored: true,
        restored_settings_revision_no,
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
    // The published settings revision that is live RIGHT NOW, captured before the pointer
    // moves. This is the half `previous_theme_key` cannot express and the reason a rollback
    // that restored only the key left the displaced theme rendering under the incoming
    // theme's colours — a complete, valid, wrong page. Read inside the same transaction as
    // the write, so two concurrent activations cannot both claim the same revision as "the
    // one being displaced".
    //
    // `null` is a real answer: a site that has published no settings has no revision to
    // bring back, and a rollback must not invent one.
    let published_id: Option<Uuid> = sqlx::query_scalar(
        "select revision_id from theme_settings_published where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(&mut **tx)
    .await?;

    // Both directions store the key they displaced, which is what makes a rollback reversible
    // in the same way an activation is. The two callers pass exactly that value, so there is
    // no flag to get wrong here.
    sqlx::query(
        "insert into site_themes (site_id, theme_key, previous_theme_key, \
                                  previous_settings_revision_id, activated_by) \
         values ($1, $2, $3, $4, $5) \
         on conflict (site_id) do update set \
           theme_key = excluded.theme_key, \
           previous_theme_key = excluded.previous_theme_key, \
           previous_settings_revision_id = excluded.previous_settings_revision_id, \
           activated_by = excluded.activated_by, \
           activated_at = now()",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(previous)
    .bind(published_id)
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
        // The URL the card actually requests, computed by the same function the asset route
        // uses to decide what it will serve. `previewImage` stays too — it is the manifest's
        // own claim, and an operator reading the card should see what the manifest said.
        "previewUrl": preview_url(
            &theme.key,
            theme.manifest.get("previewImage").and_then(Value::as_str),
        ),
        "source": theme.source,
        "canDelete": theme.source == "uploaded",
    })
}

#[cfg(test)]
mod v2_tests {
    use serde_json::json;

    use super::{preview_url, validate_v2_fields, ManifestIssue};

    /// The paths of a manifest's problems, in the order they were found.
    fn paths(issues: &[ManifestIssue]) -> Vec<&str> {
        issues.iter().map(|issue| issue.path.as_str()).collect()
    }

    /// A manifest that declares nothing beyond the required keys must produce no issues.
    ///
    /// This is the half of the rule that protects a first theme author: v1 is not a failure,
    /// because the fields it lacks are fields the renderer never reads.
    #[test]
    fn a_v1_manifest_declares_nothing_and_is_refused_nothing() {
        let issues = validate_v2_fields(&json!({ "key": "minimal", "name": "Minimal", "version": "1.0.0" }));
        assert_eq!(issues, Vec::new(), "a v1 manifest has no v2 fields to get wrong");
    }

    /// The defect this closes: a bundled or uploaded manifest that DECLARES a setting whose
    /// own default falls outside the range it declares. The presence check that shipped passes
    /// this file, so before this validator existed nothing in the platform could object to it.
    #[test]
    fn a_default_outside_its_own_declared_range_is_reported() {
        let manifest = json!({
            "key": "corporate", "name": "Corporate", "version": "1.0.0",
            "settingsSchema": {
                "containerWidth": { "type": "number", "default": 3000, "min": 720, "max": 1600 }
            }
        });
        let issues = validate_v2_fields(&manifest);
        assert_eq!(paths(&issues), vec!["settingsSchema.containerWidth.default"]);
        assert!(
            issues[0].message.contains("outside the declared range"),
            "the sentence must say what is wrong, got: {}",
            issues[0].message
        );
    }

    /// Every problem, not the first one. An author fixing a manifest one error per upload is an
    /// author who gives up, which is the same rule the package validator follows.
    #[test]
    fn every_problem_is_reported_not_only_the_first() {
        let manifest = json!({
            "key": "corporate", "name": "Corporate", "version": "1.0.0",
            "slots": ["header", "foot"],
            "tokens": { "accent": { "light": { "deep": "#fff" } } },
            "settingsSchema": {
                "containerWidth": { "type": "number", "default": 3000, "min": 720, "max": 1600 },
                "radius": { "type": "slider", "default": "0.5rem" }
            },
            "compatibility": { "engine": "wordpress" },
            "aliases": ["Not A Key"],
            "previewImage": "../../etc/passwd.svg"
        });
        let issues = validate_v2_fields(&manifest);
        // The order is the order the walker produces, which for a JSON object is the map's
        // own order (sorted by name) — not the order the fields appear in the source. An
        // assertion written against the source order would be asserting a serde detail.
        assert_eq!(
            paths(&issues),
            vec![
                "slots[1]",
                "tokens.accent",
                "settingsSchema.containerWidth.default",
                "settingsSchema.radius.type",
                "compatibility.engine",
                "aliases[0]",
                "previewImage",
            ],
            "seven declared faults, seven rows"
        );
    }

    /// A token the renderer cannot put on an element. `{light: {deep: …}}` is one level of
    /// nesting a style rule cannot read, and the value reaches the page as a CSS custom
    /// property — so this is a token that silently does nothing.
    #[test]
    fn a_token_that_is_neither_a_value_nor_a_pair_is_reported() {
        for bad in [json!({"light": {"deep": "#fff"}}), json!(7), json!(["#fff"])] {
            let manifest = json!({
                "key": "k", "name": "n", "version": "1",
                "tokens": { "accent": bad }
            });
            assert_eq!(paths(&validate_v2_fields(&manifest)), vec!["tokens.accent"]);
        }
    }

    /// A string and a `{light, dark}` pair are both readable, and a pair needs only one of the
    /// two — the same rule the package validator applies, so a bundled theme and an uploaded
    /// package are never judged by two different definitions of a valid token.
    #[test]
    fn the_shapes_the_renderer_can_use_are_accepted() {
        let manifest = json!({
            "key": "k", "name": "n", "version": "1",
            "tokens": {
                "accent": { "light": "#2f6feb", "dark": "#7aa2f7" },
                "ink": "#111111",
                "paper": { "light": "#ffffff" }
            }
        });
        assert_eq!(validate_v2_fields(&manifest), Vec::new());
    }

    /// A setting bounded at one end only is a bound the validator would never apply, so it is
    /// reported rather than stored as a promise the platform does not keep.
    #[test]
    fn a_setting_bounded_at_one_end_only_is_reported() {
        let manifest = json!({
            "key": "k", "name": "n", "version": "1",
            "settingsSchema": { "baseSize": { "type": "number", "default": 18, "min": 14 } }
        });
        let issues = validate_v2_fields(&manifest);
        assert_eq!(paths(&issues), vec!["settingsSchema.baseSize"]);
        assert!(issues[0].message.contains("both or neither"), "{}", issues[0].message);
    }

    /// A bound on a text setting applies to nothing — it is a number no code compares — and a
    /// `select` default outside its own option list is a value the operator cannot choose.
    #[test]
    fn bounds_on_text_and_a_select_default_outside_its_options_are_reported() {
        let manifest = json!({
            "key": "k", "name": "n", "version": "1",
            "settingsSchema": {
                "fontStack": { "type": "text", "default": "Inter", "min": 10, "max": 20 },
                "headerVariant": { "type": "select", "options": ["a", "b"], "default": "z" }
            }
        });
        assert_eq!(
            paths(&validate_v2_fields(&manifest)),
            vec![
                "settingsSchema.fontStack",
                "settingsSchema.headerVariant.default"
            ]
        );
    }

    /// A type the customize screen has no control for, and a setting with no type at all.
    #[test]
    fn an_unknown_or_missing_setting_type_is_reported() {
        let manifest = json!({
            "key": "k", "name": "n", "version": "1",
            "settingsSchema": {
                "gap": { "type": "slider", "default": 4 },
                "loose": { "default": 4 }
            }
        });
        assert_eq!(
            paths(&validate_v2_fields(&manifest)),
            vec!["settingsSchema.gap.type", "settingsSchema.loose.type"]
        );
    }

    /// A manifest the renderer would accept, declared in full, must produce nothing — the
    /// validator that is always right is the one nobody trusts, so the positive case is a
    /// test and not an assumption.
    #[test]
    fn a_manifest_that_renders_reports_nothing() {
        let manifest = json!({
            "key": "magazine", "name": "Magazine", "version": "1.0.0",
            "modes": ["light", "dark"],
            "slots": ["header", "footer", "home", "blog-list", "single-page", "product", "404", "search"],
            "tokens": { "canvas": { "light": "#fffdf9", "dark": "#131110" } },
            "settingsSchema": {
                "containerWidth": { "type": "number", "default": 1120, "min": 720, "max": 1600 },
                "baseSize": { "type": "number", "default": 18, "min": 14, "max": 22 },
                "radius": { "type": "number", "default": 0, "min": 0, "max": 28 }
            },
            "compatibility": { "engine": "omnion-web", "minVersion": "0.1.0" },
            "aliases": ["editorial", "news"],
            "previewImage": "preview.svg"
        });
        assert_eq!(validate_v2_fields(&manifest), Vec::new());
    }

    /// A manifest naming a slot the platform does not render is the defect this validator
    /// exists to catch, and the ten shipped themes all shipped it: every one declared
    /// `single`, while the slot the builder's picker holds is `single-page`. A presence check
    /// passed those files, because the field was there — and it was wrong.
    #[test]
    fn a_slot_the_platform_does_not_render_is_reported() {
        let manifest = json!({
            "key": "k", "name": "n", "version": "1",
            "slots": ["header", "single"]
        });
        let issues = validate_v2_fields(&manifest);
        assert_eq!(paths(&issues), vec!["slots[1]"]);
        assert!(issues[0].message.contains("single-page"), "{}", issues[0].message);
    }

    /// The card's URL and the rule that produced it, in the one place both are visible.
    #[test]
    fn the_card_url_names_the_file_the_route_serves() {
        assert_eq!(
            preview_url("magazine", Some("preview.svg")).as_deref(),
            Some("/api/v1/themes/magazine/assets/preview.svg")
        );
        // A manifest that names a directory is still the leaf that is served.
        assert_eq!(
            preview_url("magazine", Some("styles/preview.svg")).as_deref(),
            Some("/api/v1/themes/magazine/assets/preview.svg")
        );
        // A key that cannot name a directory never gets a URL, and a name the platform does
        // not serve never becomes one.
        assert_eq!(preview_url("../etc", Some("preview.svg")), None);
        assert_eq!(preview_url("mine", Some("styles/preview.png")), None);
        assert_eq!(preview_url("mine", None), None);
        // `validate_key` accepts `UPPER` by normalising it to `upper`; the URL must not be
        // built from a spelling the route would refuse, and the route must not be handed one
        // it cannot resolve on a case-sensitive filesystem.
        assert_eq!(preview_url("UPPER", Some("preview.svg")), None);
    }
}
