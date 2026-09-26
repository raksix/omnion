# REQ-124 — Module Manager & Selective Installation

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + core
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

A huge platform that installs small.

- Module registry: installed/available modules with description, version, dependencies, size of footprint.
- Install/enable/disable per module with dependency checks and data retention choice.
- Tiers: core, business suite, industry add-ons; opt-in per organization.
- Per-module navigation contribution (menus appear only when the module is enabled).
- "Minimal install" profile for a pure CMS deployment (the onboarding default).

## Implementation spec

### Scope (in / out)

**In**

- **Module registry**: first-party features live in the `modules/` tree (docs/04-MONOREPO.md keeps `modules/` for user-toggleable features and `crates/` for infrastructure) and each ships a manifest `modules/<key>/module.toml`: `key`, `name`, `summary`, `version`, `tier` (`core` | `business` | `industry`), optional `industry` label, `requires` (module keys with version ranges), `optional` integrations, `permissions_namespace`, `admin_nav` contributions, `footprint_bytes` (build artifact size), `migration_count`, `source` (`bundled` | `package`). The registry is assembled at boot by upserting manifests into `modules`; a manifest change is visible after restart by design, and a first-seen change emits `module.registered`.
- **Per-organization state**: `enabled` (active), `disabled` (data retained, UI hidden, jobs paused), `uninstalled_kept` (removed from the build's active set but data retained), or absent (available). Core modules cannot be disabled or uninstalled. Enablement is scoped to one organization and checked at every entry point — routes, nav, palette entries, jobs and events.
- **Dependency checks**: enabling a module with unmet requirements enables the required set in one all-or-nothing action (the confirmation lists exactly what will be enabled); disabling a module with enabled dependents is refused with the full chain named, never partially applied; a purge of a module others depend on is refused.
- **Data retention choice**: disable always keeps data. Uninstall offers `keep` (rows stay, module shows uninstalled-retained, re-enabling restores without loss) or `purge` (a dry-run count of every affected table precedes removal; removal follows the documented dependency order; counts before/after must match). `module_data_inventory` caches the per-table counts so the UI never counts live during a page render.
- **Tiers and profiles**: core (always on), business suite (the CRM/HR/ERP-shaped set), industry add-ons (vertical features). A profile is a named enable/disable set applied in one confirmed action: `minimal` = core + `cms` only (the onboarding default, REQ-050), plus `business` and `custom`. Each apply is one audit row and one event, and the result is verifiable from the registry rather than inferred.
- **Navigation contribution**: admin nav, command-palette entries (REQ-032) and renderer routes come only from enabled modules; a disabled module contributes nothing anywhere and its settings screens are not routed. `/api/v1/modules/nav` is the single feed the admin shell reads, so the menu can never drift from the enabled set.
- **Footprint reporting**: bytes of compiled assets, applied migration count and table count per module (manifest-declared and verified against the build in a test), so "installs small" is a number, not a slogan.
- **Admin surface**: cards-by-tier list, module detail with the dependency graph, and the disable/uninstall impact sheets.

**Out**

- The install machinery and sandbox for third-party extensibility (REQ-121 plugins, REQ-122 pipeline) — this REQ is the product-facing manager for first-party modules; licensing/edition gating of tiers (REQ-134; until then tiers are labels, stated honestly in the UI); per-module billing; cross-organization module state; uninstalling core.

### Screens (UI)

Admin app.

| Route | Purpose |
|---|---|
| `/settings/modules` | Cards by tier: state, version, dependencies, footprint, actions |
| `/settings/modules/{key}` | Detail: description, dependency graph, permissions, migrations applied, size, danger zone |
| `/settings/modules/{key}/disable` | Impact sheet: what disappears, dependents, retention note |
| `/setup/profile` | Onboarding profile picker: minimal (CMS) / business / custom |

- **Cards**: state chip (enabled, disabled, available, uninstalled — data kept), tier grouping with tier headers and counts, dependency chips with met/unmet styling, footprint line (artifact size, N migrations, M tables), and the action set (Enable, Disable, Uninstall, Open). Core cards render locked with a tooltip explaining why.
- **Detail**: dependency graph (required and required-by, each row linking), permissions namespace with a jump to REQ-068, applied migrations list with timestamps, data inventory table (table, estimated rows, last measured), and the danger zone.
- **Disable impact sheet**: exactly which nav items, palette entries and routes disappear, which jobs pause, and a statement that data is retained; when dependents exist the sheet lists them and the primary action is disabled with the reason shown (the API refuses anyway).
- **Uninstall sheet**: retention radio (`keep data` | `purge data`) with the dry-run counts for purge, a typed module-key confirmation for purge, and the dependency order that will be followed.
- **Profile picker** (also reachable from setup): each profile card lists exactly the modules it toggles, and applying shows a diff preview (enabling N, disabling M) before confirmation; the minimal card states plainly which modules remain.
- All screens: skeleton/empty/error with retry, `Esc` closes sheets, keyboard path with visible focus, light/dark parity, one column at 390 px.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/modules` | Registry with per-organization state, tiers, deps, footprint | `modules.read` |
| GET | `/api/v1/modules/{key}` | Detail: dependencies, permissions, migrations, inventory | `modules.read` |
| POST | `/api/v1/modules/{key}/enable` | Enable with required modules (all-or-nothing) | `modules.manage` |
| POST | `/api/v1/modules/{key}/disable` | Disable; refused while dependents are enabled | `modules.manage` |
| POST | `/api/v1/modules/{key}/uninstall` | `{retention: keep\|purge, confirm}` | `modules.uninstall` |
| POST | `/api/v1/modules/{key}/purge/preview` | Dry-run counts of the data purge would remove | `modules.uninstall` |
| GET | `/api/v1/modules/nav` | Nav/palette/toute contributions for enabled modules | `modules.read` |
| GET | `/api/v1/modules/profiles` | Profiles with their toggled sets | `modules.read` |
| POST | `/api/v1/modules/profiles/{id}/apply` | Bulk enable/disable in one confirmed action | `modules.manage` |
| GET | `/api/v1/modules/footprint` | Measured footprint table for the running build | `modules.read` |
| GET | `/api/v1/modules/jobs/{id}` | Progress of purge or profile-apply jobs | `modules.read` |

- Errors: `400` malformed key, `403` permission miss, `404` unknown or not-in-build module, `409` disabling/removing core or a module with enabled dependents (the chain is in the detail), `422` a profile apply that would disable core or break a dependency.
- New REQ-068 keys, category `modules`: `modules.read`, `modules.manage`, `modules.uninstall`. Core modules refuse `disable` and `uninstall` with a structured error naming the reason (`core_module`) — the UI locks the controls and never offers an action the API would refuse.

### Data model

Migration `database/migrations/00NN_module_manager.sql` (next free slot at land time; additive-only).

- `modules` — `key text pk`; `name text not null`; `summary text not null default ''`; `version text not null`; `tier text not null` check in (`core`,`business`,`industry`); `industry text null`; `requires jsonb not null default '[]'` (module keys + ranges); `optional_deps jsonb not null default '[]'`; `permissions_namespace text not null`; `nav jsonb not null default '[]'` (nav, palette and route contributions declared by the manifest); `footprint_bytes bigint not null default 0`; `migration_count int not null default 0`; `source text not null default 'bundled'` check in (`bundled`,`package`); `in_build boolean not null default true`; `updated_at` — written by the boot-time manifest upsert, never by request handlers.
- `organization_modules` — `organization_id` fk cascade; `module_key` fk `modules(key)`; `state text not null` check in (`enabled`,`disabled`,`installed`,`uninstalled_kept`); `enabled_at`, `disabled_at`; `retention text null` check in (`keep`,`purge`); `updated_by uuid null`; `updated_at`; primary key `(organization_id, module_key)`, index `(organization_id, state)` — the read path for nav and route guards.
- `module_jobs` — `id uuid pk`; `organization_id` fk cascade; `module_key` fk; `kind text not null` check in (`enable`,`disable`,`uninstall`,`purge`,`profile_apply`); `state text not null default 'queued'` check in (`queued`,`running`,`succeeded`,`failed`); `phase text null`; `detail jsonb not null default '{}'` (profile id, per-module outcomes); `error text null`; `requested_by uuid null`; `created_at`, `finished_at`; index `(state, created_at)` for the lease-claim runner.
- `module_data_inventory` — `module_key` fk; `table_name text not null`; `row_count_estimate bigint not null default 0`; `measured_at timestamptz not null default now()`; primary key `(module_key, table_name)` — refreshed by the purge dry-run and a maintenance job, so page renders never count live tables.
- Audit uses the existing central log (REQ-039) with actions `module.enabled`, `module.disabled`, `module.uninstalled`, `module.purged`, `module.profile_applied`; no separate audit table exists, and the module detail reads its history from there.

### Events

| Event | When | Payload |
|---|---|---|
| `module.registered` | boot upsert sees a new or changed manifest | `key`, `version`, `tier` |
| `module.enabled` | enabled for an organization (including via a profile) | `key`, `organization_id`, `via_profile?` |
| `module.disabled` | disabled for an organization | `key`, `organization_id` |
| `module.uninstalled` | removed with a retention choice | `key`, `organization_id`, `retention` |
| `module.purged` | data purge completed | `key`, `tables_cleared`, `rows_removed` |
| `module.profile.applied` | a profile finished applying | `profile`, `organization_id`, `enabled[]`, `disabled[]` |
| `module.job.failed` | a job failed with its phase and message | `job_id`, `key`, `phase`, `message` |
| `module.dependency_blocked` | a disable or purge was refused for dependents | `key`, `dependents[]`, `action` |

Consumed: none strictly required. The onboarding flow (REQ-050) applies the minimal profile through the API; nav consumers listen for `module.enabled` / `module.disabled` or simply re-read `/api/v1/modules/nav`. Disabled modules stop receiving events and stop having jobs claimed — the runners filter on organization state, and a test asserts each runner honours the filter.

### Acceptance criteria

- [ ] The registry lists every module in the build with tier, dependencies and footprint; a manifest mutation appears after a restart with `module.registered`.
- [ ] Enabling a module with unmet requirements enables the required set in one confirmed all-or-nothing action; a failure mid-way leaves every module in its previous state (no partial enable).
- [ ] Disabling a module with an enabled dependent is refused with the full chain named, and the state is unchanged (no partial write, no orphaned nav entry).
- [ ] Core modules refuse disable and uninstall with `core_module`; the cards render locked and the tooltip explains.
- [ ] Disabled module: routes `404`, `/api/v1/modules/nav` omits its contributions, jobs are no longer claimed, and data rows remain — re-enabling restores all three with identical counts.
- [ ] Uninstall `keep`: rows remain, the module shows uninstalled with data kept, and re-enabling restores without loss; `purge`: dry-run counts match the counts actually removed (per table), and the order follows the documented dependency order.
- [ ] Purging a module that other modules depend on is refused with the dependents named.
- [ ] Profile `minimal` applied to a fresh organization leaves only core plus `cms` enabled, and the admin nav renders CMS-only; `business` re-enables the suite in one action; each apply writes one audit row and one `module.profile.applied`.
- [ ] Minimal-install honesty check: on a fresh minimal organization the reported footprint and enabled set contain no business module, and the nav feed contains no business contribution.
- [ ] Footprint numbers match the build: the verification test compares manifest bytes with the produced artifacts and fails on drift (no invented sizes on screen).
- [ ] Disabled-module permissions: keys of a disabled module are inactive (no route accepts them) yet assignments survive; re-enabling restores the effective-permission set exactly (diff empty).
- [ ] Permission matrix holds: `modules.read` cannot enable; `modules.manage` cannot uninstall; purge requires `modules.uninstall` plus the typed confirmation.
- [ ] Job visibility: a running purge shows progress and a failure names the phase and message; retry is offered only for `keep`-safe steps, and a failed purge reports exactly which tables were cleared before failing.
- [ ] Tiers are labels: with no edition configuration, a business module enables without any license prompt, and the UI does not imply otherwise (a documented limitation until REQ-134).
- [ ] All screens reach skeleton, empty, running, failure and success states; light/dark parity, keyboard path and 390 px layout pass.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; `/settings/modules` and `/setup/profile` are in the walkthrough inventory.

### QA plan

- Walkthrough on a fresh seeded organization: confirm the minimal default, enable a business module and watch its nav appear, try to disable a dependency of it and read the refusal chain, then run a purge dry-run on a demo module and read the counts.
- Visual review: card grid alignment across tiers, state and dependency chips, locked core styling, the disable impact sheet's legibility, and the profile diff preview.
- `scripts/qa/probe-modules.cjs` (new, exits non-zero) asserts through the API: dependency blocking, all-or-nothing enable, nav contributions appearing/disappearing, purge dry-run counts equal to removed counts, footprint equality with the build, and the `403`/`409` matrix.
- Honest note: purge is exercised only against demo data, and the report states which module was purged and the counts observed.

### Slices

1. **Registry, manifests, boot upsert, read endpoints** — migration, manifest parsing, `module.registered`, list/detail/nav/footprint. *Done when:* every module lists with correct dependencies and footprint from manifests.
2. **Enable/disable, dependency rules, job pausing, audit** — state transitions, all-or-nothing enable, refusal chains, runner filters. *Done when:* blocking and all-or-nothing are proven by tests.
3. **Purge with dry-run inventory, uninstall, retention** — inventory measurement, purge ordering, keep/purge semantics. *Done when:* preview counts equal removed counts per table.
4. **Profiles, nav contribution, onboarding default, admin cards** — profile sets and apply, minimal default in REQ-050 onboarding, cards-by-tier UI. *Done when:* the minimal profile yields a CMS-only nav end to end.

### Risks / notes

- Purge is destructive: dry-run counts, typed confirmation, an audit row, and a hard rule that module data lives only in module-namespaced tables — a shared-table reference refuses the purge and names the table rather than guessing.
- Footprint is inherently approximate for compiled code: the manifest declares artifact bytes and a test verifies them; the UI labels the number as a build-time measurement, never as runtime memory.
- Tiers are labels until REQ-134 turns them into gates; the code reads that gate from one capability check rather than scattered edition branches, so the switch is a one-line change later.
- Job pausing by organization state needs runner cooperation; a test asserts each runner (notification, webhook, module jobs) filters on `organization_modules`, so a disabled module cannot keep consuming queues.
- Disable hides UI but keeps data: the docs must say that disabling is not a security boundary, and permission assignments are retained by design (they are only meaningful when routes are reachable).
- Manifest upsert at boot means a manifest edited on disk is not read mid-run; the UI shows the registry version the process actually loaded, so there is no confusion between repository state and running state.
