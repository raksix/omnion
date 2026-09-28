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
    /// When the scanner last wrote `scan_status`; null means it never has.
    pub scanned_at: Option<OffsetDateTime>,
    /// The engine name the scanner reported for the last verdict.
    pub scan_engine: Option<String>,
    /// How many versions the file has.
    pub version_count: i32,
    /// Whether the public renderer may read it without a session.
    pub is_public: bool,
    /// Whether a legal hold is on the file: no retention sweep may remove it (REQ-010).
    pub legal_hold: bool,
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
    ///
    /// The columns already *are* the rotated pair — [`crate::versions::fill_exif`] wrote them
    /// oriented — so a layout that reads them reserves the box a browser draws. This returns them
    /// unchanged, and says so at length, because the obvious version of this function applies the
    /// rotation a second time: the stored width is 3000 for an orientation-6 4000×3000 frame, the
    /// swap runs again, and the panel reports a landscape picture for a portrait photograph. A
    /// helper whose name says "apply the rotation" is exactly where a double application hides,
    /// because a unit test on the *function* still passes — the bug is in which function the
    /// *column* has already been through.
    #[must_use]
    pub fn display_size(&self) -> (Option<i32>, Option<i32>) {
        (self.width, self.height)
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
    /// The size a reader sees, which is what the columns hold: they were written oriented, so a
    /// portrait photograph stored sideways reads `3000×4000`, and a list printing the stored
    /// `4000×3000` would send an editor looking for a landscape crop of a picture that has none.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::Exif;
    use serde_json::json;

    /// A file row with a rotated camera record and the oriented columns the writer stored.
    fn rotated_file() -> MediaFile {
        MediaFile {
            id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            storage_key: "sites/a/b.jpg".to_owned(),
            filename: "b.jpg".to_owned(),
            content_type: "image/jpeg".to_owned(),
            size_bytes: 1,
            checksum: "0".repeat(64),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            folder_id: None,
            updated_at: None,
            deleted_at: None,
            deleted_by: None,
            purged_at: None,
            alt_text: String::new(),
            caption: String::new(),
            description: String::new(),
            metadata: serde_json::Value::Null,
            tags: Vec::new(),
            // The writer already applied the rotation: an orientation-6 4000x3000 frame is
            // stored as 3000x4000, because that is the box a browser draws.
            width: Some(3000),
            height: Some(4000),
            duration_ms: None,
            page_count: None,
            exif: Some(json!({ "orientation": 6 })),
            scan_status: "clean".to_owned(),
            scan_detail: String::new(),
            scanned_at: Some(OffsetDateTime::UNIX_EPOCH),
            scan_engine: Some("stub".to_owned()),
            version_count: 1,
            is_public: false,
            legal_hold: false,
        }
    }

    /// The rotation is applied once, when the column is written — not again on the way out.
    ///
    /// This is the bug the browser probe found: `display_size` re-applied the swap to columns
    /// that had already been through it, so the panel reported a landscape picture for a portrait
    /// photograph. A unit test on `oriented_size` still passed throughout, because the function was
    /// correct — it was being called on the wrong input.
    #[test]
    fn the_rotation_is_applied_once_not_twice() {
        let file = rotated_file();
        assert_eq!((file.width, file.height), (Some(3000), Some(4000)));
        assert_eq!(file.display_size(), (Some(3000), Some(4000)));
        assert_eq!(file.dimensions().as_deref(), Some("3000×4000"));
    }

    /// A file with no rotation is what it always was.
    #[test]
    fn an_upright_file_reports_its_own_size() {
        let mut file = rotated_file();
        file.width = Some(4000);
        file.height = Some(3000);
        file.exif = None;
        assert_eq!(file.display_size(), (Some(4000), Some(3000)));
        assert!(file.exif().is_none(), "no record is not an empty record");
    }

    /// The record round-trips through the column, and a record with nothing in it is `None`.
    #[test]
    fn a_record_survives_the_column_and_an_empty_one_is_none() {
        let file = rotated_file();
        let parsed = file.exif().expect("the record reads back");
        assert_eq!(parsed.orientation, Some(6));

        let mut plain = rotated_file();
        plain.exif = Some(serde_json::json!({}));
        assert!(plain.exif().is_none(), "an empty object is not a record");

        // A row written by a future release with a key this one does not know is still a file.
        let mut future = rotated_file();
        future.exif = Some(json!({ "orientation": 6, "lens_serial": "ABC" }));
        assert_eq!(
            future.exif().expect("unknown keys are ignored").orientation,
            Some(6)
        );
    }

    /// A column whose values are the wrong json type is absent, not a panic.
    #[test]
    fn a_hand_edited_column_does_not_break_a_listing() {
        let mut file = rotated_file();
        file.exif = Some(json!({ "iso": "four hundred", "make": 7 }));
        assert!(
            file.exif().is_none(),
            "nothing that reads as what it claims to be"
        );
    }

    /// A record with a body and a shutter prints the line the tab shows.
    #[test]
    fn the_summary_line_names_the_camera_and_the_exposure() {
        let record = Exif {
            make: Some("Canon".to_owned()),
            model: Some("EOS R5".to_owned()),
            iso: Some(400),
            exposure_ms: Some(5),
            aperture_x100: Some(180),
            focal_length_mm: Some(50),
            ..Exif::default()
        };
        assert_eq!(
            record.headline().as_deref(),
            Some("Canon EOS R5 · ISO 400 · 1/200 s · f/1.8 · 50 mm")
        );
        assert!(Exif::default().headline().is_none());
    }
}
