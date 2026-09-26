# REQ-121 — Plugin System & WASM Runtime

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + `crates/plugins` (new)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Extending the platform without forking it.

- Plugin manifest (name, version, engine range, permissions, hooks, migrations, settings schema).
- Plugin backend: registered routes, background jobs, event subscriptions, hooks (before/after content save, publish, user create…).
- Plugin frontend: admin screens, blocks, widgets, settings panels through the module SDK.
- Plugin-declared permissions (REQ-068) and locales; migrations run with the platform's runner.
- WASM sandbox for untrusted plugin logic with resource limits; lifecycle (install/enable/disable/uninstall) with data retention choices.

## Implementation spec

### Scope (in / out)

**In**

- New crate `crates/plugins`: manifest parsing and validation, the plugin registry, lifecycle (install/enable/disable/uninstall/upgrade), hook dispatch, sandbox execution, settings storage, plugin migrations through the platform runner, permission declaration and locale bundling.
- **Manifest** `plugin.toml`: `key`, `name`, `version` (semver), `engine` range (min/max platform version), `publisher`, `license`, `kind` (`native` compiled into the build | `wasm` sandboxed), `permissions` requested, `hooks` subscribed, `routes` (a path prefix under the plugin namespace), `jobs`, `events` subscribed, `settings_schema` (a documented JSON-Schema subset), `locales` bundled, `migrations` directory, `wasm` path + limits, `retention` class (`keep_data_on_disable` yes/no).
- **Lifecycle**: install (through the REQ-122 pipeline when the source is a registry; from a local path in development), enable, disable, upgrade, uninstall with a retention choice (`keep data` | `purge data`). State is per organization: one organization disabling a plugin affects nobody else, and enablement is checked on every entry point (routes, hooks, jobs, events).
- **Hooks**: synchronous and asynchronous subscriptions to declared hook points — before/after content save, before/after publish, user create, media upload, login. A sync hook may veto a write by returning an error (bounded timeout, documented); anything longer-running is an async event subscription instead. Every hook call is logged with duration and outcome, and a hook that exceeds its failure budget is auto-disabled with a `plugin.failed` event rather than stalling writers.
- **Backend surface**: registered routes under `/api/v1/plugins/{key}/...`, each gated by a declared permission; background jobs registered with the platform's lease-claim runner (same `for update skip locked` pattern as the webhook and notification runners) and stored in `plugin_job_runs`; event subscriptions with a per-plugin rate budget so one plugin cannot flood the bus.
- **Sandbox (wasm)**: WASI-style execution with **no** filesystem and **no** network by default; fuel metering, a memory ceiling and a wall-clock timeout per call class (sync hook default 500 ms, job default 30 s); host functions are explicit: namespaced key-value store, `emit_event` limited to declared names, `http_request` allowed only to hosts in the manifest allowlist, read-only content access gated by permission, secret read by reference (the value is returned to the call, never persisted by the plugin). A plugin that trips fuel, memory or time limits is terminated, recorded and the caller gets a typed timeout error — never a hung request.
- **Frontend contribution** through `packages/plugin-sdk`: admin nav entries, settings panels, dashboard widgets and theme blocks/widgets registered against REQ-083 slots. Components come from a registry the admin loads only for enabled plugins; there is no arbitrary script injection path and no remote code fetch at render time.
- **Declared permissions** must exist in the REQ-068 catalogue or install is refused; new keys a plugin introduces are created in the catalogue during the install gate with a human-readable description from the manifest. Locales ship with the plugin and register with the translation stack (REQ-114).
- **Migrations** run only through the platform migration runner (REQ-129 rules: additive-only; a plugin migration may not drop a table it did not create), recorded per plugin version with checksums so a modified migration file is refused at boot.
- **Admin screens**: plugin list, detail (permissions, hooks, routes, jobs, recent errors), schema-generated settings, the install flow with a permission review, and uninstall with a retention choice.

**Out**

- A hosted third-party registry, moderation, paid plugins (REQ-048 lists entries; distribution assumptions live in REQ-122), hot-reload of native code (a compiled-in plugin needs a process restart; wasm plugin code reloads on upgrade without one), multi-tenant plugin hosting, and any sandbox for the *admin frontend* — plugin UI runs in the admin origin at the same trust level as core screens, and the docs say so plainly.

### Screens (UI)

Admin app.

