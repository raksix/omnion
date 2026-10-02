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
/// Only descending orders are accepted. Ascending would need its own keyset direction in the
/// cursor, and a reverse-ordered cursor is exactly the sort of thing that works until the first
/// page and then quietly duplicates rows.
pub const SORTS: [&str; 3] = ["updated_at", "created_at", "title"];

/// Which relation a sort expression is being written for.
///
/// A sort names a *column*, and a column does not exist everywhere: a page's title lives on
/// `page_revisions`, not on `pages`, and a file has no title at all. The old code answered with a
/// bare column name and the caller prefixed it with `p.`, which meant `p.title` — a name that has
/// never existed on that table, so `?sort=title` was a `500` on every call and a `sort=title`
/// cursor on media was a `500` on the second page. Naming the relation makes the difference
/// explicit at the one place the vocabulary is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortSource {
    /// `pages` joined to its published `page_revisions`.
    Pages,
    /// `media`, which has no title and whose `updated_at` is nullable.
    Media,
}

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
    /// Parse a caller's `sort` value, without knowing which endpoint will run the query.
    ///
    /// Kept for callers that only need the vocabulary; an endpoint must use [`Self::parse_for`]
    /// so a sort the relation does not have is refused by name instead of reaching SQL.
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

    /// Parse a caller's `sort` for one specific relation.
    ///
    /// The refusal is a `400` naming the parameter and listing what *this* endpoint sorts by,
    /// because "sort=title" against a media list is a question with no answer, and answering it
    /// with a `500` from a missing column is how a documented parameter stays broken for a year.
    pub fn parse_for(raw: &str, source: SortSource) -> Result<Self> {
        let sort = Self::parse(raw)?;
        sort.expression(source).ok_or_else(|| {
            ContentError::InvalidQuery(format!(
                "sort must be one of {} for this endpoint (got \"{raw}\")",
                SORTS
                    .iter()
                    .copied()
                    .filter(
                        |name| Self::parse(name).is_ok_and(|key| key.expression(source).is_some())
                    )
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(sort)
    }

    /// The SQL expression this sort keys on, qualified for `source`.
    ///
    /// **`None` means this relation cannot be sorted that way at all**, and that is why it is an
    /// `Option` rather than a `&'static str` with a made-up name: a function that must invent a
    /// column to return a `&str` will eventually have that invented column used.
    ///
    /// The expression is the *cursor's* value source too, so a sort and its keyset predicate can
    /// never name different columns.
    #[must_use]
    pub const fn expression(self, source: SortSource) -> Option<&'static str> {
        match (self, source) {
            (Self::UpdatedAt, SortSource::Pages) => Some("p.updated_at"),
            (Self::CreatedAt, SortSource::Pages) => Some("p.created_at"),
            // The title is the revision's, which is why this is qualified and aliased.
            (Self::Title, SortSource::Pages) => Some("r.title"),
            // Media predates its own `updated_at`, so a NULL falls back to `created_at`. The
            // coalesce is used in the ORDER BY, the keyset predicate and the cursor together —
            // a cursor holding a `created_at` value compared against a coalesce is a walk that
            // repeats or skips rows without ever saying so.
            (Self::UpdatedAt, SortSource::Media) => Some("coalesce(m.updated_at, m.created_at)"),
            (Self::CreatedAt, SortSource::Media) => Some("m.created_at"),
            (Self::Title, SortSource::Media) => None,
        }
    }

    /// The select-list alias the cursor's value is read back from.
    ///
    /// The expression above is what SQL compares; this is what the row carries. They are separate
    /// names on purpose — `p.updated_at` is read as `updated_at` — and a reader that has to
    /// re-derive one from the other is a reader that will guess wrong for at least one sort.
    #[must_use]
    pub const fn read_as(self, source: SortSource) -> Option<&'static str> {
        match (self, source) {
            (Self::UpdatedAt, SortSource::Pages) | (Self::CreatedAt, SortSource::Pages) => {
                Some(match self {
                    Self::UpdatedAt => "updated_at",
                    _ => "created_at",
                })
            }
            (Self::Title, SortSource::Pages) => Some("title"),
            (Self::UpdatedAt, SortSource::Media) => Some("cursor_stamp"),
            (Self::CreatedAt, SortSource::Media) => Some("created_at"),
            (Self::Title, SortSource::Media) => None,
        }
    }
}

/// The one place a timestamp becomes text for a cursor.
///
/// A cursor is a *string*, so an instant has to be written and read in one agreed format. Both
/// halves lived in the API layer and disagreed: the pages writer formatted RFC 3339 while the
/// media writer used the driver's `Display` (`2026-09-30 21:53:21.509904 +00:00:00`), and the
/// media reader parsed RFC 3339. Each one was internally consistent and every media page after the
/// first was a `400`. The inverse had already been fixed once in the other direction, which is
/// what makes this worth naming: the format has to be a function, not a convention.
///
/// [`cursor_instant`] is the reader, and it is the only thing that parses a cursor's value.
#[must_use]
pub fn stamp(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

/// Read back a cursor value that [`stamp`] wrote, or refuse the sort order it belongs to.
///
/// The refusal is deliberately about the *sort*: a cursor from a `title` walk handed to a
/// `updated_at` walk is a caller mistake the message can actually explain, whereas "this cursor
/// is not valid" teaches the caller nothing.
pub fn cursor_instant(value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        ContentError::InvalidQuery("this cursor does not belong to this sort order".to_string())
    })
}

