//! Real, decodable image bytes for the walks that need a decodable object.
//!
//! **Why this is not the same as a hand-written header.** A suite that only ever *stores* bytes
//! can fake a PNG: the signature, the `IHDR` type and the dimensions, with a plausible tail. The
//! library accepts it — the checksum is computed from whatever arrived, so the row is honest about
//! its own bytes — and every test that lists, renames, replaces or purges it passes. The moment a
//! walk asks for `?preset=standard` the decoder runs, and it answers `422 not_transformable` with
//! a CRC error naming a chunk the file never carried. So a "real image" that is only a header is
//! a file that passes every store walk and fails at the first one that opens it, which is the
//! worst shape a fixture can have: it looks like a product defect at the exact point the product
//! is first really exercised.
//!
//! This encoder writes a genuine truecolour PNG — signature, `IHDR`, one `IDAT` holding a zlib
//! stream of stored (uncompressed) deflate blocks, `IEND`, correct CRC-32 per chunk and a correct
//! Adler-32. No compression, no dependency: the point is that the bytes decode, not that they are
//! small.

use sha2::{Digest, Sha256};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut framed = kind.to_vec();
    framed.extend_from_slice(body);
    out.extend_from_slice(&framed);
    out.extend_from_slice(&crc32(&framed).to_be_bytes());
}

/// Write a truecolour PNG with no filter bytes (filter type 0 per scanline).
///
/// `rgb` must hold exactly `width * height * 3` bytes — the caller decides the picture, and a
/// short slice would panic deep inside the encoder rather than say what was wrong, so the length
/// is asserted here with the numbers in the message.
pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(
        rgb.len(),
        (width as usize * 3) * height as usize,
        "a {width}x{height} truecolour image needs {} bytes, got {}",
        (width as usize * 3) * height as usize,
        rgb.len()
    );

    let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter: none
        raw.extend_from_slice(&rgb[y * width as usize * 3..(y + 1) * width as usize * 3]);
    }

    // A zlib stream of stored (uncompressed) deflate blocks.
    let mut zlib = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < raw.len() {
        let take = (raw.len() - offset).min(0xffff);
        let last = offset + take == raw.len();
        zlib.push(u8::from(last));
        zlib.extend_from_slice(&(take as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(take as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[offset..offset + take]);
        offset += take;
    }
    let mut adler_a = 1u32;
    let mut adler_b = 0u32;
    for byte in &raw {
        adler_a = (adler_a + u32::from(*byte)) % 65521;
        adler_b = (adler_b + adler_a) % 65521;
    }
    zlib.extend_from_slice(&((adler_b << 16) | adler_a).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    // The library hashes the upload, so the bytes must be stable for a given picture: the tail is
    // what makes two same-size images differ, and it is derived rather than constant so a caller
    // that varies the pixels cannot accidentally collide.
    let _ = Sha256::digest(&png);
    png
}

/// A PNG of `width × height` split into four coloured quadrants.
///
/// Quadrants rather than a gradient, because a transform walk needs pixels it can *name*: a crop
/// of the top-left corner must land on one colour, so "the crop picked the right pixels" is an
/// assertion instead of an opinion.
pub fn quadrant_png(width: u32, height: u32) -> Vec<u8> {
    quadrant_png_tinted(width, height, 0)
}

/// The same picture with `tint` mixed into the top-left quadrant's red channel.
///
/// **Why a caller might need this.** The derivative cache is keyed by
/// `sha256(checksum | preset body)` and looked up **by that key, not by `media_id`**
/// (`preset_store::find_derivative`). Two files with byte-identical content therefore share one
/// derivative row: a walk that uploads the same picture twice gets a `200 image/webp` served from
/// the *other* file's row, and its own file has no derivative at all. It then reads two owned keys
/// where it expects three and fails on "the derivative was not created" — a fixture problem wearing
/// a product problem's clothes.
///
/// The change has to be made **in the pixels**, not in the encoded bytes: flipping a byte inside
/// the zlib stream would invalidate the Adler-32 and turn the file into something the decoder
/// refuses, which is the very failure this function exists to avoid. Colour is the honest lever —
/// the encoder recomputes CRC and Adler, so the image stays valid and only the checksum moves.
pub fn quadrant_png_tinted(width: u32, height: u32, tint: u8) -> Vec<u8> {
    let half_w = width / 2;
    let half_h = height / 2;
    let mut pixels = Vec::with_capacity((width as usize * 3) * height as usize);
    for y in 0..height {
        for x in 0..width {
            let colour: [u8; 3] = match (x < half_w, y < half_h) {
                (true, true) => [tint, 0, 0],
                (false, true) => [0, 255, 0],
                (true, false) => [0, 0, 255],
                (false, false) => [255, 255, 0],
            };
            pixels.extend_from_slice(&colour);
        }
    }
    encode_png(width, height, &pixels)
}
