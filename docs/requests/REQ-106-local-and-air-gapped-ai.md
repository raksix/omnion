# REQ-106 — Local & Air-gapped AI Mode

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Running with no external calls at all.

- Local inference endpoints as first-class providers (Ollama, vLLM, llama.cpp, any OpenAI-compatible local server).
- Model management: list local models, pull/remove, show resource usage.
- Air-gapped mode switch that refuses any non-local provider call (with a clear error).
- Embedding and reranking models served locally; offline knowledge base operation.
- Install/run documentation and a doctor check that verifies local availability.

## Implementation spec

### Scope (in / out)

**In** — the AI half of the air gap: local endpoints as ordinary providers, model management that
delegates to the local server, and a switch that makes "never leaves the instance" a checked fact
rather than a promise. The platform-wide offline switch belongs to REQ-036; this request owns
everything the AI Hub does when that switch is on, and can be enabled on its own.

- **Local endpoints as providers** — a local endpoint is a row in `ai_providers` with
  `protocol = 'openai_compatible'`, `locality = 'local'` and a base URL whose host must be loopback,
  an RFC 1918/ULA address, a `host.docker.internal`-style name, or a host on the installation's
  internal-host allow-list. Anything else is refused at save time with a field error naming the
  host — a "local" provider pointing at the public internet is a misconfiguration and the API says
  so.
- **Locality, derived and verified** — locality is not taken on trust for the air-gap check: a
  request passes when the resolved endpoint's host is loopback/private/allow-listed, and an operator
  can re-verify with the doctor, which performs a live request to the endpoint and records the
  result. Redirects are refused by the local client (a local URL that 302s to the internet is a
  refusal, not a silent hop).
- **Embeddings and rerank locally** — an embedding model and a rerank model can be pinned per
  collection (REQ-102) to local endpoints, so indexing and retrieval continue with no egress. A
  knowledge collection whose embedding model is remote is paused when the air gap is on and flagged
  as "needs a local embedding model" with a one-click way to repoint it at an available one.
- **Air-gap mode** — an installation setting: when on, every request whose resolved provider is not
  local is refused before any network call with `403 code = "ai_airgap_blocked"`, naming the
  provider, the host and the setting that refused it. The refusal is logged (REQ-104) with status
  `blocked_airgap` and announced once on the AI screens as a banner; the switch itself is audited,
  requires a reason and an explicit confirmation, and can be flipped only by an operator with the
  air-gap permission.
- **Egress verification** — with the air gap on, the doctor offers a check that attempts a documented
  non-local call and expects the refusal; a refusal is a pass, a success is a loud failure that also
  turns the banner red. The check records when it last ran and against which host name.
- **Doctor** — one screen, one endpoint, N checks: endpoint reachable (with latency), model present,
  model answers a one-token completion, embedding model present and dimension matches the
  collections that use it, rerank model present, air-gap state, last egress-verification result,
  disk and memory available on the host if the platform can read them. Each check is
  pass/warn/fail with a plain-language cause and a suggested fix.

**Out**

- Managing GPU drivers, container runtimes or model caches on the host beyond calling the local
  server's own API.
- Training or fine-tuning locally.
- Downloading models onto the platform's own disk: the platform asks the local server to pull and
  reports what it says.
- Making remote features work offline by proxy (for example a cached remote answer); an offline
  installation gets local answers or an honest refusal.
- The non-AI parts of offline operation (updates, package installs, external webhooks) — REQ-036.

### Screens (UI)

- **`/ai/local`** — overview: endpoint cards (name, base URL host, locality badge, reachable, models
  available, last checked), the air-gap status banner with the reason and who enabled it, and stat
  tiles (local models, resident models, local requests 30d, remote calls 30d — the last one expected
  to be zero while the gap is on).
- **`/ai/local/models`** — table: Model key, Endpoint, Size, Parameters/Quantization, Context,
  Capabilities (tools / vision / embeddings / rerank), Residency, Status (available / pulling with a
  percentage / missing / error), Last used. Filters endpoint, capability, status, free text. Row
  actions Pull, Remove (confirm by key), Set as default, Copy key. Pull drawer shows progress lines
  from the server with a Cancel control; a pull that fails shows the server's error verbatim plus
  the fix hint.
- **`/ai/local/doctor`** — a run list: each check with status icon, one-line cause, expandable detail
  (raw response summary, latency, host, timestamp), a Run all button, and per-check Rerun. A summary
  line states the verdict: "Ready for air-gapped operation" or the first blocking check. Previous
  runs are listed with their verdict so a regression is visible.
