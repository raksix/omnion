# REQ-129 — Migration Safety & Release Engineering

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Schema changes that never lose a row.

- Numbered, reversible SQL migrations with `up`/`down` verified in CI (up → down → up on a fixture DB).
- Non-destructive patterns enforced by policy (add-nullable → backfill → constrain; never drop-then-add in one step).
- Deployment sequence: migrations run before the new code serves traffic; app and DB rollback documented separately.
- Seed and fixture data for local/demo installs; anonymised dump tooling for support.
- Zero/minimal-downtime strategy: additive migrations, dual reads, backfill jobs.

## Implementation spec

The existing numbered migrations under `database/migrations/` (0001–0014 and counting, append-only, commented in the `0009` style) keep their shape; this request builds the **runner** around them, the **lint policy**, the **verification gate**, the **backfill and seed machinery** and the **anonymised export tooling**. New code in `crates/migrations` plus a `omnion migrate` subcommand in the CLI; the admin surface extends the deployment centre of REQ-024 (`/deployment/*`) and shares its permission family, with new keys for the destructive operations. The upgrade sequence itself is documented in `docs/deployment/upgrade.md` (REQ-128) — this request supplies the runner and the evidence it relies on.

### Scope (in / out)

**In**

- **Runner.** Migrations keep the `NNNN_name.sql` convention, and each file may declare a `-- down` section (or a sibling `NNNN_name.down.sql`) containing the reversal. The runner applies pending migrations in order inside a single advisory lock, records a ledger row per run (version, name, checksum, direction, duration, actor, source), and refuses to start when a file's checksum differs from the ledger — an already-applied migration is immutable, and an edit is drift that must be resolved by a new migration. Before each migration it sets `lock_timeout` and `statement_timeout` from policy so a blocked DDL fails fast instead of stalling the platform. `--dry-run` prints the statements, the estimated impact and the violations without touching the database.
- **Reversible by contract.** The CI gate runs `up → down → up` on a fixture database seeded with representative rows and asserts: the down script succeeds, the schema after down matches the pre-up schema (structure comparison, not text diff), and the second up produces the same structure again. A migration without a down script is allowed only when the policy lists it as an explicit exception with a reason, and the deployment centre then marks upgrades involving it as **no database rollback**.
- **Non-destructive policy.** A lint pass over every migration refuses banned shapes by default: `drop column`, `drop table`, `rename` of a column or table, `alter column type`, adding a `not null` column without a default, adding a constraint that requires a full-table validation in the same statement as the data change, and any `drop`-then-`add` pair inside a single migration. The enforced pattern is the documented one: **add nullable → backfill in batches → constrain in a later migration**, with concurrent index creation for large tables. A violation fails CI; a maintainer may waive one with a written reason that is recorded in the policy ledger and surfaced in the upgrade helper.
- **Deployment sequence.** Migrations run as a dedicated step — the compose one-shot `migrate` service or the Helm pre-upgrade job (REQ-128) — strictly before the new application version accepts traffic. The application never runs DDL at boot; it fails readiness with a clear message when the schema version is behind what it expects. Rollback is split: **application rollback** (previous image, always available) and **database rollback** (only when the release ships verified down scripts; otherwise restore from the most recent backup, documented as the slower path). The deployment centre reads this split and the upgrade helper renders it.
- **Zero/minimal downtime strategy per change class:** additive column (instant), new index (concurrent build, outside a transaction), constraint introduction (validated after backfill, or `not valid` then validated), column removal (stop reading, stop writing, then drop in a later release), table rename (new table plus dual write then cut over, never a rename), and enum/value additions (append-only values with the old code still tolerant). Each class has a short recipe in the migration guide with the exact statement shapes.
- **Backfill jobs.** A migration may register a backfill descriptor (table, key, batch size, rate limit, resume cursor). The runner exposes backfills as first-class jobs: batched updates in primary-key order, a persisted cursor so a restart resumes exactly, a throughput display, pause/resume, and a completion event. Backfills never run inside the migration transaction for large tables.
- **Seeds and fixtures.** `database/seeds/` names datasets (`minimal`, `demo`, `fixture`), each with a manifest (name, description, row estimate, compatible range). The CLI and the panel can load a dataset into a sandbox or a development installation behind a typed confirmation; production refuses seed loads unless the installation is explicitly marked as a demo. Fixtures used by tests are generated from the same descriptors so the walkthrough and the integration tests see the same world.
- **Anonymised dump tooling.** An export builder that selects tables and rows, applies the column classification map (README-maintained: what is personal, what is secret, what is safe), and produces a dump where classified columns are removed, hashed with a per-export salt, or replaced with synthetic-but-consistent values (referential integrity preserved). The output is watermarked, expires, is downloaded once, and every step is audited. A column that is not classified refuses to export until it is classified — the tool fails closed.

