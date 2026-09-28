/**
 * Shapes the Omnion API answers with (`apps/api`, `/api/v1` — docs/02-ARCHITECTURE.md).
 *
 * They mirror the API's response bodies one to one; the API owns the field names, this file
 * only gives the admin panel types for them.
 */

/** One account (`GET /api/v1/me`). */
export type User = {
  id: string;
  organization_id: string | null;
  email: string;
  display_name: string;
  status: string;
  created_at: string;
};

/** One tenant (`GET /api/v1/organizations`). */
export type Organization = {
  id: string;
  name: string;
  slug: string;
  status: string;
  created_at: string;
  updated_at: string;
};

/** One site inside a tenant (`GET /api/v1/sites`). */
export type Site = {
  id: string;
  organization_id: string;
  key: string;
  name: string;
  status: string;
  /** Theme the renderer activates for this site (`themes/<key>`). */
  theme: string;
  created_at: string;
  updated_at: string;
};

/** One revision of a page (`GET /api/v1/pages/{id}`). */
export type Revision = {
  id: string;
  page_id: string;
  revision_no: number;
  state: string;
  title: string;
  body: string;
  summary: string | null;
  /** The page's block tree as stored JSON (REQ-063); `[]` renders the body instead. */
  blocks: unknown[];
  restored_from_id: string | null;
  created_at: string;
  published_at: string | null;
};

/**
 * One block of a page (REQ-063).
 *
 * The platform's `@omnion/types` package is the single source of truth for these shapes and
 * the renderer imports it directly; the panel re-exports the two it needs so its own screens
 * have one import and the shapes can never drift between the two apps.
 */
export type {
  BlockDefinition,
  BlockIssue,
  BlockPropSchema,
  BlockRegistry,
  BlockValidationResult,
  ContentBlock,
  ContentPattern,
  PageTemplateSummary,
  PatternBlocksResponse,
  PatternListResponse,
  TemplateListResponse,
} from "@omnion/types";

/**
 * `BlockIssue` again, as a *binding* rather than a re-export.
 *
 * `export type { X } from "…"` places nothing in this module's scope, so a type that is also
 * used inside a declaration here has to be imported in its own right. Two lines, one import —
 * and the compiler says so the moment one of them goes missing.
 */
import type { BlockIssue } from "@omnion/types";

/** One prop-level difference inside a `changed` block row (REQ-063). */
export type PropChange = {
  /** Prop name (`text`, `alt`) or `meta.<setting>` for a per-block setting. */
  path: string;
  /** The inspector's own name for the prop, when the registry declares one. */
  label: string | null;
  /** Value before the change, elided past 160 characters. */
  before: string;
  /** Value after the change. */
  after: string;
  /** The prop is only in the new revision. */
  added: boolean;
  /** The prop is only in the old revision. */
  removed: boolean;
};

/** One row of a revision compare (`GET /api/v1/pages/{id}/revisions/{rev}/diff`). */
export type BlockDiffEntry = {
  /** The same value in both revisions when the block survived — this is what makes a move
   * readable instead of a rewrite. */
  block_id: string;
  block_type: string;
  change: "added" | "removed" | "changed" | "moved" | "unchanged";
  /** Headline naming the block in a word or two, so a row is not a JSON object. */
  label: string;
  /** Where the block sat before, as `0.children.1`. */
  from_path: string;
  /** Where it sits now; empty for a removal. */
  to_path: string;
  /** Prop-level detail, for a `changed` block. */
  props: PropChange[];
  /** How many blocks travelled with this one when it was removed. */
  removed_count?: number;
};

/** The whole block compare. */
export type BlockDiff = {
  entries: BlockDiffEntry[];
  added: number;
  removed: number;
  changed: number;
  moved: number;
  /** `true` when at least one block was deleted — the only row that needs an author to look. */
  has_removals: boolean;
};

/** One side of a compare, as a pointer rather than a whole revision. */
export type DiffRevisionRef = {
  id: string;
  revision_no: number;
  state: string;
  title: string;
  created_at: string;
};

/** Two revisions, compared. `body` covers a page that still renders from plain text. */
export type RevisionDiff = {
  page_id: string;
  base: DiffRevisionRef;
  compared: DiffRevisionRef;
  blocks: BlockDiff;
  body: { changed: boolean; before: string; after: string };
};

/** One page with its working draft and the revision visitors see (`GET /api/v1/pages`). */
export type Page = {
  id: string;
  site_id: string;
  slug: string;
  page_type: string;
  status: string;
  published_revision_id: string | null;
  draft: Revision | null;
  published: Revision | null;
  created_at: string;
  updated_at: string;
};

/** The title a page shows in a list: the newest wording the API has for it. */
export function pageTitle(page: Page): string {
  return page.draft?.title ?? page.published?.title ?? page.slug;
}

