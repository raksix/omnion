# REQ-036 — Offline / Air-gapped Mode

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform / infra
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Especially for enterprises — the system runs without any internet access:

```text
Internet
   X

Omnion
 ├── Local LLM
 ├── Local Storage
 ├── Local Registry
 └── Local Marketplace Mirror
```

## Notes

- Builds on the local-AI support defined in docs/06-AI-HUB.md §19 (Ollama/vLLM).

## Implementation spec

### Scope (in / out)

**In**

- An installation-level **offline mode** flag enforced by one egress guard that every outbound client goes through (AI provider calls, package and marketplace fetches, update checks, webhook deliveries, hosted model discovery).
- An **offline bundle** format (`.omnionbundle`, a signed `tar.zst`) carrying container image archives, module/theme packages, a model manifest and a checksum manifest.
- Bundle **import by file** — the supported ingress on an isolated network — with signature verification, a dry-run preview and an explicit apply step.
- A **local service registry**: the model endpoint, object storage, container registry and package index this installation talks to, each validated for reachability without any call to the public internet.
- An **environment doctor**: egress check, local endpoint reachability, certificate trust, clock skew, bundle freshness and model availability.
- Suppressed-and-queued webhook deliveries: while the flag is on, deliveries stay queued; the queue depth is visible and drains when the flag is cleared.

**Out**

- Removing online-only features from the codebase. Offline mode is a runtime posture, not a fork.
- A general-purpose egress proxy or an editor for arbitrary internet allow-lists.
- Licence servers, telemetry upload, hosted model discovery and version-check pings.
- Downloading model weights automatically — the operator imports them into the local store.

### Screens (UI)

- `/offline` — **Offline mode** overview. Header card: state chip (`Online` / `Offline`), enabled-by, enabled-at, last egress check, queued webhook deliveries. Body: a readiness checklist of six rows (Egress blocked · Model endpoint · Object storage · Registry · Package index · Clock in sync), each with a status chip and a **Recheck** button, plus an activity feed of the last 20 offline events. A persistent banner marks the panel while the flag is on.
- `/offline/bundles` — table: **Bundle · Version · Kind · Size · SHA-256 · Signature · Imported by · Imported at · Status**. Kind is one of `app-image` `module` `theme` `model` `data`; status is one of `staged` `verified` `applied` `failed`. Filters: kind, status, imported-by, date range, free text. Bulk actions: **Verify · Apply · Delete** (typed name confirmation) **· Export manifest**. Row click opens a drawer with the manifest and the per-item file list.
- Import drawer form: **Bundle file** (required, `.omnionbundle`) · **Verify signature** (checkbox, on, locked on in offline mode) · **Mode** (Dry run / Apply, default Dry run) ·
  **Notes** (≤ 500 chars). Validation: extension, size limit, checksum present, signature valid against the installation trust key, and every bundled item's compatibility range satisfied by the running version — a failing item blocks **Apply** but not **Dry run**.
