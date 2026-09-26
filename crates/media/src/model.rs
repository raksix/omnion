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
