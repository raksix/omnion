//! What a camera wrote about its own picture (REQ-010, slice 3).
//!
//! The probe in [`crate::probe`] answers "how big is this file"; EXIF answers "who took it, with
//! what, when". Both come out of the same byte prefix, and both answer "I do not know" rather than
//! guessing — a stored camera model that is off by one digit is a fact the panel repeats forever
//! to somebody comparing a catalogue against a shoot.
//!
//! Six decisions carry this reader, and each is a place the obvious shortcut produces a plausible
//! wrong answer:
//!
//! 1. **A TIFF header is not EXIF.** The IFD format is shared by TIFF itself, by GeoTIFF and by
//!    half a dozen camera makers' proprietary blocks. What makes the block EXIF is the
//!    `Exif\0\0` signature inside a JPEG `APP1` segment, an `EXIF` chunk in a WebP container or an
//!    `eXIf` chunk in a PNG — so the container is checked first and the TIFF walk only ever runs
//!    on bytes that arrived through one of those three doors.
//! 2. **Nothing is read from outside the prefix.** Every field's value may be an *offset* into the
//!    block, and an offset is attacker-controlled. A reader that indexes rather than asking
//!    `get(..)` reads adjacent heap on a crafted file; here every read is a range request whose
//!    failure is the answer "absent", which is also the only honest answer for a truncated upload.
//! 3. **A rational with a zero denominator is absent.** EXIF stores `1/200` s and `18/10` as two
//!    integers; a camera that wrote `1/0` (or a corrupt file) would otherwise divide by zero in
//!    the middle of an upload, and the file would not be describable at all because of one bad
//!    number. Zero, negative and non-finite results are refused the same way.
//! 4. **Orientation changes the box, not the file.** Values 5–8 mean the picture is stored
//!    sideways. Browsers rotate it themselves when they render, so a layout that reserved
//!    `width × height` for a portrait photo reserves the wrong box and every grid on the site
//!    shifts. [`oriented_size`] swaps the *reported* dimensions for those orientations while the
//!    raw orientation stays in the record, so a downloader that applies it does not rotate twice.
//! 5. **A GPS fix is a flag, never coordinates.** A photo library that quietly writes an
//!    operator's home address into a row that search, an API key and a share link can all read is
//!    a leak dressed as a feature. The record says `gps: true` and the panel says so out loud —
//!    "this picture carries a location, it is not stored" — rather than quietly dropping it.
//! 6. **A string is dropped, not mangled.** Camera makes are ASCII, but the field is a byte count
//!    and a corrupt file can hold anything. A sequence that is not valid UTF-8 is absent; writing
//!    the replacement characters would put `Canonï¿½` in a catalogue forever.

/// How many bytes the block reader will look at.
///
/// The same budget as the geometry probe: a camera writes its EXIF into the first few kilobytes,
/// and a thumbnail embedded in the IFD is the one field that can push a complete block to a few
/// tens of kilobytes. Anything past this is a file whose header is not the interesting part.
pub const EXIF_HEADER_BYTES: usize = crate::probe::HEADER_BYTES;

/// The largest value a single EXIF string field may store, in bytes.
///
/// The spec allows 32 767; a lens description is a sentence, and a block that carries a
/// megabyte of "text" is a file trying to fill a jsonb column.
pub const MAX_STRING_BYTES: usize = 512;

/// Orientation tag — how the stored pixels relate to what the camera saw.
pub const ORIENTATION_TAG: u16 = 0x0112;

/// What a camera recorded about the picture.
///
/// Deliberately not a mirror of the IFD: only the fields a person actually looks up in a media
/// library are read, and each of them is normalised on the way in so the panel can render a
/// fraction without knowing what a RATIONAL is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exif {
    /// Camera maker, as written (`Canon`, `Apple`).
    pub make: Option<String>,
    /// Camera model, as written (`Canon EOS R5`, `iPhone 12 Pro`).
    pub model: Option<String>,
    /// Lens, when the body recorded one.
    pub lens: Option<String>,
    /// ISO speed the shot was taken at.
    pub iso: Option<i32>,
    /// Exposure time in milliseconds, rounded.
    pub exposure_ms: Option<i32>,
    /// Aperture, hundredths of an f-stop (180 is f/1.8) — an integer so the record stays exact.
    pub aperture_x100: Option<i32>,
    /// Focal length in millimetres, rounded.
    pub focal_length_mm: Option<i32>,
    /// Orientation as stored, 1–8. Kept so a downloader can apply it and so the size swap is
    /// traceable rather than mysterious.
    pub orientation: Option<i32>,
    /// When the shutter fired, RFC 3339 without a zone (the format carries no offset).
    pub captured_at: Option<String>,
    /// The software that wrote the file.
    pub software: Option<String>,
    /// Whether the block carried a GPS fix. The coordinates themselves are deliberately not kept.
    pub gps: bool,
}

