//! Branding asset validation (REQ-062, slice 2).
//!
//! `theme_settings_revisions.branding` is a `jsonb` column -- `{ "logo": …, "logoDark": …,
//! "favicon": … }` -- and until this module it was the ONE section of a settings payload the
//! platform stored without looking at. Tokens, typography and layout are all walked by
//! `check_token_values`, because the renderer writes every string in them into a CSS custom
//! property; branding was not, and REQ-062's own criterion 9 asks for something the store
//! could not do:
//!
//! > Logo upload rejects files above the configured size and enforces the declared min/max
//! > dimensions with a field-level message.
//!
//! Two things follow from that sentence, and both are about WHERE the answer comes from.
//!
//! **A size limit cannot live in the payload.** `branding.logo` names a file that already
//! exists in the media library; the payload carries its id, not its bytes. So the size is read
//! from the `media` row and compared against a limit the *theme manifest* declares -- which is
//! what "the configured size" means here: a platform-wide cap plus a per-theme declaration, and
//! the tighter of the two is the one that refuses. A validator that only knew its own constant
//! would refuse a 3 MB logo on a theme that says `logoMaxBytes: 1048576`.
//!
//! **A dimension is a fact about the BYTES, and the bytes are not in the row.** `media` has no
//! width or height -- the probe result lands on `media_versions` (version 1, written by the
//! upload path from the same header read). So [`BrandingAsset`] carries the geometry the caller
//! read, and the shape of that argument is the point: a validator that took only `(size,
//! content_type)` would have had to either guess the dimensions or drop the half of the
//! criterion about min/max.
//!
//! ## Why the errors are per-field rather than one refusal
//!
//! `check_token_values` refuses the whole payload, and that is right for a token map: a value
//! containing a semicolon is not a value, and there is no partial answer. Branding is
//! different -- an operator uploading a 2 MB logo and fixing a favicon are two independent
//! mistakes, and a refusal that names only the first leaves them to fix it, resubmit, and
//! discover the second. [`validate_branding`] therefore returns **every** finding it found,
//! each naming the field it belongs to, and the caller decides what to do with the set. The
//! customize screen puts one message under the input that caused it, which is the "field-level
//! message" the criterion asks for.

use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{ContentError, Result};

/// The branding keys a theme may set, in the order the panel shows them.
///
/// Fixed rather than read from the payload: an unknown key in a token map is a typo to be
/// caught, and a branding key the renderer never reads is a setting that appears to be on.
pub const BRANDING_KEYS: [&str; 3] = ["logo", "logoDark", "favicon"];

/// Platform-wide ceiling for a branding asset, applied to every key.
///
/// Deliberately far below `omnion_media::MAX_UPLOAD_BYTES` (25 MB): the media limit answers
/// "will the library store this file", and this one answers "is this a logo". A 25 MB logo is
/// stored happily and then served in the header of every page.
pub const DEFAULT_BRANDING_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Platform-wide floor for a logo's width and height.
///
/// 16 px is the favicon floor from web conventions; below it the image cannot be legible in a
/// header, and the author is uploading a texture and calling it a logo.
pub const DEFAULT_BRANDING_MIN_PX: i32 = 16;

/// Platform-wide ceiling for a logo's width and height.
///
/// A 12 000 px logo is not a logo, it is a source file -- and it costs every visitor the header
/// bandwidth on every page while looking identical at every size the header uses it.
pub const DEFAULT_BRANDING_MAX_PX: i32 = 4_000;

/// The content types a branding key may hold.
///
/// SVG is absent on purpose and the exclusion is not about sanitisation: an SVG carries
/// script, and "the logo" is a header element that must not be able to run code. The remaining
/// four are raster formats, which is also the set whose dimensions [`BrandingAsset`] can check.
pub const BRANDING_CONTENT_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/avif"];

// ---------------------------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------------------------

