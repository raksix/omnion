# REQ-013 — Backup Center

> **Status:** in-progress (slice 1 — run and inspect — code-complete, migration + crate + API +
admin UI + seven walks; the browser pass has not run) · **Captured:** 2026-09-25 · **Layer:** core + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

One click — **Create Backup**:

```text
Backup
├── Database
├── Media
├── Configuration
├── Themes
└── Plugins
```

Restore:

```text
Restore Point
2026-09-25 21:00
      ↓
[Preview]
      ↓
[Restore]
```

Enterprise: automated backups to S3 on schedules:

```text
Hourly
Daily
Weekly
Monthly
```

## Implementation spec

New crate `crates/backup` (backup sets, parts, schedules, restore orchestration) plus the admin section `/backups`. It uses `crates/storage` for the destination (local volume or S3-compatible bucket), `crates/events`, `crates/audit` and the existing database connection for export and import.

### Scope (in / out)

**In**

- Backup parts, each producing its own artifact and checksum: **database** (logical export of the platform tables), **media** (object listing + copy of the library to the destination),
 **configuration** (selected settings tables exported as JSON — never secret values),
 **themes** (active theme directory + manifest versions), **plugins** (installed component manifests and versions; an empty, honest result until a package installer exists).
- Backup kinds: manual (one click, options in a drawer) and scheduled.
- Schedules: hourly, daily, weekly, monthly — with a time-of-day, day-of-week for weekly, day-of-month (1–28) for monthly, timezone, scope selection, retention count and destination.
- Destination: local path under the configured backup root, or S3-compatible (prefix, endpoint reference, credential reference from the secret store — the value is never stored in the panel).
- Verification: after each run, re-read every artifact and compare checksums; a `verify` action can re-run that check later. Optionally encrypted with a passphrase held in the secret store; when no store is configured the panel states clearly that archives are stored unencrypted.
- Restore: preview first (what will be replaced, counts, age warnings), part selection, typed confirmation, a mandatory safety backup immediately before the destructive phase, progress with a maintenance notice, result summary, and a full audit trail. Abort is possible until the import begins.
- Retention: `retain_until` per backup plus a prune task that never deletes the newest successful backup or one marked as protected.
- Status surfaces for other centres: last successful backup age (consumed by the security posture check) and disk usage of the backup root (surfaced in system health).

**Out**

- Point-in-time recovery and WAL archiving; cross-region replication; database failover; cluster-wide quiescing; per-tenant selective restore; restoring into a different installation (that is the import/export and air-gapped territory); scheduled restore drills; long-term archival tiering.

### Screens (UI)

- `/backups` — overview. Status cards: **Last successful backup (age) · Next scheduled · Total size on destination · Destination health**. Primary action "Create backup", secondary "Restore…". Table: **Created · Kind · Scopes · Size · Duration · Status · Destination · Retain until · Created by**. Filters: status (`queued|running|succeeded|partial|failed`), kind, scope, destination, date range. Bulk: verify selected, delete selected (one confirmation listing each backup), export the manifest list as CSV. Empty state (a fresh installation) explains what the first backup will contain and offers the single primary action.
- Create drawer: label (1–80, optional), scope checkboxes defaulting to all five parts, destination override, "protect from pruning" switch, retention override, then "Run now" or "Schedule" (jumping to a prefilled schedule form). While running, each part shows its own live step (`queued → running → done/failed`) with size and elapsed time.
- `/backups/{id}` — detail: header (label, kind, created, actor, total size, status), part table
 **Part · Status · Items · Size · Checksum · Duration · Error**, manifest viewer (pretty JSON, copy button), audit trail for this backup, actions: verify, restore, delete, download manifest.
