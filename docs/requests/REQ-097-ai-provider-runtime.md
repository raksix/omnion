# REQ-097 — AI Provider Runtime & Local Models

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Bring-your-own models, cloud or local.

- Provider registry with typed capability flags: chat, stream, embeddings, image generation, audio, transcription, list-models.
- Cloud providers (OpenAI-compatible, Anthropic-style, Google, and any OpenAI-compatible endpoint) plus local inference (Ollama, vLLM, llama.cpp server).
- Per-provider API key (write-only), base URL, custom model list, enable/disable, connection test.
- Provider protocol adapters (chat completions, responses, messages) with streaming normalisation.
- Health/usage telemetry per provider and automatic failover when a provider errors.

## Implementation spec

### Scope (in / out)

**In** — the runtime under `crates/ai-hub` that v0 left open (migration `0008`, phase P11): v0 connects one protocol and streams a chat, this request turns that into a provider runtime.

- **Protocol adapters.** The client speaks through a small trait (`build_request`, `parse_response`, `normalise_stream`) with three implementations: `openai_compatible` (the existing one, which also covers local servers that mimic it), `anthropic_messages` (messages wire shape) and `google_gemini` (generate content). A fourth attaches through the trait without touching the router.
- **One normalised stream.** Whatever a vendor sends — SSE, chunked JSON, keep-alive comments — subscribers see `text`, `tool_call`, `usage`, `error` and `done` events with no vendor shape leaking upward; a stream that ends without usage records `null` tokens rather than inventing counts.
- **Typed capability flags** on every model: chat, streaming, tools, vision, embeddings, image generation, audio generation, transcription, JSON mode, list-models. The flags are data the registry edits and the router enforces — no caller guesses what a model can do.
- **Provider lifecycle** — name, protocol, kind (cloud/local), base URL, write-only key, per-provider custom model list, enable/disable, discovery of what the endpoint serves, and a server-side **connection test** that reports each step (resolve host, TLS, auth, list-models, streaming ping) with its latency.
- **Health and usage telemetry** — a probe that samples every enabled provider (interval configurable, default 60 s), a rolling status per provider (`ok`, `degraded`, `down`, `unknown`), and per-provider request/token/error counters for a window.
- **Failover** — an ordered chain of enabled providers; a call that fails *before the first streamed byte* is retried against the next provider when the request did not pin a model, and the substitution is recorded. Pinned `provider/model` requests never silently leave their provider.

**Out**

- Fine-tuning, model training, hosted vector databases, and anywhere the platform would run model-authored code (docs/09-N8N-TEARDOWN.md §13).
- Image and audio generation pipelines: the flags are declared and enforced, the execution paths refuse with a stable `not_supported` code until a later request ships them.
- Cost accounting and budgets (REQ-001), model routing policy (REQ-098), agents and tools (REQ-099/100), per-user provider keys — a provider is an installation-level connection.

### Screens (UI)

Screen files follow the existing pattern: `apps/admin/app/<route>/page.tsx` plus `apps/admin/features/ai/<area>-view.tsx`, rendered inside `AppShell`. v0's single `/ai` screen keeps its chat and hands provider management to the screens below.

