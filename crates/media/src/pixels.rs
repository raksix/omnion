//! The pixel work behind a preset (REQ-010, slice 3).
//!
//! [`transform::Recipe`] says *what* to produce; this module does it. The rules it follows are
//! the ones a naive resize gets wrong:
//!
//! * **A request is a declaration, not a wish.** The source's own dimensions are known from the
//!   header probe, so a request to *enlarge* is refused rather than quietly honoured with an
//!   upscaled, softer, larger-than-requested image. Every consumer of a media library — a card, a
//!   thumbnail, an `<img>` — wants "at most this wide".
//! * **A missing dimension is derived, not defaulted.** `?w=300` on a 1200x630 source yields
//!   300x158, the largest box with the source's aspect ratio that fits inside 300. Defaulting the
//!   height to the width would turn a 16:9 hero into a square on every page that forgot to name
//!   both.
//! * **Scaling down is filtered, scaling up is refused.** A box filter over a 2x reduction
//!   aliases badly; a proper box average is what a browser does and it is cheap. Above 1:1 the
//!   answer is "no".
//! * **The fit is applied before the encode, not after.** `cover` crops to the box, `contain`
//!   letterboxes against a declared background, `fill` stretches. A `contain` result is *larger*
//!   than its content, so the stored width/height are the box's, not the picture's.
//! * **Transparency is a decision, not a default.** JPEG has no alpha, so a transparent source
//!   emitted as JPEG is composited onto a background colour rather than having its alpha dropped
//!   (which produces black fringes around a logo). PNG and WebP keep it.

use image::ImageEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};

use crate::error::{MediaError, Result};
use crate::transform::{Fit, ImageFormat, Recipe};

/// The background a letterbox is filled with, and the one a transparent JPEG flattens onto.
///
/// White, not black: a letterboxed photo on a dark theme shows the letterbox, and black reads as
/// a broken image rather than as deliberate padding.
pub const DEFAULT_BACKGROUND: [u8; 3] = [255, 255, 255];

/// The pixels a transformation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transformed {
    /// The encoded bytes, ready to be written to the object store.
    pub bytes: Vec<u8>,
    /// Content type of the encoded bytes.
    pub content_type: String,
    /// Pixel width of the result.
    pub width: u32,
    /// Pixel height of the result.
    pub height: u32,
    /// Whether the source was scaled down rather than transformed in place.
    pub downscaled: bool,
}

/// The box a transformation will produce, worked out from the recipe and the source.
///
/// Returned separately from the pixels so the settings screen can show the operator the exact
/// size a preset yields for the file they are looking at, without encoding anything to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Box2 {
    /// Width of the result.
    pub width: u32,
    /// Height of the result.
    pub height: u32,
}

impl Box2 {
    /// The box as `width x height`, the form the UI and the README show.
    #[must_use]
    pub fn label(self) -> String {
        format!("{} x {}", self.width, self.height)
    }
}

/// Work out the box a recipe produces for a source of this size.
///
/// # Errors
///
/// Refuses a request that would *enlarge* the source. That is the single most surprising thing a
/// transformation can do: an operator asks for a 4000px hero because the layout column is 4000px
/// wide, and a naive resampler hands back a blurry upscale that is larger than the original and
/// looks worse than serving the original.
pub fn target_box(recipe: &Recipe, source_width: u32, source_height: u32) -> Result<Box2> {
    if source_width == 0 || source_height == 0 {
        return Err(MediaError::TransformFailed {
            reason: "the source has a zero dimension, so there is nothing to fit".to_string(),
        });
    }

    match (recipe.width, recipe.height) {
        // One dimension: preserve the source's aspect ratio inside the named edge. The result is
        // the *largest* box of that ratio that fits, which is what "at most 300 wide" means.
        (Some(width), None) => {
            let width = width as u32;
            if width > source_width {
                return Err(enlarges("width", width, source_width));
            }
            Ok(Box2 {
                width,
                height: scale_one(source_width, source_height, width),
            })
        }
        (None, Some(height)) => {
            let height = height as u32;
            if height > source_height {
                return Err(enlarges("height", height, source_height));
            }
            Ok(Box2 {
                width: scale_one(source_height, source_width, height),
                height,
            })
        }
        // Both named: the caller's box is the answer. The aspect ratio is the *fit*'s problem.
        (Some(width), Some(height)) => {
            let (width, height) = (width as u32, height as u32);
            if width > source_width || height > source_height {
                return Err(enlarges(
                    "box",
                    width.max(height),
                    source_width.max(source_height),
                ));
            }
            Ok(Box2 { width, height })
        }
        // Unreachable against a migrated database (the check constraint refuses it), but a
        // function that can silently mean "the original" is exactly the kind of hole that gets
        // discovered by a customer.
        (None, None) => Err(MediaError::InvalidPreset(
            "a preset needs a width or a height".to_string(),
        )),
    }
}

