//! A zip writer, small enough to read in one sitting (REQ-033, slice 4).
//!
//! # Why this exists and why it is hand-written
//!
//! A generated scaffold has to reach the developer as one file they can unpack. The obvious
//! dependency is a zip crate; the reason this is 200 lines instead is that the format's
//! *stored* (uncompressed) entry needs nothing but a CRC-32, and a scaffold is a dozen text
//! files whose total size is already capped at [`crate::scaffold::MAX_ARCHIVE_BYTES`] — 512 KB.
//! Compressing 40 KB of source with a deflate implementation to save a few kilobytes is not
//! worth a dependency whose transitive tree a security-conscious platform has to audit.
//!
//! [`crate::scaffold::MAX_ARCHIVE_BYTES`] is the argument for not compressing: the response is
//! measured against the cap, and an archive that expands *after* the cap was checked is an
//! archive nobody bounded.
//!
//! # What the format requires
//!
//! A zip is three parts:
//!
//! 1. one **local file header** per entry, immediately followed by the entry's bytes;
//! 2. one **central directory header** per entry, carrying the offsets from part 1;
////! 3. the **end of central directory** record, carrying the offset and count of part 2.
//!
//! Every multi-byte field is **little endian**. Three details are load-bearing and each of them
//! is a file that `unzip` rejects with a cheerful "not a zip file" and no other clue:
//!
//! * The local header's `version needed to extract` is 20 (2.0). Readers that see 10 assume
//!   the entry may be compressed and then hand the reader a stream they must inflate; we write
//!   `stored`, and 20 is the version that says "attributes/extra fields are present".
//! * The **data descriptor** bit (bit 3 of the general-purpose flags) is **not** set, and the
//!   sizes in the local header are therefore real rather than placeholders. Setting it would
//!   make the sizes authoritative only after the entry, which some readers and not others
//!   consult — the classic "works in Finder, fails in unzip" bug.
//! * The central directory's *relative offset of the local header* is what lets a reader seek
//!   to an entry without reading the whole archive, and it is the field most often written as
//!   zero by a hand-rolled writer. Every test below round-trips through this module's own
//!   reader, and `scripts/qa/probe-scaffold-archive.py` opens the real bytes with Python's
//!   `zipfile` — two independent readers, because a writer and a reader written by the same
//!   hand agree on their own mistake.
//!
//! # Timestamps
//!
//! The DOS timestamp is written from a caller-supplied [`OffsetDateTime`], never from
//! `now_utc()` at write time. Generation is meant to be reproducible: the same request must
//! produce the same bytes, or "compare two generated starters" — a thing a developer does when
//! a template changes — silently reports a difference that is only the clock.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::scaffold::Scaffold;

/// The largest number of bytes this writer will encode, and the largest a DOS time can express.
///
/// DOS timestamps are two 16-bit fields with second resolution and a year offset of 1980, so a
/// date before 1980 is not representable. Rather than let a caller pass one and have a reader
/// disagree about what it means, [`ms_dos_time`] clamps to 1980-01-01 and the caller is not
/// told, because the archive's own `created_at` is the authority for when it was generated and
/// the zip field is metadata.
const MAX_ARCHIVE: usize = 4 * 1024 * 1024;

/// Fixed MS-DOS epoch: 1980-01-01 00:00:00, the earliest a DOS timestamp can name.
///
/// Built through a `const`-incompatible constructor on purpose: `Date::from_calendar_date`
/// returns a `Result`, and the only date that can fail to build here is a literal one. A
/// `LazyLock` or a `fn` would work too; what does *not* work is calling `.expect()` in a const,
/// which is why this is a `fn` returning the value and the two call sites below say what they
/// mean.
fn dos_epoch() -> PrimitiveDateTime {
    PrimitiveDateTime::new(
        Date::from_calendar_date(1980, Month::January, 1).expect("1980-01-01 is a valid date"),
        Time::MIDNIGHT,
    )
}

/// One entry's bytes inside the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// The path inside the archive, with `/` separators.
    ///
    /// Validated by the caller's own slug rules; this writer writes what it is given, because a
    /// writer that silently rewrote a path would hide the very traversal the caller is being
    /// protected from.
    pub path: String,
    /// UTF-8 content.
    pub content: String,
}