**Out**

- Online schema change features of external tools; the recipes stay portable PostgreSQL.
- Cross-database migration and heterogeneous replication.
- Data-level deduplication or normalisation of historic rows (a data-quality project, not this request).
- Restoring backups automatically after a failed migration — the runbook tells an operator, the platform does not guess.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/deployment/migrations` | Ledger: applied and pending, checksum drift warnings, lock status, last run with duration |
| `/deployment/migrations/{version}` | One migration: SQL, down script, plan preview, run history, verify-down action |
| `/deployment/migrations/policy` | Policy settings, banned patterns, waivers with reasons, CI gate status |
| `/deployment/backfills` | Backfill jobs with progress, throughput, pause/resume, failure detail |
| `/deployment/seeds` | Datasets with manifest, load into sandbox behind confirmation, last loaded |
| `/deployment/exports` | Anonymised export builder, list with expiry, single-use download, revoke |

- Ledger columns: **Version · Name · Applied at · Duration · Checksum · Down verified · Actor · Source**; pending rows appear above applied ones with a "would run" badge. A checksum drift row is red with the two hashes and a link to the release notes of the migration that supersedes it — the resolution is always a new migration.
- Migration detail: statements split per section with the down script in a second pane, a plan preview (statements, estimated lock risk, rows affected when cheaply knowable, violations), and a `Verify down` action that runs the reversal against a **scratch database** rebuilt from a recent schema snapshot — the action is impossible on production and the route says so.
- Policy screen: toggles for the banned patterns (each with a plain-language explanation of what it prevents), timeout settings, backfill defaults, and the waiver list (pattern, file, reason, approver, date). Changing a policy setting is itself audited and versioned.
- Backfills screen: table of jobs with **Table · Started · Progress · Throughput · ETA · State**; a job page shows a progress bar from real counters, the resume cursor, the failing batch with its error, and `Pause`/`Resume`. A paused job keeps its cursor and can be resumed days later.
- Seeds: cards per dataset with name, description, size estimate, compatible range and a `Load` action that requires typing the dataset name; the loaded state shows when it last ran and by whom. Loading into a production installation is refused with a message that names the installation kind.
- Exports: a builder form (tables multi-select, row limit or date window, classification map preview, per-column action `remove|hash|synthetic`), then a job that reports progress and produces a file with a checksum, watermark text, expiry timestamp and a single-use download. The list shows `Expires · Downloaded · Revoked` states; `Revoke` kills the link immediately.
- States: empty states per screen; a scratch database that cannot be provisioned disables `Verify down` with the reason; error states carry request ids; lock contention shows the blocking pid and query age (from `pg_locks`/`pg_stat_activity`) rather than a generic failure.
- Keyboard: `/` search, `v` verifies the focused migration down (when allowed), `Esc` closes drawers. Mobile: tables become cards, the SQL pane scrolls horizontally inside its region.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/deployment/migrations` | Ledger: applied, pending, drift | `deployment.migrations.read` |
| GET | `/api/v1/deployment/migrations/{version}` | One migration with SQL, down script, runs | `deployment.migrations.read` |
| POST | `/api/v1/deployment/migrations/plan` | Dry-run: statements, lock risk, violations | `deployment.migrations.read` |
| POST | `/api/v1/deployment/migrations/apply` | Apply pending migrations (normally the deploy job or CLI) | `deployment.migrations.apply` |
| POST | `/api/v1/deployment/migrations/{version}/verify-down` | Rehearse the reversal on a scratch database | `deployment.migrations.verify` |
| GET | `/api/v1/deployment/migrations/lock` | Current advisory lock, blocking queries, ages | `deployment.migrations.read` |
| GET | `/api/v1/deployment/migrations/policy` | Policy settings and waivers | `deployment.migrations.read` |
| PUT | `/api/v1/deployment/migrations/policy` | Save policy settings | `deployment.migrations.apply` |
| GET | `/api/v1/deployment/migrations/violations` | Lint results for the shipped migrations | `deployment.migrations.read` |
| POST | `/api/v1/deployment/migrations/violations/{id}/waive` | Waive with a reason | `deployment.migrations.apply` |
| GET | `/api/v1/deployment/backfills` | Backfill jobs with progress | `deployment.migrations.read` |
| POST | `/api/v1/deployment/backfills/{id}/pause` | Pause a running backfill | `deployment.migrations.apply` |
| POST | `/api/v1/deployment/backfills/{id}/resume` | Resume from the cursor | `deployment.migrations.apply` |
| GET | `/api/v1/deployment/seeds` | Seed datasets and load state | `deployment.migrations.read` |
| POST | `/api/v1/deployment/seeds/{name}/load` | Load a dataset (sandbox/dev only, typed confirm) | `deployment.seeds.manage` |
| GET | `/api/v1/deployment/exports` | Export list with expiry and state | `deployment.migrations.read` |
| POST | `/api/v1/deployment/exports` | Create an anonymised export | `deployment.exports.create` |
| GET | `/api/v1/deployment/exports/{id}/download` | Single-use download, short expiry | `deployment.exports.download` |
| DELETE | `/api/v1/deployment/exports/{id}` | Revoke an export and delete the file | `deployment.exports.create` |

