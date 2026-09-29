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
