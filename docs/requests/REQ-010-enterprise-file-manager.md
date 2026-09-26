# REQ-010 — Enterprise File Manager

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/media`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A much more advanced version of the Media Library:

- folders
- permissions
- version history
- file preview
- metadata
- image transformation
- video preview
- PDF preview
- CDN
- S3
- duplicate detection
- virus scanning
- retention policies

## Implementation spec

> **Where:** `crates/media` (model, folders, versions, transformation, scan, retention) on top of `crates/storage` (object store abstraction: MinIO in development, any S3-compatible endpoint in production) · **Migration:** `database/migrations/0015_file_manager.sql` (next free slot at build time), extending the `0005_media.sql` table whose own comment already points forward to this request · **Admin routes:** `/media/*` (the existing library screen becomes the browser) · **Permission family:** `media.*` · **Depends on:** `crates/permissions` (folder grants reuse the IAM subject model, docs/07), `crates/audit`, `crates/events`, `crates/ai-hub` (optional alt-text assistance), REQ-011 for CDN purge, REQ-013 for backup inclusion.

### Scope (in / out)

**In**

- **Folders:** a per-site folder tree (create, rename, move, delete) with drag & drop, breadcrumbs and a slide-over tree on small screens; moving a folder or file never changes the storage key of the files inside.
- **Browser:** grid and list views, lazily generated thumbnails, multi-select with a bulk bar (move, tag, copy, download as archive, delete, restore), search and filters (kind, size, uploader, date range, tag, scan state, has-versions), sorting and a trash with a countdown.
- **Version history:** replacing a file creates a new version with its own storage key, checksum, size and author; any version can be downloaded; restoring an older version creates a newer one rather than rewriting history; text versions can be diffed.
- **Preview:** image (zoom, pan, fit/actual size, EXIF summary), video (poster frame, HTTP range streaming, captions when present), audio (player plus a waveform thumbnail), PDF (paged viewer), text/code (monospace with a line cap); unsupported types fall back to a download card with a file-type illustration, never a broken frame.
- **Metadata:** alt text, caption, description, tags, custom key/value pairs (`metadata jsonb` with a GIN index), EXIF/audio-tag extraction on upload, stored dimensions, duration and page count, and a metadata filter in the browser.
- **Image transformation:** named presets (width, height, fit `cover`/`contain`/`fill`, format `webp`/`avif`/`jpeg`/`png`, quality, optional watermark, optional blur placeholder) served on demand through a URL contract (`/api/v1/media/{id}/raw?preset=card`) with a content-addressed cache key so a CDN can hold results for a year.
- **Storage and delivery:** per-site storage settings (endpoint, region, bucket, path prefix, public/private default, signed-URL lifetime), a configurable public base URL, purge-on-replace through REQ-011, and a connection test that proves read/write with credentials held by reference.
- **Duplicate detection:** checksum grouping inside a site (optional cross-site search for platform owners), a report with reclaimable size, and a merge that keeps one file, repoints every reference and trashes the copies.
- **Virus scanning:** uploads enter `pending`, a scanner checks out of band, results are `clean`/`flagged`/`skipped`/`error`; flagged files are quarantined and not downloadable, an event fires, and an admin can release or delete them; an unreachable scanner never loses an upload, only blocks serving.
- **Permissions and sharing:** folder and file grants over the IAM subjects (user, group, role) with read/write/delete/share capability bits, inheritance down the tree with per-node override, and public share links with expiry, an optional password, a download counter and immediate revocation.
- **Retention and references:** policies (scope site or folder; keep versions N days; move to trash after N days; purge after N days; legal hold bypass) applied by a daily worker that records every run, a trash countdown in the UI, and a `media_references` table (maintained on publish plus a repair scan) that answers “used in” and makes purge refuse to delete a file still in use.

**Out (tracked elsewhere)**

- CDN adapters, cache rules and the purge queue → REQ-011; bucket backup/restore → REQ-013; quotas and storage billing → REQ-005/REQ-024.
- Document management (approvals, contract folders) → REQ-058; AI-generated alt text → REQ-042; PDF text extraction into the search index → REQ-002; the antivirus engine choice and its operations stay a pluggable scan client, not a bundled engine.

### Screens (UI)

| Route | Screen |
|---|---|
| `/media` | File browser: folder tree plus grid/list, toolbar, bulk bar |
| `/media/folders/{id}` | Deep link into a folder (shareable, keeps the tree state) |
| `/media/files/{id}` | File detail: preview, metadata, versions, permissions, usage, activity |
| `/media/duplicates` | Duplicate report grouped by checksum |
| `/media/trash` | Deleted files with countdown, restore and purge |
| `/media/settings` | Storage, transformation presets, scanning, retention (tabs) |

**Browser** — left: folder tree (expandable, drag targets, counts, `+ New folder`); right: breadcrumb (`Media / Campaigns / 2026`), toolbar (upload button plus a full-surface drop zone, grid/list toggle, sort, filter chips, search) and the content area. List columns: `Name` (type icon or thumbnail, inline rename on `F2`), `Kind`, `Size`, `Folder`, `Uploaded by`, `Modified`, `Versions`, `Scan` badge, `Actions` (preview, download, share, move, delete). Grid tiles show thumbnail, name, size and a selection checkbox, are keyboard reachable and open the preview on `Enter`. Bulk bar: `Move`, `Tag`, `Copy`, `Download .zip`, `Delete`, `Restore` (in trash) with a selection count and `Clear`. Keyboard: `⌘A` select all in view, `⌘U` upload, `n` new folder, arrows move focus, `Space` peeks, `F2` renames, `Del` deletes with a confirmation naming the count, `Esc` closes overlays, `⌘K` opens the command palette (REQ-032) with file actions.

**File detail** — preview on the left; tabs on the right: `Metadata` (name, path, size, MIME, dimensions/duration/pages, checksum with copy, uploaded by and at, alt text, caption, description, tags, custom key/value editor), `Versions` (number, size, author, date, `Restore`, `Download`, side-by-side diff for text), `Permissions` (inherit toggle, grant table `Subject`, `Kind`, `Read`, `Write`, `Delete`, `Share` with an add-grant form and subject picker, plus the share-link list with `Copy`, `Expires`, `Downloads`, `Revoke`), `Usage` (records referencing the file with deep links), `Activity` (uploads, version changes, moves, shares, scan results, retention actions). `New version` lives in the header and warns that live pages keep the old bytes until the CDN purge completes.

**Duplicates, trash, settings** — duplicates: groups table `Checksum`, `Files`, `Total size`, `Reclaimable`, `First seen`, expandable into member rows (`Name`, `Folder`, `Size`, `Uploaded`, `References`) with a `Keep this one` radio and `Merge group`; the cross-site toggle is visible only to platform owners and labelled as such. Trash: `Name`, `Deleted by`, `Deleted at`, `Purges in` (amber under 7 days), `Actions` (restore, purge now); `Empty trash` demands a typed confirmation and a banner explains the retention policy in force. Settings tabs: Storage (provider, endpoint, region, bucket, path prefix, public base URL, signed-URL lifetime 60–604800 s, default visibility, max upload 1–1024 MB, allowed MIME list, `Test connection` result panel), Transformation (preset table and editor with a live preview and a copyable example URL), Scanning (enabled toggle, scanner endpoint, timeout 1–120 s, action on `error`, quarantine list with `Release`/`Delete`), Retention (policy table and editor, `Run now` reporting the rows it touched); every numeric field has a range check and a field-level error.

**States, keyboard, mobile** — skeleton tiles or rows on load, a real empty state per view (`No files in this folder` with `Upload` and `New folder`, `No duplicates found` explaining how detection works), error states with retry; on mobile the tree becomes a slide-over, the grid is two columns, detail tabs become an accordion and the bulk bar docks at the bottom with the primary action first.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/media` | List files (folder, filters, sort, paging) / upload (`version_of` for replace) | `media.read` / `media.upload` |
| GET, PATCH, DELETE | `/api/v1/media/{id}` | Detail with versions, usage, scan state / rename, move, metadata / move to trash | `media.read` / `media.update` / `media.delete` |
| POST, DELETE | `/api/v1/media/{id}/restore` · `/purge` | Restore from trash / purge permanently (refuses while referenced) | `media.update` / `media.manage` |
| GET | `/api/v1/media/{id}/raw` · `/download` | Bytes with `?preset=` or `?w=&h=&fit=&format=` / original download | `media.read` |
| GET, POST | `/api/v1/media/{id}/versions` | Version history / upload a replacement | `media.read` / `media.upload` |
| POST | `/api/v1/media/{id}/versions/{version}/restore` | Restore a version as the newest one | `media.update` |
| GET | `/api/v1/media/{id}/references` · `/activity` | Usage records / activity trail | `media.read` |
| POST | `/api/v1/media/{id}/scan` · `/release` | Re-run the scanner / release a quarantined file | `media.manage` |
| GET, POST | `/api/v1/media/folders` | Folder tree / create a folder | `media.read` / `media.manage` |
| PATCH, DELETE | `/api/v1/media/folders/{id}` | Rename, move, delete (recursive ask) | `media.manage` |
| GET, PUT | `/api/v1/media/folders/{id}/grants` · `/media/{id}/grants` | Folder and file grants (inherit or override) | `media.manage` |
| GET, POST, DELETE | `/api/v1/media/{id}/shares` · `/media/shares/{id}` | Share links (expiry, optional password) / revoke | `media.share` |
| GET | `/api/v1/public/media/shared/{token}` | Public link download (expiry and password checked) | public token |
| POST, GET | `/api/v1/media/bulk` · `/media/bulk/download` | Bulk move, tag, copy, delete, restore / streamed `.zip` | `media.manage` / `media.read` |
| GET, POST | `/api/v1/media/duplicates` · `/media/duplicates/merge` | Duplicate groups / keep one, repoint references, trash the rest | `media.read` / `media.manage` |
| GET, POST | `/api/v1/media/trash` · `/media/trash/empty` | Trash listing with countdown / purge everything eligible | `media.read` / `media.manage` |
| GET, PUT | `/api/v1/media/settings` | Storage, scanning and retention settings | `media.settings.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/media/transformation-presets` | Preset CRUD | `media.settings.manage` |
| POST | `/api/v1/media/settings/test-connection` | Verify read/write against the object store | `media.settings.manage` |
| POST | `/api/v1/media/retention/run` | Apply retention rules now (audited) | `media.retention.manage` |

### Data model

```text
media        + folder_id uuid references media_folders(id) on delete set null, updated_at timestamptz, deleted_at timestamptz, deleted_by uuid, purged_at timestamptz, alt_text text not null default '', caption text not null default '', description text not null default '', metadata jsonb not null default '{}'::jsonb, tags text[] not null default '{}', width integer, height integer, duration_ms integer, page_count integer, scan_status text not null default 'pending', scan_engine text, scanned_at timestamptz, scan_detail text not null default '', version_count integer not null default 1, is_public boolean not null default false, retention_policy_id uuid
media_folders(id uuid pk, site_id uuid not null, parent_id uuid references media_folders(id) on delete restrict, name text not null, path text not null, created_by uuid, created_at, updated_at)
media_versions(id uuid pk, media_id uuid not null, version integer not null, storage_key text not null, size_bytes bigint not null, checksum text not null, content_type text not null, width integer, height integer, note text, created_by uuid, created_at, unique (media_id, version))
media_grants(id uuid pk, folder_id uuid, media_id uuid, subject_kind text not null, subject_id uuid not null, can_read boolean not null default true, can_write boolean not null default false, can_delete boolean not null default false, can_share boolean not null default false, effect text not null default 'allow', created_by uuid, created_at, check ((folder_id is null) <> (media_id is null)))   -- capability bits, not secrets
media_shares(id uuid pk, media_id uuid not null, token_hash text not null, password_hash text, expires_at timestamptz, download_count integer not null default 0, created_by uuid, created_at, revoked_at)
media_transformation_presets(id uuid pk, site_id uuid not null, name text not null, width integer, height integer, fit text not null default 'cover', format text not null default 'webp', quality integer not null default 80, watermark_media_id uuid, created_at, unique (site_id, name))
media_references(id uuid pk, media_id uuid not null, resource_kind text not null, resource_id text not null, field text not null default '', created_at, unique (media_id, resource_kind, resource_id, field))
media_retention_policies(id uuid pk, site_id uuid not null, name text not null, folder_id uuid, keep_versions_days integer not null default 365, trash_days integer not null default 30, purge_after_days integer not null default 90, legal_hold boolean not null default false, enabled boolean not null default true, created_at) · media_retention_runs(id uuid pk, site_id uuid, policy_id uuid, kind text not null, rows_touched bigint not null default 0, actor_user_id uuid, created_at)
```

Checks: `quality between 1 and 100`, `keep_versions_days >= 1`, `purge_after_days >= trash_days`, `fit in ('cover','contain','fill')`, `format in ('webp','avif','jpeg','png')`, `subject_kind in ('user','group','role')`, `scan_status in ('pending','clean','flagged','skipped','error')`. Indexes: `media_folder_idx (site_id, folder_id, created_at desc)`, partial `media_live_idx (site_id) where deleted_at is null`, `media_trash_idx (site_id, deleted_at desc) where deleted_at is not null`, `media_checksum_idx (site_id, checksum)`, `media_scan_idx (scan_status) where scan_status = 'pending'`, GIN on `metadata` and `tags`, `media_versions_media_idx (media_id, version desc)`, `media_shares_token_idx (token_hash)`.

Migration `database/migrations/0015_file_manager.sql` — append-only and commented in the `0005` style; it alters `media` with nullable columns only (existing rows read as `pending`, no folder, version 1), creates the new tables, backfills `version_count = 1` and `metadata = '{}'`, and seeds one `Standard` transformation preset plus one default retention policy per existing site.

### Events

- Emitted: `media.uploaded`, `media.updated`, `media.version_created`, `media.deleted`, `media.restored`, `media.purged`, `media.duplicate_merged`, `media.scan_clean`, `media.scan_flagged`, `media.scan_error`, `media.share_created`, `media.share_revoked`, `media.retention_applied`.
- Payloads carry ids, filename, size, checksum, folder path and scan state — never file bytes, never a share token or password. Consumed: `page.published` refreshes reference rows for the page’s media; `site.deleted` moves that site’s files to the trash rather than purging them.
- Webhook relevance: `media.version_created` and `media.purged` trigger the CDN purge (REQ-011); `media.scan_flagged` raises a security notification (REQ-012/REQ-021) and keeps the share route closed until release; automation rules may react to new uploads (for example generating derivatives).

### Acceptance criteria

- [ ] `0015_file_manager.sql` applies on a fresh database and on one with existing `media` rows; `cargo test -p omnion-media` is green.
- [ ] Folder create, rename, move and delete work; deleting a non-empty folder asks and behaves exactly as the confirmation text says.
- [ ] List and grid views share the selection; filters (kind, size, uploader, date, tag, scan state, has-versions) combine and the count matches the returned rows.
- [ ] Bulk move, tag, copy and delete apply per item and report partial failures; delete moves to trash and never purges.
- [ ] A replacement creates version N+1 with its own storage key and checksum; the previous version stays downloadable and restoring it creates a newer version.
- [ ] Image preview zooms and pans, video plays with range requests and a poster, PDF renders pages, text previews cap lines with a “download for the full file” hint, and an unsupported type shows a download card.
- [ ] `?preset=card` returns the configured size and format, results are cached by content hash, the key changes when the preset changes, and an unknown preset falls back to the original.
- [ ] Metadata and tags round-trip and are searchable; EXIF, duration and dimensions are filled for supported types on upload.
- [ ] The duplicate report groups by checksum inside a site, `Merge group` repoints references, keeps the chosen file and trashes copies, and the cross-site toggle is invisible to non-platform owners.
- [ ] Scanning: a clean file is servable, a flagged file is quarantined (raw and share routes refuse it), an unreachable scanner leaves the file stored but unserved, and an admin release writes an audit entry.
- [ ] Folder and file grants resolve with inheritance and per-node override; a deny beats an inherited allow; a user without `media.share` cannot create a link even with write access.
- [ ] Share links enforce expiry and the optional password, count downloads, revoke immediately, and are stored hashed.
- [ ] Retention removes versions past the keep window, purges trashed files past the trash window, never touches a file under legal hold, and records every run.
- [ ] Purge refuses a file that still has references and names the resources holding it.
- [ ] `Test connection` proves read/write and reports a clear failure; no settings response ever returns a credential value.
- [ ] Restore returns a file to its original folder when it still exists, otherwise to `Restored` with a note.
- [ ] Audit entries exist for upload, replace, move, delete, restore, purge, grant change, share creation and revocation, and scan release.
- [ ] Every screen has empty, loading and error states with zero high findings; the virtualised grid shows no broken thumbnails; mobile keeps the two-column grid, slide-over tree and bottom bulk bar usable.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/media`, `/media/duplicates`, `/media/trash`, `/media/settings`, one deep folder link and one file detail page (desktop) plus `/media`, `/media/settings` (mobile). The script must create a folder, upload a duplicate pair and an image, replace the image to create version 2, restore version 1, open each preview type it uploaded, edit alt text and add a tag, run a metadata filter, open the bulk bar from a two-file selection and cancel it, create and revoke a share link, delete then restore a file, open the duplicates report and merge the pair, and in Settings run `Test connection` (expected to succeed against the QA object store) and submit an out-of-range quality value to see the field error.

What the visual check should see: a populated tree, real thumbnails with no broken placeholders, a readable scan badge on every row, a versions list with two entries after the replace, a preview pane rendering the actual file rather than a blank frame, a labelled settings form with a visible connection result, and a genuine empty state when the duplicates report finds nothing — screenshots `page-media-browser`, `page-media-file-detail`, `page-media-duplicates`, `page-media-settings`, `mobile-media-browser`.

### Slices

1. **Folders + browser + trash.** Migration (folders, `media` extensions), folder CRUD, list/grid browser with filters and selection, bulk actions, trash with restore and purge, permission keys, tests. Done when a file can be uploaded, moved between folders, deleted and restored, and QA photographs both a populated browser and a real empty folder.
2. **Preview + metadata + versions.** Preview pipeline for image, video, audio, PDF and text; metadata and tag editing with EXIF/dimension extraction; version history with replace, restore and download; usage and activity tabs. Done when a replaced file shows two versions, an older version restores as the newest, and every preview type renders in the QA pass.
3. **Transformations + storage settings + shares + duplicates.** Preset CRUD with cached results, storage settings with a connection test and public base URL, the purge hook to REQ-011, share links, duplicate detection with merge. Done when a preset URL returns the transformed image, a share link works until revoked, and a merged group keeps one file with repointed references.
4. **Permissions + scanning + retention.** Folder and file grants with inheritance, the IAM subject picker, share gating, the scanning pipeline with quarantine and release, retention policies with the daily worker and run log, and reference-based purge refusal plus the repair scan. Done when a denied subject is refused on the raw route, a flagged upload is quarantined and releasable, and a retention run removes exactly the eligible rows.

### Risks / notes

- **Migration number** is “next free slot at build time”; the `media` alterations must stay nullable so existing rows keep working without a stop-the-world backfill.
- **Storage keys are immutable:** a move changes the folder reference, never the key, and a replace writes a new key — anything else breaks caches and signed URLs.
- **Transformation is on demand with a cache**, never a pre-generated matrix (presets × files would explode); cache keys include a hash of the preset definition.
- **Public delivery and privacy:** private files only through signed URLs or a share token, the public base URL is per site, and the settings screen states bucket visibility explicitly so a world-readable bucket cannot happen by accident.
- **Scanning is best-effort at ingest, strict at serve:** a scanner outage must not lose uploads, but an unscanned file must not be publicly readable until it clears.
- **Retention is destructive:** every rule change and run is audited, purge is soft-first (trash) then hard after the window, and legal hold wins over all other rules.
- **Reference bookkeeping is the weakest link** — a repair scan exists from slice 4 and is runnable from Settings, so “used in” can be trusted and purge refusals are not spurious; streamed `.zip` downloads of large selections need backpressure and a selection cap.