- `/offline/mirrors` — table: **Kind · Base URL · Enabled · Last checked · Last status · Last sync**. Kinds: `registry` `package-index` `model-store`. Filters: kind, enabled, status. Row actions: **Test · Sync now · Edit · Disable**. Add/edit form: Kind · Base URL (scheme + host, credentials in the URL rejected) · CA bundle (optional PEM upload) · Sync interval (minutes, 5–10080) · Enabled.
- States: skeleton rows while loading; real empty states on all three tables ("No bundles imported yet — import a bundle to run this installation offline."); an error state with a retry control; a failed bundle stays visible with its error text instead of disappearing.
- Keyboard: `/` focuses search, `n` opens the import drawer, `Esc` closes it, `⌘K` falls through to the command palette. Mobile: tables become card lists, the import drawer becomes a full-height sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/offline` | State, readiness rows, queued delivery count | `offline.read` |
| POST | `/api/v1/offline/enable` | Turn offline mode on (reason required) | `offline.manage` |
| POST | `/api/v1/offline/disable` | Turn offline mode off | `offline.manage` |
| POST | `/api/v1/offline/check` | Run the environment doctor | `offline.manage` |
| GET | `/api/v1/offline/bundles` | List imported bundles | `offline.read` |
| POST | `/api/v1/offline/bundles` | Upload a bundle (multipart) | `offline.manage` |
| GET | `/api/v1/offline/bundles/{id}` | Bundle detail + manifest | `offline.read` |
| POST | `/api/v1/offline/bundles/{id}/verify` | Re-verify signature and compatibility | `offline.manage` |
| POST | `/api/v1/offline/bundles/{id}/apply` | Apply a verified bundle | `offline.manage` |
| DELETE | `/api/v1/offline/bundles/{id}` | Remove a bundle record and its staged files | `offline.manage` |
| GET | `/api/v1/offline/mirrors` | List local mirror sources | `offline.read` |
| POST | `/api/v1/offline/mirrors` | Add a mirror source | `offline.manage` |
| PATCH | `/api/v1/offline/mirrors/{id}` | Edit a mirror source | `offline.manage` |
| DELETE | `/api/v1/offline/mirrors/{id}` | Remove a mirror source | `offline.manage` |
| POST | `/api/v1/offline/mirrors/{id}/sync` | Sync a mirror now | `offline.manage` |
| GET | `/api/v1/offline/doctor` | Machine-readable doctor report | `offline.read` |

Errors: `403` without the permission, `409` when a bundle is already applied, `422` with a per-item list for compatibility failures, `413` for an oversized upload.

### Data model

`database/migrations/0011_offline_mode.sql`:

- `installation_offline_state` — `id uuid pk`, `enabled boolean not null default false`, `enabled_at timestamptz`, `enabled_by uuid`, `reason text`, `last_check_at timestamptz`, `last_check jsonb`, `updated_at timestamptz not null default now()`. A single seeded row.
- `offline_bundles` — `id uuid pk`, `name text not null`, `version text not null`, `kind text not null check (kind in ('app-image','module','theme','model','data'))`, `size_bytes bigint not null`, `sha256 text not null`, `signature text`, `signed_by text`, `manifest jsonb not null default '{}'`, `status text not null default 'staged'` `check (status in ('staged','verified','applied','failed'))`, `error text`, `imported_by uuid`, `imported_at timestamptz not null default now()`, `applied_at timestamptz`. Unique `(name, version, kind)`.
- `offline_mirror_sources` — `id uuid pk`, `kind text not null` `check (kind in ('registry','package-index','model-store'))`, `base_url text not null`, `ca_bundle_pem text`, `enabled boolean not null default true`, `sync_interval_minutes int not null default 60 check (sync_interval_minutes between 5 and 10080)`, `last_checked_at timestamptz`, `last_status text`, `last_error text`, `created_by uuid`, `created_at timestamptz not null default now()`. Unique `(kind, base_url)`.
- `offline_sync_runs` — `id bigserial pk`, `mirror_source_id uuid not null references offline_mirror_sources(id) on delete cascade`, `started_at timestamptz not null default now()`, `finished_at timestamptz`, `status text not null default 'running'`, `items int not null default 0`, `bytes bigint not null default 0`, `error text`.

Indexes: `offline_bundles_status_idx (status, imported_at desc)`, `offline_bundles_kind_idx (kind)`, `offline_sync_runs_source_idx (mirror_source_id, started_at desc)`. A CA bundle is public material; no credential is ever stored in a mirror row.

### Events

- `offline.mode.enabled` — `{enabled_by, reason}`; `offline.mode.disabled` — `{enabled_by}`.
- `offline.bundle.imported` · `offline.bundle.applied` · `offline.bundle.failed`.
- `offline.mirror.synced` — `{kind, base_url, items}`.
- `offline.egress.blocked` — emitted when a guarded client is called while offline, instead of failing silently.

Webhook relevance: while the flag is on, outbound deliveries stay queued rather than failing; `/offline` shows the depth, and `offline.mode.disabled` is the signal that unblocks the drain. No webhook endpoint is ever called from the offline path.

### Acceptance criteria

- [ ] Enabling offline mode requires a typed confirmation and records actor and reason.
- [ ] With the flag on, every guarded outbound client is refused before a socket is opened, and the refusal is counted and shown on `/offline`.
- [ ] `POST /offline/check` returns one row per readiness item with `ok` / `warn` / `fail` and a readable detail string.
- [ ] A bundle with an invalid signature is rejected with `422` and is never unpacked.
- [ ] A bundle whose item compatibility range mismatches the running version is blocked at apply with a per-item reason, while dry run still returns the report.
- [ ] A verified bundle applies and shows status `applied` with a timestamp.
- [ ] Bundles and mirror sources list, filter and search; both tables have real empty states.
- [ ] Deleting a bundle requires typing its name and removes staged files from local storage.
- [ ] Mirror URLs reject embedded credentials and out-of-range sync intervals.
- [ ] A webhook delivery created while offline stays queued (not failed) and drains after the flag is cleared.
- [ ] `/offline` shows the queued delivery count and the last check result.
- [ ] Every mutation writes an audit row with actor and target.
- [ ] `offline.read` alone gets `403` on every mutation.
- [ ] The doctor report is identical through the API and on the screen.
- [ ] `cargo test --workspace` and `pnpm typecheck && pnpm build` pass.
- [ ] The QA walkthrough walks the three routes with zero high findings.

### QA plan

- The browser walkthrough visits `/offline`, `/offline/bundles` and `/offline/mirrors`; the walkthrough inventory gains those three routes.
- Controls to exercise: **Run check** (six rows render), the offline toggle with its confirm dialog (accept and cancel), **Import bundle** against a small fixture bundle in dry-run then apply, row actions **Verify / Apply / Delete** on that fixture, mirror add / edit / test / sync, both table filters, and the empty state after deleting the fixture.
- API-level checks: a guarded client is refused while offline; a tampered signature is rejected; a webhook delivery stays queued while offline and drains after disabling.
- The visual check must see: a persistent "Offline mode active" banner, status shown by icon
  **and** text (never colour alone), no request leaving the page to a public CDN (fonts, icons, logos), readable tables at 1280 px and card layout under 640 px, and no raw JSON in the page.

### Slices

1. **Schema + egress guard + doctor** — migration `0011_offline_mode.sql`, the guarded client wrapper with its refusal counter, the core `offline` routes and the `/offline` screen.
   *Done:* enabling the flag blocks a guarded call in a test, the doctor returns six rows, and `/offline` renders them.
2. **Bundles** — upload, signature/checksum verification, dry run, apply, list and detail drawer.
   *Done:* a fixture bundle imports, verifies and applies; a tampered one is rejected with `422`; `/offline/bundles` shows both outcomes.
3. **Mirrors + webhook queue** — mirror CRUD, test and sync, sync runs, queue depth surfaced on the overview.
   *Done:* a mirror syncs against a local fixture endpoint and the queued delivery count on `/offline` follows the flag.
4. **Polish** — keyboard shortcuts, mobile layout, error states, audit rows, walkthrough entries.
   *Done:* the QA walkthrough covers all three routes with zero high findings.

### Risks / notes

- On an isolated network, import is by file: **no code path may assume it can fetch anything at runtime**, so the guard belongs in the shared HTTP client wrapper, not in each caller.
- Bundle size: image archives and model weights are large — the upload path needs a generous body limit and a streaming checksum rather than a buffered one.
- The installation trust key is set at setup; rotating it must invalidate future imports only, never already-applied bundles.
- Clock skew on an isolated network breaks certificate validation and time-based codes; the doctor reports it, it does not repair it.
- Overlaps: REQ-037 (where bundle build material lives), REQ-024 (local registry deployment), REQ-048 (marketplace mirror metadata) — the bundle format is defined once and reused.
- Documentation must say plainly which features do not work without a local mirror, instead of letting an operator discover it after the network is cut.