- **`/ai/settings/airgap`** — the switch with a description of exactly what stops, a reason field
  (10–500 characters, required), a type-to-confirm control, the current state with actor and time,
  the internal-host allow-list editor, and the last egress-verification result. Flipping it on opens
  a confirmation that lists the non-local providers in use and asks for an acknowledgement.
- **Keyboard** — `/` focuses the model search, `P` pulls the focused model, `R` reruns the focused
  doctor check, `D` runs all checks, `G` then `L` goes to local, `↑/↓` + `Enter` move and open, `Esc`
  closes drawers.
- **Mobile (<1024px)** — endpoint cards stack, the model table becomes cards with the pull progress
  inline, the doctor collapses to a status list with expandable detail, and the air-gap confirmation
  becomes a full-screen sheet with the acknowledgement control above the fold.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/local/endpoints` | Local endpoints with locality, reachability and model counts | `ai.local.read` |
| POST | `/api/v1/ai/local/endpoints` | Register a local endpoint (host must be local or allow-listed) | `ai.local.manage` |
| GET | `/api/v1/ai/local/models` | Models the local endpoints serve, with the last known status | `ai.local.read` |
| POST | `/api/v1/ai/local/models/pull` | Ask an endpoint to pull a model (progress polled) | `ai.local.manage` |
| DELETE | `/api/v1/ai/local/models` | Remove a model from its endpoint | `ai.local.manage` |
| GET/POST | `/api/v1/ai/local/doctor` | Read the last run / start a new doctor run | `ai.local.read` / `ai.local.manage` |
| GET/PUT | `/api/v1/ai/airgap` | Read / change the air-gap switch (reason required) | `ai.local.read` / `ai.airgap.manage` |
| POST | `/api/v1/ai/airgap/verify` | Attempt a non-local call and expect a refusal | `ai.airgap.manage` |

New catalogue keys: `ai.local.read`, `ai.local.manage`, `ai.airgap.manage`. `ai.airgap.manage` is
granted to the Owner role only by default and is listed on the security screen as a high-impact
permission. A refused call answers `403 code = "ai_airgap_blocked"` with the provider name and host
in the message, never a generic "request failed".

### Data model

Migration `database/migrations/00NN_ai_local.sql` (00NN = next free integer at land time; 0020 was
free when this was written). It extends `ai_providers` (REQ-001) with `locality` text default
`'remote'` (`'local'`), `host_kind` text null (`'loopback'`, `'private'`, `'allowlisted'`) and
`last_seen_at` timestamptz null.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_local_models` | id uuid pk, provider_id uuid → ai_providers cascade, model_key text, display_name text, size_bytes bigint null, parameter_count bigint null, quantization text null, context_window int null, supports_tools bool default false, supports_vision bool default false, supports_embeddings bool default false, supports_rerank bool default false, embedding_dimension int null, status text ('available','pulling','missing','error'), pull_progress int default 0, pull_message text null, resident bool default false, last_used_at timestamptz null, updated_at | unique `(provider_id, model_key)`; `(provider_id, status)`; `(status)` where status = 'pulling' |
| `ai_airgap_state` | id smallint pk check (id = 1), enabled bool default false, reason text null, enabled_by uuid null → users set null, enabled_at timestamptz null, low_confidence_ack bool default false, egress_verified_at timestamptz null, egress_verify_target text null, egress_verify_result text null, updated_at | pk only (single row, enforced by the check) |
| `ai_airgap_hosts` | id uuid pk, host text, note text, created_by uuid null → users set null, created_at | unique `(host)` |
| `ai_local_doctor_runs` | id bigserial pk, organization_id uuid null, status text ('passed','warned','failed'), checks jsonb default '[]' (`[{key, status, detail, latency_ms, fix}]`), airgap_enabled bool, triggered_by uuid null → users set null, started_at, finished_at | `(started_at desc)`; `(status)` where status <> 'passed' |

