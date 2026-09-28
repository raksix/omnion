//! Rows and shapes of the media library.

use time::OffsetDateTime;
use uuid::Uuid;

/// Largest file the media library accepts.
///
/// The panel and the API enforce the same number; the enterprise file manager raises it per
/// storage setting later (docs/requests/REQ-010).
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// Longest file name a media row keeps.
pub const MAX_FILENAME_LENGTH: usize = 200;

/// A stored file of one site.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Media {
    /// Primary key — also the identifier the public serve path uses.
    pub id: Uuid,
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// Object key inside the bucket.
    pub storage_key: String,
    /// File name as the uploader wrote it, reduced to its safe form.
    pub filename: String,
    /// Content type the upload declared, normalised to `type/subtype`.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Account that uploaded the file, when a person did.
    pub created_by: Option<Uuid>,
    /// When the file arrived.
    pub created_at: OffsetDateTime,
}

impl Media {
    /// Size in bytes as an unsigned number.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size_bytes.max(0).unsigned_abs()
    }
}

/// A stored file as the file manager reads it: the core row plus its folder, its editorial
/// fields and the three states it can be in (REQ-010).
///
/// The jsonb and array columns are read as `serde_json::Value` / `Vec<String>` directly by sqlx, so
/// a row with an unexpected shape surfaces as a decode error at the boundary rather than as a
/// `NULL` that later code has to guess about.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MediaFile {
    /// Primary key.
    pub id: Uuid,
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// Object key inside the bucket — immutable for the life of the file.
    pub storage_key: String,
    /// File name as the uploader wrote it, reduced to its safe form.
    pub filename: String,
    /// Content type the file is stored with.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Account that uploaded the file.
    pub created_by: Option<Uuid>,
    /// When the file arrived.
    pub created_at: OffsetDateTime,
    /// Folder the file sits in (`None` before the library root was assigned).
    pub folder_id: Option<Uuid>,
    /// Last change that was not a version.
    pub updated_at: Option<OffsetDateTime>,
    /// When the file was moved to the trash.
    pub deleted_at: Option<OffsetDateTime>,
    /// Who moved it to the trash.
    pub deleted_by: Option<Uuid>,
    /// When the bytes were actually removed.
    pub purged_at: Option<OffsetDateTime>,
    /// Accessibility text.
    pub alt_text: String,
    /// Editorial caption.
    pub caption: String,
    /// Longer description.
    pub description: String,
    /// Editor-defined key/value pairs.
    pub metadata: serde_json::Value,
    /// Tags.
    pub tags: Vec<String>,
    /// Pixel width, when the format carries it.
    pub width: Option<i32>,
    /// Pixel height, when the format carries it.
    pub height: Option<i32>,
    /// Duration in milliseconds, for audio and video.
    pub duration_ms: Option<i32>,
    /// Page count, for documents.
    pub page_count: Option<i32>,
    /// What the file's own EXIF block said, when its format carries one.
    ///
    /// Held as the stored json rather than as an [`Exif`](crate::exif::Exif) so sqlx decodes the
    /// column natively: a row written by a future release, or by a hand-edited jsonb column, must
    /// not make a whole file listing fail to decode. [`MediaFile::exif`] parses it, and an
    /// unrecognised key in there is ignored rather than refused.
    pub exif: Option<serde_json::Value>,
    /// Scan state: `pending`, `clean`, `flagged`, `skipped` or `error`.
    pub scan_status: String,
    /// What the scanner reported.
    pub scan_detail: String,
    /// How many versions the file has.
    pub version_count: i32,
    /// Whether the public renderer may read it without a session.
    pub is_public: bool,
}

impl MediaFile {
    /// The camera record, parsed, or `None` when the file carries none.
    ///
    /// A column that is present but unreadable is `None` as well: a row written by a future
    /// release with a key this one does not know is not a file whose metadata tab should refuse
    /// to open.
    #[must_use]
    pub fn exif(&self) -> Option<crate::exif::Exif> {
        let value = self.exif.as_ref()?;
        let parsed = crate::exif::Exif::from_json(value);
        (!parsed.is_empty()).then_some(parsed)
    }

    /// The dimensions a layout should reserve, with the stored rotation applied.
    #[must_use]
    pub fn display_size(&self) -> (Option<i32>, Option<i32>) {
        crate::exif::oriented_size(
            self.width,
            self.height,
            self.exif().and_then(|e| e.orientation),
        )
    }

    /// Size in bytes as an unsigned number.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size_bytes.max(0).unsigned_abs()
    }

    /// The `kind` the browser filters by: the content type's main type (`image`, `video`, …).
    #[must_use]
    pub fn kind(&self) -> &str {
        self.content_type
            .split('/')
            .next()
            .unwrap_or(&self.content_type)
    }

    /// A compact description for a `description` list — the two lines a list row shows.
    ///
    /// The size a reader sees, not the size the pixels are stored at: a portrait photograph
    /// stored sideways is `3000×4000` on screen, and a list that prints `4000×3000` sends an
    /// editor looking for a landscape crop of a picture that has none.
    #[must_use]
    pub fn dimensions(&self) -> Option<String> {
        match self.display_size() {
            (Some(width), Some(height)) => Some(format!("{width}×{height}")),
            _ => None,
        }
    }
}

/// A media row that is about to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMedia {
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// Object key the bytes were written under.
    pub storage_key: String,
    /// Reduced file name.
    pub filename: String,
    /// Normalised content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Account that uploaded the file.
    pub created_by: Option<Uuid>,
}