impl ArchiveEntry {
    /// Build an entry.
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// The bytes of a generated archive, and what the writer had to report about them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The encoded zip.
    pub bytes: Vec<u8>,
    /// How many entries it holds.
    pub entry_count: usize,
    /// The uncompressed total — the same number `Scaffold::byte_size` carries.
    pub byte_size: usize,
}

/// Encode entries as a stored-entry zip.
///
/// # Panics
///
/// Panics if the entries exceed [`MAX_ARCHIVE`]. That is a programming error, not an input
/// one: the caller passes a validated [`Scaffold`], whose own cap is checked before generation,
/// so reaching this ceiling means a new template grew past what the cap promised.
pub fn zip(entries: &[ArchiveEntry], stamp: OffsetDateTime) -> Archive {
    let (dostime, dosdate) = ms_dos_time(stamp);
    let mut out: Vec<u8> =
        Vec::with_capacity(64 + entries.iter().map(|e| e.content.len() + 64).sum::<usize>());

    // Part 1 and part 2 need each other: the central directory records where each local header
    // started, and the end record needs where the directory started. Both are only knowable
    // while writing, so the offsets are collected on the way through part 1 and the directory is
    // appended afterwards. Doing it the other way round means either a second pass over the
    // bytes or a guess, and a guess here produces an archive that unzips on one machine.
    let mut directory: Vec<u8> = Vec::new();
    let mut byte_size = 0usize;

    for entry in entries {
        let offset = out.len() as u32;
        let name = entry.path.as_bytes();
        let crc = crc32(entry.content.as_bytes());
        let size = entry.content.len() as u32;

        // Local file header: signature, version 2.0, flags 0 (no descriptor, no UTF-8 bit —
        // the names here are ASCII slugs, and setting bit 11 for a name that does not need it
        // is a lie a strict reader may act on), method 0, mtime, mdate, crc, sizes, name length.
        push_u32(&mut out, 0x0403_4b50);
        push_u16(&mut out, 20);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, dostime);
        push_u16(&mut out, dosdate);
        push_u32(&mut out, crc);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0); // extra field length
        out.extend_from_slice(name);
        out.extend_from_slice(entry.content.as_bytes());

        // Central directory header for the same entry.
        push_u32(&mut directory, 0x0201_4b50);
        push_u16(&mut directory, 20); // version made by
        push_u16(&mut directory, 20); // version needed
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, dostime);
        push_u16(&mut directory, dosdate);
        push_u32(&mut directory, crc);
        push_u32(&mut directory, size);
        push_u32(&mut directory, size);
        push_u16(&mut directory, name.len() as u16);
        push_u16(&mut directory, 0); // extra
        push_u16(&mut directory, 0); // comment
        push_u16(&mut directory, 0); // disk number start
        push_u16(&mut directory, 0); // internal attributes
        // 0o100644: a regular file, rw-r--r--. Without the "regular file" bits a reader may
        // treat the entry as a directory, and some then drop the payload on extract.
        push_u32(&mut directory, 0o100644 << 16);
        push_u32(&mut directory, offset);
        directory.extend_from_slice(name);

        byte_size += entry.content.len();
    }

    if out.len() + directory.len() > MAX_ARCHIVE {
        panic!(
            "a scaffold archive of {} bytes exceeds the {MAX_ARCHIVE}-byte ceiling",
            out.len() + directory.len()
        );
    }

    let directory_offset = out.len() as u32;
    let directory_size = directory.len() as u32;
    out.extend_from_slice(&directory);

    // End of central directory. The disk numbers are 0 and the counts are `entries` twice: once
    // for "on this disk" and once for "in total". A reader that checks only the second against
    // the directory it found and the first against zero is the common shape, and writing only
    // one of the two is how an archive opens in one unzip and not another.
    push_u32(&mut out, 0x0605_4b50);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, entries.len() as u16);
    push_u16(&mut out, entries.len() as u16);
    push_u32(&mut out, directory_size);
    push_u32(&mut out, directory_offset);
    push_u16(&mut out, 0); // comment length

    Archive {
        bytes: out,
        entry_count: entries.len(),
        byte_size,
    }
}