impl Exif {
    /// Whether the camera said nothing this reader understands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.make.is_none()
            && self.model.is_none()
            && self.lens.is_none()
            && self.iso.is_none()
            && self.exposure_ms.is_none()
            && self.aperture_x100.is_none()
            && self.focal_length_mm.is_none()
            && self.orientation.is_none()
            && self.captured_at.is_none()
            && self.software.is_none()
            && !self.gps
    }

    /// The aperture as the panel prints it, in f-stops with one decimal (`1.8`).
    #[must_use]
    pub fn aperture(&self) -> Option<f32> {
        self.aperture_x100.map(|value| value as f32 / 100.0)
    }

    /// The exposure as the panel prints it (`1/200 s`, or `0.8 s` when it is long).
    #[must_use]
    pub fn exposure(&self) -> Option<String> {
        let millis = self.exposure_ms?;
        if millis <= 0 {
            return None;
        }
        if millis >= 1000 {
            return Some(format!("{:.1} s", millis as f32 / 1000.0));
        }
        // 1/200 s is 5 ms. Rounded to the nearest millisecond it stays exactly 5, and a
        // third-of-a-millisecond shutter does not exist on any camera anybody has.
        Some(format!("1/{} s", 1000 / millis))
    }

    /// One line summarising the shot, for the metadata tab.
    #[must_use]
    pub fn headline(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        let body = [self.make.as_deref(), self.model.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !body.is_empty() {
            parts.push(body);
        }
        if let Some(iso) = self.iso {
            parts.push(format!("ISO {iso}"));
        }
        if let Some(exposure) = self.exposure() {
            parts.push(exposure);
        }
        if let Some(aperture) = self.aperture() {
            parts.push(format!("f/{aperture:.1}"));
        }
        if let Some(focal) = self.focal_length_mm {
            parts.push(format!("{focal} mm"));
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join(" · "))
    }
}

/// The record as json, with a field written only when the camera actually said it.
///
/// Hand-written rather than derived because the crate has no `serde` dependency, and because an
/// absent field is a *decision* here: `{"iso": null}` stored in a jsonb column is a value somebody
/// will later filter on, while a missing key is honestly "the camera did not say".
#[must_use]
pub fn to_json(exif: &Exif) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in [
        ("make", &exif.make),
        ("model", &exif.model),
        ("lens", &exif.lens),
        ("software", &exif.software),
        ("captured_at", &exif.captured_at),
    ] {
        if let Some(text) = value {
            parts.push(format!("\"{key}\":{}", json_string(text)));
        }
    }
    for (key, value) in [
        ("iso", exif.iso),
        ("exposure_ms", exif.exposure_ms),
        ("aperture_x100", exif.aperture_x100),
        ("focal_length_mm", exif.focal_length_mm),
        ("orientation", exif.orientation),
    ] {
        if let Some(number) = value {
            parts.push(format!("\"{key}\":{number}"));
        }
    }
    if exif.gps {
        parts.push("\"gps\":true".to_owned());
    }
    if parts.is_empty() {
        return "{}".to_owned();
    }
    format!("{{{}}}", parts.join(","))
}

/// A JSON string literal, quotes and backslashes escaped and control characters dropped.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other if (other as u32) < 0x20 => {}
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// Read the EXIF block out of a byte prefix, if the format carries one.
///
/// The container is checked before any TIFF parsing, and the whole read is bounded: a file with
/// no EXIF, a file whose EXIF starts past the prefix, and a file whose EXIF block is malformed
/// halfway through all answer the same way — an empty record, never a panic and never a field
/// read from outside the bytes.
#[must_use]
pub fn read(content_type: &str, head: &[u8]) -> Exif {
    let block = match content_type {
        "image/jpeg" => jpeg_app1(head),
        "image/png" => chunk(head, b"eXIf"),
        "image/webp" => chunk(head, b"EXIF"),
        _ => None,
    };
    let Some(block) = block else {
        return Exif::default();
    };
    // The block starts at the TIFF header. A file that is shorter than the smallest legal header
    // is not a camera, it is a truncated upload.
    if block.len() < 8 {
        return Exif::default();
    }
    let mut out = Exif::default();
    read_tiff(block, &mut out);
    out
}

