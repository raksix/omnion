//! `/api/v1/media` — the file manager surface (REQ-010, slice 1).
//!
//! The v0 library was one flat list per site. This module turns it into a file system: folders, a
//! browser listing with filters and sorting, a bulk bar over a selection, and a trash with a
//! countdown. Three rules are enforced here rather than in the client, because a client can be
//! anything:
//!
//! * **a move never rewrites the storage key** — `folder_id` is the only thing that changes, so
//!   published pages, cached derivatives and signed URLs keep pointing at the same bytes;
//! * **a delete is a state** — `DELETE /api/v1/media/{id}` trashes, and only the explicit `purge`
//!   route removes bytes, so a mistaken delete is recoverable;
//! * **a folder of another site is not reachable** — every folder id is resolved through the
//!   caller's organization before it is read or written, on top of the permission guard.
//!
//! The media routes live in [`crate::routes::media`] still; this file adds the file-manager half so
//! the v0 contract (`GET /api/v1/media?site_id=…` returning `{site_id, media}`) keeps answering for
//! the clients that already use it, while the browser gets `/api/v1/media/files`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::Site;
use omnion_media::{
    Folder, FolderMove, ListQuery, MediaError, MediaFile, MetadataPatch, NewFolder, Sort,
    TrashEntry, child_path, sanitize_folder_name, validate_folder_name,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::state::AppState;

/// Default window a trashed file is kept, in days, until retention policies (slice 4) land.
pub const DEFAULT_TRASH_DAYS: i64 = 30;

/// Largest selection one bulk call may carry.
pub const MAX_BULK_SELECTION: usize = 500;

// ---------------------------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------------------------

/// One file as the browser and the file-detail screen need it.
#[derive(Debug, Serialize)]
pub struct FileBody {
    /// Media id.
    pub id: Uuid,
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// Folder the file sits in.
    pub folder_id: Option<Uuid>,
    /// File name.
    pub filename: String,
    /// Content type.
    pub content_type: String,
    /// Main type of the content type — the `kind` filter.
    pub kind: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256.
    pub checksum: String,
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
    /// Pixel width, when known.
    pub width: Option<i32>,
    /// Pixel height, when known.
    pub height: Option<i32>,
    /// Duration in milliseconds, when known.
    pub duration_ms: Option<i32>,
    /// Page count, when known.
    pub page_count: Option<i32>,
    /// Scan state.
    pub scan_status: String,
    /// How many versions the file has.
    pub version_count: i32,
    /// Panel read path of the bytes.
    pub raw_path: String,
    /// Public read path of the bytes.
    pub public_path: String,
    /// Who uploaded it.
    pub uploaded_by: Option<Uuid>,
    /// When it arrived, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change that was not a version, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

impl FileBody {
    /// Describe one row for the panel.
    fn build(file: &MediaFile) -> Self {
        Self {
            id: file.id,
            site_id: file.site_id,
            folder_id: file.folder_id,
            filename: file.filename.clone(),
            content_type: file.content_type.clone(),
            kind: file.kind().to_owned(),
            size_bytes: file.size(),
            checksum: file.checksum.clone(),
            alt_text: file.alt_text.clone(),
            caption: file.caption.clone(),
            description: file.description.clone(),
            metadata: file.metadata.clone(),
            tags: file.tags.clone(),
            width: file.width,
            height: file.height,
            duration_ms: file.duration_ms,
            page_count: file.page_count,
            scan_status: file.scan_status.clone(),
            version_count: file.version_count,
            raw_path: format!("/api/v1/media/{}/raw", file.id),
            public_path: format!("/api/v1/public/media/{}", file.id),
            uploaded_by: file.created_by,
            created_at: file.created_at,
            updated_at: file.updated_at,
        }
    }
}

/// One folder of the tree.
#[derive(Debug, Clone, Serialize)]
pub struct FolderBody {
    /// Folder id.
    pub id: Uuid,
    /// Parent folder; `None` for the library root.
    pub parent_id: Option<Uuid>,
    /// Name as typed.
    pub name: String,
    /// Materialised path from the root.
    pub path: String,
    /// `true` for the library root, which cannot be renamed, moved or deleted.
    pub is_root: bool,
    /// Depth below the root (the root itself is 0).
    pub depth: usize,
    /// How many live files sit directly in this folder.
    pub file_count: i64,
    /// When the folder was created, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl FolderBody {
    /// Describe one folder for the panel.
    fn build(folder: &Folder, file_count: i64) -> Self {
        Self {
            id: folder.id,
            parent_id: folder.parent_id,
            name: folder.name.clone(),
            path: folder.path.clone(),
            is_root: folder.is_root(),
            // The root's own name is segment 0, so a child sits one deeper.
            depth: folder.segments().len().saturating_sub(1),
            file_count,
            created_at: folder.created_at,
        }
    }
}

/// The browser listing: the files of a folder plus the total the filters matched.
#[derive(Debug, Serialize)]
pub struct FileListResponse {
    /// Site the listing belongs to.
    pub site_id: Uuid,
    /// Folder the listing is scoped to.
    pub folder_id: Option<Uuid>,
    /// Breadcrumb from the root, for the panel's path bar.
    pub breadcrumb: Vec<BreadcrumbCrumb>,
    /// The rows of this page.
    pub files: Vec<FileBody>,
    /// How many rows the filters match in total.
    pub total: i64,
    /// Whether a next page exists.
    pub has_more: bool,
}

/// One step of the breadcrumb.
#[derive(Debug, Clone, Serialize)]
pub struct BreadcrumbCrumb {
    /// Folder id.
    pub id: Uuid,
    /// Folder name.
    pub name: String,
}

/// The folder tree of one site.
#[derive(Debug, Serialize)]
pub struct FolderTreeResponse {
    /// Site the tree belongs to.
    pub site_id: Uuid,
    /// The root folder, always present.
    pub root: FolderBody,
    /// Every folder of the site in path order.
    pub folders: Vec<FolderBody>,
}

/// The trash listing with its summary.
#[derive(Debug, Serialize)]
pub struct TrashResponse {
    /// Site the trash belongs to.
    pub site_id: Uuid,
    /// How many days a trashed file is kept before its bytes go.
    pub retention_days: i64,
    /// How many files the trash holds.
    pub file_count: i64,
    /// How many bytes they still hold.
    pub total_bytes: i64,
    /// The trashed rows of this page.
    pub entries: Vec<TrashBody>,
}

/// One trashed file.
#[derive(Debug, Serialize)]
pub struct TrashBody {
    /// The file.
    #[serde(flatten)]
    pub file: FileBody,
    /// When it was moved to the trash, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// Who moved it there.
    pub deleted_by: Option<Uuid>,
    /// When the retention window purges it, RFC 3339.
    #[serde(with = "time::serde::rfc3339::option")]
    pub purges_at: Option<OffsetDateTime>,
}

/// The answer to a bulk action: what changed, and what did not.
#[derive(Debug, Serialize)]
pub struct BulkResponse {
    /// How many files the request asked about.
    pub requested: usize,
    /// How many files actually changed.
    pub changed: u64,
    /// Per-file failures, so a partial bulk never looks like a complete one.
    pub failures: Vec<BulkFailure>,
}

/// One file a bulk action could not change.
#[derive(Debug, Serialize)]
pub struct BulkFailure {
    /// The file.
    pub id: Uuid,
    /// Why it was refused.
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/media/files` — the browser listing.
#[derive(Debug, Deserialize)]
pub struct FileQuery {
    /// Site whose library is listed.
    pub site_id: Uuid,
    /// Folder to list; omitted lists the whole library.
    pub folder_id: Option<Uuid>,
    /// Include the folder's subfolders.
    #[serde(default)]
    pub recursive: bool,
    /// Free-text match on the file name.
    pub search: Option<String>,
    /// Content-type prefix (`image`, `video`, `application/pdf`, …).
    pub kind: Option<String>,
    /// Smallest size in bytes.
    pub min_bytes: Option<i64>,
    /// Largest size in bytes.
    pub max_bytes: Option<i64>,
    /// Only files uploaded by this account.
    pub uploaded_by: Option<Uuid>,
    /// Only files uploaded at or after this moment.
    pub created_after: Option<OffsetDateTime>,
    /// Only files uploaded before this moment.
    pub created_before: Option<OffsetDateTime>,
    /// Only files carrying this tag.
    pub tag: Option<String>,
    /// Only files in this scan state.
    pub scan_status: Option<String>,
    /// Only files with more than one version.
    #[serde(default)]
    pub has_versions: bool,
    /// Sort key.
    pub sort: Option<String>,
    /// Page size (1–500).
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

impl FileQuery {
    /// The store query this request describes.
    ///
    /// Built from a borrow so the handler keeps the `sort` key, which the store query does not
    /// carry: sorting is presentation, filtering is the query.
    fn list_query(&self) -> ListQuery {
        ListQuery {
            folder_id: self.folder_id,
            include_subfolders: self.recursive,
            search: self.search.clone(),
            kind: self.kind.clone(),
            min_bytes: self.min_bytes,
            max_bytes: self.max_bytes,
            uploaded_by: self.uploaded_by,
            created_after: self.created_after,
            created_before: self.created_before,
            tag: self.tag.clone(),
            scan_status: self.scan_status.clone(),
            has_versions: self.has_versions,
            limit: self.limit.unwrap_or(100).clamp(1, 500),
            offset: self.offset.unwrap_or(0).max(0),
        }
    }
}

/// `POST /api/v1/media/folders` — create one folder.
#[derive(Debug, Deserialize)]
pub struct CreateFolderBody {
    /// Site the folder belongs to.
    pub site_id: Uuid,
    /// Parent folder; omitted means the library root.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Name as typed.
    pub name: String,
}

/// `PATCH /api/v1/media/folders/{id}` — rename or move one folder.
#[derive(Debug, Deserialize)]
pub struct MoveFolderBody {
    /// New name; omitted keeps the current one.
    pub name: Option<String>,
    /// New parent; omitted keeps the current one.
    pub parent_id: Option<Uuid>,
}

/// `PATCH /api/v1/media/files/{id}` — rename, move, edit metadata.
#[derive(Debug, Deserialize)]
pub struct UpdateFileBody {
    /// New file name.
    pub filename: Option<String>,
    /// New accessibility text.
    pub alt_text: Option<String>,
    /// New caption.
    pub caption: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// The whole tag set.
    pub tags: Option<Vec<String>>,
    /// The whole metadata object.
    pub metadata: Option<serde_json::Value>,
    /// The folder to move the file into; `null` moves it to the library root.
    pub folder_id: Option<Option<Uuid>>,
}

impl UpdateFileBody {
    /// The store patch this request describes.
    ///
    /// `folder_id` is `Option<Option<Uuid>>` in both types, so "leave it where it is" and "move it
    /// to the library root" stay distinguishable: a request that omits the field does not move the
    /// file, a request that sends `null` does.
    fn into_patch(self) -> MetadataPatch {
        MetadataPatch {
            filename: self.filename,
            alt_text: self.alt_text,
            caption: self.caption,
            description: self.description,
            tags: self.tags,
            metadata: self.metadata,
            folder_id: self.folder_id,
        }
    }
}

/// What a bulk call does.
#[derive(Debug, Deserialize)]
pub struct BulkBody {
    /// Site the files belong to.
    pub site_id: Uuid,
    /// The files to act on.
    pub ids: Vec<Uuid>,
    /// `delete` (trash), `restore` or `purge`.
    pub action: String,
    /// Destination folder for `move`.
    pub folder_id: Option<Uuid>,
    /// Tags to add for `tag`.
    pub tags: Option<Vec<String>>,
}

/// `GET /api/v1/media/trash` — the trashed files of a site.
#[derive(Debug, Deserialize)]
pub struct TrashQuery {
    /// Site whose trash is listed.
    pub site_id: Uuid,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Handlers — folders
// ---------------------------------------------------------------------------------------------

/// The folder tree of a site, with the file count of every folder.
pub async fn folder_tree(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<FolderSiteQuery>,
) -> std::result::Result<Json<FolderTreeResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();
    // The root first: it is a get-or-create, so on a site created after the migration this is the
    // call that materialises it. Listing before materialising would answer a tree with no root in
    // it — exactly the empty rail the operator sees when they open a new site's media.
    let root = omnion_media::root_folder(pool, site.id).await?;
    let folders = omnion_media::list_folders(pool, site.id).await?;

    let mut bodies = Vec::with_capacity(folders.len());
    for folder in &folders {
        // The count is the files that sit *directly* in the folder. Files in a subfolder belong
        // to that subfolder's own count, so summing the tree would double-count the library.
        let count = omnion_media::count_in_folder(pool, folder.id).await?;
        bodies.push(FolderBody::build(folder, count.max(0)));
    }

    // The migration materialises one root per site, so the tree always has exactly one; the
    // fallback keeps the screen honest if an installation is mid-upgrade and has none.
    let root_body = bodies
        .iter()
        .find(|body| body.is_root)
        .cloned()
        .unwrap_or_else(|| FolderBody::build(&root, 0));

    Ok(Json(FolderTreeResponse {
        site_id: site.id,
        root: root_body,
        folders: bodies,
    }))
}

/// `GET /api/v1/media/folders` — the site query alone.
#[derive(Debug, Deserialize)]
pub struct FolderSiteQuery {
    /// Site whose tree is listed.
    pub site_id: Uuid,
}

/// Create one folder.
pub async fn create_folder(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateFolderBody>,
) -> std::result::Result<(StatusCode, Json<FolderBody>), ApiError> {
    let site = site_in_scope(&state, &current, body.site_id).await?;
    let pool = state.db().pool();

    let parent = match body.parent_id {
        Some(id) => folder_in_scope(pool, id, site.id).await?,
        None => omnion_media::root_folder(pool, site.id).await?,
    };

    let name = sanitize_folder_name(&body.name);
    validate_folder_name(&name)?;
    let path = child_path(&parent.path, &name)?;

    let folder = omnion_media::insert_folder(
        pool,
        NewFolder {
            site_id: site.id,
            parent_id: parent.id,
            name: name.clone(),
            parent_path: path,
            created_by: Some(current.user.id),
        },
    )
    .await?;

    bus::emit(
        pool,
        NewEvent::new("media.folder_created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "folder_id": folder.id,
                "parent_id": folder.parent_id,
                "name": folder.name,
                "path": folder.path,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.folder_created")
            .target("media_folder", folder.id.to_string())
            .metadata(json!({ "site_id": site.id, "name": folder.name, "path": folder.path }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(FolderBody::build(&folder, 0))))
}

/// Rename or move one folder, rewriting the paths of everything under it.
pub async fn move_folder(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(folder_id): Path<Uuid>,
    Json(body): Json<MoveFolderBody>,
) -> std::result::Result<Json<FolderBody>, ApiError> {
    let pool = state.db().pool();
    let existing = omnion_media::find_folder(pool, folder_id)
        .await?
        .ok_or_else(folder_not_found)?;
    let site = site_in_scope(&state, &current, existing.site_id).await?;

    // An omitted `parent_id` means "stay where you are", not "go to the root": the body documents
    // that, and a rename is by far the most common reason to call this route. Resolving the parent
    // from the current row also means a move and a rename cannot disagree about the tree.
    let parent = match body.parent_id {
        Some(id) => folder_in_scope(pool, id, site.id).await?,
        None => match existing.parent_id {
            Some(parent_id) => folder_in_scope(pool, parent_id, site.id).await?,
            None => omnion_media::root_folder(pool, site.id).await?,
        },
    };

    let name = match body.name {
        Some(raw) => {
            let name = sanitize_folder_name(&raw);
            validate_folder_name(&name)?;
            name
        }
        None => existing.name.clone(),
    };

    let moved = omnion_media::move_folder(
        pool,
        folder_id,
        &FolderMove {
            name,
            parent_path: parent.path.clone(),
        },
    )
    .await?;

    bus::emit(
        pool,
        NewEvent::new("media.folder_moved")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "folder_id": moved.id,
                "name": moved.name,
                "path": moved.path,
                "from_path": existing.path,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.folder_moved")
            .target("media_folder", moved.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "from_path": existing.path,
                "to_path": moved.path,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    let count = omnion_media::count_in_folder(pool, moved.id).await?;
    Ok(Json(FolderBody::build(&moved, count)))
}

/// Delete one empty folder. A folder that still holds something is refused with the count.
pub async fn delete_folder(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(folder_id): Path<Uuid>,
) -> std::result::Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let folder = omnion_media::find_folder(pool, folder_id)
        .await?
        .ok_or_else(folder_not_found)?;
    let site = site_in_scope(&state, &current, folder.site_id).await?;

    omnion_media::delete_empty_folder(pool, folder_id).await?;

    bus::emit(
        pool,
        NewEvent::new("media.folder_deleted")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({ "folder_id": folder_id, "path": folder.path })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.folder_deleted")
            .target("media_folder", folder_id.to_string())
            .metadata(json!({ "site_id": site.id, "path": folder.path }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Handlers — files
// ---------------------------------------------------------------------------------------------

/// The browser listing of one folder.
pub async fn list_files(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<FileQuery>,
) -> std::result::Result<Json<FileListResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();

    // A folder id from another site would otherwise list another tenant's files through this
    // site's scope, so the folder is resolved and refused before the listing runs.
    let mut list_query = query.list_query();
    if let Some(folder_id) = list_query.folder_id {
        let folder = folder_in_scope(pool, folder_id, site.id).await?;
        list_query.folder_id = Some(folder.id);
    }

    let page = omnion_media::list_files(
        pool,
        site.id,
        &list_query,
        Sort::parse(query.sort.as_deref()),
    )
    .await?;

    let breadcrumb = match list_query.folder_id {
        Some(folder_id) => breadcrumb_for(pool, folder_id).await?,
        None => Vec::new(),
    };

    Ok(Json(FileListResponse {
        site_id: site.id,
        folder_id: list_query.folder_id,
        breadcrumb,
        has_more: (list_query.offset + i64::try_from(page.files.len()).unwrap_or(0)) < page.total,
        total: page.total,
        files: page.files.iter().map(FileBody::build).collect(),
    }))
}

/// One file of the library, with the folder it sits in.
pub async fn get_file(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(file_id): Path<Uuid>,
) -> std::result::Result<Json<FileBody>, ApiError> {
    let file = file_in_scope(&state, &current, file_id).await?;
    Ok(Json(FileBody::build(&file)))
}

/// Rename, move or edit the metadata of one file.
pub async fn update_file(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
    Json(body): Json<UpdateFileBody>,
) -> std::result::Result<Json<FileBody>, ApiError> {
    let pool = state.db().pool();
    let existing = file_in_scope(&state, &current, file_id).await?;
    let site = site_of(&state, existing.site_id).await?;

    let mut patch = body.into_patch();
    if let Some(filename) = patch.filename.as_deref() {
        // A rename goes through the same reduction an upload does, so a renamed file cannot carry
        // a path separator or a control character into an object key or a header.
        let sanitized = omnion_media::sanitize_filename(filename)?;
        validate_folder_name(&sanitized)?;
        patch.filename = Some(sanitized);
    }
    if let Some(Some(folder_id)) = patch.folder_id {
        let folder = folder_in_scope(pool, folder_id, existing.site_id).await?;
        patch.folder_id = Some(Some(folder.id));
    }
    if let Some(tags) = patch.tags.as_deref() {
        patch.tags = Some(normalize_tags(tags));
    }

    if !omnion_media::update_file(pool, file_id, &patch).await? {
        return Err(file_not_found());
    }

    let updated = omnion_media::find_file(pool, file_id)
        .await?
        .ok_or_else(file_not_found)?;

    bus::emit(
        pool,
        NewEvent::new("media.updated")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": updated.id,
                "site_id": site.id,
                "filename": updated.filename,
                "folder_id": updated.folder_id,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.updated")
            .target("media", updated.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": updated.filename,
                "folder_id": updated.folder_id,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(FileBody::build(&updated)))
}

/// Move one file to the trash. The bytes stay until the trash is purged.
pub async fn trash_file(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
) -> std::result::Result<Json<FileBody>, ApiError> {
    let pool = state.db().pool();
    let existing = file_in_scope(&state, &current, file_id).await?;
    let site = site_of(&state, existing.site_id).await?;

    omnion_media::trash_files(pool, &[file_id], current.user.id).await?;
    let trashed = omnion_media::find_file_any_state(pool, file_id)
        .await?
        .ok_or_else(file_not_found)?;

    bus::emit(
        pool,
        NewEvent::new("media.deleted")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": trashed.id,
                "site_id": site.id,
                "filename": trashed.filename,
                "retained": true,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.deleted")
            .target("media", trashed.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": trashed.filename,
                "size_bytes": trashed.size(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(FileBody::build(&trashed)))
}

/// Bring one file back from the trash, into the folder it was deleted from.
pub async fn restore_file(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
) -> std::result::Result<Json<FileBody>, ApiError> {
    let pool = state.db().pool();
    let site = site_of_trashed(&state, &current, file_id).await?;

    omnion_media::restore_files(pool, &[file_id]).await?;
    let restored = omnion_media::find_file(pool, file_id)
        .await?
        .ok_or_else(file_not_found)?;

    bus::emit(
        pool,
        NewEvent::new("media.restored")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({ "media_id": restored.id, "site_id": site.id })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.restored")
            .target("media", restored.id.to_string())
            .metadata(json!({ "site_id": site.id, "filename": restored.filename }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(FileBody::build(&restored)))
}

/// Purge one file for good: the bytes and then the row.
pub async fn purge_file(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(file_id): Path<Uuid>,
) -> std::result::Result<StatusCode, ApiError> {
    let pool = state.db().pool();
    let site = site_of_trashed(&state, &current, file_id).await?;
    let file = omnion_media::find_file_any_state(pool, file_id)
        .await?
        .ok_or_else(file_not_found)?;

    if file.deleted_at.is_none() {
        return Err(ApiError::bad_request(
            "file_not_trashed",
            "move the file to the trash before purging it",
        ));
    }

    // The bytes go first: when the store refuses, the row stays and the operator can retry
    // instead of leaving a row that points at nothing.
    state.storage().delete(&file.storage_key).await?;
    if omnion_media::purge_files(pool, &[file_id]).await? == 0 {
        return Err(file_not_found());
    }

    bus::emit(
        pool,
        NewEvent::new("media.purged")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "media_id": file.id,
                "site_id": site.id,
                "filename": file.filename,
                "size_bytes": file.size(),
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.purged")
            .target("media", file.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "filename": file.filename,
                "size_bytes": file.size(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// The trash of a site, with the countdown on every row.
pub async fn list_trash(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<TrashQuery>,
) -> std::result::Result<Json<TrashResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();

    let entries = omnion_media::list_trash(
        pool,
        site.id,
        DEFAULT_TRASH_DAYS,
        query.limit.unwrap_or(100),
        query.offset.unwrap_or(0),
    )
    .await?;
    let (file_count, total_bytes) = omnion_media::trash_summary(pool, site.id).await?;

    Ok(Json(TrashResponse {
        site_id: site.id,
        retention_days: DEFAULT_TRASH_DAYS,
        file_count,
        total_bytes,
        entries: entries.iter().map(trash_body).collect(),
    }))
}

/// Purge every trashed file of a site. The retention worker calls the same rule from slice 4.
pub async fn empty_trash(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<TrashQuery>,
) -> std::result::Result<Json<BulkResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let pool = state.db().pool();

    let ids = omnion_media::trashed_ids(pool, site.id).await?;

    let keys = omnion_media::storage_keys(pool, &ids).await?;
    for key in keys {
        if let Err(error) = state.storage().delete(&key).await {
            tracing::warn!(error = %error, key, "a trashed object could not be removed");
        }
    }
    let purged = omnion_media::purge_files(pool, &ids).await?;

    bus::emit(
        pool,
        NewEvent::new("media.purged")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({ "site_id": site.id, "files_purged": purged })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.trash_emptied")
            .target("site", site.id.to_string())
            .metadata(json!({ "site_id": site.id, "files_purged": purged }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(BulkResponse {
        requested: ids.len(),
        changed: purged,
        failures: Vec::new(),
    }))
}

/// One bulk action over a selection: move, tag, delete, restore or purge.
pub async fn bulk_action(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<BulkBody>,
) -> std::result::Result<Json<BulkResponse>, ApiError> {
    let site = site_in_scope(&state, &current, body.site_id).await?;
    let pool = state.db().pool();

    if body.ids.len() > MAX_BULK_SELECTION {
        return Err(ApiError::bad_request(
            "selection_too_large",
            format!("a bulk action carries at most {MAX_BULK_SELECTION} files"),
        ));
    }

    let requested = body.ids.len();
    let mut failures = Vec::new();
    let mut eligible: Vec<Uuid> = Vec::new();

    // Every id is checked against this site first. A selection that mixes in a file of another
    // site gets a per-file failure rather than a whole-call refusal: an operator who selected
    // forty files and one of them is a stale row should still get the other thirty-nine.
    for id in &body.ids {
        let file = omnion_media::find_file_any_state(pool, *id).await?;
        match file {
            Some(file) if file.site_id == site.id => eligible.push(*id),
            Some(file) => failures.push(BulkFailure {
                id: *id,
                message: format!("{} belongs to another site", file.filename),
            }),
            None => failures.push(BulkFailure {
                id: *id,
                message: "no such file".to_owned(),
            }),
        }
    }

    let changed = match body.action.as_str() {
        "delete" => omnion_media::trash_files(pool, &eligible, current.user.id).await?,
        "restore" => omnion_media::restore_files(pool, &eligible).await?,
        "purge" => {
            let keys = omnion_media::storage_keys(pool, &eligible).await?;
            for key in keys {
                if let Err(error) = state.storage().delete(&key).await {
                    tracing::warn!(error = %error, key, "a bulk-purged object could not be removed");
                }
            }
            omnion_media::purge_files(pool, &eligible).await?
        }
        "move" => {
            let target = match body.folder_id {
                Some(id) => folder_in_scope(pool, id, site.id).await?.id,
                None => omnion_media::root_folder(pool, site.id).await?.id,
            };
            let mut moved = 0;
            for id in &eligible {
                match move_file(pool, *id, Some(target)).await {
                    Ok(()) => moved += 1,
                    Err(error) => failures.push(BulkFailure {
                        id: *id,
                        message: error.to_string(),
                    }),
                }
            }
            moved
        }
        "tag" => {
            let tags = normalize_tags(body.tags.as_deref().unwrap_or(&[]));
            if tags.is_empty() {
                return Err(ApiError::bad_request(
                    "tags_required",
                    "name at least one tag",
                ));
            }
            let mut tagged = 0;
            for id in &eligible {
                if let Some(file) = omnion_media::find_file(pool, *id).await? {
                    let merged = merge_tags(&file.tags, &tags);
                    if omnion_media::update_file(
                        pool,
                        *id,
                        &MetadataPatch {
                            tags: Some(merged),
                            ..MetadataPatch::default()
                        },
                    )
                    .await?
                    {
                        tagged += 1;
                    }
                }
            }
            tagged
        }
        other => {
            return Err(ApiError::bad_request(
                "unknown_bulk_action",
                format!("`{other}` is not a bulk action (move, tag, delete, restore, purge)"),
            ));
        }
    };

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.bulk_action")
            .target("site", site.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "action": body.action,
                "requested": requested,
                "changed": changed,
                "failed": failures.len(),
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(BulkResponse {
        requested,
        changed,
        failures,
    }))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Move one file into a folder, refusing a trashed one.
async fn move_file(
    pool: &sqlx::PgPool,
    file_id: Uuid,
    folder_id: Option<Uuid>,
) -> omnion_media::Result<()> {
    let patch = MetadataPatch {
        folder_id: Some(folder_id),
        ..MetadataPatch::default()
    };
    if omnion_media::update_file(pool, file_id, &patch).await? {
        Ok(())
    } else {
        Err(MediaError::FileTrashed)
    }
}

/// Reduce a tag list to the form the column stores: trimmed, lower-cased, unique, sorted.
fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut clean: Vec<String> = tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty() && tag.len() <= 60)
        .collect();
    clean.sort();
    clean.dedup();
    clean.truncate(20);
    clean
}

/// Add tags to a file's existing set.
fn merge_tags(existing: &[String], added: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = existing.to_vec();
    merged.extend_from_slice(added);
    normalize_tags(&merged)
}

/// The breadcrumb of one folder, from the root down to it.
///
/// The chain is walked through `parent_id`, not reconstructed from the `path`: two folders can
/// share a name at different depths, and a breadcrumb built by name would point at the wrong one.
async fn breadcrumb_for(
    pool: &sqlx::PgPool,
    folder_id: Uuid,
) -> std::result::Result<Vec<BreadcrumbCrumb>, ApiError> {
    let mut crumbs = Vec::new();
    let mut cursor = omnion_media::find_folder(pool, folder_id)
        .await?
        .ok_or_else(folder_not_found)?;

    // Bounded by the depth of a tree whose path cannot exceed MAX_FOLDER_PATH_LENGTH, so a
    // corrupted parent chain cannot spin here forever.
    loop {
        crumbs.push(BreadcrumbCrumb {
            id: cursor.id,
            name: cursor.name.clone(),
        });
        match cursor.parent_id {
            Some(parent_id) => {
                cursor = omnion_media::find_folder(pool, parent_id)
                    .await?
                    .ok_or_else(folder_not_found)?;
            }
            None => break,
        }
    }
    crumbs.reverse();
    Ok(crumbs)
}

/// Load a folder and refuse it when it belongs to another site.
async fn folder_in_scope(
    pool: &sqlx::PgPool,
    folder_id: Uuid,
    site_id: Uuid,
) -> omnion_media::Result<Folder> {
    let folder = omnion_media::find_folder(pool, folder_id)
        .await?
        .ok_or(MediaError::FolderNotFound)?;
    omnion_media::assert_same_site(&folder, site_id)?;
    Ok(folder)
}

/// Load a live file and refuse it when its site is out of the caller's scope.
async fn file_in_scope(
    state: &AppState,
    current: &CurrentSession,
    file_id: Uuid,
) -> std::result::Result<MediaFile, ApiError> {
    let file = omnion_media::find_file(state.db().pool(), file_id)
        .await?
        .ok_or_else(file_not_found)?;
    // The site load *is* the scope check: it answers 404 for a site that does not exist and
    // refuses one from another organization, so a file id alone never reaches another tenant.
    site_in_scope(state, current, file.site_id).await?;
    Ok(file)
}

/// Load the site of a trashed file and refuse it when it is out of the caller's scope.
async fn site_of_trashed(
    state: &AppState,
    current: &CurrentSession,
    file_id: Uuid,
) -> std::result::Result<Site, ApiError> {
    let file = omnion_media::find_file_any_state(state.db().pool(), file_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(file_not_found)?;
    site_in_scope(state, current, file.site_id).await
}

/// Load a site by id.
async fn site_of(state: &AppState, site_id: Uuid) -> std::result::Result<Site, ApiError> {
    crate::routes::media::site_of(state, site_id).await
}

/// Describe one trash entry.
fn trash_body(entry: &TrashEntry) -> TrashBody {
    TrashBody {
        file: FileBody::build(&entry.file),
        deleted_at: entry.file.deleted_at.unwrap_or(entry.file.created_at),
        deleted_by: entry.file.deleted_by,
        purges_at: entry.purges_at,
    }
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// `404` for a folder that does not exist.
fn folder_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "folder_not_found",
        "no such folder in this library",
    )
}

fn file_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "media_not_found",
        "no such media in this library",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_without_a_site_id_is_refused() {
        assert!(
            serde_json::from_str::<FileQuery>(
                r#"{"folder_id":"11111111-1111-1111-1111-111111111111"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<TrashQuery>(r#"{}"#).is_err());
    }

    #[test]
    fn a_bulk_action_names_itself() {
        let body: BulkBody = serde_json::from_str(
            r#"{"site_id":"11111111-1111-1111-1111-111111111111","ids":[],"action":"move","folder_id":"22222222-2222-2222-2222-222222222222"}"#,
        )
        .expect("a bulk body parses");
        assert_eq!(body.action, "move");
        assert_eq!(body.ids.len(), 0);
    }

    #[test]
    fn tags_are_reduced_before_they_are_stored() {
        let tags = normalize_tags(&[
            "  Summer ".to_owned(),
            "summer".to_owned(),
            String::new(),
            "AUTUMN".to_owned(),
        ]);
        assert_eq!(tags, vec!["autumn".to_owned(), "summer".to_owned()]);
        // Merging is idempotent: tagging a file twice does not double its tags.
        assert_eq!(merge_tags(&tags, &tags), tags);
    }

    #[test]
    fn a_tag_longer_than_the_column_is_dropped_rather_than_truncated() {
        let long = "x".repeat(61);
        assert!(normalize_tags(&[long]).is_empty());
    }

    #[test]
    fn the_listing_paging_defaults_stay_inside_the_bounds() {
        let query: FileQuery = serde_json::from_str(
            r#"{"site_id":"11111111-1111-1111-1111-111111111111","limit":9999,"offset":-5}"#,
        )
        .expect("a listing query parses");
        let list = query.list_query();
        assert_eq!(list.limit, 500);
        assert_eq!(list.offset, 0);
    }
}