Errors: `403` on every destructive action without its dedicated permission, `409` when another migration run holds the lock, `422` when a plan contains a banned pattern that is not waived, `423` when the schema is behind what the running application expects (readiness-style refusal with the expected version), `410` on an expired export download.

### Data model

Migration: `database/migrations/0030_migration_safety.sql` (next free slot at tick time), additive, with a down script per the policy it introduces — it is the first migration to demonstrate it.

- `schema_migrations` — `version text pk` (`0001`-style), `name text not null`, `checksum text not null` (sha256 of the file as applied), `applied_at timestamptz not null default now()`, `duration_ms int not null default 0`, `statement_count int not null default 0`, `actor text not null`, `source text not null check (source in ('cli','deploy','ci','boot'))` `down_verified_at timestamptz`, `down_verified_by text`, `has_down boolean not null default false`, `waiver_reason text`.
- `migration_runs` — `id bigserial pk`, `version text not null`, `direction text not null check (direction in ('up','down'))` `status text not null check (status in ('running','succeeded','failed','aborted'))` `started_at timestamptz not null default now()`, `finished_at`, `duration_ms int`, `actor text not null`, `source text not null`, `error text`, `plan jsonb not null default '{}'`; index `(version, started_at desc)` and `(status) where status = 'running'`.
- `migration_policy` — single row: `id smallint pk default 1 check (id = 1)`, `require_down_scripts boolean not null default true`, `lock_timeout_ms int not null default 5000 check (lock_timeout_ms between 100 and 60000)`, `statement_timeout_ms int not null default 300000`, `banned_patterns jsonb not null default '{}'` (pattern → enabled), `backfill_batch_size int not null default 5000 check (backfill_batch_size between 100 and 100000)`, `backfill_rate_per_second int not null default 200`, `require_approval_for_destructive boolean not null default true`, `updated_by uuid`, `updated_at`.
- `migration_violations` — `id bigserial pk`, `version text not null`, `pattern text not null`, `severity text not null check (severity in ('error','warning'))` `line int`, `excerpt text`, `detected_at timestamptz not null default now()`, `waived_by uuid`, `waived_at`, `waiver_reason text`; unique `(version, pattern, line)`. Rewritten by the lint pass on every CI run; a waiver survives because the waiver row is keyed, not the finding.
- `backfill_jobs` — `id uuid pk`, `version text not null` (the migration that registered it), `table_name text not null`, `key_column text not null`, `status text not null default 'pending' check (status in ('pending','running','paused','succeeded','failed','cancelled'))` `total_rows bigint`, `processed_rows bigint not null default 0`, `cursor text`, `batch_size int not null`, `rate_per_second int`, `error text`, `started_at`, `finished_at`, `paused_at`, `resumed_count int not null default 0`; index `(status) where status in ('pending','running','paused')`.
- `seed_datasets` — `name text pk`, `description text not null default ''`, `kind text not null check (kind in ('minimal','demo','fixture'))` `row_estimate int`, `compatible_range text not null default ''`, `builtin boolean not null default true`, `updated_at`. **`seed_loads`** — `id uuid pk`, `dataset text not null references seed_datasets(name) on delete restrict`, `target text not null check (target in ('sandbox','development','demo'))` `status text not null`, `rows_written bigint not null default 0`, `actor uuid`, `started_at`, `finished_at`, `error text`.
- `anonymized_exports` — `id uuid pk`, `requested_by uuid`, `reason text not null`, `tables text[] not null default '{}'`, `row_limit int`, `window_start timestamptz`, `window_end timestamptz`, `column_actions jsonb not null default '{}'` (table.column → `remove|hash|synthetic`), `status text not null default 'queued' check (status in ('queued','running','ready','failed','revoked','expired'))` `file_key text`, `file_size bigint`, `checksum text`, `watermark text not null`, `expires_at timestamptz not null`, `download_count int not null default 0`, `last_downloaded_at`, `created_at`; index `(status, expires_at)`.
- `column_classifications` — `table_name text not null`, `column_name text not null`, `class text not null check (class in ('personal','secret','identifier','safe'))` `default_action text not null check (default_action in ('remove','hash','synthetic','keep'))` `notes text not null default ''`, primary key `(table_name, column_name)`. The export builder fails closed when a selected column is missing here — this table is what makes "we forgot to strip that column" impossible rather than unlikely.
- Indexes and retention: export files live in object storage with a lifecycle rule matching `expires_at`; `migration_runs` rows are pruned to a bounded history; `backfill_jobs` rows persist because the cursor is operationally meaningful.

