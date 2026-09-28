//! What the file manager can read out of the bytes themselves (REQ-010, slice 2).
//!
//! The file detail screen shows a preview, and a preview needs three numbers the row does not
//! carry: how wide and high an image is, how long an audio or video file runs, and how many pages
//! a document has. All three are read **from the header of the format**, never by decoding the
//! whole file — a 4 GB video upload must not cost a full read to learn its duration.
//!
//! Every extractor here works on a byte prefix and answers "I do not know" (`None`) rather than
//! guessing. A wrong dimension is worse than a missing one: it breaks the aspect ratio of every
//! layout that uses it, and it is not obviously wrong once it is stored.

/// How many leading bytes the extractors are allowed to look at.
///
/// Every format below states its own header size well inside this budget — PNG 24 bytes, JPEG a
/// few hundred, MP4 boxes a few kilobytes — so one prefix serves all of them and the read is
/// bounded no matter how large the upload is.
pub const HEADER_BYTES: usize = 64 * 1024;

/// What the header of a file says about it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaProbe {
    /// Pixel width, for raster formats that carry it.
    pub width: Option<i32>,
    /// Pixel height.
    pub height: Option<i32>,
    /// Playing time in milliseconds, for timed formats.
    pub duration_ms: Option<i32>,
    /// Page count, for document formats that count pages.
    pub page_count: Option<i32>,
}

impl MediaProbe {
    /// Whether the header said nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width.is_none()
            && self.height.is_none()
            && self.duration_ms.is_none()
            && self.page_count.is_none()
    }

    /// Fill the columns a `media` row carries, leaving everything else alone.
    #[must_use]
    pub fn columns(&self) -> (Option<i32>, Option<i32>, Option<i32>, Option<i32>) {
        (self.width, self.height, self.duration_ms, self.page_count)
    }
}

