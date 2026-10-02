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

// ---------------------------------------------------------------------------------------------
// Featured media (REQ-064, slice 4d)
// ---------------------------------------------------------------------------------------------

/**
 * Whether a page's picture can be drawn.
 *
 * The FOUR states rather than a boolean, because the panel's words and the renderer's markup are
 * different for each: `none` is a page that never had a hero, `trashed` is a page that lost one
 * and says so, and `missing` is a column pointing at a row that is gone. A boolean collapses the
 * last three into "no image", which is exactly the message that sends an operator looking in the
 * wrong place after somebody emptied the trash.
 */
export type FeaturedAvailability = "none" | "available" | "trashed" | "missing";

/** A page's featured image, with the file's state folded in. */
export type FeaturedMedia = {
  page_id: string;
  site_id: string;
  slug: string;
  media_id: string | null;
  /** Alt text for THIS page's use — not the file's own `alt_text`. */
  alt: string;
  legend: string;
  /** 0–1 fraction, `null` when the page has never been cropped. */
  focal_x: number | null;
  focal_y: number | null;
  updated_at: string;
  storage_key: string | null;
  filename: string | null;
  content_type: string | null;
  width: number | null;
  height: number | null;
  deleted_at: string | null;
};

/**
 * Exactly what a renderer will draw, and nothing else.
 *
 * A separate type from `FeaturedMedia` so the panel cannot reach the storage key and build a URL
 * by hand: a hand-built URL is one base-path setting away from an image that 404s on every page
 * of the site. A `null` `render` here is a `null` on the public payload, so the preview cannot be
 * greener than what a visitor gets.
 */
export type FeaturedImage = {
  media_id: string;
  url: string;
  alt: string;
  legend: string;
  object_position: string | null;
  width: number | null;
  height: number | null;
};

/** One row of the picker's list. */
export type FeaturedCandidate = {
  id: string;
  filename: string;
  content_type: string;
  storage_key: string;
  width: number | null;
  height: number | null;
  /**
   * The file's OWN alt text, offered as a starting point.
   *
   * Never written for the page without the editor pressing the button that copies it: the same
   * file is the hero of several pages with different descriptions, and a store that copied it
   * would rename the image everywhere on the first save.
   */
  alt_text: string;
  /** How many pages already feature this file — the "reuse" signal, made visible. */
  used_by_pages: number;
};

/** `GET`/`PUT /api/v1/pages/{id}/featured-media`. */
export type FeaturedMediaBody = {
  media: FeaturedMedia;
  /** The chip's words, from the server — the panel never invents a state name. */
  availability_label: string;
  /** The degradation sentence, `null` when there is nothing wrong. */
  warning: string | null;
  render: FeaturedImage | null;
};

/** A change to one page's featured image. */
export type FeaturedChanges = {
  media_id?: string | null;
  alt?: string | null;
  legend?: string | null;
  /**
   * `undefined` leaves the crop alone; `null` CLEARS it; a number sets it.
   *
   * Three states, and the difference between the first two is the whole reason the API reads this
   * payload by hand — serde cannot tell a missing key from a JSON null, so a plain optional field
   * would make the *Clear crop* button a silent no-op.
   */
  focal_x?: number | null;
  focal_y?: number | null;
  clear?: boolean;
};

/** `GET /api/v1/sites/{site_id}/featured-media/candidates`. */
export type FeaturedCandidatesBody = {
  candidates: FeaturedCandidate[];
  limit: number;
};

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
  /** The files this page's blocks name, and what can be done with each (REQ-063, slice 4). */
  media: BlockMediaReport;
  /** Files the tree names. */
  media_file_count: number;
  /** How many of them cannot be served. */
  media_broken_count: number;
  /** The server's one-line summary, or `""` when nothing is broken. */
  media_warning: string;
  /** Ids this frame is *pretending* are deleted, and name a file on this page. */
  simulated_media: number;
};

/**
 * One block's relationship with one file.
 *
 * Mirrors the server's `BlockMediaRef` rather than importing it, because the panel's own feature
 * exports already bind server types by hand and `@omnion/types` does not carry the media report
 * (it is a *content* report, not a block-registry document). The duplication is deliberate and
 * narrow: a field added here without one on the server is a `undefined` in the panel, and the
 * panel renders every field it is given.
 */
export type BlockMediaRef = {
  /** Block that names the file. */
  block_id: string;
  /** The block's type (`image`, `gallery`). */
  block_type: string;
  /** Prop the file came from (`[0].props.src`). */
  path: string;
  /** The media id, as stored. */
  media_id: string;
  /** `live`, `trashed` or `purged` — see the server's `FileState`. */
  state: "live" | "trashed" | "purged";
  /** The viewport this block draws on, as `hide_on` names it. */
  visible_on: "none" | "mobile" | "desktop";
  /** A caption the block degrades to, when the file is gone. */
  caption: string | null;
  /** What to do about it, in the author's words. */
  advice: string;
};