/// Every file of a scaffold as archive entries, hidden ones included.
///
/// The preview in the API response filters `shown_in_preview` — a tree of dotfiles is noise in
/// a panel — but the *archive* must contain the `.env.example` the README warns about, or a
/// developer unpacks a starter that tells them to set a variable and has no example to copy.
/// A scaffold that previews one set of files and downloads another is a defect that only shows
/// up after unzipping, which is the worst place to find it.
pub fn entries_of(scaffold: &Scaffold) -> Vec<ArchiveEntry> {
    scaffold
        .files
        .iter()
        .map(|file| ArchiveEntry::new(file.path.clone(), file.content.clone()))
        .collect()
}

/// The archive's filename for a scaffold, as a download it will be saved under.
///
/// `omnion-plugin-my-plugin-live.zip` — the vendor prefix, the slug, the target and the
/// extension, in that order, so a downloads folder with six starters in it is still readable.
///
/// [`Scaffold::slug`] is *already* `"{kind}-{name}"`, which is what makes the difference between
/// two archives with the same name legible. The first version of this function also wrote
/// `scaffold.kind.as_str()`, producing `omnion-plugin-plugin-my-plugin-live.zip`: correct, still
/// unpackable, and the first thing anyone reading the downloads folder would have to notice.
/// The kind is therefore taken from the slug and the test asserts the exact string, so the
/// duplication cannot come back unnoticed.
///
/// The name is built from values the slug rule already constrained to `[A-Za-z0-9_-]`, so it is
/// safe in a `Content-Disposition` filename: no quote, no slash, no newline, and no header
/// injection through it.
pub fn filename_for(scaffold: &Scaffold) -> String {
    format!(
        "omnion-{}-{}.zip",
        scaffold.slug(),
        scaffold.target.as_str()
    )
}

/// A CRC-32 (IEEE 802.3, the polynomial zip uses), computed without a table.
///
/// A table would be 1 KB of source for a function called a dozen times on files of a few
/// kilobytes; the bitwise form is one line of arithmetic and this is not a hot path. The
/// reflected form of 0xEDB88320 is the whole algorithm: xor the low byte, shift right, and
/// mask the top bit with the polynomial.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = if crc & 1 == 0 { 0 } else { 0xEDB8_8320 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Split an instant into the two DOS fields, clamping to the representable range.
///
/// DOS time is `(hour << 11) | (minute << 5) | (second / 2)` and the date is
/// `((year - 1980) << 9) | (month << 5) | day`. Second resolution is two seconds, which is
/// deliberate in the format and not something to round "correctly" here: a value that is not
/// even is floored, so a generated archive never claims a time later than it was made.
fn ms_dos_time(stamp: OffsetDateTime) -> (u16, u16) {
    let clamped = PrimitiveDateTime::new(stamp.date(), stamp.time()).max(dos_epoch());
    let date = clamped.date();
    let time_of_day = clamped.time();

    // `time::Time` keeps its fields private behind accessors, so the values come from
    // `hour()` / `minute()` / `second()`. The clamp above already guarantees the range, but a
    // `try_from` that fell back to 0 would be a silent wrong date, so the conversions are
    // total by construction: `hour` is 0–23, `minute` 0–59, `second` 0–59.
    let dos_time = u16::try_from(
        (u16::from(time_of_day.hour()) << 11)
            | (u16::from(time_of_day.minute()) << 5)
            | (u16::from(time_of_day.second() / 2)),
    )
    .unwrap_or(0);

    // Every term is `i32` because the year offset is a subtraction: a year before 1980 is
    // negative before the clamp, and mixing that with a `u16` month is the E0277 this comment
    // exists to prevent someone from re-introducing.
    let dos_date = u16::try_from(
        ((i32::from(date.year()) - 1980) << 9)
            | (i32::from(u8::from(date.month())) << 5)
            | i32::from(date.day()),
    )
    .unwrap_or(0);
    (dos_time, dos_date)
}

// ---------------------------------------------------------------------------------------------
// A reader, so the tests above are not asking the writer to check its own homework
// ---------------------------------------------------------------------------------------------

/// One entry as read back out of an archive.
///
/// The point of this type is the `Entry` that `read` returns for a file the caller asked for:
/// it is a *failure* when the bytes cannot be recovered, and a test that only checked the header
/// would pass against an archive whose payload is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadEntry {
    /// The path stored in the archive.
    pub path: String,
    /// The bytes stored at that path.
    pub content: String,
}

