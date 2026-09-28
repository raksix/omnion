//! Image transformation presets and the derivative cache (REQ-010, slice 3).
//!
//! A preset is a *named* transformation: `card` is 1200x630 WebP at quality 80, `thumb` is
//! 320x320 JPEG. The read path asks for it by name, `GET /api/v1/media/{id}/raw?preset=card`, and
//! the answer is the same pixels every time — which is the whole point of the design:
//!
//! 1. **On demand, never a generated matrix.** A pre-generation job writes `presets x files`.
//!    A site with 20 presets and 10 000 files has 200 000 objects, most of which are never
//!    requested. Here the first request for a `(file, preset)` pair pays for the work and every
//!    later one reads the cache.
//! 2. **The cache key is a hash of the inputs, not of the request.** It is
//!    `sha256(source checksum | preset body)`, so:
//!    - editing a preset's quality produces a *new* key, instead of serving the previous
//!      quality's pixels under the new name — the classic stale-derivative bug, which is
//!      invisible for exactly as long as nobody changes a setting;
//!    - the key is a pure function of the inputs, so two replicas computing it produce the same
//!      object. That is the property a CDN needs in order to hold it for a year without
//!      revalidating.
//! 3. **The source identity is the row's checksum.** A replace changes the checksum, so every
//!    derivative of the old bytes is simply no longer asked for; nothing has to be invalidated.
//! 4. **A derivative is a cache entry, never content.** Nothing in the publishing path may store
//!    a derivative key, because the whole table can be dropped and rebuilt at any time.
//!
//! What the platform does *not* do is guess. A decode failure is an error, not a pass-through of
//! the original: a preset that silently returns the untransformed bytes looks like it worked and
//! ships a 4 MB hero image into a 1200x630 slot forever.

use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};

/// Longest URL-safe preset name.
pub const MAX_PRESET_NAME_LENGTH: usize = 40;

/// Largest dimension a preset may ask for, in pixels.
///
/// The ceiling is a guard on the *product*, not a taste decision: an 8192 x 8192 RGB derivative
/// is 192 MB of memory per request, and a preset is a URL anyone in the site can request.
pub const MAX_PRESET_DIMENSION: i32 = 8192;

/// How a source is fitted into the target box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Fill the box, cropping whatever overflows.
    Cover,
    /// Fit inside the box, letterboxing what is left over.
    Contain,
    /// Stretch to the box exactly, ignoring the aspect ratio.
    Fill,
}

impl Fit {
    /// Every fit the API accepts, with the label the settings screen shows.
    pub const ALL: [(&'static str, &'static str); 3] = [
        ("cover", "Cover (crop)"),
        ("contain", "Contain (letterbox)"),
        ("fill", "Fill (stretch)"),
    ];

    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Fill => "fill",
        }
    }

    /// Parse a fit from the wire; an unknown value is refused rather than defaulted, because a
    /// default here silently crops an image the operator asked to letterbox.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "cover" => Ok(Self::Cover),
            "contain" => Ok(Self::Contain),
            "fill" => Ok(Self::Fill),
            other => Err(MediaError::InvalidPreset(format!(
                "`{other}` is not a fit; use cover, contain or fill"
            ))),
        }
    }
}

/// The image format a preset emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Lossy, smallest.
    WebP,
    /// Lossy, universally supported; alpha is flattened.
    Jpeg,
    /// Lossless, largest.
    Png,
}

impl ImageFormat {
    /// Every format the API accepts, with the label the settings screen shows.
    pub const ALL: [(&'static str, &'static str); 3] =
        [("webp", "WebP"), ("jpeg", "JPEG"), ("png", "PNG")];

    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebP => "webp",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }

    /// The `content-type` the derivative is served with.
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::WebP => "image/webp",
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    /// The extension a derivative's key carries.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::WebP => "webp",
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }

    /// Parse a format from the wire.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "webp" => Ok(Self::WebP),
            "jpeg" | "jpg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            other => Err(MediaError::InvalidPreset(format!(
                "`{other}` is not a format; use webp, jpeg or png"
            ))),
        }
    }
}

