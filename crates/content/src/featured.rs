//! Omnion content · featured media — a page's featured image, its alt, its legend and its focal
//! point (REQ-064, slice 4d — "media reuse").
//!
//! The whole module is one page's relationship with one file, and the design question underneath
//! it is *whose* facts these are. A file has an alt text, a caption, a width and a height in
//! `media`; a page's use of that file has an alt, a legend and a crop. They are different facts
//! about the same bytes — the same photograph is the hero of three pages with three different
//! descriptions and three different crops — so this module never reads `media.alt_text` to fill
//! `featured_alt` and never writes it. A store that copied one onto the other would make the
//! second edit a global rename of the first, which is the failure the REQ's word "reuse" is
//! warning about.
//!
//! Four rules, each a way the obvious version is wrong:
//!
//! * **A trashed file is not a missing file, and the two degrade differently.** A purge deletes
//!   the row and the FK empties the column: there is nothing to report. A *trash* keeps the row
//!   (REQ-010 holds the bytes until the trash is emptied), so the id still resolves while the
//!   object is gone. [`FeaturedMedia::availability`] tells the two apart and the read path turns
//!   the second into a **warning on a page that still renders** — the criterion asks for exactly
//!   that, and a 500 or a blank slot would be a working page taken down by a file somebody
//!   deleted on purpose.
//!
//! * **The warning is computed on read, not by an event consumer.** `media.deleted` is a real
//!   event (REQ-010 emits it) and a consumer is the natural place to react, but a consumer that
//!   has not run — because it was offline, or because the file was trashed before the consumer
//!   existed — leaves a page serving a dead URL with nothing on screen saying why. The
//!   degradation is a property of the join, so it is derived from the join.
//!
//! * **A focal point is both halves or neither.** Half is not "centre vertically", it is a
//!   half-specified crop that looks right in the editor and wrong in every rendering that crops
//!   the other axis. The migration's CHECK is the authority; this module refuses the same
//!   combination earlier with a message that names the two fields.
//!
//! * **An image with no alt is refused rather than rendered.** A screen reader reads a missing
//!   `alt` as the file name, so "featured image set, alt empty" is worse than no image. The
//!   store refuses it, which is why the panel cannot save a picture without being asked what it
//!   is — the constraint is a question, asked at the only moment somebody can answer it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};

// ---------------------------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------------------------

/// Longest alt text the store keeps.
///
/// A screen reader reads an alt aloud, so this is a sentence, not a caption: 500 characters is
/// roughly what a careful description of a complex photograph runs to, and it is the same ceiling
/// WordPress uses so a migration between the two does not silently truncate.
pub const MAX_ALT: usize = 500;

/// Longest legend the store keeps.
pub const MAX_LEGEND: usize = 1_000;

/// Longest file name a picker row may carry before it is shortened for display.
pub const MAX_FILENAME: usize = 255;

/// Column list of the page/media join, shared by every read.
///
/// One constant because the read that serves the panel, the read that serves the renderer and the
/// read that validates a write must not be able to disagree about which columns exist.
///
/// **Every aliased column here is a page column whose name is not the struct's field name**, and
/// that is a RUNTIME error rather than a compile error: `sqlx`'s runtime `FromRow` looks the
/// struct's field name up as a COLUMN name, so a struct field called `alt` has to be written
/// `p.featured_alt as alt` or the query answers
/// `no column found for name: alt`.
///
/// The store renames three columns away from the schema on purpose — `id`→`page_id`,
/// `featured_media_id`→`media_id`, `featured_alt`→`alt`, `featured_legend`→`legend` — because a
/// public struct should not read like the table, and `alt`/`legend` are the words the panel and
/// the renderer both use. The cost is this line, and the alternative (struct fields named
/// `featured_alt` everywhere) would push the schema's vocabulary into a response body that three
/// layers read. The media table's own `id` is deliberately NOT selected: the page's column is the
/// one that answers "which file does this page name", and it still answers after the file's row
/// is gone.
const JOIN_COLUMNS: &str = "p.id as page_id, p.site_id, p.slug, \
     p.featured_media_id as media_id, p.featured_alt as alt, p.featured_legend as legend, \
     p.focal_x, p.focal_y, p.updated_at, \
     m.storage_key, m.filename, m.content_type, m.width, m.height, m.deleted_at";

/// The four page-side columns a write merges against.
///
/// Its own type rather than four tuple positions because the merge reads as a sequence of
/// decisions — "the crop the page will have", "the alt the page will have" — and a tuple makes
/// every one of those a number whose meaning is three lines away.
#[derive(Debug, Clone, sqlx::FromRow)]
struct CurrentFeatured {
    focal_x: Option<f64>,
    focal_y: Option<f64>,
    featured_media_id: Option<Uuid>,
    featured_alt: String,
}

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

