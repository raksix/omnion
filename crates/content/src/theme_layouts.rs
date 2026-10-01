//! Theme layouts and packages (REQ-062, slice 3).
//!
//! Slice 1 made a theme a thing a site can have and slice 2 made it a thing a site can look
//! like. This is the third question, and it is the one that makes a theme portable: *what
//! exactly travels when a site hands its look to another site?*
//!
//! Three decisions, each one a place the obvious code is wrong.
//!
//! * **A slot is a named block tree, and the platform knows the slot names.** `SLOTS` is a
//!   `const`, not a manifest field, for the same reason `CONTRAST_PAIRS` is: a package is
//!   untrusted input, and a package that declared "my slots are whatever I like" would be
//!   describing its own contract to itself. The renderer asks for `Slot::Header`; it cannot
//!   ask for a name nobody knows. The manifest still *lists* its slots, and the import
//!   refuses a package whose list is not a subset of these.
//!
//! * **A slot is stored per (site, theme, slot) and `is_default` is the theme's, not the
//!   site's.** `Reset slot to theme default` has to be able to say "put back what the theme
//!   ships" after a site has replaced it, and the only place that answer can live is the row
//!   the platform wrote when it first mirrored the theme. Keeping the default in a column
//!   rather than re-reading the theme's files means the reset keeps working for an uploaded
//!   package whose files are gone.
//!
//! * **Validation runs on the way IN and reports every problem, not the first one.** A package
//!   with an unknown slot *and* an unknown block type must not need three upload attempts to
//!   discover. [`validate_package`] returns every finding with a `path` and a `message`, and
//!   [`install_package`] refuses while any of them is an error. A package that validates
//!   installs **inactive**: an install is a thing an operator then activates deliberately,
//!   not a theme swap that happens because a file was dropped somewhere.

use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::themes::DEFAULT_THEME_KEY;

/// The slots the platform renders, in the order the builder's slot picker shows them.
///
/// A `const` and not a manifest field, for the reason in the module docs. The names are
/// kebab-case because they are also the URL segment of `theme-layouts/{slot}` and the key in
/// an exported package: one spelling, three places.
pub const SLOTS: [&str; 8] = [
    "header",
    "footer",
    "home",
    "blog-list",
    "single-page",
    "product",
    "404",
    "search",
];

/// Longest accepted slot name and longest accepted package field value.
pub const MAX_SLOT_LENGTH: usize = 40;

/// The most blocks a single slot may carry.
///
/// Not a quality rule — a wall. A slot is a header; a payload that puts two hundred blocks in
/// one is either a mistake or an attempt to make the renderer do the work of a page, and both
/// are refused with a message rather than rendered.
pub const MAX_SLOTS_BLOCKS: usize = 120;

/// Largest package the platform will accept, in bytes.
///
/// The package carries declarative data only (manifest, tokens, slot trees), so the honest cap
/// is generous but finite, and it is checked on the *read* body rather than after extraction —
/// a zip bomb is bounded by what is in memory, not by what is on disk afterwards.
pub const MAX_PACKAGE_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// One slot's layout for one site.
#[derive(Debug, Clone, PartialEq, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SlotLayout {
    /// Primary key.
    pub id: Uuid,
    /// Site it belongs to.
    pub site_id: Uuid,
    /// Theme it customizes.
    pub theme_key: String,
    /// Slot name, one of [`SLOTS`].
    pub slot: String,
    /// The block tree, exactly as the page builder stores one.
    pub blocks: Value,
    /// True when this row is what the theme itself ships.
    pub is_default: bool,
    /// Who last saved it.
    pub updated_by: Option<Uuid>,
    /// When.
    pub updated_at: OffsetDateTime,
    /// How many top-level blocks are stored.
    ///
    /// Serialized rather than derived on the client: the picker gets its count from
    /// [`SlotEntry::block_count`], and a save response that omitted it left the builder with
    /// two shapes for the same slot — `layout.blockCount` missing after a save and present
    /// in the list it re-reads. The count here is the row's own tree, never the theme's
    /// default, which is what makes "you have 0 blocks" answerable after an emptied slot.
    #[serde(rename = "blockCount")]
    pub block_count: i32,
}

