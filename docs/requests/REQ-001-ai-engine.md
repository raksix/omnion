# REQ-001 — AI Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/ai`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Give the platform a dedicated **Omnion AI Engine**:

- AI provider abstraction
- OpenAI / Anthropic / Gemini / local LLM support
- Per-site AI configuration
- Content generation
- Content summarization
- Automatic SEO
- Alt-text generation
- Translation
- Document analysis
- AI chatbot
- RAG / knowledge base
- Embedding / vector search
- AI workflow actions (see REQ-003)
- AI agent/plugin system
- Natural-language operations inside the admin

## Example

> "Analyze the posts published in the last 30 days and list the ones with weak SEO."

## Notes

- Lives under `modules/` — the Core stays thin.
- Full design: [`docs/06-AI-HUB.md`](../06-AI-HUB.md) — providers, model registry, model
  router, agents, tools, permissions, approvals, RAG, cost manager, data guard.
- Reuses the provider-abstraction pattern planned for translation (docs/01-VISION.md, §5).
- Natural-language admin operations build on the command palette (REQ-002) and the
  automation engine (REQ-003).

## Implementation spec

### Scope (in / out)

**In** — `crates/ai-hub` already connects providers, holds a model registry and streams chat
(migration `0008`, phase P11). This request builds the engine on that base:

- **Task router** — the platform asks for a *job* (`chat`, `summarize`, `seo`, `alt_text`, `translate`, `embed`, `vision`, `long_context`) and the router resolves `provider/model` from the registry's capability flags plus a task map an operator edits on `/ai/settings`.
- **Content tasks**, callable from the editor and the API: summarize a revision, generate an SEO title and description, suggest a slug, generate alt text for a media item, translate a revision into a draft, analyse a document into a summary with key points.
- **Knowledge base (RAG)** — collections, documents (page revisions, media, pasted notes), chunking, embeddings, vector search, and answers that cite the chunks they used.
- **Agent runtime** — an agent is a system prompt + model + tool allow-list + approval list; every turn is a durable `ai_runs` / `ai_run_steps` pair, every tool call passes the same permission guard a human request would, and dangerous tools wait in an approval inbox.
- **Cost manager** — token and cost accounting per provider, model, organization, site and user; monthly budgets with warn/block thresholds; usage screens.
- **Data guard** — per-provider data policy (user text, media, analytics, private content) and PII masking before a request leaves the process.
- **Natural-language admin operations** — an "Ask Omnion" mode inside the command palette (REQ-002) that compiles a sentence into tool calls and shows the plan before running it.
- **Packaging** — the feature ships as the `modules/ai` workspace member (`omnion-ai`); `crates/ai-hub` keeps provider/registry/router primitives, the module owns agents, knowledge, cost and the API surface the screens call.

**Out**

- Fine-tuning, model training, hosted vector databases, and in-process dynamic code execution (docs/09-N8N-TEARDOWN.md §13, lesson 14).
- Image/audio generation and speech transcription — the trait keeps the methods, no screen ships them.
- Payments: cost is accounting from token counts and model prices, never an invoice.

### Screens (UI)

Screen files follow the existing pattern: `apps/admin/app/<route>/page.tsx` plus
`apps/admin/features/ai/<area>-view.tsx`, rendered inside `AppShell`.

