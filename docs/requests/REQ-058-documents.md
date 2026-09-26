# REQ-058 — Documents & Knowledge

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/documents`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Internal documents and the knowledge base.

- **Folders & documents** with upload, versioning (immutable versions + restore), locking, sharing (internal link / public token).
- **Knowledge base**: wiki-style pages with hierarchical tree, markdown editor, backlinks, "related", full-text search.
- **Templates**: document/page templates with variables (customer name, date).
- **Search**: unified with the global search engine (REQ-002).
- **Permissions**: inheritable folder permissions with per-role grants.
- **Events**: `documents.document.updated`, `documents.page.published`.

## Implementation spec

### Scope (in / out)

**In**
- Folder tree with inherited per-role grants (`documents.read` / `documents.write` / `documents.manage`) plus per-folder overrides, and a "who can see this" explainer on every node.
- Document upload (multi-file, drag & drop), immutable versions, restore, version notes, metadata compare between two versions.
- Check-out locking with expiry: a locked document refuses new versions until released or the lock times out.
- Sharing: internal deep links for colleagues and public token links (opaque token, optional expiry, revocable, read-only).
- Knowledge base: spaces, hierarchical page tree, markdown editor with live preview, `[[wiki-links]]`, backlinks, related pages, draft → published revisions, full-text search.
- Templates for documents and KB pages with variables (`{{customer_name}}`, `{{today}}`, `{{author}}`) rendered at creation time.
- Search: one indexer contract feeds the global search engine (REQ-002); a `document_search` view keeps the module useful before REQ-002 lands.
- Audit entries for every mutation via `omnion-audit`; events through the existing event bus and webhook deliveries.

**Out**
- Real-time co-editing (presence, CRDT) — later phase.
- OCR, auto-classification, redaction.
- E-signature (REQ-030), retention/legal hold (REQ-038), external DMS connectors.
- Any storage path outside object storage (`omnion-storage`); rows never carry local file paths.

### Screens (UI)

| Route | Screen |
|---|---|
| `/documents` | Document browser — folder tree (left rail) + document table |
| `/documents/<id>` | Document detail — preview, versions, sharing, permissions, activity |
| `/documents/templates` · `/documents/templates/<id>/edit` | Template list and template editor |
| `/documents/shared` | Shared with me / shared by me |
| `/kb` · `/kb/<page_id>` | Knowledge base space tree and read view |
| `/kb/<page_id>/edit` | Markdown editor (saves a draft revision; publish from here) |
| `/kb/search?q=` | Knowledge search results (same result card as global search) |

- **Document browser.** Toolbar: `New folder`, `Upload` (multi-file), `New from template`, search box, view toggle (table / compact). Table columns: Name, Type, Version, Owner, Updated, Size, Tags, Shared. Filters: folder (from tree), type, owner, tag, updated range, `shared externally`. Row click opens detail; checkbox column enables bulk actions — Move, Tag, Download (zip), Delete, Copy link. Folder tree: expand/collapse, inline rename, drag to move, right-click menu (New subfolder, Rename, Permissions, Delete). Empty state offers `Upload` and `New from template`; loading uses a skeleton table; a failed folder load keeps the tree and shows a retry strip.
- **Document detail.** Header: breadcrumb, name, status badges (`Locked`, `Shared`, `Template`), actions `Download`, `New version`, `Lock`/`Unlock`, `Share`, `Delete`. Tabs: Preview (pdf/image/text inline; unknown types offer download), Versions (v#, author, date, size, note, actions Download / Compare / Restore — restore opens a confirm dialog stating that history is never rewritten), Permissions (inherited grants greyed out with the inheriting folder named; overrides listed with an "Add grant" form: subject type user/group/role, subject, capability), Activity (audit trail), Comments (reuses the revision-comment model from `crates/content`).
- **Knowledge base.** Left: space selector, page tree with drag reorder, `+ New page`, `Import markdown`. Read view: rendered markdown, table of contents, right rail with Backlinks, Related, Page info (author, revision, published at). Editor: markdown textarea + live preview toggle, toolbar (bold, italic, heading, list, code, link, table), `[[` opens a page picker that inserts a wiki link, `Save draft` / `Publish` / `Discard`, dirty-state warning on navigation, `⌘S` save. Broken wiki links render dashed and are listed under Backlinks as "missing pages" with a one-click create.
- **Shared view.** Two tabs: Shared with me (Document, Owner, Shared at, Capability), Shared by me (Document, Link kind internal/public, Expires, Views, Revoke).
- **Keyboard.** `/` focuses search, `n` new folder, `u` upload, `↑/↓` move tree selection, `Enter` open, `o` open detail, `Esc` closes dialogs/panels. **Mobile.** Tree collapses to a folder breadcrumb picker; the table becomes a card list (Name, Updated, Owner, Size); detail tabs become segmented control; the editor stacks textarea above preview. Upload progress is a persistent bottom sheet on small screens.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/documents/folders` | Folder tree (`?parent_id=`, `?depth=`) | `documents.read` |
| POST | `/api/v1/documents/folders` | Create folder (name, parent, grants) | `documents.manage` |
| PATCH · DELETE | `/api/v1/documents/folders/{id}` | Rename / move / re-parent · delete (empty or recursive with confirm) | `documents.manage` |
| GET · POST | `/api/v1/documents` | List documents (`?folder_id=&type=&owner=&tag=&shared=&updated_from=`) · create (title, folder, tags) | `documents.read` · `documents.write` |
| GET · PATCH · DELETE | `/api/v1/documents/{id}` | Read · rename/move/tag · delete (soft: latest version kept for restore) | `documents.read` · `documents.write` · `documents.manage` |
| POST | `/api/v1/documents/{id}/versions` | Upload a new version (multipart, optional note) | `documents.write` |
| GET | `/api/v1/documents/{id}/versions` · `/versions/{no}` | Version list · one version (metadata + `?download=1`) | `documents.read` |
| POST | `/api/v1/documents/{id}/versions/{no}/restore` | Copy an old version forward as the new head | `documents.write` |
| POST · DELETE | `/api/v1/documents/{id}/lock` | Check out · release | `documents.write` |
| GET · POST | `/api/v1/documents/{id}/shares` | List shares · create (kind, expires_at) | `documents.manage` |
| DELETE | `/api/v1/documents/{id}/shares/{share_id}` | Revoke a share | `documents.manage` |
| GET | `/api/v1/public/documents/{token}` | Public read of one shared document (unauthenticated, token only) | — |
| GET · POST | `/api/v1/documents/templates` | Template list · create/update template | `documents.read` · `documents.manage` |
| POST | `/api/v1/documents/from-template` | Create a document from a template with variables resolved | `documents.write` |
| GET · POST | `/api/v1/kb/pages` | Page tree (`?space=`) · create page | `documents.read` · `documents.write` |
| GET · PATCH | `/api/v1/kb/pages/{id}` | Read page (rendered + raw) · update draft revision | `documents.read` · `documents.write` |
| POST | `/api/v1/kb/pages/{id}/publish` | Publish the working draft | `documents.write` |
| GET | `/api/v1/kb/pages/{id}/backlinks` · `/related` | Backlinks · related pages | `documents.read` |
| GET | `/api/v1/search?q=&types=document,kb_page` | Unified search entry point (REQ-002 contract) | `documents.read` |

Cached by default: folder tree output and the permission explainer are computed per request and never cached across scopes.

### Data model

Migrations: `0100_documents.sql`, `0101_documents_sharing.sql`, `0102_knowledge.sql` (reserved band 0100–0115 for this REQ; the ledger is append-only — take the next free number if one is already used).

```sql
-- 0100_documents.sql
document_folders (id uuid pk, organization_id uuid not null -> organizations,
  parent_id uuid null -> document_folders, name text not null, path text not null,
  created_by uuid null -> users, created_at/updated_at timestamptz not null default now())
  unique (organization_id, path); index (organization_id, parent_id)
document_folder_grants (id uuid pk, folder_id uuid not null -> document_folders,
  subject_kind text in ('user','group','role'), subject_id uuid not null,
  capability text in ('read','write','manage'), created_at timestamptz, created_by uuid)
  unique (folder_id, subject_kind, subject_id)
documents (id uuid pk, organization_id uuid not null, folder_id uuid not null -> document_folders,
  title text not null check (length(btrim(title)) > 0), doc_type text not null default 'file',
  tags text[] not null default '{}', locked_by uuid null -> users, locked_at timestamptz,
  lock_expires_at timestamptz, template_id uuid null, created_by uuid null, deleted_at timestamptz,
  created_at/updated_at timestamptz)
  index (organization_id, folder_id, updated_at desc); index (tags) using gin
document_versions (id uuid pk, document_id uuid not null -> documents on delete cascade,
  version_no integer not null check (version_no >= 1), storage_key text not null unique,
  filename text not null, content_type text not null, size_bytes bigint not null check (> 0),
  checksum text not null, note text, restored_from_id uuid null -> document_versions,
  created_by uuid null, created_at timestamptz not null default now())
  unique (document_id, version_no); index (document_id, created_at desc)
-- 0101_documents_sharing.sql
document_shares (id uuid pk, document_id uuid not null -> documents on delete cascade,
  token_hash text not null unique, created_by uuid null, expires_at timestamptz,
  revoked_at timestamptz, last_accessed_at timestamptz, access_count integer not null default 0,
  created_at timestamptz)  -- the raw token is never stored, only its hash
-- 0102_knowledge.sql
kb_pages (id uuid pk, organization_id uuid not null, space text not null, parent_id uuid null,
  position integer not null default 0, slug text not null, title text not null, status text,
  published_revision_id uuid null, created_by uuid null, created_at/updated_at timestamptz)
  unique (organization_id, space, slug); index (organization_id, space, position)
kb_revisions (id uuid pk, kb_page_id uuid not null on delete cascade, revision_no integer not null,
  state text in ('draft','published','archived'), title text not null, body_markdown text not null,
  restored_from_id uuid null, created_by uuid null, created_at, published_at timestamptz)
  unique (kb_page_id, revision_no); partial unique (kb_page_id) where state = 'draft'
kb_links (from_page_id uuid, to_page_id uuid null, to_slug text not null, created_at)
  index (to_page_id); index (from_page_id)
document_templates (id uuid pk, organization_id uuid, key text, name text not null, kind text,
  body text not null, variables jsonb not null default '[]', created_by/at/updated_at)
```

### Events

| Event | When | Payload sketch |
|---|---|---|
| `documents.document.updated` | Metadata change or new version | `document_id`, `folder_id`, `version_no`, `actor_user_id`, `changed` |
| `documents.document.shared` | Share created or revoked | `document_id`, `share_id`, `kind`, `expires_at` |
| `documents.document.locked` | Check-out / release | `document_id`, `locked_by`, `expires_at` |
| `documents.folder.created` | Folder created | `folder_id`, `parent_id` |
| `documents.page.published` | KB page published | `page_id`, `space`, `slug`, `revision_no` |

Consumed: `content.page.deleted` (unlink KB references), `media.deleted` (mark broken attachments). All events ride the existing bus, so any subscriber receives a signed webhook delivery with redelivery from the events screen; payloads carry ids and never file contents or tokens.

### Acceptance criteria

- [ ] A folder tree renders a 3-level hierarchy, supports inline rename and drag-to-move, and keeps selection on the tree while the document table filters.
- [ ] Uploading three files at once creates three documents, each with version 1 and a recorded checksum and size.
- [ ] Adding a second version leaves version 1 byte-identical on download (immutability proven by checksum), and Restore copies the old version forward as a new head version with `restored_from_id` set.
- [ ] Compare between two versions shows a text diff for text types and a metadata diff for binary types.
- [ ] Locking a document blocks a second upload with a clear "locked by …" error; the lock expires after the configured minutes and the document accepts versions again.
- [ ] A public share link opens the document in a signed-out browser, expires at its `expires_at`, and stops working after Revoke.
- [ ] The share list shows view counts and last access; no raw token is retrievable after creation, only at creation.
- [ ] Inherited grants: a grant on a parent folder gives read on children; an override on a child wins over the parent and is labelled "override" in the UI.
- [ ] A user without any grant sees only folders shared with them, and a 404 (not 403) for a document outside their scope.
- [ ] KB pages save as draft revisions and publish; a second edit after publish creates revision 2 without touching revision 1.
- [ ] `[[wiki-links]]` create backlinks on the target page; a link to a non-existent slug appears as "missing page" and creating it links the pair.
- [ ] Related pages shows at least three neighbours for a page with inbound links, ordered by link count.
- [ ] Document creation from a template resolves `{{customer_name}}`, `{{today}}` and `{{author}}` and leaves unknown variables untouched and highlighted.
- [ ] Search returns documents and KB pages for a query, with type filters, inside 300 ms on 10k documents.
- [ ] Every mutation writes one audit row naming actor, action and object; the Activity tab renders those rows without a separate data source.
- [ ] `documents.document.updated` and `documents.page.published` are delivered to a subscribed test endpoint and appear in the events screen with redeliver available.

### QA plan

Walkthrough (`scripts/qa/run.sh`) must reach and click: sidebar `Documents`; `/documents` toolbar buttons (`New folder`, `Upload`, `New from template`), folder tree expand/collapse/rename, table sort, every filter select, bulk-action bar after selecting a row; `/documents/<id>` tabs Preview → Versions → Permissions → Activity → Comments, `New version`, `Lock`/`Unlock`, `Share`, Restore confirm dialog, and the folder permission form; `/documents/templates` and one template editor including the variable palette; `/kb` space selector, page tree, `+ New page`; `/kb/<page_id>/edit` with `⌘S`, Save draft, Publish, the `[[` picker; `/kb/search?q=leave`; `/documents/shared` both tabs. Vision check: the document browser shows a real folder tree, a populated table with status badges and no clipped columns at 1440×900 and 390×844; the KB read view shows rendered markdown, a table of contents and a populated backlinks rail; upload progress and empty states render; no placeholder text ("coming soon", "lorem") anywhere.

### Slices

1. **Folders, documents, versions, locks.** Migration `0100_documents.sql`; folder CRUD + tree screen, document CRUD, version upload/list/restore/compare, check-out lock with expiry, audit rows. *Done when:* acceptance items 1–5 pass and `/documents` plus `/documents/<id>` are in the walkthrough inventory.
2. **Sharing and inherited permissions.** Migration `0101_documents_sharing.sql`; share create/revoke/list with counts, public token read route, folder grants with inheritance and overrides, permission explainer. *Done when:* items 6–9 pass and a signed-out browser opens a live share link in the QA pass.
3. **Knowledge base.** Migration `0102_knowledge.sql`; spaces, page tree, markdown editor with live preview, draft/publish revisions, wiki links, backlinks, related pages, `/kb` screens. *Done when:* items 10–12 pass.
4. **Templates, search, events.** Template CRUD + variable rendering + `/documents/from-template`; indexer contract and `document_search` view wired to the search route; emit the five events and verify a webhook delivery. *Done when:* items 13–16 pass.

### Risks / notes

- Token sharing is the sharp edge: store only a hash, never log the raw token, and rate-limit the public read route per token and per IP.
- Permission inheritance must resolve once per request and be covered by tests for deep trees (5+ levels) — recursive SQL is easy to get wrong and easy to leak through.
- Version immutability depends on the storage layer never overwriting a key; new versions always write a new key, and the delete path is soft.
- Large uploads push through the API; keep the existing body limit, stream to object storage, and never buffer whole files in the panel.
- The `documents.pages.*` naming must not clash with the CMS: knowledge pages are `kb_*` tables and `documents.page.*` events only.
- Turkish example copy (template bodies, folder names in the seeded sample space) stays in user-facing seed data, not in code or comments.