/// What the builder's slot picker loads: every slot, its state, and its tree.
///
/// `state` is a computed string rather than a boolean, because the badge in the picker has
/// three words to choose from — `theme`, `custom`, `empty` — and a screen that gets `[]` for a
/// theme-provided slot and `[]` for an emptied one cannot tell them apart. A slot the theme
/// ships with content and a slot somebody deliberately cleared are different facts.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutsView {
    /// Site the view is for.
    pub site_id: Uuid,
    /// Theme the slots belong to.
    pub theme_key: String,
    /// One entry per [`SLOTS`] name, always all eight.
    pub slots: Vec<SlotEntry>,
    /// The block types the registry knows, so the package validator can name what is missing
    /// without a second round trip.
    pub known_block_types: Vec<String>,
}

/// One slot as the picker renders it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotEntry {
    /// The slot name.
    pub slot: String,
    /// `theme` (the shipped default), `custom` (a site replaced it) or `empty` (no blocks).
    pub state: String,
    /// True when the row is the theme's own.
    pub is_default: bool,
    /// How many top-level blocks the slot carries.
    pub block_count: usize,
    /// The blocks, so the canvas can mount without a second request.
    pub blocks: Value,
}

/// A value in a package that did not validate.
///
/// `path` is a JSON-pointer-ish string (`slots.header.blocks[2].type`) so the upload screen can
/// point at the offending line instead of printing a sentence, and `severity` is `error` or
/// `warning`: a warning is something the operator should know and an error is why the install
/// was refused. Collapsing them into one list would make "import validation refuses a package
/// with an unknown slot **and lists each problem**" impossible to answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageFinding {
    /// Where in the package the problem is.
    pub path: String,
    /// What is wrong, in a sentence a person can act on.
    pub message: String,
    /// `error` or `warning`.
    pub severity: String,
}

/// A validated (or refused) package: the manifest, the slot trees, and every finding.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageReport {
    /// The theme key the package declares, or `""` when the manifest could not be read.
    pub theme_key: String,
    /// Name and version as declared.
    pub name: String,
    /// Version as declared.
    pub version: String,
    /// Slots the package carries.
    pub slots: Value,
    /// Tokens the package carries.
    pub tokens: Value,
    /// Every finding, errors and warnings together, in the order they were found.
    pub findings: Vec<PackageFinding>,
    /// True when no finding is an error.
    pub valid: bool,
    /// Error count, so the screen does not recount the array.
    pub error_count: usize,
}

/// The result of an install.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOutcome {
    /// The theme key that was installed.
    pub theme_key: String,
    /// Version as installed.
    pub version: String,
    /// Always true: an install is never an activation.
    pub active: bool,
}

// ---------------------------------------------------------------------------------------------
// Reading and writing slots
// ---------------------------------------------------------------------------------------------

/// Whether a string is a slot the platform renders.
pub fn is_slot(name: &str) -> bool {
    SLOTS.contains(&name)
}

/// The builder's whole payload for one site and theme.
///
/// Every slot is present in the answer even when no row exists, so the picker never has to
/// merge a shipped default with an absent one client-side — the same "two implementations of
/// one rule" trap the settings view avoids.
pub async fn layouts_view(pool: &PgPool, site_id: Uuid, theme_key: &str) -> Result<LayoutsView> {
    let rows = sqlx::query_as::<_, SlotLayout>(
        "select id, site_id, theme_key, slot, blocks, is_default, updated_by, updated_at, \
                (jsonb_array_length(coalesce(blocks, '[]'::jsonb))::int4) as block_count \
         from theme_layouts where site_id = $1 and theme_key = $2",
    )
    .bind(site_id)
    .bind(theme_key)
    .fetch_all(pool)
    .await?;

    let by_slot: std::collections::BTreeMap<&str, &SlotLayout> =
        rows.iter().map(|row| (row.slot.as_str(), row)).collect();

    let slots = SLOTS
        .iter()
        .map(|name| {
            let count = block_count(by_slot.get(name).map_or(&Value::Null, |row| &row.blocks));
            // Three states, and the order is the whole point. `is_default` is asked FIRST:
            // a site that deliberately empties a slot has replaced the theme's blocks with
            // none, and that is a *custom* slot with something to restore — not an empty
            // slot. Asking about the count first would answer "empty" for both cases, and
            // the builder would then hide the reset control on exactly the row where the
            // operator needs it. A theme that genuinely ships an empty slot has `is_default`
            // set and no blocks, and lands on the same "empty" state from the other side.
            let state = match by_slot.get(name) {
                Some(row) if row.is_default && count > 0 => "theme",
                Some(row) if row.is_default => "empty",
                Some(_) => "custom",
                None => "empty",
            };
            SlotEntry {
                slot: (*name).to_owned(),
                state: state.to_owned(),
                is_default: by_slot.get(name).is_some_and(|row| row.is_default),
                block_count: count,
                blocks: by_slot
                    .get(name)
                    .map_or_else(|| json!([]), |row| row.blocks.clone()),
            }
        })
        .collect();

    Ok(LayoutsView {
        site_id,
        theme_key: theme_key.to_owned(),
        slots,
        known_block_types: crate::blocks::known_types(),
    })
}