/// What the limits are for one site/theme, after the manifest and the platform defaults meet.
///
/// Built by [`BrandingLimits::for_theme`]; every field has a platform default, so a theme that
/// declares nothing is still checked rather than unchecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrandingLimits {
    /// Largest accepted file, in bytes.
    pub max_bytes: u64,
    /// Smallest accepted width and height, in pixels.
    pub min_px: i32,
    /// Largest accepted width and height, in pixels.
    pub max_px: i32,
    /// Content types accepted, lowercased.
    pub content_types: [&'static str; 4],
}

impl Default for BrandingLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_BRANDING_MAX_BYTES,
            min_px: DEFAULT_BRANDING_MIN_PX,
            max_px: DEFAULT_BRANDING_MAX_PX,
            content_types: BRANDING_CONTENT_TYPES,
        }
    }
}

impl BrandingLimits {
    /// The platform defaults narrowed by whatever a theme manifest declares.
    ///
    /// A manifest's number is read as "this theme's own cap" and the **tighter** of it and the
    /// platform default wins, rather than the manifest overriding outright. An override would
    /// let a theme raise the platform ceiling for everybody who installs it, which is a
    /// property no single theme should have over a shared installation.
    ///
    /// A declaration that is not a positive number is IGNORED, and that is deliberate: the
    /// manifest validator owns the shape of a `settingsSchema` entry (tick 56 built it to
    /// refuse a bound on a text setting and a default outside its own range), so a second
    /// stricter parse here would only disagree with it in a way neither screen explains. Zero
    /// is the case that matters most -- read as a cap it would refuse every logo on the site.
    #[must_use]
    pub fn for_theme(manifest: &Value) -> Self {
        let declared = manifest.get("settingsSchema");
        let mut limits = Self::default();

        if let Some(max) = declared
            .and_then(|schema| schema.get("logoMaxBytes"))
            .and_then(positive_u64)
        {
            limits.max_bytes = limits.max_bytes.min(max);
        }
        if let Some(min) = declared
            .and_then(|schema| schema.get("logoMinPx"))
            .and_then(positive_i32)
        {
            limits.min_px = limits.min_px.max(min);
        }
        if let Some(max) = declared
            .and_then(|schema| schema.get("logoMaxPx"))
            .and_then(positive_i32)
        {
            limits.max_px = limits.max_px.min(max);
        }
        limits
    }
}

/// A manifest value that names a byte count, or `None` when it says nothing usable.
fn positive_u64(value: &Value) -> Option<u64> {
    value.as_u64().filter(|v| *v > 0)
}

/// A manifest value that names a pixel count, or `None` when it says nothing usable.
fn positive_i32(value: &Value) -> Option<i32> {
    value.as_i64().filter(|v| *v > 0).map(|v| v as i32)
}

// ---------------------------------------------------------------------------------------------
// The asset under test
// ---------------------------------------------------------------------------------------------

/// One branding reference, as the store resolved it against the media library.
///
/// The caller fills this in from the rows it read; the validator never touches a database, so
/// the rules below are unit-testable without one and the same rules serve a save, a publish and
/// a restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrandingAsset {
    /// The media id the payload named.
    pub media_id: Uuid,
    /// The file's stored size.
    pub size_bytes: i64,
    /// The declared content type, as the media row records it.
    pub content_type: String,
    /// Measured width, when the file's format carries one.
    pub width: Option<i32>,
    /// Measured height, when the file's format carries one.
    pub height: Option<i32>,
}

impl BrandingAsset {
    /// A reference to a file whose geometry the probe could not read.
    ///
    /// Separate from the normal constructor so a test that cares about size only does not have
    /// to invent dimensions, and so "we could not measure it" is a value rather than an
    /// `Option` around the whole asset.
    #[must_use]
    pub fn unmeasured(media_id: Uuid, size_bytes: i64, content_type: &str) -> Self {
        Self {
            media_id,
            size_bytes,
            content_type: content_type.to_owned(),
            width: None,
            height: None,
        }
    }
}

