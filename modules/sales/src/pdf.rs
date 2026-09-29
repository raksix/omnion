//! A PDF writer small enough to read in one sitting, and the two documents built with it.
//!
//! ## Why the module writes its own PDF
//!
//! REQ-052's last box asks for "a real document … (verified by opening it)", and REQ-029 — the
//! platform's general document engine — is **not built**. Three answers were possible: wait for
//! REQ-029, print an HTML page and call it a PDF, or write the document. This module is the third
//! one, and the reason it is here rather than in `crates/` is deliberate: a PDF writer is
//! infrastructure the whole platform would want, but until a second request needs it, moving it
//! would be a guess about a module that does not exist yet. **It lives with the two documents
//! that actually need it**, and it is written so that moving it later is a `git mv` plus a
//! `pub use`, not a rewrite.
//!
//! ## Why there is no dependency
//!
//! This is a **public repository**, and a sales document is the one artifact that has to be
//! reproducible by anybody who checks the tag out. A font-embedding crate is a tree of typefaces
//! measured in megabytes; `printpdf` would make the crate's build depend on a font the platform
//! does not ship. A PDF 1.4 file is a cross-reference table and a content stream, and the part of
//! it a quote needs — a header, a line grid, a totals block, page breaks — is about four hundred
//! lines. The cost is paid once, in code anybody can read; the alternative is paid forever, in
//! bytes downloaded on every build.
//!
//! ## What a PDF 1.4 document can and cannot print
//!
//! The base-14 fonts (`Helvetica`, `Helvetica-Bold`) are what a viewer is required to have, and
//! their standard encoding is **WinAnsi** (cp1252). Every character below U+00A0 and a good part
//! of Latin-1 is in it, so "Şirket Ltd." prints correctly. Characters outside it — CJK, Greek,
//! Cyrillic, and a few Latin letters Turkish needs (`ğ`) — have no glyph in a base-14 font at
//! all, and a viewer draws nothing for them. [`encode_text`] therefore **replaces** what it cannot
//! render and **counts** the replacements, and [`Document::text`] records the count per string so
//! a caller can say "this document has 3 characters that will not print" instead of shipping a
//! customer's name with holes in it. A missing glyph silently prints as nothing, which is the worst
//! outcome available: the file opens, the totals are right, and the recipient does not know that
//! part of the document is missing.
//!
//! Embedding a Unicode font would fix that and would also make this module a font-subsetting
//! engine. The honest trade is documented rather than hidden, and the count is exposed so the
//! answer can be revisited when a second document needs it.

// ---------------------------------------------------------------------------------------------
// Page geometry — A4 in PostScript points (1 pt = 1/72 inch).
// ---------------------------------------------------------------------------------------------

/// A4's width in points.
const PAGE_WIDTH: f32 = 595.28;
/// A4's height in points.
const PAGE_HEIGHT: f32 = 841.89;
/// The margin on every side.
const MARGIN: f32 = 48.0;
/// The width a line of text may occupy.
const CONTENT_WIDTH: f32 = PAGE_WIDTH - (2.0 * MARGIN);
/// Where the body may not go below: the footer sits under this.
const BODY_BOTTOM: f32 = 72.0;
/// The height of one table row.
const ROW_HEIGHT: f32 = 15.0;
/// The extra gap under a table's header row.
const HEADER_GAP: f32 = 6.0;
/// The widest a line of text may be drawn before it is wrapped.
const MAX_LINE: f32 = 520.0;

// The body font sizes. Named rather than inlined because a reader changing one of them by hand
// has to find all of them, and because the header and the grid are deliberately different sizes.
const FONT_SIZE_BODY: f32 = 9.0;
const FONT_SIZE_SMALL: f32 = 8.0;
const FONT_SIZE_TITLE: f32 = 16.0;

// ---------------------------------------------------------------------------------------------
// Helvetica metrics
// ---------------------------------------------------------------------------------------------

/// The width of every printable ASCII character in `Helvetica`, in 1/1000 em.
///
/// This is the standard Adobe AFM table for the base-14 font. It is here for one reason: a money
/// column is **right-aligned**, and right-alignment means measuring the string. Without widths a
/// document can only left-align its amounts, which reads as a spreadsheet rather than a document.
/// A base-14 font's metrics are fixed by the specification, so this table never goes stale and
/// needs no font file to be correct.
#[rustfmt::skip]
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // ' ' .. '/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // '0' .. '9'
    278, 278, 584, 584, 584, 556, 1015, // ':' .. '@'
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, // 'A' .. 'P'
    778, 722, 667, 611, 722, 667, 944, 667, 667, 611, // 'Q' .. 'Z'
    278, 278, 278, 469, 556, 333, // '[' .. '`'
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, // 'a' .. 'p'
    556, 333, 500, 278, 556, 500, 722, 500, 500, 500, // 'q' .. 'z'
    334, 260, 334, 584, // '{' .. '~'
];

/// The width of `Helvetica-Bold`, for the same range.
///
/// Only the characters the header actually uses are correct here; the rest fall back to
/// [`HELVETICA_WIDTHS`], which is right for the letters it covers and a few points out for a few
/// others. A bold headline measured three points narrow is invisible; a bold **money** figure
/// measured by the regular table is not used anywhere, because every amount is set in the body
/// face.
#[rustfmt::skip]
const HELVETICA_BOLD_WIDTHS: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    333, 333, 584, 584, 584, 611, 975,
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667,
    778, 722, 667, 611, 722, 667, 944, 667, 667, 611,
    333, 278, 333, 584, 556, 333,
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611,
    611, 389, 556, 333, 611, 556, 778, 556, 556, 500,
    389, 280, 389, 584,
];