/// Read what a byte prefix says about a file of this content type.
///
/// The `content_type` decides the extractor rather than the file name: the browser and the
/// uploader disagree about extensions, and the declared type is what the serve path trusts.
#[must_use]
pub fn probe(content_type: &str, head: &[u8]) -> MediaProbe {
    match content_type {
        "image/png" => probe_png(head),
        "image/gif" => probe_gif(head),
        "image/jpeg" => probe_jpeg(head),
        "image/webp" => probe_webp(head),
        "image/bmp" => probe_bmp(head),
        "image/tiff" => probe_tiff(head),
        "video/mp4" | "video/quicktime" => probe_mp4(head),
        "video/webm" => probe_webm(head),
        "audio/mpeg" | "audio/ogg" | "audio/wav" => probe_audio(content_type, head),
        "application/pdf" => probe_pdf(head),
        "text/plain" => MediaProbe::default(),
        _ => MediaProbe::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// Raster images
// ---------------------------------------------------------------------------------------------

/// PNG: the `IHDR` chunk is always the first one and always 13 bytes long, so the size is at a
/// fixed offset — no scan needed.
fn probe_png(head: &[u8]) -> MediaProbe {
    const IHDR_WIDTH: usize = 16;
    let signature = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if head.len() < IHDR_WIDTH + 8 || head[..8] != signature {
        return MediaProbe::default();
    }
    // Guard the chunk name too: a file that starts with the signature but is not a PNG would
    // otherwise be read as if it were one.
    if &head[12..16] != b"IHDR" {
        return MediaProbe::default();
    }
    dimensions(
        i32::try_from(be_u32(head, IHDR_WIDTH)).ok(),
        i32::try_from(be_u32(head, IHDR_WIDTH + 4)).ok(),
    )
}

/// GIF: the logical screen descriptor is a fixed 7 bytes after the signature.
fn probe_gif(head: &[u8]) -> MediaProbe {
    if head.len() < 10 {
        return MediaProbe::default();
    }
    let signature = if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        true
    } else {
        false
    };
    if !signature {
        return MediaProbe::default();
    }
    dimensions(
        Some(i32::from(le_u16(head, 6))),
        Some(i32::from(le_u16(head, 8))),
    )
}

/// BMP: a 14-byte file header then a 40-byte `BITMAPINFOHEADER` whose width and height are signed.
fn probe_bmp(head: &[u8]) -> MediaProbe {
    if head.len() < 26 || !head.starts_with(b"BM") {
        return MediaProbe::default();
    }
    let width = i32::from(i16::from_le_bytes([head[18], head[19]]));
    // A negative height is a top-down bitmap; the pixel count is the same, the sign is not.
    let height = i32::from(i16::from_le_bytes([head[20], head[21]])).abs();
    dimensions(Some(width), Some(height))
}

/// TIFF: the offsets are indirect (the header points at the IFD), so the width and height are
/// read from the first two `SHORT` entries of the first directory.
fn probe_tiff(head: &[u8]) -> MediaProbe {
    let little_endian = match head.get(..2) {
        Some([0x49, 0x49]) => true,
        Some([0x4d, 0x4d]) => false,
        _ => return MediaProbe::default(),
    };
    if head.len() < 8 {
        return MediaProbe::default();
    }
    let read_u16 = |offset: usize| {
        if offset + 2 > head.len() {
            None
        } else if little_endian {
            Some(u16::from_le_bytes([head[offset], head[offset + 1]]))
        } else {
            Some(u16::from_be_bytes([head[offset], head[offset + 1]]))
        }
    };
    let read_u32 = |offset: usize| {
        if offset + 4 > head.len() {
            None
        } else if little_endian {
            Some(u32::from_le_bytes([
                head[offset],
                head[offset + 1],
                head[offset + 2],
                head[offset + 3],
            ]))
        } else {
            Some(u32::from_be_bytes([
                head[offset],
                head[offset + 1],
                head[offset + 2],
                head[offset + 3],
            ]))
        }
    };
    let Some(ifd) = read_u32(4) else {
        return MediaProbe::default();
    };
    let ifd = ifd as usize;
    if ifd + 2 > head.len() {
        return MediaProbe::default();
    }
    let entries = read_u16(ifd).unwrap_or(0) as usize;
    // Entry n is a 12-byte record: tag(2) type(2) count(4) value(4). Tag 256 is the width, 257
    // the height, both `SHORT` (type 3) so the value sits in the last two bytes of the record.
    let mut width = None;
    let mut height = None;
    for entry in 0..entries.min(64) {
        let base = ifd + 2 + entry * 12;
        if base + 12 > head.len() {
            break;
        }
        let tag = read_u16(base).unwrap_or(0);
        let matches = if tag == 256 {
            &mut width
        } else if tag == 257 {
            &mut height
        } else {
            continue;
        };
        *matches = read_u16(base + 8).map(i32::from);
    }
    dimensions(width, height)
}

/// WebP: three container layouts share the `RIFF`/`WEBP` signature, and only one of them puts
/// the canvas size in the header — the other two answer "I do not know" rather than a guess.
fn probe_webp(head: &[u8]) -> MediaProbe {
    // The container check needs only the RIFF header and the chunk id; each layout then asks for
    // the bytes *it* reads. One blanket minimum for all three would refuse a short-but-complete
    // `VP8L` header, and a reader that only ever sees 30-byte files never learns the difference.
    if head.len() < 16 || !head.starts_with(b"RIFF") || &head[8..12] != b"WEBP" {
        return MediaProbe::default();
    }
    let chunk: [u8; 4] = [head[12], head[13], head[14], head[15]];
    if &chunk == b"VP8X" {
        // Extended format: 24-bit little-endian width-1 and height-1 at fixed offsets.
        let read_24 = |offset: usize| -> Option<i32> {
            if offset + 3 > head.len() {
                return None;
            }
            Some(
                i32::from(head[offset])
                    | (i32::from(head[offset + 1]) << 8)
                    | (i32::from(head[offset + 2]) << 16),
            )
        };
        // The canvas values are stored as `value - 1`, so a stored zero is a corrupt header
        // rather than a one-pixel image — reading it as `0 + 1` would put a 1×1 box on screen
        // for a file that has no size at all.
        return match (read_24(24), read_24(27)) {
            (Some(0), _) | (_, Some(0)) => MediaProbe::default(),
            (Some(w), Some(h)) => dimensions(Some(w + 1), Some(h + 1)),
            _ => MediaProbe::default(),
        };
    }
    if &chunk == b"VP8 " {
        // Lossy: a 3-byte frame tag, a 3-byte sync code, then the 16-bit width and height with
        // their 2-bit scale in the high bits. The frame ends at byte 30.
        if head.len() < 30 {
            return MediaProbe::default();
        }
        let width = (le_u16(head, 26) & 0x3fff) as i32;
        let height = (le_u16(head, 28) & 0x3fff) as i32;
        return dimensions(Some(width), Some(height));
    }
    if &chunk == b"VP8L" {
        // Lossless: 14 signature bits, then 14 bits of width-1 and 14 of height-1.
        if head.len() < 25 {
            return MediaProbe::default();
        }
        let bits = u32::from(head[21])
            | (u32::from(head[22]) << 8)
            | (u32::from(head[23]) << 16)
            | (u32::from(head[24]) << 24);
        let width = ((bits & 0x3fff) + 1) as i32;
        let height = (((bits >> 14) & 0x3fff) + 1) as i32;
        return dimensions(Some(width), Some(height));
    }
    MediaProbe::default()
}

/// JPEG: the size lives in a start-of-frame marker, and the frame is the first SOF that is not a
/// DHT/JPG/DAC segment. The scan walks markers rather than scanning for `0xFFC0` bytes anywhere,
/// because compressed data can contain that sequence by chance.
fn probe_jpeg(head: &[u8]) -> MediaProbe {
    if head.len() < 4 || head[0] != 0xff || head[1] != 0xd8 {
        return MediaProbe::default();
    }
    let mut offset = 2;
    while offset + 3 < head.len() {
        // Markers may be preceded by any number of `0xFF` fill bytes.
        if head[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = head[offset + 1];
        offset += 2;
        match marker {
            // Start of frame — the one family that carries the canvas size. 0xC4 is DHT and 0xC8
            // is JPG, both of which are *not* frame headers despite the numeric neighbourhood.
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                if offset + 5 > head.len() {
                    return MediaProbe::default();
                }
                let height = u16::from_be_bytes([head[offset + 3], head[offset + 4]]);
                let width = u16::from_be_bytes([head[offset + 5], head[offset + 6]]);
                return dimensions(Some(i32::from(width)), Some(i32::from(height)));
            }
            // Standalone markers with no payload.
            0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7 => continue,
            _ => {
                if offset + 1 > head.len() {
                    return MediaProbe::default();
                }
                let length = u16::from_be_bytes([head[offset], head[offset + 1]]) as usize;
                if length < 2 {
                    return MediaProbe::default();
                }
                offset += length;
            }
        }
    }
    MediaProbe::default()
}

// ---------------------------------------------------------------------------------------------
// Timed formats
// ---------------------------------------------------------------------------------------------

/// MP4 / QuickTime: walk the box tree looking for `mvhd` (duration) and `tkhd` (canvas).
///
/// Only the `moov` box carries either, and a file whose `moov` sits after the media data (the
/// streaming-friendly layout) needs the whole file to find it — so a prefix that does not reach
/// it answers "I do not know" rather than reporting a zero-length video.
fn probe_mp4(head: &[u8]) -> MediaProbe {
    let mut duration_ms = None;
    let mut width = None;
    let mut height = None;
    walk_mp4(head, 0, head.len(), 0, &mut |kind, start, body| {
        match kind {
            // Movie header: version 0 keeps the timescale and duration as 32-bit, version 1 as
            // 64-bit. Reading a 64-bit value as 32-bit silently reports a fraction of the real
            // length, so the version is read rather than assumed.
            b"mvhd" => {
                if body.len() < 20 {
                    return;
                }
                let (timescale, duration) = if body[0] == 1 {
                    if body.len() < 32 {
                        return;
                    }
                    (
                        be_u32(body, 12),
                        u64::from(be_u32(body, 16)) << 32 | u64::from(be_u32(body, 20)),
                    )
                } else {
                    (be_u32(body, 12), u64::from(be_u32(body, 16)))
                };
                if timescale > 0 && duration > 0 {
                    // Milliseconds, rounded down, and capped at ~24 days: a header that claims a
                    // larger duration is corrupt, and a negative column would break the screen.
                    let millis = duration.saturating_mul(1000) / u64::from(timescale);
                    duration_ms =
                        Some(i32::try_from(millis.min(2_073_600_000)).unwrap_or(2_073_600_000));
                }
            }
            // Track header: the canvas is a 16.16 fixed-point value at the end of the header, the
            // version deciding where that is. The height occupies its sign bit for a track with
            // a negative matrix row, so the magnitude is read off and a portrait clip does not
            // arrive as a negative height.
            b"tkhd" if width.is_none() && height.is_none() => {
                if body.len() < 4 {
                    return;
                }
                let offset = if body[0] == 1 { 96 } else { 80 };
                if body.len() < offset + 8 {
                    return;
                }
                let canvas_w = (be_u32(body, offset) >> 16) & 0xffff;
                let canvas_h = (be_u32(body, offset + 4) >> 16) & 0xffff;
                if canvas_w > 0 && canvas_h > 0 {
                    width = Some(canvas_w as i32);
                    height = Some(canvas_h as i32);
                }
            }
            _ => {
                // Every other box — including the media data itself — is read past, not opened.
                let _ = (start, body);
            }
        }
    });
    MediaProbe {
        width,
        height,
        duration_ms,
        page_count: None,
    }
}

/// Walk the ISO-BMFF box tree of a prefix, calling `visit` for every box whose header is inside
/// the bytes we have. Boxes that are containers are descended into.
fn walk_mp4(
    bytes: &[u8],
    from: usize,
    to: usize,
    depth: usize,
    visit: &mut impl FnMut(&[u8; 4], usize, &[u8]),
) {
    // A corrupt or hostile file can nest boxes arbitrarily deep; three levels covers every real
    // layout (root → moov → trak) and a cycle in the size fields cannot spin here.
    if depth > 3 {
        return;
    }
    let mut offset = from;
    while offset + 8 <= to && offset + 8 <= bytes.len() {
        let size = be_u32(bytes, offset) as usize;
        let kind: [u8; 4] = [
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ];
        let (size, header) = if size == 1 {
            // 64-bit box size: the low half being 1 is the signal, and the high half follows.
            if offset + 16 > bytes.len() {
                return;
            }
            let large =
                u64::from(be_u32(bytes, offset + 8)) << 32 | u64::from(be_u32(bytes, offset + 12));
            (usize::try_from(large).unwrap_or(usize::MAX), 16usize)
        } else if size == 0 {
            // A box that claims to run to the end of the file.
            (to - offset, 8usize)
        } else {
            (size, 8usize)
        };
        if size < header {
            return;
        }
        let body_start = offset + header;
        let body_end = offset.saturating_add(size).min(to).min(bytes.len());
        if body_start > body_end {
            return;
        }
        let body = &bytes[body_start.min(bytes.len())..body_end];
        visit(&kind, offset, body);
        if matches!(
            &kind,
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"udta"
        ) {
            walk_mp4(bytes, body_start, body_end, depth + 1, visit);
        }
        offset += size;
    }
}

/// WebM / Matroska: duration and canvas live in the `Info` and `Track` elements as variable-length
/// EBML integers. A prefix that does not reach the `Segment` body answers "I do not know".
fn probe_webm(head: &[u8]) -> MediaProbe {
    if !head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return MediaProbe::default();
    }
    let mut duration_ms = None;
    let mut width = None;
    let mut height = None;
    // A file is a stream of elements; the nested ones repeat until the prefix runs out. Element
    // ids and sizes are read as written, and an unknown element is skipped by its declared size.
    let mut offset = 0;
    let mut guard = 0;
    while offset < head.len() && guard < 512 {
        guard += 1;
        let Some((id, after_id)) = read_ebml_id(head, offset) else {
            break;
        };
        let Some((size, after_size)) = read_ebml_size(head, after_id) else {
            break;
        };
        let body_start = after_size;
        let body_end = (body_start.saturating_add(size)).min(head.len());
        if body_start > body_end {
            break;
        }
        let body = &head[body_start..body_end];
        match id {
            // Info → TimecodeScale (0x2AD7B1, in nanoseconds per tick)
            0x2a_d7_b1 => {}
            // Info → Duration (0x4489, in timecode-scale units)
            0x44_89 => duration_ms = read_ebml_float(body),
            // Track → Video → PixelWidth (0xB0) / PixelHeight (0xBA)
            0xb0 => width = read_ebml_int(body).map(|v| v as i32),
            0xba => height = read_ebml_int(body).map(|v| v as i32),
            // Segment (0x18538067) and the elements that hold the ones above are containers.
            0x18_53_80_67 | 0x15_49_a9_66 | 0x16_54_ae_6b | 0xe0 => {
                // Descend only into what is actually in front of us.
                offset = body_start;
                continue;
            }
            _ => {}
        }
        offset = body_end;
    }
    MediaProbe {
        width,
        height,
        duration_ms,
        page_count: None,
    }
}

