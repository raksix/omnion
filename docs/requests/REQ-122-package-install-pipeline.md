# REQ-122 — Package Install Pipeline (gates, resolver)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Installing things safely.

- Install pipeline with gates: signature/hash check, engine compatibility, permission review, migration dry-run.
- Dependency and version resolver with a compatibility matrix; conflict blocking that explains the reason.
- Atomic install with rollback on failure; uninstall with data-removal choice.
- Update path for installed packages (patch/minor/major rules) and a changelog view.
- Package registry metadata for the marketplace (REQ-048) and the installer CLI (REQ-131).

## Implementation spec

### Scope (in / out)

**In**

- The install engine behind everything installable — apps, modules, plugins, themes, templates: this REQ defines the pipeline (plan → gates → resolve → atomic apply → rollback), the resolver and the update rules; the installer surface referenced by REQ-044 and the marketplace (REQ-048) calls this pipeline rather than keeping its own copy.
- **Gates**, evaluated in a fixed order during a *plan* (a dry run that changes nothing), each returning pass/fail/warn with a machine-readable detail: (1) **integrity** — SHA-256 hash plus a detached signature over the bundle verified against the trusted key set; unsigned bundles are refused by default, with an organization-level development policy that allows them only for installs recorded in audit and clearly labelled in the UI; (2) **engine compatibility** — the bundle's engine range against the running platform version and migration-runner version; (3) **permission review** — declared permission keys diffed against the REQ-068 catalogue, with new keys surfaced for approval (they land pending, never auto-granted); (4) **migration dry-run** — every migration statement is executed inside a transaction and rolled back, and the plan records the captured effect; statements that cannot run in a transaction are marked "manual verification required" — never silently passed; (5) **footprint** — declared size and table count against free disk and configured limits.
- **Resolver**: semver ranges over package keys, a dependency graph resolved depth-first with conflict detection, a deterministic order (same inputs → identical plan), optional and peer dependencies treated distinctly, and a **compatibility matrix** view (platform version × package version × dependency versions) used by the marketplace and CLI. A conflict is refused with the chain in plain language, naming both requirements (e.g. "X needs Y ≥ 2.0 < 3; Z pins Y = 1.5").
- **Lockfile**: the resolved graph is persisted with the install (`package_dependencies`) so uninstall and upgrades are reproducible from the recorded versions, not re-resolved from a moving world.
- **Atomic apply**: a phase journal (staging → migrations → data → assets → permissions → enable) where each phase records what it did and its compensation; a failure rolls back along the journal, migrations run in the platform runner's transaction rules (REQ-129), and a partial state is always visible as `failed` with the phase, the message and whether rollback completed — never a silent half-install.
- **Uninstall**: reverse-dependency check (blocked with the dependents named unless an explicit cascade is chosen), data retention choice (`keep` | `purge`), and asset removal; the ledger row is kept so the history is auditable.
- **Updates**: patch, minor and major rules as policy (`manual` | auto-patch | auto-minor | auto-major, default manual at major), a pre-update plan so the same gates apply, changelog view from the bundle's version notes, downgrade refused, and an available-update check that emits `package.update_available` rather than silently upgrading.
- **Registry metadata** for the marketplace (REQ-048) and the CLI (REQ-131): the bundle index format (key, version, engine range, dependencies, permissions, size, checksum, signature, changelog) is defined here and read by both.

**Out**

- The marketplace UI, catalogue browsing and submissions (REQ-048); the CLI binary itself (REQ-131); payment and licensing enforcement (REQ-023/134 — a license field is displayed, not policed); hosting a remote registry; unattended auto-upgrade daemons (the check job emits an event; applying is an explicit decision in v1).

### Screens (UI)

Admin app, settings section.

| Route | Purpose |
|---|---|
| `/settings/packages` | Installed packages: version, scope, update badge, footprint, status |
| `/settings/packages/{id}` | Detail tabs: Versions, Dependencies, Permissions, Changelog, Gate history |
| `/settings/packages/updates` | Available updates with policy, changelog summary, one-click update (runs the full plan) |
| install / update / uninstall sheets | Gate checklist, permission diff, dry-run summary, rollback point, retention choice |