/// The TIFF block of a JPEG's first `APP1` segment that carries the EXIF signature.
///
/// Only the *first* one is read: a second `APP1` block is an XMP packet or a maker's private
/// data, and concatenating two TIFF streams and walking the result is how a reader ends up with
/// one camera's orientation and another camera's lens.
fn jpeg_app1(head: &[u8]) -> Option<&[u8]> {
    if head.len() < 4 || head[0] != 0xff || head[1] != 0xd8 {
        return None;
    }
    let mut offset = 2;
    while offset + 3 < head.len() {
        if head[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = head[offset + 1];
        offset += 2;
        match marker {
            // Start of scan: the entropy-coded data follows and no metadata after it is safe to
            // read by length, because the byte pattern `FF xx` occurs by chance inside it.
            0xda | 0xd9 => return None,
            0xd8 | 0x01 | 0xd0..=0xd7 => continue,
            0xe1 => {
                let length = be_u16(head, offset) as usize;
                // The length field counts itself. A segment that claims less than its own header,
                // or more than the prefix holds, is not a segment this reader can use.
                if length < 8 || offset + length > head.len() {
                    return None;
                }
                let payload = &head[offset + 2..offset + length];
                return payload.strip_prefix(b"Exif\0\0");
            }
            _ => {
                if offset + 1 > head.len() {
                    return None;
                }
                let length = be_u16(head, offset) as usize;
                if length < 2 {
                    return None;
                }
                offset += length;
            }
        }
    }
    None
}

/// The payload of a RIFF (`WebP`) or PNG chunk, from the container's first chunk onwards.
fn chunk<'a>(head: &'a [u8], wanted: &[u8; 4]) -> Option<&'a [u8]> {
    if head.len() < 16 || !head.starts_with(b"RIFF") || &head[8..12] != b"WEBP" {
        return png_chunk(head, wanted);
    }
    // RIFF: `size` at 4 is the file length minus eight, and chunk headers are 8 bytes with the
    // payload padded to an even length. The bound is the prefix, not the declared size, so a
    // length field claiming a gigabyte cannot make this walk read past what it was given.
    let mut offset = 12;
    while offset + 8 <= head.len() {
        let id: [u8; 4] = [
            head[offset],
            head[offset + 1],
            head[offset + 2],
            head[offset + 3],
        ];
        let length = u32::from_le_bytes([
            head[offset + 4],
            head[offset + 5],
            head[offset + 6],
            head[offset + 7],
        ]) as usize;
        let start = offset + 8;
        let end = start.saturating_add(length);
        if start > head.len() {
            return None;
        }
        if &id == wanted {
            return Some(&head[start..end.min(head.len())]);
        }
        // Padding byte after an odd-sized chunk.
        offset = end + (length & 1);
    }
    None
}

/// The payload of a PNG `eXIf` chunk.
fn png_chunk<'a>(head: &'a [u8], wanted: &[u8; 4]) -> Option<&'a [u8]> {
    let signature = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if head.len() < 8 || head[..8] != signature {
        return None;
    }
    let mut offset = 8;
    while offset + 8 <= head.len() {
        let length = u32::from_be_bytes([
            head[offset],
            head[offset + 1],
            head[offset + 2],
            head[offset + 3],
        ]) as usize;
        let id: [u8; 4] = [
            head[offset + 4],
            head[offset + 5],
            head[offset + 6],
            head[offset + 7],
        ];
        let start = offset + 8;
        if start > head.len() {
            return None;
        }
        let end = start.saturating_add(length);
        if &id == wanted {
            return Some(&head[start..end.min(head.len())]);
        }
        // `IEND` is the last chunk; anything past it is padding.
        if &id == b"IEND" {
            return None;
        }
        // The 4-byte CRC is part of the chunk's own length field for this walk's purposes.
        offset = end.saturating_add(4);
    }
    None
}

/// Byte order of a TIFF block, and the block itself.
struct Tiff<'a> {
    bytes: &'a [u8],
    little: bool,
}

impl<'a> Tiff<'a> {
    /// A `u16` at `offset`, or `None` when the block does not reach it.
    fn u16_at(&self, offset: usize) -> Option<u16> {
        let slice = self.bytes.get(offset..offset.checked_add(2)?)?;
        Some(if self.little {
            u16::from_le_bytes([slice[0], slice[1]])
        } else {
            u16::from_be_bytes([slice[0], slice[1]])
        })
    }