/// The width of a character in the chosen face, in 1/1000 em.
///
/// A character outside the table (an accented letter, a currency sign) takes the **average** of
/// its face's table rather than a width of zero, so a string of unknown characters is laid out as
/// approximately the right size instead of collapsing to nothing.
fn char_width(ch: char, bold: bool) -> f32 {
    let code = ch as u32;
    if (0x20..=0x7E).contains(&code) {
        let index = (code - 0x20) as usize;
        let table = if bold {
            &HELVETICA_BOLD_WIDTHS
        } else {
            &HELVETICA_WIDTHS
        };
        return f32::from(table[index]);
    }
    let table = if bold {
        &HELVETICA_BOLD_WIDTHS
    } else {
        &HELVETICA_WIDTHS
    };
    let total: u32 = table.iter().map(|w| u32::from(*w)).sum();
    total as f32 / table.len() as f32
}

/// The width of a string at a given size, in points.
#[must_use]
pub fn text_width(text: &str, size: f32, bold: bool) -> f32 {
    let thousandths: f32 = text.chars().map(|ch| char_width(ch, bold)).sum();
    thousandths * size / 1000.0
}

/// One character the base-14 fonts cannot draw, and what the document prints instead.
const UNRENDERABLE: char = '?';

/// How a run of text became printable, and what was lost doing it.
///
/// The count is the whole point: a PDF that silently drops a customer's `ğ` opens perfectly and
/// is wrong, and the recipient has no way to know. [`Document::unrenderable_characters`] is what a
/// route turns into a response header.
///
/// It counts **both** kinds of loss, deliberately, because a caller who needs to warn somebody
/// does not care which of the two happened:
///
/// * a character the base-14 fonts have no glyph for at all (CJK, Greek, Cyrillic), drawn as `?`;
/// * a character that has no *exact* glyph in cp1252 but a readable near-miss (the Turkish
///   `ı`/`İ`/`ğ`/`Ğ`), drawn as the closest letter.
///
/// The second is the more dangerous of the two, which is the opposite of what it looks like. A `?`
/// is visible; "Sirket" printed for "Şirket İğde" reads as a correct name that is not the right
/// one, and a Turkish organization emailing a Turkish customer is exactly where that lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Encoding {
    /// Characters that could not be drawn exactly, in either of the two ways above.
    pub replaced: usize,
}

/// Encode text into WinAnsi, escaping the three characters a PDF literal string reserves.
///
/// `(` and `)` close and open a literal string, and `\` escapes — a customer whose company is
/// `Acme (Holdings) Ltd.` is not a rare thing, and an unescaped parenthesis is a file that does not
/// open. The replacement is a `?` rather than a space, because a space in a name reads as a typo
/// and a `?` reads as "something is here that is not right".
#[must_use]
pub fn encode_text(text: &str) -> (Vec<u8>, Encoding) {
    let mut out = Vec::with_capacity(text.len());
    let mut encoding = Encoding::default();
    for ch in text.chars() {
        match ch {
            '(' => out.extend_from_slice(b"\\("),
            ')' => out.extend_from_slice(b"\\)"),
            '\\' => out.extend_from_slice(b"\\\\"),
            // Readable near-miss: drawn as the closest letter, and counted, because "silket"
            // printed for "Şirket İğde" is a wrong name rather than an obvious hole.
            _ => match turkish_alias(ch) {
                Some(byte) => {
                    encoding.replaced += 1;
                    out.push(byte);
                }
                None => match winansi(ch) {
                    Some(byte) => out.push(byte),
                    None => {
                        encoding.replaced += 1;
                        out.push(b'?');
                    }
                },
            },
        }
    }
    (out, encoding)
}

/// The nearest readable ASCII for a letter cp1252 does not carry, if there is one.
///
/// `ı`, `İ`, `ğ`, `Ğ` **and `Ş`/`ş`** are Turkish additions to the Latin alphabet that post-date
/// cp1252, so a base-14 font has no glyph for any of them. A reader can still make sense of
/// `i`/`I`/`g`/`G`/`S`/`s` where the original was meant, which is a different situation from a CJK
/// character that has nothing nearby — so these are mapped, while everything else is drawn as `?`.
///
/// The cedilla pair is the one that matters most and the one everybody gets wrong: **cp1252's
/// 0x8A/0x9A are `Š` and `ş`, the carons — not `Ş` and `ş`, the cedillas.** The two look identical
/// in most fonts and differ by one bit in the encoding, and a table written from memory puts the
/// cedilla on the caron's slot, which prints "Sirket" for "Şirket" while every test that only
/// reads ASCII passes. The mapping below is verified with `bytes([0x8A]).decode("cp1252")`, and
/// [`the_s_cedilla_and_the_s_caron_are_different_characters`] pins it.
fn turkish_alias(ch: char) -> Option<u8> {
    Some(match ch {
        'ı' => b'i',
        'İ' => b'I',
        'ğ' => b'g',
        'Ğ' => b'G',
        'ş' => b's',
        'Ş' => b'S',
        _ => return None,
    })
}

