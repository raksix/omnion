# REQ-098 — Model Registry & Router

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Picking the right model for the job.

- Model catalog with metadata: context window, tool support, vision, streaming, embedding, cost per 1K tokens.
- Capability flags surfaced in the panel (searchable, filterable table).
- Task-based routing: cheap, translation, coding, vision, long-context, embedding, critical — each mapping to a preferred model with fallbacks.
- Per-organization/site default model and per-feature override (e.g. copilots vs content generation).
- Route explanation in AI logs: which model answered a request and why.

## Implementation spec

### Scope (in / out)

**In** — the catalog and the decisions built on it. v0's router (`crates/ai-hub/src/router.rs`) answers "which pair serves this request" deterministically; this request adds the metadata that makes the choice informed and the record that makes it explainable.

- **Model catalog** as a first-class screen: context window, modality and tool flags, streaming, embeddings, JSON mode, output ceiling, and cost per million tokens in and out (the unit the accounting already uses; the panel may render per 1K, the column stays per million).
- **Seven routing tasks** — `cheap`, `translation`, `coding`, `vision`, `long_context`, `embedding`, `critical` — each mapping to an ordered candidate list (primary plus fallbacks), at three scopes: installation default, per organization, per site.
- **Per-feature overrides** — a named feature (`content_assist`, `copilot`, `chat`, `translate`, `seo`, `alt_text`, `summarize`, `agent_default`) may pin its own model without disturbing the task map.
- **Resolution order**, stated once and tested: explicit `provider/model` in the request → feature override (site, then organization, then installation) → task route (same scopes) → installation default model → refuse. A candidate that is disabled, missing or fails a capability requirement is skipped with a reason.
- **Route decisions** — one row per resolved request carrying the task, feature, what the caller asked for, the model that answered, the index in the fallback list and a human-readable reason; the AI logs screen reads it, and a fallback is explained rather than guessed.
- **A dry-run endpoint** — resolve a hypothetical request (task, feature, scope, required capabilities) without calling any provider, so an operator can see what the map does today.

**Out**

