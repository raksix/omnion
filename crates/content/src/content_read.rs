//! The read model behind the headless content API (REQ-019, slice 2).
//!
//! `api_tokens.rs` decides *who* may read. This module decides *what* they read, and the three
//! decisions in it are the ones a naive implementation gets wrong:
//!
//! 1. **A cursor is a keyset, not an offset.** `?cursor=` carries `(updated_at, id)` so a page
//!    deleted between two calls does not shift every later row forward and make a caller
//!    re-read (or skip) content it has never seen. An `OFFSET` cursor is simpler to write and
//!    silently wrong for the exact use this API exists for — a frontend rebuilding its cache.
//!
//! 2. **Sorting is newest-first and the keyset must match the sort.** A cursor that is opaque
//!    here but a raw `OFFSET` there would work until the first deletion, so the cursor is signed
//!    and self-describing: [`encode_cursor`] / [`decode_cursor`] round-trip, and a value that was
//!    not produced by [`encode_cursor`] is refused instead of being read as garbage.
//!
//! 3. **Field selection is a projection, not a filter.** `?fields=slug,title` must still carry
//!    `id`, `slug`, `etag` and `updated_at` — the keys a caller needs to cache and revalidate
//!    with — because a projection that can drop `slug` produces a response that cannot be used to
//!    request the next page. [`Identity keys`] names that set once, and [`project_page`] and
//!    [`project_media`] both take it from there.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::error::ContentError;

/// Result of validating a read query: the filters are known-good and the SQL can be built.
pub type Result<T> = std::result::Result<T, ContentError>;

// ---------------------------------------------------------------------------------------------
// Limits and vocabulary
// ---------------------------------------------------------------------------------------------

/// Most rows one call may return.
///
/// A hard ceiling rather than a default: a content API that will "return the rest" is a content
/// API that gets asked to return the rest by a script, and the answer is a timeout at the far
/// end and an out-of-memory process in the middle.
pub const MAX_LIMIT: i64 = 100;

/// Limit used when the caller names none.
pub const DEFAULT_LIMIT: i64 = 20;

/// Longest accepted `fields` value, as a guard against a pathological list.
pub const MAX_FIELDS: usize = 32;

/// The sort orders the surface offers.
///
/// Only `updated_at` and `created_at` descending are accepted. Ascending would need its own
/// keyset direction in the cursor, and a reverse-ordered cursor is exactly the sort of thing that
/// works until the first page and then quietly duplicates rows.
pub const SORTS: [&str; 3] = ["updated_at", "created_at", "title"];

/// Which column a `sort` value addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Newest change first.
    UpdatedAt,
    /// Newest creation first.
    CreatedAt,
    /// Alphabetical by title.
    Title,
}

impl SortKey {
    /// Parse a caller's `sort` value.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "updated_at" => Ok(Self::UpdatedAt),
            "created_at" => Ok(Self::CreatedAt),
            "title" => Ok(Self::Title),
            other => Err(ContentError::InvalidQuery(format!(
                "sort must be one of {} (got \"{other}\")",
                SORTS.join(", ")
            ))),
        }
    }

    /// The SQL column, which is also the cursor's second component.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::CreatedAt => "created_at",
            Self::Title => "title",
        }
    }
}

/// The identity keys every item carries whatever `fields` asks for.
///
/// Refusing to drop these is what makes a projected response still usable: `slug` addresses the
/// next call, `etag` revalidates it, and `id` is the only stable handle a caller can have when a
/// slug is renamed. [`SELECTABLE_FIELDS`] is the rest — the projection whitelist.
pub const IDENTITY_FIELDS: [&str; 5] = ["id", "slug", "type", "locale", "updated_at"];

// ---------------------------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------------------------

/// One page position: the sort value of the last row, plus its id as the tiebreak.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    /// The value of the sort column on the last row of the previous page.
    pub value: String,
    /// The id of that row, which disambiguates equal sort values.
    pub id: Uuid,
}

impl Cursor {
    /// Build a cursor from a row's sort value and id.
    pub fn new(value: impl Into<String>, id: Uuid) -> Self {
        Self {
            value: value.into(),
            id,
        }
    }
}

/// Domain separation for the cursor digest, so a value cannot be confused with any other
/// hash this platform produces.
const CURSOR_DOMAIN: &[u8] = b"omnion/content-cursor/v1";