/// The cp1252 byte for a character, or `None` when the base-14 fonts cannot draw it.
fn winansi(ch: char) -> Option<u8> {
    let code = ch as u32;
    // The 0x20..=0x7E range is identical in ASCII and cp1252.
    if (0x20..=0x7E).contains(&code) {
        return Some(code as u8);
    }
    let byte = match ch {
        '\u{20AC}' => 0x80, // €
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85, // …
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89, // %
        '\u{0160}' => 0x8A, // Š — verified: cp1252 0x8A is U+0160
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C, // Œ
        '\u{017D}' => 0x8E, // Ž — verified: cp1252 0x8E is U+017D, not a letter Turkish uses
        '\u{2018}' => 0x91, // '
        '\u{2019}' => 0x92, // '
        '\u{201C}' => 0x93, // "
        '\u{201D}' => 0x94, // "
        '\u{2022}' => 0x95, // •
        '\u{2013}' => 0x96, // –
        '\u{2014}' => 0x97, // —
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99, // ™
        '\u{0161}' => 0x9A, // ş — verified: cp1252 0x9A is U+0161
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C, // œ
        '\u{017E}' => 0x9E, // ž — the pair of 0x8E
        '\u{0178}' => 0x9F, // Ÿ
        '\u{00A0}' => 0xA0, // non-breaking space
        '¡' => 0xA1,
        '¢' => 0xA2,
        '£' => 0xA3,
        '¤' => 0xA4,
        '¥' => 0xA5,
        '¦' => 0xA6,
        '§' => 0xA7,
        '¨' => 0xA8,
        '©' => 0xA9,
        'ª' => 0xAA,
        '«' => 0xAB,
        '¬' => 0xAC,
        '\u{00AD}' => 0xAD, // soft hyphen
        '®' => 0xAE,
        '¯' => 0xAF,
        '°' => 0xB0,
        '±' => 0xB1,
        '²' => 0xB2,
        '³' => 0xB3,
        '´' => 0xB4,
        'µ' => 0xB5,
        '¶' => 0xB6,
        '·' => 0xB7,
        '¸' => 0xB8,
        '¹' => 0xB9,
        'º' => 0xBA,
        '»' => 0xBB,
        '¼' => 0xBC,
        '½' => 0xBD,
        '¾' => 0xBE,
        '¿' => 0xBF,
        'À' => 0xC0,
        'Á' => 0xC1,
        'Â' => 0xC2,
        'Ã' => 0xC3,
        'Ä' => 0xC4,
        'Å' => 0xC5,
        'Æ' => 0xC6,
        'Ç' => 0xC7,
        'È' => 0xC8,
        'É' => 0xC9,
        'Ê' => 0xCA,
        'Ë' => 0xCB,
        'Ì' => 0xCC,
        'Í' => 0xCD,
        'Î' => 0xCE,
        'Ï' => 0xCF,
        'Ð' => 0xD0,
        'Ñ' => 0xD1,
        'Ò' => 0xD2,
        'Ó' => 0xD3,
        'Ô' => 0xD4,
        'Õ' => 0xD5,
        'Ö' => 0xD6,
        '×' => 0xD7,
        'Ø' => 0xD8,
        'Ù' => 0xD9,
        'Ú' => 0xDA,
        'Û' => 0xDB,
        'Ü' => 0xDC,
        'Ý' => 0xDD,
        'Þ' => 0xDE,
        'ß' => 0xDF,
        'à' => 0xE0,
        'á' => 0xE1,
        'â' => 0xE2,
        'ã' => 0xE3,
        'ä' => 0xE4,
        'å' => 0xE5,
        'æ' => 0xE6,
        'ç' => 0xE7,
        'è' => 0xE8,
        'é' => 0xE9,
        'ê' => 0xEA,
        'ë' => 0xEB,
        'ì' => 0xEC,
        'í' => 0xED,
        'î' => 0xEE,
        'ï' => 0xEF,
        'ð' => 0xF0,
        'ñ' => 0xF1,
        'ò' => 0xF2,
        'ó' => 0xF3,
        'ô' => 0xF4,
        'õ' => 0xF5,
        'ö' => 0xF6,
        '÷' => 0xF7,
        'ø' => 0xF8,
        'ù' => 0xF9,
        'ú' => 0xFA,
        'û' => 0xFB,
        'ü' => 0xFC,
        'ý' => 0xFD,
        'þ' => 0xFE,
        'ÿ' => 0xFF,
        // Everything cp1252 does not carry — the Turkish `ı`/`İ`/`ğ`/`Ğ` included — is handled
        // by `turkish_alias` (a readable near-miss) or falls through to `?`. Keeping this arm
        // absent is what makes that layering the only path to a substitution: a table entry here
        // would win over the alias table and quietly undo the rule.
        _ => return None,
    };
    Some(byte)
}

// ---------------------------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------------------------

/// One page of content: the content stream plus the y-cursor it was written to.
///
/// **`ops` is a byte buffer, and that is not a detail.** A PDF content stream is binary: the
/// `Tj` operand is a literal string in the font's encoding, which here is **WinAnsi (cp1252)**,
/// not UTF-8. Building the stream as a Rust `String` forces every non-ASCII byte through
/// `from_utf8_lossy`, which replaces bytes like 0x97 (—) and 0x8A (Ş) with U+FFFD — so a Turkish
/// company's name came out of the writer as "S?irket I?ge" while every test that only read ASCII
/// passed. Nothing in a type signature warns about this; the buffer is simply the wrong type for
/// the format.
#[derive(Debug)]
struct Page {
    ops: Vec<u8>,
    cursor: f32,
}

impl Page {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            cursor: PAGE_HEIGHT - MARGIN,
        }
    }

    /// The current page's operator stream as text.
    ///
    /// Lossy **on the way out** and never on the way in: the bytes in this buffer are WinAnsi, so
    /// a decoded copy is a convenience for reading and a fidelity warning at the same time. It
    /// is public because a layout that is asserted on has to be inspectable from the outside —
    /// the alignment test lives in `documents`, which lays out rows with this module's writer and
    /// has no business reaching into a private buffer to check its own arithmetic.
    #[must_use]
    pub fn ops_text(&self) -> String {
        String::from_utf8_lossy(&self.ops).into_owned()
    }
}