/// Read one entry out of an archive this module wrote.
///
/// Walks the end-of-central-directory record backwards, then the directory, then the local
/// header the directory points at, and finally the payload. It verifies the stored CRC against
/// the bytes it found, so a truncated or mis-offset archive fails here rather than producing a
/// `.env.example` of zeroes.
pub fn read(bytes: &[u8], wanted: &str) -> Option<ReadEntry> {
    // The end record is last, but a trailing comment may follow it, so scan backwards for the
    // signature rather than assuming an exact offset from the end.
    //
    // The bound is inclusive because the record is 22 bytes long: its first byte sits at
    // `len - 22` when the comment is empty, and `0..=len - 22` is the range that contains it.
    // Writing `0..len.saturating_sub(22)` — the tempting off-by-two — excludes the only offset
    // a commentless archive has. An *empty* archive is the case that proves it: the end record is
    // then the whole file, at offset 0, and the half-open range never reaches it.
    let eocd = (0..=bytes.len().saturating_sub(22))
        .rev()
        .find(|offset| u32_at(bytes, *offset) == Some(0x0605_4b50))?;

    let count = u16_at(bytes, eocd + 10)? as usize;
    let directory_offset = u32_at(bytes, eocd + 16)? as usize;

    let mut cursor = directory_offset;
    for _ in 0..count {
        if u32_at(bytes, cursor)? != 0x0201_4b50 {
            return None;
        }
        let method = u16_at(bytes, cursor + 10)?;
        if method != 0 {
            // This reader only handles what this writer produces. A `stored`-only reader that
            // silently accepted method 8 would be a reader that guesses.
            return None;
        }
        let crc = u32_at(bytes, cursor + 16)?;
        let size = u32_at(bytes, cursor + 24)? as usize;
        let name_len = u16_at(bytes, cursor + 28)? as usize;
        let offset = u32_at(bytes, cursor + 42)? as usize;
        let name_start = cursor + 46;
        let name = std::str::from_utf8(bytes.get(name_start..name_start + name_len)?).ok()?;

        if name == wanted {
            // The local header repeats the name, and its length is the authority for where the
            // payload begins — not the directory's, which a corrupt archive can disagree with.
            if u32_at(bytes, offset)? != 0x0403_4b50 {
                return None;
            }
            let local_name_len = u16_at(bytes, offset + 26)? as usize;
            let local_extra_len = u16_at(bytes, offset + 28)? as usize;
            let start = offset + 30 + local_name_len + local_extra_len;
            let payload = bytes.get(start..start + size)?;
            if crc32(payload) != crc {
                return None;
            }
            return Some(ReadEntry {
                path: name.to_string(),
                content: String::from_utf8(payload.to_vec()).ok()?,
            });
        }

        cursor = name_start + name_len;
    }
    None
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{ScaffoldKind, ScaffoldTarget};
    use crate::templates::generate as generate_template;

    /// An instant, built from its parts.
    ///
    /// `OffsetDateTime::new_utc` takes a `Date` and a `Time` — two arguments, not a
    /// `PrimitiveDateTime` — so the helper exists to keep three call sites from re-deriving that
    /// and to make the date in each test visible at the call site that cares about it.
    fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        OffsetDateTime::new_utc(
            Date::from_calendar_date(year, month, day).expect("a calendar date in range"),
            Time::from_hms(hour, minute, second).expect("a time in range"),
        )
    }

    fn stamp() -> OffsetDateTime {
        at(2026, Month::October, 2, 14, 30, 40)
    }

    fn sample() -> Vec<ArchiveEntry> {
        vec![
            ArchiveEntry::new(
                "my-plugin/package.json",
                "{\n  \"name\": \"my-plugin\"\n}\n",
            ),
            ArchiveEntry::new("my-plugin/index.ts", "export const routes = [];\n"),
            ArchiveEntry::new("my-plugin/.env.example", "OMNION_API_URL=\n"),
        ]
    }

    // ── the format ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn an_archive_round_trips_every_entry_with_its_bytes() {
        let archive = zip(&sample(), stamp());
        for entry in sample() {
            let back = read(&archive.bytes, &entry.path)
                .unwrap_or_else(|| panic!("{} went missing", entry.path));
            assert_eq!(back.path, entry.path);
            assert_eq!(back.content, entry.content);
        }
    }

    #[test]
    fn the_end_record_points_at_a_directory_that_starts_where_the_local_headers_stopped() {
        let entries = sample();
        let archive = zip(&entries, stamp());
        let bytes = &archive.bytes;

        let eocd = (0..=bytes.len() - 22)
            .rev()
            .find(|offset| u32_at(bytes, *offset) == Some(0x0605_4b50))
            .expect("no end-of-central-directory record");
        let directory_offset = u32_at(bytes, eocd + 16).unwrap() as usize;
        assert_eq!(u32_at(bytes, directory_offset), Some(0x0201_4b50));

        // Every local header the directory names must be found, and each must name its own path.
        // A wrong relative offset is the classic hand-rolled-writer bug and it is invisible to
        // a test that only ever reads the entry it happens to ask for first.
        let count = u16_at(bytes, eocd + 10).unwrap() as usize;
        let mut cursor = directory_offset;
        let mut named = Vec::new();
        for _ in 0..count {
            let offset = u32_at(bytes, cursor + 42).unwrap() as usize;
            let name_len = u16_at(bytes, cursor + 28).unwrap() as usize;
            let name = std::str::from_utf8(&bytes[cursor + 46..cursor + 46 + name_len])
                .unwrap()
                .to_string();
            assert_eq!(
                u32_at(bytes, offset),
                Some(0x0403_4b50),
                "{name} has no local header"
            );
            let local_name_len = u16_at(bytes, offset + 26).unwrap() as usize;
            let local_name =
                std::str::from_utf8(&bytes[offset + 30..offset + 30 + local_name_len]).unwrap();
            assert_eq!(local_name, name, "the two headers disagree about the path");
            named.push(name);
            cursor += 46 + name_len;
        }
        assert_eq!(
            named,
            vec![
                "my-plugin/package.json",
                "my-plugin/index.ts",
                "my-plugin/.env.example"
            ]
        );
    }

    #[test]
    fn entries_are_stored_not_deflated_so_the_crc_matches_the_bytes_on_disk() {
        let archive = zip(&sample(), stamp());
        let entry = read(&archive.bytes, "my-plugin/index.ts").unwrap();
        // If the payload were compressed, the reader above would have refused on the method
        // byte; this asserts the thing that matters at the end of the chain — the CRC in the
        // central directory equals the CRC of the bytes that came back.
        assert!(
            archive
                .bytes
                .windows(entry.content.len())
                .any(|window| window == entry.content.as_bytes())
        );
    }

    #[test]
    fn the_counts_appear_in_both_halves_of_the_end_record() {
        let archive = zip(&sample(), stamp());
        let bytes = &archive.bytes;
        let eocd = (0..=bytes.len() - 22)
            .rev()
            .find(|offset| u32_at(bytes, *offset) == Some(0x0605_4b50))
            .unwrap();
        // A reader that consults "entries on this disk" and one that consults "entries in
        // total" are both common; writing one and leaving the other at zero produces an archive
        // that opens in some tools and reports an empty one in others.
        assert_eq!(u16_at(bytes, eocd + 8).unwrap(), 3);
        assert_eq!(u16_at(bytes, eocd + 10).unwrap(), 3);
    }

    // ── the writer's promises ────────────────────────────────────────────────────────────────

    #[test]
    fn a_generated_scaffold_archives_with_its_hidden_files_and_previews_fewer() {
        // The defect this guards: a writer built from the API's *preview* list would drop the
        // `.env.example` and the `.gitignore`, and the developer would only find out after
        // unzipping — the one moment they cannot ask the platform a question.
        let scaffold = generate_template(ScaffoldKind::Plugin, "my-plugin", ScaffoldTarget::Live)
            .expect("the plugin template generates");

        let shown = scaffold
            .files
            .iter()
            .filter(|file| file.shown_in_preview)
            .count();
        let all = scaffold.files.len();
        assert!(
            all > shown,
            "the template stopped having hidden files to lose"
        );

        let archive = zip(&entries_of(&scaffold), stamp());
        assert_eq!(archive.entry_count, all);
        for file in &scaffold.files {
            let back = read(&archive.bytes, &file.path)
                .unwrap_or_else(|| panic!("{} is missing from the archive", file.path));
            assert_eq!(back.content, file.content);
        }
    }

    #[test]
    fn the_archive_size_and_the_scaffolds_own_figure_are_the_same_number() {
        let scaffold =
            generate_template(ScaffoldKind::Theme, "acme-theme", ScaffoldTarget::Sandbox)
                .expect("the theme template generates");
        let archive = zip(&entries_of(&scaffold), stamp());
        // `Scaffold::byte_size` is what the panel shows and what `sdk_scaffolds.byte_size` is
        // checked against by a constraint; a zip writer that reported a different total would
        // put two numbers for one thing on the same screen.
        assert_eq!(archive.byte_size, scaffold.byte_size);
    }

    #[test]
    fn generation_is_reproducible_because_the_stamp_is_the_callers() {
        // Two generations a second apart must be byte-identical, or "diff the two starters" is
        // a diff of the clock. This is why `zip` takes the timestamp instead of reading it.
        let one = zip(&sample(), stamp());
        let two = zip(&sample(), stamp());
        assert_eq!(one.bytes, two.bytes);
        let later = zip(&sample(), stamp() + time::Duration::seconds(3600));
        assert_ne!(
            one.bytes, later.bytes,
            "the timestamp is not reaching the bytes"
        );
    }

    #[test]
    fn a_pre_dos_date_is_clamped_rather_than_written_as_garbage() {
        // 1979 is not representable. Writing the raw year offset would produce a date field
        // that some readers resolve to a year in the 2100s; clamping names the earliest instant
        // the format can express.
        let ancient = at(1979, Month::December, 31, 0, 0, 0);
        let (_, date) = ms_dos_time(ancient);
        assert_eq!(
            date & 0b1_1111_1111,
            1 | (1 << 5),
            "the clamped date is not 1980-01-01"
        );
    }

    #[test]
    fn seconds_are_floored_to_the_two_second_resolution_the_format_has() {
        let odd = at(2026, Month::October, 2, 14, 30, 41);
        let (odd_time, _) = ms_dos_time(odd);
        let (even_time, _) = ms_dos_time(odd - time::Duration::seconds(1));
        assert_eq!(
            odd_time, even_time,
            "an odd second was rounded up, not floored"
        );
    }

    #[test]
    fn a_corrupt_payload_is_refused_rather_than_returned() {
        // Truncating the archive must not yield an entry with a plausible-looking body: the
        // reader checks the CRC, and this is the assertion that the check is reached.
        let mut archive = zip(&sample(), stamp());
        const PAYLOAD: &[u8] = b"export const routes = [];\n";
        let marker = archive
            .bytes
            .windows(PAYLOAD.len())
            .position(|window| window == PAYLOAD)
            .expect("the payload is not in the bytes at all");
        archive.bytes[marker] = b'X';
        assert!(read(&archive.bytes, "my-plugin/index.ts").is_none());
    }

    #[test]
    fn an_empty_archive_is_still_an_archive() {
        // `zip()` of nothing has to emit the end record, or a scaffold with no files produces
        // a 0-byte download that a reader calls "not a zip file" rather than "no files".
        let archive = zip(&[], stamp());
        assert_eq!(archive.entry_count, 0);
        assert_eq!(archive.byte_size, 0);
        assert!(
            u32_at(&archive.bytes, 0) == Some(0x0605_4b50),
            "no end record at offset 0"
        );
    }

    #[test]
    fn the_download_name_is_built_only_from_slug_characters() {
        // `filename_for` feeds a `Content-Disposition` header. A name carrying a quote, a
        // newline or a slash there is a header injection, so the test is about the alphabet
        // rather than about the formatting.
        let scaffold = generate_template(ScaffoldKind::Plugin, "my-plugin", ScaffoldTarget::Live)
            .expect("the plugin template generates");
        let name = filename_for(&scaffold);
        assert_eq!(name, "omnion-plugin-my-plugin-live.zip");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
            "{name} carries a character that has no business in a header"
        );
    }

    #[test]
    fn crc32_matches_the_known_vector() {
        // The published check value for 0xEDB88320. Without it a refactor that inverted the
        // input or the output would still round-trip through this module's own reader, and the
        // archive would be rejected by every real unzip.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