`ai_airgap_state` is seeded by the migration with `enabled = false`. Host validation is one shared
function (`is_local_host`) used by the endpoint save path, the air-gap check and the doctor, so the
three cannot drift apart; the allow-list widens it, never replaces it.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.local.model.pulled` / `.removed` / `.pull_failed` | emitted | endpoint, model key, size, error |
| `ai.airgap.enabled` / `.disabled` | emitted | reason, actor — the audit entry for a compliance-relevant switch |
| `ai.airgap.call_refused` | emitted | feature, provider, host — the count of these is the switch's proof |
| `ai.airgap.verify.passed` / `.failed` | emitted | target, latency, result — a failed verification is the loudest alert in this request |

### Acceptance criteria

- [ ] With the air gap on, a chat addressed to a remote provider is refused with `403 ai_airgap_blocked` naming the provider and host, and the attempt is logged with `status = 'blocked_airgap'`.
- [ ] With the air gap on, the same chat addressed to a local endpoint answers normally (round-trip through a local stub server in the test).
- [ ] A local endpoint that redirects to a non-local host is refused, and the refusal names the redirect target.
- [ ] Enabling the air gap requires a reason; an empty or too-short reason is a field error, and the audit entry carries the actor, the reason and the time.
- [ ] `/api/v1/ai/local/models` lists what the endpoint serves and a pull moves a model from `missing` to `available` through `pulling` with progress visible in the UI; the same key cannot be pulled twice concurrently.
- [ ] A knowledge collection pinned to a local embedding model indexes and searches with the remote provider unreachable (test runs with the remote endpoint pointed at a closed port).
- [ ] With the air gap on, a collection whose embedding model is remote is listed as blocked with a "needs a local embedding model" chip and a one-click repoint that works when a local embedding model exists.
- [ ] `/api/v1/ai/airgap/verify` reports a pass when the refusal happens and a failure when a call escapes; a failure turns the `/ai/local` banner red and emits `ai.airgap.verify.failed`.
- [ ] The doctor reports each of reachability, model presence, a one-token completion, embedding presence and dimension, and air-gap state with a pass/warn/fail and a fix hint; a rerun after fixing a check changes the verdict.
- [ ] The "Run AI locally" documentation page exists, names the supported servers, the verification steps, and what stops working while the gap is on.
- [ ] Every screen has empty, loading and error states with a real action; `/ai/local` renders with no endpoint registered at all.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/ai/local` with no endpoints and follow the empty state's action;
register a local endpoint with a public host (field error), then with a loopback or allow-listed
host (saved); open `/ai/local/models`, refresh the list, pull a model and watch progress, cancel a
second pull, remove a model (once refused because a collection uses it, once successful after
repointing that collection); open `/ai/local/doctor` and run all checks, fix one deliberately broken
check (wrong base URL) and rerun to see the verdict change; open `/ai/settings/airgap`, try to enable
without a reason (field error), enable with a reason, read the banner, then in a chat send a message
to the remote default (refused, message names the provider) and to the local model (answered); run
`/api/v1/ai/airgap/verify` from the screen and read the pass; add an internal host to the allow-list
and re-register an endpoint on it; then disable the gap and confirm the banner clears.

The visual check must see: locality and reachability badges that are distinguishable without colour
alone, the air-gap banner visible on every AI screen while it is on, pull progress that does not
overlap the model key column, doctor rows with an obvious fix hint per failing check, the
confirmation sheet's acknowledgement control above the fold on mobile, no raw i18n keys, and a mobile
pass (390×844) over local overview, models, doctor and the air-gap switch.

### Slices

1. **Local endpoints and model management** — `ai_providers` locality/host columns, `is_local_host`,
   the redirect-refusing local client, `ai_local_models`, list/pull/remove, `/ai/local` and
   `/ai/local/models`, permission keys.
   *Done when:* a local endpoint serves a chat and a pull moves through `pulling` to `available`
   while a bad key surfaces the server's error.
2. **Air-gap enforcement** — `ai_airgap_state`, `ai_airgap_hosts`, the pre-call check, the honest
   refusal, audit and events, the switch screen with reason and confirmation, the banner.
   *Done when:* with the gap on, a remote provider is refused and logged, a local one answers, and
   the switch requires a reason and an actor.
3. **Local embeddings, offline knowledge and evaluation interaction** — embedding and rerank model
   pinning, collection pause/repoint, judge-model refusal, offline indexing test.
   *Done when:* a collection pinned to local embeddings indexes and answers with the remote endpoint
   down.
4. **Doctor, verification and documentation** — the doctor checks and runs, egress verification,
   `/ai/local/doctor`, `/ai/settings/airgap` verification panel, the "Run AI locally" page, mobile
   and empty states.
   *Done when:* a broken check is diagnosed with a fix, the egress verification distinguishes pass
   from failure, and the docs page matches the shipped behaviour.

### Risks / notes

- "Local" is a claim that must be checked, not a label: the host test plus the redirect refusal plus
  the egress verification are the three controls that make the switch worth trusting.
- Local quality is lower than frontier quality: the screens should set the expectation (model size,
  context window, tool support) before an operator points production copilots at a small model.
- The air gap is a switch that can strand features: the confirmation must list what stops, and the
  refusal message must always name the route that changes it.