/// What is wrong with one branding key.
///
/// `#[serde(rename_all = "camelCase")]` because these go into the settings save response and
/// the customize screen reads them by field -- and a field name that differs by a letter between
/// the two is exactly the drift that once made the gallery read `undefined` (tick 57).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "code")]
pub enum BrandingFinding {
    /// The value is not a string or `null`, so it names no file at all.
    NotAnAsset {
        /// The branding key.
        field: String,
    },
    /// The value is not a uuid, so it cannot name a media row.
    NotAMediaId {
        /// The branding key.
        field: String,
    },
    /// The payload names a media id that is not in this site's library.
    UnknownAsset {
        /// The branding key.
        field: String,
        /// The id that named nothing.
        media_id: Uuid,
    },
    /// The file's type is not one a branding key accepts.
    WrongType {
        /// The branding key.
        field: String,
        /// The type the media row recorded.
        content_type: String,
        /// The types this key would have accepted.
        accepted: Vec<String>,
    },
    /// The file is larger than the effective limit.
    TooLarge {
        /// The branding key.
        field: String,
        /// The file's stored size.
        size_bytes: i64,
        /// The limit that refused it.
        limit: u64,
        /// The sentence to show.
        message: String,
    },
    /// A measured dimension is below the effective floor.
    TooSmall {
        /// The branding key.
        field: String,
        /// Measured width.
        width: i32,
        /// Measured height.
        height: i32,
        /// The floor that refused it.
        minimum: i32,
        /// The sentence to show.
        message: String,
    },
    /// A measured dimension is above the effective ceiling.
    TooLargePixel {
        /// The branding key.
        field: String,
        /// Measured width.
        width: i32,
        /// Measured height.
        height: i32,
        /// The ceiling that refused it.
        maximum: i32,
        /// The sentence to show.
        message: String,
    },
}