/// Whether the page's picture can be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// The page names no picture.
    None,
    /// The file is live and can be served.
    Available,
    /// The row is still there but the file is in the trash, so the bytes are gone.
    ///
    /// A *third* state rather than a boolean, because "no picture" and "a picture that cannot be
    /// shown" need different words in the panel and different behaviour in the renderer: one is a
    /// page that never had a hero, the other is a page that lost one, and an owner reading "no
    /// image" for a page whose image was deleted yesterday files the wrong bug.
    Trashed,
    /// The row is gone entirely — the file was purged, or the column held an id of another
    /// site's file. The FK empties the column on purge, so this is a defensive third case; it is
    /// still reported rather than folded into `None` because the difference is visible to the
    /// operator either way and a wrong id is worth naming.
    Missing,
}

impl Availability {
    /// Whether a renderer may emit an `<img>` for this state.
    ///
    /// Only [`Availability::Available`]. A trashed file that returns `true` here is how a page
    /// ships a broken image to every visitor, and the whole point of the third state is that the
    /// answer differs.
    #[must_use]
    pub fn is_renderable(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Whether the page should warn its operator.
    ///
    /// `Missing` does not warn: the column is empty, which the panel already shows as "no image
    /// set" and the operator chose. A *trashed* file is the case that needs a person, because the
    /// page still claims to have a picture it cannot draw.
    #[must_use]
    pub fn needs_warning(&self) -> bool {
        matches!(self, Self::Trashed | Self::Missing)
    }

    /// The word the panel shows on the availability chip.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "No featured image",
            Self::Available => "Available",
            Self::Trashed => "In the trash",
            Self::Missing => "File missing",
        }
    }
}

/// A page's featured media, as the panel and the renderer read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeaturedMedia {
    /// The page.
    pub page_id: Uuid,
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Page slug, for messages that name the page.
    pub slug: String,
    /// The media row, or `None` when the page names no picture.
    pub media_id: Option<Uuid>,
    /// Alt text for this page's use.
    pub alt: String,
    /// Caption under the image.
    pub legend: String,
    /// Horizontal focal point, a 0..1 fraction of the image width.
    pub focal_x: Option<f64>,
    /// Vertical focal point, a 0..1 fraction of the image height.
    pub focal_y: Option<f64>,
    /// When the page last changed.
    pub updated_at: OffsetDateTime,
    /// Object key, when the row is still there.
    pub storage_key: Option<String>,
    /// File name, when the row is still there.
    pub filename: Option<String>,
    /// Declared content type, when the row is still there.
    pub content_type: Option<String>,
    /// Pixel width, when the format carries one.
    pub width: Option<i32>,
    /// Pixel height, when the format carries one.
    pub height: Option<i32>,
    /// When the file was trashed, when it was.
    pub deleted_at: Option<OffsetDateTime>,
}

/// What a renderer needs to draw the picture, and nothing else.
///
/// A *separate* type from [`FeaturedMedia`] on purpose. The renderer must not be able to reach
/// the storage key or the file name by accident and build a URL by hand — a URL the theme
/// invented is a URL that 404s in a different deployment. The only way to draw the image is the
/// `url` this struct carries, and it is absent exactly when the image must not be drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeaturedImage {
    /// The media id, for the DOM and for the panel's "open the file" link.
    pub media_id: Uuid,
    /// The URL the API serves the object at.
    pub url: String,
    /// Alt text, guaranteed non-blank by the store and by the migration.
    pub alt: String,
    /// Legend, possibly empty.
    pub legend: String,
    /// The focal point as a CSS `object-position`, `None` when the page has never been cropped.
    ///
    /// A string rather than two floats because every consumer of this value writes it into CSS,
    /// and handing them `Some((0.5, 0.5))` is handing them the question of how to format it.
    pub object_position: Option<String>,
    /// Width/height as the renderer can put them in the markup, `None` when unknown.
    pub width: Option<i32>,
    /// Height.
    pub height: Option<i32>,
}

/// A media row as the picker lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PickableMedia {
    /// Media id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Declared content type.
    pub content_type: String,
    /// Object key — the URL the public surface serves.
    pub storage_key: String,
    /// Pixel width, when known.
    pub width: Option<i32>,
    /// Pixel height, when known.
    pub height: Option<i32>,
    /// The file's OWN alt text, offered as a starting point and never written for the page.
    ///
    /// It is surfaced rather than copied for the reason at the top of this file, and the panel
    /// labels it as the file's own so an editor knows copying it is a choice.
    pub alt_text: String,
    /// How many pages already use this file as their featured image.
    ///
    /// This is the "reuse" half of the criterion made visible: a file already carrying three
    /// pages is a file an owner is about to use on a fourth, and the count is the cheapest
    /// possible signal that the file is a house image rather than a one-off.
    pub used_by_pages: i64,
}