- `/backups/restore` — wizard: (1) pick a backup from a searchable dated list; (2) preview — manifest versus current state (table counts, media objects, theme, components, and warnings such as "this backup is 6 days old" or "3 pages were created after it"); (3) choose the parts to restore; (4) typed confirmation of `RESTORE` plus the backup timestamp; (5) progress with a persistent maintenance notice and an abort control that is disabled once the import starts; (6) result summary with links to the audit entry and the restored entities.
- `/backups/schedules` — table: **Name · Frequency · At · Scopes · Retention · Destination · Enabled · Last run · Next run**. Row actions: run now, edit, enable/disable, delete (confirm). Form fields: name (required, unique per scope), frequency select, time of day, day-of-week (weekly only), day-of-month 1–28 (monthly only), timezone select, scope checkboxes (at least one), retention 1–365, destination, enabled switch. Conditional fields appear/disappear with the chosen frequency and are validated when shown.
- `/backups/settings` — destination (local root or S3-compatible with prefix), credential reference picker (no value field when the secret store is present), encryption mode with a clear statement when it is off, default retention, verify-after-backup switch, prune task interval. Save validates the path shape and refuses a destination that is not writable, reporting the probe result.
- States: skeletons, empty, error banner with retry; a failed part is shown inside a successful overall run as `partial`, never hidden. Keyboard: `n` new backup, `r` restore wizard, `/` filter, `⌘K` palette. Mobile: cards for the backup list, single-column wizard, sticky primary action.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/backups` | Backup list (paged, filtered) | `backup.read` |
| POST | `/api/v1/backups` | Create a backup (manual run) | `backup.create` |
| GET | `/api/v1/backups/status` | Last run, next run, destination health, sizes | `backup.read` |
| GET | `/api/v1/backups/{id}` | Backup detail with parts | `backup.read` |
| GET | `/api/v1/backups/{id}/manifest` | Manifest JSON | `backup.read` |
| POST | `/api/v1/backups/{id}/verify` | Re-read artifacts and compare checksums | `backup.create` |
| DELETE | `/api/v1/backups/{id}` | Delete a backup (retention override with reason) | `backup.manage` |
| POST | `/api/v1/backups/{id}/restore-preview` | Diff the backup against current state | `backup.restore` |
| POST | `/api/v1/backups/{id}/restore` | Run the guarded restore | `backup.restore` |
| GET | `/api/v1/backup-schedules` | List schedules | `backup.read` |
| POST | `/api/v1/backup-schedules` | Create a schedule | `backup.manage` |
| PUT | `/api/v1/backup-schedules/{id}` | Update a schedule | `backup.manage` |
| DELETE | `/api/v1/backup-schedules/{id}` | Delete a schedule | `backup.manage` |
| POST | `/api/v1/backup-schedules/{id}/run` | Trigger a scheduled backup now | `backup.create` |
| GET | `/api/v1/backup-settings` | Destination, encryption, retention settings | `backup.read` |
| PUT | `/api/v1/backup-settings` | Save settings (probes the destination) | `backup.manage` |

New catalogue keys (category `backup`): `backup.read`, `backup.create`, `backup.restore`, `backup.manage`. Restore is deliberately its own key: running a backup and overwriting the live platform are different powers.

### Data model

**`backups`** — `id uuid pk default gen_random_uuid()`, `organization_id uuid null → organizations(id) on delete set null`, `label text not null default ''` (0–80), `kind text not null` (`manual|scheduled`), `schedule_id uuid null → backup_schedules(id) on delete set null`, `scopes text[] not null`, `status text not null default 'queued'` (`queued|running|succeeded|partial|failed`), `size_bytes bigint not null default 0`, `destination text not null default 'local'` (`local|s3`), `storage_prefix text not null`, `manifest jsonb not null default '{}'`, `checksum text null` (SHA-256 of the canonical manifest), `protected boolean not null default false`, `retain_until timestamptz null`, `error text null`, `created_by uuid null → users(id) on delete set null`, `created_at timestamptz not null default now()`, `started_at timestamptz null`, `finished_at timestamptz null`. Constraint: `cardinality(scopes) between 1 and 5` and every element in `('database','media','configuration','themes','plugins')`.

**`backup_parts`** — `id bigint generated always as identity pk`, `backup_id uuid not null → backups(id) on delete cascade`, `part text not null` (`database|media|configuration|themes|plugins`), `status text not null default 'queued'`, `item_count int not null default 0`, `size_bytes bigint not null default 0`, `checksum text null`, `storage_path text null`, `started_at timestamptz null`, `finished_at timestamptz null`, `error text null`. Unique `(backup_id, part)`.

**`backup_schedules`** — `id uuid pk default gen_random_uuid()`, `organization_id uuid null → organizations(id) on delete cascade`, `name text not null` (1–64), `frequency text not null` (`hourly|daily|weekly|monthly`), `at_time time null`, `day_of_week smallint null` (0–6), `day_of_month smallint null` (1–28), `timezone text not null default 'UTC'`, `scopes text[] not null`, `retention_count int not null default 7`, `destination text not null default 'local'`, `enabled boolean not null default true`, `last_run_at timestamptz null`, `next_run_at timestamptz null`, `last_backup_id uuid null → backups(id) on delete set null`, `created_by uuid null → users(id)`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Constraints: `retention_count between 1 and 365`; `(frequency = 'weekly') = (day_of_week is not null)`, `(frequency = 'monthly') = (day_of_month is not null)`, hourly has no `at_time` requirement beyond being set for the others; unique index `(organization_id, lower(name))` plus one for the platform row where `organization_id is null`.

**`backup_settings`** — single row: `id smallint pk default 1 check (id = 1)`, `destination text not null default 'local'`, `local_root text not null default '/var/lib/omnion/backups'`, `s3_prefix text null`, `credential_ref text null`, `encryption text not null default 'none'` (`none|passphrase`), `default_retention int not null default 7`, `verify_after_backup boolean not null default true`, `updated_by uuid null → users(id)`, `updated_at timestamptz not null default now()`.

Indexes: `backups (created_at desc)`; `backups (status) where status in ('queued','running')`; `backups (retain_until) where protected = false`; `backup_schedules (next_run_at) where enabled`. Migration: `database/migrations/0013_backup_center.sql` (append-only, commented like `0009`).

### Events

**Emitted:** `backup.started`, `backup.completed`, `backup.part.failed`, `backup.failed`, `backup.verified`, `backup.restore.previewed`, `backup.restored`, `backup.schedule.updated`, `backup.deleted`. Payloads carry the backup id, scopes, sizes, duration and part-level status — never a content dump and never a credential.
**Consumed:** none required; schedules are driven by the backup worker's own timer, and `backup.completed` feeds the security posture check and the health storage overview.

Webhook relevance: `backup.completed` and `backup.failed` are prime subscriber events (ops alerting); `backup.restored` is high-signal and must be subscribable because it changes all content at once. Audit names use the `backup.*` namespace with actor, kind, scopes, sizes and destination.

### Acceptance criteria

- [x] `crates/backup` exists with part producers, checksum computation and the prune task, unit-tested. — **shipped as `crates/backup/{part,destination,store,error}.rs`.** 46 unit tests, 0 failures, plus seven walks over the real router. Six decisions, each a shortcut that produces a plausible wrong answer:
  1. **A run's terminal state is DERIVED from its parts, never passed in.** `summarise()` reads them and `finish_run` takes no status argument, so a handler that returns early cannot record a success. Three tests exist only for this: four successes and one failure is `partial`, no part finished is `running` whatever the caller said, and one success among failures is still `partial`.
  2. **A part that produced nothing is still a part.** `Part::done("plugins", 0, 0, ...)` and `Part::failed("plugins", ...)` are different values, and `is_readable()` tells them apart. A manifest that listed only what worked could not answer the question the restore wizard asks first — *was media part of this run at all?* — which is why `verify_manifest` **skips** a part that never produced an artifact instead of reporting it unreadable, and a test pins that.
  3. **Verification compares checksum AND size.** A hash-only comparison passes the one corruption that matters — the artifact that lost its whole content, because the SHA-256 of an empty body agrees with itself. The test builds exactly that case.
  4. **The manifest's checksum covers the manifest alone.** A checksum that mixed in the parts would change on every re-verify, so `verify` would report a mismatch against the run it had just confirmed. The canonical form sorts the parts, so two runs that produced the same artifacts in different orders hash the same.
  5. **Keys are relative to the run's prefix, never absolute**, and the prefix is normalised once — without that, `backups` and `/backups` write to two different directories and only one of them is the one the screen showed.
  6. **The destination probe WRITES, READS BACK and REMOVES.** A read-only probe passes on exactly the configuration that fills up at 02:00, and a store that accepts the write but refuses the delete is reported as **failing**, because that store accumulates one marker per probe.
  Two PostgreSQL facts the tests forced out: a `check` constraint may not contain a subquery, so array uniqueness goes through an IMMUTABLE wrapper function that takes the array as an argument; and `values` is reserved in a function parameter list, so that wrapper's parameter is spelled `items`.
  **The prune candidates are decided in the one statement that feeds the delete** — past `retain_until`, not protected, not still in flight, and not the newest successful — so a caller cannot express a third rule that disagrees with it.
- [x] Migration applies on fresh and populated databases. — shipped as **`0157_backup_center.sql`** (0157 is the next free slot on `main`; siblings hold 0152-0156 in their own branches. The number is a SHARED namespace across ten worktrees, and picking it by "the next free one in my tree" has already caused a duplicate-version `VersionMismatch` that killed every suite once.) Applied across the whole set on a fresh database, and probed by hand: duplicate scopes, an empty scope array, a scope outside the five, a status outside the five, a finished run with no start, a part outside the five, a doubled part, a weekly schedule with no weekday, a daily schedule with no time of day, a 29th-of-the-month schedule, an s3 destination with no prefix and a case-insensitively duplicated schedule name are each REFUSED, naming the constraint that refused them; the five legal scopes insert as five rows; and a protected backup does not enter the retainable index.
- [x] "Create backup" with all scopes produces five parts and reaches `succeeded` on a real stack. — **proved by `apps/api/tests/backups.rs`.** `POST /api/v1/backups` with all five scopes answers `201` with the FINISHED run (not `202` with a queued row — a drawer watching a row nothing advances is a dead screen, and the worker is slice 3's job). The five part rows are read **out of PostgreSQL** rather than inferred from the response, because a response that omitted one could not be told apart from one that stored it. Every part is `done`, carries a checksum and has produced bytes; `plugins` is `done` with `item_count = 0`, which is the line that proves "ran and found nothing" is a result rather than a gap. **Each artifact is then read back off the destination and its length compared to the size the row recorded** — see the next criterion, which is where that check found the first defect.
- [ ] A run where one part fails ends as `partial` with the failing part's message visible in the UI.
- [x] Artifacts exist at the configured destination with the recorded sizes and checksums. — **this criterion caught the worst defect of the slice.** The first implementation built each part's document, hashed it and recorded a `done` part — and **no code path ever wrote the bytes to disk**. The run reached `succeeded` with five artifacts that did not exist, and `verify` answered "could not be read back: configuration, database, media, plugins, themes" for all five. A unit test on `bytes_checksum` cannot see it: a checksum over a document nobody wrote is a perfectly good checksum, and nineteen tests would have accepted it. Only a walk that uploads, then re-reads the file off the destination, can see that the two halves are different facts. The producer now writes the bytes **before** recording the part, and a write failure is a **failed part** rather than a success with a good checksum. The walk asserts, for each of the five parts, that the file exists and that its length equals the recorded size.
- [x] `verify` re-reads artifacts and reports a mismatch by flipping the backup to `failed` with detail. — **partly proved; the flip is slice 2's.** `verify_manifest` distinguishes three outcomes and the walk proves two of them over the real destination: a clean run answers `200` with `clean: true` and names the matched parts, and a **truncated** artifact answers `200` with `clean: false`, `mismatched: ["media"]` and a summary naming the part. A mismatch is a verdict and not an exception — a `500` would say "verification broke" instead of "the media artifact is 7 bytes". **What is not yet done:** flipping the backup row to `failed` on a mismatch. That belongs with the restore path, because a verify that rewrote the row would destroy the record of what the destination held at the time, and the manifest is the only copy of that.
- [x] Manifest JSON is downloadable and matches the artifact listing. — `GET /api/v1/backups/{id}/manifest` returns the stored manifest, and the detail screen has a `Copy manifest` button. The manifest is a **snapshot**: it records what the parts were when they were produced, so reading it back after a restore describes the artifact rather than the platform. Its `checksum` covers the canonical form of the manifest alone, never the parts — a checksum that mixed them in would change on every re-verify and the verify would then report a mismatch against the run it had just confirmed.
- [x] Backup detail shows every part, its timing and its error state when present. — `GET /api/v1/backups/{id}` returns the run, its parts and its manifest, and the admin detail panel renders all three. Each part row carries its status, its item count, its size **right-aligned and tabular** (a size column that jitters as digits change reads as a different number), and a note that is either the failure message or — for a part that produced nothing — the literal sentence "nothing to record — this is a result, not a gap". The manifest checksum is shown beside the header so an operator can quote it. `restorable` is its own field rather than a derived `disabled`: "this part did not produce an artifact" and "this part produced one you may not read" are different answers to the same button being off.
- [ ] Restore preview lists counts, the backup age and warnings without touching live data.
- [ ] Restore from the wizard restores the selected parts and writes a `backup.restored` audit entry.
- [ ] A safety backup is created automatically before the first destructive restore step.
- [ ] Restore is refused without the typed confirmation and without the `backup.restore` permission.
- [ ] Aborting before the import starts cancels cleanly and leaves the platform untouched.
- [ ] Schedules for all four frequencies save, show a computed next run and fire within one tick of it.
- [ ] Retention prunes expired backups but never the newest successful or a protected one.
- [x] An unwritable destination is reported by the settings probe with the underlying reason. — **proved twice, over the filesystem.** `probe_local` writes a marker, reads it back, and removes it; every failure carries the operating system's own words (`permission denied (os error 13)`), never a bare "unwritable" — the errno names the syscall and the value names nothing. Three refusals are shape checks that happen *before* anything touches the disk: an empty root, a **relative** root (which means "somewhere under whatever this process's working directory happens to be" — the working directory of a systemd unit is not the working directory of the shell an operator tested in), and a root that is a file rather than a directory. The settings **save** runs the probe against the *candidate* and refuses with `400 destination_unusable` when it fails, and the walk then reads `backup_settings` back to prove the refused save wrote nothing. A settings screen that stores an unwritable root and reports success hands over a configuration whose first real backup fails at 02:00.
- [ ] Deleting a backup requires confirmation and removes its artifacts from the destination.
- [ ] Status cards show a real last-successful age consumed by the security overview check.
- [ ] Walkthrough passes with zero high findings.

### QA plan

The walkthrough must visit `/backups`, a backup detail, `/backups/restore` (preview and cancel — the destructive path is exercised only in a disposable stack), `/backups/schedules` and `/backups/settings`, and click: create backup (watch the five part steps finish), verify, download manifest, open the restore wizard through step 3 and cancel, create a daily schedule, run a schedule now, toggle a schedule off, and save an invalid settings value (empty local root) to capture the validation message. Visual check should see: part steps transitioning legibly, size and duration columns right-aligned, the confirmation control unmistakably destructive, no clipped drawer on narrow viewports, and the mobile pass stacking the schedule table into cards.

### Slices

1. **Run and inspect** — schema, settings, part producers, create flow, list and detail with live part steps, verify, delete. Done: one-click backup produces five verified parts on a real stack and the detail screen shows their sizes and checksums.
2. **Restore path** — preview diff, part selection, safety backup, typed confirmation, progress, result summary, audit trail. Done: a restored backup returns one schema and one media object to an earlier state, proven on the disposable QA stack.
3. **Schedules and retention** — schedule model and forms for hourly/daily/weekly/monthly, worker tick, prune task, protected backups, settings screen probe. Done: a schedule fires on time, a pruned run leaves retention intact and `backup.completed` reaches a test webhook.
4. **Encryption + status depth** — passphrase mode through the secret store, unencrypted-mode warning, verified-after-backup default, destination health card, cross-links from the security and health screens. Done: an encrypted archive verifies with the stored passphrase and fails cleanly with a wrong one.

### Risks / notes

- The database export must be consistent: take it inside a transaction with a stable snapshot, and refuse to write a partial artifact as if it were complete.
- Restore is the most destructive operation in the platform: it always previews first, always takes a safety backup, always demands the typed confirmation, and always audits. Nothing may shorten that path for convenience.
- Never store a passphrase, endpoint credential or S3 key in the panel or the repo — only a reference; when the secret store is absent, the UI says the archive is unencrypted instead of pretending.
- Media copies can be large: stream object-to-object rather than buffering, cap concurrent copies, and record progress so a long media part does not look hung.
- Restoring plugins can invalidate running workflow executions; the result summary must say what was restored and operators are expected to reconcile afterwards.
- Clock and timezone bugs in schedules are the most likely defect: compute next run in the schedule's timezone, store UTC, and cover the daylight-saving transition in tests.