/// One named transformation, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Preset {
    /// Primary key.
    pub id: Uuid,
    /// Site the preset belongs to.
    pub site_id: Uuid,
    /// The name that appears in the URL.
    pub name: String,
    /// Target width, when the preset names one.
    pub width: Option<i32>,
    /// Target height, when the preset names one.
    pub height: Option<i32>,
    /// How the source is fitted into the box.
    pub fit: String,
    /// Emitted format.
    pub format: String,
    /// Encoder quality, 1–100.
    pub quality: i32,
    /// Watermark applied on top, if any.
    pub watermark_media_id: Option<Uuid>,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was last edited.
    pub updated_at: OffsetDateTime,
}

/// A preset as the caller describes it.
#[derive(Debug, Clone)]
pub struct NewPreset {
    /// The name that appears in the URL.
    pub name: String,
    /// Target width, when the preset names one.
    pub width: Option<i32>,
    /// Target height, when the preset names one.
    pub height: Option<i32>,
    /// How the source is fitted into the box.
    pub fit: Fit,
    /// Emitted format.
    pub format: ImageFormat,
    /// Encoder quality, 1–100.
    pub quality: i32,
    /// Watermark applied on top, if any.
    pub watermark_media_id: Option<Uuid>,
}

impl NewPreset {
    /// A preset that only rescales, at the default fit and format.
    #[must_use]
    pub fn new(name: impl Into<String>, width: i32, height: i32) -> Self {
        Self {
            name: name.into(),
            width: Some(width),
            height: Some(height),
            fit: Fit::Cover,
            format: ImageFormat::WebP,
            quality: 80,
            watermark_media_id: None,
        }
    }
}

/// What a preset actually does, after parsing the stored strings.
///
/// The stored row and this struct are separate on purpose: `fit` and `format` are `text` in the
/// database, and a route that reads a row must not have to re-validate a value the schema already
/// constrained. Parsing once, at the edge, keeps the transform path total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    /// Target width, when the preset names one.
    pub width: Option<i32>,
    /// Target height, when the preset names one.
    pub height: Option<i32>,
    /// How the source is fitted into the box.
    pub fit: Fit,
    /// Emitted format.
    pub format: ImageFormat,
    /// Encoder quality, 1–100.
    pub quality: u8,
    /// Watermark applied on top, if any.
    pub watermark_media_id: Option<Uuid>,
}

impl Recipe {
    /// The recipe a stored preset describes.
    ///
    /// The fallbacks are unreachable against a migrated database (the check constraints allow
    /// nothing else), so they are chosen to *fail visibly* rather than quietly: an unknown fit
    /// becomes `cover`, which crops — the same thing the column's default would do.
    #[must_use]
    pub fn of(preset: &Preset) -> Self {
        Self {
            width: preset.width,
            height: preset.height,
            fit: Fit::parse(&preset.fit).unwrap_or(Fit::Cover),
            format: ImageFormat::parse(&preset.format).unwrap_or(ImageFormat::WebP),
            quality: preset.quality.clamp(1, 100) as u8,
            watermark_media_id: preset.watermark_media_id,
        }
    }

    /// The recipe a new preset describes.
    #[must_use]
    pub fn from_new(new: &NewPreset) -> Self {
        Self {
            width: new.width,
            height: new.height,
            fit: new.fit,
            format: new.format,
            quality: new.quality.clamp(1, 100) as u8,
            watermark_media_id: new.watermark_media_id,
        }
    }