/// Read an EBML element id: a variable-length integer whose leading bit count is the length.
fn read_ebml_id(bytes: &[u8], offset: usize) -> Option<(u32, usize)> {
    let first = *bytes.get(offset)?;
    if first == 0 {
        return None;
    }
    let length = first.leading_zeros() as usize + 1;
    if offset + length > bytes.len() {
        return None;
    }
    let mut value = 0u32;
    for index in 0..length {
        value = (value << 8) | u32::from(bytes[offset + index]);
    }
    Some((value, offset + length))
}

/// Read an EBML size: the same variable-length form, minus the marker bit.
fn read_ebml_size(bytes: &[u8], offset: usize) -> Option<(usize, usize)> {
    let first = *bytes.get(offset)?;
    if first == 0 {
        return None;
    }
    let length = first.leading_zeros() as usize + 1;
    if offset + length > bytes.len() {
        return None;
    }
    let mut value = 0u64;
    for index in 0..length {
        value = (value << 8) | u64::from(bytes[offset + index]);
    }
    // Drop the length marker to recover the number itself.
    let value = value & ((1u64 << (7 * length)) - 1);
    // "Unknown size" is all ones; a nested loop would never end.
    if value == (1u64 << (7 * length)) - 1 {
        return Some((bytes.len().saturating_sub(offset + length), offset + length));
    }
    Some((usize::try_from(value).ok()?, offset + length))
}