### Events

- **Emitted:** `migration.applied`, `migration.failed`, `migration.rolled_back` (down run), `migration.down_verified`, `migration.violation.detected`, `migration.policy.updated`, `migration.waiver.granted`, `backfill.started`, `backfill.progress` (throttled), `backfill.paused`, `backfill.completed`, `seed.loaded`, `anonymized_export.created`, `anonymized_export.downloaded`, `anonymized_export.expired`.
- **Consumed:** `deployment.started`/`succeeded`/`failed` (REQ-024) links a migration run to the deploy that triggered it, so the deployment history and the ledger tell one story; `secret.rotated` (REQ-037) is deliberately **not** tied to migrations — re-encryption is a job, not DDL.
- Webhook relevance: `migration.applied`, `migration.failed` and `anonymized_export.downloaded` are the payloads an operator wires into chat and into the security trail. Payloads carry versions, names, durations and counts — never SQL text with embedded literals, never export contents.
- Notification relevance: a failed migration during a deploy notifies holders of `deployment.migrations.apply` immediately; a created anonymised export notifies the support lead through REQ-021 because a data artifact has left the platform's control.

### Acceptance criteria

- [ ] `database/migrations/0030_migration_safety.sql` applies on a fresh and a populated database, and its own down script reverses it.
- [ ] The runner applies pending migrations in order under a single advisory lock and writes one ledger row per run with a checksum.
- [ ] A second concurrent runner is refused with `409` and waits or exits cleanly rather than interleaving.
- [ ] A file edited after being applied is detected as checksum drift and blocks the run with a message naming the offending file.
- [ ] CI runs `up → down → up` on the seeded fixture database for every migration, and the schema comparison detects a hand-broken down script (proven with a deliberately bad fixture).
- [ ] A migration without a down script fails the gate unless a waiver with a reason exists, and the upgrade helper marks it as no database rollback.
- [ ] The lint pass fails the documented banned shapes (`drop column`, `drop table`, `rename`, type change, non-nullable column without default, drop-then-add pair) with file and line references.
- [ ] The plan preview shows the statements, the timeout settings and the violations without executing anything.
- [ ] A blocked DDL fails within `lock_timeout` and the lock screen names the blocking pid and query age.
- [ ] The documented zero-downtime recipe is proven end to end on a live fixture with traffic: add nullable → deploy dual read/write → backfill → constrain in a later migration, with no failed request throughout.
- [ ] A backfill runs in batches, survives a process restart, resumes from its cursor without re-processing completed rows, and completes with accurate counters.
- [ ] A paused backfill resumes days later and the pause/resume/complete events are emitted exactly once each.
- [ ] Seeds load `minimal`, `demo` and `fixture` datasets into a sandbox, and a production installation refuses the load with a message naming its kind.
- [ ] Fixtures produced for tests and for the walkthrough come from the same descriptors and match.
- [ ] An anonymised export refuses to run while any selected column lacks a classification.
- [ ] The export output contains no classified value (asserted by grepping the produced file for fixture personal data and secret values); hashed columns are consistent across tables so joins still work.
- [ ] The export file is watermarked, expires, downloads once (`410` afterwards) and can be revoked immediately; all three states are visible in the panel.
- [ ] `docs/deployment/upgrade.md` names the migration-before-code rule, the application/database rollback split and the per-class recipes; the steps were executed verbatim in QA.
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The migration pass runs against a seeded fixture database: apply all, run the ledger checks, then execute the deliberately broken fixtures (missing down script, banned pattern, checksum drift, blocked DDL) and assert each fails with its documented message and code. The `up → down → up` gate runs in CI and its output is what the panel renders as `Down verified`. The live-traffic pass proves the zero-downtime recipe: a fixture application keeps serving requests while a nullable column, a backfill and a later constraint are applied in sequence; the request log must show no failures and the backfill counters must match the row count.