/** Every file a page's blocks name, and what can be done with it. */
export type BlockMediaReport = {
  /** One entry per block/prop pair that names a file, in tree order. */
  refs: BlockMediaRef[];
  /** Distinct files the tree names. */
  file_count: number;
  /** Distinct files that cannot be served. */
  broken_count: number;
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

/** One transformation preset (`GET /api/v1/media/transformation-presets`). */
export type MediaPreset = {
  id: string;
  name: string;
  width: number | null;
  height: number | null;
  /** `cover` | `contain` | `fill`. */
  fit: string;
  /** `webp` | `jpeg` | `png`. */
  format: string;
  quality: number;
  /** What the settings screen reads: `1200 x 630 · cover · WebP q80`. */
  summary: string;
  /** The query a page appends, e.g. `?preset=card`. */
  example_query: string;
};

/**
 * A site's storage settings (`GET /api/v1/media/settings`).
 *
 * There is no credential field here and there is never going to be one: the API stores a
 * *reference* to the key the deployment already holds, so a settings screen that could render a
 * secret is a screen a future change has to be trusted not to make.
 */
export type MediaStorageSettings = {
  driver: string;
  endpoint: string;
  region: string;
  bucket: string;
  path_prefix: string;
  public_base_url: string;
  /** Where a public file will actually be served from, in words. */
  public_base_summary: string;
  signed_url_ttl_seconds: number;
  default_visibility: string;
  max_upload_mb: number;
  allowed_content_types: string[];
  /** Whether the row has ever been written since it was created. */
  configured: boolean;
  /** The privacy consequence of `default_visibility`, spelled out. */
  visibility_note: string;
};

/** What a save or a connection test sends. Every field is optional and folded onto the row. */
export type MediaStorageSettingsInput = Partial<
  Omit<MediaStorageSettings, "public_base_summary" | "configured" | "visibility_note">
>;

/**
 * A site's virus-scanning policy (`GET /api/v1/media/scan-settings`).
 *
 * There is no field that could hold a secret: the row stores the *name* of the environment
 * variable the deployment keeps the scanner's shared secret under, and `secret_available`
 * says whether this process can actually see it. A screen that could render a credential is a
 * screen a future change has to be trusted not to make.
 */
export type MediaScanSettings = {
  enabled: boolean;
  endpoint: string;
  /** The *name* of the environment variable holding the shared secret — never the secret. */
  secret_env: string;
  timeout_seconds: number;
  /** `hold` refuses to serve a file whose scan could not complete; `serve` serves it anyway. */
  on_error: string;
  max_scan_mb: number;
  /** Whether this process can see `secret_env` in its own environment. */
  secret_available: boolean;
  /** What the current policy does to a file, in a sentence. */
  behaviour: string;
  /** How many files are still waiting for their first scan. */
  pending_count: number;
  configured: boolean;
};

/** What a save sends. Every field is optional and folded onto the stored row. */
export type MediaScanSettingsInput = Partial<
  Omit<MediaScanSettings, "secret_available" | "behaviour" | "pending_count" | "configured">
>;

// ---------------------------------------------------------------------------------------------
// Retention (REQ-010, slice 4)
// ---------------------------------------------------------------------------------------------

/** One retention policy. */
export type MediaRetentionPolicy = {
  id: string;
  site_id: string;
  name: string;
  /** The folder it governs; `null` means the whole site. */
  folder_id: string | null;
  /** The folder's path, so the screen names a folder rather than printing a uuid. */
  folder_path: string;
  keep_versions_days: number;
  trash_days: number;
  purge_after_days: number;
  legal_hold: boolean;
  enabled: boolean;
  /** What the policy does to a file, in one sentence. */
  behaviour: string;
  /** What the policy is scoped to, in words. */
  scope: string;
};

/** The policy list of a site, with the numbers the screen cannot compute on its own. */
export type MediaRetentionList = {
  site_id: string;
  policies: MediaRetentionPolicy[];
  /** Trashed files whose restore window has closed. */
  past_restore: number;
  /** The bytes those files occupy. */
  past_restore_bytes: number;
  last_run: MediaRetentionRun | null;
  /** One sentence about the site-wide rule. */
  summary: string;
};

/** A policy as the editor sends it. Every field is optional so a save sends only what changed. */
export type MediaRetentionPolicyInput = {
  name?: string;
  /**
   * `undefined` leaves the scope alone; `null` widens the policy to the whole site.
   *
   * The distinction is load-bearing: the API reads the scope from the **key's presence**, so a
   * form that sends `folder_id: undefined` (which `JSON.stringify` drops) edits the other two
   * windows and leaves the scope exactly as it was.
   */
  folder_id?: string | null;
  keep_versions_days?: number;
  trash_days?: number;
  purge_after_days?: number;
  legal_hold?: boolean;
  enabled?: boolean;
};

/** One row of the retention run log. */
export type MediaRetentionRun = {
  id: string;
  kind: string;
  versions_removed: number;
  versions_bytes: number;
  purged: number;
  purged_bytes: number;
  /** Files the sweep would not purge because something still points at them. */
  refused: number;
  /** Files the legal hold removed from the eligible set. */
  held_back: number;
  /** What stopped the run; empty when nothing did. */
  error: string;
  started_at: string;
  finished_at: string | null;
  /** One sentence — a number without a word is not an answer an operator can act on. */
  summary: string;
  bytes_reclaimed: number;
};

/** The run log of a site, newest first. */
export type MediaRetentionRunList = {
  site_id: string;
  runs: MediaRetentionRun[];
};

/** One "this file is still referenced" line, naming the record that holds it. */
export type MediaPurgeRefusal = {
  media_id: string;
  filename: string;
  resource_kind: string;
  resource_id: string;
  field: string;
  describe: string;
};

/** The answer to "run retention now". */
export type MediaRetentionRunResult = {
  run_id: string;
  run: MediaRetentionRun;
  remaining_files: number;
  remaining_bytes: number;
  refused: MediaPurgeRefusal[];
};

/** The answer to "repair the stale references". */
export type MediaRetentionRepair = {
  site_id: string;
  references_removed: number;
  summary: string;
};

/** One held file (`GET /api/v1/media/quarantine`). */
export type MediaQuarantineEntry = {
  id: string;
  media_id: string;
  /** What the scanner said, in its own words. */
  detail: string;
  quarantined_at: string;
  run_id: string | null;
};

/** The quarantine list of a site, with its totals. */
export type MediaQuarantineList = {
  site_id: string;
  file_count: number;
  /** How many bytes the held files occupy — a held icon and a held video differ in urgency. */
  total_bytes: number;
  entries: MediaQuarantineEntry[];
};

/** One scanning pass (`GET /api/v1/media/scan/runs`). */
export type MediaScanRun = {
  id: string;
  kind: string;
  outcome: string;
  scanned: number;
  flagged: number;
  errors: number;
  skipped: number;
  endpoint: string;
  engine: string;
  started_at: string;
  finished_at: string | null;
  /** One sentence, never a number without a word. */
  summary: string;
};

/** The run log of a site. */
export type MediaScanRunList = {
  site_id: string;
  runs: MediaScanRun[];
};

/** The answer to "run the sweep now" (`POST /api/v1/media/scan/run`). */
export type MediaSweepResult = {
  run_id: string;
  scanned: number;
  flagged: number;
  errors: number;
  skipped: number;
  outcome: string;
  summary: string;
  quarantine: MediaQuarantineList;
};

/** The answer to a scanner probe (`POST /api/v1/media/scan/test`). */
export type MediaScanProbe = {
  ok: boolean;
  status: string;
  detail: string;
  engine: string;
  note: string;
};

/** The answer to a connection test (`POST /api/v1/media/settings/test-connection`). */
export type MediaStorageProbe = {
  ok: boolean;
  detail: string;
  driver: string;
  target: string;
  public_base_url: string;
  elapsed_ms: number;
};

/**
 * One camera record, as the platform stored it.
 *
 * Every field is optional because the camera said what it said: a phone writes no aperture, a
 * studio body writes no software, and a screenshot writes nothing at all. A missing key means
 * "not recorded" and the panel says so rather than printing a zero.
 */
export type MediaExif = {
  make?: string;
  model?: string;
  lens?: string;
  software?: string;
  /** RFC 3339 without a zone — the EXIF format carries no offset. */
  captured_at?: string;
  iso?: number;
  /** Shutter time in milliseconds, so `1/200 s` is 5. */
  exposure_ms?: number;
  /** Aperture in hundredths of an f-stop, so f/1.8 is 180. */
  aperture_x100?: number;
  focal_length_mm?: number;
  /** 1–8. Values 5–8 mean the pixels are stored sideways. */
  orientation?: number;
  /** The file carried a location. The coordinates are deliberately not kept. */
  gps?: boolean;
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
  /**
   * What the file's own EXIF block said, when its format carries one (REQ-010, slice 3).
   *
   * A structured object rather than a pre-formatted line, so the panel renders a row per field
   * and so a key this release does not know is carried through instead of dropped. A GPS fix is
   * `gps: true` and nothing more — the platform never stores the coordinates.
   */
  exif?: MediaExif | null;
  /** The size to reserve on screen, with the stored rotation applied. */
  display_width: number | null;
  /** The height on screen. */
  display_height: number | null;
  scan_status: string;
  version_count: number;
  /**
   * Whether a legal hold is on the file (REQ-010, slice 4).
   *
   * `false` rather than `absent` for a server that predates the column: the detail screen's
   * hold switch reads this to decide which button to draw, and `undefined` would have to be
   * treated as "not held" anyway — so the type says so rather than leaving it to a truthiness
   * check that happens to work.
   */
  legal_hold: boolean;
  uploaded_by: string | null;
  updated_at: string | null;
};

/** One version of a file (`GET /api/v1/media/{id}/versions`). */
export type MediaVersion = {
  id: string;
  version: number;
  size_bytes: number;
  checksum: string;
  content_type: string;
  width: number | null;
  height: number | null;
  note: string;
  created_by: string | null;
  created_at: string;
  /** Panel read path of *this* version's bytes. */
  raw_path: string;
  /** Whether this is the version the file currently serves. */
  is_current: boolean;
};

/** The history of one file, newest first. */
export type MediaVersionList = {
  media_id: string;
  current_version: number;
  /** Counted from the history, not read off the row's counter. */
  version_total: number;
  versions: MediaVersion[];
};

/**
 * One place a file is used (REQ-010, slice 4).
 *
 * `resolved` is the field the whole tab exists for. A reference whose referent the platform
 * cannot see is a row that will refuse a purge for ever, and the reader has to be able to see
 * *which* row is stale before deciding between repointing the record and running the repair
 * scan — so it travels on every row, not only as a total.
 */
export type MediaUsageEntry = {
  id: string;
  /** What kind of record points here (`page`, `theme`, …). */
  resource_kind: string;
  /** The referent's id, as text — it may be a slug, so it is never parsed in the panel either. */
  resource_id: string;
  /** Which field of that record points here; empty when the record *is* the file. */
  field: string;
  /** A name for the referent, when the platform can resolve one. */
  label: string | null;
  /** The referent's lifecycle state, for a page. */
  status: string | null;
  /** Where the referent lives in the panel, when it can be resolved. */
  path: string | null;
  /** Whether the platform can still see the referent. */
  resolved: boolean;
  created_at: string;
};

/**
 * One file's usage.
 *
 * `records` and `rows` disagree in ordinary use — a page naming the same hero in three fields is
 * one record and three rows — and "used in 3 places" beside one page is the number that makes
 * somebody delete a page. `summary` is the API's sentence rather than something the panel builds
 * out of two integers, so the screen and the API cannot disagree about what the numbers mean.
 */
export type MediaUsage = {
  media_id: string;
  /** Distinct records naming this file. */
  records: number;
  /** Of those, the ones the platform can still see. */
  resolved: number;
  /** Reference rows. */
  rows: number;
  /** Whether the read was cut short by the screen's bound. */
  truncated: boolean;
  /** One sentence saying what the numbers mean. */
  summary: string;
  usage: MediaUsageEntry[];
};

/** One audited action against a file. */
export type MediaActivityEntry = {
  id: number;
  /** The stable action name (`media.deleted`, …) — never rendered raw. */
  action: string;
  /** The same action as a sentence, so the panel never builds a label out of a verb. */
  summary: string;
  /** Who did it, when the account still exists. */
  actor: string | null;
  /** `user`, `agent`, `service` or `system`. */
  actor_type: string;
  metadata: unknown;
  occurred_at: string;
};

/** One file's activity, newest first. */
export type MediaActivity = {
  media_id: string;
  total: number;
  truncated: boolean;
  activity: MediaActivityEntry[];
};

/**
 * One share link, as the panel reads it (REQ-010, slice 3).
 *
 * There is no `token` field here, and that is not an oversight: the API cannot return one after
 * creation, because the row stores only its hash. A type that carried a token would be a lie
 * that shows up as an empty column in the table.
 */
export type MediaShare = {
  id: string;
  media_id: string;
  /** When the link stops working; `null` means "until revoked". */
  expires_at: string | null;
  /** Whether the link needs a password. The password itself is never returned. */
  has_password: boolean;
  /** Downloads that produced bytes. */
  download_count: number;
  created_at: string;
  /** When it was revoked; `null` while it is live. */
  revoked_at: string | null;
  revoked_reason: string;
  /** `live`, `expired` or `revoked`, resolved by the API against the clock. */
  state: "live" | "expired" | "revoked";
};

/**
 * A freshly created share, with the token shown once.
 *
 * `token` and `url` exist on this type and on no other — which is how the screen knows to put
 * the one-time copy panel on screen and to never try to show a link it cannot re-derive.
 */
/**
 * One grant on a folder or a file (docs/requests/REQ-010, slice 4).
 *
 * `subject_label` is `null` for a subject that has since been deleted. The panel prints
 * "Deleted subject" rather than a raw uuid: a stale row refuses nobody and grants nobody, and
 * the only action that matters on it is removal — a line showing a uuid teaches nobody which
 * grant to remove.
 */
export type MediaGrant = {
  id: string;
  subject_kind: "user" | "group" | "role";
  subject_id: string;
  subject_label: string | null;
  can_read: boolean;
  can_write: boolean;
  can_delete: boolean;
  can_share: boolean;
  effect: "allow" | "deny";
  created_by: string | null;
  created_at: string;
  /** The capability words, for a summary line. */
  capabilities: string[];
};

/** One folder on a file's chain, nearest first, with what it contributes. */
export type MediaGrantChainNode = {
  id: string;
  /** The folder's materialised path, which is already the breadcrumb. */
  path: string;
  grant_count: number;
  /** Whether it carries a deny that reaches this file's subject set. */
  has_deny: boolean;
};

/** What the permissions tab reads: the rows on one node, and the chain above a file. */
export type MediaGrantsResponse = {
  target_kind: "file" | "folder";
  target_id: string;
  grants: MediaGrant[];
  /** Whether a grant on this node reaches what is inside it. Always true for a folder. */
  inherits: boolean;
  chain: MediaGrantChainNode[];
};

/** What a grant is written with. Absent bits are `false`, never "unchanged". */
export type NewMediaGrant = {
  subject_kind: "user" | "group" | "role";
  subject_id: string;
  can_read?: boolean;
  can_write?: boolean;
  can_delete?: boolean;
  can_share?: boolean;
  effect?: "allow" | "deny";
};

/**
 * One subject the picker may offer.
 *
 * `suggested` marks a group: it is the row that survives somebody joining and leaving a team,
 * so a grant given to a person has to be rewritten when the person changes roles and a grant
 * given to a group does not.
 */
export type MediaGrantSubject = {
  id: string;
  kind: "user" | "group" | "role";
  label: string;
  /** An email, a member count, or a role key — the second line of the picker's row. */
  detail: string;
  suggested: boolean;
};

export type CreatedMediaShare = {
  share: MediaShare;
  url: string;
  token: string;
  notice: string;
};

/**
 * One row of the duplicate report (`GET /api/v1/media/duplicates`).
 *
 * `full_checksum` is what the merge posts; `checksum` is the short form for the eye and is
 * **not** a usable value. The two being different types' worth of fields rather than one
 * truncated one is what stops a client from sending the short one and being told, correctly,
 * that it is not a checksum.
 */
export type MediaDuplicateGroup = {
  site_id: string;
  /** Site name; present only where the caller asked for names. */
  site_name?: string;
  /** First sixteen characters, with an ellipsis. For reading, never for posting. */
  checksum: string;
  /** The full checksum — the value `Merge group` sends. */
  full_checksum: string;
  /** How many live files share it. */
  file_count: number;
  total_bytes: number;
  /** The group minus one keeper: what a purge of the copies returns, not the group's size. */
  reclaimable_bytes: number;
  first_seen: string;
  last_seen: string;
  /** Present only when the report was asked to expand. */
  files?: MediaDuplicateFile[];
};

/** One file inside a duplicate group. */
export type MediaDuplicateFile = {
  id: string;
  filename: string;
  folder_id: string | null;
  size_bytes: number;
  content_type: string;
  uploaded_at: string;
  /** How many *records* use it — a page naming it in two fields is one usage. */
  reference_count: number;
  raw_path: string;
};

/** The per-site report. Every group here has a keeper to pick and a merge button. */
export type MediaDuplicateReport = {
  site_ids: string[];
  cross_site: false;
  reclaimable_bytes: number;
  group_count: number;
  groups: MediaDuplicateGroup[];
};

/** One site's holding of an installation-wide duplicate. */
export type MediaCrossSiteCopy = {
  site_id: string;
  site_name: string;
  file_count: number;
  site_bytes: number;
};

/**
 * A checksum the whole installation holds more than once.
 *
 * No `reclaimable_bytes`, and that absence is the feature: nothing on this report can return
 * the space, because a merge repoints rows inside one site and cannot decide which tenant keeps
 * the file. The screen says where the copies are and sends the operator to the site that can act.
 */
export type MediaCrossSiteGroup = {
  checksum: string;
  full_checksum: string;
  file_count: number;
  site_count: number;
  total_bytes: number;
  first_seen: string;
  last_seen: string;
  sites: MediaCrossSiteCopy[];
};

/** The installation-wide report, for a platform account. */
export type MediaCrossSiteReport = {
  site_ids: string[];
  cross_site: true;
  group_count: number;
  total_bytes: number;
  groups: MediaCrossSiteGroup[];
  notice: string;
};

/** What a merge did. `notice` says the bytes are *pending*, because they are. */
export type MediaMergeResult = {
  kept: string;
  trashed: string[];
  references_moved: number;
  references_collapsed: number;
  bytes_pending_purge: number;
  shares_revoked: number;
  notice: string;
};

/** What a replace or a restore did. */
export type MediaReplaceResult = {
  file: MediaFile;
  version: MediaVersion;
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
  /**
   * Upload window, as `YYYY-MM-DD` — a day, not a timestamp.
   *
   * A date input can only offer a day, and "uploaded on 3 October" is the question an operator is
   * asking. `mediaQuery` turns it into the two instants the store takes: `created_after` is the
   * start of that day **in the operator's own timezone** (so a file uploaded at 23:00 on the 3rd
   * is inside the window, and a file uploaded at 01:00 on the 4th is not), and `created_before`
   * is the start of the *next* day — which is what makes the last day inclusive.
   */
  created_after?: string;
  /** The last day of the window, inclusive; expanded to the start of the following day. */
  created_before?: string;
  tag?: string;
  metadata?: string;
  scan_status?: string;
  has_versions?: boolean;
  sort?: string;
  limit?: number;
  offset?: number;
};

/** One account the uploader filter offers, from `GET /api/v1/media/uploaders`. */
export type MediaUploader = {
  id: string;
  label: string;
  files: number;
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


// ---------------------------------------------------------------------------------------------
// Notifications (docs/requests/REQ-021, slice 1)
// ---------------------------------------------------------------------------------------------

/** One notification as the panel reads it. */
export type NotificationRow = {
  id: string;
  category: string;
  priority: string;
  title: string;
  body: string;
  /** Where its link goes; `null` means the panel must not render a link. */
  url: string | null;
  source_type: string | null;
  source_id: string | null;
  payload: unknown;
  read_at: string | null;
  archived_at: string | null;
  created_at: string;
  /**
   * One row per channel the notification was tried on, oldest first. Empty means nothing has
   * been attempted yet — which is a real state, not a failure to load.
   *
   * **Only the detail route fills this.** The list is a page of rows the reader has not opened,
   * so carrying deliveries there would cost one query per row to say "nothing was sent" about
   * notifications nobody has clicked.
   */
  deliveries: NotificationDeliveryRow[];
};

/**
 * What became of one channel, as the reader is shown it.
 *
 * The drawer exists so that "it is in my panel but the e-mail never arrived" is a row somebody
 * can read rather than an absence they have to interpret. `attempts`/`max_attempts` are carried
 * together for that reason: "failed" on its own does not say whether the platform tried once or
 * gave up.
 */
export type NotificationDeliveryRow = {
  channel: NotificationChannel;
  status: NotificationDeliveryStatus;
  attempts: number;
  max_attempts: number;
  /** The transport's own status code, when it answered with one. */
  response_status: number | null;
  /** Why it did not go out, in the platform's words. */
  error: string | null;
  sent_at: string | null;
  /** When the next attempt is due; `null` once the row is no longer retryable. */
  next_attempt_at: string | null;
};

/** One grouped line of the bell. */
export type NotificationCategoryCount = {
  category: string;
  count: number;
};

/** The bell's numbers: one total, and one line per category including the empty ones. */
export type NotificationSummary = {
  unread: number;
  by_category: NotificationCategoryCount[];
};

/** A page of the list, with the cursor for the next one. */
export type NotificationPage = {
  notifications: NotificationRow[];
  has_more: boolean;
  next_before: string | null;
};

/** What one bulk action really changed, and the unread count after it. */
export type NotificationBulkResult = {
  action: string;
  changed: number;
  unread: number;
};

/** The filters the list accepts; every field is optional and every one is shareable in a URL. */
export type NotificationFilters = {
  category?: string;
  read?: "unread" | "read";
  priority?: string;
  channel?: string;
  archived?: boolean;
  with_read?: boolean;
  before?: string;
  limit?: number;
};

/** The closed vocabulary, so the panel never hard-codes what the server already knows. */
export const NOTIFICATION_CATEGORIES = [
  "approval",
  "security",
  "update",
  "ticket",
  "system",
  "mention",
] as const;

export const NOTIFICATION_PRIORITIES = ["low", "normal", "high", "critical"] as const;

export const NOTIFICATION_CHANNELS = [
  "in_app",
  "email",
  "web_push",
  "webhook",
  "chat",
] as const;

/**
 * One of the five channels, as a type.
 *
 * Derived from the const rather than written out, so a channel added to the list is a channel
 * the delivery rows can be typed with. A hand-written union is one more list to keep in step,
 * and a channel in the database that the union does not name is a `type` error rather than a
 * runtime surprise — which is the direction that catches it.
 */
export type NotificationChannel = (typeof NOTIFICATION_CHANNELS)[number];

// ---------------------------------------------------------------------------------------------
// Slice 2: the reader's own channel configuration
// ---------------------------------------------------------------------------------------------

/**
 * One cell of the matrix: "does category *C* reach me over *channel*?".
 *
 * The form never invents a cell — the server sends all thirty and the form renders what it is
 * given, so a channel added in a later slice appears here with no change to this file.
 */
export type NotificationPreferenceCell = {
  category: string;
  channel: string;
  enabled: boolean;
};

/** Quiet hours, the timezone and the digest cadence. */
export type NotificationSettingsRow = {
  /** `HH:MM` in the reader's own timezone, or `null` for no window. */
  quiet_hours_start: string | null;
  quiet_hours_end: string | null;
  /** IANA zone name; an unknown one is read as UTC by the server. */
  timezone: string;
  /** `off`, `daily` or `weekly`. */
  digest_cadence: string;
  /** Which weekday a weekly digest goes out on, 0 = Monday. */
  digest_weekday: number | null;
  /** Which hour a digest goes out in. */
  digest_hour: number;
};

/**
 * The whole preferences answer.
 *
 * `locked_channel` comes from the server rather than being hard-coded here: the rule that
 * in-app cannot be switched off is a server rule, and a form that hard-codes the name while
 * the server owns the rule is one rename away from a checkbox that lies.
 */
export type NotificationPreferences = {
  cells: NotificationPreferenceCell[];
  settings: NotificationSettingsRow;
  locked_channel: string;
};

/** What a save changed, and the authoritative state to render from. */
export type NotificationPreferencesSaved = {
  /** How many cells actually changed value — zero is a legitimate answer. */
  changed: number;
  cells: NotificationPreferenceCell[];
  settings: NotificationSettingsRow;
  locked_channel: string;
};

export const DIGEST_CADENCES = ["off", "daily", "weekly"] as const;

/** 0 = Monday, which is the numbering the server's `extract(dow) - 1` uses. */
export const DIGEST_WEEKDAYS = [
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
  "Sunday",
] as const;

/**
 * The zones the form offers.
 *
 * A choice list, not the IANA database: the server reads anything else as UTC, and a select
 * with 400 entries is a select nobody scrolls.
 */
export const NOTIFICATION_TIMEZONES = [
  "UTC",
  "Europe/Istanbul",
  "Europe/Berlin",
  "Europe/London",
  "Europe/Paris",
  "Europe/Madrid",
  "Europe/Rome",
  "Europe/Amsterdam",
  "America/New_York",
  "America/Los_Angeles",
  "America/Sao_Paulo",
  "Asia/Dubai",
  "Asia/Kolkata",
  "Asia/Tokyo",
  "Australia/Sydney",
] as const;

// ---------------------------------------------------------------------------------------------
// Slice 3: the half that leaves the panel
// ---------------------------------------------------------------------------------------------

/** The four states a delivery can be in. A closed list, so the filter chips are exhaustive. */
export const NOTIFICATION_DELIVERY_STATUSES = [
  "pending",
  "sent",
  "failed",
  "skipped",
] as const;

export type NotificationDeliveryStatus = (typeof NOTIFICATION_DELIVERY_STATUSES)[number];

/**
 * One row of the organization's delivery log.
 *
 * **There is no `title` and no `body` here, and that is the design.** An administrator opening
 * the outbox during an incident needs to know *that* a delivery failed and *whose* it was — the
 * content is a customer record, and this is the screen with the widest audience in the panel.
 * The server's type cannot express it either, so adding a field is a visible review event rather
 * than a "let's just show it" at the end of a feature.
 */
export type NotificationOutboxRow = {
  id: string;
  notification_id: string;
  category: string;
  priority: string;
  user_id: string;
  channel: string;
  status: NotificationDeliveryStatus;
  attempts: number;
  max_attempts: number;
  response_status: number | null;
  error: string | null;
  sent_at: string | null;
  created_at: string;
};

/** The counts behind the filter chips, plus the total so a chip need not add them up itself. */
export type NotificationOutboxCounts = {
  pending: number;
  sent: number;
  failed: number;
  skipped: number;
  total: number;
};

/** The outbox answer: a page, the counts, and how far back the log reaches. */
export type NotificationOutbox = {
  rows: NotificationOutboxRow[];
  counts: NotificationOutboxCounts;
  retention_days: number;
};

/**
 * One registered browser, as the devices list shows it.
 *
 * `endpoint_hint` is `…abcdef01` — enough for a reader to recognise their own phone, useless to
 * somebody who screenshots the screen. The full endpoint is a capability key and the server
 * never sends it back.
 */
export type NotificationDevice = {
  id: string;
  endpoint_hint: string;
  user_agent: string | null;
  created_at: string;
  last_seen_at: string;
};

/** What registering a browser did — the four outcomes, not a boolean. */
export type NotificationPushOutcome = "created" | "refreshed" | "reassigned" | "re-keyed";

/** What a channel can do on this installation, and why. */
export type NotificationChannelReadiness = {
  channel: string;
  available: boolean;
  locked: boolean;
  detail: string;
};

/**
 * The installation's push key, and whether a browser can subscribe with it.
 *
 * `public_key` is `null` rather than `""` on purpose: it goes straight into
 * `applicationServerKey`, and an empty string there makes `pushManager.subscribe`
 * reject the call. `available` is false in the half-configured case too — a key with
 * no contact address can be used to subscribe but never to send.
 */
export type NotificationPushKey = {
  public_key: string | null;
  available: boolean;
  reason: string;
};

/** The four shapes a routing rule can address. Kept as data for the form's select. */
export const NOTIFICATION_RECIPIENT_SHAPES = [
  { value: "actor", label: "The actor who caused it", needsTarget: false },
  { value: "permission:", label: "Everybody holding a permission", needsTarget: true },
  { value: "role:", label: "Everybody with a role", needsTarget: true },
  { value: "payload_user:", label: "The user named in the payload", needsTarget: true },
] as const;

/** One rule of the router: an event name, a category, and who hears about it. */
export type NotificationRouteRule = {
  id: string;
  event_name: string;
  category: string;
  priority: string;
  recipient: string;
  title_template: string;
  url_template: string | null;
  enabled: boolean;
  created_by: string | null;
  created_at: string;
};

/** What one routing pass did — the counts are the whole point of the answer. */
export type NotificationRouteReport = {
  created: number;
  deduped: number;
  unmatched_rules: number;
  unknown_event: boolean;
};

// ---------------------------------------------------------------------------------------------
// Menus and the scheduled publishing queue (REQ-064, slice 1)
// ---------------------------------------------------------------------------------------------

/**
 * A menu as the list shows it: the name, the theme slots it renders into and the number of items
 * it holds — nested ones included, because a menu with two rows on screen and eleven entries is
 * not a menu with two entries.
 */
// ---------------------------------------------------------------------------------------------
// Forms (REQ-064, slice 2)
// ---------------------------------------------------------------------------------------------

/**
 * One field of a form, as the builder's inspector reads it.
 *
 * `rules` and `options` are free-shaped because they are per-field and never queried — the same
 * reason the store keeps them as JSON inside the field rather than as columns.
 */
export type FormField = {
  id: string;
  key: string;
  label: string;
  field_type: string;
  required: boolean;
  placeholder: string | null;
  help_text: string | null;
  width: string;
  rules: Record<string, unknown>;
  options: unknown;
};

/** The closed vocabularies the palette and inspector draw from. */
export type FormsVocabulary = {
  field_types: string[];
  statuses: string[];
  submit_actions: string[];
  max_fields: number;
  max_answer_length: number;
};

/** A form as the list reads it. */
export type Form = {
  id: string;
  site_id: string;
  site_key: string;
  key: string;
  name: string;
  status: string;
  /**
   * The settings travel with the form rather than in a second endpoint: the settings drawer and
   * the builder's Publish button are one screen, and a drawer that has to fetch before it can
   * show what the form currently does renders the defaults half the time.
   */
  submit_action: string;
  submit_message: string | null;
  redirect_url: string | null;
  notify_emails: string[];
  honeypot: boolean;
  min_fill_seconds: number;
  rate_limit_per_hour: number;
  retention_days: number;
  field_count: number;
  unread_count: number;
  spam_count: number;
  updated_at: string;
};

/** A form with its fields and its settings: the builder's document. */
export type FormDetail = Form & {
  fields: FormField[];
  vocabulary: FormsVocabulary;
};

/**
 * A submission as the inbox reads it.
 *
 * `summary` is derived by the API from the form's *field keys*, not from the first text field:
 * a form whose first field is a subject line would otherwise show that subject in the Name
 * column, which reads as bad data rather than as a guess.
 */
export type Submission = {
  id: string;
  answers: Record<string, unknown>;
  consent_text: string | null;
  source_path: string | null;
  status: string;
  spam_score: number;
  /**
   * What became of the builder's notification. `null` means "no attempt was ever recorded",
   * which is NOT the same as "not sent" — a row from before the send existed has no record,
   * and drawing it as a delivery failure would be a lie about history.
   */
  notified_at?: string | null;
  notify_status?: "sent" | "skipped" | "failed" | null;
  notify_error?: string | null;
  created_at: string;
  summary?: {
    name?: string;
    email?: string;
    text: string;
  };
};

/** One page of the inbox. */
export type Inbox = {
  submissions: Submission[];
  total: number;
  counts: { new: number; read: number; spam: number; archived: number };
};

export type Menu = {
  id: string;
  site_id: string;
  /**
   * The site's GLOBAL key. The public routes address a site by key or host, the authenticated
   * ones by uuid; the audience preview calls a public route, so it needs this and not `site_id`.
   */
  site_key: string;
  key: string;
  name: string;
  locations: string[];
  item_count: number;
  updated_at: string;
};

/**
 * One navigation row.
 *
 * `id` is minted by the editor and never rewritten, which is what lets a drag be expressed as a
 * whole-tree write: the same id moves to a new parent, and the store replaces the whole document
 * in one transaction instead of applying six partial updates that can half-apply.
 */
export type MenuItem = {
  id: string;
  parent_id: string | null;
  position: number;
  label: string;
  item_type: string;
  page_id: string | null;
  url: string;
  target: string;
  rel: string;
  css_class: string;
  enabled: boolean;
  visibility: string;
  visibility_roles: string[];
};

/** The closed vocabularies the editor draws its pickers from, sent by the server. */
export type MenuVocabulary = {
  locations: string[];
  item_types: string[];
  visibilities: string[];
  max_depth: number;
};

/** The editor's own document: the menu, its items and the vocabulary. */
export type MenuDetail = Menu & {
  items: MenuItem[];
  vocabulary: MenuVocabulary;
};

/** What an item can link to, in the editor's own words. */
export const MENU_ITEM_TYPES = [
  { value: "page", label: "Page", hint: "A page of this site" },
  { value: "url", label: "URL", hint: "Any address, absolute or site-relative" },
  { value: "anchor", label: "Anchor", hint: "A #section on the current page" },
  { value: "index", label: "Site index", hint: "The front page of the site" },
] as const;

/** Who may see an item. The server refuses anything outside this list. */
export const MENU_VISIBILITIES = [
  { value: "everyone", label: "Everyone" },
  { value: "members", label: "Signed-in visitors" },
  { value: "logged_out", label: "Signed-out visitors" },
  { value: "roles", label: "Visitors with these roles" },
] as const;

/** One queue row: a promise that a page appears or disappears at an instant. */
export type PublishingEntry = {
  id: string;
  page_id: string;
  page_slug: string;
  page_title: string;
  page_type: string;
  action: string;
  /** RFC 3339, UTC. `timezone` beside it is the author's wall clock, not an offset. */
  scheduled_at: string;
  timezone: string;
  status: string;
  result: string;
  error: string;
  claimed_at: string | null;
};

/** The four states a queue row can be in, and the ones a worker can still act on. */
export const PUBLISHING_STATUSES = [
  { value: "", label: "All states" },
  { value: "pending", label: "Pending" },
  { value: "done", label: "Done" },
  { value: "failed", label: "Failed" },
  { value: "cancelled", label: "Cancelled" },
] as const;

/** One audience-filtered navigation row, as `GET /api/v1/public/menus/{location}` answers. */
export type RenderedMenuItem = {
  id: string;
  label: string;
  /** Already resolved: a `page` item's slug became a path, an `anchor` kept its hash. */
  href: string;
  external: boolean;
  rel: string;
  css_class: string;
  children: RenderedMenuItem[];
};

/** What the theme draws for one location. */
export type RenderedMenu = {
  key: string;
  name: string;
  items: RenderedMenuItem[];
};

/** One row of the event feed, as `/api/v1/events` answers it. */
export type EventRow = {
  id: number;
  name: string;
  organization_id: string | null;
  site_id: string | null;
  actor_user_id: string | null;
  payload: unknown;
  created_at: string;
};

/** One page of the feed: the rows plus the keyset the next page is read with. */
export type EventPage = {
  events: EventRow[];
  /** Id of the last row, to pass back as `cursor`. `null` at the end of the feed. */
  next_cursor: number | null;
  /** Whether a further page exists behind this one. */
  has_more: boolean;
};

/** What narrows the feed. Every field is optional; set fields are combined with AND. */
export type EventFilters = {
  /** Exact names, any of which may match. */
  name?: string[];
  site_id?: string;
  actor_user_id?: string;
  /** RFC 3339 lower bound. */
  from?: string;
  /** RFC 3339 upper bound. */
  to?: string;
  /** Keyset cursor: the previous page's last row id. */
  cursor?: number;
  limit?: number;
};

/** One payload field of one event name, as the catalogue describes it. */
export type CatalogueField = {
  name: string;
  kind: "uuid" | "string" | "integer" | "boolean" | "timestamp" | "json" | "any";
  required: boolean;
};

/** One entry of the event catalogue. */
export type CatalogueEntry = {
  name: string;
  area: string;
  group: string;
  description: string;
  status: "live" | "reserved";
  payload_fields: CatalogueField[];
  /** Deliveries this name produced in the last 24 hours, for this organization. */
  deliveries_24h: number;
};

/** ---------------------------------------------------------------------------------------------
 * The webhook endpoints and their delivery history (REQ-016, slice 2).
 * ------------------------------------------------------------------------------------------- */

/** One endpoint as the API describes it. `secret` is present exactly once, at creation. */
export type WebhookEndpoint = {
  id: string;
  organization_id: string;
  name: string;
  url: string;
  /** Expanded subscription list, including every name a group wildcard covers. */
  events: string[];
  enabled: boolean;
  /** Shown only in the creation response. Every other read omits the key entirely. */
  secret?: string;
  created_at: string;
  updated_at: string;
};

/** Every endpoint this account may see. */
export type WebhookList = { webhooks: WebhookEndpoint[] };

/** What a test delivery queued. */
export type WebhookTestReport = { event_id: number; deliveries: number };

/** A rotation's answer: the endpoint, and the one secret it will ever show again. */
export type WebhookRotation = WebhookEndpoint & { secret: string };

/** One queued delivery, as the history read describes it. */
export type WebhookDelivery = {
  id: string;
  event_id: number;
  event_name: string;
  status: "pending" | "delivered" | "failed";
  attempts: number;
  max_attempts: number;
  next_attempt_at: string;
  response_status: number | null;
  error: string | null;
  delivered_at: string | null;
  created_at: string;
  /** What asked for this row: the bus, an operator's test, or a forced replay. */
  trigger: "event" | "test" | "replay";
  /** How long the receiver took. `null` until the row has run. */
  duration_ms: number | null;
  redeliver_count: number;
  replayed_at: string | null;
};

/** The keyset one page hands to the next. Both halves, or the API refuses it. */
export type WebhookDeliveryCursor = { at: string; id: string };

/** One page of a delivery history. */
export type WebhookDeliveryPage = {
  deliveries: WebhookDelivery[];
  /** How many rows the filter matches, so the table can say "25 of 340". */
  total: number;
  has_more: boolean;
  next_cursor: WebhookDeliveryCursor | null;
};

/** What narrows a delivery history. Set fields combine with AND; `status` and `name` are OR. */
export type WebhookDeliveryFilters = {
  /** Statuses, any of which may match. */
  status?: string[];
  /** Event names, any of which may match. */
  name?: string[];
  from?: string;
  to?: string;
  /** Substring of the delivery id or the event name. */
  q?: string;
  cursor_at?: string;
  cursor_id?: string;
  limit?: number;
};

/** One row of a bulk redelivery that did not move, and why. */
export type WebhookRedeliverSkip = { delivery_id: string; code: string; message: string };

/** A bulk redelivery's answer: what moved, and what did not with a reason each. */
export type WebhookRedeliverBatch = { queued: number; skipped: WebhookRedeliverSkip[] };

/** What an endpoint's receiver has been doing. */
export type WebhookStats = {
  window_hours: number;
  /** Settled **traffic** rows the receiver accepted; test rows are excluded. */
  delivered: number;
  /** Settled traffic rows that ran out of attempts; test rows are excluded. */
  failed: number;
  /** Rows still waiting, whatever their age. */
  pending: number;
  /** Every row in the window, tests included. */
  total: number;
  /** Rows an operator asked for by hand, reported apart from the rate. */
  tests: number;
  /** Share of settled traffic rows accepted; `null` when none settled. */
  success_rate: number | null;
  /** 95th percentile receiver duration; `null` when nothing ran. */
  p95_duration_ms: number | null;
};

/** The whole catalogue, grouped by area for the picker. */
export type EventCatalogue = {
  areas: string[];
  events: CatalogueEntry[];
  live_count: number;
  reserved_count: number;
  max_subscriptions: number;
};

/** One sweep of the event bus's retention, as the run log records it. */
export type RetentionRun = {
  id: string;
  /** `null` is the platform's own events. */
  organization_id: string | null;
  started_at: string;
  finished_at: string | null;
  /** The window that was applied, in days. */
  window_days: number;
  /** The instant older rows were swept. */
  cutoff: string;
  events_deleted: number;
  /** Delivery rows removed with their events. */
  deliveries_deleted: number;
  error: string | null;
};

/**
 * How much history this organization keeps, and the last sweeps that ran.
 *
 * `due` is deliberately the same predicate the sweeper uses — an event a receiver is still owed
 * is history, not due — so a screen that says "12 due" is never contradicted by a sweep that
 * removes nothing.
 */
export type RetentionStatus = {
  organization_id: string | null;
  window_days: number;
  /** Shortest window the API accepts, so the input can be bounded by the server's own rule. */
  min_days: number;
  max_days: number;
  events: number;
  due: number;
  last_run: RetentionRun | null;
  recent_runs: RetentionRun[];
};

/** What a manual sweep removed. */
export type SweepResult = {
  organization_id: string | null;
  window_days: number;
  cutoff: string;
  events_deleted: number;
  deliveries_deleted: number;
  run_id: string;
};


// ---------------------------------------------------------------------------------------------
// Security centre (REQ-012, slice 1)
// ---------------------------------------------------------------------------------------------

/** What a posture check can say. `unknown` is the honest one and is never a failure. */
export type SecurityCheckState = "pass" | "warn" | "fail" | "unknown";

/** Where a finding came from. */
export type SecurityFindingSource = "config" | "dependency" | "platform" | "report";

/** How bad a finding is, worst first. */
export type SecuritySeverity = "critical" | "high" | "medium" | "low" | "info";

/** What has been done about a finding. */
export type SecurityFindingStatus = "open" | "acknowledged" | "fixed" | "ignored";

/**
 * One posture check row.
 *
 * `checked_at` is `null` when the check has **never been evaluated**, which is different from
 * an old timestamp and must not be rendered as one: the panel shows the "Run checks" call to
 * action next to it instead of a date in 1970. The action link is present on every row,
 * including passing ones, because a row whose action is empty renders a dead button.
 */
export type SecurityCheck = {
  key: string;
  label: string;
  state: SecurityCheckState;
  detail: Record<string, unknown>;
  checked_at: string | null;
  action_href: string;
  action_label: string;
};

/** The posture overview's answer. */
export type SecurityOverview = {
  checks: SecurityCheck[];
  /** 0-100. An `unknown` check counts as a question, never as a clearance. */
  score: number;
  /** How many checks are in each state, all four keys always present. */
  summary: Record<SecurityCheckState, number>;
  open_findings: SecuritySeverityCount[];
  last_run_at: string | null;
  registry: string[];
};

/** One bucket of the score ring's legend. */
export type SecuritySeverityCount = {
  severity: SecuritySeverity;
  count: number;
};

/**
 * A finding.
 *
 * `ignore_lapsed` is read rather than stored: an ignore with a date in the past is not an
 * ignore, the row is not rewritten (a job that did not fire would hide a finding), and the
 * panel greys the row and offers "reopen" instead of pretending the ignore still holds.
 */
export type SecurityFinding = {
  id: string;
  source: SecurityFindingSource;
  severity: SecuritySeverity;
  title: string;
  description: string;
  component: string | null;
  component_version: string | null;
  fixed_in: string | null;
  status: SecurityFindingStatus;
  ignore_reason: string | null;
  ignored_until: string | null;
  ignore_lapsed: boolean;
  note: string | null;
  first_seen_at: string;
  last_seen_at: string;
  is_open: boolean;
};

/** The findings screen's filters, echoed back so the UI can show what it is looking at. */
export type SecurityFindingFilter = {
  severity?: SecuritySeverity | "";
  status?: SecurityFindingStatus | "";
  source?: SecurityFindingSource | "";
  component?: string;
  search?: string;
};

/** One page of findings. `total` is the filter's whole count, not this page's length. */
export type SecurityFindingPage = {
  findings: SecurityFinding[];
  total: number;
  offset: number;
  filter: {
    severity: string | null;
    status: string | null;
    source: string | null;
    component: string | null;
    search: string | null;
    limit: number;
  };
};

/** What a bulk status change did — reported per row, never silently. */
export type SecurityBulkResult = {
  updated: string[];
  /** Ids that matched nothing: stale selections, not failures. */
  missing: string[];
};

/** What an ingest did: what is new, what was already known, what was refused. */
export type SecurityImportReport = {
  created: number;
  refreshed: number;
  rejected: string[];
};

// ---------------------------------------------------------------------------------------------
// SEO toolkit (REQ-064, slice 3)
// ---------------------------------------------------------------------------------------------

/** The editable SEO fields of one page. */
export type PageSeo = {
  seo_title: string | null;
  seo_description: string | null;
  canonical_url: string | null;
  og_title: string | null;
  og_description: string | null;
  og_image_media_id: string | null;
  twitter_card: string;
  robots: string;
  structured_data_type: string | null;
  structured_data: Record<string, unknown>;
};

/** The `<meta>` set a crawler reads, built by the server's own generator. */
export type SeoTags = {
  title: string;
  description: string | null;
  canonical: string | null;
  og_title: string;
  og_description: string | null;
  og_image: string | null;
  og_type: string;
  twitter_card: string;
  robots: string;
  json_ld: string | null;
  /** Schema fields the chosen type wants and this page cannot supply. */
  missing_fields: string[];
};

/** A page's fields plus the tags they produce. */
export type PageSeoBody = {
  seo: PageSeo;
  tags: SeoTags;
};

/** One redirect rule. */
export type SeoRedirect = {
  id: string;
  from_path: string;
  to_path: string;
  status_code: number;
  pattern: string;
  enabled: boolean;
  hits: number;
  last_hit_at: string | null;
};

/** What `Test a path` found, including the rules that also answer it. */
export type SeoRedirectTest = {
  path: string;
  matched: SeoRedirect | null;
  also_matched: SeoRedirect[];
};

/** One row a redirect file was refused on, with the reason a human can act on. */
export type SeoRedirectRejection = {
  /** 1-based line in the uploaded file, or 0 for a refusal about the file as a whole. */
  line: number;
  /** The row as it was read. */
  row: string;
  /** Why it was refused. */
  reason: string;
};

/**
 * What an import did, or would do on a dry run.
 *
 * `imported` and `accepted` are three different numbers and keeping them apart is the point: a
 * dry run has all the accepted rows and no imported ones, and a refused file has imported none
 * of the rows it accepted. A single "rows" number would render those two states identically.
 */
export type SeoRedirectImport = {
  clean: boolean;
  imported: number;
  accepted: number;
  summary: string;
  rejected: SeoRedirectRejection[];
};

/** One broken internal link. */
export type SeoBrokenLink = {
  id: string;
  source_page_id: string | null;
  source_slug: string | null;
  target_url: string;
  anchor_text: string | null;
  status: number | null;
  ignored: boolean;
};

/** A site's stored settings, with the sitemap the panel previews. */
export type SeoSettings = {
  sitemap_types: string[];
  default_priority: number;
  default_change_frequency: string;
  sitemap_xml: string | null;
  sitemap_last_generated_at: string | null;
  /** How many URLs the stored sitemap holds, so an empty preview can explain itself. */
  sitemap_url_count: number;
  robots_txt: string;
};

/**
 * The closed vocabularies the editor offers.
 *
 * Served rather than hard-coded: a picker that offers a schema type the server then refuses is a
 * picker whose rejection arrives as an unexplained 400.
 */
export type SeoVocabulary = {
  structured_data_types: string[];
  twitter_cards: string[];
  redirect_patterns: string[];
  redirect_status_codes: number[];
  change_frequencies: string[];
  /** This site's own page types, for the sitemap's inclusion list. */
  page_types: string[];
};

/** Everything the SEO screen draws, in one read. */
export type SeoOverview = {
  site: { id: string; key: string; name: string; host: string | null };
  vocabulary: SeoVocabulary;
  settings: SeoSettings;
  redirects: SeoRedirect[];
  broken_links: SeoBrokenLink[];
  robots_warnings: string[];
};

// ---------------------------------------------------------------------------------------------
// Page comments (REQ-064, slice 4a)
// ---------------------------------------------------------------------------------------------

/** The four moderation states, as the API reports them. */
export type CommentStatus = "pending" | "approved" | "spam" | "trash";

/** One comment as the inbox draws it. Carries the address and the client fingerprint. */
export type CommentInboxRow = {
  id: string;
  page_id: string;
  parent_id: string | null;
  reply_depth: number;
  author_name: string;
  author_email: string;
  ip_hint: string | null;
  user_agent: string | null;
  body: string;
  status: CommentStatus;
  /** Which heuristic marked it, or a moderator's note. */
  spam_reason: string | null;
  approved_at: string | null;
  approved_by: string | null;
  is_staff_reply: boolean;
  created_at: string;
  updated_at: string;
  /** The page's title, so a moderator reads "About us" rather than a slug. */
  page_title: string | null;
};

/** One tab's count. */
export type CommentTabCount = { status: string; count: number };

/** The inbox page: the rows and the four tab counts. */
export type CommentInbox = {
  comments: CommentInboxRow[];
  total: number;
  counts: CommentTabCount[];
  /** The states the panel offers, generated by the API rather than typed here twice. */
  statuses: string[];
};

/** What a bulk moderation did, per comment. */
export type CommentBulkResult = {
  updated: string[];
  missing: string[];
  refused: string[];
  complete: boolean;
  requested: number;
};

/** The moderation policy. */
export type CommentSettings = {
  site_id: string;
  comments_enabled: boolean;
  auto_approve_after_comments: number;
  blocked_words: string[];
  max_links_per_comment: number;
  min_fill_seconds: number;
  per_ip_per_hour: number;
  notify_on_comment: boolean;
  updated_at: string;
};

/** One ban. An `ip` ban's value is a fingerprint, never an address. */
export type CommentBan = {
  id: string;
  kind: "email" | "ip";
  value: string;
  reason: string | null;
  created_at: string;
  expires_at: string | null;
  active: boolean;
};

/** The settings screen in one read. */
export type CommentSettingsDocument = {
  settings: CommentSettings;
  bans: CommentBan[];
};

/** A reply as the site's own author. */
export type NewCommentReply = {
  site_id: string;
  author_name: string;
  body: string;
};

// ---------------------------------------------------------------------------------------------
// Members (REQ-064, slice 4c) — visitor accounts, their sessions, the site policy
// ---------------------------------------------------------------------------------------------

/**
 * The three visitor states.
 *
 * `pending` is a signup that has not clicked its verification link. It is NOT a failure and the
 * panel must not render it as one: a row that says "broken" for an account that is merely
 * waiting teaches an operator that verification is broken.
 */
export type MemberStatus = "pending" | "verified" | "blocked";

/**
 * A visitor account, as the panel may describe them.
 *
 * There is deliberately no password field of any kind on this type. The API's own struct omits
 * `password_hash` structurally rather than with `skip_serializing_if`, and a type that had the
 * field would let the panel render one the day somebody adds it back.
 *
 * `has_password` is a BOOLEAN rather than the hash, and it is the only honest way to say what the
 * panel needs: "invited, has never claimed the account" and "signed in yesterday" are two
 * different rows the operator must be able to tell apart, and neither is answered by a hash.
 */
export type Member = {
  id: string;
  site_id: string;
  email: string;
  name: string | null;
  /** The SITE's own role names. These are not panel roles and resolve to nothing in IAM. */
  roles: string[];
  status: MemberStatus;
  verified_at: string | null;
  last_signin_at: string | null;
  has_password: boolean;
  /** Live member sessions right now. Not panel sessions — `cms_member_sessions` only. */
  live_sessions: number;
  signin_note: string | null;
  created_at: string;
};

/** One live member session, as the drawer lists them. */
export type MemberSignin = {
  id: string;
  created_at: string;
  last_seen_at: string;
  expires_at: string;
};

/** The members table and its three chips. */
export type MemberList = {
  members: Member[];
  total: number;
  counts: { pending: number; verified: number; blocked: number };
};

/** The drawer: the member plus the last ten sign-ins. */
export type MemberDetail = {
  member: Member;
  recent_signins: MemberSignin[];
};

/**
 * The site's membership policy.
 *
 * `gated_page_behaviour` is the one field the panel has to argue about rather than merely set:
 * `not_found` answers 404 and discloses nothing, `prompt` answers 401 with a sign-in link and
 * therefore advertises that the page exists. The default is `not_found` and the REQ's criterion
 * says so, which is why this is a radio with an explanation and not a checkbox.
 */
export type MemberSettings = {
  site_id: string;
  signup_enabled: boolean;
  require_verification: boolean;
  default_roles: string[];
  post_signin_redirect: string | null;
  gated_page_behaviour: "not_found" | "prompt";
  updated_at: string;
};

/** The settings screen in one read, with the one number the summary line needs. */
export type MemberSettingsDocument = {
  settings: MemberSettings;
  verified_count: number;
};

/** What a mail-bound action reports. `unavailable` means no mail transport is configured. */
export type MemberDelivery = {
  member_id: string;
  delivery: "sent" | "unavailable" | string;
};

// ---------------------------------------------------------------------------------------------
// Newsletter (REQ-064, slice 4b) — lists, double opt-in subscribers, the sent archive
// ---------------------------------------------------------------------------------------------

/**
 * The four subscriber states.
 *
 * `pending` is the whole point of a double opt-in: the row exists and is NOT deliverable yet.
 * A UI that renders `pending` as a failure teaches an owner that subscribers are broken; it is
 * the state a signup is in until somebody clicks the link.
 */
export type SubscriberStatus = "pending" | "confirmed" | "unsubscribed" | "bounced";

/** One list. `key` is what a theme's signup form posts to, so the panel always shows it. */
export type NewsletterList = {
  id: string;
  site_id: string;
  organization_id: string;
  key: string;
  name: string;
  description: string | null;
  double_opt_in: boolean;
  created_at: string;
  updated_at: string;
  /**
   * The per-state counts, present on the list read and absent on a single-row read — the row
   * itself has no such columns. `undefined` means "not in this response", so the panel shows a
   * placeholder rather than printing zero, which would read as "this list has no subscribers".
   */
  counts?: ListCounts;
};

/** The counts beside a list, from the same read as the list. */
export type ListCounts = {
  pending: number;
  confirmed: number;
  unsubscribed: number;
  bounced: number;
  total: number;
};

/** A list with its counts — one row of the list screen. */
export type NewsletterListRow = { list: NewsletterList; counts: ListCounts };

/** One subscriber row. */
export type NewsletterSubscriber = {
  id: string;
  site_id: string;
  list_id: string;
  email: string;
  name: string | null;
  source: string | null;
  status: SubscriberStatus;
  /** The store never returns the token digests — they are not in the panel's vocabulary. */
  confirmed_at: string | null;
  unsubscribed_at: string | null;
  /** Whether the confirmation link is still waiting for a click, and when it stops being one. */
  confirm_expires_at: string | null;
  /** Why the row is in the state it is in. The first question an owner asks. */
  status_reason: string | null;
  created_at: string;
  updated_at: string;
  /** The list's name, so a mixed-list table does not need a join in the browser. */
  list_name: string | null;
};

/** A page of subscribers plus its total. */
export type SubscriberPage = {
  subscribers: NewsletterSubscriber[];
  total: number;
  limit: number;
  offset: number;
};

/** One sent issue in the archive list. */
export type NewsletterIssue = {
  id: string;
  site_id: string;
  list_id: string;
  subject: string;
  /** How many addresses the send reached — what the send knew, not today's count. */
  recipient_count: number;
  archive_slug: string;
  sent_at: string;
  list_name: string | null;
};

/** What a CSV import did, and what it refused to do. */
export type ImportReport = {
  added: number;
  blank: number;
  /** One entry per address already on the list, with the state it is in. */
  skipped: { email: string; status: string }[];
};

/** A new list, as the create form sends it. `key` is derived server-side when omitted. */
export type NewNewsletterList = {
  site_id: string;
  name: string;
  key?: string;
  description?: string;
  double_opt_in?: boolean;
};

// ---------------------------------------------------------------------------------------------
// Security centre (REQ-012, slice 2) — the header policy
// ---------------------------------------------------------------------------------------------

/**
 * Whether the CSP is enforced or only reported.
 *
 * The two are mutually exclusive on the wire: `report_only` sends
 * `Content-Security-Policy-Report-Only` and sends no enforcing header at all, because sending
 * both applies the policy while claiming to only report it.
 */
export type CspMode = "report_only" | "enforce";

/**
 * One CSP directive row.
 *
 * `values` is a **list**, not a string: a source with a space in it is refused by the server,
 * and a form that joined them into one field would only discover that on save. Splitting here
 * is what lets the directive field and the source field be edited apart.
 */
export type CspDirective = {
  directive: string;
  values: string[];
};

/** HSTS as stored. `max_age_seconds: null` means the header is not sent at all. */
export type HstsPolicy = {
  max_age_seconds: number | null;
  include_subdomains: boolean;
  preload: boolean;
};

/**
 * One rendered header line.
 *
 * `value: null` is **not** an omission — it is "configured off", and the screen renders it as a
 * row with a strike rather than hiding it. A header an operator turned off and cannot see is a
 * header they will not know is off. `rendered` is produced by the same function the response
 * middleware applies, which is what makes the preview a preview.
 */
export type HeaderLine = {
  name: string;
  value: string | null;
};

/** The policy as `GET /api/v1/security/headers` returns it. */
export type HeaderPolicyDocument = {
  csp_mode: CspMode;
  csp: CspDirective[];
  hsts: HstsPolicy;
  content_type_options: boolean;
  referrer_policy: string | null;
  permissions_policy: string[];
  /** The exact lines a response carries right now. */
  rendered: HeaderLine[];
  /** `false` means nobody has saved a policy yet and the baseline is showing. */
  saved: boolean;
  updated_by: string | null;
  updated_at: string | null;
};

/** The save request. `expected_document` is the compare-and-swap key. */
export type HeaderPolicySave = {
  csp_mode: CspMode;
  csp: CspDirective[];
  hsts_max_age_seconds: number | null;
  hsts_include_subdomains: boolean;
  hsts_preload: boolean;
  content_type_options: boolean;
  referrer_policy: string | null;
  permissions_policy: string[];
  expected_document?: unknown;
};

/** The save's answer: the stored policy, flattened, plus the history row it wrote. */
export type HeaderPolicySaved = HeaderPolicyDocument & { change_id: number | null };

// -------------------------------------------------------------------------------------------
// Security centre, slice 3 — rate limiting and sign-in protection
//
// The verdict type is the important one. `retry_after` is `number | null` and NOT optional:
// the server always sends the field, and a `?:` would let a client read a missing field as
// "no wait" — the one reading that turns a refusal into a suggestion. Likewise `ceiling` is
// the *merged* ceiling (limit + burst), sent by the server so the table and the tester cannot
// each compute it and disagree on one row.
// -------------------------------------------------------------------------------------------

/** One rate-limit scope, as `GET /api/v1/security/rate-limits` returns it. */
export type RateLimitScope = {
  scope: string;
  window_seconds: number;
  limit: number;
  burst: number;
  enabled: boolean;
  /** `limit + burst`, computed by the server. */
  ceiling: number;
  /** Seconds until this window frees a place. */
  window_remaining_seconds: number;
};

/** The limiter document, merged with the platform's baseline. */
export type RateLimitsDocument = {
  /** Always all five scopes: a missing row and an unconfigured row are different states. */
  scopes: RateLimitScope[];
  /** The scope names this build knows — the form's own vocabulary check. */
  vocabulary: string[];
  updated_by: string | null;
  updated_at: string | null;
  /** `false` means the baseline is showing because nobody has saved one. */
  is_saved: boolean;
};

/** The save request. `expected_scopes` is the compare-and-swap key. */
export type RateLimitsSave = {
  scopes: Array<{
    scope: string;
    window_seconds: number;
    limit: number;
    burst: number;
    enabled: boolean;
  }>;
  expected_scopes?: unknown;
};

/** The save's answer: the stored document plus the row it wrote. */
export type RateLimitsSaved = RateLimitsDocument & { change_id: number | null };

/** A dry run of one request through the limiter. */
export type RateLimitTestRequest = {
  method: string;
  path: string;
  client_ip?: string | null;
  user_id?: string | null;
  /** The counter to assume — what makes the tester usable during a real incident. */
  count?: number | null;
  machine_key?: boolean;
};

/**
 * The limiter's answer, produced by the *same* function the middleware calls.
 *
 * `counter_identity` and `counter_key` are shown on purpose: an operator explaining a refusal
 * to a developer needs to say "your budget is `ip:203.0.113.7` in the `sign_in` scope", and
 * the key is what makes that checkable against a Redis dump.
 */
export type RateLimitTestResponse = {
  scope: string;
  verdict: {
    limited: boolean;
    scope: string;
    reason: string;
    count: number;
    ceiling: number;
    retry_after: number | null;
  };
  counter_identity: string;
  counter_key: string;
};

/** The brute-force policy, as `GET /api/v1/security/sign-in-protection` returns it. */
export type LockoutPolicy = {
  window_seconds: number;
  attempts: number;
  lockout_minutes: number;
  progressive_delay: boolean;
  base_delay_seconds: number;
  reset_on_success: boolean;
};

/** The accepted ranges, sent by the server so the form does not hard-code them twice. */
export type LockoutBounds = {
  window_seconds: [number, number];
  attempts: [number, number];
  lockout_minutes: [number, number];
  base_delay_seconds: [number, number];
  max_delay_seconds: number;
};

/** The lockout document, with the live count so the form can say so before the table. */
export type SignInProtectionDocument = {
  policy: LockoutPolicy;
  locked_accounts: number;
  is_saved: boolean;
  bounds: LockoutBounds;
};

/** The save request. `expected_policy` is the compare-and-swap key. */
export type SignInProtectionSave = {
  policy: LockoutPolicy;
  expected_policy?: unknown;
};

/** The save's answer: the stored document plus the row it wrote. */
export type SignInProtectionSaved = SignInProtectionDocument & { change_id: number | null };

/** One account currently locked out. */
export type LockedAccount = {
  user_id: string;
  email: string;
  locked_until: string;
  seconds_remaining: number;
  failed_sign_in_count: number;
};

/** The locked list, soonest to expire first, with the true total behind the page. */
export type LockedAccountsPage = {
  accounts: LockedAccount[];
  total: number;
};

// -- REQ-012 slice 4: the IP access lists -------------------------------------------------

/** One row of `/security/ip-access`, as the server sends it. */
export type IpRule = {
  id: string;
  kind: "allow" | "deny";
  cidr: string;
  note: string;
  created_by: string | null;
  created_at: string;
  expires_at: string | null;
  /** Whether the rule is past its expiry right now. Expired rules stay visible rather than
   *  disappearing: a rule that vanishes is a rule nobody knows they have. */
  expired: boolean;
};

/** Both lists and the two counts the summary line shows. */
export type IpRulesPage = {
  rules: IpRule[];
  deny_count: number;
  allow_count: number;
};

/** The create form's payload. `expires_at` is RFC 3339 or null. */
export type CreateIpRuleInput = {
  kind: "allow" | "deny";
  cidr: string;
  note: string;
  expires_at: string | null;
};

/** The created rule, plus the self-lockout warning when it applies. */
export type CreateIpRuleResult = {
  rule: IpRule;
  /** Whether the rule covers the caller's own address. */
  blocks_you: boolean;
  /** The same sentence, ready to render. Null when `blocks_you` is false. */
  warning: string | null;
};

/** The tester's verdict for one address. */
export type IpTestResult = {
  blocked: boolean;
  decision: "allow" | "deny" | null;
  matched_rule: IpRule | null;
  /** A rule that would have matched but has expired — the explanation for a deny that stopped
   *  applying. Never decisive, and the screen says so where it renders it. */
  expired_rule: IpRule | null;
  reason: string;
  normalised: string;
};

// -- REQ-012 slice 4: the security-event timeline -----------------------------------------

/** One row of `/security/events`, as the server sends it. */
export type SecurityEvent = {
  /** `"<source>:<id>"` — unique across both source tables. */
  id: string;
  /**
   * Which table the row came from. Rendered rather than hidden: a merged list whose rows do not
   * say where they came from is a list nobody can reason about during an incident.
   */
  source: "audit" | "sign_in";
  occurred_at: string;
  /** The stable action name, or the sign-in outcome word. */
  action: string;
  /** The screen's category. Served by the API rather than hard-coded in the panel. */
  category: string;
  /**
   * Who acted — **null on a sign-in attempt**, and null there is a fact: nobody was
   * authenticated. Rendering it as a blank cell would read as a rendering fault.
   */
  actor: string | null;
  /** The account an action was *about*. Not the actor: a lockout names its subject. */
  subject_user_id: string | null;
  client_ip: string | null;
  user_agent: string | null;
  /** One line an operator reads instead of parsing the action name. */
  outcome: string;
  /**
   * A key-level digest of the audit metadata — `csp_mode=set, directive_count=2` — never the
   * metadata itself, because a security event is the row most likely to be forwarded out of the
   * platform.
   */
  detail: string | null;
  /** Whether this row is a refusal worth looking at. */
  refused: boolean;
};

/** One page of the timeline, plus the counters the header shows. */
export type SecurityEventsPage = {
  events: SecurityEvent[];
  /** How many rows the filter matched in total — the screen says "50 of 312" with this. */
  total: number;
  audit_count: number;
  sign_in_count: number;
  /** Whether the returned page is shorter than the total. */
  truncated: boolean;
  /** The categories the filter offers, served from the server's own registry. */
  categories: string[];
};

/**
 * One row of the secret inventory.
 *
 * There is deliberately no `value`, `secret`, `ciphertext`, `hash` or `preview` field, and the
 * API is walked to keep it that way. The screen shows a *name* and what the platform can say
 * about it; an operator who needs the value is rotating it in the environment, not reading it
 * here. A type that could hold one would make "the panel shows no secrets" a rendering promise
 * rather than a structural fact.
 */
export type SecretReference = {
  /** `source:name` — unique across sources, because two sources may name the same thing. */
  key: string;
  /** The reference: an environment variable name or a secret-store key. */
  name: string;
  /** Which of the sources it came from. */
  source: string;
  /** What the reference is scoped to — a provider slug, an endpoint, or the platform. */
  scope: string;
  /**
   * The best rotation timestamp the platform can observe, which is the reference row's own
   * timestamp. `evidence` says whether it was edited or merely created, because a date an
   * operator reads as "rotated on" when it only means "registered on" is worse than no date.
   */
  rotated_at: string | null;
  evidence: "reference_changed" | "reference_created" | "unknown";
  /** Days since `rotated_at`, when there is one. */
  age_days: number | null;
  /**
   * How many rows hold real material behind this reference. A **count**, never the material —
   * this number is what replaced the value.
   */
  material_count: number;
  expired: boolean;
  /**
   * What the platform can honestly say. There is no `healthy`: the platform can see that a
   * reference exists and can read nothing about the value behind it.
   */
  state: "unverifiable" | "missing" | "expired";
  /** The reason behind the state, shown in the row. */
  note: string;
};

/** The inventory as the screen receives it. */
export type SecretInventory = {
  secrets: SecretReference[];
  total: number;
  missing: number;
  unverifiable: number;
  /** The source vocabulary, served rather than hard-coded so a dead filter cannot appear. */
  sources: string[];
  /** Every state the vocabulary can produce — the screen's legend, served from the server. */
  states: string[];
  /** What this screen cannot see. Rendered as a permanent note. */
  limitation: string;
};

/** The security-event filter, as the screen holds it. Every field is sent only when set. */
export type SecurityEventsFilter = {
  /** Free text over the action, the outcome or the account an attempt was made against. */
  q?: string;
  category?: string;
  source?: "audit" | "sign_in";
  since?: string;
  until?: string;
  limit?: number;
};

/** The directives this build recognises, in render order — the form's own dropdown. */
export const CSP_DIRECTIVE_NAMES = [
  "default-src",
  "base-uri",
  "object-src",
  "frame-ancestors",
  "script-src",
  "script-src-elem",
  "script-src-attr",
  "style-src",
  "img-src",
  "font-src",
  "connect-src",
  "form-action",
  "frame-src",
  "media-src",
  "worker-src",
  "manifest-src",
  "upgrade-insecure-requests",
  "block-all-mixed-content",
  "require-trusted-types-for",
] as const;

/** The `Referrer-Policy` values browsers implement. An empty choice sends no header. */
export const REFERRER_POLICIES = [
  "no-referrer",
  "no-referrer-when-downgrade",
  "origin",
  "origin-when-cross-origin",
  "same-origin",
  "strict-origin",
  "strict-origin-when-cross-origin",
  "unsafe-url",
] as const;

/**
 * The `max-age` below which a browser ignores the whole HSTS header (~6 months).
 *
 * Kept beside the form because it is the difference between "a shorter policy" and "a policy
 * the browser silently drops", and an operator setting 3600 deserves to be told before saving.
 */
export const MIN_HSTS_MAX_AGE = 15_768_000;

/* ---------------------------------------------------------------------------------------------
 * Backups (REQ-013)
 * ------------------------------------------------------------------------------------------- */

/** One part of a run, as the detail screen's table reads it. */
export interface BackupPart {
  /** Which part: database, media, configuration, themes or plugins. */
  part: string;
  /** queued, running, done or failed. */
  status: string;
  /** Things accounted for — rows for `database`, objects for `media`. */
  item_count: number;
  /** Bytes the artifact occupies. */
  size_bytes: number;
  /** Hex SHA-256 of the artifact. */
  checksum: string | null;
  /** The artifact's key inside the run's prefix. */
  storage_path: string | null;
  /** Why it failed. */
  error: string | null;
  /** Whether the restore wizard may offer this part. */
  restorable: boolean;
}

/** One run, as the list and the detail screen read it. */
export interface BackupRun {
  /** Run id. */
  id: string;
  /** Tenant it belongs to. */
  organization_id: string | null;
  /** Operator's label; may be empty. */
  label: string;
  /** manual or scheduled. */
  kind: string;
  /** The schedule that started it. */
  schedule_id: string | null;
  /** The parts it was asked for. */
  scopes: string[];
  /** queued, running, succeeded, partial or failed. */
  status: string;
  /** Sum of its parts' sizes. */
  size_bytes: number;
  /** local or s3. */
  destination: string;
  /** Prefix its artifacts live under. */
  storage_prefix: string;
  /** SHA-256 over its manifest. */
  checksum: string | null;
  /** Whether the prune sweep leaves it alone. */
  protected: boolean;
  /** When the prune sweep may remove it. */
  retain_until: string | null;
  /** Why it failed. */
  error: string | null;
  /** Who started it. */
  created_by: string | null;
  /** When it was asked for. */
  created_at: string;
  /** When it began producing. */
  started_at: string | null;
  /** When it stopped producing. */
  finished_at: string | null;
  /** The label, or the created instant when there is no label. */
  title: string;
}

/** How many runs are in each state — the filter chips and the status cards read the same object. */
export interface BackupStatusCounts {
  /** Waiting to start. */
  queued: number;
  /** In flight. */
  running: number;
  /** Every part produced its artifact. */
  succeeded: number;
  /** Some parts produced theirs, some did not. */
  partial: number;
  /** No part produced its artifact. */
  failed: number;
}

/** A destination's state, with the probe's verdict. */
export interface BackupDestination {
  /** local or s3. */
  kind: string;
  /** The absolute root, for a local destination. */
  local_root: string;
  /** The bucket prefix, for an s3 destination. */
  s3_prefix: string | null;
  /** A secret-store reference — never a value. */
  credential_ref: string | null;
  /** Whether the last probe passed. */
  writable: boolean;
  /** The operating system's reason when it did not. */
  reason: string;
  /** The line the screen shows under the probe result. */
  message: string;
  /** none or passphrase. */
  encryption: string;
  /** Room left on the destination — a different fact from `writable`. */
  headroom: BackupHeadroom;
}

/**
 * Whether the next backup fits on the destination.
 *
 * `healthy` | `tight` | `full` | `unknown`. `unknown` is a real state, not an absence: a
 * destination nobody has measured yet has a free-space number but no yardstick to judge it
 * against, and printing `0 B` there would be a number the operator enlarges a disk over.
 */
export type BackupHeadroomLevel = "healthy" | "tight" | "full" | "unknown";

/** A destination's room, and the two numbers its verdict was judged from. */
export interface BackupHeadroom {
  /** The verdict. */
  level: BackupHeadroomLevel;
  /** Free bytes on the destination's filesystem, or null when the kernel would not say. */
  free_bytes: number | null;
  /** The largest backup this tenant holds there — the yardstick. */
  largest_backup_bytes: number | null;
  /** The sentence the card shows under the numbers. */
  message: string;
}

/** The four cards at the top of the overview. */
export interface BackupStatus {
  /** When the last run that produced artifacts finished. */
  last_successful_at: string | null;
  /** That run's id, so the card links to a specific row. */
  last_successful_id: string | null;
  /** How long ago that was, in seconds. */
  last_successful_age_seconds: number | null;
  /** Total bytes this tenant's backups occupy. */
  total_size_bytes: number;
  /** The counts behind the filter chips. */
  counts: BackupStatusCounts;
  /** How many backups the prune sweep will never remove. */
  protected: number;
  /** The nearest schedule that is due. */
  next_scheduled_at: string | null;
  /** The destination's health. */
  destination: BackupDestination;
}

/** A page of runs. */
export interface BackupList {
  /** The page's rows. */
  items: BackupRun[];
  /** How many rows the filters match in total. */
  total: number;
  /** The counts behind the chips. */
  counts: BackupStatusCounts;
}

/** A run's detail: the row, its parts and its manifest. */
export interface BackupDetail {
  /** The run. */
  backup: BackupRun;
  /** Its parts, in execution order. */
  parts: BackupPart[];
  /** Its manifest, as stored. */
  manifest: unknown;
}

/** What a verification pass found. */
export interface BackupVerification {
  /** The run that was verified. */
  backup_id: string;
  /** Whether every part matched. */
  clean: boolean;
  /** Parts whose recorded checksum and size both match. */
  matched: string[];
  /** Parts whose recorded checksum does not match. */
  mismatched: string[];
  /** Parts that could not be read back at all. */
  unreadable: string[];
  /** Artifacts the run never asked for. */
  unexpected: string[];
  /** One sentence naming what is wrong rather than only that something is. */
  summary: string;
}

/** One destination entry a delete could not remove. */
export interface BackupPurgeFailure {
  /** The path as the operating system named it. */
  path: string;
  /** The operating system's own words — `permission denied (os error 13)`. */
  reason: string;
}

/**
 * What removing a run actually did on the destination.
 *
 * The row being gone and the bytes being gone are two separate facts, and the screen says so
 * rather than collapsing them: a `204` would render "removed" over a directory that is still
 * full of the platform's media library.
 */
export interface BackupPurge {
  /** The directory that was targeted, in full. */
  root: string;
  /** Whether the run's directory existed at all before the delete. */
  existed: boolean;
  /** How many filesystem entries were removed, at any depth. */
  removed_entries: number;
  /** How many could not be removed and are still on the destination. */
  failed_entries: number;
  /** The first few failures, with the operating system's own words. */
  failures: BackupPurgeFailure[];
}

/**
 * One artifact the retention sweep could not remove.
 *
 * Its own type rather than a formatted string because the screen shows the path and the
 * operating system's reason in two different places, and a string that gets split back into
 * two is a string that will be split wrong.
 */
export interface BackupStrandedArtifact {
  /** The run whose bytes are still on the destination. */
  backup_id: string;
  /** Where the run's directory is. */
  path: string;
  /** The operating system's own words. */
  reason: string;
}

/** How loudly a restore warning is. `danger` is drawn as a refusal, not a decoration. */
export type RestoreWarningSeverity = "notice" | "caution" | "danger";

/** The machine-readable warning kinds, so the UI can react to one and the audit can query it. */
export type RestoreWarningCode =
  | "stale_archive"
  | "not_the_newest"
  | "data_loss"
  | "part_unavailable"
  | "run_incomplete"
  | "manifest_version"
  | "passphrase_required";

/** One thing an operator must know before restoring. */
export interface RestoreWarning {
  severity: RestoreWarningSeverity;
  code: RestoreWarningCode;
  message: string;
}

/** What one archived part does to live data when it is restored. */
export type RestoreMode = "replace" | "merge" | "advisory";

/** One part of an archive, as the wizard renders it. */
export interface RestorablePart {
  part: string;
  /** Whether the artifact was re-read and agrees with the manifest. */
  available: boolean;
  /** Why it is not available, in the store's words, when it is not. */
  reason: string | null;
  item_count: number;
  size_bytes: number;
  checksum: string | null;
  /** Live rows or objects this part would overwrite. */
  live_matches: number;
  /** Live rows or objects this part would drop, because they are not in the archive. */
  live_dropped: number;
  mode: RestoreMode;
}

/**
 * What a restore of one run would do.
 *
 * `total_live_dropped` is the number the whole screen exists to show: how much the operator
 * loses by choosing this restore point. It is not derivable from the manifest, which is why
 * the preview re-reads the destination and counts the live side rather than rendering the
 * archive's own numbers.
 */
export interface RestorePreview {
  backup_id: string;
  label: string;
  finished_at: string | null;
  age_days: number;
  parts: RestorablePart[];
  warnings: RestoreWarning[];
  restorable_bytes: number;
  total_live_dropped: number;
  total_live_matches: number;
  /** The phrase the operator must type; empty when nothing is restorable. */
  confirm_phrase: string;
  restorable: boolean;
}

/** One object a media restore could not write back. */
export interface RestoreFailure {
  storage_key: string;
  /** The key inside the archive, for a hand-check on the destination. */
  archive_key: string;
  /** What went wrong, in the store's own words. */
  reason: string;
}

/**
 * What the media half of a restore did.
 *
 * `objects_failed` is a separate field from `objects_restored` and not a derived state: a
 * restore that wrote three of four objects is neither a success nor a failure, and the screen
 * has to say which four.
 */
export interface MediaRestoreReport {
  objects_restored: number;
  rows_touched: number;
  bytes_restored: number;
  objects_failed: number;
  /** The named failures; the list is capped and the count is not. */
  failures: RestoreFailure[];
  /** Live items the restore removed. Always zero — the preview priced that separately. */
  dropped: number;
}

/** What a restore did, as the result panel renders it. */
/**
 * A queued restore (REQ-013, slice 2c).
 *
 * `cancellable` is a field rather than a `status === "queued"` derivation the panel makes.
 * "Can I still stop this" is the only question an operator is asking when they look at a
 * restore, and a panel that re-derives it from a status list is one edit away from offering a
 * "stop" button on a restore that has already taken its safety backup — which would discard
 * the one thing the operator was told they had.
 */
export interface RestoreJob {
  id: string;
  backup_id: string;
  parts: string[];
  status: "queued" | "running" | "succeeded" | "failed" | "aborted";
  /** Whether an abort is still possible. Only ever true while `status` is `queued`. */
  cancellable: boolean;
  cancel_requested: boolean;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  /** The protected run to go back to. Present on every `succeeded` job. */
  safety_backup_id: string | null;
  /** The loss the operator agreed to, carried from the preview they read. */
  live_dropped: number;
  live_matches: number;
  result: RestoreOutcome | null;
  error: string | null;
}

export interface RestoreOutcome {
  backup_id: string;
  /** The parts considered, in manifest order. */
  parts: string[];
  /** The parts actually performed. A part in `parts` and not here was recorded, not applied. */
  restored: string[];
  media: MediaRestoreReport;
  /** The protected run to go back to. Always present. */
  safety_backup_id: string;
  live_dropped: number;
  live_matches: number;
  summary: string;
}

/**
 * What one retention sweep did.
 *
 * `removed`, `partial` and `stranded` are three different facts and the screen says all
 * three: "pruned 4" and "3 of those 4 had a stuck file" are not the same sentence, and a
 * screen that renders only the first one is the sentence the delete route stopped saying a
 * tick ago — over a destination nobody is watching.
 */
export interface BackupSweepReport {
  /** Tenants the sweep walked. */
  walked: number;
  /** Runs the exemptions offered to the sweep. */
  candidates: number;
  /** Runs whose artifacts were completely removed. */
  removed: number;
  /** Runs whose row is gone but whose artifacts could not all be removed. */
  partial: number;
  /** Tenants whose sweep failed outright. */
  failed: number;
  /** Every artifact the sweep could not take, in the store's own words. */
  stranded: BackupStrandedArtifact[];
  /** When the sweep ran, in UTC. */
  at: string;
}

/** The result of taking a backup. */
export interface BackupCreateResult {
  /** The finished run. */
  backup: BackupRun;
  /** Its parts, with the state each reached. */
  parts: BackupPart[];
}

/** The settings record. */
export interface BackupSettings {
  /** local or s3. */
  destination: string;
  /** Absolute root. */
  local_root: string;
  /** Bucket prefix. */
  s3_prefix: string | null;
  /** A secret-store reference — never a value. */
  credential_ref: string | null;
  /** none or passphrase. */
  encryption: string;
  /** Default retention for a new schedule. */
  default_retention: number;
  /** Whether a run re-reads its own artifacts. */
  verify_after_backup: boolean;
  /** When it was last saved. */
  updated_at: string;
}

/** A recurring backup definition. */
export interface BackupSchedule {
  /** Row id. */
  id: string;
  /** Tenant it belongs to. */
  organization_id: string | null;
  /** Display name. */
  name: string;
  /** hourly, daily, weekly or monthly. */
  frequency: string;
  /** Time of day, for everything but hourly. */
  at_time: string | null;
  /** Weekday, for weekly only. */
  day_of_week: number | null;
  /** Day of the month, for monthly only. */
  day_of_month: number | null;
  /** Timezone the schedule is computed in. */
  timezone: string;
  /** The parts it produces. */
  scopes: string[];
  /** How many of its own runs it keeps. */
  retention_count: number;
  /** Destination. */
  destination: string;
  /** Whether the worker acts on it. */
  enabled: boolean;
  /** When it last ran. */
  last_run_at: string | null;
  /** When it next runs. */
  next_run_at: string | null;
  /** The run it produced last. */
  last_backup_id: string | null;
  /** What the screen says the frequency means, in one sentence. */
  cadence: string;
}

// ---------------------------------------------------------------------------------------------
// Content API tokens (REQ-019, slice 1)
// ---------------------------------------------------------------------------------------------

/**
 * A content API token as the list renders it.
 *
 * There is deliberately no field a secret could hide in: the row is the server's `TokenBody`, and
 * that struct cannot hold one either. A `secret?: string` here would be `undefined` forever and
 * would teach the next person that this shape is where a secret goes.
 */
export type ContentApiToken = {
  id: string;
  name: string;
  /** The copyable `omn_xxxxxxxx` marker — the only part of the credential that is ever shown. */
  prefix: string;
  /** `null` means every site of the organization. */
  site_id: string | null;
  /** The site's key when scoped, for a column a person can read. */
  site_key: string | null;
  scopes: string[];
  allowed_origins: string[];
  rate_limit_per_minute: number;
  expires_at: string | null;
  revoked_at: string | null;
  last_used_at: string | null;
  created_at: string;
  /** `active`, `expired` or `revoked`. Derived by the server from the two columns above. */
  status: "active" | "expired" | "revoked";
};

/** The create/rotate response: the row plus the one copy of the plaintext. */
export type CreatedContentApiToken = {
  token: ContentApiToken;
  /**
   * `omn_<prefix>_<secret>`, shown exactly once. It is not fetchable afterwards — the API stores
   * only its digest — so the screen that renders it must not offer a "show again".
   */
  plaintext: string;
  /** Always `true`; the client renders the warning from the payload rather than from a guess. */
  plaintext_shown_once: boolean;
};

/** One expiry choice the create dialog offers. */
export type ExpiryPreset = {
  label: string;
  /** `0` means "never". */
  days: number;
};

/** One rate-limit tier. */
export type RateTier = {
  label: string;
  per_minute: number;
  /** Whether the tier needs a permission the other one does not. */
  elevated: boolean;
};

/** A scope that exists in the store but is not implemented in v1. */
export type ReservedScope = {
  scope: string;
  /** Why it is not live, in the panel's own voice. */
  note: string;
};

/** Everything the create dialog may offer, read from the server rather than hard-coded. */
export type ContentApiVocabulary = {
  /** The scopes v1 offers. */
  scopes: string[];
  /** Reserved by name, so a future write surface needs no migration. */
  reserved_scopes: ReservedScope[];
  expiry_presets: ExpiryPreset[];
  rate_tiers: RateTier[];
  max_name_length: number;
};

/**
 * `GET /api/v1/content-api/usage` — the Usage tab's whole answer (REQ-019, slice 3).
 *
 * Three summaries over two sources (the durable table and the live Redis window), and the split is
 * carried on the wire rather than resolved by the panel. `rows` is raw; `series`, `tokens` and
 * `endpoints` are the server's roll-ups, and the panel renders those rather than computing its own
 * — a client-side sum is a second answer to "how much did this token do", and the second answer is
 * the one that disagrees with the usage route's own numbers.
 */
export type ContentApiUsage = {
  /** Days the answer covers. The server clamps a larger ask rather than refusing it. */
  days: number;
  /** The durable rows, newest day first — the raw material behind every roll-up above. */
  rows: ContentApiUsageRow[];
  /** Per-token totals, the table under the chart. */
  tokens: ContentApiUsageToken[];
  /** What the traffic is made of, busiest first. */
  endpoints: ContentApiUsageEndpoint[];
  /** One bar per day, oldest first, zero-filled so a quiet day is a short bar and not a gap. */
  series: ContentApiUsageDay[];
  /**
   * Whether the live window could be read at all.
   *
   * `false` means `pending_requests` is `null` — **not** `0`. A panel that rendered it as zero would
   * be telling an operator nothing is in flight while the counter is unreachable.
   */
  pending_readable: boolean;
  /** Requests counted but not yet flushed, or `null` when the window could not be read. */
  pending_requests: number | null;
  /** Refusals in the live window, or `null` when the window could not be read. */
  pending_throttled: number | null;
};

/** One durable row. `endpoint` is the matched route, with no `/api/v1` prefix. */
export type ContentApiUsageRow = {
  token_id: string;
  /** ISO day, `YYYY-MM-DD`. */
  day: string;
  endpoint: string;
  requests: number;
  errors: number;
  throttled: number;
};

/** One token's line in the usage table. */
export type ContentApiUsageToken = {
  token_id: string;
  name: string;
  flushed_requests: number;
  flushed_errors: number;
  flushed_throttled: number;
  /** Counted in the live window, not yet in the table. */
  pending_requests: number;
  /** Counted in the live window, not yet in the table. */
  pending_throttled: number;
  /** `null` when the token has never authenticated. */
  last_used_at: string | null;
};

/** One line of the endpoint leaderboard. */
export type ContentApiUsageEndpoint = {
  endpoint: string;
  /** Flushed **plus** pending, so it is comparable with the per-token total. */
  requests: number;
  throttled: number;
  /** How many of `requests` are already durable. `requests - flushed_requests` is still counting. */
  flushed_requests: number;
};

/** One bar of the chart. */
export type ContentApiUsageDay = {
  /** ISO day, `YYYY-MM-DD`. */
  day: string;
  requests: number;
  throttled: number;
};

/**
 * One documented operation, as the Docs tab reads it.
 *
 * A *narrowed* view of the OpenAPI document rather than the document itself typed out. The panel
 * indexes into a handful of well-known keys, and a `Record<string, unknown>` for everything else
 * would let a rename in the document turn into `undefined` at render time rather than a type
 * error. The tests assert the server's document actually has these keys.
 */
export type OpenApiOperation = {
  /** Stable id — also the Explorer's dropdown key (`pages.list`). */
  operationId: string;
  /** One line written for somebody integrating. */
  summary: string;
  /** The scope the token needs, or `null` for a route that needs only a valid token. */
  "x-required-scope": string | null;
  /** `posts` is a page type today; the blog module will give it its own fields. */
  "x-experimental"?: boolean;
  parameters: OpenApiParameter[];
  responses: Record<string, { description: string }>;
};

/** One query (or path, or header) parameter. */
export type OpenApiParameter = {
  name: string;
  /** `query`, `path` or `header`. */
  in: string;
  required: boolean;
  description: string;
};

/**
 * The document the Docs tab renders.
 *
 * Only the keys the screen reads are typed; `paths` is indexed by the path string and the method
 * beside it, because that is exactly the document's own shape and pretending otherwise would mean
 * a second data model that has to be kept in step.
 */
export type OpenApiDocument = {
  openapi: string;
  info: {
    title: string;
    version: string;
    description: string;
    license?: { name: string };
  };
  servers: { url: string }[];
  paths: Record<string, Record<string, OpenApiOperation>>;
  components: {
    securitySchemes: Record<string, { type: string; scheme?: string; description?: string }>;
    schemas: Record<string, { description?: string; required?: string[]; properties?: Record<string, unknown> }>;
  };
};

/** One response header the Explorer pane shows, in the order the platform considers load-bearing. */
export type ExplorerHeader = {
  /** Header name, lower-case, as sent. */
  name: string;
  /** Header value. */
  value: string;
};

/** The call rendered as a caller would write it, in three languages. */
export type ExplorerSnippets = {
  curl: string;
  fetch: string;
  python: string;
};

/**
 * Which token made the call — and never any part of its secret.
 *
 * There is deliberately no field a plaintext could hide in. The platform stores only a digest, so
 * a type with a `plaintext?: string` would be `undefined` forever and would teach the next person
 * that this is where a credential goes.
 */
export type ExplorerToken = {
  id: string;
  name: string;
  /** The `omn_xxxxxxxx` marker — the only part that is ever displayed. */
  prefix: string;
  /** Requests-per-minute tier, so a refusal can be compared with the budget. */
  rate_limit_per_minute: number;
  /**
   * Budget left in this minute after the call, or `null` when the counter could not be read.
   *
   * `null` is "we do not know", which is not `0` — the meter fails open, and a `0` here would tell
   * an operator their token is spent when the platform never counted anything.
   */
  remaining: number | null;
};

/**
 * `POST /api/v1/content-api/explorer` — one real call, made as a chosen token (REQ-019, slice 3).
 *
 * The **server's** answer, not a reconstruction of it. The status, the headers, the body and the
 * `duration_ms` all come from the handler that serves an integrator, so a reader comparing the
 * pane against their own integration is comparing the same two answers.
 */
export type ExplorerAnswer = {
  /** The documented operation that was called. */
  operation_id: string;
  /** Its HTTP method, upper-case, from the document. */
  method: string;
  /** The resolved request URL, exactly as dispatched. */
  url: string;
  /**
   * The route the call was **metered** against (`/content/pages`).
   *
   * The template, not the resolved path: one endpoint is one row on the Usage tab's leaderboard
   * however many slugs were walked, and this is the field that makes the two screens reconcilable.
   */
  metered_route: string;
  /** Status the read surface answered. */
  status: number;
  /** The headers a caller branches on. */
  headers: ExplorerHeader[];
  /** The parsed body, or the raw text as a string when the route did not answer JSON. */
  body: unknown;
  /** Whether `body` is the parsed document rather than a string. */
  body_is_json: boolean;
  /** Wall-clock milliseconds the dispatch took. */
  duration_ms: number;
  /**
   * `next_cursor` lifted out of a list response, or `null`.
   *
   * Lifted by the server rather than read out of `body` in the browser, because the pane renders
   * the body and the "next page" button from the same response and two reads of one response can
   * disagree about what it said.
   */
  next_cursor: string | null;
  /** The token that acted. */
  token: ExplorerToken;
  /** The call in three languages. */
  snippets: ExplorerSnippets;
};

/** One endpoint as the Explorer's picker offers it. */
export type ExplorerEndpoint = {
  /** The document's `operationId`; also the deep link's `endpoint` value. */
  operationId: string;
  /** Upper-case method badge. */
  method: string;
  /** The path template, shown so the reader knows what they are about to call. */
  path: string;
  /** One line, straight from the document. */
  summary: string;
  /** The scope a token needs, or `null` for a route that needs only a valid token. */
  requiredScope: string | null;
  /** `posts` is a page type today; the blog module will give it fields of its own. */
  experimental: boolean;
  /** Declared query parameters, in document order, with their descriptions. */
  query: { name: string; description: string }[];
  /** Declared path parameters — always required, and rendered as such. */
  pathParams: { name: string; description: string }[];
};

// -------------------------------------------------------------------------------------------
// System health (REQ-014).
//
// The four states are a closed set on the server and here, and the reason the client
// repeats the list instead of typing `state: string` is the same one the server closes it
// for: a colour map keyed by a string is a map that renders `undefined` in a class
// attribute the first time a probe learns a fifth word. The badge, the label and the icon
// all come out of one record, so a state can never have a colour and no label.
// -------------------------------------------------------------------------------------------

/** The four words a service state can be. `unknown` is NOT "fine". */
export type HealthState = "healthy" | "degraded" | "down" | "unknown";

/** One check inside a service row. */
export type HealthCheck = {
  check: string;
  state: string;
  message: string;
  latency_ms: number;
};

/** One service row. Always present for every registered service, probed or not. */
export type HealthService = {
  service: string;
  state: HealthState;
  description: string;
  latency_ms: number | null;
  checked_at: string | null;
  message: string;
  detail: Record<string, unknown>;
  checks: HealthCheck[];
  href: string;
};

/** One host metric card. `threshold: null` means no opinion is configured. */
export type HealthHostMetric = {
  metric: string;
  value: number;
  unit: string;
  state: string;
  threshold: number | null;
};

/** The overview's verdict, as a word and as a sentence. */
export type HealthBanner = {
  state: HealthState;
  headline: string;
  worst_service: string | null;
};

/** `GET /api/v1/health/overview`. */
export type HealthOverview = {
  services: HealthService[];
  host: HealthHostMetric[];
  banner: HealthBanner;
  counts: Record<HealthState, number>;
  last_checked_at: string | null;
  registry: string[];
  sample_count: number;
};

/** One metric of one service, with the newest value it published and its 24 h trend. */
export type HealthServiceMetric = {
  metric: string;
  value: number;
  unit: string;
  sampled_at: string;
  /**
   * The metric's values over the last 24 h, oldest first.
   *
   * Empty when the window holds no samples — and empty is a real answer here, because
   * a platform whose history was pruned or never recorded is genuinely unknown, not zero.
   * The screen draws a dot for one point and a line for two or more, so this array is the
   * only thing separating a real trend from an empty box.
   */
  series: number[];
};

/** `GET /api/v1/health/services/{key}`. */
export type HealthServiceDetail = HealthService & { metrics: HealthServiceMetric[] };

/** `GET /api/v1/health/summary` — the one line other centres embed. */
export type HealthSummary = {
  state: HealthState;
  headline: string;
  worst_service: string | null;
  /** `true` only when every registered service is healthy. Never true while any is unprobed. */
  operational: boolean;
  counts: Record<HealthState, number>;
};

/** One point of a metric's series, oldest first. */
export type HealthSamplePoint = {
  value: number;
  unit: string;
  state: string;
  sampled_at: string;
};

/** What the retention prune deleted. */
export type HealthPruneResult = { deleted: number; retention_days: number };

/**
 * The three windows the metric table offers.
 *
 * A name rather than an hour count, so the label on screen, the label in the CSV filename and
 * the window the server queried are the same string. Anything else is refused server-side.
 */
export type HealthRangeKey = "1h" | "24h" | "7d";

/** One row of `GET /api/v1/health/metrics`. */
export type HealthMetricRow = {
  service: string;
  metric: string;
  unit: string;
  samples: number;
  /** `null` on a window with no samples — never `0`, which is a value. */
  current: number | null;
  min: number | null;
  avg: number | null;
  max: number | null;
  state: string;
  last_sample_at: string | null;
  /** The window's values, oldest first. Empty when there are no samples. */
  series: number[];
};

/** `GET /api/v1/health/metrics` — the aggregated table for one range. */
export type HealthMetricsReport = {
  range: string;
  ranges: string[];
  metrics: HealthMetricRow[];
  total_samples: number;
};

// ---------------------------------------------------------------------------------------------
// Incidents and threshold policy (REQ-014, slice 3)
// ---------------------------------------------------------------------------------------------

/** One row of `GET /api/v1/health/incidents`. */
export type HealthIncident = {
  id: string;
  service: string;
  from_state: string;
  to_state: string;
  summary: string;
  detail: Record<string, unknown>;
  started_at: string;
  /** `null` while the incident is open. */
  resolved_at: string | null;
  /** Seconds between open and resolve, or `null` while it is still open. */
  duration_seconds: number | null;
  /** True when a maintenance window covered the moment it opened. */
  suppressed: boolean;
  acknowledged_by: string | null;
  acknowledged_at: string | null;
  note: string | null;
};

/** `GET /api/v1/health/incidents` — a page plus the count behind the filter. */
export type HealthIncidentPage = {
  incidents: HealthIncident[];
  total: number;
  /**
   * Every service the platform probes.
   *
   * Sent with the list rather than fetched separately so the filter dropdown cannot offer a
   * value the server would answer `unknown_service` for — the vocabulary and the filter
   * options are the same read.
   */
  services: string[];
};

/** One threshold row on the settings form. */
export type HealthThreshold = {
  metric: string;
  warn: number;
  crit: number;
  direction: "above" | "below";
  unit: string;
  /**
   * False when the numbers are the **suggestion** rather than something an operator saved.
   *
   * The form marks these, because a placeholder that looks like a saved value is a limit
   * nobody chose being read as a limit they chose.
   */
  configured: boolean;
};

/** One maintenance window. */
export type HealthMaintenanceWindow = {
  id: string;
  starts_at: string;
  ends_at: string;
  services: string[];
  note: string;
  created_by: string | null;
  created_at: string;
  /** True when `now` is inside the window — the only flag a row can have. */
  active: boolean;
};

/** `GET`/`PUT /api/v1/health/settings`. */
export type HealthSettings = {
  check_interval_seconds: number;
  worker_stale_seconds: number;
  thresholds: HealthThreshold[];
  notifications: Record<string, boolean>;
  updated_by: string | null;
  updated_at: string;
  /** The inclusive bounds the form enforces, so the UI and the API agree. */
  bounds: {
    check_interval_seconds: [number, number];
    worker_stale_seconds: [number, number];
  };
  /** How many breaches the ledger holds, resolved ones included. */
  breaches: number;
};

/** What `PATCH /health/incidents/{id}` accepts. */
export type HealthIncidentAction = "acknowledge" | "resolve";