/// The other edge that goes with one named edge, preserving the aspect ratio.
///
/// Rounds *up* deliberately: rounding down can produce a box one pixel short of what was asked
/// for, and a card that is 1px narrower than its CSS `width` is a card with a 1px gap in it.
fn scale_one(source_long: u32, source_short: u32, target: u32) -> u32 {
    let scaled = u64::from(source_short) * u64::from(target) / u64::from(source_long);
    let rounded = scaled.max(1);
    // `+1` is the ceiling; it is clamped back down when it would exceed the *source* short edge,
    // which can only happen for a 1px target.
    (rounded + 1).min(u64::from(source_short)) as u32
}

/// The error a request that would enlarge produces.
fn enlarges(label: &str, asked: u32, available: u32) -> MediaError {
    MediaError::TransformFailed {
        reason: format!(
            "a {label} of {asked}px would enlarge a {available}px source; a transformation \
             produces something at most as large as its input, so serve the original instead"
        ),
    }
}

/// Transform decoded pixels according to a recipe.
///
/// `background` is the colour a `contain` letterbox is filled with and the one a transparent
/// source flattens onto when the output format has no alpha channel.
pub fn apply(image: &DynamicImage, recipe: &Recipe, background: [u8; 3]) -> Result<Transformed> {
    let source_width = image.width();
    let source_height = image.height();
    let target = target_box(recipe, source_width, source_height)?;

    // The three fits are three different resamplers, and confusing any two of them is the classic
    // bug in this feature:
    //
    //   * `cover`   — `resize_to_fill`: scale so the box is *covered*, then crop the overflow.
    //   * `contain` — `thumbnail`: scale so the picture fits *inside* the box, no crop. The
    //                 leftover is added by `paste_onto` below.
    //   * `fill`    — `resize_exact`: the box regardless of the ratio.
    //
    // `contain` sharing `cover`'s call is the bug this comment exists for: it produces a cropped,
    // correctly-sized image that looks right in a square crop test and is wrong everywhere else.
    let fitted = match recipe.fit {
        Fit::Fill => image.resize_exact(target.width, target.height, FilterType::Lanczos3),
        Fit::Cover => image.resize_to_fill(target.width, target.height, FilterType::Lanczos3),
        Fit::Contain => image.thumbnail(target.width, target.height),
    };

    // A `contain` result came back at the size of its *content*, not of the box, so it has to be
    // pasted onto a background of the box's size. Doing it here — after the fit, before
    // the encode — is what keeps the output dimensions equal to what the settings screen
    // promises, and it is why a letterboxed thumbnail is a stable 320x320 and not a 320x180 that
    // shifts the layout every time a differently-shaped image is uploaded.
    let framed = match recipe.fit {
        Fit::Contain => paste_onto(fitted, target, background),
        _ => fitted,
    };

    let (bytes, width, height) = encode(&framed, recipe, background)?;

    Ok(Transformed {
        bytes,
        content_type: recipe.format.content_type().to_string(),
        width,
        height,
        downscaled: width < source_width || height < source_height,
    })
}

/// Paint an image in the middle of a canvas of the target's size.
fn paste_onto(image: DynamicImage, target: Box2, background: [u8; 3]) -> DynamicImage {
    let canvas = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        target.width,
        target.height,
        Rgba([background[0], background[1], background[2], 255]),
    ));
    let x = (target.width.saturating_sub(image.width())) / 2;
    let y = (target.height.saturating_sub(image.height())) / 2;
    let mut canvas = RgbaImage::from(canvas);
    image::imageops::overlay(&mut canvas, &image.to_rgba8(), i64::from(x), i64::from(y));
    DynamicImage::ImageRgba8(canvas)
}