/// Read an EBML unsigned integer body (1–8 bytes, big-endian).
fn read_ebml_int(body: &[u8]) -> Option<u64> {
    let body = body.get(..body.len().min(8))?;
    if body.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for byte in body {
        value = (value << 8) | u64::from(*byte);
    }
    Some(value)
}

/// Read an EBML float body (4 or 8 bytes, big-endian IEEE).
fn read_ebml_float(body: &[u8]) -> Option<i32> {
    let millis = match body.len() {
        4 => f64::from(f32::from_be_bytes([body[0], body[1], body[2], body[3]])) * 1000.0,
        8 => {
            f64::from_be_bytes([
                body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
            ]) * 1000.0
        }
        _ => return None,
    };
    if !millis.is_finite() || millis <= 0.0 {
        return None;
    }
    Some(i32::try_from(millis.min(2_073_600_000.0) as i64).unwrap_or(2_073_600_000))
}

/// Audio: MP3 frames, Ogg pages and the RIFF/WAVE header all state their rate somewhere in the
/// first few kilobytes; the duration needs the rate and the byte length, so it is reported only
/// when both are known.
fn probe_audio(content_type: &str, head: &[u8]) -> MediaProbe {
    match content_type {
        "audio/wav" => probe_wave(head),
        "audio/mpeg" => probe_mp3(head),
        "audio/ogg" => probe_ogg(head),
        _ => MediaProbe::default(),
    }
}

