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
  restored_from_id: string | null;
  created_at: string;
  published_at: string | null;
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
  metadata?: string;
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
// CDN / edge (REQ-011)
// ---------------------------------------------------------------------------------------------

/** Which request parts form a rule's cache key. */
export type CdnCacheKey = {
  host: boolean;
  path: boolean;
  /** Query parameters the key keeps; everything else is dropped from it. */
  query_include: string[];
  /** Vary on the language cookie, so one language's page is never served to another. */
  language_cookie: boolean;
};

/** What forces a rule to hand the request to the origin. */
export type CdnBypass = {
  cookie_names: string[];
  query_params: string[];
  header_names: string[];
};

/** One cache rule, in the shape `/api/v1/cdn/rules` returns it. */
export type CdnCacheRule = {
  id: string;
  site_id: string;
  name: string;
  /** Lower wins. The panel table is ordered by this. */
  priority: number;
  path_pattern: string;
  methods: string[];
  edge_ttl_seconds: number;
  browser_ttl_seconds: number;
  swr_seconds: number;
  cache_key: CdnCacheKey;
  bypass: CdnBypass;
  enabled: boolean;
  created_by: string | null;
  created_at: string;
  updated_at: string;
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

/** What the rule form sends. Every field is optional and folded onto the stored row. */
export type CdnCacheRuleInput = {
  site_id: string;
  name: string;
  path_pattern: string;
  priority?: number;
  methods?: string[];
  edge_ttl_seconds?: number;
  browser_ttl_seconds?: number;
  swr_seconds?: number;
  cache_key?: Partial<CdnCacheKey>;
  bypass?: Partial<CdnBypass>;
  enabled?: boolean;
};

/**
 * A stored rule that no longer compiles.
 *
 * Surfaced rather than hidden: a rule that silently stopped matching is the failure an
 * operator finds days later from a page that is still serving the old revision.
 */
export type CdnUnreadableRule = {
  id: string;
  reason: string;
};

export type CdnRulesResponse = {
  rules: CdnCacheRule[];
  unreadable: CdnUnreadableRule[];
};

/**
 * The CDN settings row for one site.
 *
 * There is no credential field, and that is the design rather than an omission: the API
 * never returns the stored value, so the panel can only say whether one is *present* and
 * offer to replace it.
 */
export type CdnSettings = {
  site_id: string | null;
  provider: string;
  endpoint_url: string | null;
  zone_ref: string | null;
  /** Whether a credential is stored. The value itself is write-only. */
  has_credential: boolean;
  /** Trigger event name -> enabled. */
  auto_purge: Record<string, boolean>;
  batch_size: number;
  max_attempts: number;
  updated_at: string;
};

/** What the settings form sends. `credential` is write-only and omitted when unchanged. */
export type CdnSettingsInput = {
  site_id: string | null;
  provider: string;
  endpoint_url?: string | null;
  zone_ref?: string | null;
  credential?: string | null;
  auto_purge?: Record<string, boolean>;
  batch_size?: number;
  max_attempts?: number;
};

/**
 * One shipped provider adapter, as the catalogue describes it.
 *
 * The list contains only adapters that are actually implemented: an adapter that is listed
 * but not shipped is a dead button, and the whole point of the catalogue is that choosing
 * from it is a real choice.
 */
export type CdnAdapterInfo = {
  key: string;
  label: string;
  /** One line saying what the adapter actually does, in the adapter's own terms. */
  description: string;
  needs_endpoint: boolean;
  needs_zone: boolean;
  needs_credential: boolean;
};

/**
 * A CDN adapter as `/api/v1/cdn/adapters` returns it.
 *
 * The two capability flags are read from a throwaway adapter instance on the server rather
 * than from the catalogue table, so a listed capability and a shipped one cannot drift. The
 * panel uses them to disable a mode the adapter genuinely cannot run: offering "purge by
 * tag" to an adapter that cannot would queue work that fails at drain time.
 */
export type CdnAdapter = CdnAdapterInfo & {
  supports_tags: boolean;
  supports_purge_all: boolean;
  /** Every catalogue entry is shipped by construction; kept for a client that renders one. */
  shipped: boolean;
};

/** What a purge is for: absolute paths, surrogate keys, or the whole zone. */
export type CdnPurgeKind = "url" | "tag" | "all";

/**
 * Where a purge is in its life.
 *
 * `running` covers a purge with items still waiting out a retry backoff, which is why it is
 * not merged with `queued`: the operator's question is "is this moving?", and a purge whose
 * next attempt is 40 seconds away is moving.
 */
export type CdnPurgeStatus = "queued" | "running" | "succeeded" | "partial" | "failed";

/** One purge, as the history table and the drawer see it. */
export type CdnPurge = {
  id: string;
  /** `null` once the site this purge was for has been deleted. */
  site_id: string | null;
  kind: CdnPurgeKind;
  targets: string[];
  status: CdnPurgeStatus;
  /** The adapter that ran (or will run) it, captured when the purge was requested. */
  provider: string;
  item_count: number;
  failed_count: number;
  requested_by: string | null;
  requested_at: string;
  started_at: string | null;
  finished_at: string | null;
  /** The provider's own message, verbatim. Never a summary. */
  error: string | null;
  /**
   * Whether a retry has anything to do.
   *
   * Computed by the server so the panel and the API cannot disagree: a retry button on a
   * `succeeded` row is a dead button, and the request forbids those.
   */
  retryable: boolean;
};

/** One target inside a purge. */
export type CdnPurgeItem = {
  id: number;
  target: string;
  /** `pending` covers "waiting for its next attempt", not only "not started". */
  status: "pending" | "running" | "done" | "failed";
  attempts: number;
  next_attempt_at: string;
  response_status: number | null;
  error: string | null;
  done_at: string | null;
};

/** A purge plus its items — the detail drawer. */
export type CdnPurgeDetail = {
  purge: CdnPurge;
  items: CdnPurgeItem[];
  /**
   * The event behind an automatic purge (REQ-011 slice 3), or `null` for one a person
   * asked for. The absence is the signal, not a missing field.
   */
  source: CdnPurgeSource | null;
};

/** What the platform raised the purge for. */
export type CdnPurgeSource = {
  /** The bus event's id. */
  event_id: number;
  /** The event's name, e.g. `page.published`. */
  trigger: string;
};

/** A page of history. `total` is the unpaged count so the panel can say "50 of 312". */
export type CdnPurgePage = {
  purges: CdnPurge[];
  total: number;
  /** The console's own cap, so the form can count before submitting. */
  max_targets: number;
};

/** The filters the history table sends. Every one is optional and every one narrows. */
export type CdnPurgeFilters = {
  status?: CdnPurgeStatus | null;
  kind?: CdnPurgeKind | null;
  /** RFC 3339. */
  since?: string | null;
  /** RFC 3339. */
  until?: string | null;
  limit?: number | null;
  offset?: number | null;
};

/** What the console sends. `zone_confirmed` is the typed `PURGE`, as a fact. */
export type CdnPurgeInput = {
  site_id: string;
  kind: CdnPurgeKind;
  targets: string[];
  /** Required for `kind: "all"`; the form only sets it when the field literally says PURGE. */
  zone_confirmed?: boolean;
};

/** The overview's cards. */
export type CdnStatus = {
  provider: string;
  /** `false` when a settings row names an adapter this build does not ship. */
  provider_shipped: boolean;
  /** Items waiting to be attempted. */
  queue_depth: number;
  /** Purges that have not reached a terminal state. */
  open_purges: number;
  purges_24h: number;
  succeeded_24h: number;
  partial_24h: number;
  failed_24h: number;
  /**
   * Share of the window that did not fully succeed, as a percentage.
   *
   * `partial` counts towards it: a purge where twenty of forty targets went through leaves
   * a page still stale for some visitors, and a card reading 0% for that would be reporting
   * the wrong thing. A window with no purges is `0`, never `NaN`.
   */
  failure_rate: number;
  /** The last 20 purges, newest first. */
  recent: CdnPurge[];
};

/** The inline result under the settings screen's "Test connection" button. */
export type CdnProviderProbe = {
  ok: boolean;
  latency_ms: number;
  status: number | null;
  message: string;
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

// Security centre (REQ-012, slice 2) — the header policy
/**
 * Whether the CSP is enforced or only reported.
 *
 * The two are mutually exclusive on the wire: `report_only` sends
 * `Content-Security-Policy-Report-Only` and sends no enforcing header at all, because sending
 * both applies the policy while claiming to only report it.
 */
export type CspMode = "report_only" | "enforce";

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

// ---------------------------------------------------------------------------------------------
// Staging environments (REQ-017, slices 1 and 2)
// ---------------------------------------------------------------------------------------------

/** One area of a clone, with the count the panel shows next to its label. */
export type EnvironmentAreaProgress = {
  /** The wire name the API accepts back (`pages`, `translations`, …). */
  name: string;
  /** The label a person reads. */
  label: string;
  /** Rows expected. */
  total: number;
  /** Rows copied so far. */
  done: number;
};

/** A clone job as the list row and the Overview tab see it. */
export type EnvironmentCloneJob = {
  id: string;
  environment_id: string;
  /** `pending`, `running`, `done`, `failed` or `cancelled`. */
  status: string;
  areas: EnvironmentAreaProgress[];
  items_done: number;
  items_total: number;
  /**
   * Completion, 0–100.
   *
   * Zero while the total is still unknown, which is a *different state* from zero progress on a
   * known total. A panel that renders both as 0% shows a clone that is counting as one that is
   * stuck, and the operator's next click is a cancel on a job that was about to finish.
   */
  percent: number;
  summary: string;
  error: string | null;
  started_at: string | null;
  finished_at: string | null;
  created_by: string | null;
  created_at: string;
  /** Whether a cancel button should be offered, decided by the API rather than here. */
  cancellable: boolean;
};

/** What an environment currently holds, per area. */
export type EnvironmentContentCounts = {
  pages: number;
  translations: number;
  workflows: number;
  settings: number;
  revisions: number;
  total: number;
};

/** One environment. */
export type Environment = {
  id: string;
  organization_id: string;
  key: string;
  name: string;
  /** `production` or `staging`. */
  type: "production" | "staging";
  /** `active`, `cloning`, `error` or `archived`. */
  status: string;
  cloned_from_environment_id: string | null;
  cloned_at: string | null;
  staging_host: string | null;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  content: EnvironmentContentCounts;
  clone: EnvironmentCloneJob | null;
  /**
   * Whether the panel should offer a re-clone.
   *
   * Computed by the API, not by the panel: a button that appears on a row whose request would be
   * refused is a dead button with a worse name than "Clone".
   */
  reclonable: boolean;
};

/** One checkbox in the create wizard. */
export type EnvironmentAreaOption = {
  name: string;
  label: string;
  /** What it costs, in words rather than a fake number. */
  weight: string;
  /**
   * Whether ticking this actually copies anything.
   *
   * Three of the six areas do not copy: menus and site settings are one row per tenant, and the
   * theme is a column on the shared site. They used to render as ordinary checkboxes, so a tick
   * was accepted, priced in the estimate and then did nothing. `false` renders the area as a
   * disclosed boundary rather than a promise, and `note` is the reason.
   */
  copies: boolean;
  /** Why an area copies nothing. `null` for the ones that do. */
  note: string | null;
};

/** `GET /api/v1/environments`. */
export type EnvironmentListResponse = {
  environments: Environment[];
  total: number;
  areas: EnvironmentAreaOption[];
  /** The organization's production environment key — the wizard's "clone from" line. */
  source_key: string;
};

/** `GET /api/v1/environments/{id}`. */
export type EnvironmentDetailResponse = {
  environment: Environment;
  /** The job history, newest first. */
  jobs: EnvironmentCloneJob[];
  /** The estimate for the next clone, in words. */
  estimate: string;
};

/**
 * The list filters, which live in the URL's query string so a reload keeps them.
 *
 * `limit` is here and not in the URL: the panel owns its page size, and a page size an operator
 * can type into the address bar is a page size that will eventually be `10000`.
 */
export type EnvironmentFilters = {
  type?: "production" | "staging" | "";
  status?: string;
  search?: string;
  limit?: number;
};

/** `GET /api/v1/environments/{id}/changes` — what staging holds that production does not. */
export type ChangeSetResponse = {
  environment_id: string;
  environment_key: string;
  /** The environment this one was cloned from. */
  production_id: string;
  items: ChangeItem[];
  added: number;
  updated: number;
  deleted: number;
  /** `true` when nothing differs — the normal state of a fresh environment. */
  empty: boolean;
};

/** One row of the change set, as the Changes tab reads it. */
export type ChangeItem = {
  site_id: string;
  slug: string;
  /**
   * The page's id **in the environment that holds it** — staging's id for `added` and `updated`,
   * production's id for `deleted`. This is the id a promotion freezes and addresses items by, and
   * for a deletion it is the only id that exists.
   */
  page_id: string;
  /** `added`, `updated` or `deleted`. */
  kind: "added" | "updated" | "deleted";
  changed_by: string | null;
  changed_at: string;
  title: string | null;
};

/** One entry of a promotion's step log — the dialog's timeline. */
export type PromotionStep = {
  /** `validate`, `apply`, `audit` or `done`. */
  step: string;
  at: string;
  /** A phrase a person reads, written by the server. */
  detail: string;
};

/**
 * One promotion, as the Promotions tab and the dialog read it.
 *
 * The counts are the *frozen* set's counts, not the change set's live counts: a promotion is a
 * record of what was read at request time, and a panel that recounted them from staging would
 * describe a different deploy than the one that ran.
 */
export type Promotion = {
  id: string;
  environment_id: string;
  target_environment_id: string;
  /** `pending_approval`, `approved`, `running`, `done`, `failed` or `cancelled`. */
  status: string;
  item_count: number;
  /** How many items write a production row — deletions do not. */
  write_count: number;
  added: number;
  updated: number;
  deleted: number;
  /** The conflicting item ids, which the dialog leads with. */
  conflicts: string[];
  /**
   * Whether the operator must type the environment's name before the deploy.
   *
   * Decided by the server, above 25 items, so the panel and the promotion can never disagree
   * about the threshold — and a panel-side constant would drift the first time it moved.
   */
  requires_typed_confirmation: boolean;
  requested_by: string | null;
  approved_by: string | null;
  approved_at: string | null;
  steps: PromotionStep[];
  error: string | null;
  created_at: string;
  finished_at: string | null;
};

/** One frozen item, as the dialog's conflict list and the detail panel read it. */
export type FrozenItem = {
  page_id: string;
  site_id: string;
  slug: string;
  title: string | null;
  kind: "added" | "updated" | "deleted";
  /** Production's `updated_at` at request time. Absent for an `added` row. */
  base_updated_at: string | null;
  /** SHA-256 of production's published revision at request time. */
  base_digest: string;
};

/** The frozen change set, which is what runs. */
export type FrozenChangeSet = {
  environment_id: string;
  target_environment_id: string;
  items: FrozenItem[];
};

/** The answer to a promotion request: the record plus the set it froze, in one response. */
export type PromotionRequested = {
  promotion: Promotion;
  changes: FrozenChangeSet;
};

/** `GET /api/v1/promotions/{id}` — the record and its frozen set. */
export type PromotionDetail = PromotionRequested;

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

export type RateLimitsSaved = RateLimitsDocument & { change_id: number | null };

export type RateLimitTestRequest = {
  method: string;
  path: string;
  client_ip?: string | null;
  user_id?: string | null;
  /** The counter to assume — what makes the tester usable during a real incident. */
  count?: number | null;
  machine_key?: boolean;
};

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

export type LockoutPolicy = {
  window_seconds: number;
  attempts: number;
  lockout_minutes: number;
  progressive_delay: boolean;
  base_delay_seconds: number;
  reset_on_success: boolean;
};

export type LockoutBounds = {
  window_seconds: [number, number];
  attempts: [number, number];
  lockout_minutes: [number, number];
  base_delay_seconds: [number, number];
  max_delay_seconds: number;
};

export type SignInProtectionDocument = {
  policy: LockoutPolicy;
  locked_accounts: number;
  is_saved: boolean;
  bounds: LockoutBounds;
};

export type SignInProtectionSave = {
  policy: LockoutPolicy;
  expected_policy?: unknown;
};

export type SignInProtectionSaved = SignInProtectionDocument & { change_id: number | null };

export type LockedAccount = {
  user_id: string;
  email: string;
  locked_until: string;
  seconds_remaining: number;
  failed_sign_in_count: number;
};

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

/* ---------------------------------------------------------------------------------------------
 * The deployment centre (REQ-024, slice 1).
 *
 * `DeploymentAvailability` is the one shape worth reading twice. The API sends the card's second
 * line three ways — an upgrade on offer, nothing newer, or a newer release that may NOT be
 * offered and the reason why — and the panel renders all three. A single `available: string`
 * would make the third case a sentence the panel has to parse, and the panel parsing a sentence
 * is how "2.6.0 — needs core 2.6 or newer" ends up on a card as just "2.6.0".
 * ---------------------------------------------------------------------------------------------
 */

/** What the card's `Available` line says, as the three states it can be in. */
export type DeploymentAvailability =
  | { state: "upgrade"; version: string; breaking: boolean }
  | { state: "up-to-date" }
  | { state: "blocked"; candidate: string; reason: string };

/** `GET /api/v1/deployment/version`. */
export type DeploymentVersion = {
  version: string;
  service: string;
  channel: string;
  /** Whether the API understood the configured channel string. */
  channel_understood: boolean;
  core: string | null;
  /** The rendered line. Never an empty string. */
  available: string;
  availability: DeploymentAvailability;
  upgrade_available: boolean;
};

/** The version block, repeated on several responses so the panel never re-derives it. */
export type DeploymentVersionSummary = {
  current: string;
  available: string;
  availability: DeploymentAvailability;
  /** The cached-data banner, or null. Rendered verbatim — the wording is a product surface. */
  stale_banner: string | null;
};

/** What a card offers as a rollback. */
export type DeploymentRollbackOffer = {
  to_version: string;
  /** Whether that version is in the cache with notes. */
  known: boolean;
};

/** One environment card (`GET /api/v1/deployment/environments`). */
export type DeploymentEnvironmentCard = {
  environment: string;
  name: string;
  /** `healthy`, `degraded` or `unreachable`. */
  health: string;
  version: string | null;
  checked_at: string | null;
  /** The failing probe, when degraded. The tooltip's second line. */
  failing_probe: string | null;
  available: string;
  availability: DeploymentAvailability;
  deployable: boolean;
  /** Why not, when it may not. A disabled button with no reason is a dead control. */
  blocked_reason: string | null;
  rollback: DeploymentRollbackOffer | null;
  last_deploy: DeploymentHistoryRow | null;
};

/* ── REQ-024 slice 2: the deploy wizard ─────────────────────────────────────────────────────── */

/** One pre-flight row. `state` is `pass`, `warn`, `fail` or `unknown`. */
export type DeploymentPreflightRow = {
  id: string;
  /** The check's title, so the panel does not hard-code the copy. */
  title: string;
  state: string;
  detail: string;
  /** Empty when there is nothing to suggest. */
  action: string | null;
  needs_acknowledgement: boolean;
};

/** `POST /api/v1/deployment/environments/{id}/preflight`. */
export type DeploymentPreflight = {
  checks: DeploymentPreflightRow[];
  blocked: boolean;
  can_continue: boolean;
  needs_acknowledgement: boolean;
  production: boolean;
  /** `type_version` for production, `none` elsewhere. */
  confirmation: string;
  requires_maintenance_window: boolean;
  /** Identifies this report; changes when a check's result changes. */
  token: string;
};

/** A deploy job, as the wizard's step 3 and the history expansion read it. */
export type DeploymentJob = {
  id: string;
  environment: string;
  kind: string;
  status: string;
  from_version: string | null;
  to_version: string | null;
  started_by: string | null;
  reason: string | null;
  started_at: string;
  finished_at: string | null;
  elapsed_ms: number | null;
  error: string | null;
  /** Whole percent, counting finished steps only. */
  progress_percent: number;
  cancellable: boolean;
  /** Why `Cancel` is refused, when it is. */
  cancel_refusal: string | null;
  steps: DeploymentStep[];
};

/** `GET /api/v1/deployment/jobs/{id}`. */
export type DeploymentJobResponse = { job: DeploymentJob };

/** `GET /api/v1/deployment/jobs/{id}/log?cursor=`. */
export type DeploymentLogChunk = {
  chunk: string;
  cursor: number;
  running: boolean;
  status: string;
};

/** `POST /api/v1/deployment/jobs/{id}/cancel`. */
export type DeploymentCancelResponse = {
  job: DeploymentJob;
  message: string;
};

/* ── REQ-024 slice 3: rollback and the maintenance window ─────────────────────────────────── */

/**
 * One environment's maintenance window.
 *
 * `active` is deliberately separate from `enabled`, and the two differ in the case that matters:
 * a window scheduled for tomorrow is `enabled` and not `active`. A screen that showed only the
 * toggle would tell an operator the promise is live when it is not — the one error this feature
 * cannot make, because the operator is the one who relies on it.
 */
export type DeploymentMaintenanceWindow = {
  environment: string;
  enabled: boolean;
  active: boolean;
  message: string;
  starts_at: string | null;
  ends_at: string | null;
  /** `all` or `admin`. */
  scope: string;
  updated_by: string | null;
  /** `null` until somebody has configured this environment at all. */
  updated_at: string | null;
  max_message_length: number;
};

/** `GET /api/v1/deployment/maintenance`. */
export type DeploymentMaintenanceResponse = {
  windows: DeploymentMaintenanceWindow[];
  /** Every window open right now, across all environments. What the shell banner renders. */
  active: DeploymentMaintenanceWindow[];
  max_message_length: number;
};

/** `POST /api/v1/deployment/environments/{environment}/rollback`. */
export type DeploymentRollbackResponse = {
  job: DeploymentJob;
  message: string;
};

/** `GET /api/v1/deployment/environments`. */
export type DeploymentEnvironmentsResponse = {
  environments: DeploymentEnvironmentCard[];
  summary: DeploymentVersionSummary;
};

/** `GET /api/v1/deployment/environments/{environment}`. */
export type DeploymentEnvironmentDetail = {
  environment: DeploymentEnvironmentCard;
  history: DeploymentHistoryRow[];
};

/** One release in the browser. */
export type DeploymentRelease = {
  version: string;
  channel: string;
  released_at: string | null;
  notes: string;
  breaking: boolean;
  migrations: string[];
  core_min: string | null;
  artifact_checksum: string | null;
  checked_at: string;
  is_available: boolean;
};

/** `GET /api/v1/deployment/releases`. */
export type DeploymentReleasesResponse = {
  channel: string;
  releases: DeploymentRelease[];
  stale_banner: string | null;
  /** True when the channel is genuinely empty, so the panel can say so in words. */
  empty: boolean;
};

/** `GET /api/v1/deployment/releases/{version}` — the `View Changes` screen. */
export type DeploymentReleaseDetail = {
  release: DeploymentRelease;
  summary: DeploymentVersionSummary;
  /** Set only when this release is the card's offer — an older version opens without a deploy. */
  upgrade_from: string | null;
};

/** One step of a run. */
export type DeploymentStep = {
  position: number;
  name: string;
  status: string;
  output: string;
  started_at: string | null;
  finished_at: string | null;
};

/** One history row. */
export type DeploymentHistoryRow = {
  id: string;
  environment: string;
  kind: string;
  from_version: string | null;
  to_version: string | null;
  status: string;
  strategy: string;
  started_by: string | null;
  reason: string | null;
  error: string | null;
  started_at: string;
  finished_at: string | null;
  duration_ms: number | null;
  steps: DeploymentStep[];
};

/** The filter the API applied, echoed. The chips read this rather than their own state. */
export type DeploymentHistoryFilter = {
  environment: string | null;
  kind: string | null;
  status: string | null;
  window: string;
};

/** `GET /api/v1/deployment/history`. */
export type DeploymentHistoryResponse = {
  rows: DeploymentHistoryRow[];
  total: number;
  filter: DeploymentHistoryFilter;
};

/** The filter chips the history screen offers. */
export type DeploymentHistoryFilters = {
  environment?: string;
  kind?: string;
  status?: string;
  window?: string;
  limit?: number;
  offset?: number;
};

/** One check row, in the wizard's own vocabulary. */
export type DeploymentCheckRow = {
  check: string;
  title: string;
  state: "pass" | "warn" | "fail" | "unknown";
  detail: string;
  suggestion: string | null;
};

/** `GET /api/v1/deployment/checks`. */
export type DeploymentChecksResponse = {
  channel: string;
  last_run_at: string | null;
  last_finished_at: string | null;
  last_status: "completed" | "failed" | null;
  last_error: string | null;
  last_seen: number | null;
  last_announced: string[];
  stale_banner: string | null;
  next_run_at: string | null;
  due_in_seconds: number | null;
  announced_total: number;
  rows: DeploymentCheckRow[];
};

/** `POST /api/v1/deployment/checks/run`. */
export type DeploymentCheckRunResponse = {
  result:
    | { state: "completed"; announced: string[]; seen: number }
    | { state: "failed"; reason: string };
  summary: DeploymentVersionSummary;
  announced: string[];
  checks: DeploymentChecksResponse;
};

/** ---------------------------------------------------------------------------------------------
 * The cluster panel (REQ-024, slice 4).
 *
 * A metric is a three-state union rather than `number | null` for the reason the server sends it
 * this way: the panel must never turn "the runtime did not report it" into a zero, and it must be
 * able to say WHY a cell is empty. `unknown` carries that reason as a sentence.
 * ------------------------------------------------------------------------------------------- */

export type ClusterMetric =
  | { state: "millicores"; value: number }
  | { state: "bytes"; value: number }
  | { state: "unknown"; reason: string };

/** One workload's row in the cluster table. */
export type ClusterWorkload = {
  name: string;
  replicas_desired: number;
  replicas_ready: number;
  /** `ready / desired` as a percentage, or null when desired is zero — which is a real state. */
  ready_percent: number | null;
  rollout_banner: string | null;
  cpu_request: ClusterMetric;
  /** Already `unknown` when the workload declares no limit, so there is no second way to say nothing. */
  cpu_limit: ClusterMetric;
  cpu_usage: ClusterMetric;
  /** Null rather than 0 when there is no limit to be a percentage of. */
  cpu_percent: number | null;
  cpu_over_limit: boolean;
  memory_request: ClusterMetric;
  memory_limit: ClusterMetric;
  memory_usage: ClusterMetric;
  memory_percent: number | null;
  memory_over_limit: boolean;
  restarts: number;
  age_seconds: number;
  age_label: string;
  cpu_sparkline: ClusterSparkline | null;
  cpu_expected_samples: number;
};

/** A normalised sparkline: values as percentages of the band, plus what the panel labels it with. */
export type ClusterSparkline = {
  /** One entry per drawn column, 0..100. A 50 is a gap or a flat series, not a measurement. */
  points: number[];
  min: number | null;
  max: number | null;
  flat: boolean;
  gaps: number;
};

/** `GET /api/v1/deployment/cluster`.
 *
 *  `runtime` is the literal `"cluster"` and not `"cluster" | "single"`: it is the discriminant
 *  that lets TypeScript narrow a `DeploymentClusterResponse | DeploymentProcessCard` in the
 *  screen, and a union of two literals is not a discriminant — the two branches become
 *  indistinguishable to the compiler and every `reading.runtime === "cluster"` needs a cast
 *  beside it. The server never sends the other value on a `200`.
 */
export type DeploymentClusterResponse = {
  runtime: "cluster";
  workloads: ClusterWorkload[];
  rollout_banner: string | null;
  /** Every workload over one of its own limits, in words — a clamped bar would not say it. */
  over_limit: string[];
  read_at: string;
  window_minutes: number;
  sparkline_width: number;
};

/** The single-instance alternative card. What a non-cluster deployment shows instead. */
export type DeploymentProcessCard = {
  runtime: "single";
  process: { uptime_seconds: number; resident_memory: ClusterMetric } | null;
  reason: string | null;
};

/** `GET /api/v1/deployment/cluster/{environment}/samples`. */
export type ClusterSamplesResponse = {
  workload: string;
  window_minutes: number;
  expected_samples: number;
  stored_samples: number;
  gaps: number;
  sparkline: ClusterSparkline;
};

/** `POST /api/v1/deployment/cluster/{environment}/sample`. */
export type ClusterSampleRunResponse = {
  environment: string;
  runtime: "cluster" | "single";
  written: number;
  pruned: number;
};

/** `POST /api/v1/deployment/cluster/{environment}/restart`. */
export type ClusterRestartResponse = {
  job: DeploymentJob;
  message: string;
  workload: string;
};

// The developer platform (REQ-033, slice 1)
// ---------------------------------------------------------------------------------------------

/**
 * One API key, as the API returns it.
 *
 * There is no `secret` field, and that is the design rather than an omission: the secret is
 * write-only, so this type — the one the list, the detail screen and every filter read from —
 * cannot carry one even if the API were changed to try. `MintedApiKey` is the separate type that
 * only `create` and `rotate` return.
 */
export type ApiKey = {
  id: string;
  organization_id: string;
  name: string;
  /** The public identifier. Safe to show, log and quote in a support conversation. */
  prefix: string;
  scopes: string[];
  environment: "live" | "sandbox";
  rate_tier: "standard" | "high";
  /** CIDR blocks, or `null` for "any source address". */
  ip_allowlist: string[] | null;
  expires_at: string | null;
  last_used_at: string | null;
  revoked_at: string | null;
  rotated_at: string | null;
  created_by: string;
  created_at: string;
  status: "active" | "pending" | "revoked" | "expired";
};

/**
 * The response of `POST /api/v1/api-keys` and `POST /api/v1/api-keys/{id}/rotate`.
 *
 * It *is* an `ApiKey` with one extra field — the API flattens the key to the top level — so a
 * client that already reads a key needs no new shape to read a minted one. The presence of
 * `secret` is the signal the panel uses to open the one-time dialog.
 */
export type MintedApiKey = ApiKey & {
  /** Shown exactly once. The API will never return it again. */
  secret: string;
};

/** `GET /api/v1/api-keys`. */
export type ApiKeysResponse = {
  keys: ApiKey[];
};

/** One day of a key's usage chart. */
export type ApiKeyUsagePoint = {
  day: string;
  requests: number;
  errors: number;
  p95_ms: number | null;
};

/** `GET /api/v1/api-keys/{id}` — the key plus its chart, in one round trip. */
export type ApiKeyDetail = {
  key: ApiKey;
  usage: ApiKeyUsagePoint[];
};

/** One request in the log. Metadata only: there is no body field, by design. */
export type ApiRequestLog = {
  id: number;
  organization_id: string;
  /** Which key authenticated it, or `null` for a session-authenticated call. */
  api_key_id: string | null;
  actor_user_id: string | null;
  method: string;
  /** Path without the query string. */
  path: string;
  status: number;
  duration_ms: number;
  /** The id the caller can quote in a bug report. */
  request_id: string;
  bytes_in: number | null;
  bytes_out: number | null;
  error_code: string | null;
  created_at: string;
};

/** `GET /api/v1/request-logs`. */
export type ApiRequestLogPage = {
  items: ApiRequestLog[];
  /** How many rows matched the filter, not just this page. */
  total: number;
  has_more: boolean;
};


// The API Explorer (REQ-033, slice 2)
// ---------------------------------------------------------------------------------------------

/** The slice of JSON Schema the Explorer's form draws. */
export type ExplorerSchema = {
  type: "string" | "integer" | "number" | "boolean" | "object" | "array";
  enum?: string[];
  format?: string;
  items?: ExplorerSchema;
  properties?: { name: string }[] & Record<string, ExplorerSchema>;
  required?: string[];
  description?: string;
};

/** One path or query parameter of a documented operation. */
export type ExplorerParameter = {
  name: string;
  in: "path" | "query";
  required?: boolean;
  description?: string;
  type: string;
  enum?: string[];
  format?: string;
  description_text?: string;
};

/**
 * One operation in the Explorer browser.
 *
 * `permission` is the permission the route guard checks, not documentation: the Explorer
 * resolves it before dispatching so a refusal names it. It is also what the browser filters on,
 * so a role that cannot publish a page does not see a publish button it would only be refused
 * by.
 */
export type ExplorerOperation = {
  id: string;
  method: string;
  path: string;
  tag: string;
  summary: string;
  permission: string | null;
  parameters: ExplorerParameter[];
  body: ExplorerSchema | null;
};

/**
 * `GET /api/v1/dev/operations` — the operations this caller may see.
 *
 * `can_send` is a separate field rather than something the panel derives from the list. Reading
 * the reference and *acting as the person at the screen* are two different permissions, and
 * only the server can answer the second one: a client that guessed would guess wrong in the
 * direction that matters, showing a Send button that is always `403`.
 */
export type ExplorerOperations = {
  operations: ExplorerOperation[];
  can_send: boolean;
};

/** What one Explorer call answered. */
export type ExplorerResult = {
  status: number;
  body: string;
  body_is_json: boolean;
  duration_ms: number;
  sent_path: string;
  request_id: string | null;
  /** `true` when a layer refused rather than a handler answering. */
  refused: boolean;
};

/** One copyable form of a call, with `$OMNION_API_KEY` as the credential. */
export type ExplorerSnippet = { language: string; code: string };

/** `POST /api/v1/dev/explorer/requests`. */
export type ExplorerRun = {
  result: ExplorerResult;
  snippets: ExplorerSnippet[];
};

/** What the send form collects. */
export type ExplorerRunInput = {
  method: string;
  /** The template, e.g. `/api/v1/pages/{id}`. */
  path: string;
  /** Path parameters, in the template's order. */
  path_params: string[];
  /** `name=value` pairs. */
  query: string[];
  /** Raw request body. */
  body?: string;
};