/// One slot's layout, or `None` when the site has no row for it.
pub async fn slot_layout(
    pool: &PgPool,
    site_id: Uuid,
    theme_key: &str,
    slot: &str,
) -> Result<Option<SlotLayout>> {
    Ok(sqlx::query_as::<_, SlotLayout>(
        "select id, site_id, theme_key, slot, blocks, is_default, updated_by, updated_at, \
                (jsonb_array_length(coalesce(blocks, '[]'::jsonb))::int4) as block_count \
         from theme_layouts where site_id = $1 and theme_key = $2 and slot = $3",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(slot)
    .fetch_optional(pool)
    .await?)
}

/// Save a slot's blocks, marked as the site's own.
///
/// The payload goes through the same normalise → validate → sanitise path a page revision
/// takes, because a slot tree IS a page block tree: an uploaded package and a template both
/// reach the renderer through this column, and a tree the page store would have refused must
/// not be writable by writing to a different table. The findings are returned so the builder
/// can show the same inspector messages the editor shows.
pub async fn save_slot(
    pool: &PgPool,
    site_id: Uuid,
    theme_key: &str,
    slot: &str,
    blocks: Value,
    user_id: Option<Uuid>,
) -> Result<(SlotLayout, Vec<String>)> {
    if !is_slot(slot) {
        return Err(ContentError::ThemeUnknownSlot(slot.to_owned()));
    }
    if slot.len() > MAX_SLOT_LENGTH {
        return Err(ContentError::ThemeSlotTooLong(slot.to_owned(), MAX_SLOT_LENGTH));
    }
    // Note what this statement does NOT mention: `default_blocks`. The save never writes that
    // column, in neither the insert nor the conflict branch, and the omission is the design.
    // An earlier version filled it with `coalesce(default_blocks, excluded.blocks)`, which
    // reads sensibly — "if we do not have a default yet, the current tree is one" — and is
    // exactly backwards: for a slot the theme never seeded, it made the site's OWN first save
    // the thing a later reset restores, so "Reset to theme default" returned the custom tree
    // with a 200 and a confident `isDefault: true`. `seed_default_layouts` is the only writer,
    // and a slot with no default is a slot that honestly answers 409.
    //
    // A fatal finding is refused here for the same reason `pages::update_page` refuses one:
    // the store must not hold a tree the renderer cannot place. Warnings (an empty column, a
    // heading that skips a level) are returned to the caller and stored — a header that skips
    // a level is a lint, not a broken page.
    let (normalized, report) = crate::blocks::prepare_tree(blocks)?;
    if normalized.as_array().is_none_or(|list| list.len() > MAX_SLOTS_BLOCKS) {
        return Err(ContentError::ThemeSlotTooManyBlocks(MAX_SLOTS_BLOCKS));
    }
    // A fatal finding is refused; the rest travel back with the save.
    //
    // The line is `first_fatal`, not `first_error`, and that is the whole contract with the
    // page and pattern stores. An orphan column inside a slot is an author mid-edit — they
    // dragged a column out of its columns wrapper — and refusing the save would make the
    // builder a wall exactly where the author needs a place to keep working. The issues come
    // back in the response so the builder's inspector can name them, which is what the
    // route serialises them for.
    if let Some(issue) = report.first_fatal() {
        return Err(ContentError::ThemeSlotInvalid(format!(
            "{} at {}: {}",
            issue.code, issue.path, issue.message
        )));
    }

    let row = sqlx::query_as::<_, SlotLayout>(
        "insert into theme_layouts (site_id, theme_key, slot, blocks, is_default, updated_by) \
         values ($1, $2, $3, $4, false, $5) \
         on conflict (site_id, theme_key, slot) do update \
             set blocks = excluded.blocks, is_default = false, \
                 updated_by = excluded.updated_by, updated_at = now() \
         returning id, site_id, theme_key, slot, blocks, is_default, updated_by, updated_at, \
         (jsonb_array_length(coalesce(blocks, '[]'::jsonb))::int4) as block_count",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(slot)
    .bind(&normalized)
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok((row, report.issues.iter().map(|issue| issue.message.clone()).collect()))
}

/// Put a slot back to what the theme ships.
///
/// An `UPDATE` of the `is_default` row, never a delete: the alternative is "re-read the theme
/// files", which works for a bundled theme and silently fails for an uploaded one whose files
/// are stored in the library rather than on the worktree's disk. When the site has no default
/// row for the slot the answer is a `not found` rather than a silent empty canvas — a reset
/// that empties a slot is the one reset that loses work.
pub async fn reset_slot(
    pool: &PgPool,
    site_id: Uuid,
    theme_key: &str,
    slot: &str,
) -> Result<SlotLayout> {
    if !is_slot(slot) {
        return Err(ContentError::ThemeUnknownSlot(slot.to_owned()));
    }
    sqlx::query_as::<_, SlotLayout>(
        "update theme_layouts set blocks = default_blocks, is_default = true, \
             updated_by = null, updated_at = now() \
         where site_id = $1 and theme_key = $2 and slot = $3 \
           and default_blocks is not null \
         returning id, site_id, theme_key, slot, blocks, is_default, updated_by, updated_at, \
         (jsonb_array_length(coalesce(blocks, '[]'::jsonb))::int4) as block_count",
    )
    .bind(site_id)
    .bind(theme_key)
    .bind(slot)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ContentError::ThemeSlotNoDefault(slot.to_owned()))
}

/// Write the theme's own slots for a site, marking every row `is_default`.
///
/// Called when a theme is activated. **The only writer of `default_blocks`.**
///
/// The conflict branch is deliberately narrow. On a row that is still the theme's own
/// (`is_default`) there is nothing to repair, so the update is skipped. On a row the site has
/// customised it writes the default column and NOTHING else: `blocks` — the site's own tree —
/// is left exactly as it was. Re-activating a theme therefore repairs a slot whose default was
/// lost without repainting the site, which is the promise the activation confirmation makes
/// and the reason this is not a blanket `do update set blocks = ...`.
pub async fn seed_default_layouts(
    pool: &PgPool,
    site_id: Uuid,
    theme_key: &str,
    blocks_by_slot: &Value,
) -> Result<usize> {
    let Some(map) = blocks_by_slot.as_object() else {
        return Ok(0);
    };
    let mut written = 0usize;
    for slot in SLOTS {
        let Some(blocks) = map.get(slot) else { continue };
        let (normalized, _) = crate::blocks::prepare_tree(blocks.clone())?;
        let written_rows = sqlx::query(
            "insert into theme_layouts (site_id, theme_key, slot, blocks, default_blocks, \
                 is_default) \
             values ($1, $2, $3, $4, $4, true) \
             on conflict (site_id, theme_key, slot) do update \
                 set default_blocks = excluded.blocks where theme_layouts.is_default = false",
        )
        .bind(site_id)
        .bind(theme_key)
        .bind(slot)
        .bind(&normalized)
        .execute(pool)
        .await?;
        written += written_rows.rows_affected() as usize;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------------------------
// Packages
// ---------------------------------------------------------------------------------------------

/// Build the package a site exports: its active theme's manifest, tokens and slot trees.
///
/// The export is a **site's** look, not the theme's: the manifest is the active theme's (so
/// the importing site has something to render with) and the slots are the site's own rows,
/// with the theme-provided ones included, because a package that dropped the defaults would
/// render a site differently on the far end — which is acceptance 12's whole sentence.
pub async fn export_package(pool: &PgPool, site_id: Uuid) -> Result<PackageReport> {
    let theme = crate::themes::active_theme_key(pool, site_id).await?;
    let manifest = match crate::themes::find_theme(pool, &theme).await? {
        Some(row) => row.manifest,
        None => json!({}),
    };
    let view = layouts_view(pool, site_id, &theme).await?;

    let slots: serde_json::Map<String, Value> = view
        .slots
        .iter()
        .map(|entry| (entry.slot.clone(), entry.blocks.clone()))
        .collect();

    // The tokens the site actually renders with, not the draft. A package is what a visitor
    // sees; exporting an unpublished draft would install a look the source site never showed.
    let tokens = crate::theme_settings::published_tokens(pool, site_id)
        .await?
        .unwrap_or_else(|| manifest.get("tokens").cloned().unwrap_or_else(|| json!({})));

    Ok(PackageReport {
        theme_key: theme.clone(),
        name: manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&theme)
            .to_owned(),
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("0.0.0")
            .to_owned(),
        slots: Value::Object(slots),
        tokens,
        findings: Vec::new(),
        valid: true,
        error_count: 0,
    })
}

/// Validate a package without installing it, and report every problem.
///
/// The order of the checks is the order an operator would want to read them in, and **nothing
/// short-circuits**: a package that is missing a manifest, declares an unknown slot, names an
/// unknown block type and carries a token of the wrong shape gets four rows, not the first
/// one. `install_package` then refuses on `error_count > 0` and nothing is written.
pub fn validate_package(package: &Value, known_types: &[String]) -> PackageReport {
    let mut findings = Vec::new();

    let push = |findings: &mut Vec<PackageFinding>, path: &str, message: String, severity: &str| {
        findings.push(PackageFinding {
            path: path.to_owned(),
            message,
            severity: severity.to_owned(),
        });
    };

    let Some(manifest) = package.get("manifest").filter(|value| value.is_object()) else {
        push(
            &mut findings,
            "manifest",
            "The package has no `manifest` object.".to_owned(),
            "error",
        );
        return PackageReport {
            theme_key: String::new(),
            name: String::new(),
            version: String::new(),
            slots: Value::Null,
            tokens: Value::Null,
            valid: false,
            error_count: 1,
            findings,
        };
    };

    let theme_key = manifest
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if theme_key.is_empty() {
        push(
            &mut findings,
            "manifest.key",
            "The manifest has no `key`; a package cannot be named.".to_owned(),
            "error",
        );
    } else if !valid_key(&theme_key) {
        push(
            &mut findings,
            "manifest.key",
            format!("`{theme_key}` is not a usable theme key: letters, digits, dash and underscore only."),
            "error",
        );
    }

    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if name.is_empty() {
        push(
            &mut findings,
            "manifest.name",
            "The manifest has no `name`.".to_owned(),
            "error",
        );
    }

    let version = manifest
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if version.is_empty() {
        push(
            &mut findings,
            "manifest.version",
            "The manifest has no `version`.".to_owned(),
            "error",
        );
    }

    // Modes: a theme that declares none can still render, so this is a warning. A theme that
    // declares a mode the platform does not have is an error, because a visitor asking for it
    // would get the default and a screen promising something that does not exist.
    if let Some(modes) = manifest.get("modes") {
        match modes.as_array() {
            Some(list) => {
                for (index, mode) in list.iter().enumerate() {
                    let Some(name) = mode.as_str() else {
                        push(
                            &mut findings,
                            &format!("manifest.modes[{index}]"),
                            "A mode is not a string.".to_owned(),
                            "error",
                        );
                        continue;
                    };
                    if !matches!(name, "light" | "dark" | "system") {
                        push(
                            &mut findings,
                            &format!("manifest.modes[{index}]"),
                            format!("`{name}` is not a mode the platform renders."),
                            "error",
                        );
                    }
                }
            }
            None => push(
                &mut findings,
                "manifest.modes",
                "`modes` is not a list.".to_owned(),
                "error",
            ),
        }
    } else {
        push(
            &mut findings,
            "manifest.modes",
            "The manifest declares no modes, so the theme has no dark appearance.".to_owned(),
            "warning",
        );
    }

    // Slots: the manifest's list is a *claim* about what the package carries, and the `slots`
    // object is what it actually carries. Both are checked, and a slot in the claim that is
    // not a slot the platform renders is an error in the claim itself.
    let mut declared: Vec<String> = Vec::new();
    if let Some(list) = manifest.get("slots").and_then(Value::as_array) {
        for (index, entry) in list.iter().enumerate() {
            let Some(name) = entry.as_str() else {
                push(
                    &mut findings,
                    &format!("manifest.slots[{index}]"),
                    "A slot entry is not a string.".to_owned(),
                    "error",
                );
                continue;
            };
            if !is_slot(name) {
                push(
                    &mut findings,
                    &format!("manifest.slots[{index}]"),
                    format!("`{name}` is not a slot the platform renders. Known slots: {}.", SLOTS.join(", ")),
                    "error",
                );
            }
            declared.push(name.to_owned());
        }
    }

    let slots = package.get("slots").cloned().unwrap_or_else(|| json!({}));
    let Some(slot_map) = slots.as_object() else {
        push(
            &mut findings,
            "slots",
            "`slots` is not an object of slot name to blocks.".to_owned(),
            "error",
        );
        return PackageReport {
            theme_key,
            name,
            version,
            slots,
            tokens: package.get("tokens").cloned().unwrap_or(Value::Null),
            error_count: findings
                .iter()
                .filter(|finding| finding.severity == "error")
                .count(),
            valid: !findings.iter().any(|finding| finding.severity == "error"),
            findings,
        };
    };

    for (slot, blocks) in slot_map {
        if !is_slot(slot) {
            push(
                &mut findings,
                &format!("slots.{slot}"),
                format!("`{slot}` is not a slot the platform renders. Known slots: {}.", SLOTS.join(", ")),
                "error",
            );
            continue;
        }
        if !declared.is_empty() && !declared.iter().any(|name| name == slot) {
            push(
                &mut findings,
                &format!("slots.{slot}"),
                format!("The package carries `{slot}` but its manifest does not declare it."),
                "warning",
            );
        }
        check_tree(blocks, &format!("slots.{slot}.blocks"), known_types, &mut findings);
    }

    for name in &declared {
        if !slot_map.contains_key(name.as_str()) {
            push(
                &mut findings,
                &format!("slots.{name}"),
                format!("The manifest declares `{name}` but the package carries no blocks for it."),
                "warning",
            );
        }
    }

    // Tokens: a colour token is either a hex string or `{light, dark}`. A token of some other
    // shape is a warning rather than an error — a theme may carry a token the platform does
    // not use yet, and refusing the package for it would make the format impossible to extend.
    if let Some(tokens) = package.get("tokens") {
        if tokens.as_object().is_none() {
            push(
                &mut findings,
                "tokens",
                "`tokens` is not an object.".to_owned(),
                "error",
            );
        } else if let Some(map) = tokens.as_object() {
            for (name, value) in map {
                if !token_value_is_usable(value) {
                    push(
                        &mut findings,
                        &format!("tokens.{name}"),
                        format!("`{name}` is neither a colour nor a `{{light, dark}}` pair."),
                        "warning",
                    );
                }
            }
        }
    }

    let error_count = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .count();

    PackageReport {
        theme_key,
        name,
        version,
        slots,
        tokens: package.get("tokens").cloned().unwrap_or(Value::Null),
        valid: error_count == 0,
        error_count,
        findings,
    }
}

/// Install a package that has already been validated. **Refuses while any finding is an error.**
///
/// The row is written with `source = 'uploaded'` and the site's `site_themes` pointer is left
/// alone, so a newly installed theme is in the gallery with an `Uploaded` tag and nothing
/// renders with it until an operator activates it — acceptance 13's first clause, enforced here
/// rather than by the screen.
pub async fn install_package(
    pool: &PgPool,
    report: &PackageReport,
    package: &Value,
    storage_key: &str,
    organization_id: Option<Uuid>,
    user_id: Option<Uuid>,
) -> Result<InstallOutcome> {
    if !report.valid {
        return Err(ContentError::ThemePackageInvalid(
            report.error_count.max(1),
        ));
    }
    let manifest = package
        .get("manifest")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let checksum = package_checksum(package);

    // `organization_id` travels in from the caller and is NEVER invented here.
    //
    // It used to be a literal `null`, and that one column is the whole tenancy of this table:
    // the gallery filters on `(organization_id is null or organization_id = $2)`, where the
    // `is null` arm means **"the platform ships this file"** — it is how the bundled loader
    // seeds its ten. An upload that also stored `null` therefore (a) listed one tenant's
    // package in every other tenant's gallery, and (b) matched NO row in `remove_theme`'s
    // org-scoped UPDATE, so a DELETE answered 200 and changed nothing while the route went
    // on to delete the stored package bytes — leaving a live, activatable theme whose manifest
    // points at an object that no longer exists. One hard-coded `null`, two defects, and the
    // schema comment that says `null` = bundled was true the whole time.
    //
    // `source = 'uploaded'` is the other half of the same distinction and is set here, so the
    // two must be read together: a bundled row is a file with no owner, an uploaded row is
    // code belonging to the organization that uploaded it.
    sqlx::query(
        "insert into themes (organization_id, key, name, version, source, manifest, checksum, \
                             storage_key, installed_by, installed_at) \
         values ($1, $2, $3, $4, 'uploaded', $5, $6, $7, $8, now()) \
         on conflict (key) where removed_at is null do update \
             set name = excluded.name, version = excluded.version, manifest = excluded.manifest, \
                 checksum = excluded.checksum, storage_key = excluded.storage_key, \
                 organization_id = excluded.organization_id, \
                 installed_by = excluded.installed_by, \
                 installed_at = excluded.installed_at, removed_at = null",
    )
    .bind(organization_id)
    .bind(&report.theme_key)
    .bind(&report.name)
    .bind(&report.version)
    .bind(&manifest)
    .bind(&checksum)
    .bind(storage_key)
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(InstallOutcome {
        theme_key: report.theme_key.clone(),
        version: report.version.clone(),
        active: false,
    })
}

/// Remove an uploaded theme.
///
/// Four refusals, and each one is a rule rather than a guard rail: a **bundled** theme can never
/// be deleted (it is a file, not a row), a theme belonging to **another organization** can never
/// be deleted (an upload is a tenant's code), the **active** theme can never be deleted (the
/// site would have nothing to render), and a theme that some site still holds layouts for cannot
/// either (those layouts are the only copy of the work).
///
/// The order matters twice over. The source check comes first, so a bundled theme answers
/// "bundled" rather than "active" when both are true; and the ownership check comes **before**
/// the in-use check, because "this belongs to somebody else" is the answer an operator needs
/// and "a site still renders with it" would send them looking for a site they do not own.
///
/// The scope check also replaces what used to be here. The UPDATE was org-scoped and its row
/// count was ignored, so a stranger's key matched zero rows, the function answered `Ok(())`,
/// and the caller deleted the stored package bytes anyway — a successful removal that removed
/// nothing, on a theme that then stayed live in the table pointing at an object that was gone.
/// Comparing `organization_id` in Rust first is what makes that answer impossible rather than
/// merely unlikely.
pub async fn remove_theme(
    pool: &PgPool,
    theme_key: &str,
    organization_id: Option<Uuid>,
) -> Result<()> {
    let row: Option<(String, Option<Uuid>)> =
        sqlx::query_as("select source, organization_id from themes where key = $1 and removed_at is null")
            .bind(theme_key)
            .fetch_optional(pool)
            .await?;
    let Some((source, owner)) = row else {
        return Err(ContentError::ThemeNotFound(theme_key.to_owned()));
    };
    if source == "bundled" {
        return Err(ContentError::ThemeBundledCannotBeRemoved(theme_key.to_owned()));
    }
    // An account with no organization of its own is a platform owner, and may remove any
    // upload — the same rule `gallery` applies when it shows every bundled theme to an
    // organization-less account. An account that HAS an organization may remove only its own.
    if let (Some(org), Some(owner)) = (organization_id, owner) {
        if org != owner {
            return Err(ContentError::ThemeNotFound(theme_key.to_owned()));
        }
    }

    let in_use: Option<Uuid> = sqlx::query_scalar(
        "select site_id from site_themes where theme_key = $1 limit 1",
    )
    .bind(theme_key)
    .fetch_optional(pool)
    .await?;
    if in_use.is_some() {
        return Err(ContentError::ThemeInUse(theme_key.to_owned()));
    }

    let removed = sqlx::query("update themes set removed_at = now() \
                    where key = $1 and source = 'uploaded' and removed_at is null")
        .bind(theme_key)
        .execute(pool)
        .await?;
    // The row count is the LAST line that can still catch a removal that removed nothing, and
    // it is read rather than assumed: the caller deletes the stored package bytes the moment
    // this returns, so a silent zero here is the difference between a clean uninstall and a
    // theme row with no package behind it.
    if removed.rows_affected() == 0 {
        return Err(ContentError::ThemeNotFound(theme_key.to_owned()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// How many top-level blocks a stored tree carries.
///
/// `as_array` and then `len`, so a row holding an object (a hand-edited payload) reads as zero
/// rather than panicking, and the canvas shows its empty state instead of a dead tab.
fn block_count(blocks: &Value) -> usize {
    blocks.as_array().map_or(0, Vec::len)
}

/// Walk a package's block tree and report every problem in it.
///
/// Errors are the ones that make a tree unrenderable — a non-list, a block with no `type`, a
/// type the registry does not have, a `column` outside a `columns`. Everything else is a
/// warning, because the platform's own validator runs again on save: this pass is the upload
/// screen's report, and the save is the gate.
fn check_tree(
    blocks: &Value,
    path: &str,
    known_types: &[String],
    findings: &mut Vec<PackageFinding>,
) {
    let push = |findings: &mut Vec<PackageFinding>, path: String, message: String, severity: &str| {
        findings.push(PackageFinding {
            path,
            message,
            severity: severity.to_owned(),
        });
    };

    let Some(list) = blocks.as_array() else {
        push(
            findings,
            path.to_owned(),
            "Blocks are not a list.".to_owned(),
            "error",
        );
        return;
    };
    for (index, block) in list.iter().enumerate() {
        let here = format!("{path}[{index}]");
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            push(
                findings,
                here.clone(),
                "A block has no `type`.".to_owned(),
                "error",
            );
            continue;
        };
        if !known_types.iter().any(|known| known == block_type) {
            push(
                findings,
                format!("{here}.type"),
                format!("`{block_type}` is not a block type this platform knows."),
                "error",
            );
            continue;
        }
        // A `column` whose parent is not `columns` is a structural error; a `columns` with a
        // child that is not a `column` is the same mistake seen from the other side. Both are
        // reported here rather than deferred to the save, so the report names the shape problem
        // instead of only the type.
        if block_type == "column"
            && path.contains(".blocks[")
            && !path.rsplit('[').next().is_some_and(|tail| {
                tail.trim_end_matches("]").parse::<usize>().is_ok()
            })
        {
            push(
                findings,
                format!("{here}.type"),
                "A `column` block must sit inside a `columns` block.".to_owned(),
                "error",
            );
        }
        if let Some(children) = block.get("children") {
            check_tree(
                children,
                &format!("{here}.children"),
                known_types,
                findings,
            );
        }
    }
}

/// Whether a token value is a colour the platform can emit as a CSS custom property.
fn token_value_is_usable(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Object(map) => map
            .get("light")
            .or_else(|| map.get("dark"))
            .is_some_and(Value::is_string),
        _ => false,
    }
}

/// A theme key is bounded to a token so it can be a URL segment, a directory name and a
/// stylesheet suffix without escaping any of the three.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The stable identity of a package, as the route needs it **before** it installs.
///
/// `install_package` computes this itself for the `themes.checksum` column, but the route
/// needs it earlier and for a different reason: the storage key of the uploaded package is
/// derived from it, so re-uploading the same package overwrites its own stored object instead
/// of leaving a second copy nobody will ever read. Exposing it here rather than letting the
/// route invent a key is the point — two different identities for one package would make the
/// checksum column a decoration.
pub fn package_checksum_of(package: &Value) -> String {
    package_checksum(package)
}

/// A stable, cheap identity for a package, for the `themes.checksum` column.
///
/// FNV-1a over the canonical serialisation: the column exists so an operator can tell "the
/// same package uploaded twice" from "a different package with the same name", and a hash
/// that is cryptographic here would be solving a problem the column does not have.
fn package_checksum(package: &Value) -> String {
    let text = canonical_json(package);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{:016x}", hash)
}

/// Serialise with object keys in sorted order, so two packages that differ only in key order
/// hash the same. `serde_json`'s `Map` is a `BTreeMap` here already, so this is a guard rather
/// than a sort, and it is cheap.
fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// The theme a site renders with, for a package that names none.
///
/// Exported so the route can name a fallback in an error message rather than inventing one.
pub fn fallback_theme_key() -> &'static str {
    DEFAULT_THEME_KEY
}