| Route | Purpose |
|---|---|
| `/settings/plugins` | Installed + available list: status, version, publisher, kind, footprint |
| `/settings/plugins/{key}` | Detail tabs: Overview, Permissions, Hooks, Routes, Jobs, Logs |
| `/settings/plugins/{key}/settings` | Settings form generated from the manifest schema |
| `/settings/plugins/install` | Install flow: source, gate results, permission review, retention |

- **List** columns: name, key, publisher, kind badge (`native` | `wasm`), status chip (enabled, disabled, installed, failed), version with an update badge, footprint, actions. Filters: status, kind, publisher; search by name/key.
- **Detail** — Permissions tab lists each requested key as a human sentence with its rationale and a grant/revoke control; Hooks tab shows each hook with mode, timeout, call count, last error and an enable/disable switch; Routes tab lists served paths; Jobs tab shows registered jobs and recent runs; Logs tab is a bounded, filterable tail (level, time window) scoped to this plugin only.
- **Install flow**: source step (registry entry or uploaded bundle), a results list of the REQ-122 gates, the permission review with descriptions, and the retention choice for the eventual uninstall; the same gate list is what the API returns, so the sheet cannot show a friendlier story than the pipeline.
- Danger zone: uninstall with a radio choice (keep data | purge data) and a typed confirmation for purge; a plugin with dependents lists them and is refused.
- All screens: skeleton/empty/error with retry, `Esc` closes sheets, keyboard path with visible focus, light/dark parity, one column at 390 px.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/plugins` | Registry with per-organization state | `plugins.read` |
| GET | `/api/v1/plugins/{key}` | Detail: permissions, hooks, routes, jobs, settings schema | `plugins.read` |
| POST | `/api/v1/plugins/install` | Plan + execute an install from a registry or bundle | `plugins.install` |
| GET | `/api/v1/plugins/installs/{id}` | Install job progress (REQ-122 phases) | `plugins.read` |
| POST | `/api/v1/plugins/{key}/enable` | Enable for the caller's organization | `plugins.manage` |
| POST | `/api/v1/plugins/{key}/disable` | Disable; data kept, routes 404, jobs paused | `plugins.manage` |
| POST | `/api/v1/plugins/{key}/upgrade` | Move to a newer compatible version | `plugins.install` |
| DELETE | `/api/v1/plugins/{key}` | Uninstall `{retention: keep\|purge, confirm}` | `plugins.uninstall` |
| GET | `/api/v1/plugins/{key}/settings` | Current settings values (secrets by reference) | `plugins.read` |
| PUT | `/api/v1/plugins/{key}/settings` | Save settings, validated against the schema | `plugins.settings.manage` |
| POST | `/api/v1/plugins/{key}/settings/test` | Run the plugin's settings-validation entry point | `plugins.settings.manage` |
| GET | `/api/v1/plugins/{key}/logs` | Bounded log tail for this plugin | `plugins.read` |
| GET | `/api/v1/plugins/{key}/permissions` | Declared keys with grant state | `plugins.read` |
| POST | `/api/v1/plugins/{key}/permissions/{perm}/grant` | Grant one declared permission | `plugins.permissions.manage` |
| POST | `/api/v1/plugins/{key}/permissions/{perm}/revoke` | Revoke one granted permission | `plugins.permissions.manage` |
| POST | `/api/v1/plugins/{key}/hooks/{hook}/disable` | Kill a misbehaving hook immediately | `plugins.manage` |

- Plugin routes live under `/api/v1/plugins/{key}/...`, inherit the `plugin.{key}` namespace for their own permission keys, and are unreachable (`404`) while the plugin is disabled for the organization.
- Errors: `400` invalid settings value or manifest field (field named), `403` permission or organisation-scope miss, `404` unknown or disabled plugin, `409` dependency or permission-catalogue conflict, `422` sandbox limit violation at install time (e.g. a bundle over the size cap).
- New REQ-068 keys, category `plugins`: `plugins.read`, `plugins.install`, `plugins.manage`, `plugins.uninstall`, `plugins.settings.manage`, `plugins.permissions.manage`.

### Data model

Migration `database/migrations/00NN_plugins.sql` (next free slot at land time; additive-only, `0009` commenting style).

- `plugins` — `key text pk` (`^[a-z][a-z0-9_]*$`, ≤ 48); `name text not null`; `publisher text not null`; `kind text not null` check in (`native`,`wasm`); `status text not null default 'installed'` check in (`installed`,`enabled`,`disabled`,`failed`); `version text not null`; `manifest jsonb not null` (`jsonb_typeof = 'object'`); `footprint_bytes bigint not null default 0`; `checksum text null`; `signature_verified boolean not null default false`; `installed_by uuid null`; `installed_at`, `enabled_at`, `updated_at`.
- `plugin_versions` — `id uuid pk`; `plugin_key` fk cascade; `version text not null`; `manifest jsonb not null`; `wasm_sha256 text null`; `state text not null default 'active'` check in (`active`,`superseded`,`failed`); `installed_at`; unique `(plugin_key, version)`.
- `plugin_settings` — `plugin_key` fk cascade; `organization_id uuid null` (null = platform default row); `values jsonb not null default '{}'`; `updated_by`, `updated_at`; unique `(plugin_key, coalesce(organization_id, '00000000-0000-0000-0000-000000000000'))`; secret-valued fields store a secret reference, never a value.
- `plugin_permissions` — `plugin_key` fk cascade; `permission_key text not null`; `rationale text not null` (≤ 200, shown in the review sheet); `granted boolean not null default false`; `granted_by uuid null`; `granted_at`, `revoked_at`; primary key `(plugin_key, permission_key)`.
- `plugin_hooks` — `plugin_key` fk cascade; `hook text not null` (e.g. `content.before_save`, `content.after_publish`, `user.created`); `mode text not null` check in (`sync`,`async`); `timeout_ms int not null default 500`; `enabled boolean not null default true`; `failure_count int not null default 0`; `last_error text null`; `last_called_at timestamptz null`; primary key `(plugin_key, hook)`.
- `plugin_event_subscriptions` — `plugin_key` fk cascade; `event_name text not null`; `budget_per_hour int not null default 1000`; `enabled boolean not null default true`; primary key `(plugin_key, event_name)` — a subscription to an undeclared event name is refused at install.
- `plugin_migrations` — `plugin_key` fk cascade; `version text not null`; `filename text not null`; `checksum text not null`; `applied_at`; `applied_by uuid null`; primary key `(plugin_key, filename)` — written only by the platform runner; a checksum mismatch on boot refuses the plugin with `plugin.failed`.
- `plugin_job_runs` — `id uuid pk`; `plugin_key` fk cascade; `job text not null`; `state text not null default 'queued'` check in (`queued`,`running`,`succeeded`,`failed`); `attempts int not null default 0`; `next_attempt_at timestamptz not null default now()`; `error text null`; `started_at`, `finished_at`, `created_at`; index `(state, next_attempt_at)` for the lease-claim runner, index `(plugin_key, created_at desc)`.
- `plugin_logs` — `id bigserial pk`; `plugin_key` fk cascade; `at`; `level text not null` check in (`debug`,`info`,`warn`,`error`); `message text not null`; `context jsonb not null default '{}'`; index `(plugin_key, at desc)`; pruned on retention (documented window, default 14 days).
- `plugin_dependencies` — `plugin_key` fk cascade; `dependency_key text not null`; `range text not null`; primary key `(plugin_key, dependency_key)` — the resolver reads this without parsing manifests per request.

### Events

| Event | When | Payload |
|---|---|---|
| `plugin.installed` | install job commits | `key`, `version`, `kind` |
| `plugin.enabled` | enabled for an organization | `key`, `organization_id` |
| `plugin.disabled` | disabled for an organization | `key`, `organization_id` |
| `plugin.upgraded` | version change applied | `key`, `from`, `to` |
| `plugin.uninstalled` | removed with retention choice | `key`, `retention` |
| `plugin.failed` | sandbox limit, migration checksum or hook budget trip | `key`, `reason`, `hook?` |
| `plugin.permission.granted` | a declared key is granted | `key`, `permission` |
| `plugin.permission.revoked` | a declared key is revoked | `key`, `permission` |
| `plugin.hook.error` | a hook call fails (logged, counted) | `key`, `hook`, `message` |
| `plugin.settings.changed` | settings saved | `key`, `organization_id` |

Consumed: the plugin's own declared event subscriptions, delivered through the existing bus with the per-plugin rate budget; an undeclared event name cannot be subscribed at install. Deliveries to a disabled plugin are dropped, not queued — the subscription is inactive by definition.

### Acceptance criteria

- [ ] Manifest validation refuses unnamed/duplicate keys, bad semver, an engine range that excludes the running version, an undeclared hook or event name, and an unknown settings-schema keyword — each with the exact field named.
- [ ] Install writes audit rows (REQ-039) and `plugin.installed`; uninstall writes its own; permission grants are audited with actor and time.
- [ ] Permission review lists every requested key as a sentence before install; nothing is granted by default; a plugin route whose key is ungranted returns `403` and logs the attempt.
- [ ] A wasm plugin that loops forever is killed at the fuel or time limit with `plugin.failed`; the triggering request returns a typed error and no admin screen hangs (bounded by the documented timeout).
- [ ] The sandbox denies filesystem and network access unless declared: a test plugin attempting each gets a typed denial, and an `http_request` to a host outside the allowlist is refused.
- [ ] Sync hook veto: a `content.before_publish` hook returning an error blocks the publish with the plugin's message surfaced in the publish UI, and no version is created.
- [ ] A hook exceeding its failure budget (documented count within a window) is auto-disabled with `plugin.failed`, and the affected workflow continues without it.
- [ ] Plugin migrations run only through the platform runner with checksums; a modified file is refused at the next boot with the filename named, and the plugin moves to `failed` without taking the platform down.
- [ ] Disable: routes `404`, jobs stop being claimed (`plugin_job_runs` untouched by the runner), events are dropped, data remains; re-enable restores all three with identical counts.
- [ ] Uninstall with `keep`: rows remain and the plugin is listed as uninstalled-retained; `purge`: a dry-run count precedes removal and counts before/after match exactly.
- [ ] Settings: the generated form and the API both reject invalid values against the schema (server-side proof), and a secret-typed field stores only a reference (verified by inspecting the row).
- [ ] Logs are plugin-scoped: no endpoint returns another plugin's or the core's log lines.
- [ ] Frontend: an enabled plugin's nav entry, settings panel and one widget/block render; disabling removes them (after the next navigation), and no component loads for a disabled plugin (checked in the built output).
- [ ] Native plugins are labelled trusted in the UI; upgrading a wasm plugin reloads its code without a restart while native upgrades state "restart required".
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; every new screen is in the walkthrough inventory.

### QA plan

- Walkthrough: install the example wasm plugin from `plugins/examples` (ships in the repo), read the gate list, review and grant its permissions, enable, open its settings, confirm its nav entry and one block, disable, re-enable, uninstall with keep, then purge.
- Visual review: permission review legibility (sentence per key), hook table with timeouts and errors, log tail rendering, and status chips in light/dark.
- `scripts/qa/probe-plugin-sandbox.cjs` (new, exits non-zero) asserts through the API and a test plugin: infinite loop killed with `plugin.failed`; filesystem/network denial; `http_request` allowlist; ungranted route `403`; event delivered only to enabled plugins; migration checksum refusal.
- Honest note: native plugins require a process restart for code changes — the walkthrough restarts the dev API and says so in the report instead of pretending hot reload exists.

### Slices

1. **Manifest, registry, lifecycle, permissions** — migration, `crates/plugins`, install/enable/disable/uninstall with audit and events. *Done when:* the lifecycle works end to end for a native fixture plugin and all ten events fire in tests.
2. **Sandbox and host functions** — fuel/memory/time limits, kv store, emit, allowlisted HTTP, denials. *Done when:* the probe kills a loop and denies each undeclared capability with typed errors.
3. **Hooks, jobs, event subscriptions** — dispatch, veto semantics, failure budget, leases, rate budget. *Done when:* a before-publish veto blocks a publish with the plugin's message and an over-budget hook disables itself.
4. **Frontend SDK and admin screens** — plugin-sdk contributions, settings schema forms, list/detail/logs. *Done when:* the example plugin contributes nav, settings and a block visible only while enabled.

### Risks / notes

- The wasm sandbox is the security boundary between the platform and third-party code: network and filesystem are off by default, fuel and wall-clock limits are mandatory, and an upgrade that changes the permission set requires a fresh permission review rather than inheriting grants.
- Native plugins are trusted code compiled into the build; the docs and the UI state this, and a native plugin cannot be installed from an uploaded bundle without a rebuild.
- Hook timeouts protect writers: 500 ms default sync budget, auto-disable after the failure budget, and every auto-disable is a visible event and log line — never a silent degradation.
- Plugin migrations are additive-only by policy; a plugin may not touch tables it did not create, and the runner refuses such statements with the offending statement named.
- Frontend trust is explicit: plugin UI runs in the admin origin; there is no iframe isolation in v1 and the docs say so rather than implying a sandbox.
- Footprint and size claims come from measured bytes in the manifest verification test, not from author estimates.