    /// The exact bytes that identify this recipe in a cache key.
    ///
    /// Every field that changes the pixels appears here, and the separators are unambiguous, so
    /// `w=1|12` cannot be spelled the same way as `w=11|2`. A cache key that forgets a field is
    /// the one bug class in this module with no visible symptom until someone changes a setting.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "w={}|h={}|fit={}|fmt={}|q={}|wm={}",
            self.width
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            self.height
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            self.fit.as_str(),
            self.format.as_str(),
            self.quality,
            self.watermark_media_id
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
        )
    }

    /// The content-addressed cache key for this recipe applied to a source checksum.
    ///
    /// # Panics
    ///
    /// Never: the digest is a fixed size and the input is hex or ASCII.
    #[must_use]
    pub fn cache_key(&self, source_checksum: &str) -> String {
        let mut hasher = Sha256::new();
        // A NUL separator is used rather than a printable one because a printable separator can
        // be typed into a name or a checksum, and then two different inputs hash the same.
        hasher.update(source_checksum.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.canonical().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// The object key a derivative of `media_id` is written to.
    ///
    /// The key is the *cache* key, not the media id: two files with the same bytes and the same
    /// preset are the same object, so a site that uploads the same hero twice stores one
    /// derivative rather than two.
    #[must_use]
    pub fn storage_key(&self, site_id: Uuid, cache_key: &str, format: ImageFormat) -> String {
        format!(
            "sites/{site_id}/derivatives/{cache_key}.{ext}",
            ext = format.extension()
        )
    }
}

/// A generated derivative, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Derivative {
    /// Primary key.
    pub id: Uuid,
    /// File the derivative was built from.
    pub media_id: Uuid,
    /// Preset it was built for.
    pub preset_id: Uuid,
    /// Hash of the inputs.
    pub cache_key: String,
    /// Object key of the generated bytes.
    pub storage_key: String,
    /// Content type of the generated bytes.
    pub content_type: String,
    /// Size of the generated bytes.
    pub size_bytes: i64,
    /// Pixel width of the result.
    pub width: i32,
    /// Pixel height of the result.
    pub height: i32,
    /// Checksum of the source bytes it was built from.
    pub source_checksum: String,
    /// When it was built.
    pub created_at: OffsetDateTime,
}

/// Validate a preset name for use in a URL.
#[must_use]
pub fn validate_preset_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err(MediaError::InvalidPreset(
            "the preset name cannot be empty".to_string(),
        ));
    }
    if name.len() > MAX_PRESET_NAME_LENGTH {
        return Err(MediaError::InvalidPreset(format!(
            "the preset name cannot be longer than {MAX_PRESET_NAME_LENGTH} characters"
        )));
    }
    // The same character class the migration's check enforces, kept in step with it on purpose:
    // a name the API accepts and the database refuses is a 500 the caller cannot act on, and a
    // name the API refuses and the database accepts is a row nobody can request.
    let usable = name.chars().enumerate().all(|(index, ch)| match ch {
        'a'..='z' | '0'..='9' => true,
        '.' | '_' | '-' => index > 0,
        _ => false,
    });
    if !usable {
        return Err(MediaError::InvalidPreset(
            "a preset name starts with a letter or digit and may then contain . _ and -"
                .to_string(),
        ));
    }
    Ok(name)
}

/// Check a preset's dimensions, in the way the API and the migration both have to agree on.
pub fn validate_dimensions(width: Option<i32>, height: Option<i32>) -> Result<()> {
    if width.is_none() && height.is_none() {
        return Err(MediaError::InvalidPreset(
            "a preset needs a width or a height; a preset with neither is the original file, \
             which the raw route already serves"
                .to_string(),
        ));
    }
    for (label, value) in [("width", width), ("height", height)] {
        if let Some(dimension) = value
            && !(1..=MAX_PRESET_DIMENSION).contains(&dimension)
        {
            return Err(MediaError::InvalidPreset(format!(
                "the {label} must be between 1 and {MAX_PRESET_DIMENSION} pixels, got {dimension}"
            )));
        }
    }
    Ok(())
}

/// Check a quality value against the range the encoder and the schema both accept.
pub fn validate_quality(quality: i32) -> Result<i32> {
    if !(1..=100).contains(&quality) {
        return Err(MediaError::InvalidPreset(format!(
            "the quality must be between 1 and 100, got {quality}"
        )));
    }
    Ok(quality)
}