- **`/ai/providers`** — table: Name, Protocol, Kind (Cloud/Local badge), Host (from Base URL), Models (enabled/total), Health (status dot + last check + latency), Priority, Default badge, Updated. Search by name/host; filters kind, protocol, health; bulk Enable/Disable; row actions Test, Discover, Models, Set default, Disable, Remove (confirm by typing the name). Empty state "No provider connected yet" with a Connect provider button; `LoadingTable` skeleton; error banner carrying the API message and Retry.
- **`/ai/providers/new`, `/ai/providers/[id]`** — tabs **Connection · Models · Health · Usage**. Form: Name (1–64, unique case-insensitively), Protocol (select of the three adapters), Kind, Base URL (http/https, no whitespace, trailing slash normalised away), API key (write-only; a stored key renders as "Stored" with Replace and Clear, blank keeps it), Timeout ms (1000–120000, default 30000), Max retries (0–5, default 1), Priority (1–1000, default 100), Enabled. Field-level validation; the API message lands under its field.
- **Connection test** — a modal listing the five steps, each pending/ok/failed with latency, the total time, and the provider's own error text on failure; the stored key is never rendered and a failure message is shown verbatim, truncated to 500 characters.
- **Models tab** — the `/ai/models` table filtered to this provider (REQ-098 owns the full catalog), with Add model, Edit capabilities, Enable/Disable and Discover. Discover renders a **diff** (new, changed, removed) with counts and applies only on Confirm.
- **Health tab** — status header, uptime percentage for 24 h/7 d, latency sparkline, and the last 50 probe samples (Time, Status, Latency, HTTP status, Error), with "Probe now" taking one sample immediately.
- **Usage tab** — requests, prompt/completion tokens, error rate and p95 latency for a range picker (24 h/7 d/30 d), grouped by day, with a CSV export.
- **Failover order** — a drag-ordered list of enabled providers with visible rank numbers and a preview of the chain after disabled providers drop out; the order is one PUT on drop with an undo toast.
- **Keyboard** — `⌘K` palette, `⌘⇧A` AI Hub, `G` then `P` providers, `/` focus the search, `N` new provider, `T` test the focused row, `Esc` close drawers, `↑/↓` + `Enter` move/open a row.
- **Mobile (<1024px)** — tables become label/value cards, the form stacks to one column, the health sparkline becomes a list, the test modal is a full-height sheet, drag-order becomes up/down buttons; no action hides behind hover.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/providers` | List providers with health, model counts, priority (`q`, `kind`, `protocol`, `health`) | `ai.providers.read` |
| POST | `/api/v1/ai/providers` | Connect a provider (name, protocol, kind, base_url, api_key?, timeout_ms, max_retries, priority) | `ai.providers.manage` |
| PATCH | `/api/v1/ai/providers/{id}` | Change fields; key follows Keep/Set/Clear; enable/disable; priority | `ai.providers.manage` |
| DELETE | `/api/v1/ai/providers/{id}` | Remove a provider, its models and its health samples | `ai.providers.manage` |
| POST | `/api/v1/ai/providers/{id}/test` | Run the connection test server-side and return the step results | `ai.providers.manage` |
| POST | `/api/v1/ai/providers/{id}/discover-models` | Ask the endpoint which models it serves; returns a diff, never writes | `ai.providers.manage` |
| PUT | `/api/v1/ai/providers/{id}/models` | Replace the provider's model set (exists) | `ai.providers.manage` |
| PATCH | `/api/v1/ai/models/{id}` | Edit one model's capability flags, limits, enabled (exists in part) | `ai.providers.manage` |
| PUT | `/api/v1/ai/providers/order` | Replace the failover order (`ids` in rank order) | `ai.providers.manage` |
| POST | `/api/v1/ai/providers/{id}/probe` | Take one health sample now | `ai.providers.manage` |
| GET | `/api/v1/ai/providers/{id}/health` | Health status and samples for a window (`hours`) | `ai.providers.read` |
| GET | `/api/v1/ai/providers/{id}/usage` | Requests, tokens, errors, latency for a window | `ai.providers.read` |
| GET | `/api/v1/ai/protocols` | The supported protocols with their capability notes (drives the form select) | `ai.providers.read` |

Every route sits behind `guards::require("…")` in `apps/api/src/routes/ai.rs`; a protocol outside `SUPPORTED_PROTOCOLS` is refused with a stable code and the list of supported values, and an unknown provider id answers `404` (never `403`, never a leak of another installation's rows).

### Data model

Migration `database/migrations/0016_ai_provider_runtime.sql` (take the next free number at implementation time; `0011` is claimed by REQ-001's engine migration — released migrations are append-only).

- `alter table ai_providers` adds `kind text not null default 'cloud'` (`cloud`/`local`), `timeout_ms int not null default 30000` (1000–120000), `max_retries int not null default 1` (0–5), `priority int not null default 100` (1–1000), `last_health text not null default 'unknown'` (`ok`/`degraded`/`down`/`unknown`), `last_checked_at timestamptz`, `last_error text`; the existing `ai_providers_protocol_check` is dropped and re-added wider (`openai_compatible`, `anthropic_messages`, `google_gemini`), and a new `ai_providers_priority_idx (priority, lower(name))` serves the failover walk.
- `alter table ai_models` adds `supports_image_generation`, `supports_audio_generation`, `supports_transcription`, `supports_json_mode` (bool, default false, matching the existing flag style) and `max_output_tokens int` (null or > 0).
- New `ai_provider_health`: id bigserial pk, provider_id uuid → ai_providers cascade, status text (`ok`,`degraded`,`down`), latency_ms int (≥ 0), http_status int null, error text null, checked_at timestamptz default now(); indexes `(provider_id, checked_at desc)` and `(checked_at)` for the pruner.
- The probe runner (`ai_health_runner` in `apps/api`, spawned like `workflow_runner`, config `OMNION_AI_HEALTH_RUNNER`, default on) samples each enabled provider every 60 s, writes one row and updates `last_health`/`last_checked_at` in the same transaction; samples older than 30 days are pruned by the same tick.
- Health transitions are computed, not stored: `down` after three consecutive failures, `degraded` when latency is over 1.5× the provider's 7-day median or any failure is seen, `ok` after two successes.
- Failover uses enabled providers ordered by `priority`, then `lower(name)`; generation attempts are never retried — a request that already received stream bytes is not replayed, and a retry is attempted only in the request-building and first-byte phases.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.provider.connected` | emitted | name, protocol, kind — audit trail and AI logs |
| `ai.provider.updated` | emitted | changed fields, never the key |
| `ai.provider.removed` | emitted | name, model count |
| `ai.provider.health_changed` | emitted | provider, from, to, latency_ms, error |
| `ai.provider.test_failed` | emitted | provider, failing step, provider error text |
| `ai.provider.discovery_applied` | emitted | added/changed/removed counts |
| `ai.provider.failover_used` | emitted | requested provider, substitute provider, task |