/**
 * The renderer-frame payload of a page's working draft
 * (`GET /api/v1/pages/{id}/preview?viewport=`).
 *
 * It carries BOTH trees. `blocks` is what is stored; `visible_blocks` is what the requested
 * viewport actually renders, after the server dropped the blocks the author hid from that
 * screen. A frame that only received the filtered tree could not tell a hidden block from a
 * deleted one — and "where did my block go" is the first question an author asks a phone
 * preview.
 */
export type PagePreview = {
  page_id: string;
  slug: string;
  title: string;
  viewport: "desktop" | "mobile";
  blocks: unknown[];
  visible_blocks: unknown[];
  block_count: number;
  visible_count: number;
  body: string;
  revision_id: string;
  revision_no: number;
  /** The revision visitors see, when there is one. */
  published_revision_no: number | null;
  can_publish: boolean;
  issues: BlockIssue[];
};

/** One file in a site's media library (`GET /api/v1/media`). */
export type Media = {
  id: string;
  site_id: string;
  filename: string;
  content_type: string;
  size_bytes: number;
  checksum: string;
  /** Panel read path of the bytes (`GET /api/v1/media/{id}/raw`). */
  raw_path: string;
  /** Public read path of the bytes (`GET /api/v1/public/media/{id}`). */
  public_path: string;
  created_at: string;
};

/** One folder of the media tree (`GET /api/v1/media/folders`). */
export type MediaFolder = {
  id: string;
  parent_id: string | null;
  name: string;
  /** Materialised path from the library root, e.g. `Media/Campaigns/2026`. */
  path: string;
  /** `true` for the library root, which cannot be renamed, moved or deleted. */
  is_root: boolean;
  /** Depth below the root; the root itself is 0. */
  depth: number;
  /** How many live files sit directly in this folder. */
  file_count: number;
  created_at: string;
};

/** The folder tree of one site. */
export type MediaFolderTree = {
  site_id: string;
  root: MediaFolder;
  folders: MediaFolder[];
};

/** One file as the browser reads it (`GET /api/v1/media/files`). */
export type MediaFile = Media & {
  folder_id: string | null;
  kind: string;
  alt_text: string;
  caption: string;
  description: string;
  metadata: Record<string, unknown>;
  tags: string[];
  width: number | null;
  height: number | null;
  duration_ms: number | null;
  page_count: number | null;
  scan_status: string;
  version_count: number;
  uploaded_by: string | null;
  updated_at: string | null;
};

/** One step of the breadcrumb above a folder. */
export type MediaCrumb = { id: string; name: string };

/** One page of the browser listing. */
export type MediaFilePage = {
  site_id: string;
  folder_id: string | null;
  breadcrumb: MediaCrumb[];
  files: MediaFile[];
  /** How many rows the filters match in total, not just on this page. */
  total: number;
  has_more: boolean;
};

/** The filters the browser listing understands; every one is optional. */
export type MediaFilters = {
  folder_id?: string | null;
  recursive?: boolean;
  search?: string;
  kind?: string;
  min_bytes?: number;
  max_bytes?: number;
  uploaded_by?: string;
  tag?: string;
  scan_status?: string;
  has_versions?: boolean;
  sort?: string;
  limit?: number;
  offset?: number;
};

/** One trashed file. */
export type MediaTrashEntry = MediaFile & {
  deleted_at: string;
  deleted_by: string | null;
  /** When the retention window purges it, if a policy applies. */
  purges_at: string | null;
};

/** The trash of one site. */
export type MediaTrash = {
  site_id: string;
  retention_days: number;
  file_count: number;
  total_bytes: number;
  entries: MediaTrashEntry[];
};

/** The answer to a bulk action: what changed and what did not. */
export type MediaBulkResult = {
  requested: number;
  changed: number;
  failures: { id: string; message: string }[];
};

/** One theme this installation bundles (`GET /api/v1/onboarding`). */
export type BundledTheme = {
  key: string;
  name: string;
  description: string;
};

/** Per-step progress of the first run. */
export type OnboardingSteps = {
  owner: boolean;
  organization: boolean;
  site: boolean;
  theme: boolean;
  ai: boolean;
};

/** What the first run created so far. */
export type OnboardingSummary = {
  organization_name: string | null;
  site_name: string | null;
  site_theme: string | null;
};

/** One getting-started item of the dashboard checklist. */
export type ChecklistItem = {
  key: string;
  label: string;
  description: string;
  href: string;
  done: boolean;
};

/** The first-run picture (`GET /api/v1/onboarding`). */
export type OnboardingStatus = {
  /** `true` while the installation has no accounts at all. */
  needs_setup: boolean;
  /** `true` when accounts exist but the first run was never closed. */
  in_progress: boolean;
  /** `true` once the first run is closed. */
  completed: boolean;
  steps: OnboardingSteps;
  summary: OnboardingSummary;
  checklist: ChecklistItem[];
  themes: BundledTheme[];
};

/** Answer of `POST /api/v1/onboarding/owner`. */
export type OwnerSetupResult = {
  user: User;
  onboarding: OnboardingStatus;
};