- **`/ai`** — four stat cards (providers, models, tokens this month, cost this month), "Recent runs" table (Time, Agent/Goal ≤60 chars, Model, Tokens, Cost, Status), a budget bar with the warn mark, quick actions Connect provider / New agent / Index a collection.
- **`/ai/providers`** — table: Name, Protocol, Base URL, Models, Default badge, Status, Updated; search by name/base URL; row actions Edit, Set default, Discover models, Enable/Disable, Delete (confirm by typing the name). Form: Name (1–64, unique case-insensitively), Protocol (OpenAI-compatible — the only value in v1), Base URL (http/https, no whitespace), API key (write-only; blank keeps the stored one), Enabled. Empty state "No provider connected yet"; `LoadingTable` skeleton; error banner with the API message and Retry.
- **`/ai/models`** — table: Model key (`provider/model`), Provider, Context window, Tools, Vision, Streaming, Embeddings, Default, Status; filters provider and capability; bulk Enable/Disable/Delete; row actions Edit capabilities and Set default.
- **`/ai/agents`, `/ai/agents/new`, `/ai/agents/[id]`** — table: Name, Model, Tools, Approvals, Runs 30d, Last run, Status, Updated. Form: Name (1–80), Description (≤400), Model (required), System prompt (≤8000 with a live counter), Temperature (0.00–1.00 step 0.05, default 0.20), Max steps (1–20, default 8), Tools (grouped multi-select, each row naming the permission it needs and disabled when the editor lacks it), Approvals (multi-select of tool keys), Enabled. Validation is field-level; an unknown tool key is refused by the API and shown under its field.
- **`/ai/knowledge`, `/ai/knowledge/[collection]`** — collections table: Name, Documents, Chunks, Embedding model, Status (indexing/indexed/failed), Last indexed. Detail: documents table (Title, Source, Language, Chunks, Status, Indexed at, Error) with Add documents (page picker, media picker, paste text), Reindex and Delete, plus a search box returning ranked chunks with source links.
- **`/ai/runs`, `/ai/runs/[id]`** — table: Started, Agent (or "Chat"), User, Goal, Model, Steps, Tokens, Cost, Status, Approval badge; filters agent, status, date range, user. Detail: run header, step trace (kind, tool, arguments, result/error, tokens, duration), Approve/Reject on a pending step, Cancel while running.
- **`/ai/costs`** — range picker (7/30/90 days, custom), group-by (Provider/Model/Agent/Organization/Site/User), table with Tokens, Requests, Cost and a share bar, totals row, CSV export; budgets panel with monthly limit, warn %, action (warn/block) and per-site overrides.
- **`/ai/settings`** — routing table (one row per task → model select, "Use default" reset); per-provider data policy (Allow user data, Allow media, Allow analytics, Allow private content, Mask PII on by default, "Test masking" preview); danger zone "Pause the engine for this installation".
- **Editor integrations** — `/pages/[id]` toolbar gains an **AI** menu: Summarize, SEO title + description (writes the fields), Suggest slug, Translate → language picker (draft translation), Ask Omnion about this page (palette in Ask mode with the page as context). Generated text lands in a draft revision with a `source='ai'` comment naming the model. `/media` detail drawer gains Generate alt text.
- **Ask mode (palette shell owned by REQ-002)** — `⌘K` → a sentence → the palette shows a plan, one line per tool call with its target; Enter executes when every tool is permitted, blocked rows render disabled with the missing permission named, destructive tools route to approvals.
- **Keyboard** — `⌘K` palette, `⌘⇧A` AI Hub, `G` then `P` providers, `G` then `R` runs, `/` focus the screen search, `N` new row, `Esc` close drawers, `↑/↓` + `Enter` move/open a row.
- **Mobile (<1024px)** — tables become label/value cards, the agent form stacks, the routing table becomes a dropdown list, the palette is a full-screen sheet, the run trace is an accordion; no action hides behind hover.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/providers` | List connected providers | `ai.providers.read` |
| POST | `/api/v1/ai/providers` | Connect a provider | `ai.providers.manage` |
| PATCH/DELETE | `/api/v1/ai/providers/{id}` | Change or remove a provider | `ai.providers.manage` |
| GET | `/api/v1/ai/models` | Model registry with capability flags | `ai.providers.read` |
| PUT | `/api/v1/ai/providers/{id}/models` | Replace a provider's model set | `ai.providers.manage` |
| POST | `/api/v1/ai/providers/{id}/discover-models` | Ask the provider what it serves | `ai.providers.manage` |
| POST | `/api/v1/ai/chat` | Streaming chat (SSE) — exists | `ai.chat` |
| POST | `/api/v1/ai/tasks/{kind}` | Content/vision task (`summarize`, `seo`, `alt_text`, `translate`, `analyze`) | `ai.chat` |
| GET/PUT | `/api/v1/ai/routing` | Read/replace the task → model map | `ai.providers.read` / `ai.settings.manage` |
| GET/PUT | `/api/v1/ai/data-policy` | Per-provider policy + PII masking | `ai.providers.read` / `ai.settings.manage` |
| GET/POST | `/api/v1/ai/agents` | List / create agents | `ai.agents.read` / `ai.agents.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/agents/{id}` | Read / change / remove an agent | `ai.agents.read` / `ai.agents.manage` |
| POST | `/api/v1/ai/agents/{id}/run` | Start a run (SSE progress) | `ai.agents.run` |
| GET | `/api/v1/ai/runs`, `/api/v1/ai/runs/{id}` | Run history and one trace | `ai.agents.read` |
| POST | `/api/v1/ai/runs/{id}/approve` · `/reject` · `/cancel` | Decide a pending step, stop a run | `ai.approvals.act` / `ai.agents.run` |
| GET/POST | `/api/v1/ai/knowledge/collections` | List / create collections | `ai.knowledge.read` / `ai.knowledge.manage` |
| PATCH/DELETE | `/api/v1/ai/knowledge/collections/{id}` | Change / remove a collection | `ai.knowledge.manage` |
| POST | `/api/v1/ai/knowledge/collections/{id}/documents` · `/reindex` | Add documents, re-embed | `ai.knowledge.manage` |
| GET | `/api/v1/ai/knowledge/search` | Vector search with citations | `ai.knowledge.read` |
| GET | `/api/v1/ai/usage` | Usage/cost roll-ups (`from`, `to`, `group_by`) | `ai.usage.read` |
| GET/PUT | `/api/v1/ai/budgets` | Read/set monthly limits | `ai.usage.read` / `ai.settings.manage` |

New keys in the catalogue (`crates/permissions`): `ai.agents.read`, `ai.agents.manage`,
`ai.agents.run`, `ai.approvals.act`, `ai.knowledge.read`, `ai.knowledge.manage`, `ai.usage.read`,
`ai.settings.manage`. Every tool resolves to the human permission of the same endpoint — an agent
never widens authority.

### Data model

Migration `database/migrations/0011_ai_engine.sql` (take the next free number if 0011 is used —
released migrations are append-only). It also adds `input_cost_micros_per_mtok` and
`output_cost_micros_per_mtok` (bigint, nullable, ≥0) to `ai_models`, and to `ai_providers`:
`allow_user_data`, `allow_media`, `allow_analytics`, `allow_private_content` (bool, default false)
plus `mask_pii` (bool, default true).

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_agents` | id uuid pk, organization_id uuid → organizations cascade, site_id uuid → sites cascade, key text, name text, description text default '', system_prompt text, model_id uuid → ai_models set null, temperature numeric(3,2) default 0.20, max_steps int default 8, tools jsonb default '[]', approvals jsonb default '[]', enabled bool default true, created_by uuid → users set null, created_at, updated_at | unique `(organization_id, key)`; `(organization_id, created_at desc)` |
| `ai_runs` | id uuid pk, organization_id, site_id, agent_id uuid → ai_agents set null, user_id uuid → users set null, trigger text ('chat','agent','workflow','schedule'), goal text, status text ('running','awaiting_approval','completed','failed','cancelled'), model_id uuid set null, prompt_tokens int, completion_tokens int, cost_micros bigint, started_at, finished_at, error text | `(organization_id, started_at desc)`; `(started_at)` where running; `(agent_id, started_at desc)` |
| `ai_run_steps` | id uuid pk, run_id uuid → ai_runs cascade, step_no int, kind text ('message','tool_call','tool_result','approval','note'), tool text, arguments jsonb, result jsonb, status text, prompt_tokens int, completion_tokens int, error text, started_at, finished_at, unique `(run_id, step_no)` | `(run_id, step_no)` |
| `ai_approvals` | id uuid pk, run_id uuid cascade, step_id uuid cascade, organization_id uuid, requested_at, decided_by uuid → users set null, decided_at, decision text ('approved','rejected'), note text, check `(decision is null) = (decided_at is null)` | `(organization_id, requested_at)` where decision is null |
| `ai_knowledge_collections` | id uuid pk, organization_id, site_id, key text, name text, description text default '', embedding_model_id uuid set null, chunk_size int default 800, chunk_overlap int default 120, enabled bool default true, created_at, updated_at | unique `(organization_id, key)` |
| `ai_knowledge_documents` | id uuid pk, collection_id uuid cascade, organization_id, source text ('page_revision','media','manual'), source_id text, title text, language text, status text ('pending','indexed','failed'), chunk_count int default 0, error text, indexed_at, created_at | unique `(collection_id, source, source_id)`; `(status)` where not 'indexed' |
| `ai_knowledge_chunks` | id bigserial pk, document_id uuid cascade, collection_id uuid, chunk_no int, content text, tokens int, embedding vector(1536) | `(document_id, chunk_no)`; ivfflat/hnsw on `embedding` (cosine) |
| `ai_usage` | id bigserial pk, organization_id, site_id, user_id uuid set null, run_id uuid set null, provider_id uuid set null, model_id uuid set null, task text, prompt_tokens int, completion_tokens int, cost_micros bigint, created_at | `(organization_id, created_at desc)`; `(created_at)`; `(model_id, created_at desc)` |
| `ai_budgets` | id uuid pk, organization_id uuid cascade, site_id uuid cascade, monthly_limit_micros bigint, warn_percent int default 80, action text ('warn','block'), created_at, updated_at, checks limit ≥0 and warn_percent 1–100 | unique `(organization_id, coalesce(site_id,'00000000-…'::uuid))` |
| `ai_memory` | id uuid pk, organization_id, site_id, agent_id uuid cascade, scope text ('organization','site','agent','user'), user_id uuid cascade, key text, value jsonb, updated_at | unique `(scope, coalesce(organization_id…), coalesce(agent_id…), coalesce(user_id…), key)` |