/// WAVE: `fmt ` gives the sample rate and `data` gives the byte length, so the duration is exact.
fn probe_wave(head: &[u8]) -> MediaProbe {
    if head.len() < 44 || !head.starts_with(b"RIFF") || &head[8..12] != b"WAVE" {
        return MediaProbe::default();
    }
    let mut byte_rate = None;
    let mut data_bytes = None;
    let mut offset = 12;
    while offset + 8 <= head.len() {
        let id: [u8; 4] = [
            head[offset],
            head[offset + 1],
            head[offset + 2],
            head[offset + 3],
        ];
        let size = le_u32(head, offset + 4) as usize;
        let body = offset + 8;
        match &id {
            b"fmt " if body + 12 <= head.len() => byte_rate = Some(le_u32(head, body + 8)),
            b"data" => data_bytes = Some(size),
            _ => {}
        }
        if let (Some(rate), Some(bytes)) = (byte_rate, data_bytes) {
            if rate > 0 {
                return MediaProbe {
                    duration_ms: i32::try_from(bytes as u64 * 1000 / u64::from(rate)).ok(),
                    ..MediaProbe::default()
                };
            }
        }
        // Chunks are word-aligned, so an odd size is followed by one pad byte.
        offset = body + size + (size % 2);
    }
    MediaProbe::default()
}