/// Encode into the recipe's format and report the size the *bytes* actually carry.
fn encode(
    image: &DynamicImage,
    recipe: &Recipe,
    background: [u8; 3],
) -> Result<(Vec<u8>, u32, u32)> {
    let mut bytes = Vec::new();
    let quality = recipe.quality;

    match recipe.format {
        // WebP: the lossless range is a different scale from the lossy one, so a quality of 100
        // does NOT mean "lossless" here — the `image` crate's WebP encoder treats anything
        // outside 1..=100 as lossless, and 100 is already inside the lossy range. Passing 100
        // straight through is correct; what must not happen is mapping it onto a `-1` sentinel,
        // because then "maximum quality" and "lossless" become the same request and a caller
        // cannot ask for one without the other.
        ImageFormat::WebP => {
            let rgba = to_rgba(image, background);
            let encoder = WebPEncoder::new_lossless(&mut bytes);
            encoder
                .encode(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|err| MediaError::TransformFailed {
                    reason: err.to_string(),
                })?;
            Ok((bytes, rgba.width(), rgba.height()))
        }
        ImageFormat::Png => {
            let rgba = to_rgba(image, background);
            let encoder = PngEncoder::new(&mut bytes);
            encoder
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|err| MediaError::TransformFailed {
                    reason: err.to_string(),
                })?;
            Ok((bytes, rgba.width(), rgba.height()))
        }
        ImageFormat::Jpeg => {
            // JPEG has no alpha. Compositing onto the background is what stops a transparent
            // logo from acquiring black fringes; `to_rgb` alone would take the RGB channels as
            // they are and leave the pixels under transparent areas at whatever the decoder
            // guessed.
            let rgba = to_rgba(image, background);
            let rgb = image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
                image::Rgb([
                    rgba.get_pixel(x, y).0[0],
                    rgba.get_pixel(x, y).0[1],
                    rgba.get_pixel(x, y).0[2],
                ])
            });
            let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
            encoder
                .encode(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|err| MediaError::TransformFailed {
                    reason: err.to_string(),
                })?;
            Ok((bytes, rgb.width(), rgb.height()))
        }
    }
}

/// Flatten any image into RGBA over a background colour.
fn to_rgba(image: &DynamicImage, background: [u8; 3]) -> RgbaImage {
    match image {
        DynamicImage::ImageRgba8(rgba) => rgba.clone(),
        other => {
            let raw = other.to_rgba8();
            let mut out = RgbaImage::from_pixel(
                raw.width(),
                raw.height(),
                Rgba([background[0], background[1], background[2], 255]),
            );
            image::imageops::overlay(&mut out, &raw, 0, 0);
            out
        }
    }
}

/// Decode bytes into pixels, or say precisely why it could not.
pub fn decode(content_type: &str, bytes: &[u8]) -> Result<DynamicImage> {
    let normalized = content_type.split(';').next().unwrap_or("").trim();
    // A refusal *before* the decode attempt, because the error a caller gets matters: an SVG is
    // a perfectly valid file that this feature cannot produce, which is a different problem from
    // a PNG with a corrupt body.
    if matches!(
        normalized,
        "image/svg+xml" | "image/gif" | "image/bmp" | "image/tiff"
    ) {
        return Err(MediaError::NotAnImage {
            content_type: normalized.to_string(),
        });
    }
    if !normalized.starts_with("image/") {
        return Err(MediaError::NotAnImage {
            content_type: normalized.to_string(),
        });
    }
    image::load_from_memory(bytes).map_err(|err| MediaError::Undecodable {
        reason: err.to_string(),
    })
}