The walkthrough visits `/deployment/migrations`, a migration detail, `/deployment/migrations/policy`, `/deployment/backfills`, `/deployment/seeds` and `/deployment/exports`, and clicks: plan preview, verify-down on a scratch database (and confirms the action is absent on a production-marked environment), a waiver with a reason, pause/resume on a running backfill, a seed load with the typed confirmation (and the production refusal), and a full export cycle including the post-expiry download refusal. The export grep is part of the pass, not a separate review. The visual check must see: drift warnings that do not rely on colour, a real progress bar and throughput number on the backfill screen, a checklist marker for the point of no return, readable SQL panes at 1280 px and card layout under 640 px.

### Slices

1. **Runner + ledger + CI gate.** Ledger tables, checksum tracking, advisory lock, timeouts, `omnion migrate` up/down/plan, the `up → down → up` CI job on a seeded fixture, `/deployment/migrations` list and detail. *Done when:* CI proves every migration reversible and a hand-broken down script fails the gate.
2. **Policy lint + plan + verify-down.** Banned-pattern lint with waivers, plan preview, scratch-database verify-down, lock view, policy screen. *Done when:* a banned fixture is refused with file and line, a waiver is recorded, and verify-down runs on a scratch database only.
3. **Backfills + seeds + zero-downtime proof.** Backfill descriptors and jobs with cursors, pause/resume, seeds and fixtures from descriptors, the live-traffic recipe demonstration, both screens. *Done when:* a restarted backfill resumes exactly once and the live fixture serves every request through the recipe.
4. **Anonymised exports + upgrade documentation.** Column classification map, export builder and job, watermark/expiry/single-use download/revoke, events and notifications, `/deployment/exports`, upgrade guide finalised with the rollback split. *Done when:* the export grep is clean, the states behave, and the guide's steps match what QA executed.

### Risks / notes

- A down script is a loaded gun: rehearsing it is safe on a scratch database and dangerous on production. The rule is absolute — the panel never offers the reversal on a production-marked environment, and `down` in the runner requires an explicit operator flag that the deployment tooling never sets.
- Editing an applied migration is the most common cause of silent drift: checksums make it loud, and the only fix is a new migration. Contributors need that stated in the contributing guide, not just enforced by the runner.
- Lock contention is the realistic way a safe migration still causes an outage: DDL waits behind a long transaction, and the wait then blocks everything behind it. Timeouts are short by default, the lock screen names the blocker, and index creation always runs concurrently.
- Backfills compete with live traffic for the same rows; batch size, rate limit, optional off-hours windows and a pause button exist so an operator can trade duration for load.
- Anonymisation is allow-list security: the classification table fails closed, hashing uses a per-export salt, and the export file is watermarked, expiring and single-use. A forgotten column is a data incident, so the map is reviewed as part of any migration that adds a personal-data column.
- Seed and fixture data must never reach production: loads are refused by installation kind, and the descriptors live in the repository so tests and demos cannot drift apart.
- Rollback semantics need to be honest per release: when a release ships a destructive migration with a waiver, the upgrade documentation says the database rollback is a restore, and the helper says the same thing at the moment of the decision.
- The migration ledger is an audit surface as much as an operational one: actor, source and duration are recorded for every run, and CI runs are labelled as such so a human review of the trail is possible.