Providers are installation-level, so these events carry `organization_id = null` and fan out to no webhook endpoint (migration `0009` semantics); they exist for the audit trail and the AI logs screen. Every operation is tool-call shaped for REQ-100: `provider.connect`, `provider.test`, `provider.update` are the registry names for the same actions.

### Acceptance criteria

- [ ] Connecting a provider with each of the three protocols stores it and lists it with the right kind and health `unknown`.
- [ ] A protocol outside `SUPPORTED_PROTOCOLS` is refused with a stable code naming the supported values (API) and a disabled option (UI).
- [ ] The API key is never returned by any endpoint; a stored key shows as "Stored", Replace changes it, Clear removes it, blank keeps it.
- [ ] The connection test reports per-step outcomes and a total latency against a live local endpoint, and names the failing step against a dead one.
- [ ] Discovery against a live endpoint returns a diff; applying it adds, updates and removes exactly the models in the diff, and re-running discovery without changes produces an empty diff.
- [ ] A streamed answer from each adapter produces the same normalised event sequence (`text`… `usage`, `done`), and a mid-stream vendor error surfaces as one `error` event plus a failed usage row.
- [ ] A model with `supports_streaming = false` refuses a streaming request with a clear message; a model with `supports_vision = false` refuses an image-bearing request before any call leaves the process.
- [ ] The flags the panel shows equal the flags the router reads (asserted in a test against the same row).
- [ ] The health probe writes a sample per enabled provider per tick, and a provider that starts failing goes `degraded` then `down` with an `ai.provider.health_changed` event each time the status changes.
- [ ] "Probe now" writes exactly one sample and refreshes the header without a page reload.
- [ ] The failover order PUT persists the rank order; the chain preview hides disabled providers and shows rank collisions resolved by name.
- [ ] A failing provider is replaced by the next in order for a request that named only a task, the substitution is recorded as `ai.provider.failover_used`, and the caller sees the final provider in the response metadata.
- [ ] A request pinned as `provider/model` fails with the provider's error and is **not** rerouted, and a request that already received stream bytes is never retried (tests assert exactly one upstream attempt each).
- [ ] Provider usage (requests, tokens, errors) equals the rows the runtime recorded for the window, asserted against SQL in the test.
- [ ] Local endpoints work without a key: an Ollama-shaped, a vLLM-shaped and a llama.cpp-shaped local base URL each pass Test, Discover and a streamed chat.
- [ ] Removing a provider removes its models and health samples, and is refused while it is the installation's default (the message tells the operator to set another default first).
- [ ] Every screen has empty, loading and error states with a real call to action; no dead button and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: open `/ai/providers` on a fresh database (empty state → Connect provider); connect a local endpoint first with a malformed base URL (field error), then correctly (test modal green on all five steps); connect a second, cloud-shaped provider with a wrong key and read the failing step; discover models on both, apply one diff, then re-run discovery and see an empty diff; set a default; edit a model's flags and open a chat with it; disable a provider and watch the failover order preview update; kill the local endpoint and watch health go `degraded` then `down` plus the event in the AI logs; probe manually; walk the Usage tab, switch the range and export the CSV. The mobile pass (390×844) and the fresh-database empty states run over every screen.