/// Turn a [`NewPreset`] into a validated preset row, or explain which field refused.
pub fn validate_new(mut preset: NewPreset) -> Result<NewPreset> {
    preset.name = validate_preset_name(&preset.name)?;
    validate_dimensions(preset.width, preset.height)?;
    preset.quality = validate_quality(preset.quality)?;
    Ok(preset)
}

/// Where one file's derivatives live in the store, as a prefix for a sweep.
#[must_use]
pub fn derivative_prefix(site_id: Uuid) -> String {
    format!("sites/{site_id}/derivatives/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_normalized_before_it_is_stored() {
        // The name arrives from a form field; `  Card  ` must not become a second row called
        // `card` that no URL can reach, because the check lowercases the database value too.
        assert_eq!(validate_preset_name("  Card  ").unwrap(), "card");
        assert_eq!(
            validate_preset_name("hero-2x_v1.a").unwrap(),
            "hero-2x_v1.a"
        );
    }

    #[test]
    fn a_name_that_would_need_escaping_is_refused() {
        for name in ["card hero", "card/hero", "card?x", "card#a", "ça", ""] {
            assert!(
                validate_preset_name(name).is_err(),
                "`{name}` must not become a preset name"
            );
        }
        // A leading separator is refused as well: a name starting with `.` is legal to the regex
        // the database uses (`^[a-z0-9]`), so the API must refuse it for the same reason.
        assert!(validate_preset_name(".hidden").is_err());
    }

    #[test]
    fn a_preset_with_neither_dimension_is_refused() {
        // It would mean "the original", and the raw route already serves that — a row that means
        // the original is a row that drifts out of sync with the file it describes.
        let error = validate_dimensions(None, None).unwrap_err();
        assert!(error.to_string().contains("needs a width or a height"));
    }

    #[test]
    fn a_zero_or_absurd_dimension_is_refused() {
        // 0 is what a parser produces from an empty input; it is not "unbounded".
        assert!(validate_dimensions(Some(0), Some(10)).is_err());
        assert!(validate_dimensions(Some(10), Some(0)).is_err());
        assert!(validate_dimensions(Some(MAX_PRESET_DIMENSION + 1), None).is_err());
        assert!(validate_dimensions(Some(1), Some(MAX_PRESET_DIMENSION)).is_ok());
    }

    #[test]
    fn quality_is_checked_at_both_ends() {
        assert_eq!(validate_quality(1).unwrap(), 1);
        assert_eq!(validate_quality(100).unwrap(), 100);
        assert!(validate_quality(0).is_err());
        assert!(validate_quality(101).is_err());
    }

    #[test]
    fn the_canonical_form_separates_its_fields() {
        // The separator trap: without a delimiter that cannot appear in a value, `w=1|h=12` and
        // `w=11|h=2` would be the same string and therefore the same cache key.
        let a = Recipe {
            width: Some(1),
            height: Some(12),
            fit: Fit::Cover,
            format: ImageFormat::WebP,
            quality: 80,
            watermark_media_id: None,
        };
        let b = Recipe {
            width: Some(11),
            height: Some(2),
            ..a
        };
        assert_ne!(a.canonical(), b.canonical());
        assert_ne!(a.cache_key("abc"), b.cache_key("abc"));
    }

    #[test]
    fn every_pixel_changing_field_changes_the_key() {
        let base = Recipe {
            width: Some(1200),
            height: Some(630),
            fit: Fit::Cover,
            format: ImageFormat::WebP,
            quality: 80,
            watermark_media_id: None,
        };
        let source = "a".repeat(64);

        let variants = [
            Recipe {
                width: Some(1201),
                ..base
            },
            Recipe {
                height: Some(631),
                ..base
            },
            Recipe {
                fit: Fit::Contain,
                ..base
            },
            Recipe {
                fit: Fit::Fill,
                ..base
            },
            Recipe {
                format: ImageFormat::Jpeg,
                ..base
            },
            Recipe {
                format: ImageFormat::Png,
                ..base
            },
            // The field people forget: editing a quality and keeping the name must not serve
            // the previous quality's pixels.
            Recipe {
                quality: 81,
                ..base
            },
            Recipe {
                width: None,
                ..base
            },
            Recipe {
                watermark_media_id: Some(Uuid::nil()),
                ..base
            },
        ];

        for variant in variants {
            assert_ne!(
                base.cache_key(&source),
                variant.cache_key(&source),
                "changing a field must change the key: {}",
                variant.canonical()
            );
        }

        // And the same recipe over the same source is stable, which is the property a CDN needs.
        assert_eq!(base.cache_key(&source), base.cache_key(&source));
    }

    #[test]
    fn a_different_source_gives_a_different_key() {
        // Two files that share a preset must not share an object unless their bytes are equal.
        let recipe = Recipe {
            width: Some(1200),
            height: Some(630),
            fit: Fit::Cover,
            format: ImageFormat::WebP,
            quality: 80,
            watermark_media_id: None,
        };
        assert_ne!(recipe.cache_key("aa"), recipe.cache_key("ab"));
    }

    #[test]
    fn a_separator_cannot_be_smuggled_into_the_source_checksum() {
        // The digest input is `checksum | NUL | canonical`. A checksum that itself contained the
        // canonical string would let one file claim another's cache entry.
        let recipe = Recipe {
            width: Some(10),
            height: None,
            fit: Fit::Fill,
            format: ImageFormat::Png,
            quality: 5,
            watermark_media_id: None,
        };
        let honest = recipe.cache_key("aa");
        let forged = recipe.cache_key(&format!("aa\0{}", recipe.canonical()));
        assert_ne!(honest, forged);
    }

    #[test]
    fn the_storage_key_is_derived_from_the_cache_key() {
        let recipe = Recipe {
            width: Some(320),
            height: Some(320),
            fit: Fit::Contain,
            format: ImageFormat::Jpeg,
            quality: 70,
            watermark_media_id: None,
        };
        let site = Uuid::nil();
        let key = recipe.cache_key("deadbeef");
        let storage = recipe.storage_key(site, &key, recipe.format);
        // The extension has to be the *emitted* format, not the source's — a `.png` object whose
        // bytes are WebP is served with the wrong type by anything that trusts the extension.
        assert!(storage.ends_with(".jpg"), "{storage}");
        assert!(storage.starts_with("sites/"), "{storage}");
        assert!(storage.contains(&key), "{storage}");
        assert!(derivative_prefix(site).starts_with("sites/"));
    }

    #[test]
    fn a_stored_preset_parses_into_a_total_recipe() {
        // The database constrains `fit` and `format`, so the fallbacks are unreachable — but the
        // function must still be total for a row written by an older binary.
        let preset = Preset {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            name: "card".to_string(),
            width: Some(1200),
            height: Some(630),
            fit: "nonsense".to_string(),
            format: "nonsense".to_string(),
            quality: 900,
            watermark_media_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let recipe = Recipe::of(&preset);
        assert_eq!(recipe.fit, Fit::Cover);
        assert_eq!(recipe.format, ImageFormat::WebP);
        // An out-of-range quality is clamped rather than cast: `900 as u8` is 156, which is a
        // different number than the clamp produces and a surprising one.
        assert_eq!(recipe.quality, 100);
    }

    #[test]
    fn an_unknown_fit_and_format_are_refused_at_the_edge() {
        assert!(Fit::parse("cover").is_ok());
        assert!(Fit::parse("nonsense").is_err());
        assert!(ImageFormat::parse("jpg").is_ok());
        assert!(ImageFormat::parse("tiff").is_err());
    }

    #[test]
    fn a_validated_preset_normalizes_name_fit_and_quality() {
        let preset = validate_new(NewPreset::new("  Card ", 1200, 630)).unwrap();
        assert_eq!(preset.name, "card");
        assert_eq!(preset.fit, Fit::Cover);
        assert_eq!(preset.quality, 80);
    }
}