    /// A `u32` at `offset`, or `None` when the block does not reach it.
    fn u32_at(&self, offset: usize) -> Option<u32> {
        let slice = self.bytes.get(offset..offset.checked_add(4)?)?;
        let raw = [slice[0], slice[1], slice[2], slice[3]];
        Some(if self.little {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    }
}

/// The byte width of one TIFF field type, or `None` for a type this reader does not know.
fn type_width(kind: u16) -> Option<usize> {
    Some(match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

/// Where an entry's value lives.
///
/// Four bytes or fewer live *inside* the entry; anything wider is an offset into the block. An
/// offset past the block is `None` rather than a panic, which is the whole reason this is a
/// function instead of an index expression — every field whose value is an offset is attacker
/// controlled, and a `u32` from a file is not a length anybody may trust.
fn value_of<'a>(tiff: &Tiff<'a>, entry: usize) -> Option<&'a [u8]> {
    let kind = tiff.u16_at(entry + 2)?;
    let count = tiff.u32_at(entry + 4)? as usize;
    let width = type_width(kind)?;
    // A count that would overflow is a corrupt block; a `count * width` that wraps to a small
    // number is a slice that reads the wrong bytes and looks plausible.
    let total = width.checked_mul(count)?;
    let start = if total <= 4 {
        entry + 8
    } else {
        tiff.u32_at(entry + 8)? as usize
    };
    let end = start.checked_add(total)?;
    tiff.bytes.get(start..end)
}

/// Read every recognised tag of one directory, and follow the sub-directory pointers.
fn read_ifd(tiff: &Tiff<'_>, offset: usize, depth: usize, out: &mut Exif) {
    // Two levels are the legal shape: IFD0 and the Exif sub-directory. A third level is a
    // malformed block, and following pointers without a bound is a cycle waiting to happen.
    if depth > 1 {
        return;
    }
    let Some(count) = tiff.u16_at(offset) else {
        return;
    };
    for index in 0..count as usize {
        // A directory whose entry table would run past the block is truncated, not malformed in
        // a way this reader can step over: the count field is attacker-controlled, so it is the
        // bound that decides how much of the block is even considered an entry.
        let Some(entry) = offset
            .checked_add(2)
            .and_then(|base| base.checked_add(index.checked_mul(12)?))
        else {
            return;
        };
        let Some(tag) = tiff.u16_at(entry) else {
            continue;
        };
        let bytes = value_of(tiff, entry);
        match tag {
            // The two directory pointers are read even when the directory they name is broken:
            // a GPS pointer is what sets the flag, and that is the whole point of the field.
            0x8769 => {
                if let Some(target) = tiff.u32_at(entry + 8) {
                    read_ifd(tiff, target as usize, depth + 1, out);
                }
            }
            0x8825 => out.gps = true,
            ORIENTATION_TAG => {
                out.orientation = bytes.and_then(|_| {
                    let value = first_u16(tiff, entry)?;
                    (1..=8).contains(&value).then_some(i32::from(value))
                })
            }
            // IFD0: the fields a camera writes without a sub-directory.
            0x010f => out.make = bytes.and_then(text),
            0x0110 => out.model = bytes.and_then(text),
            0x0131 => out.software = bytes.and_then(text),
            0x0132 => out.captured_at = bytes.and_then(text).and_then(|raw| datetime(&raw)),
            // The Exif sub-directory.
            0x9003 => out.captured_at = bytes.and_then(text).and_then(|raw| datetime(&raw)),
            0x829a => out.exposure_ms = bytes.and_then(|raw| rational_x1000(tiff, raw)),
            0x829d => out.aperture_x100 = bytes.and_then(|raw| rational_x100(tiff, raw)),
            0x8827 => {
                out.iso = bytes.and_then(|_| {
                    tiff.u16_at(entry + 8)
                        .filter(|_| count_is_one(tiff, entry))
                        .map(i32::from)
                })
            }
            0x920a => out.focal_length_mm = bytes.and_then(|raw| rational_round(tiff, raw)),
            0xa434 => out.lens = bytes.and_then(text),
            _ => {}
        }
    }
}

/// The single `u16` of an inline numeric entry.
fn first_u16(tiff: &Tiff<'_>, entry: usize) -> Option<u16> {
    if count_is_one(tiff, entry) {
        tiff.u16_at(entry + 8)
    } else {
        tiff.u32_at(entry + 8)
            .and_then(|wide| u16::try_from(wide).ok())
    }
}

/// Whether an entry holds exactly one value — a "the first one" read on a two-element list is
/// how `ISO 100, 200` becomes `ISO 100` and nobody notices.
fn count_is_one(tiff: &Tiff<'_>, entry: usize) -> bool {
    tiff.u32_at(entry + 4) == Some(1)
}

/// A rational, kept in thousandths so an exposure below one millisecond is still distinguishable.
///
/// A fraction is written with the denominator *larger* than the numerator far more often than not
/// — `1/200 s` is the normal shutter speed, not a corrupt block — so the only refusals are the
/// ones that would make the arithmetic wrong rather than merely small.
fn rational_x1000(tiff: &Tiff<'_>, raw: &[u8]) -> Option<i32> {
    let (num, den) = pair(tiff, raw)?;
    // A zero numerator is "unset"; a zero denominator is a division the block does not allow.
    if num == 0 || den == 0 {
        return None;
    }
    // An exposure is under a minute; anything longer is a camera that wrote a sentinel.
    if num > den * 3_600 {
        return None;
    }
    let millis = num.saturating_mul(1000) / den;
    i32::try_from(millis).ok().filter(|value| *value > 0)
}

/// A rational rounded to a whole unit (millimetres, hundredths of an f-stop).
fn rational_round(tiff: &Tiff<'_>, raw: &[u8]) -> Option<i32> {
    rational_x1000(tiff, raw)
        .map(|scaled| scaled.div_euclid(1000))
        .filter(|value| *value > 0)
}

/// A rational scaled to hundredths — an f-stop is never worth a thousandth.
///
/// The guard is a *ceiling*, not a floor: `f/1.8` is `18/10` and `f/0.95` is `95/100`, so the
/// numerator is allowed to exceed the denominator (a stop wider than f/1), and what is refused is
/// a value so small it rounds to nothing — which is what a "1/0" block produces.
fn rational_x100(tiff: &Tiff<'_>, raw: &[u8]) -> Option<i32> {
    let (num, den) = pair(tiff, raw)?;
    if num == 0 || den == 0 {
        return None;
    }
    // The widest stop anybody has a lens for is f/0.95; f/0.5 is a rounding artefact of a
    // corrupt block, and beyond f/128 the value is noise in a sensor.
    if num > den * 128 {
        return None;
    }
    i32::try_from(num.saturating_mul(100) / den)
        .ok()
        .filter(|value| *value > 0)
}

/// Numerator and denominator of a RATIONAL, in the entry's byte order.
fn pair(tiff: &Tiff<'_>, raw: &[u8]) -> Option<(u64, u64)> {
    if raw.len() < 8 {
        return None;
    }
    let words = [
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
    ];
    if tiff.little {
        Some((u64::from(words[0]), u64::from(words[1])))
    } else {
        Some((u64::from(words[1]), u64::from(words[0])))
    }
}

/// An ASCII field as a trimmed string, or `None` when it is not one.
fn text(raw: &[u8]) -> Option<String> {
    // The declared count includes the NUL terminator, and a corrupt block may not have one.
    let trimmed = match raw.iter().position(|byte| *byte == 0) {
        Some(end) => &raw[..end],
        None => raw,
    };
    let value = std::str::from_utf8(trimmed).ok()?.trim();
    if value.is_empty() || value.len() > MAX_STRING_BYTES {
        return None;
    }
    Some(value.to_owned())
}

/// `YYYY:MM:DD HH:MM:SS` as written by a camera, as RFC 3339 without a zone.
///
/// The digits are checked rather than trusted: a date is a field people sort by, and `"0000:00:00
/// 00:00:00"` from a body with no clock is not the epoch, it is nothing.
fn datetime(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.len() != 19 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !digits(0..4)
        || bytes[4] != b':'
        || !digits(5..7)
        || bytes[7] != b':'
        || !digits(8..10)
        || bytes[10] != b' '
        || !digits(11..13)
        || bytes[13] != b':'
        || !digits(14..16)
        || bytes[16] != b':'
        || !digits(17..19)
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| raw[range].parse::<i32>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || year < 1800
    {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

/// The size a layout should reserve, with EXIF orientation applied.
///
/// Orientations 5–8 store the picture rotated by a quarter turn. A browser rotates it when it
/// renders, so a grid that reserved the stored `width × height` for a portrait shot reserves the
/// wrong box and every image below it shifts; the swap is therefore applied to the *reported*
/// dimensions while [`Exif::orientation`] keeps the raw value, so nothing is rotated twice.
#[must_use]
pub fn oriented_size(
    width: Option<i32>,
    height: Option<i32>,
    orientation: Option<i32>,
) -> (Option<i32>, Option<i32>) {
    let rotated = matches!(orientation, Some(5..=8));
    if rotated {
        (height, width)
    } else {
        (width, height)
    }
}

/// A `u16` at `offset`, big-endian, or `0` when the prefix is too short.
fn be_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes
        .get(offset..offset.saturating_add(2))
        .map(|slice| u16::from_be_bytes([slice[0], slice[1]]))
        .unwrap_or(0)
}

/// Walk a TIFF block and fill `out` with everything this reader understands.
fn read_tiff(block: &[u8], out: &mut Exif) {
    let little = match &block[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return,
    };
    // 42 is the only legal TIFF magic. Without it this is not a TIFF block, and reading IFDs out
    // of arbitrary bytes is how a reader ends up with a "lens" of seven control characters.
    if block.get(2..4) != Some(&42u16.to_le_bytes())
        && block.get(2..4) != Some(&42u16.to_be_bytes())
    {
        return;
    }
    let tiff = Tiff {
        bytes: block,
        little,
    };
    let Some(first) = tiff.u32_at(4) else {
        return;
    };
    read_ifd(&tiff, first as usize, 0, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes before the first directory: `II`, the magic, and the offset of IFD0.
    const HEADER: usize = 8;

    /// The tag whose value is the offset of the Exif sub-directory.
    const SUBDIR_TAG: u16 = 0x8769;

    /// The tag whose value is the offset of the GPS directory.
    const GPS_TAG: u16 = 0x8825;

    /// Build a little-endian TIFF block out of named directories.
    ///
    /// The builder exists so the tests read like the format rather than like a hex dump: an
    /// offset past the end of the block is a one-line change here and an unreadable byte soup in
    /// the test body.
    struct Dir {
        entries: Vec<(u16, u16, Vec<u8>)>,
    }

    impl Dir {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        /// An ASCII field (type 2), NUL-terminated as a camera writes it.
        fn ascii(mut self, tag: u16, value: &str) -> Self {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            self.entries.push((tag, 2, bytes));
            self
        }

        /// A SHORT (type 3) held inline: one element, two bytes.
        fn short(mut self, tag: u16, value: u16) -> Self {
            self.entries.push((tag, 3, value.to_le_bytes().to_vec()));
            self
        }

        /// A RATIONAL (type 5): two little-endian `u32`s.
        fn rational(mut self, tag: u16, num: u32, den: u32) -> Self {
            let mut bytes = num.to_le_bytes().to_vec();
            bytes.extend_from_slice(&den.to_le_bytes());
            self.entries.push((tag, 5, bytes));
            self
        }

        /// A LONG (type 4) held inline: one element, four bytes.
        fn long(mut self, tag: u16, value: u32) -> Self {
            self.entries.push((tag, 4, value.to_le_bytes().to_vec()));
            self
        }

        /// A directory pointer, resolved once the layout is known.
        fn pointer(mut self, tag: u16) -> Self {
            self.entries.push((tag, 4, vec![0; 4]));
            self
        }
    }

    /// Serialise directories into one TIFF block.
    ///
    /// `sub` is the directory the root's `SUBDIR_TAG`/`GPS_TAG` pointer names. The two-pass shape
    /// is the format's own: a value too wide for its entry is an *offset*, and an offset is not
    /// known until the bytes it points at have been laid out.
    fn tiff(root: &Dir, sub: Option<&Dir>) -> Vec<u8> {
        // The header, then IFD0, whose offset is the constant `HEADER`.
        let mut out = vec![b'I', b'I', 42, 0, HEADER as u8, 0, 0, 0];
        let (root_bytes, pointer_at) = directory(root, HEADER);
        out.extend_from_slice(&root_bytes);

        if let Some(sub) = sub {
            // The sub-directory follows *everything* IFD0 owns — its table, its next-directory
            // pointer and its wide values — so an ASCII field and a sub-directory cannot overlap.
            let sub_base = out.len();
            if let Some(at) = pointer_at {
                out[HEADER + at..HEADER + at + 4].copy_from_slice(&(sub_base as u32).to_le_bytes());
            }
            // Every offset *inside* the sub-directory is measured from the start of the block, so
            // it is told where the block has got to.
            out.extend_from_slice(&directory(sub, sub_base).0);
        }
        out
    }

    /// How many elements a value of this type and byte length holds.
    ///
    /// ASCII and BYTE count bytes; the numeric types count values of their own width. Only those
    /// two families appear in the fields this builder writes.
    fn element_count(kind: u16, bytes: usize) -> u16 {
        let width = match kind {
            1 | 2 | 6 | 7 => 1,
            3 => 2,
            4 | 9 => 4,
            5 => 8,
            _ => 1,
        };
        u16::try_from(bytes / width).unwrap_or(1).max(1)
    }

    /// One directory as its own bytes, plus where its sub-directory pointer sits inside them.
    ///
    /// `block_base` is where this directory starts inside the TIFF block, because a value offset
    /// is measured from the *block* — a sub-directory's first wide value is not at its own zero.
    ///
    /// The wide values go *after* the table rather than inline, because an inline value would
    /// land on the next entry's bytes — a block the reader then refuses for reasons that look like
    /// a parser bug and are actually a builder that wrote the format wrong.
    fn directory(dir: &Dir, block_base: usize) -> (Vec<u8>, Option<usize>) {
        let mut out = u16::try_from(dir.entries.len())
            .expect("a test directory is not 65k entries")
            .to_le_bytes()
            .to_vec();
        // Where each too-wide value's bytes will land, filled as the table is written.
        let mut wide: Vec<(usize, &[u8])> = Vec::new();
        // Where the directory pointer's offset sits, to be resolved by the caller.
        let mut pointer_at = None;
        for (tag, kind, value) in &dir.entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            // The count is a number of *elements*, not of bytes: `ISO 400` is one SHORT, so its
            // count is 1 even though a SHORT is two bytes wide. Writing the byte length here is
            // the classic EXIF builder slip, and a reader that trusts it sees two ISO values.
            out.extend_from_slice(&(u32::from(element_count(*kind, value.len()))).to_le_bytes());
            if *tag == SUBDIR_TAG || *tag == GPS_TAG {
                pointer_at = Some(out.len());
                out.extend_from_slice(&[0; 4]);
            } else if value.len() <= 4 {
                let mut inline = value.clone();
                inline.resize(4, 0);
                out.extend_from_slice(&inline);
            } else {
                wide.push((out.len(), value));
                out.extend_from_slice(&[0; 4]);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes());

        // The wide values are appended in the order their entries appeared, so each one's offset
        // is the end of everything written so far plus what came before it.
        let mut written = 0;
        let mut here = block_base + out.len();
        for (at, value) in &wide {
            let target = (here + written) as u32;
            out[*at..at + 4].copy_from_slice(&target.to_le_bytes());
            out.extend_from_slice(value);
            written += value.len();
        }
        (out, pointer_at)
    }

    /// A JPEG whose `APP1` carries `payload`, in front of a `SOF0` frame.
    fn jpeg(app1_payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8];
        let length = (app1_payload.len() + 2) as u16;
        bytes.extend_from_slice(&[0xff, 0xe1]);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(app1_payload);
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&600u16.to_be_bytes());
        bytes.extend_from_slice(&800u16.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
        bytes
    }

    /// A JPEG whose `APP1` payload is the EXIF signature followed by a TIFF block.
    fn jpeg_exif(block: &[u8]) -> Vec<u8> {
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(block);
        jpeg(&payload)
    }

    #[test]
    fn a_file_with_no_exif_reads_as_nothing() {
        assert!(read("image/png", &[0x89, b'P', b'N', b'G']).is_empty());
        assert!(read("application/pdf", b"%PDF-1.7").is_empty());
        assert!(read("image/jpeg", &[]).is_empty());
    }

    #[test]
    fn a_camera_writes_the_whole_loan_shot_card() {
        let root = Dir::new()
            .ascii(0x010f, "Canon")
            .ascii(0x0110, "Canon EOS R5")
            .ascii(0x0131, "Digital Photo Professional")
            .short(ORIENTATION_TAG, 1)
            .short(0x8827, 400)
            .pointer(0x8769);
        let sub = Dir::new()
            .rational(0x829a, 1, 200) // 1/200 s
            .rational(0x829d, 180, 100) // f/1.8
            .rational(0x920a, 5000, 100) // 50 mm
            .ascii(0x9003, "2019:07:04 12:34:56")
            .ascii(0xa434, "RF 24-70mm F2.8 L IS USM");
        let found = read("image/jpeg", &jpeg_exif(&tiff(&root, Some(&sub))));

        assert_eq!(found.make.as_deref(), Some("Canon"));
        assert_eq!(found.model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(
            found.software.as_deref(),
            Some("Digital Photo Professional")
        );
        assert_eq!(found.iso, Some(400));
        assert_eq!(found.exposure_ms, Some(5));
        assert_eq!(found.aperture_x100, Some(180));
        assert_eq!(found.focal_length_mm, Some(50));
        assert_eq!(found.orientation, Some(1));
        assert_eq!(found.captured_at.as_deref(), Some("2019-07-04T12:34:56"));
        assert_eq!(found.lens.as_deref(), Some("RF 24-70mm F2.8 L IS USM"));
        assert!(!found.gps);
        assert_eq!(
            found.headline().as_deref(),
            Some("Canon Canon EOS R5 · ISO 400 · 1/200 s · f/1.8 · 50 mm")
        );
    }

    #[test]
    fn a_long_string_field_does_not_overflow_the_record() {
        let root = Dir::new().pointer(0x8769);
        let sub = Dir::new().ascii(0xa434, &"x".repeat(MAX_STRING_BYTES + 40));
        let found = read("image/jpeg", &jpeg_exif(&tiff(&root, Some(&sub))));
        assert!(found.lens.is_none());
    }

    #[test]
    fn a_gps_pointer_sets_the_flag_without_reading_the_coordinates() {
        // The GPS directory hangs off a pointer of its own (`0x8825`), not off the Exif
        // sub-directory — a block whose only pointer is the Exif one carries no location at all,
        // and saying otherwise would be a flag the file never set.
        let root = Dir::new().pointer(GPS_TAG);
        let sub = Dir::new().long(0x0001, 2);
        let block = tiff(&root, Some(&sub));
        let found = read("image/jpeg", &jpeg_exif(&block));

        assert!(found.gps);
        // The serialised record names the fact and nothing else: there is no field in the type a
        // coordinate could occupy, so the assertion is about the bytes rather than a policy.
        let json = to_json(&found);
        assert!(json.contains("\"gps\":true"));
        assert!(!json.contains("lat"));
        assert!(!json.contains("lon"));
        assert!(!json.contains("GPS"));
    }

    #[test]
    fn a_sub_directory_that_is_not_the_gps_one_sets_no_location() {
        // The Exif sub-directory holds the exposure; a file with only that is not a geotagged
        // photo, and the flag is the difference between "we know where" and "we do not".
        let root = Dir::new().pointer(SUBDIR_TAG);
        let sub = Dir::new().rational(0x829a, 1, 200);
        let found = read("image/jpeg", &jpeg_exif(&tiff(&root, Some(&sub))));
        assert!(!found.gps);
        assert_eq!(found.exposure_ms, Some(5));
    }

    #[test]
    fn a_rotated_picture_reports_the_box_the_browser_will_draw() {
        let root = Dir::new().short(ORIENTATION_TAG, 6).pointer(0x8769);
        let sub = Dir::new().ascii(0x9003, "2019:07:04 12:34:56");
        let found = read("image/jpeg", &jpeg_exif(&tiff(&root, Some(&sub))));

        assert_eq!(found.orientation, Some(6));
        // A 4000×3000 sensor rotated a quarter turn is a portrait picture: 3000×4000.
        assert_eq!(
            oriented_size(Some(4000), Some(3000), found.orientation),
            (Some(3000), Some(4000))
        );
    }

    #[test]
    fn an_orientation_outside_one_to_eight_is_absent() {
        for value in [0u16, 9, 255] {
            let found = read(
                "image/jpeg",
                &jpeg_exif(&tiff(&Dir::new().short(ORIENTATION_TAG, value), None)),
            );
            assert_eq!(
                found.orientation, None,
                "orientation {value} must not be stored"
            );
        }
    }

    #[test]
    fn an_app1_without_the_exif_signature_is_not_exif() {
        // XMP lives in APP1 too, and it is XML rather than a TIFF block.
        let bytes = jpeg(b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>");
        assert!(read("image/jpeg", &bytes).is_empty());
    }

    #[test]
    fn a_tiff_header_is_not_exif() {
        // The IFD format is shared by TIFF itself and by GeoTIFF. A `.tif` that was declared as a
        // JPEG is not a camera block, and reading it as one is how a "lens" of seven control
        // characters ends up in a catalogue.
        let mut bytes = jpeg(b"");
        bytes.extend_from_slice(b"II*\0\x08\0\0\0");
        assert!(read("image/jpeg", &bytes).is_empty());
    }

    #[test]
    fn an_offset_past_the_block_is_refused_rather_than_indexed() {
        // A hand-built entry whose value pointer claims a megabyte into a 40-byte block. The
        // reader must answer "absent" instead of indexing past the slice.
        let mut block = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        block.extend_from_slice(&1u16.to_le_bytes());
        block.extend_from_slice(&0x0112u16.to_le_bytes());
        block.extend_from_slice(&3u16.to_le_bytes());
        block.extend_from_slice(&1u32.to_le_bytes());
        block.extend_from_slice(&0x0010_0000u32.to_le_bytes());
        block.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(read("image/jpeg", &jpeg_exif(&block)).orientation, None);
    }

    #[test]
    fn a_zero_denominator_is_absent_rather_than_a_division_by_zero() {
        let root = Dir::new().pointer(0x8769);
        let sub = Dir::new().rational(0x829a, 1, 0).rational(0x829d, 18, 0);
        let found = read("image/jpeg", &jpeg_exif(&tiff(&root, Some(&sub))));
        assert_eq!(found.exposure_ms, None);
        assert_eq!(found.aperture_x100, None);
    }

    #[test]
    fn a_wrong_tiff_magic_is_not_a_block() {
        // 42 is the only legal TIFF magic. A block that starts with anything else is not a
        // directory, and walking one anyway is how a reader invents fields.
        let mut block = tiff(&Dir::new().ascii(0x010f, "Canon"), None);
        block[2] = 43;
        assert!(read("image/jpeg", &jpeg_exif(&block)).make.is_none());
    }

    #[test]
    fn a_string_that_is_not_utf8_is_dropped_not_mangled() {
        assert!(text(&[0xff, 0xfe, 0x00]).is_none());
        assert_eq!(text(b"Canon\0").as_deref(), Some("Canon"));
        assert!(text(b"\0").is_none());
    }

    #[test]
    fn a_camera_date_becomes_rfc_3339_and_a_bogus_one_does_not() {
        assert_eq!(
            datetime("2019:07:04 12:34:56").as_deref(),
            Some("2019-07-04T12:34:56")
        );
        // A body with no clock writes zeros; that is not the epoch, it is nothing.
        assert!(datetime("0000:00:00 00:00:00").is_none());
        assert!(datetime("2019:13:01 00:00:00").is_none());
        assert!(datetime("2019:01:01 25:00:00").is_none());
        assert!(datetime("2019-01-01 00:00:00").is_none());
        assert!(datetime("not a date").is_none());
    }

    #[test]
    fn the_exposure_reads_as_a_shutter_speed() {
        let fast = Exif {
            exposure_ms: Some(5),
            ..Exif::default()
        };
        assert_eq!(fast.exposure().as_deref(), Some("1/200 s"));
        let long = Exif {
            exposure_ms: Some(1600),
            ..Exif::default()
        };
        assert_eq!(long.exposure().as_deref(), Some("1.6 s"));
    }

    #[test]
    fn an_empty_record_serialises_to_nothing_a_search_can_match() {
        assert_eq!(to_json(&Exif::default()), "{}");
    }

    #[test]
    fn a_png_carries_its_exif_in_a_chunk() {
        let block = tiff(&Dir::new().ascii(0x010f, "Canon"), None);
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&(block.len() as u32).to_be_bytes());
        png.extend_from_slice(b"eXIf");
        png.extend_from_slice(&block);
        png.extend_from_slice(&[0, 0, 0, 0]); // CRC, unread by this reader
        assert_eq!(read("image/png", &png).make.as_deref(), Some("Canon"));
    }

    #[test]
    fn a_webp_carries_its_exif_in_a_chunk() {
        let block = tiff(&Dir::new().ascii(0x0110, "iPhone 12 Pro"), None);
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&0u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"EXIF");
        webp.extend_from_slice(&(block.len() as u32).to_le_bytes());
        webp.extend_from_slice(&block);
        assert_eq!(
            read("image/webp", &webp).model.as_deref(),
            Some("iPhone 12 Pro")
        );
    }

    #[test]
    fn a_big_endian_block_is_read_in_its_own_order() {
        // Cameras that write `MM` would otherwise produce make = "nonaC" and an ISO of 256.
        let mut block = vec![b'M', b'M', 0, 42, 0, 0, 0, 8];
        block.extend_from_slice(&1u16.to_be_bytes());
        block.extend_from_slice(&0x010fu16.to_be_bytes());
        block.extend_from_slice(&2u16.to_be_bytes());
        block.extend_from_slice(&6u32.to_be_bytes());
        // The value is too wide to sit in the entry, so the entry holds an offset — and the
        // offset is big-endian too, which is the whole point of the test.
        let here = (block.len() + 4) as u32;
        block.extend_from_slice(&here.to_be_bytes());
        block.extend_from_slice(b"Canon\0");
        block.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            read("image/jpeg", &jpeg_exif(&block)).make.as_deref(),
            Some("Canon")
        );
    }
}