/// Whether a result set that returned `fetched` rows for a `limit` has another page.
///
/// The list queries fetch `limit + 1`, so `fetched > limit` is the whole test and it is exact:
/// a set of exactly `limit` rows returns `limit` and stops, and a set of `limit + 1` returns
/// `limit + 1` and continues. There is no count query, no extra round trip and no guess.
///
/// **`fetched == limit` is the rule that was in this crate's `Page` helper, and it is wrong.** It
/// reads a full page as "there may be more", so the last page of a set whose size is a multiple of
/// the limit hands back a cursor that walks to an empty page — a client that stops on an empty
/// page reports a duplicate empty fetch forever, and one that stops on the count reports a
/// truncated set. The two rules differ on exactly the sets whose size is divisible by the limit,
/// which is why a single-page assertion can never see the difference.
#[must_use]
pub const fn continues(fetched: i64, limit: i64) -> bool {
    fetched > limit
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
        // **`stamp`, not the driver's `Display`.** These two are the *only* places the surface
        // shows a caller an instant, and `updated_since` accepts only RFC 3339 — so a
        // `to_string()` here means the documented rebuild primitive does not work: read a page,
        // take its `updated_at`, ask for changes since it, get a `400`. The integration test had
        // a hand-written `to_rfc3339` helper to work around exactly this, which is the shape of a
        // product bug wearing a test's clothes. The API now round trips its own output.
        value.insert("updated_at".into(), Value::String(stamp(self.updated_at)));
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
        // **`stamp`, not the driver's `Display`.** These two are the *only* places the surface
        // shows a caller an instant, and `updated_since` accepts only RFC 3339 — so a
        // `to_string()` here means the documented rebuild primitive does not work: read a page,
        // take its `updated_at`, ask for changes since it, get a `400`. The integration test had
        // a hand-written `to_rfc3339` helper to work around exactly this, which is the shape of a
        // product bug wearing a test's clothes. The API now round trips its own output.
        value.insert("updated_at".into(), Value::String(stamp(self.updated_at)));
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

// A `Page` helper used to live here, with `next_cursor` derived from `fetched == limit`. Nothing
// ever called it: the routes build their own envelope, and they use the only correct rule —
// `fetched > limit`, decided from an over-fetch of one row. So the helper and the product
// disagreed about the same question, in the same crate, and the helper was the wrong one. A
// "full page may have more" rule hands the last full page a cursor that walks to an empty result,
// which is the behaviour the over-fetch exists to avoid; the wrong copy was only a bug away from
// being the copy that got used.

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
    fn a_cursor_is_issued_only_when_a_row_is_left_behind() {
        // The exact cases the old `fetched == limit` rule got wrong. `continues` is fed the count
        // the query actually returns, which is `limit + 1` when more exist.
        assert!(
            continues(3, 2),
            "three rows for a limit of two means one is behind the cursor"
        );
        assert!(
            !continues(2, 2),
            "two rows for a limit of two is the last page, and the rule that said otherwise \
             handed this page a cursor that walks to an empty result"
        );
        assert!(!continues(0, 5), "an empty page has nothing to continue");
        assert!(!continues(1, 5), "a short page is the last page");
    }

    #[test]
    fn a_sort_the_relation_does_not_have_is_refused_by_name() {
        // `pages` has no `title` column — the title is the revision's — and `media` has no title
        // at all. Both used to be prefixed blindly and reach SQL as `p.title` / `m.title`.
        assert_eq!(
            SortKey::parse_for("title", SortSource::Pages).expect("pages sort by title"),
            SortKey::Title
        );
        let refused = SortKey::parse_for("title", SortSource::Media)
            .expect_err("a file has no title to sort by");
        let message = refused.to_string();
        assert!(
            message.contains("title"),
            "it names what was asked: {message}"
        );
        assert!(
            message.contains("updated_at"),
            "and lists what this endpoint does offer: {message}"
        );
        // And the refusal is not a crash on a missing column.
        assert!(SortKey::Title.expression(SortSource::Media).is_none());
    }

    #[test]
    fn every_sort_expression_is_a_column_that_qualifies_to_its_own_table() {
        // The whole point of qualifying: a name that does not belong to the relation is a 500 the
        // first time a caller uses the documented parameter.
        for sort in [SortKey::UpdatedAt, SortKey::CreatedAt, SortKey::Title] {
            let expression = sort
                .expression(SortSource::Pages)
                .expect("pages can sort three ways");
            assert!(
                expression.starts_with("p.") || expression.starts_with("r."),
                "{expression} is not qualified to the pages query"
            );
        }
        for sort in [SortKey::UpdatedAt, SortKey::CreatedAt] {
            let expression = sort
                .expression(SortSource::Media)
                .expect("media sorts by its timestamps");
            assert!(
                expression.starts_with("m.") || expression.starts_with("coalesce(m."),
                "{expression} is not qualified to the media query"
            );
        }
    }

    #[test]
    fn the_media_sort_and_the_row_it_is_read_back_from_agree() {
        // The media default sorts on a coalesce, so the value the cursor holds is NOT `updated_at`
        // and cannot be read back as one. `read_as` names the alias, and the media query must
        // select that alias — this is the pair that has to be built together.
        assert_eq!(
            SortKey::UpdatedAt.read_as(SortSource::Media),
            Some("cursor_stamp"),
            "a coalesce has to be selected under a name a row can be read by"
        );
        assert_eq!(
            SortKey::CreatedAt.read_as(SortSource::Media),
            Some("created_at")
        );
        assert_eq!(SortKey::Title.read_as(SortSource::Media), None);
    }

    #[test]
    fn a_stamp_written_into_a_cursor_reads_back_as_the_same_instant() {
        // The round trip the media list got wrong: the writer used the driver's `Display`
        // (`2026-09-30 21:53:21.509904 +00:00:00`) and the reader parsed RFC 3339, so page one
        // worked and every page after it was a 400.
        let at = OffsetDateTime::parse("2026-09-30T21:53:21.509904Z", &Rfc3339).expect("time");
        let written = stamp(at);
        assert!(written.ends_with('Z'), "RFC 3339 in UTC: {written}");
        assert!(
            !written.contains(" +00:00:00"),
            "and not the driver's display form: {written}"
        );
        assert_eq!(cursor_instant(&written).expect("reads back"), at);
    }

    #[test]
    fn a_title_cursor_is_refused_where_a_timestamp_one_is_required() {
        // The two ends of a walk, told apart by the sort rather than by a generic "not valid".
        let error = cursor_instant("Some Title").expect_err("a title is not an instant");
        assert!(
            error.to_string().contains("sort order"),
            "the message has to be actionable: {error}"
        );
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