/// Encode a cursor into the opaque string a caller passes back as `?cursor=`.
///
/// The payload is `value|id` and the digest covers both plus a fixed domain, so a cursor is
/// checked for authenticity without a server-side session. Deliberately *not* encrypted: the
/// values inside are the caller's own sort key and id, and a caller may want to see what they
/// are holding in a bug report. What the digest buys is the refusal to read a hand-edited value
/// as a query, not secrecy.
#[must_use]
pub fn encode_cursor(cursor: &Cursor) -> String {
    let payload = format!("{}|{}", cursor.value, cursor.id);
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(b"\0");
    hasher.update(payload.as_bytes());
    let digest = hasher.finalize();
    let mac = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{payload}|{mac}")
}

/// Decode a cursor, or refuse it.
///
/// A refused cursor is a `400 invalid_parameter` naming `cursor` — never a `500`, and never a
/// silent first page: a caller whose cursor expired or was truncated must be told, because the
/// alternative (answering page one) makes a paginating client believe it has seen everything.
pub fn decode_cursor(raw: &str) -> Result<Cursor> {
    let bad = || ContentError::InvalidQuery("this cursor is not valid".to_string());
    let (payload, mac) = raw.rsplit_once('|').ok_or_else(bad)?;
    let (value, id) = payload.rsplit_once('|').ok_or_else(bad)?;
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(b"\0");
    hasher.update(payload.as_bytes());
    let digest = hasher.finalize();
    let expected: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if expected != mac {
        return Err(bad());
    }
    let id = Uuid::parse_str(id).map_err(|_| bad())?;
    Ok(Cursor::new(value, id))
}

// ---------------------------------------------------------------------------------------------
// Field selection
// ---------------------------------------------------------------------------------------------

/// The projection whitelist for pages: identity keys plus everything a page can offer.
pub const SELECTABLE_PAGE_FIELDS: [&str; 9] = [
    "id",
    "slug",
    "type",
    "locale",
    "updated_at",
    "title",
    "body",
    "summary",
    "revision",
];

/// The projection whitelist for media.
pub const SELECTABLE_MEDIA_FIELDS: [&str; 10] = [
    "id",
    "slug",
    "type",
    "locale",
    "updated_at",
    "filename",
    "mime",
    "size",
    "alt_text",
    "description",
];

/// A validated `fields` selection.
///
/// Built through [`Fields::parse`] so the identity keys are unioned in exactly once, and the
/// unknown-field error names the offender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields {
    selected: Vec<String>,
}

impl Fields {
    /// Parse a caller's `?fields=` value against a whitelist.
    ///
    /// `None` or empty selects everything the endpoint can offer, which is why this returns the
    /// whitelist rather than an empty list: the caller's "no filter" and "everything" are the
    /// same request, and representing them differently would need a branch at every render.
    pub fn parse(raw: Option<&str>, whitelist: &[&str]) -> Result<Self> {
        let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self {
                selected: whitelist.iter().map(|field| (*field).to_string()).collect(),
            });
        };
        let mut seen: Vec<String> = IDENTITY_FIELDS
            .iter()
            .map(|field| (*field).to_string())
            .collect();
        let mut unknown: Vec<&str> = Vec::new();
        for field in raw.split(',') {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            if !whitelist.contains(&field) {
                unknown.push(field);
                continue;
            }
            let owned = field.to_string();
            if !seen.contains(&owned) {
                seen.push(owned);
            }
        }
        if !unknown.is_empty() {
            // One message listing every bad field: a caller who typos a list of six should fix
            // all six, not discover them one request at a time.
            return Err(ContentError::InvalidQuery(format!(
                "unknown field(s): {}; available: {}",
                unknown.join(", "),
                whitelist.join(", ")
            )));
        }
        Ok(Self { selected: seen })
    }

    /// Whether a key survives the projection.
    #[must_use]
    pub fn wants(&self, key: &str) -> bool {
        self.selected.iter().any(|field| field == key)
    }

    /// Keep only the selected keys of a value map.
    ///
    /// Applied last, so a caller cannot smuggle a key past the whitelist by nesting it inside a
    /// value that was already selected — the map it walks is the item's own flat fields.
    pub fn project(&self, mut value: Map<String, Value>) -> Map<String, Value> {
        value.retain(|key, _| self.wants(key));
        value
    }
}

// ---------------------------------------------------------------------------------------------
// ETags
// ---------------------------------------------------------------------------------------------