The migration enables `pgvector` (`create extension if not exists vector`) and fails loudly when the
extension is unavailable — knowledge stays off rather than silently degrading. Embeddings are pinned
to 1536 dimensions in v1; a model with another dimension is rejected when a collection is created,
with the reason in the message. Cost is tokens × model prices; a model without prices records tokens
at zero cost and is flagged "no price configured" on `/ai/costs`.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.run.started` | emitted | run, agent, model, user — an organization's webhook endpoints may subscribe |
| `ai.run.completed` | emitted | tokens, cost, duration — usable as an automation trigger (REQ-003) |
| `ai.run.failed` | emitted | error code — the usual alert hook |
| `ai.run.approval_requested` | emitted | run, step, tool — drives the approval inbox and REQ-021 notifications |
| `ai.run.approved` / `ai.run.rejected` | emitted | run, decided_by |
| `ai.knowledge.document.indexed` / `.failed` | emitted | document, chunk count / error |
| `ai.budget.threshold_reached` | emitted | organization, site, percent, action |
| `page.published`, `media.created` | consumed | per-collection subscriptions re-index the named source |

All events ride the existing signed webhook bus (`x-omnion-signature`) and carry `organization_id`,
so an org-scoped endpoint only receives its own deliveries.

### Acceptance criteria

- [ ] Provider, model, routing, agent, knowledge, run and cost screens exist at the routes above and appear in the QA walkthrough inventory.
- [ ] `POST /api/v1/ai/tasks/summarize` with a page revision id returns a summary and writes an `ai_usage` row with tokens.
- [ ] The editor AI menu inserts an SEO title and description into the open revision and records a `source='ai'` comment naming the model.
- [ ] Alt-text generation works from the media drawer and stores the text on the media record.
- [ ] Translation writes a draft translation for the target language; publishing still requires a human.
- [ ] A model with `supports_embeddings = false` cannot be chosen as a collection's embedding model (API refuses, UI filters it out).
- [ ] Indexing a page revision produces chunks and a `/ai/knowledge/search` hit with a source link; reindexing twice leaves the same chunk count.
- [ ] A permitted agent tool calls the API and records a `tool_call` step with arguments and result; an unpermitted tool is refused with a stable code and no partial side effect.
- [ ] A tool on the approval list parks the run as `awaiting_approval`; approving resumes it, rejecting ends it without the effect.
- [ ] Approving requires `ai.approvals.act`; an account with only `ai.agents.read` sees the control disabled with the missing permission named.
- [ ] `/ai/costs` totals equal the sum of `ai_usage.cost_micros` for the chosen range (asserted against SQL in the test).
- [ ] A budget with `action='block'` refuses the next call with a clear message; `warn` lets it through and emits `ai.budget.threshold_reached`.
- [ ] PII masking replaces an e-mail address in a prompt with a placeholder before the provider call (proven with a stub provider) and restores it in the answer.
- [ ] A provider with `allow_private_content = false` refuses a task carrying a draft revision and names the policy.
- [ ] Ask mode renders a plan and refuses to execute one containing a tool the caller lacks.
- [ ] Organization A cannot read organization B's agents, runs or collections (404).
- [ ] Every screen has empty, loading and error states with a real call to action; no dead button and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: open `/ai` and follow every stat link; create a provider (invalid URL first →
field error, then a working local endpoint), discover models, set a default, toggle a capability on
`/ai/models`; create an agent, disable one tool, save, run it once and watch the SSE steps land in
`/ai/runs`; cancel a run and decide an approval from the run detail; create a collection, index a
page, search it and click a citation; open `/ai/costs`, switch grouping and range, export the CSV;
open `/ai/settings`, flip a data-policy toggle and run Test masking. The mobile pass (390×844) and
the fresh-database empty states run over every screen.

The visual check must see: one primary button per screen, aligned numeric columns, no clipped
cost/date cells, readable status-badge contrast, the palette sheet filling the mobile viewport, no
raw i18n keys, and no text overlapping the run-trace rows.

### Slices

1. **Task router + content tasks** — task map endpoint, `POST /ai/tasks/{kind}`, price columns, `ai_usage` writes, editor AI menu (summarize, SEO, slug), routing table UI.
   *Done when:* summarize and SEO run end to end from the editor, tokens and cost land in `ai_usage`, and `/ai/costs` shows the same number.
2. **Knowledge base** — `pgvector` migration, collections/documents/chunks, indexing from a page revision and a pasted note, search with citations, `/ai/knowledge` screens, reindex.
   *Done when:* a fresh collection indexes a page and a search returns the chunk with its source link, twice in a row with the same result count.
3. **Agents, tools, approvals** — agent CRUD and screens, tool registry mapped to permission keys, run/step persistence, approval inbox, cancel, `/ai/runs` list and trace.
   *Done when:* a permitted run performs a real API write, an unpermitted one is refused, and a gated one completes only after a decision.
4. **Cost, guard, NL operations** — budgets and blocking, data policy with PII masking, Ask-mode palette integration, events and webhooks, mobile and empty-state polish.
   *Done when:* a blocked budget stops a call, masking is proven against a stub provider, and Ask mode executes a two-tool plan on permitted tools only.

### Risks / notes

- `pgvector` is not available on every managed PostgreSQL: the migration must fail with a clear
  message and the UI must say "knowledge indexing unavailable" instead of offering a dead button.
- Price fields are operator-entered and drift; label cost as an estimate, never as an invoice.
- Provider keys are secret material: written through the API, never returned (the v0 rule holds).
- Agents must not become a permission back door — the agent role is checked per tool call, not once
  per run.
- Cost accounting must be written on stream completion *and* on stream failure, or budgets
  underestimate; and a prompt larger than the chosen model's context window is refused naming the
  model and the limit, never silently truncated.