- **Install/update sheet**: the five gates as a checklist with pass/fail/warn and the reason for each (a failed gate names the check and the offending value), the permission diff (new keys pending, removed keys noted), the resolved dependency tree, the migration dry-run summary (statement count, tables touched, "manual verification required" markers), and the target scope; the primary button is disabled while any gate fails.
- **Failure view**: names the phase, the gate or apply message, and the rollback result (`rolled back` | `partial — see journal`); the only offered action is a fresh plan, never "resume mid-apply".
- **Detail** tabs mirror the API: Versions (installed history with actor and time), Dependencies (lockfile with resolved versions and pin sources), Permissions (grant state, pending first), Changelog (per version, markdown), Gate history (previous plans with results).
- **Updates** list: package, installed → available version, change class (patch/minor/major), policy tag, changelog excerpt, size delta; a major update requires an explicit confirmation naming the versions.
- All screens: skeleton/empty/error with retry, permission-diff rows readable at 390 px, keyboard path with visible focus, light/dark parity.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/packages` | Installed packages with scope, version, status | `packages.read` |
| GET | `/api/v1/packages/{id}` | Detail: versions, dependencies, permissions, changelog | `packages.read` |
| POST | `/api/v1/packages/plan` | Dry run: `{source, ref, scope}` → gates, resolved graph, migration plan | `packages.plan` |
| GET | `/api/v1/packages/plans/{plan_id}` | Plan result (expires after a documented TTL) | `packages.read` |
| POST | `/api/v1/packages/installs` | Execute a plan (must reference a fresh plan id) | `packages.install` |
| GET | `/api/v1/packages/installs/{id}` | Phase journal and result | `packages.read` |
| POST | `/api/v1/packages/installs/{id}/cancel` | Cancel before apply starts; between phases during apply | `packages.install` |
| GET | `/api/v1/packages/{id}/updates` | Available updates with change class and policy | `packages.read` |
| POST | `/api/v1/packages/{id}/updates/{version}/plan` | Plan an update (same gates) | `packages.plan` |
| POST | `/api/v1/packages/{id}/updates/{version}/apply` | Apply a planned update | `packages.update` |
| POST | `/api/v1/packages/{id}/uninstall` | `{retention: keep\|purge, cascade, confirm}` | `packages.uninstall` |
| GET | `/api/v1/packages/{id}/changelog` | Version notes from bundles | `packages.read` |
| GET | `/api/v1/packages/gates/policy` | Signature policy (org scoped) | `packages.policy.manage` |
| PUT | `/api/v1/packages/gates/policy` | Update signature/unsigned policy (audited) | `packages.policy.manage` |

- Errors: `400` malformed source or reference, `403` permission miss, `404` unknown package, `409` executing a stale or already-consumed plan, `422` a failed gate (detail carries the gate and reason) or a blocked uninstall with dependents.
- New REQ-068 keys, category `packages`: `packages.read`, `packages.plan`, `packages.install`, `packages.update`, `packages.uninstall`, `packages.policy.manage`. Executing an install always requires a plan produced by the same actor within the TTL.

### Data model

Migration `database/migrations/00NN_package_pipeline.sql` (next free slot at land time; additive-only). The marketplace (REQ-048) reads this ledger — `packages` and `package_installs` are the single source of install truth, and no second ledger exists.

- `packages` — `id uuid pk`; `key text not null`; `kind text not null` check in (`app`,`module`,`plugin`,`theme`,`template`); `scope text not null` check in (`organization`,`global`); `organization_id uuid null` (required when scope = organization); `name text not null`; `description text not null default ''`; `current_version text not null`; `status text not null default 'installed'` check in (`installed`,`disabled`,`failed`,`uninstalled`); `source text not null` check in (`registry`,`local`,`bundled`); `installed_by uuid null`; `installed_at`, `updated_at`; unique `(coalesce(organization_id, '00000000-0000-0000-0000-000000000000'), lower(key))`.
- `package_versions` — `id uuid pk`; `package_id` fk cascade; `version text not null` (semver shape check); `manifest jsonb not null`; `checksum_sha256 text not null`; `signature text null`; `signature_verified boolean not null default false`; `key_fingerprint text null`; `size_bytes bigint not null default 0`; `released_at timestamptz null`; `yanked boolean not null default false`; unique `(package_id, version)`.
- `package_install_plans` — `id uuid pk`; `requested_by uuid not null`; `source text not null`; `request jsonb not null`; `resolved jsonb not null` (graph + deterministic order); `gate_summary jsonb not null` (per-gate state and detail); `migration_plan jsonb not null` (statement summaries — never literal secret values); `expires_at timestamptz not null`; `consumed_at timestamptz null`; `created_at`; index `(expires_at)`.
- `package_installs` — `id uuid pk`; `plan_id` fk; `state text not null default 'queued'` check in (`queued`,`gates`,`applying`,`rolling_back`,`succeeded`,`failed`,`rolled_back`,`cancelled`); `phase text null`; `journal jsonb not null default '[]'` (per phase: action, result, compensation); `error text null`; `requested_by`; `started_at`, `finished_at`, `created_at`; index `(state, created_at)` for the runner claim.
- `package_dependencies` — `package_version_id` fk cascade; `dependency_key text not null`; `range text not null`; `resolved_version text not null`; `kind text not null` check in (`runtime`,`optional`,`peer`); primary key `(package_version_id, dependency_key)` — the lockfile for install, uninstall and upgrade.
- `package_gate_results` — `id uuid pk`; `plan_id` fk cascade; `gate text not null` check in (`integrity`,`engine`,`permissions`,`migrations`,`footprint`); `state text not null` check in (`pass`,`fail`,`warn`,`skipped`); `detail jsonb not null default '{}'`; `checked_at`; index `(plan_id)`.
- `package_permission_grants` — `package_id` fk cascade; `permission_key text not null`; `state text not null default 'pending'` check in (`pending`,`granted`,`revoked`); `granted_by uuid null`; `granted_at`, `revoked_at`; primary key `(package_id, permission_key)`.
- `package_update_policy` — `organization_id` fk cascade; `package_key text not null` (`*` = default for the organization); `policy text not null default 'manual'` check in (`manual`,`auto_patch`,`auto_minor`,`auto_major`); `set_by uuid null`; `updated_at`; primary key `(organization_id, package_key)`.

### Events

| Event | When | Payload |
|---|---|---|
| `package.install_planned` | a plan is produced | `plan_id`, `keys[]`, `gate_summary` |
| `package.gate_failed` | a gate fails during planning | `plan_id`, `gate`, `reason` |
| `package.install_started` | apply begins | `install_id`, `plan_id` |
| `package.installed` | apply succeeds (REQ-048 consumes this) | `install_id`, `packages[{key,version}]`, `scope` |
| `package.install_failed` | apply fails (REQ-048 consumes this) | `install_id`, `phase`, `message`, `rolled_back` |
| `package.install_rolled_back` | rollback completes | `install_id`, `phases_reversed[]` |
| `package.uninstalled` | removal commits | `package_id`, `key`, `retention` |
| `package.update_available` | the check job finds a newer compatible version | `package_id`, `current`, `available`, `change_class` |
| `package.updated` | an update applies | `package_id`, `from`, `to`, `change_class` |
| `package.signature_rejected` | integrity gate refuses a bundle | `source`, `key`, `reason` (no payload contents) |

Consumed: none — this REQ is a producer. REQ-048 (marketplace Installed view) and REQ-121's install flow consume `package.installed` / `package.install_failed` as defined here, and the event names are frozen for that reason. Payloads carry ids, keys, versions and gate names only.

### Acceptance criteria

- [ ] An unsigned bundle is refused with gate `integrity` and a named reason; the development override requires `packages.policy.manage`, is written to audit, and the UI labels the resulting install as unsigned wherever it appears.
- [ ] A tampered bundle (hash mismatch) and a bundle whose signature does not verify against the trusted key set are each refused with the fingerprint named.
- [ ] An engine-range mismatch is refused naming the required range and the running version.
- [ ] Resolver: conflicting constraints fail with the full chain (both requirements named); a diamond dependency resolves once; two identical plan requests produce byte-identical resolved graphs (determinism test).
- [ ] The lockfile is persisted with the install and an uninstall uses the recorded resolved versions, not a fresh resolution (proved by uninstalling after the registry changed).
- [ ] Permission gate: new keys land `pending` and are listed in the plan; routes behind them `403` until granted, and revocation restores the refusal.
- [ ] Migration dry-run captures the statement plan and leaves the schema unchanged (migration table and `information_schema` compared before/after); a non-transactional statement is marked "manual verification required" in the plan — never silently passed.
- [ ] A forced failure mid-apply rolls back: no partial table or half-written assets remain (checked by counts and object listing), the install is `failed` with the phase, and `package.install_failed` carries `rolled_back: true`.
- [ ] Cancel before apply is clean; cancel during apply is honoured between phases and refused mid-phase with the state shown — never a silent no-op.
- [ ] Uninstall with dependents is refused naming them; an explicit cascade plus confirmation removes dependents in reverse order, and `keep` retains data rows (counts) while `purge` removes them with matching dry-run counts.
- [ ] Update policy: patch flows under `auto_patch` without a review step, minor and major always produce a plan; a downgrade request is refused naming both versions; the changelog view matches the version rows.
- [ ] Executing a stale plan (expired or already consumed) returns `409`; executing another actor's plan is refused.
- [ ] Permission matrix holds: `packages.read` cannot plan; `packages.plan` cannot execute; policy changes require `packages.policy.manage` and write audit rows.
- [ ] A stale `applying` install (worker restart) is reclaimed by the lease and surfaced with a banner offering a fresh plan; no double apply occurs.
- [ ] The marketplace listed-package install (REQ-048 flow) produces the same gate sheet and ledger rows as a CLI-driven install for the same bundle (parity check).
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; every new screen is in the walkthrough inventory.

### QA plan

- Walkthrough uses a bundled content-only example package (safe to install repeatedly): open the plan sheet, read all five gates, install, then uninstall with `keep` and again with `purge`; force a gate failure using a bad-checksum fixture and confirm the failure view names the gate and blocks execution.
- Visual review: gate checklist legibility (reason visible per failed gate), permission diff rows, the resolved tree, rollback result text, and update list change-class tags.
- `scripts/qa/probe-package-pipeline.cjs` (new, exits non-zero) asserts through the API: each gate failure path with fixtures, plan determinism (two runs equal), `409` on stale plan, rollback leaves no partial schema, uninstall dependents blocked, `package.installed` / `package.install_failed` / `package.install_rolled_back` on the event feed.
- Honest note: real signed bundles in QA come from the test fixture key; the report states the key fingerprint used and never claims a production key was exercised.

### Slices

1. **Package store, plan endpoint, gates integrity + engine** — migration, ledger, plan lifecycle with TTL, gate results. *Done when:* bad checksum, bad signature and engine mismatch are refused with named reasons and `package.gate_failed`.
2. **Resolver, lockfile, conflict explanation** — graph resolution, determinism, dependency persistence, compatibility matrix read for REQ-048/131. *Done when:* conflict chains and deterministic output are proven by tests.
3. **Apply, rollback, installs API, uninstall** — phase journal, compensating actions, retention choices, cancel semantics. *Done when:* a forced mid-apply failure rolls back to a clean state.
4. **Updates, policy, changelog, permissions gate** — change classes, update plans, `package.update_available`, permission grant flow, changelog view. *Done when:* auto-patch applies and a major update requires the explicit confirmation in QA.

### Risks / notes

- Signing keys are operationally sensitive: key rotation and revoked-key handling are documented, a revoked key refuses new installs while recorded verifications stay readable, and the policy override is audited and loudly labelled in every surface that shows the package.
- The dry-run never touches production data: it runs in a transaction that is rolled back, and anything the runner cannot roll back is marked for manual verification — a dry run that cannot verify must not masquerade as a pass.
- Rollback of data migrations is best-effort: the journal records exactly what was compensated, and the failure view distinguishes `rolled back` from `partial` so nobody reads a partial state as clean.
- Restart during apply: the lease reclaims the install, and the only offered recovery is a fresh plan — resuming mid-journal is deliberately not supported in v1.
- The resolver refuses moving targets: ranges resolve to concrete versions recorded in the lockfile, and a yanked version already installed stays installed until an explicit update (with a warning).
- Gate ordering is fixed and documented; changing it is a breaking change to the plan format the marketplace and CLI read, so it ships with a plan schema version bump.