/// A PDF under construction.
///
/// The API is deliberately a cursor, not a layout engine: [`text`] draws one string at the current
/// y, [`row`] draws a table row, and [`space`] moves the cursor. A quote is a header, a table, a
/// totals block and a footer — a flow layout, top to bottom — and anything more would be a second
/// layout engine nobody asked for.
#[derive(Debug)]
pub struct Document {
    pages: Vec<Page>,
    encoding: Encoding,
    title: String,
}

impl Document {
    /// Start a document. `title` is both the PDF's own title (what a viewer's window says) and
    /// the default for the file's name.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            pages: vec![Page::new()],
            encoding: Encoding::default(),
            title: title.into(),
        }
    }

    /// How many characters could not be drawn, across the whole document.
    ///
    /// A route surfaces this as a header so the *sender* learns the document is degraded — the
    /// recipient, who has no access to that header, is exactly the person a silent hole in their
    /// own name would hurt.
    #[must_use]
    pub fn unrenderable_characters(&self) -> usize {
        self.encoding.replaced
    }

    /// The document's title, for the PDF's info dictionary.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// How many pages the document has so far.
    ///
    /// The grid uses this to redraw its column headings after a break. The alternative — a
    /// document that decides for itself when it is repeating a header — would be a layout engine
    /// inside the writer, and a page break is a property of **where the cursor is**, not of how
    /// many rows have been drawn: a grid of 40 short descriptions and a grid of 40 long ones break
    /// at different rows, so counting rows to predict a break is wrong for exactly the documents
    /// that need the repeated heading most.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The current page's operator stream as text.
    ///
    /// Public because a caller that *lays out* with this writer has to be able to check its own
    /// arithmetic, and `documents` — which builds the quote and order — is exactly that caller.
    /// Reading a layout through `finish()` instead means decoding the whole serialized file,
    /// which is lossy for exactly the non-ASCII bytes a document is judged on.
    #[must_use]
    pub fn ops_text(&self) -> String {
        self.pages.last().map(Page::ops_text).unwrap_or_default()
    }

    /// Move the cursor down, starting a new page when there is no room left.
    pub fn space(&mut self, by: f32) {
        let page = self
            .pages
            .last_mut()
            .expect("a document always has one page");
        page.cursor -= by;
        if page.cursor < BODY_BOTTOM {
            self.pages.push(Page::new());
        }
    }

    /// Draw one line of text, left-aligned at `x`.
    ///
    /// `x` is absolute rather than relative to the margin because a totals block is aligned to the
    /// **right** margin and a grid has seven columns; a caller that has to re-add `MARGIN` at
    /// every call site will get one of them wrong.
    pub fn text(&mut self, x: f32, size: f32, bold: bool, content: &str) {
        if content.is_empty() {
            return;
        }
        let (encoded, encoding) = encode_text(content);
        self.encoding.replaced += encoding.replaced;
        let page = self
            .pages
            .last_mut()
            .expect("a document always has one page");
        let baseline = page.cursor - size;
        let font = if bold { "/F2" } else { "/F1" };
        // `Td` rather than `Tm`: the text matrix is reset by `ET`, so `Td` is relative to the
        // line start every time and cannot drift down the page over a long document.
        page.ops.extend_from_slice(
            format!("BT {font} {size} Tf 1 0 0 1 {x} {baseline:.2} Td (").as_bytes(),
        );
        // The encoded bytes go in **as bytes**. Routing them through a `String` here is what
        // destroyed every non-ASCII character in the document, and the fix is invisible in a
        // signature: the lossy conversion is an explicit call, not a type error.
        page.ops.extend_from_slice(&encoded);
        page.ops.extend_from_slice(b") Tj ET\n");
    }

    /// Draw one line of text, right-aligned so that it **ends** at `right`.
    ///
    /// This is the whole reason the module carries the width tables. A money column that is left
    /// aligned makes 9.90 and 1,234.50 line up on their first digit, and a document whose amounts
    /// do not line up is a document nobody trusts with a hundred rows on it.
    pub fn text_right(&mut self, right: f32, size: f32, bold: bool, content: &str) {
        if content.is_empty() {
            return;
        }
        let width = text_width(content, size, bold);
        self.text(right - width, size, bold, content);
    }

    /// Draw `content` wrapped to `width`, as one paragraph, and leave the cursor below it.
    ///
    /// Wrapping happens on **words**, and a word longer than the line is broken mid-word rather
    /// than drawn off the page — a customer reference or a long SKU arrives as one unbreakable
    /// token more often than one would like.
    pub fn paragraph(&mut self, x: f32, size: f32, leading: f32, width: f32, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        for line in wrap(content, size, width) {
            self.text(x, size, false, &line);
            self.space(leading);
        }
    }

    /// Draw a horizontal rule the full width of the content area.
    pub fn rule(&mut self) {
        let page = self
            .pages
            .last_mut()
            .expect("a document always has one page");
        let y = page.cursor;
        page.ops.extend_from_slice(
            format!(
                "0.78 0.78 0.78 RG 0.6 w {} {y:.2} m {} {y:.2} l S\n",
                MARGIN,
                PAGE_WIDTH - MARGIN
            )
            .as_bytes(),
        );
    }

    /// Draw a filled rectangle, used for the table's header band.
    pub fn band(&mut self, height: f32, grey: f32) {
        let page = self
            .pages
            .last_mut()
            .expect("a document always has one page");
        let y = page.cursor - height;
        page.ops.extend_from_slice(
            format!(
                "{grey} g {x} {y:.2} {w} {h} re f\n",
                x = MARGIN,
                w = CONTENT_WIDTH,
                h = height
            )
            .as_bytes(),
        );
    }

    /// Write a table row: a band, the cells left or right aligned inside fixed columns, and the
    /// cursor moved past it.
    ///
    /// `columns` is `(x, right_edge, align)`: the caller owns the geometry so the header and the
    /// rows cannot disagree about where a column starts — the alternative, a `Table` type that
    /// decides for itself, is how a totals column ends up 4pt from where the header rule is.
    pub fn row(&mut self, columns: &[(f32, f32, Align)], cells: &[&str], band: bool) {
        if band {
            self.band(ROW_HEIGHT + 4.0, 0.94);
        }
        for ((x, right, align), cell) in columns.iter().zip(cells.iter()) {
            match align {
                Align::Left => self.text(*x, FONT_SIZE_BODY, false, cell),
                Align::Right => self.text_right(*right, FONT_SIZE_BODY, false, cell),
                Align::Center => {
                    let width = text_width(cell, FONT_SIZE_BODY, false);
                    self.text(x - (width / 2.0), FONT_SIZE_BODY, false, cell);
                }
            }
        }
        self.space(if band { ROW_HEIGHT + 4.0 } else { ROW_HEIGHT });
    }

    /// Serialize the document to PDF bytes.
    ///
    /// The structure is the minimal legal PDF 1.4 file: a catalogue, a page tree, one page and one
    /// content stream per page, the two base-14 fonts, and an xref table. The byte offsets in the
    /// table are the whole reason this is hand-written rather than assembled by string
    /// concatenation — a cross-reference table that is off by one byte produces a file that opens
    /// in one viewer and not another, and the difference is invisible in a string diff.
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        // Object numbering, fixed up front so every reference is known before any body is written.
        const CATALOG: usize = 1;
        const PAGES: usize = 2;
        const FIRST_PAGE: usize = 3;
        // Two pages of page-tree node per page (the page and its content stream), then the two
        // font objects at the end.
        let first_font = FIRST_PAGE + (self.pages.len() * 2);
        let regular_font = first_font;
        let bold_font = first_font + 1;
        // One **past** the last font: the info dictionary is a real object and is written after
        // them. Counting it as part of the fonts made the xref table one entry short, so the
        // trailer pointed at an object the table did not list — a file that opens in a lenient
        // reader and is repaired (or refused) by a strict one.
        let total = bold_font + 1;

        let mut objects: Vec<Vec<u8>> = Vec::with_capacity(total);
        let page_ids: Vec<usize> = (0..self.pages.len())
            .map(|index| FIRST_PAGE + (index * 2))
            .collect();

        objects.push(Vec::new()); // 1 — the catalogue, filled at the end
        objects.push(Vec::new()); // 2 — the page tree, filled at the end
        for (index, page) in self.pages.iter().enumerate() {
            // `/Length` counts **bytes**, which is only true because the stream is a byte buffer.
            let mut stream = format!("/Length {} >>\nstream\n", page.ops.len()).into_bytes();
            stream.extend_from_slice(&page.ops);
            stream.extend_from_slice(b"endstream");
            objects.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}] \
                     /Resources << /Font << /F1 {regular_font} 0 R /F2 {bold_font} 0 R >> >> \
                     /Contents {} 0 R >>",
                    FIRST_PAGE + (index * 2) + 1
                )
                .into_bytes(),
            );
            objects.push(stream);
        }
        objects.push(
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        );
        objects.push(
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        );
        objects.push(
            format!(
                "<< /Title ({}) /Producer (Omnion Sales) /Creator (Omnion) >>",
                pdf_literal(&self.title)
            )
            .into_bytes(),
        );

        let kids = page_ids
            .iter()
            .map(|id| format!("{id} 0 R"))
            .collect::<Vec<_>>()
            .join(" ");
        objects[CATALOG - 1] = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();
        objects[PAGES - 1] = format!(
            "<< /Type /Pages /Kids [{kids}] /Count {} >>",
            page_ids.len()
        )
        .into_bytes();

        let mut out: Vec<u8> = Vec::with_capacity(4096);
        out.extend_from_slice(b"%PDF-1.4\n");
        // A binary comment: a PDF that has only ASCII is read as text by some transfer agents, and
        // the file is a binary download.
        out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

        let mut offsets = vec![0usize; total + 1];
        for (index, body) in objects.iter().enumerate() {
            offsets[index + 1] = out.len();
            out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }

        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", total + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info {} 0 R >>\nstartxref\n{}\n%%EOF\n",
                total + 1,
                total,
                xref
            )
            .as_bytes(),
        );
        out
    }
}