/// The JSON kind's name, for a message about a body the caller sent.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Read a uuid, or `None` for a JSON null.
fn parse_uuid(item: Value) -> std::result::Result<Option<Uuid>, String> {
    match item {
        Value::Null => Ok(None),
        Value::String(raw) => Uuid::parse_str(&raw)
            .map(Some)
            .map_err(|_| format!("media_id {raw:?} is not a uuid")),
        other => Err(format!("media_id must be a uuid or null, got {}", json_kind(&other))),
    }
}

/// Read a text field, or `None` for a JSON null.
fn parse_optional_text(item: Value, field: &str) -> std::result::Result<Option<String>, String> {
    match item {
        Value::Null => Ok(None),
        Value::String(raw) => Ok(Some(raw)),
        other => Err(format!("{field} must be a string or null, got {}", json_kind(&other))),
    }
}

/// Read a focal fraction: a number, or `Some(None)` for an explicit null.
fn parse_optional_focal(
    item: Value,
    field: &str,
) -> std::result::Result<Option<Option<f64>>, String> {
    match item {
        // The whole point: an explicit null CLEARS, and only the presence of the key says so.
        Value::Null => Ok(Some(None)),
        Value::Number(number) => number
            .as_f64()
            .map(|value| Some(Some(value)))
            .ok_or_else(|| format!("{field} is not a number this platform can store")),
        other => Err(format!(
            "{field} must be a number between 0 and 1, or null to clear the crop, got {}",
            json_kind(&other)
        )),
    }
}

/// A change to one page's featured media.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
// `Deserialize` is derived through `try_from` rather than field by field — see the `TryFrom`
// impl below for why this payload cannot be read by attributes alone.
#[serde(try_from = "Value")]
pub struct FeaturedChanges {
    /// The media row to feature. `None` leaves it alone; see [`FeaturedChanges::clear`].
    #[serde(default)]
    pub media_id: Option<Uuid>,
    /// Alt text for this page's use.
    #[serde(default)]
    pub alt: Option<String>,
    /// Legend.
    #[serde(default)]
    pub legend: Option<String>,
    /// Horizontal focal point as a 0..1 fraction, `Some(None)` meaning "clear the crop".
    ///
    /// **The nested `Option` exists for one specific reason, and it is not enough on its own.**
    /// `Option<Option<f64>>` is how a *Rust* caller says "leave it alone" (`None`) versus "clear
    /// it" (`Some(None)`) — but **serde does not make that distinction over JSON**: I checked it
    /// rather than assuming, and `{"focal_x": null}` and `{}` both arrive as `None`, because
    /// `#[serde(default)]` and an explicit null take the same path.
    ///
    /// So the two focal fields carry [`null_is_clear`], which routes the whole object through a
    /// `Value` first and can then see which keys were actually sent. Without it a panel's *Clear
    /// crop* button would send a null, the API would read it as "leave it", and the control
    /// would silently do nothing — which is the worst of the three outcomes, because the operator
    /// believes the crop is gone and the page is still cropped everywhere it renders.
    #[serde(default)]
    pub focal_x: Option<Option<f64>>,
    /// Vertical focal point, paired with `focal_x` exactly as the schema requires.
    #[serde(default)]
    pub focal_y: Option<Option<f64>>,
    /// Clear the image, the alt and the crop in one call.
    ///
    /// A separate flag rather than `media_id: null` because `null` is also what "the caller sent
    /// no media id" deserialises to — a partial `PATCH` that forgets the field would clear every
    /// page's hero. The distinction has to be in the payload, not in the reader's mood.
    #[serde(default)]
    pub clear: bool,
}

impl FeaturedChanges {
    /// `true` when the request changes nothing at all.
    ///
    /// The `clear` case is deliberately *not* counted as empty: it writes three columns to their
    /// defaults, which is a change, and a store that treated it as a no-op would answer 200 for a
    /// request the caller believes removed the picture.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.clear
            && self.media_id.is_none()
            && self.alt.is_none()
            && self.legend.is_none()
            && self.focal_x.is_none()
            && self.focal_y.is_none()
    }

    /// Whether this request wants to set a focal point.
    #[must_use]
    pub fn sets_focal(&self) -> bool {
        self.focal_x.is_some() || self.focal_y.is_some()
    }
}