- Cost accounting, budgets and the usage store (REQ-001); this request writes decision rows and reads the model prices the engine owns.
- Provider connection, protocol adapters, health and failover order (REQ-097); a route names a model, never a base URL.
- Embedding storage, chunking and vector search (REQ-001's knowledge base) — routing only picks the embedding model a collection may use.
- Automatic price discovery from vendor pages; prices are operator-entered and labelled an estimate.

### Screens (UI)

- **`/ai/models`** — the catalog table: Model (`provider/model`, monospace), Provider, Context window (formatted), Capabilities (pill row: tools, vision, streaming, embeddings, image, audio, transcription, JSON), In/Out cost per 1K, Used by (routes + overrides + agents), Default badge, Status. Search across key/display name/provider; capability filter chips, provider filter, status filter; sortable columns; bulk Enable/Disable; row actions Edit capabilities, Edit price, Set default, Copy `provider/model`. Empty state "No model registered yet" with Discover models (REQ-097) and Add model.
- **`/ai/models/[id]`** — header with the pair id and the serving provider's health; sections Metadata (context window, output ceiling, flags, price with an "estimate" note and its last-updated date), Routing (task routes and feature overrides pointing here, with links), Usage 30 days (requests, tokens, cost) linking to `/ai/costs`.
- **`/ai/routing`** — scope selector (Installation · Organization · Site picker), then one row per task: Task (name plus a one-line description), Primary (model select with a capability filter), Fallbacks (drag-ordered chips), Requirement chips (`tools`, `vision`, `long_context`, `json`), Last resolved (model + relative time). Row actions Reset to inherited, Copy from installation. Below it the **feature overrides** table: Feature, Scope, Model, Set by, Updated, with Add override. A **"Route a request"** panel (task, feature, required capabilities, token estimate) calls the dry-run endpoint and renders the decision, the reason and every skipped candidate with its reason.
- **`/ai/logs`** — the route decision log: Time, Task, Feature, Requested, Resolved (`provider/model`), Fallback index (badge when > 0), Reason, Run link, Status. Filters task, feature, resolved model, fallback used, date range, user; group-by model for a share view; CSV export; a row opens the decision detail with the full candidate walk (attempted, skipped, chosen — each with a reason).
- **States** — `LoadingTable` skeletons, `EmptyState` with a real action (no route configured → "Set a primary model"), an error banner with the API message and Retry, and a warning banner naming any task that cannot resolve at all.
- **Keyboard** — `⌘K` palette (with a "Route a request" entry), `⌘⇧A` AI Hub, `G` then `M` models, `G` then `T` routing, `/` focus search, `N` add override, `R` reset a row, `↑/↓` + `Enter` move/open, `Esc` close the drawer.
- **Mobile (<1024px)** — the catalog becomes label/value cards with wrapping capability pills; the routing table becomes an accordion of task cards (select plus stacked fallback chips, add/remove in a sheet); the decision log becomes cards with the reason clamped to two lines and expandable; fallback reorder offers up/down buttons instead of drag; no hover-only actions.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/models` | Catalog with metadata, prices and usage counts (`q`, `capability`, `provider`, `status`, `sort`) | `ai.providers.read` |
| PATCH | `/api/v1/ai/models/{id}` | Edit flags, context window, output ceiling, price, enabled | `ai.providers.manage` |
| PUT | `/api/v1/ai/models/{id}/default` | Make it the installation default (moves the badge transactionally) | `ai.providers.manage` |
| GET | `/api/v1/ai/routing` | Task map for a scope (`organization_id`, `site_id`) with inherited rows marked | `ai.providers.read` |
| PUT | `/api/v1/ai/routing` | Replace the task map for a scope (validated: candidates exist, requirements satisfiable) | `ai.settings.manage` |
| POST | `/api/v1/ai/routing/preview` | Dry-run: resolve task/feature/scope/requirements, return the candidate walk | `ai.providers.read` |
| GET | `/api/v1/ai/features` | Known feature keys with their descriptions (drives the override form) | `ai.providers.read` |
| GET/PUT | `/api/v1/ai/routing/overrides` | Read/replace per-feature overrides for a scope | `ai.providers.read` / `ai.settings.manage` |
| GET | `/api/v1/ai/logs/decisions` | Route decision log (`task`, `feature`, `model`, `fallback`, `from`, `to`) | `ai.usage.read` |
| GET | `/api/v1/ai/logs/decisions/{id}` | One decision with its full candidate walk | `ai.usage.read` |
| GET | `/api/v1/ai/routing/unresolved` | Tasks and features that cannot resolve, with the reason | `ai.providers.read` |

New catalogue keys (`crates/permissions::catalogue` + `seed.rs`): `ai.settings.manage` and `ai.usage.read` (shared with REQ-001 — land once, in whichever request ships first, and never twice). Every route sits behind `guards::require("…")`. A routing PUT is refused when any candidate is disabled, of the wrong kind (an embedding-only model in a chat task) or missing a required capability; the refusal names the task, the candidate and the failing requirement.

### Data model

Migration `database/migrations/0017_ai_router.sql` (take the next free number at implementation time).

- `alter table ai_models` adds `input_cost_micros_per_mtok bigint` and `output_cost_micros_per_mtok bigint` (both null or ≥ 0 — REQ-001's migration may already carry them; check first, add once), `price_source text not null default 'manual'`, `price_updated_at timestamptz`, `max_output_tokens int` (shared with REQ-097 — land once), `capabilities_verified_at timestamptz`, `capabilities_source text not null default 'manual'` (`manual`,`discovery`,`probe`).
- `ai_task_routes`: id uuid pk, organization_id uuid → organizations cascade null, site_id uuid → sites cascade null, task text (one of the seven keys), position int ≥ 1, model_id uuid → ai_models set null, requirements text[] default `'{}'` (`tools`,`vision`,`long_context`,`json`), updated_by uuid → users set null, created_at, updated_at; unique folded `(coalesce(organization_id…), coalesce(site_id…), task, position)` — folded the way `command_recents` folds its dedupe key — plus `(task)`.
- `ai_feature_overrides`: id uuid pk, organization_id uuid cascade null, site_id uuid cascade null, feature text, model_id uuid → ai_models cascade, updated_by uuid → users set null, created_at, updated_at; unique folded `(scope…, feature)`.
- `ai_route_decisions`: id bigserial pk, organization_id uuid → organizations set null, site_id uuid set null, user_id uuid → users set null, run_id uuid null, task text, feature text null, requested text null, resolved_provider_id uuid null, resolved_model_id uuid null, fallback_index int not null default 0, requirements text[] default `'{}'`, reason text not null, walk jsonb not null default `'[]'`, created_at timestamptz default now(); indexes `(organization_id, created_at desc)`, `(resolved_model_id, created_at desc)`, `(task, created_at desc)`, `(created_at)` for the pruner.
- `alter table ai_usage` adds `decision_id bigint → ai_route_decisions set null` plus `(decision_id)`, so a cost row and the decision behind it are one join; if `ai_usage` does not exist yet it lands with REQ-001's migration and this request only adds the column then.
- Decision rows are pruned after 90 days by a small `ai_log_runner` in `apps/api` (config `OMNION_AI_LOG_RUNNER`); the counters the panel shows for older windows come from `ai_usage`, which this request never prunes.
- The resolve function keeps v0's determinism: ordered candidates, disabled/missing skipped, ties broken by provider name, and a decision row is written **before** the provider call so a failure still explains itself.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.model.registered` | emitted | provider/model, flags, metadata source |
| `ai.model.updated` | emitted | changed fields (flags, context window, price) |
| `ai.model.disabled` | emitted | provider/model, routes referencing it |
| `ai.route.updated` | emitted | scope, task map diff (added/removed/reordered) |
| `ai.route.fallback_used` | emitted | task, feature, requested, resolved, index — the alert hook for a degrading primary |
| `ai.route.unresolved` | emitted | task/feature, scope, reason — nothing answered, an operator must act |

All events ride the existing signed webhook bus; org/site-scoped events deliver to that scope's endpoints, and an installation-level route change carries `organization_id = null` and only reaches the audit trail. Route decisions are also the input the AI logs screen (docs/06 §17) renders per run.

### Acceptance criteria

- [ ] The catalog lists every registered model with context window, flags, price and usage counts, and the capability chips filter the table (multi-select, asserted in a UI test).
- [ ] `/ai/models` search matches on model key, display name and provider name; the empty state offers both Discover and Add.
- [ ] A model with `supports_embeddings = false` cannot be chosen for an `embedding` task or as a collection's embedding model (API refuses, UI filters the select).
- [ ] A task map with a primary and two fallbacks resolves to the primary; disabling the primary inside a test resolves to the first fallback and writes `fallback_index = 1` with a reason naming the skip.
- [ ] A candidate that fails a required capability is skipped for that reason, and the decision walk records it.
- [ ] Resolution order is exact: explicit pin beats feature override beats task route beats installation default; a test asserts each adjacent pair.
- [ ] Feature overrides resolve site over organization over installation; removing an override restores the inherited model without touching the task map.
- [ ] A site-level task map does not change another site's decisions (test asserts two sites diverge).
- [ ] `POST /ai/routing/preview` returns the walk (attempted, skipped with reason, chosen) and performs zero provider calls (test counts upstream requests = 0).
- [ ] A routing PUT naming a disabled or capability-incompatible model is refused with the task, candidate and requirement in the message.
- [ ] Price edits surface on `/ai/costs` for new requests only — historical rows keep their recorded cost (test asserts an old `ai_usage` row is unchanged).
- [ ] Every task row shows "Last resolved" from the newest decision; a task that cannot resolve renders the warning banner and appears in `/ai/routing/unresolved`.
- [ ] `/ai/logs` filters by task, feature, model, fallback-used and date, and the CSV export matches the filtered rows row-for-row.
- [ ] A decision detail shows the full candidate walk including skipped candidates with their reasons, and links to the run when the request came from an agent.
- [ ] Removing a model a route references leaves the route row with a null candidate and an `ai.route.unresolved` event, and the panel marks the row as needing attention.
- [ ] Decision rows older than the retention window are pruned by the runner while usage counters for the same window stay complete (test asserts counts).
- [ ] Organization A cannot read or write organization B's route maps or decision log (404).
- [ ] Every screen has empty, loading and error states with a real call to action; no dead control and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: open `/ai/models` on a fresh install (empty state → Discover models), apply a discovery diff, edit one model's price and flags, confirm the capability filter narrows the table; open `/ai/routing`, set a primary and two fallbacks for `cheap`, add a `tools` requirement to `critical`, save, then ask "Route a request" for `critical` with tools and read the walk; disable the primary and see the fallback answer plus the badge in `/ai/logs`; add an organization feature override for `copilot`, switch the scope selector and verify the installation row is untouched; reset a row; export the decision log CSV and compare two rows against the detail view. The mobile pass (390×844) runs the accordion routing editor and the card view of the decision log; the fresh-database empty states run over every screen.

The visual check must see: numeric columns right-aligned (context window, costs), capability pills not overflowing their cell, fallback chips readable at 390 px, a route warning banner not mistakable for an error, no raw i18n keys, and no text overlapping the drag handles.

### Slices

1. **Catalog** — metadata columns, price columns, the `/ai/models` table with filters and bulk actions, the model detail screen, and flags reaching the router's checks.
   *Done when:* a price edit changes the cost of the next request only, and a capability-incompatible model is refused everywhere it could be picked.
2. **Task routing and overrides** — `ai_task_routes`, `ai_feature_overrides`, the `/ai/routing` screen with scope selector and drag order, the resolution order, the dry-run preview, validation on PUT.
   *Done when:* a two-fallback route degrades correctly under test, the preview performs no provider call, and scopes do not leak into each other.
3. **Decision log and explanation** — `ai_route_decisions`, the decision writer in the resolve path, `/ai/logs` with its detail view, the retention runner, the `ai_usage.decision_id` link, the events.
   *Done when:* every resolved request has a decision row with a reason, a fallback is visible end to end, and a cost row joins back to its decision.

### Risks / notes

- **Prices drift and are entered by hand.** Label cost as an estimate, date the price row, and never retro-edit historical usage — an accounting number that changes after the fact is worse than an approximate one.
- The routing table is the most tempting place to hide complexity: keep the requirement chips to four values, resolve deterministically, and make every skip explain itself — a route the operator cannot read is a route they will not trust.
- Capability metadata is operator-entered or discovered and either can be wrong: a refused request must say which flag refused it, and a model that keeps failing with an incompatible-capability error should be flagged on the model screen.
- Feature keys are an interface: adding one means the registry constant, the override form and the doc change together — a feature pin that silently does nothing is worse than no override.
- A task with no resolvable candidate must fail loudly (`ai.route.unresolved`) and the panel must show the warning, but other tasks keep serving; never fall back to a model nobody chose for that task.
- Keep the explanation cheap: one decision row per request on the hot path, carrying identifiers and a short reason — not the prompt — with the walk bounded to candidates actually considered.