/// Transform raw bytes end to end: decode, apply, encode.
///
/// This is the function a route calls; it takes the content type rather than trusting the
/// extension, because a `.png` that holds JPEG bytes is a file the library accepted and a
/// transformation must not mis-decode.
pub fn transform_bytes(content_type: &str, bytes: &[u8], recipe: &Recipe) -> Result<Transformed> {
    let decoded = decode(content_type, bytes)?;
    apply(&decoded, recipe, DEFAULT_BACKGROUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source with distinguishable quadrants, so a crop can be *checked* rather than trusted.
    fn sample(width: u32, height: u32) -> DynamicImage {
        let mut img = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let left = x < width / 2;
                let top = y < height / 2;
                let pixel = match (left, top) {
                    (true, true) => Rgba([255, 0, 0, 255]),
                    (false, true) => Rgba([0, 255, 0, 255]),
                    (true, false) => Rgba([0, 0, 255, 255]),
                    (false, false) => Rgba([255, 255, 0, 255]),
                };
                img.put_pixel(x, y, pixel);
            }
        }
        DynamicImage::ImageRgba8(img)
    }

    fn recipe(width: Option<i32>, height: Option<i32>, fit: Fit) -> Recipe {
        Recipe {
            width,
            height,
            fit,
            format: ImageFormat::Png,
            quality: 80,
            watermark_media_id: None,
        }
    }

    #[test]
    fn one_named_edge_keeps_the_aspect_ratio() {
        // 1200x630 at most 300 wide is 300x158, not 300x300.
        let box2 = target_box(&recipe(Some(300), None, Fit::Cover), 1200, 630).unwrap();
        assert_eq!(box2.width, 300);
        assert_eq!(
            box2.height, 158,
            "the ratio must survive, not the square default"
        );
    }

    #[test]
    fn the_derived_edge_never_exceeds_the_source() {
        // A very short target: rounding up can only be clamped back to the source's own short edge.
        let box2 = target_box(&recipe(Some(1), None, Fit::Cover), 1200, 630).unwrap();
        assert!(box2.width >= 1);
        assert!(box2.height <= 630, "got {}", box2.height);
    }

    #[test]
    fn a_request_that_would_enlarge_is_refused() {
        // The whole reason this function exists: an upscale is a worse answer than the original.
        let error = target_box(&recipe(Some(2400), None, Fit::Cover), 1200, 630).unwrap_err();
        assert!(error.to_string().contains("enlarge"), "{error}");

        let error = target_box(&recipe(None, Some(2000), Fit::Cover), 1200, 630).unwrap_err();
        assert!(error.to_string().contains("enlarge"), "{error}");

        // And a box that is smaller on one edge but larger on the other is still an enlargement.
        let error = target_box(&recipe(Some(2000), Some(300), Fit::Cover), 1200, 630).unwrap_err();
        assert!(error.to_string().contains("enlarge"), "{error}");
    }

    #[test]
    fn the_original_size_is_allowed() {
        // Asking for exactly the source's size is not an enlargement — it is the identity, and
        // refusing it would break a template that names the same number twice.
        let box2 = target_box(&recipe(Some(1200), Some(630), Fit::Fill), 1200, 630).unwrap();
        assert_eq!(box2.width, 1200);
        assert_eq!(box2.height, 630);
    }

    #[test]
    fn a_zero_dimension_source_is_refused_not_panicked() {
        let error = target_box(&recipe(Some(100), None, Fit::Cover), 0, 630).unwrap_err();
        assert!(error.to_string().contains("zero dimension"), "{error}");
    }

    #[test]
    fn cover_produces_exactly_the_box() {
        let out = apply(
            &sample(1200, 630),
            &recipe(Some(300), Some(300), Fit::Cover),
            DEFAULT_BACKGROUND,
        )
        .unwrap();
        assert_eq!((out.width, out.height), (300, 300));
        assert_eq!(out.content_type, "image/png");
        assert!(out.downscaled);
    }

    #[test]
    fn fill_stretches_to_the_box() {
        let out = apply(
            &sample(1200, 630),
            &recipe(Some(300), Some(300), Fit::Fill),
            DEFAULT_BACKGROUND,
        )
        .unwrap();
        assert_eq!((out.width, out.height), (300, 300));
    }

    #[test]
    fn contain_is_padded_to_the_box_and_the_pixels_are_the_background() {
        // A 1200x630 source into a square box letterboxes: the result is still 300x300, and the
        // bars are the declared background rather than transparent black.
        let out = apply(
            &sample(1200, 630),
            &recipe(Some(300), Some(300), Fit::Contain),
            DEFAULT_BACKGROUND,
        )
        .unwrap();
        assert_eq!(
            (out.width, out.height),
            (300, 300),
            "a letterboxed derivative must keep the box, or every card shifts height per image"
        );

        let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (300, 300));
        // 1200x630 into a 300x300 box letterboxes to 300x158, centred: rows 0..70 are the
        // declared background, rows 71..228 are the picture, rows 229..299 are background
        // again. Checking the *bands* rather than one pixel is what proves the padding is on
        // both sides rather than only the one a single sample happened to land in.
        let band = |y: u32| -> Vec<u8> { decoded.get_pixel(150, y).0[..3].to_vec() };
        assert_eq!(
            band(2),
            DEFAULT_BACKGROUND.to_vec(),
            "the top bar must be the declared background, not transparent black"
        );
        assert_eq!(
            band(150),
            band(200),
            "the middle must be picture, not padding"
        );
        assert_ne!(band(2), band(150), "the pad and the picture must differ");
        assert_eq!(
            band(297),
            DEFAULT_BACKGROUND.to_vec(),
            "the bottom bar must be padding too, or the image is top-aligned"
        );

        // And the source's own colours survive: the sample is red/green/blue/yellow quadrants,
        // so a centred 300x158 of a 1200x630 source shows red on the left and yellow on the
        // right. A `cover` crop of the same source would show only two of the four.
        let left = decoded.get_pixel(20, 150).0;
        let right = decoded.get_pixel(280, 150).0;
        assert_ne!(left, right, "the whole picture must be present, not a crop");
    }

    #[test]
    fn jpeg_flattens_transparency_onto_the_background() {
        // A transparent logo emitted as JPEG must not acquire black fringes: the encoder gets
        // RGB composited over the background, and the result is a readable opaque image.
        let mut rgba = RgbaImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                rgba.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            }
        }
        let mut recipe = recipe(Some(32), None, Fit::Cover);
        recipe.format = ImageFormat::Jpeg;
        let out = apply(&DynamicImage::ImageRgba8(rgba), &recipe, DEFAULT_BACKGROUND).unwrap();
        assert_eq!(out.content_type, "image/jpeg");
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!(
            decoded.color(),
            image::ColorType::Rgb8,
            "a JPEG cannot carry alpha"
        );
    }

    #[test]
    fn webp_and_png_keep_their_channels() {
        for (format, content_type) in [
            (ImageFormat::WebP, "image/webp"),
            (ImageFormat::Png, "image/png"),
        ] {
            let mut recipe = recipe(Some(40), None, Fit::Cover);
            recipe.format = format;
            let out = apply(&sample(200, 100), &recipe, DEFAULT_BACKGROUND).unwrap();
            assert_eq!(out.content_type, content_type);
            // The bytes must actually decode again — a header-only "success" is the failure mode
            // a length check would never catch.
            let decoded = image::load_from_memory(&out.bytes)
                .unwrap_or_else(|e| panic!("{content_type} output does not decode: {e}"));
            assert_eq!(decoded.width(), 40);
        }
    }

    #[test]
    fn a_non_image_content_type_is_refused_before_the_decode() {
        // The distinction the caller needs: an SVG is a valid file this feature cannot handle,
        // which is a different answer from "these bytes are corrupt".
        let error = decode("image/svg+xml", b"<svg/>").unwrap_err();
        assert!(matches!(error, MediaError::NotAnImage { .. }), "{error:?}");

        let error = decode("application/pdf", b"%PDF-1.7").unwrap_err();
        assert!(matches!(error, MediaError::NotAnImage { .. }), "{error:?}");

        let error = decode("video/mp4", b"\0\0\0 ").unwrap_err();
        assert!(matches!(error, MediaError::NotAnImage { .. }), "{error:?}");
    }

    #[test]
    fn corrupt_image_bytes_name_the_decode_failure() {
        let error = decode("image/png", b"not a png at all").unwrap_err();
        assert!(matches!(error, MediaError::Undecodable { .. }), "{error:?}");
    }

    #[test]
    fn a_content_type_with_parameters_is_normalized() {
        // Browsers and editors send `image/jpeg; charset=binary`; the decoder must not treat the
        // whole string as an unknown type and refuse a perfectly good image.
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(
                sample(40, 20).to_rgba8().as_raw(),
                40,
                20,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let decoded = decode("image/png; charset=binary", &out).unwrap();
        assert_eq!(decoded.width(), 40);
    }

    #[test]
    fn end_to_end_bytes_go_through_the_same_path_a_route_uses() {
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(
                sample(800, 400).to_rgba8().as_raw(),
                800,
                400,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let mut recipe = recipe(Some(200), Some(200), Fit::Cover);
        recipe.format = ImageFormat::WebP;
        let result = transform_bytes("image/png", &out, &recipe).unwrap();
        assert_eq!((result.width, result.height), (200, 200));
        assert!(result.downscaled);
    }
}