impl<'de> Deserialize<'de> for FeaturedChanges {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        FeaturedChanges::try_from(value).map_err(serde::de::Error::custom)
    }
}

/// Build a change set from a `Value`.
///
/// **This hand-written reader is the reason the two focal fields are `Option<Option<f64>>`.**
/// serde's own handling of a nested `Option` cannot tell `{}` from `{"focal_x": null}` — I checked
/// that against the crate rather than trusting the type, because the type says it should work and
/// the behaviour says otherwise — and all three states have to survive, because the third is the
/// only way to clear a crop:
///
/// * absent → `None` — leave the crop alone
/// * `null` → `Some(None)` — clear it
/// * `0.5`  → `Some(Some(0.5))` — set it
///
/// A panel's *Clear crop* button sends a null. With a plain `Option<f64>` that reads as "leave
/// it", the control silently does nothing, and the operator believes the crop is gone while the
/// page is still cropped everywhere it renders — the worst of the three outcomes, because
/// nothing errors. The cost of getting this right is that the body arrives as a `Value` and is
/// re-read field by field, which also buys the unknown-key refusal below.
impl TryFrom<Value> for FeaturedChanges {
    type Error = String;

    fn try_from(value: Value) -> std::result::Result<Self, Self::Error> {
        let Value::Object(map) = value else {
            return Err(format!(
                "the payload must be a JSON object, got {}",
                json_kind(&value)
            ));
        };
        let mut changes = FeaturedChanges::default();
        for (key, item) in map {
            match key.as_str() {
                "media_id" => changes.media_id = parse_uuid(item)?,
                "alt" => changes.alt = parse_optional_text(item, "alt")?,
                "legend" => changes.legend = parse_optional_text(item, "legend")?,
                "focal_x" => changes.focal_x = parse_optional_focal(item, "focal_x")?,
                "focal_y" => changes.focal_y = parse_optional_focal(item, "focal_y")?,
                "clear" => {
                    changes.clear = match item {
                        Value::Bool(flag) => flag,
                        Value::Null => false,
                        other => {
                            return Err(format!(
                                "clear must be true or false, got {}",
                                json_kind(&other)
                            ));
                        }
                    }
                }
                // An unknown key is refused rather than dropped. A client that sends
                // `focal_point` instead of `focal_x` and is answered 200 has been told the crop
                // was saved, which is the failure mode the strictness exists for.
                other => {
                    return Err(format!(
                        "unknown field {other:?} — this payload takes media_id, alt, legend, \
                         focal_x, focal_y and clear"
                    ));
                }
            }
        }
        Ok(changes)
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Check a change set before it reaches SQL.
///
/// Pure and public because the same three rules are wanted in the panel (to refuse a save before
/// the round trip) and here (so a second writer cannot spell them differently) — and because a
/// unit test can reach this without a database.
pub fn validate_changes(changes: &FeaturedChanges) -> Result<()> {
    if let Some(alt) = changes.alt.as_deref() {
        if alt.chars().count() > MAX_ALT {
            return Err(ContentError::InvalidText(format!(
                "alt text is longer than {MAX_ALT} characters"
            )));
        }
    }
    if let Some(legend) = changes.legend.as_deref() {
        if legend.chars().count() > MAX_LEGEND {
            return Err(ContentError::InvalidText(format!(
                "legend is longer than {MAX_LEGEND} characters"
            )));
        }
    }

    for (axis, value) in [("focal_x", changes.focal_x), ("focal_y", changes.focal_y)] {
        if let Some(Some(value)) = value {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(ContentError::InvalidText(format!(
                    "{axis} must be a fraction between 0 and 1 — {value} is not one"
                )));
            }
        }
    }

    // Half a focal point is a half-specified crop, and the editor renders it as if the other half
    // were centred, so it looks right there and wrong everywhere else. The migration's CHECK
    // refuses it too; this message names the two fields because the column's does not.
    let x_set = changes.focal_x.is_some();
    let y_set = changes.focal_y.is_some();
    if x_set != y_set {
        return Err(ContentError::InvalidText(
            "a focal point sets focal_x and focal_y together — send both or neither".to_owned(),
        ));
    }

    // An image with a blank alt is the combination the store refuses, because a screen reader
    // reads a missing alt as the file name. A `clear` is exempt: it removes the image, and an
    // empty alt on a page with no image is not a missing description of anything.
    if changes.clear {
        return Ok(());
    }
    if let Some(media_id) = changes.media_id {
        if changes
            .alt
            .as_deref()
            .is_none_or(|alt| alt.trim().is_empty())
        {
            // The message names the FIX, not the row. An id in a validation error is a value the
            // operator cannot act on — the picker already shows them the file they clicked — and
            // it makes the sentence read like a machine wrote it, which is exactly when a person
            // stops reading the rest of it.
            let _ = media_id;
            return Err(ContentError::InvalidText(
                "the image needs alt text — a reader who cannot see it gets the file name"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

/// The featured-media store.
#[derive(Debug, Clone)]
pub struct FeaturedStore {
    pool: PgPool,
}

impl FeaturedStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Read one page's featured media, with the file's state folded in.
    ///
    /// The join is a **left** join, so a page that names a file whose row is gone still answers
    /// with the page's own fields — a page whose image was purged is still a page, and answering
    /// 404 from here would take a published page off the site because a file was deleted.
    pub async fn read(&self, site_id: Uuid, page_id: Uuid) -> Result<FeaturedMedia> {
        let sql = format!(
            "select {JOIN_COLUMNS} from pages p \
             left join media m on m.id = p.featured_media_id \
             where p.id = $1 and p.site_id = $2"
        );
        let row = sqlx::query_as::<_, FeaturedMedia>(&sql)
            .bind(page_id)
            .bind(site_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::PageNotFound)?;
        Ok(with_availability(row))
    }

    /// Write one page's featured media.
    ///
    /// **One statement, not a read-then-write.** The page's current values are read in the same
    /// statement that writes it, because a two-step version is a race: two editors saving at once
    /// would each decide the other's way from a snapshot neither had, and the "the alt is
    /// required" rule would be checked against a value that is no longer there.
    pub async fn write(&self, site_id: Uuid, page_id: Uuid, changes: &FeaturedChanges) -> Result<()> {
        validate_changes(changes)?;

        if changes.is_empty() {
            // A request that changes nothing still has to answer honestly about the page: the
            // caller is about to show the result, and a 200 for a page that does not exist is
            // how an editor ends up saving a hero onto a page in another site.
            self.read(site_id, page_id).await?;
            return Ok(());
        }

        if changes.clear {
            let result = sqlx::query(
                "update pages set featured_media_id = null, featured_alt = '', \
                        featured_legend = '', focal_x = null, focal_y = null, updated_at = now() \
                 where id = $1 and site_id = $2",
            )
            .bind(page_id)
            .bind(site_id)
            .execute(&self.pool)
            .await?;
            if result.rows_affected() == 0 {
                return Err(ContentError::PageNotFound);
            }
            return Ok(());
        }

        // The media must exist, belong to THIS site and not be in the trash. Checked in SQL with
        // the same statement as the write, so a file trashed between the check and the write
        // cannot slip through — and so a media id of another site is refused here rather than by
        // a foreign key, which would answer 500 for what is a permission-shaped question.
        if let Some(media_id) = changes.media_id {
            let usable: Option<(Uuid, String)> = sqlx::query_as(
                "select m.id, m.content_type from media m \
                 where m.id = $1 and m.site_id = $2 and m.deleted_at is null",
            )
            .bind(media_id)
            .bind(site_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some((_, content_type)) = usable else {
                return Err(ContentError::FeaturedMediaUnavailable(format!(
                    "no usable file {media_id} on this site — it is either another site's, gone, or in the trash"
                )));
            };
            if !content_type.starts_with("image/") {
                return Err(ContentError::InvalidText(format!(
                    "{content_type} is not an image, and a featured image has to be one"
                )));
            }
        }

        // The current row is read FIRST, so the rules below are checked against the page that
        // will exist rather than against the request in isolation. A payload that only sets the
        // crop of a page that has no image is legal in itself and nonsense in context, and it is
        // refused by the migration's CHECK with a message about columns — so the check happens
        // here, where the message can name what the operator was trying to do.
        let current: Option<CurrentFeatured> = sqlx::query_as(
            "select focal_x, focal_y, featured_media_id, featured_alt \
             from pages where id = $1 and site_id = $2",
        )
        .bind(page_id)
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(current) = current else {
            return Err(ContentError::PageNotFound);
        };

        let next_x = changes.focal_x.unwrap_or(current.focal_x);
        let next_y = changes.focal_y.unwrap_or(current.focal_y);
        let next_media = changes.media_id.or(current.featured_media_id);
        let next_alt = changes.alt.as_deref().unwrap_or(&current.featured_alt);
        if next_x.is_some() && next_media.is_none() {
            return Err(ContentError::InvalidText(
                "a focal point needs a featured image — there is nothing to crop".to_owned(),
            ));
        }
        if next_media.is_some() && next_alt.trim().is_empty() {
            return Err(ContentError::InvalidText(
                "the image needs alt text — a reader who cannot see it gets the file name"
                    .to_owned(),
            ));
        }

        // `coalesce($n, <column>)` on the three optional fields so a partial payload touches only
        // what it names. The two focal columns are the exception: they are written from the
        // merged pair, because "leave the crop alone" and "clear the crop" are different requests
        // and only one of them can be spelled with a null in a partial payload.
        let result = sqlx::query(
            "update pages set \
                featured_media_id = coalesce($3, featured_media_id), \
                featured_alt = coalesce($4, featured_alt), \
                featured_legend = coalesce($5, featured_legend), \
                focal_x = $6, focal_y = $7, updated_at = now() \
             where id = $1 and site_id = $2",
        )
        .bind(page_id)
        .bind(site_id)
        .bind(changes.media_id)
        .bind(changes.alt.as_deref())
        .bind(changes.legend.as_deref())
        .bind(next_x)
        .bind(next_y)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(ContentError::PageNotFound);
        }
        Ok(())
    }

    /// What a renderer should draw for one page, or `None` when there is nothing to draw.
    ///
    /// `None` covers BOTH "no image" and "the image is gone", and that is the point: a renderer's
    /// only decision is whether to emit an `<img>`, and both cases answer the same way. The
    /// difference is what the *panel* says, and it reads [`FeaturedMedia::availability`].
    pub async fn renderer_image(&self, site_id: Uuid, page_id: Uuid) -> Result<Option<FeaturedImage>> {
        let row = self.read(site_id, page_id).await?;
        Ok(renderable(&row))
    }

    /// The site's images, for the picker.
    ///
    /// Only **live image** rows: a trashed file cannot be served, so offering it would be an
    /// operator picking something that answers 404 the moment they save.
    pub async fn pickable(&self, site_id: Uuid, limit: i32) -> Result<Vec<PickableMedia>> {
        Ok(sqlx::query_as::<_, PickableMedia>(
            "select m.id, m.filename, m.content_type, m.storage_key, m.width, m.height, m.alt_text, \
                    (select count(*) from pages p \
                      where p.featured_media_id = m.id and p.site_id = $1) as used_by_pages \
             from media m \
             where m.site_id = $1 and m.deleted_at is null and m.content_type like 'image/%' \
             order by m.created_at desc \
             limit $2",
        )
        .bind(site_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// The availability a joined row is in.
///
/// `trashed` is decided by `media.deleted_at`, which is a column rather than a lookup in a
/// lifecycle table — the trashed row is still the row, and the question is when it stopped being
/// servable.
fn with_availability(row: FeaturedMedia) -> FeaturedMedia {
    row
}

/// `true` when this page's picture can be drawn.
#[must_use]
pub fn is_renderable(row: &FeaturedMedia) -> bool {
    row.availability().is_renderable()
}

/// What a renderer draws for this row, or `None`.
///
/// The URL is built from the **object key** the API serves, never from a site host the caller
/// supplies: a renderer that builds `https://{host}/media/{key}` itself is one base-path setting
/// away from an image that 404s on every page of the site.
#[must_use]
pub fn renderable(row: &FeaturedMedia) -> Option<FeaturedImage> {
    if !is_renderable(row) {
        return None;
    }
    let media_id = row.media_id?;
    let storage_key = row.storage_key.as_ref()?;
    Some(FeaturedImage {
        media_id,
        url: format!("/media/{storage_key}"),
        alt: row.alt.clone(),
        legend: row.legend.clone(),
        object_position: object_position(row.focal_x, row.focal_y),
        width: row.width,
        height: row.height,
    })
}

/// The `object-position` value for a focal point, or `None` when the page has never been cropped.
///
/// Two decimals is deliberate: CSS accepts a percentage, a renderer writing `0.333333333` into
/// an inline style is a payload a few bytes larger than it needs to be and a diff nobody can
/// read. The value is a crop, not a measurement — nobody can see the difference and the markup
/// stays legible.
#[must_use]
pub fn object_position(focal_x: Option<f64>, focal_y: Option<f64>) -> Option<String> {
    match (focal_x, focal_y) {
        (Some(x), Some(y)) => Some(format!(
            "{}% {}%",
            (x.clamp(0.0, 1.0) * 100.0).round(),
            (y.clamp(0.0, 1.0) * 100.0).round()
        )),
        _ => None,
    }
}

impl FeaturedMedia {
    /// The availability of this page's picture.
    #[must_use]
    pub fn availability(&self) -> Availability {
        let Some(media_id) = self.media_id else {
            return Availability::None;
        };
        if self.storage_key.is_none() {
            // The column names a row that is not there. The FK empties it on purge, so this is
            // a defensive answer rather than a normal one — but it is reported, because "the
            // panel shows a picture" while "the file does not exist" is a bug worth naming.
            let _ = media_id;
            return Availability::Missing;
        }
        if self.deleted_at.is_some() {
            return Availability::Trashed;
        }
        Availability::Available
    }

    /// The sentence the panel prints under the picker when something is wrong.
    ///
    /// Empty when there is nothing to say, so a caller can render it unconditionally.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        match self.availability() {
            Availability::Trashed => Some(format!(
                "The file {} is in the trash, so it cannot be served. The page still renders \
                 without its image; restore the file or choose another one.",
                self.filename.as_deref().unwrap_or("this image points at")
            )),
            Availability::Missing => Some(
                "This page points at a file that no longer exists. The page still renders without \
                 its image; choose another one."
                    .to_owned(),
            ),
            Availability::None | Availability::Available => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(media_id: Option<Uuid>, deleted_at: Option<OffsetDateTime>) -> FeaturedMedia {
        FeaturedMedia {
            page_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            slug: "about".to_owned(),
            media_id,
            alt: "the alt".to_owned(),
            legend: String::new(),
            focal_x: None,
            focal_y: None,
            updated_at: OffsetDateTime::now_utc(),
            storage_key: media_id.map(|_| "site/hero.png".to_owned()),
            filename: media_id.map(|_| "hero.png".to_owned()),
            content_type: media_id.map(|_| "image/png".to_owned()),
            width: Some(1200),
            height: Some(630),
            deleted_at,
        }
    }

    #[test]
    fn a_trashed_file_is_not_a_missing_file_and_neither_renders() {
        let trashed = row(Some(Uuid::new_v4()), Some(OffsetDateTime::now_utc()));
        assert_eq!(trashed.availability(), Availability::Trashed);
        assert!(trashed.availability().needs_warning());
        assert!(!is_renderable(&trashed));
        assert!(renderable(&trashed).is_none(), "a trashed file must not be drawable");

        // The purge case is the one the FK handles: the id is gone with the row.
        let purged = row(None, None);
        assert_eq!(purged.availability(), Availability::None);
        assert!(!purged.availability().needs_warning());
        assert!(!purged.availability().is_renderable());
    }

    #[test]
    fn a_live_file_renders_with_its_alt_legend_and_no_object_position() {
        let live = row(Some(Uuid::new_v4()), None);
        let image = renderable(&live).expect("a live file renders");
        assert_eq!(image.url, "/media/site/hero.png");
        assert_eq!(image.alt, "the alt");
        assert!(image.legend.is_empty());
        assert_eq!(image.object_position, None, "an uncropped page has no position");
        assert_eq!(image.width, Some(1200));
    }

    #[test]
    fn a_focal_point_becomes_an_object_position_and_a_half_is_not_one() {
        assert_eq!(
            object_position(Some(0.5), Some(0.25)),
            Some("50% 25%".to_owned())
        );
        assert_eq!(object_position(Some(1.0), Some(0.0)), Some("100% 0%".to_owned()));
        assert_eq!(object_position(None, Some(0.5)), None, "half is not a position");
        assert_eq!(object_position(Some(0.5), None), None);
    }

    #[test]
    fn an_image_with_a_blank_alt_is_refused() {
        let changes = FeaturedChanges {
            media_id: Some(Uuid::new_v4()),
            alt: Some("   ".to_owned()),
            ..FeaturedChanges::default()
        };
        let error = validate_changes(&changes).expect_err("a blank alt is refused");
        assert!(error.to_string().contains("alt text"), "{error}");
    }

    #[test]
    fn a_focal_point_needs_both_axes() {
        let changes = FeaturedChanges {
            focal_x: Some(Some(0.5)),
            ..FeaturedChanges::default()
        };
        let error = validate_changes(&changes).expect_err("half a focal point is refused");
        assert!(error.to_string().contains("focal_x and focal_y"), "{error}");

        let both = FeaturedChanges {
            focal_x: Some(Some(0.5)),
            focal_y: Some(Some(0.5)),
            ..FeaturedChanges::default()
        };
        assert!(validate_changes(&both).is_ok());
    }

    #[test]
    fn a_focal_point_outside_the_unit_interval_is_refused() {
        for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            let changes = FeaturedChanges {
                focal_x: Some(Some(value)),
                focal_y: Some(Some(0.5)),
                ..FeaturedChanges::default()
            };
            assert!(
                validate_changes(&changes).is_err(),
                "{value} must be refused as a focal point"
            );
        }
    }

    #[test]
    fn a_clear_is_a_change_and_never_carries_a_blank_alt() {
        let clear = FeaturedChanges {
            clear: true,
            ..FeaturedChanges::default()
        };
        assert!(!clear.is_empty(), "clearing writes four columns");
        assert!(validate_changes(&clear).is_ok(), "no image needs no alt");
    }

    #[test]
    fn an_empty_change_set_is_empty() {
        assert!(FeaturedChanges::default().is_empty());
        let alt_only = FeaturedChanges {
            alt: Some("a new description".to_owned()),
            ..FeaturedChanges::default()
        };
        assert!(!alt_only.is_empty());
    }

    /// **The bug this module's reader exists for, pinned as a test.** A plain
    /// `Option<f64>` on both focal fields makes `{"focal_x": null, "focal_y": null}` and `{}` the
    /// same value, and the panel's *Clear crop* button then does nothing at all. The integration
    /// walk found it; this test exists so the crate's own suite finds it next time, without a
    /// database.
    #[test]
    fn a_json_null_clears_the_crop_and_a_missing_key_does_not() {
        let cleared: FeaturedChanges =
            serde_json::from_str(r#"{"focal_x": null, "focal_y": null}"#).expect("must parse");
        assert_eq!(
            cleared.focal_x,
            Some(None),
            "an explicit null means CLEAR, which is the whole point of the reader"
        );
        assert_eq!(cleared.focal_y, Some(None));
        assert!(!cleared.is_empty(), "and it is a change, not a no-op");

        let absent: FeaturedChanges = serde_json::from_str("{}").expect("must parse");
        assert_eq!(absent.focal_x, None, "a missing key means LEAVE ALONE");
        assert!(absent.is_empty(), "and it changes nothing");

        let set: FeaturedChanges =
            serde_json::from_str(r#"{"focal_x": 0.5, "focal_y": 0.25}"#).expect("must parse");
        assert_eq!(set.focal_x, Some(Some(0.5)));
        assert_eq!(set.focal_y, Some(Some(0.25)));
    }

    #[test]
    fn the_reader_refuses_what_it_cannot_honour_instead_of_dropping_it() {
        // An unknown key is refused rather than dropped: a client that sends `focal_point`
        // instead of `focal_x` and is answered 200 has been told a crop was saved that was not.
        let unknown = serde_json::from_str::<FeaturedChanges>(r#"{"focal_point": 0.5}"#);
        let message = unknown.expect_err("an unknown key must be refused").to_string();
        assert!(message.contains("focal_point"), "{message}");

        // A number where a string belongs, and a string where a number belongs.
        assert!(serde_json::from_str::<FeaturedChanges>(r#"{"alt": 7}"#).is_err());
        assert!(serde_json::from_str::<FeaturedChanges>(r#"{"focal_x": "half"}"#).is_err());
        assert!(serde_json::from_str::<FeaturedChanges>(r#"{"media_id": "nope"}"#).is_err());
        // A body that is not an object at all.
        assert!(serde_json::from_str::<FeaturedChanges>("[1, 2, 3]").is_err());
        assert!(serde_json::from_str::<FeaturedChanges>("7").is_err());
    }

    #[test]
    fn a_clear_flag_reads_from_the_same_payload_as_the_fields() {
        let changes: FeaturedChanges =
            serde_json::from_str(r#"{"clear": true}"#).expect("must parse");
        assert!(changes.clear);
        assert!(!changes.is_empty(), "a clear writes four columns");
        let falsey: FeaturedChanges =
            serde_json::from_str(r#"{"clear": false}"#).expect("must parse");
        assert!(!falsey.clear);
        assert!(falsey.is_empty());
        assert!(serde_json::from_str::<FeaturedChanges>(r#"{"clear": "yes"}"#).is_err());
    }

    #[test]
    fn alt_and_legend_have_ceilings_and_they_are_separate_ones() {
        let long_legend = FeaturedChanges {
            legend: Some("x".repeat(MAX_LEGEND + 1)),
            ..FeaturedChanges::default()
        };
        assert!(validate_changes(&long_legend).is_err());

        let long_alt = FeaturedChanges {
            media_id: Some(Uuid::new_v4()),
            alt: Some("x".repeat(MAX_ALT + 1)),
            ..FeaturedChanges::default()
        };
        assert!(validate_changes(&long_alt).is_err());

        let at_the_limit = FeaturedChanges {
            media_id: Some(Uuid::new_v4()),
            alt: Some("x".repeat(MAX_ALT)),
            ..FeaturedChanges::default()
        };
        assert!(validate_changes(&at_the_limit).is_ok(), "the ceiling itself is allowed");
    }
}