impl BrandingFinding {
    /// The branding key this finding belongs to, which is the field the panel labels it under.
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            Self::NotAnAsset { field }
            | Self::NotAMediaId { field }
            | Self::UnknownAsset { field, .. }
            | Self::WrongType { field, .. }
            | Self::TooLarge { field, .. }
            | Self::TooSmall { field, .. }
            | Self::TooLargePixel { field, .. } => field,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------------------------

/// Check a `branding` section against the limits and the files it names.
///
/// Returns **every** finding, not the first: the panel shows one message per field, and an
/// operator who fixes the logo and resubmits should not have to discover the favicon problem
/// next. Empty is the normal, passing answer -- `branding: {}` and `branding: null` both check
/// clean, because a site that has never set a logo has nothing to get wrong.
///
/// `resolved` is the caller's map from media id to what the library knows about it. A key that
/// names an id the caller did not resolve produces [`BrandingFinding::UnknownAsset`] rather
/// than being skipped, because a setting that points at nothing renders a broken header and
/// the operator has no other way to find out.
#[must_use]
pub fn validate_branding(
    branding: &Value,
    limits: &BrandingLimits,
    resolved: &HashMap<Uuid, BrandingAsset>,
) -> Vec<BrandingFinding> {
    let Some(object) = branding.as_object() else {
        // `null` and a non-object are already refused by `check_token_values` in the section
        // walk; a second refusal here would put two different messages on one field.
        return Vec::new();
    };

    let mut findings = Vec::new();

    for (field, value) in object {
        if !BRANDING_KEYS.contains(&field.as_str()) {
            continue;
        }
        // `null` clears the logo. That is the panel's own "no logo" state, not a finding.
        if value.is_null() {
            continue;
        }
        let Some(text) = value.as_str() else {
            findings.push(BrandingFinding::NotAnAsset {
                field: field.clone(),
            });
            continue;
        };
        let Ok(media_id) = Uuid::parse_str(text.trim()) else {
            findings.push(BrandingFinding::NotAMediaId {
                field: field.clone(),
            });
            continue;
        };
        let Some(asset) = resolved.get(&media_id) else {
            findings.push(BrandingFinding::UnknownAsset {
                field: field.clone(),
                media_id,
            });
            continue;
        };

        let normalized_type = asset.content_type.split(';').next().unwrap_or("").trim();
        if !limits
            .content_types
            .iter()
            .any(|accepted| accepted.eq_ignore_ascii_case(normalized_type))
        {
            findings.push(BrandingFinding::WrongType {
                field: field.clone(),
                content_type: normalized_type.to_owned(),
                accepted: limits
                    .content_types
                    .iter()
                    .map(|t| (*t).to_owned())
                    .collect(),
            });
            continue;
        }

        let size = asset.size_bytes.max(0) as u64;
        if size > limits.max_bytes {
            findings.push(BrandingFinding::TooLarge {
                field: field.clone(),
                size_bytes: asset.size_bytes,
                limit: limits.max_bytes,
                message: format!(
                    "{field} is {size} bytes, over the {} byte limit for a branding asset",
                    limits.max_bytes
                ),
            });
        }

        // Only dimensions the probe actually read are checked. A format that carries no
        // geometry (and a row whose version 1 predates the probe) yields `None`, and a
        // validator that refused those would make "the file is fine but we cannot measure it"
        // indistinguishable from "the file is wrong".
        if let (Some(width), Some(height)) = (asset.width, asset.height) {
            if width.min(height) < limits.min_px {
                findings.push(BrandingFinding::TooSmall {
                    field: field.clone(),
                    width,
                    height,
                    minimum: limits.min_px,
                    message: format!(
                        "{field} measures {width}x{height}; a branding asset must be at least \
                         {}px on its shorter side",
                        limits.min_px
                    ),
                });
            }
            if width.max(height) > limits.max_px {
                findings.push(BrandingFinding::TooLargePixel {
                    field: field.clone(),
                    width,
                    height,
                    maximum: limits.max_px,
                    message: format!(
                        "{field} measures {width}x{height}; a branding asset may be at most \
                         {}px on its longer side",
                        limits.max_px
                    ),
                });
            }
        }
    }

    findings
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// The message for one branding finding, as an owned string.
///
/// A finding carries the id and the numbers; this is the sentence a person reads. It is a free
/// function rather than a method because two of the variants build their text from their own
/// fields and three carry a message already formatted at the check site -- and **all five go
/// through here**, so a caller rendering a finding has exactly one place a message could come
/// from.
///
/// Owned rather than borrowed on purpose. A `&str` signature would have to build those sentences
/// somewhere with the caller's lifetime, and the only place available was a leak: an
/// `UnknownAsset` message is the one case that cannot be lifted off the finding it belongs to,
/// so a borrowed API forces `Box::leak` per render. This is a settings screen, opened and saved
/// repeatedly, and a validator that leaks on every keystroke is worse than the bug it was
/// written for.
#[must_use]
pub fn message_for(finding: &BrandingFinding) -> String {
    match finding {
        BrandingFinding::NotAnAsset { .. } => "This field takes a media id, or nothing.".to_owned(),
        BrandingFinding::NotAMediaId { .. } => {
            "This field takes a media id copied from the file library.".to_owned()
        }
        BrandingFinding::UnknownAsset { media_id, .. } => {
            // Names the id back, which is what the operator needs to find the file: it is the
            // only way to say WHICH reference is broken when two keys point at two different
            // files, and the panel lists each under its own key.
            format!("No file {media_id} in this site's library.")
        }
        BrandingFinding::WrongType {
            content_type,
            accepted,
            ..
        } => format!(
            "{content_type} is not a logo. Use one of: {}.",
            accepted.join(", ")
        ),
        BrandingFinding::TooLarge { message, .. }
        | BrandingFinding::TooSmall { message, .. }
        | BrandingFinding::TooLargePixel { message, .. } => message.clone(),
    }
}

/// The first message for a branding key, for a caller that shows one line per field.
///
/// Built from the first finding that names the field rather than from a match over the codes,
/// so the order is the order the checks ran in -- type, then size, then geometry -- and a
/// caller that shows one line shows the same one [`message_for`] would produce for that
/// finding.
#[must_use]
pub fn first_message_for(findings: &[BrandingFinding], field: &str) -> Option<String> {
    findings
        .iter()
        .find(|finding| finding.field() == field)
        .map(message_for)
}

/// Turn the first finding into a refusal, for a caller that stops at one.
///
/// The save path uses [`validate_branding`] and reports the whole set; this is for a caller that
/// wants the crate's own [`ContentError::InvalidField`] shape.
pub fn refuse_branding(findings: &[BrandingFinding]) -> Result<()> {
    match findings.first() {
        None => Ok(()),
        Some(finding) => Err(ContentError::InvalidField(format!(
            "{}: {}",
            finding.field(),
            message_for(finding)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn png(width: i32, height: i32) -> BrandingAsset {
        BrandingAsset {
            media_id: id(1),
            size_bytes: 4_000,
            content_type: "image/png".to_owned(),
            width: Some(width),
            height: Some(height),
        }
    }

    fn map(assets: &[BrandingAsset]) -> HashMap<Uuid, BrandingAsset> {
        assets.iter().map(|a| (a.media_id, a.clone())).collect()
    }

    fn empty_map() -> HashMap<Uuid, BrandingAsset> {
        HashMap::new()
    }

    #[test]
    fn an_empty_branding_section_has_nothing_to_get_wrong() {
        let limits = BrandingLimits::default();
        for value in [json!({}), Value::Null] {
            assert!(validate_branding(&value, &limits, &empty_map()).is_empty());
        }
    }

    #[test]
    fn a_measured_logo_inside_the_limits_passes() {
        let branding = json!({ "logo": id(1).to_string() });
        let findings = validate_branding(
            &branding,
            &BrandingLimits::default(),
            &map(&[png(400, 120)]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_logo_over_the_size_limit_is_refused_with_a_field_message() {
        let mut asset = png(400, 120);
        asset.size_bytes = 3 * 1024 * 1024;
        let branding = json!({ "logo": id(1).to_string() });
        let findings = validate_branding(&branding, &BrandingLimits::default(), &map(&[asset]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].field(), "logo");
        assert!(matches!(findings[0], BrandingFinding::TooLarge { .. }));
        assert!(
            first_message_for(&findings, "logo")
                .unwrap()
                .contains("byte limit")
        );
    }

    #[test]
    fn a_logo_under_the_floor_is_refused_on_its_shorter_side() {
        // 240x8 is below 16 on the shorter side while the longer side is comfortably legal,
        // which is the case a "max(width, height)" check would pass.
        let branding = json!({ "logo": id(1).to_string() });
        let findings =
            validate_branding(&branding, &BrandingLimits::default(), &map(&[png(240, 8)]));
        assert_eq!(findings.len(), 1);
        assert!(matches!(findings[0], BrandingFinding::TooSmall { .. }));
        assert!(
            first_message_for(&findings, "logo")
                .unwrap()
                .contains("shorter side")
        );
    }

    #[test]
    fn a_logo_over_the_ceiling_is_refused_on_its_longer_side() {
        let branding = json!({ "logo": id(1).to_string() });
        let findings = validate_branding(
            &branding,
            &BrandingLimits::default(),
            &map(&[png(9_000, 200)]),
        );
        assert_eq!(findings.len(), 1);
        assert!(matches!(findings[0], BrandingFinding::TooLargePixel { .. }));
        assert!(
            first_message_for(&findings, "logo")
                .unwrap()
                .contains("longer side")
        );
    }

    #[test]
    fn every_fault_is_reported_not_only_the_first() {
        let mut big = png(9_000, 200);
        big.size_bytes = 9 * 1024 * 1024;
        let wrong_type = BrandingAsset {
            media_id: id(2),
            size_bytes: 1_000,
            content_type: "image/svg+xml".to_owned(),
            width: Some(64),
            height: Some(64),
        };
        let branding = json!({
            "logo": id(1).to_string(),
            "logoDark": id(2).to_string(),
            "favicon": id(99).to_string(),
        });
        let findings = validate_branding(
            &branding,
            &BrandingLimits::default(),
            &map(&[big, wrong_type]),
        );
        // logo: too large AND too many pixels; logoDark: wrong type; favicon: not in the library.
        assert_eq!(findings.len(), 4, "{findings:?}");
        // Asserted as a COUNT PER FIELD rather than as a sequence, because the findings come
        // out in the jsonb object's own iteration order, not in the panel's key order: a test
        // that pins the sequence is pinning `serde_json`'s map implementation, and the same
        // findings would then "fail" on a build where the object iterates differently. What the
        // panel actually needs is that `logo` was reported TWICE — once for the bytes and once
        // for the pixels, which is the whole point of reporting every fault rather than
        // stopping at the first.
        for (field, expected) in [("logo", 2), ("logoDark", 1), ("favicon", 1)] {
            assert_eq!(
                findings.iter().filter(|f| f.field() == field).count(),
                expected,
                "{field} in {findings:?}"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_not_a_branding_key() {
        let branding = json!({ "notALogo": id(1).to_string() });
        assert!(
            validate_branding(
                &branding,
                &BrandingLimits::default(),
                &map(&[png(400, 120)])
            )
            .is_empty()
        );
    }

    #[test]
    fn a_null_clears_a_logo_and_is_not_a_fault() {
        let branding = json!({ "logo": null, "favicon": null });
        assert!(validate_branding(&branding, &BrandingLimits::default(), &empty_map()).is_empty());
    }

    #[test]
    fn a_value_that_is_not_a_media_id_is_refused_by_name() {
        let branding = json!({ "logo": "https://example.com/logo.png" });
        let findings = validate_branding(&branding, &BrandingLimits::default(), &empty_map());
        assert!(matches!(findings[0], BrandingFinding::NotAMediaId { .. }));
    }

    #[test]
    fn a_remote_url_is_not_the_same_as_a_bare_uuid_that_names_nothing() {
        // A bare uuid passes the shape check and is then refused for naming no file -- the two
        // steps, with two different messages, which is what lets the panel's own hint be true.
        let branding = json!({ "logo": "00000000-0000-0000-0000-000000000001" });
        let findings = validate_branding(&branding, &BrandingLimits::default(), &empty_map());
        assert!(matches!(findings[0], BrandingFinding::UnknownAsset { .. }));
        let message = first_message_for(&findings, "logo").unwrap();
        assert!(
            message.contains("00000000-0000-0000-0000-000000000001"),
            "{message}"
        );
    }

    #[test]
    fn an_svg_logo_is_refused_by_type_and_names_what_is_accepted() {
        let asset = BrandingAsset {
            media_id: id(3),
            size_bytes: 900,
            content_type: "image/svg+xml".to_owned(),
            width: None,
            height: None,
        };
        let branding = json!({ "logo": id(3).to_string() });
        let findings = validate_branding(&branding, &BrandingLimits::default(), &map(&[asset]));
        assert!(matches!(findings[0], BrandingFinding::WrongType { .. }));
        let message = first_message_for(&findings, "logo").unwrap();
        assert!(message.contains("image/png"), "{message}");
    }

    #[test]
    fn a_content_type_with_parameters_is_matched_on_its_type() {
        let mut asset = png(400, 120);
        asset.content_type = "image/png; charset=binary".to_owned();
        let branding = json!({ "logo": id(1).to_string() });
        assert!(
            validate_branding(&branding, &BrandingLimits::default(), &map(&[asset])).is_empty()
        );
    }

    #[test]
    fn a_file_whose_geometry_cannot_be_read_is_checked_for_size_only() {
        let asset = BrandingAsset::unmeasured(id(4), 4_000, "image/png");
        let branding = json!({ "logo": id(4).to_string() });
        assert!(
            validate_branding(&branding, &BrandingLimits::default(), &map(&[asset])).is_empty()
        );

        let huge = BrandingAsset::unmeasured(id(4), 9 * 1024 * 1024, "image/png");
        let findings = validate_branding(&branding, &BrandingLimits::default(), &map(&[huge]));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn a_manifest_cap_tightens_the_platform_default_and_never_raises_it() {
        let tight =
            BrandingLimits::for_theme(&json!({ "settingsSchema": { "logoMaxBytes": 1_048_576 } }));
        assert_eq!(tight.max_bytes, 1_048_576);

        // A theme that declares a LARGER cap does not raise the platform ceiling for everybody.
        let loose = BrandingLimits::for_theme(
            &json!({ "settingsSchema": { "logoMaxBytes": 900_000_000 } }),
        );
        assert_eq!(loose.max_bytes, DEFAULT_BRANDING_MAX_BYTES);

        // Same rule on pixels: a floor may be raised, a ceiling may be lowered.
        let raised = BrandingLimits::for_theme(&json!({ "settingsSchema": { "logoMinPx": 64 } }));
        assert_eq!(raised.min_px, 64);
        let lowered = BrandingLimits::for_theme(&json!({ "settingsSchema": { "logoMaxPx": 900 } }));
        assert_eq!(lowered.max_px, 900);
    }

    #[test]
    fn a_manifest_with_no_declarations_gets_the_platform_defaults() {
        for manifest in [json!({}), json!({ "settingsSchema": {} })] {
            assert_eq!(
                BrandingLimits::for_theme(&manifest),
                BrandingLimits::default()
            );
        }
    }

    #[test]
    fn a_nonsense_declaration_is_ignored_rather_than_guessed_at() {
        // Zero, a negative, a string and a float all say nothing usable, and each must leave the
        // platform default standing -- a `max_bytes` of 0 would refuse every logo on the site.
        for value in [json!(0), json!(-5), json!("2MB"), json!(1.5)] {
            let manifest = json!({ "settingsSchema": { "logoMaxBytes": value } });
            assert_eq!(
                BrandingLimits::for_theme(&manifest).max_bytes,
                DEFAULT_BRANDING_MAX_BYTES,
                "{value}"
            );
        }
    }

    #[test]
    fn the_declared_limit_is_the_one_that_refuses() {
        let mut asset = png(400, 120);
        asset.size_bytes = 1_500_000; // over the theme's 1 MiB cap, under the platform's 2 MiB.
        let branding = json!({ "logo": id(1).to_string() });
        let limits =
            BrandingLimits::for_theme(&json!({ "settingsSchema": { "logoMaxBytes": 1_048_576 } }));
        let findings = validate_branding(&branding, &limits, &map(&[asset.clone()]));
        assert_eq!(findings.len(), 1, "the declared cap must be what refuses");

        // And the same file passes when the theme declares nothing.
        assert!(
            validate_branding(&branding, &BrandingLimits::default(), &map(&[asset])).is_empty()
        );
    }

    #[test]
    fn a_refusal_carries_the_field_it_belongs_to() {
        let findings = vec![BrandingFinding::TooLarge {
            field: "logoDark".to_owned(),
            size_bytes: 9,
            limit: 4,
            message: "too big".to_owned(),
        }];
        let error = refuse_branding(&findings).expect_err("a finding must refuse");
        assert!(error.to_string().contains("logoDark"), "{error}");
        assert!(refuse_branding(&[]).is_ok());
    }
}