/// A weak ETag for a content item, derived from what a reader can observe.
///
/// `updated_at` plus the id, not the body: hashing the body would make every response a full
/// read of the text, and the timestamp is already what the cache would have compared. The
/// timestamp is normalised to milliseconds because PostgreSQL's microsecond precision would
/// otherwise produce a different ETag for the same row read twice.
///
/// The id goes in **whole**. A 48-bit prefix is plenty for a random v4 in production and looks
/// tidier in a header, but it is not a property the function can claim: any id scheme that shares
/// a prefix across rows (a counter, a per-tenant sequence, a test's `from_u128`) collapses to one
/// ETag, and two different items would then revalidate each other's cache entry. A unit test that
/// builds ids as `from_u128(n)` catches exactly that, which is why it is here.
#[must_use]
pub fn etag_for(updated_at: OffsetDateTime, id: Uuid) -> String {
    let millis = updated_at.unix_timestamp_nanos() / 1_000_000;
    format!("W/\"{millis:x}-{}\"", id.simple())
}

/// The ETag of a whole list response, over the items it contains.
///
/// A list needs its own ETag: the headers of a single-item response say nothing about whether
/// the *set* changed, and a caller rebuilding a cache asks the set question, not the item one.
#[must_use]
pub fn list_etag(items: &[(String, Uuid)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(b"list\0");
    for (updated_at, id) in items {
        hasher.update(updated_at.as_bytes());
        hasher.update(b"\0");
        hasher.update(id.to_string().as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let short = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("W/\"{short}\"")
}

/// Parse an `updated_since` parameter into an instant.
pub fn parse_updated_since(raw: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| {
        ContentError::InvalidQuery(format!(
            "updated_since must be an RFC 3339 timestamp (got \"{raw}\")"
        ))
    })
}

/// Validate a language tag the same way the translation store does, so a locale that the panel
/// would accept is not refused by the API and vice versa.
pub fn parse_locale(raw: &str) -> Result<String> {
    crate::validation::validate_language(raw)
}

// ---------------------------------------------------------------------------------------------
// Read shapes
// ---------------------------------------------------------------------------------------------

/// A published page as the content surface serves it.
///
/// `revision` is the published revision only, and the type carries no status: a caller must not
/// be able to learn that a draft exists. The store that builds this reads only published rows,
/// so a status field here would be a field that is always `published` and always lies by
/// omission about everything else.
#[derive(Debug, Clone)]
pub struct ReadPage {
    /// Row identity.
    pub id: Uuid,
    /// Address inside the site.
    pub slug: String,
    /// Content type (`page`, or `post` for the blog alias).
    pub page_type: String,
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Last change, RFC 3339.
    pub updated_at: OffsetDateTime,
    /// Creation time, RFC 3339.
    pub created_at: OffsetDateTime,
    /// Published revision id, when the page has one.
    pub published_revision_id: Option<Uuid>,
    /// Title of the published revision.
    pub title: String,
    /// Body of the published revision.
    pub body: String,
    /// Summary, when the author wrote one.
    pub summary: Option<String>,
    /// Revision number visitors see.
    pub revision: i32,
}

impl ReadPage {
    /// Render the item, honouring a field selection and the requested locale.
    ///
    /// `translations` are the overlay rows for this page keyed by `(language, field)`; a locale
    /// that has no overlay leaves the base value in place rather than emptying the field, because
    /// an API that answers `null` for an untranslated title is indistinguishable from one that
    /// has no title.
    pub fn to_value(
        &self,
        fields: &Fields,
        locale: Option<&str>,
        translations: &[(String, String, String)],
    ) -> Value {
        let overlay = |name: &str| -> Option<String> {
            let language = locale?;
            let exact = translations
                .iter()
                .find(|(lang, field, _)| lang == language && field == name)
                .map(|(_, _, value)| value.clone());
            match exact {
                Some(value) => Some(value),
                // Fall back to the language part of `tr-TR` so a site that stores `tr` still
                // serves Turkish to a caller asking for `tr-TR`.
                None => language
                    .split_once('-')
                    .map(|(base, _)| base)
                    .and_then(|base| {
                        translations
                            .iter()
                            .find(|(lang, field, _)| lang == base && field == name)
                            .map(|(_, _, value)| value.clone())
                    }),
            }
        };
        let title = overlay("title").unwrap_or_else(|| self.title.clone());
        let summary = overlay("summary").or_else(|| self.summary.clone());
        let body = overlay("body").unwrap_or_else(|| self.body.clone());

        let mut value = Map::new();
        value.insert("id".into(), Value::String(self.id.to_string()));
        value.insert("slug".into(), Value::String(self.slug.clone()));
        value.insert("type".into(), Value::String(self.page_type.clone()));
        value.insert(
            "locale".into(),
            Value::String(locale.map_or_else(|| "en".to_string(), str::to_string)),
        );
        value.insert(
            "updated_at".into(),
            Value::String(self.updated_at.to_string()),
        );
        value.insert(
            "etag".into(),
            Value::String(etag_for(self.updated_at, self.id)),
        );
        if fields.wants("title") {
            value.insert("title".into(), Value::String(title));
        }
        if fields.wants("body") {
            value.insert("body".into(), Value::String(body));
        }
        if fields.wants("summary") {
            value.insert("summary".into(), summary.map_or(Value::Null, Value::String));
        }
        if fields.wants("revision") {
            value.insert("revision".into(), Value::from(self.revision));
        }
        Value::Object(value)
    }
}

/// A media item as the content surface serves it: metadata, never bytes.
///
/// The signed URL is added by the API layer, which owns the installation's storage policy; this
/// shape carries the storage key so that layer can sign it without a second query.
#[derive(Debug, Clone)]
pub struct ReadMedia {
    /// Row identity.
    pub id: Uuid,
    /// Address inside the site — the filename, since media has no slug of its own.
    pub slug: String,
    /// Site the item belongs to.
    pub site_id: Uuid,
    /// Last change, RFC 3339.
    pub updated_at: OffsetDateTime,
    /// Original file name.
    pub filename: String,
    /// MIME type.
    pub mime: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Pixel width, when the platform knows it.
    pub width: Option<i32>,
    /// Pixel height, when the platform knows it.
    pub height: Option<i32>,
    /// Object key the API signs.
    pub storage_key: String,
    /// Alt text the uploader or an editor wrote — the field a frontend renders as `alt`.
    pub alt_text: String,
    /// Longer description, when there is one.
    pub description: String,
    /// Trash marker; a trashed item is never served.
    pub deleted_at: Option<OffsetDateTime>,
}

impl ReadMedia {
    /// Render the item, honouring a field selection.
    #[must_use]
    pub fn to_value(&self, fields: &Fields) -> Value {
        let mut value = Map::new();
        value.insert("id".into(), Value::String(self.id.to_string()));
        value.insert("slug".into(), Value::String(self.slug.clone()));
        value.insert("type".into(), Value::String("media".into()));
        value.insert("locale".into(), Value::String("en".into()));
        value.insert(
            "updated_at".into(),
            Value::String(self.updated_at.to_string()),
        );
        value.insert(
            "etag".into(),
            Value::String(etag_for(self.updated_at, self.id)),
        );
        if fields.wants("filename") {
            value.insert("filename".into(), Value::String(self.filename.clone()));
        }
        if fields.wants("mime") {
            value.insert("mime".into(), Value::String(self.mime.clone()));
        }
        if fields.wants("alt_text") {
            value.insert("alt_text".into(), Value::String(self.alt_text.clone()));
        }
        if fields.wants("description") {
            value.insert(
                "description".into(),
                Value::String(self.description.clone()),
            );
        }
        if fields.wants("size") {
            value.insert("size".into(), Value::from(self.size_bytes));
            if let Some(width) = self.width {
                value.insert("width".into(), Value::from(width));
            }
            if let Some(height) = self.height {
                value.insert("height".into(), Value::from(height));
            }
        }
        Value::Object(value)
    }
}

/// One page of results plus the cursor that continues it.
#[derive(Debug, Clone)]
pub struct Page<T> {
    /// The rows themselves.
    pub items: Vec<T>,
    /// Cursor for the next page, or `None` when this was the last one.
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// Build a page, deriving `next_cursor` from whether more rows may exist.
    ///
    /// `fetched == limit` is the honest test. `has_more` computed by a second count query is
    /// exact and costs a scan; a short page is the same answer for free, and a full page that
    /// happens to be the last one costs the caller one empty round trip instead of a scan on
    /// every request.
    #[must_use]
    pub fn new(items: Vec<T>, fetched: i64, limit: i64, cursor_of: impl Fn(&T) -> Cursor) -> Self {
        let next_cursor = if fetched == limit && !items.is_empty() {
            items.last().map(|item| encode_cursor(&cursor_of(item)))
        } else {
            None
        };
        Self { items, next_cursor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cursor() -> Cursor {
        Cursor::new("2026-09-30 12:00:00.000000+00", Uuid::nil())
    }

    #[test]
    fn a_cursor_round_trips() {
        let original = cursor();
        let encoded = encode_cursor(&original);
        assert_eq!(decode_cursor(&encoded).expect("round trip"), original);
    }

    #[test]
    fn a_hand_edited_cursor_is_refused_rather_than_read() {
        let encoded = encode_cursor(&cursor());
        // Change the id, keep the digest: the value would otherwise be a valid query the caller
        // never asked for.
        let tampered = encoded.replace(&Uuid::nil().to_string(), &Uuid::from_u128(7).to_string());
        assert!(
            decode_cursor(&tampered).is_err(),
            "a tampered cursor must not decode"
        );
        // And a value that was never a cursor at all.
        assert!(decode_cursor("7").is_err());
        assert!(decode_cursor("").is_err());
    }

    #[test]
    fn a_cursor_containing_a_pipe_in_its_value_still_round_trips() {
        // The payload splits on the LAST `|`, so a sort value containing a pipe cannot be
        // confused with the id boundary.
        let original = Cursor::new("a|b|c", Uuid::nil());
        assert_eq!(
            decode_cursor(&encode_cursor(&original)).expect("pipe"),
            original
        );
    }

    #[test]
    fn a_projection_keeps_the_identity_keys() {
        let fields = Fields::parse(Some("title"), &SELECTABLE_PAGE_FIELDS).expect("valid");
        assert!(fields.wants("title"));
        assert!(!fields.wants("body"));
        // Even though `body` was not asked for, the keys a caller needs to keep paging are
        // always there.
        for identity in IDENTITY_FIELDS {
            assert!(
                fields.wants(identity),
                "{identity} must survive a projection"
            );
        }
    }

    #[test]
    fn no_fields_means_everything_the_endpoint_offers() {
        let all = Fields::parse(None, &SELECTABLE_PAGE_FIELDS).expect("default");
        for field in SELECTABLE_PAGE_FIELDS {
            assert!(all.wants(field));
        }
    }

    #[test]
    fn an_unknown_field_is_refused_by_name() {
        let error = Fields::parse(Some("title,nope,alsonope"), &SELECTABLE_PAGE_FIELDS)
            .expect_err("unknown field must be refused");
        let message = error.to_string();
        assert!(message.contains("nope"), "the message names it: {message}");
        assert!(
            message.contains("alsonope"),
            "and every other one: {message}"
        );
    }

    #[test]
    fn sort_accepts_the_three_offered_orders_and_nothing_else() {
        assert_eq!(
            SortKey::parse("updated_at").expect("ok"),
            SortKey::UpdatedAt
        );
        assert_eq!(
            SortKey::parse("created_at").expect("ok"),
            SortKey::CreatedAt
        );
        assert_eq!(SortKey::parse("title").expect("ok"), SortKey::Title);
        assert!(SortKey::parse("slug").is_err(), "slug is not offered");
        assert!(SortKey::parse("updated_at desc").is_err());
    }

    #[test]
    fn an_etag_is_stable_across_reads_of_the_same_row() {
        let at = OffsetDateTime::parse("2026-09-30T12:00:00.123456Z", &Rfc3339).expect("time");
        let first = etag_for(at, Uuid::from_u128(1));
        let second = etag_for(at, Uuid::from_u128(1));
        assert_eq!(first, second);
        assert_ne!(first, etag_for(at, Uuid::from_u128(2)));
    }

    #[test]
    fn a_list_etag_changes_when_an_item_does() {
        let a = ("2026-09-30T12:00:00Z".to_string(), Uuid::from_u128(1));
        let b = ("2026-09-30T13:00:00Z".to_string(), Uuid::from_u128(2));
        let before = list_etag(&[a.clone(), b.clone()]);
        let after = list_etag(&[a.clone(), ("2026-09-30T14:00:00Z".to_string(), b.1)]);
        assert_ne!(before, after, "a changed row changes the list etag");
        assert_eq!(before, list_etag(&[a, b]), "the same set is the same etag");
    }

    #[test]
    fn updated_since_wants_rfc_3339_and_says_so_when_it_does_not() {
        assert!(parse_updated_since("2026-09-30T12:00:00Z").is_ok());
        let error = parse_updated_since("yesterday").expect_err("not a timestamp");
        assert!(error.to_string().contains("RFC 3339"), "{error}");
    }

    #[test]
    fn a_full_page_continues_and_a_short_one_does_not() {
        let rows: Vec<Cursor> = (0..2)
            .map(|n| Cursor::new(n.to_string(), Uuid::from_u128(n)))
            .collect();
        let full = Page::new(rows.clone(), 2, 2, |item| item.clone());
        assert!(full.next_cursor.is_some(), "a full page may have more");

        let short = Page::new(rows.clone(), 2, 5, |item| item.clone());
        assert!(short.next_cursor.is_none(), "a short page is the last page");

        let empty = Page::new(Vec::new(), 0, 5, |item: &Cursor| item.clone());
        assert!(empty.next_cursor.is_none(), "an empty page has no cursor");
    }

    #[test]
    fn the_next_cursor_continues_from_the_last_row() {
        let rows: Vec<Cursor> = (0..3)
            .map(|n| Cursor::new(n.to_string(), Uuid::from_u128(n)))
            .collect();
        let page = Page::new(rows.clone(), 3, 3, |item| item.clone());
        let decoded = decode_cursor(&page.next_cursor.expect("cursor")).expect("decodes");
        assert_eq!(decoded, *rows.last().expect("a row"));
    }

    #[test]
    fn a_page_renders_its_identity_keys_whatever_was_asked_for() {
        let page = ReadPage {
            id: Uuid::from_u128(9),
            slug: "about".into(),
            page_type: "page".into(),
            site_id: Uuid::from_u128(1),
            updated_at: OffsetDateTime::parse("2026-09-30T12:00:00Z", &Rfc3339).expect("time"),
            created_at: OffsetDateTime::parse("2026-09-01T12:00:00Z", &Rfc3339).expect("time"),
            published_revision_id: Some(Uuid::from_u128(10)),
            title: "About".into(),
            body: "Long body".into(),
            summary: None,
            revision: 3,
        };
        let fields = Fields::parse(Some("title"), &SELECTABLE_PAGE_FIELDS).expect("valid");
        let value = page.to_value(&fields, None, &[]);
        let object = value.as_object().expect("an object");
        for key in [
            "id",
            "slug",
            "type",
            "locale",
            "updated_at",
            "etag",
            "title",
        ] {
            assert!(object.contains_key(key), "{key} is always present");
        }
        assert!(!object.contains_key("body"), "body was not selected");
    }

    #[test]
    fn a_locale_overlay_replaces_the_base_value() {
        let page = ReadPage {
            id: Uuid::from_u128(9),
            slug: "about".into(),
            page_type: "page".into(),
            site_id: Uuid::from_u128(1),
            updated_at: OffsetDateTime::parse("2026-09-30T12:00:00Z", &Rfc3339).expect("time"),
            created_at: OffsetDateTime::parse("2026-09-01T12:00:00Z", &Rfc3339).expect("time"),
            published_revision_id: None,
            title: "About".into(),
            body: "Body".into(),
            summary: None,
            revision: 1,
        };
        let overlay = vec![(
            "tr".to_string(),
            "title".to_string(),
            "Hakkında".to_string(),
        )];
        let value = page.to_value(
            &Fields::parse(None, &SELECTABLE_PAGE_FIELDS).expect("all"),
            Some("tr"),
            &overlay,
        );
        assert_eq!(value["title"], json!("Hakkında"));
        // And a regional tag falls back to the base language a site stored.
        let regional = page.to_value(
            &Fields::parse(None, &SELECTABLE_PAGE_FIELDS).expect("all"),
            Some("tr-TR"),
            &overlay,
        );
        assert_eq!(regional["title"], json!("Hakkında"));
    }

    #[test]
    fn a_locale_with_no_overlay_keeps_the_base_value() {
        let page = ReadPage {
            id: Uuid::from_u128(9),
            slug: "about".into(),
            page_type: "page".into(),
            site_id: Uuid::from_u128(1),
            updated_at: OffsetDateTime::parse("2026-09-30T12:00:00Z", &Rfc3339).expect("time"),
            created_at: OffsetDateTime::parse("2026-09-01T12:00:00Z", &Rfc3339).expect("time"),
            published_revision_id: None,
            title: "About".into(),
            body: "Body".into(),
            summary: None,
            revision: 1,
        };
        let value = page.to_value(
            &Fields::parse(None, &SELECTABLE_PAGE_FIELDS).expect("all"),
            Some("de"),
            &[],
        );
        assert_eq!(
            value["title"],
            json!("About"),
            "an untranslated field is not nulled"
        );
        assert_eq!(
            value["locale"],
            json!("de"),
            "but the response says what it answered in"
        );
    }
}