/// Which edge of its column a cell is aligned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// The cell starts at the column's left edge.
    Left,
    /// The cell ends at the column's right edge.
    Right,
    /// The cell is centred on the column.
    Center,
}

/// A string in the PDF's own literal-string form, for the info dictionary.
fn pdf_literal(text: &str) -> String {
    let (encoded, _) = encode_text(text);
    String::from_utf8_lossy(&encoded).into_owned()
}

/// Break a paragraph into lines that each fit in `width`.
///
/// Words longer than the line are split at the character level: a SKU like
/// `OMN-2026-0042-ASSEMBLY-PREMIUM` is one token and a document that lets it run off the right
/// margin is a document with the amount column overwritten.
#[must_use]
pub fn wrap(text: &str, size: f32, width: f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if text_width(&candidate, size, false) <= width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if text_width(word, size, false) <= width {
            current = word.to_string();
            continue;
        }
        let mut chunk = String::new();
        for ch in word.chars() {
            let next = format!("{chunk}{ch}");
            if text_width(&next, size, false) > width && !chunk.is_empty() {
                lines.push(std::mem::take(&mut chunk));
            }
            chunk.push(ch);
        }
        current = chunk;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The right margin, for a caller that wants a right-aligned column flush with the page.
pub const RIGHT_EDGE: f32 = PAGE_WIDTH - MARGIN;
/// The left margin.
pub const LEFT_EDGE: f32 = MARGIN;
/// The usable width of the content area.
pub const WIDTH: f32 = CONTENT_WIDTH;
/// The top of the content area, where the first line's cursor starts.
pub const TOP: f32 = PAGE_HEIGHT - MARGIN;
/// The vertical distance between two lines of body text.
pub const LEADING: f32 = 12.0;
/// The largest width a paragraph is ever wrapped to, so text cannot run under the margin.
pub const PARAGRAPH_WIDTH: f32 = MAX_LINE;

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The index of the first occurrence of `needle`, over **bytes**.
    ///
    /// A byte search and not a string one because a PDF is not text: it opens with four bytes
    /// that are not valid UTF-8, so a lossy decode changes every offset after the header. The
    /// whole class of "my xref test passes but the file is wrong" bugs comes from reading a
    /// binary format as a string.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// The printable text of a PDF, for asserting on what a document says.
    ///
    /// Lossy on purpose and used only for **content** assertions, never for offsets: WinAnsi bytes
    /// that are not valid UTF-8 become U+FFFD here, so a name with a cedilla cannot be found by
    /// searching for the original string.
    fn printed(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn a_finished_document_is_a_pdf() {
        let mut doc = Document::new("Quote Q-2026-0001");
        doc.text(MARGIN, 16.0, true, "Quote Q-2026-0001");
        let bytes = doc.finish();
        assert!(
            bytes.starts_with(b"%PDF-1.4\n"),
            "the header is the first thing a reader looks for"
        );
        assert!(bytes.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn the_xref_offsets_point_at_their_objects() {
        // The failure this catches is silent and catastrophic: a file that opens in one viewer and
        // shows a blank page in another, or is refused outright.
        let mut doc = Document::new("Order SO-2026-0001");
        for index in 0..80 {
            doc.text(MARGIN, 9.0, false, &format!("line {index}"));
        }
        let bytes = doc.finish();
        // The **bytes**, never a decoded string: the file starts with a binary comment whose four
        // bytes are not valid UTF-8, and `from_utf8_lossy` replaces each invalid byte with
        // U+FFFD — turning four bytes into twelve. Every offset past the header is then wrong in
        // the decoded text, and the test would be measuring the decoder rather than the file.
        let xref_at = find(&bytes, b"startxref").expect("a trailer");
        let declared: usize = std::str::from_utf8(&bytes[xref_at + 9..])
            .expect("a trailer is ASCII")
            .trim()
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .expect("a numeric startxref");
        assert_eq!(
            declared,
            find(&bytes, b"xref\n0 ").expect("the table"),
            "startxref must name the table"
        );

        // Every entry must land on "N 0 obj".
        let table = std::str::from_utf8(&bytes[declared..]).expect("a table is ASCII");
        let mut checked = 0usize;
        // Counted up front: a walk that checks every entry and then reports "too few" tells you
        // the loop never matched, not that the table is short.
        // Table line 0 is the literal "xref", line 1 is "0 <count>", and line 2 is the free
        // entry. So after `skip(2)` the first line is the free one and object N sits at line
        // N + 2. Getting this off by one is why the previous version asked line 2 to land on
        // object 1 and was told, correctly, that it lands on the file header.
        for (index, line) in table.lines().enumerate().skip(2) {
            // An entry is ten hex digits, a space, five more, a space and its type — **19 bytes
            // after `lines()` strips the newline** (20 with it). The previous version required 20
            // and rejected every entry, which is why the walk counted none. Walking without any
            // shape check instead runs into the trailer, where "<< /Size 8" is ten bytes and does
            // not parse at all.
            if line.len() != 19 || !(line.ends_with(" n ") || line.ends_with(" f ")) {
                break;
            }
            let offset: usize = line[..10].parse().expect("ten hex digits");
            if line.ends_with(" f ") {
                assert_eq!(
                    offset, 0,
                    "the free entry is object 0 and is always at zero"
                );
                assert_eq!(index, 2, "it is the first line of the table body");
                continue;
            }
            let at = String::from_utf8_lossy(&bytes[offset..]);
            let expected = format!("{} 0 obj", index - 2);
            assert!(
                at.starts_with(&expected),
                "entry {index} claims offset {offset}, which holds {} rather than {expected}",
                at.chars().take(40).collect::<String>()
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "only {checked} entries were checked; the 20-byte shape did not match: {table}"
        );
    }

    #[test]
    fn every_stream_length_is_the_stream_it_describes() {
        // `/Length` is what a reader uses to find the end of a stream. Wrong by one and the next
        // object is read as content: the file opens and is nonsense.
        let mut doc = Document::new("t");
        // 200 rows at `LEADING` each is ~2,400pt over an 842pt page, so this really is
        // multi-page. The earlier fixture used 3pt of leading per row and produced 360pt — one
        // page — while its own comment claimed several, and the assertion below is what noticed.
        for index in 0..200 {
            doc.text(MARGIN, 9.0, false, &format!("row {index}"));
            doc.space(LEADING);
        }
        let bytes = doc.finish();
        let mut checked = 0;
        let mut cursor = 0usize;
        while let Some(at) = find(&bytes[cursor..], b"/Length ") {
            cursor += at + 8;
            let rest = &bytes[cursor..];
            let end = rest.iter().position(|b| *b == b' ').expect("a number");
            let declared: usize = std::str::from_utf8(&rest[..end])
                .expect("a length is ASCII")
                .parse()
                .expect("a numeric length");
            let stream_at = find(rest, b"stream\n").expect("the stream marker") + "stream\n".len();
            let body = printed(&rest[stream_at..stream_at + declared]);
            let body = body.as_str();
            assert!(
                body.ends_with("ET\n") || body.contains(" Tj ") || body.contains(" re f"),
                "a stream of {declared} bytes does not end where it should: {}",
                body.chars().take(60).collect::<String>()
            );
            cursor += stream_at + declared;
            checked += 1;
        }
        assert!(
            checked > 1,
            "a document with pages should have several streams"
        );
    }

    #[test]
    fn a_non_ascii_character_survives_into_the_stream_intact() {
        // The bug this pins, and it is the one that made every other assertion here useless: the
        // content stream was a `String`, so the WinAnsi bytes went through `from_utf8_lossy` and
        // every non-ASCII character came out as U+FFFD. A document printed "S?irket" for
        // "Şirket" — and every test that read only ASCII passed the whole time, because the
        // corruption happened on the way into the page, not on the way out of the encoder.
        //
        // Asserted on the **byte** and on the absence of U+FFFD's three-byte sequence, since
        // neither survives a lossy decode into something you could search for as a character.
        let mut doc = Document::new("Şirket");
        // The cedilla `Ş` is aliased to `S` (cp1252 has no glyph for it); the caron `Š` is in
        // cp1252 and must arrive as its own raw byte 0x8A. Both appear in this one string, so the
        // test covers the two rules and — more importantly — proves a raw non-UTF-8 byte survives
        // the trip into the page at all.
        doc.text(MARGIN, FONT_SIZE_BODY, false, "Ş Š Çö — 2026");
        let stream = &doc.pages[0].ops;
        assert!(
            stream
                .windows(5)
                .any(|w| w == [0x53, 0x20, 0x8A, 0x20, 0xC7]),
            "the caron and the cedilla C must both be their own cp1252 bytes"
        );
        assert!(
            !stream.windows(3).any(|w| w == [0xEF, 0xBF, 0xBD]),
            "a replacement character means a byte was decoded where it should have been copied: {:?}",
            String::from_utf8_lossy(stream)
        );
        assert!(
            stream.windows(3).any(|w| w == [0x20, 0x97, 0x20]),
            "the em dash is WinAnsi 0x97 between spaces"
        );
        // The title goes through the same writer, and a document whose window bar reads a row of
        // question marks is the most visible possible failure of this bug.
        let title = find(&doc.finish(), b"/Title (Sirket)").expect("the aliased title");
        assert!(title > 0);
    }

    #[test]
    fn text_scales_with_its_size() {
        assert!(text_width("Total", 9.0, false) > text_width("Total", 4.5, false));
        assert!(text_width("", 9.0, false) == 0.0);
        // Bold is wider than regular for the same string at the same size.
        assert!(text_width("Total", 9.0, true) > text_width("Total", 9.0, false));
    }

    #[test]
    fn a_right_aligned_column_lines_up_by_its_last_digit() {
        // The property the money column depends on: two amounts of different lengths end at the
        // same x, so their decimal points sit in the same column.
        let mut doc = Document::new("t");
        doc.text_right(RIGHT_EDGE, FONT_SIZE_BODY, false, "9.90");
        doc.text_right(RIGHT_EDGE, FONT_SIZE_BODY, false, "1,234.50");
        let ops = doc.pages[0].ops_text();
        let xs: Vec<f32> = ops
            .lines()
            .filter_map(|line| {
                let at = line.find(" 1 0 0 1 ")? + 9;
                let value = &line[at..];
                let end = value.find(' ')?;
                value[..end].parse().ok()
            })
            .collect();
        assert_eq!(xs.len(), 2);
        let short_right = xs[0] + text_width("9.90", FONT_SIZE_BODY, false);
        let long_right = xs[1] + text_width("1,234.50", FONT_SIZE_BODY, false);
        assert!(
            (short_right - long_right).abs() < 0.02,
            "right edges are {short_right} and {long_right}"
        );
    }

    #[test]
    fn the_three_reserved_characters_are_escaped() {
        let (bytes, encoding) = encode_text("Acme (Holdings) \\ Ltd.");
        assert_eq!(encoding.replaced, 0);
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text, r"Acme \(Holdings\) \\ Ltd.");
    }

    #[test]
    fn a_character_the_font_lacks_is_counted_rather_than_dropped() {
        // A base-14 font has no glyph for CJK, and a document that prints nothing for it is a
        // document with a hole in the recipient's name. The count is what makes that visible.
        let (bytes, encoding) = encode_text(" Şirket ğ Ming 的 Ltd");
        // Three, not two: `Ş` is a cedilla and cp1252 has no glyph for it either, so it is
        // aliased and counted alongside `ğ` and the CJK name. The earlier expectation of two was
        // written while the table wrongly believed `Ş` was in cp1252.
        assert_eq!(
            encoding.replaced, 3,
            "Ş and ğ are aliased, and the CJK name has no near-miss at all"
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("Ming"),
            "printable text survives"
        );
    }

    #[test]
    fn a_turkish_letter_the_font_lacks_prints_its_nearest_readable_letter() {
        // The two kinds of loss are both counted, but they are drawn differently on purpose:
        // a `?` is an obvious hole, whereas `g` for `ğ` reads as a plausible name. Counting it
        // is what lets the route warn the sender that the document is degraded.
        let (bytes, encoding) = encode_text("İğde ırmak");
        assert_eq!(encoding.replaced, 3, "İ, ğ and ı are all outside cp1252");
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "Igde irmak",
            "the alias is readable rather than a row of question marks"
        );
    }

    #[test]
    fn a_letter_cp1252_carries_is_left_alone() {
        // The carons `Š` `š` and the accents `Ç` `ö` `ü` `Ü` ARE in cp1252, so counting them as
        // lost would be a false warning on almost every document a Turkish organization produces.
        //
        // Asserted on the **bytes**, not on a decoded string: a lossy decode turns each
        // non-UTF-8 byte into U+FFFD, so `printed.contains('Ç')` is false for a document that
        // printed Ç perfectly well. The decoder is the lossy part here, not the writer.
        //
        // Every value is from `bytes([cp]).decode("cp1252")`, checked one at a time — an earlier
        // version of this test asserted bytes written from memory and was wrong about two of them.
        let (bytes, encoding) = encode_text("Širket Çözüm ülem");
        assert_eq!(encoding.replaced, 0, "none of these is outside WinAnsi");
        // Byte for byte, from the writer itself: `Š` is 0x8A, `Ç` 0xC7, `ö` 0xF6, `ü` 0xFC.
        // The previous version of this literal ended in 0xDC (Ü) — a letter the string does not
        // contain — which is what a hand-written byte table gets you.
        assert_eq!(
            bytes, b"\x8airket \xc7\xf6z\xfcm \xfclem",
            "each character is its own cp1252 byte, none substituted"
        );
    }

    #[test]
    fn the_s_cedilla_and_the_s_caron_are_different_characters() {
        // cp1252 0x8A/0x9A are `Š`/`ş` (caron). `Ş`/`ş` (cedilla) are not in cp1252 at all and
        // take the alias path. Written as a test because the two pairs are visually near-identical
        // and no compiler or type checker distinguishes them — a table written from memory gets
        // this wrong and every ASCII-only test still passes.
        // The carons are U+0160/U+0161 and ARE in cp1252; the cedillas are U+015E/U+015F and are
        // NOT. Both pairs are named by code point here, because the two letters are nearly
        // identical on screen and this test's first version used the cedilla in place of the
        // caron in two of its four assertions — which is exactly the mistake the test exists to
        // catch, made by the test.
        assert_eq!(winansi('\u{0160}'), Some(0x8A), "caron capital: in cp1252");
        assert_eq!(winansi('\u{0161}'), Some(0x9A), "caron small: in cp1252");
        assert_eq!(winansi('\u{015E}'), None, "cedilla capital: not in cp1252");
        assert_eq!(winansi('\u{015F}'), None, "cedilla small: not in cp1252");
        // …and the two cedillas are therefore the ones the alias table is for.
        assert_eq!(turkish_alias('\u{015E}'), Some(b'S'));
        assert_eq!(turkish_alias('\u{015F}'), Some(b's'));
    }

    #[test]
    fn the_s_cedilla_and_the_s_caron_do_not_share_a_slot() {
        // The four assertions in the test above are the whole point; this one states it once more
        // as a *pair*, because a reader who changes one character and not the other will not see
        // the test above fail — they will see two unrelated assertions and assume both are fine.
        // `Š`/`ş` (caron) occupy 0x8A/0x9A and are drawn exactly; `Ş`/`ş` (cedilla) are not in
        // cp1252 at all and go through the alias table. Verified, not remembered.
        assert_eq!(winansi('\u{0160}'), Some(0x8A));
        assert_eq!(winansi('\u{0161}'), Some(0x9A));
        assert_eq!(winansi('\u{015E}'), None);
        assert_eq!(winansi('\u{015F}'), None);
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken_rather_than_allowed_off_the_page() {
        let long = "OMN-2026-0042-ASSEMBLY-PREMIUM-VARIANT";
        let lines = wrap(long, FONT_SIZE_BODY, 120.0);
        assert!(
            lines.len() > 1,
            "one token wider than the line has to break"
        );
        for line in &lines {
            assert!(
                text_width(line, FONT_SIZE_BODY, false) <= 120.0,
                "{line:?} is still too wide"
            );
        }
    }

    #[test]
    fn wrapping_keeps_every_word() {
        let text = "Quotation for the annual maintenance of the office espresso machine, \
                    including two service visits and all parts.";
        let lines = wrap(text, FONT_SIZE_BODY, 220.0);
        let joined = lines.join(" ");
        for word in text.split_whitespace() {
            assert!(joined.contains(word), "{word:?} went missing");
        }
    }

    #[test]
    fn a_long_document_breaks_onto_several_pages() {
        let mut doc = Document::new("t");
        for index in 0..300 {
            doc.text(MARGIN, FONT_SIZE_BODY, false, &format!("line {index}"));
            doc.space(LEADING);
        }
        let bytes = doc.finish();
        let text = String::from_utf8_lossy(&bytes);
        let count = text
            .find("/Count ")
            .and_then(|at| text[at + 7..].split(' ').next()?.parse::<usize>().ok())
            .expect("a page count");
        assert!(
            count > 1,
            "300 lines cannot fit on one page, so {count} is suspicious"
        );
        // Every page in the tree must have a page object.
        assert_eq!(text.matches("/Type /Page ").count(), count);
    }

    #[test]
    fn a_row_writes_every_cell_it_is_given() {
        let columns = [
            (MARGIN, MARGIN + 200.0, Align::Left),
            (MARGIN + 210.0, MARGIN + 260.0, Align::Right),
        ];
        let mut doc = Document::new("t");
        doc.row(&columns, &["Widget", "19.90"], false);
        let ops = &doc.pages[0].ops_text();
        assert!(ops.contains("(Widget) Tj"));
        assert!(ops.contains("(19.90) Tj"));
    }
}