The visual check must see: one primary button per screen, a readable status-dot pairing on the health column, no clipped base-URL or date cells, the key field never rendering a value, the test modal filling the mobile viewport, no raw i18n keys, and the failover list dragging with no text overlap.

### Slices

1. **Adapters and the connection test** — protocol trait, the two new adapters, `SUPPORTED_PROTOCOLS` widening, migration `0016` provider columns, `/ai/protocols`, the test endpoint and modal, the wider form.
   *Done when:* all three protocols connect, test and stream against real endpoints, and a failing step is named with the provider's own error.
2. **Capability flags and discovery** — modality flags on `ai_models`, the Models tab, the discovery diff with apply, router enforcement of each flag, `max_output_tokens`.
   *Done when:* a discovery diff applies once and repeats empty, and a flag set to false is refused by both the API and the panel.
3. **Health, failover and telemetry** — health table, probe runner with pruning, status computation, Health and Usage tabs, failover order UI and substitution logic, the provider events.
   *Done when:* a provider taken down mid-day shows `down` with samples and an event, a task-routed request fails over once, and a pinned one does not.

### Risks / notes

- **Base URLs are operator-supplied and dialled by the server.** Allow only http/https, refuse credentials embedded in the URL, and keep the platform's own metadata endpoints out of reach (a deny list checked at connect time and again before each call, so a DNS rebind cannot walk around it).
- Keys are secret material: written through the API, never returned, never logged, never in an event payload; a provider's error text is shown only after stripping anything key-shaped.
- The key is stored as a column today. Say so plainly — a request that needs stronger protection moves it to the secrets store (REQ-037) rather than implying it is already there.
- Streaming normalisation is where vendor quirks concentrate: finish-reason-only streams, keep-alive comments, tool calls split across chunks and a stream ending without usage are each a fixture test, not a hope.
- Failover must stay loud: every substitution emits an event and lands in the route decision (REQ-098), and a `down` provider is skipped only after the operator sees it — automatic skipping never applies to an explicitly pinned call.
- Retries are for idempotent reads (list-models, probe) with a capped backoff; a chat request retries at most once and only before the first byte.