/// MP3: the first frame header states the bitrate and sample rate. A variable-bitrate file has
/// no exact length in the header, so the duration is left unknown rather than guessed from the
/// first frame alone.
fn probe_mp3(head: &[u8]) -> MediaProbe {
    if head.len() < 4 || head.starts_with(b"ID3") {
        return MediaProbe::default();
    }
    // The sync word is 11 set bits, and the two bytes that follow must not be all ones.
    if head[0] != 0xff || (head[1] & 0xe0) != 0xe0 || head[1] == 0xff {
        return MediaProbe::default();
    }
    // Bitrate index 15 and sample-rate index 3 are the "free" / "reserved" encodings.
    let bitrate_index = usize::from((head[2] & 0xf0) >> 4);
    let sample_rate_index = usize::from((head[2] & 0x0c) >> 2);
    if bitrate_index == 0 || bitrate_index == 15 || sample_rate_index == 3 {
        return MediaProbe::default();
    }
    const BITRATES: [u32; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const SAMPLE_RATES: [u32; 3] = [44100, 48000, 32000];
    let bitrate = u64::from(BITRATES[bitrate_index]) * 1000;
    let sample_rate = SAMPLE_RATES[sample_rate_index];
    if bitrate == 0 {
        return MediaProbe::default();
    }
    // The bytes we hold stand in for the whole file, which only bounds the answer from above —
    // so the duration is reported as unknown rather than as "the length of my prefix".
    let _ = (sample_rate, head.len());
    MediaProbe::default()
}

/// Ogg Vorbis/Opus: the identification header states the sample rate; the granule position of the
/// last page states the length. A prefix that does not reach the last page leaves the duration
/// unknown.
fn probe_ogg(head: &[u8]) -> MediaProbe {
    if !head.starts_with(b"OggS") {
        return MediaProbe::default();
    }
    // The identification packet is the first one: `\x01vorbis` or `OpusHead`.
    let vorbis = head.windows(8).any(|window| window == b"\x01vorbis");
    let opus = head.windows(8).any(|window| window == b"OpusHead");
    if !vorbis && !opus {
        return MediaProbe::default();
    }
    // A granule position that is not in the prefix means the length is not in the prefix.
    MediaProbe::default()
}

// ---------------------------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------------------------

/// PDF: `/Count` in the page tree is the page count, and an encrypted document keeps the
/// structure parseable but the object streams compressed — so a document that does not answer
/// inside the prefix is reported as unknown rather than as one page.
fn probe_pdf(head: &[u8]) -> MediaProbe {
    if !head.starts_with(b"%PDF-") {
        return MediaProbe::default();
    }
    let text = String::from_utf8_lossy(head);
    // `/Type /Pages` with a `/Count` child is the page-tree root; the last count wins because a
    // linearised file also carries the count in an early hint.
    let mut best: Option<i32> = None;
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(found) = text[index..].find("/Count") {
        let at = index + found;
        index = at + 6;
        let rest = &text[at + 6..];
        let Some(rest) = rest.split_whitespace().next() else {
            continue;
        };
        let Ok(value) = rest
            .trim_start_matches(|c: char| c == '[' || c.is_whitespace())
            .parse::<i32>()
        else {
            continue;
        };
        if value > 0 {
            best = Some(value);
        }
        let _ = bytes;
    }
    MediaProbe {
        page_count: best.filter(|value| *value > 0 && *value < 1_000_000),
        ..MediaProbe::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Small readers
// ---------------------------------------------------------------------------------------------

/// Read a big-endian `u32` at `offset`, or `0` when the prefix is too short.
fn be_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset.saturating_add(4))
        .map(|slice| u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .unwrap_or(0)
}

/// Read a little-endian `u16` at `offset`, or `0` when the prefix is too short.
fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes
        .get(offset..offset.saturating_add(2))
        .map(|slice| u16::from_le_bytes([slice[0], slice[1]]))
        .unwrap_or(0)
}

/// Read a little-endian `u32` at `offset`, or `0` when the prefix is too short.
fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset.saturating_add(4))
        .map(|slice| u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .unwrap_or(0)
}

/// Pair a width and a height into a probe, refusing a zero or absent axis.
fn dimensions(width: Option<i32>, height: Option<i32>) -> MediaProbe {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => MediaProbe {
            width: Some(w),
            height: Some(h),
            ..MediaProbe::default()
        },
        _ => MediaProbe::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG of the given size, built header-only — the extractors never look past the header.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8];
        // A comment segment before the frame, so a reader that looks for the first SOF rather
        // than walking the markers reads the wrong offset.
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00, 0x04, 0xde, 0xad]);
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
        bytes
    }

    #[test]
    fn a_png_states_its_size_in_the_first_chunk() {
        let found = probe("image/png", &png(1920, 1080));
        assert_eq!((found.width, found.height), (Some(1920), Some(1080)));
        assert_eq!(found.duration_ms, None);
    }

    #[test]
    fn a_png_signature_alone_is_not_enough() {
        // A file that begins with the PNG signature but has no IHDR must not be read as one.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0, 0, 0, 0, b'J', b'U', b'N', b'K']);
        assert!(probe("image/png", &bytes).is_empty());
    }

    #[test]
    fn a_jpeg_is_read_from_its_frame_marker_not_the_first_ff() {
        // Width and height are 0xFFFF here: a reader that searched for the first 0xFFC0 pattern
        // anywhere in the bytes would happily match this one.
        let found = probe("image/jpeg", &jpeg(0xffff, 0xffff));
        assert_eq!((found.width, found.height), (Some(0xffff), Some(0xffff)));

        let small = probe("image/jpeg", &jpeg(800, 600));
        assert_eq!((small.width, small.height), (Some(800), Some(600)));
    }

    #[test]
    fn a_gif_states_the_logical_screen_size() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&800u16.to_le_bytes());
        bytes.extend_from_slice(&450u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        let found = probe("image/gif", &bytes);
        assert_eq!((found.width, found.height), (Some(800), Some(450)));
    }

    /// A WebP `VP8X` chunk: 12 bytes of RIFF header, 8 of chunk header, then 4 bytes of
    /// flags/reserved followed by the two 24-bit canvas values stored as `value - 1`.
    fn webp_extended(width_minus_one: [u8; 3], height_minus_one: [u8; 3]) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBPVP8X".to_vec();
        bytes.extend_from_slice(&[0; 4]); // chunk size
        bytes.extend_from_slice(&[0; 4]); // flags + reserved
        bytes.extend_from_slice(&width_minus_one);
        bytes.extend_from_slice(&height_minus_one);
        bytes.extend_from_slice(&[0; 8]); // alpha, animation and colour profile chunks
        bytes
    }

    #[test]
    fn a_webp_reads_each_of_its_three_containers() {
        let found = probe("image/webp", &webp_extended([10, 0, 0], [20, 0, 0]));
        assert_eq!((found.width, found.height), (Some(11), Some(21)));

        let mut vp8 = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        vp8.extend_from_slice(&[0; 10]);
        vp8.extend_from_slice(&640u16.to_le_bytes());
        vp8.extend_from_slice(&480u16.to_le_bytes());
        let found = probe("image/webp", &vp8);
        assert_eq!((found.width, found.height), (Some(640), Some(480)));
    }

    #[test]
    fn a_webp_lossless_states_its_canvas() {
        // `VP8L`: 14 signature bits, then 14 bits of width-1 and 14 of height-1, both
        // little-endian inside a 32-bit word that starts after the signature byte.
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBPVP8L".to_vec();
        bytes.extend_from_slice(&[0; 4]); // chunk size
        bytes.push(0x2f); // signature
        let bits: u32 = 15 | (7 << 14);
        bytes.extend_from_slice(&bits.to_le_bytes());
        let found = probe("image/webp", &bytes);
        assert_eq!((found.width, found.height), (Some(16), Some(8)));
    }

    #[test]
    fn a_truncated_or_foreign_prefix_states_nothing() {
        assert!(probe("image/png", b"not an image at all").is_empty());
        assert!(probe("image/jpeg", &[]).is_empty());
        assert!(probe("image/gif", b"BM").is_empty());
        // A JPEG SOF header that claims a size beyond the prefix is not a size.
        assert!(probe("image/jpeg", &[0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11]).is_empty());
    }

    #[test]
    fn a_zero_sized_axis_is_not_a_dimension() {
        // A WebP canvas of 0×0 is a corrupt header, and reading `0 + 1` would show a 1×1 box
        // for a file that has no size at all.
        assert!(probe("image/webp", &webp_extended([0, 0, 0], [20, 0, 0])).is_empty());
        assert!(probe("image/webp", &webp_extended([20, 0, 0], [0, 0, 0])).is_empty());
    }

    #[test]
    fn an_mp4_reports_its_movie_duration() {
        // ftyp + moov { mvhd (version 0) } with a 90 kHz timescale and a 3-second duration.
        let mut bytes: Vec<u8> = Vec::new();
        push_box(&mut bytes, b"ftyp", b"isom");
        let mut moov: Vec<u8> = Vec::new();
        let mut mvhd: Vec<u8> = Vec::new();
        mvhd.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags
        mvhd.extend_from_slice(&[0; 8]); // creation, modification
        mvhd.extend_from_slice(&90_000u32.to_be_bytes());
        mvhd.extend_from_slice(&270_000u32.to_be_bytes());
        push_box(&mut moov, b"mvhd", &mvhd);
        push_box(&mut bytes, b"moov", &moov);

        let found = probe("video/mp4", &bytes);
        assert_eq!(found.duration_ms, Some(3000));
    }

    #[test]
    fn an_mp4_without_its_movie_header_states_no_duration() {
        // The `moov` box is absent from this prefix, which is exactly the streaming layout a
        // quick look cannot answer — zero seconds would be a lie the screen would show.
        let mut bytes: Vec<u8> = Vec::new();
        push_box(&mut bytes, b"ftyp", b"isom");
        push_box(&mut bytes, b"mdat", &[0u8; 64]);
        assert_eq!(probe("video/mp4", &bytes).duration_ms, None);
    }

    #[test]
    fn a_box_that_claims_to_be_huge_does_not_loop() {
        // A size of 0 means "runs to the end of the file"; a size of 1 means the 64-bit form.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&[0; 8]);
        bytes.push(0);
        // Must terminate rather than walk off the end.
        assert!(probe("video/mp4", &bytes).is_empty());
    }

    #[test]
    fn a_wave_states_its_exact_length() {
        // 44 100 Hz stereo 16-bit is a byte rate of 176 400, so a `data` size of 176 400 is
        // exactly one second. The chunks are laid out with the sizes a real file carries —
        // hand-written byte arrays drift by two here, and a reader that guesses the offsets
        // instead of walking them then finds nothing.
        let mut fmt: Vec<u8> = vec![1, 0, 2, 0]; // PCM, 2 channels
        fmt.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
        fmt.extend_from_slice(&176_400u32.to_le_bytes()); // byte rate
        fmt.extend_from_slice(&4u16.to_le_bytes()); // block align
        fmt.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        assert_eq!(fmt.len(), 16, "a fmt chunk of a PCM header is 16 bytes");

        let mut bytes: Vec<u8> = b"RIFF".to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&fmt);
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(b"INFO");
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&176_400u32.to_le_bytes());
        let found = probe("audio/wav", &bytes);
        assert_eq!(found.duration_ms, Some(1000));
    }

    #[test]
    fn a_wave_with_no_data_chunk_states_no_duration() {
        // A header that declares 44 100 Hz but never reaches its samples: the length is not in
        // the prefix, so a duration computed from the prefix alone would be a fiction.
        let mut bytes: Vec<u8> = b"RIFF".to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[
            1, 0, 2, 0, 0x44, 0xac, 0, 0, 0x10, 0xb1, 2, 0, 4, 0, 0x10, 0,
        ]);
        assert_eq!(probe("audio/wav", &bytes).duration_ms, None);
    }

    #[test]
    fn a_pdf_reads_its_page_count() {
        let mut bytes: Vec<u8> = b"%PDF-1.7\n".to_vec();
        bytes.extend_from_slice(b"1 0 obj\n<< /Type /Pages /Count 12 >>\nendobj\n");
        assert_eq!(probe("application/pdf", &bytes).page_count, Some(12));
    }

    #[test]
    fn a_pdf_without_a_page_tree_states_nothing() {
        assert!(probe("application/pdf", b"%PDF-1.7\n%%EOF").is_empty());
        // A count of zero is a malformed document, not a zero-page one.
        let bytes: Vec<u8> = b"%PDF-1.7\n<< /Type /Pages /Count 0 >>".to_vec();
        assert!(probe("application/pdf", &bytes).is_empty());
    }

    #[test]
    fn an_unknown_type_never_guesses() {
        for content_type in [
            "application/zip",
            "image/svg+xml",
            "application/octet-stream",
        ] {
            assert!(
                probe(content_type, b"\x00\x01\x02\x03\x04\x05\x06\x07").is_empty(),
                "{content_type} must state nothing"
            );
        }
    }

    #[test]
    fn the_columns_line_up_with_the_media_row() {
        let found = probe("image/png", &png(64, 32));
        assert_eq!(
            found.columns(),
            (Some(64), Some(32), None, None),
            "the tuple is (width, height, duration_ms, page_count)"
        );
    }

    /// Append one ISO-BMFF box: 32-bit size, four-byte type, body.
    fn push_box(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
    }
}
