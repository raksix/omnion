# REQ-042 — AI Copilots in Every Module

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** AI Hub + per-module UI
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Instead of a single AI screen — a copilot in each module:

```text
CRM        → CRM Copilot
Accounting → Finance Copilot
CMS        → Content Copilot
HR         → HR Copilot
Support    → Support Copilot
```

All backed by the same AI Hub.

## Notes

- Copilots are thin UI shells over AI Hub agents (docs/06-AI-HUB.md §14–15); each copilot
  ships with a preset role and tool permissions (docs/07-IAM.md §14).

## Implementation spec

### Scope (in / out)

**In**

- A copilot **registry**: reusable presets (`crm`, `finance`, `content`, `hr`, `support`, plus a
  `general` fallback) binding a system prompt, a model from the AI Hub registry, a tool
  allowlist, a data scope and a cost cap.
- One shared `CopilotPanel` component mounted by every module screen; context (module, route,
  selected record id, filters) passes in as a structured context block.
- Seeded presets with read-only domain tools, and write tools behind the approval flow
  (docs/06-AI-HUB.md §9) — no copilot writes without an approved tool call.
- Conversation persistence, transcript export, per-copilot usage and cost attribution, and
  thumbs feedback stored as a message annotation.
- Tool-call transparency: each call renders as a card with tool name, arguments, permission
  check, approval state and result summary; RAG answers cite their source documents.

**Out**

- New model providers or routing rules (REQ-001/REQ-047 own AI Hub depth).
- Autonomous background runs and scheduled copilot jobs (later, gated on approvals).
- Copilots inside `apps/web` public rendering; the panel is an admin surface.
- Voice input, image generation, cross-organization data access (tools inherit caller scope).

### Screens (UI)

Admin routes:

- `/copilots` — registry table. Columns: Copilot (key + name), Module, Model, Tools (`n`, hover
  list), Scope chip, Daily cap, Usage 30 d (messages + cost), Status, Updated. Filters:
  module, status, text. Bulk actions: enable, disable, duplicate (`<key>-copy`), export JSON.
  Row actions: Edit, Preview, Conversations.
- `/copilots/new` and `/copilots/{key}` — one form. Fields: Key (slug `^[a-z][a-z0-9-]{2,31}$`,
  immutable after create), Display name (1–48, required), Module (select, required), Description
  (≤160), System prompt (20–8000 chars, live token estimate), Model (searchable picker from the
  registry), Temperature (0–1, step 0.05, default 0.3), Max output tokens (256–8192, default
  1024), Tools (multi-select with search; each entry names the permission it needs, write tools
  carry a warning icon), Data scope (organization / site / department radio), Daily cost cap
  (0 = uncapped, warned), Status toggle. Validation renders inline per field; Save is disabled
  while invalid, shows "Saving…" while pending, toasts on success and pins a field-level error
  on 422.
- `/copilots/{key}/conversations` — table (User, Title, Messages, Cost, Started, Last message);
  row opens a read-only transcript drawer with role bubbles, tool cards and citations; export
  JSON or delete (audited).
- `/copilots/{key}/preview` — composer that runs the copilot with the editor's draft and lists
  planned tool calls without executing them.
- Embedded `CopilotPanel` (right rail ≥`xl`, bottom sheet below): header with name and model
  chip, context chip ("Contact · Ayşe Yılmaz"), three suggested prompts per module, message
  list, streaming bubble, tool-call cards with approval buttons, stop-generating, feedback row
  (👍/👎 + note), "Open transcript". Empty state: "Ask <Copilot> about this <entity>";
  loading: three skeleton bubbles; error: inline bubble with `Retry` and the code; offline:
  "Reconnecting to AI Hub".
- Keyboard: `Cmd/Ctrl+J` toggle, `Esc` close, `Cmd/Ctrl+Enter` send, `Up` in an empty composer
  edits the last prompt, `Cmd/Ctrl+Shift+L` new conversation, `Tab` cycles tool cards.
- Mobile: full-height bottom sheet with a drag handle, suggested prompts as a chip row, tool
  cards collapsed to one line.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/copilots` | List copilot presets | `copilots.read` |
| POST | `/api/v1/copilots` | Create a preset | `copilots.manage` |
| GET | `/api/v1/copilots/{key}` | Preset detail with tools and scope | `copilots.read` |
| PATCH | `/api/v1/copilots/{key}` | Update preset fields | `copilots.manage` |
| DELETE | `/api/v1/copilots/{key}` | Delete when it has no conversations, else 409 with guidance | `copilots.manage` |
| POST | `/api/v1/copilots/{key}/chat` | Send a message; SSE stream of tokens, tool calls, citations | `copilots.use` |
| GET | `/api/v1/copilots/{key}/conversations` | Conversation list (own history unless `copilots.manage`) | `copilots.use` |
| GET | `/api/v1/copilots/conversations/{id}` | Full transcript with tool calls and citations | `copilots.use` |
| POST | `/api/v1/copilots/conversations/{id}/messages/{mid}/feedback` | 👍/👎 plus note | `copilots.use` |
| POST | `/api/v1/copilots/{key}/preview` | Dry run: planned tool calls, nothing executed | `copilots.manage` |
| GET | `/api/v1/copilots/{key}/usage` | Messages, tokens and cost per day | `copilots.read` |

Tool execution re-checks the caller's permission per tool (docs/07-IAM.md §14); write tools open
an approval request and the stream pauses until it is resolved.

### Data model

Migration `database/migrations/0012_ai_copilots.sql` (next free number at build time).

- `ai_copilots` — `id uuid pk`, `key text not null`, `name text not null`, `module_slug text
  not null default 'general'`, `description text not null default ''`, `system_prompt text not
  null`, `model_id uuid null references ai_models(id) on delete set null`, `temperature
  numeric(3,2) not null default 0.30 check (temperature between 0 and 1)`, `max_output_tokens
  int not null default 1024`, `tool_keys jsonb not null default '[]'::jsonb`, `scope jsonb not
  null default '{}'::jsonb`, `daily_cost_cap_cents int not null default 0`, `status text not
  null default 'enabled' check (status in ('enabled','disabled'))`, `created_by uuid references
  users(id) on delete set null`, `created_at`, `updated_at`. Unique `ai_copilots_key_key (key)`.
- `ai_copilot_conversations` — `id uuid pk`, `copilot_id uuid references ai_copilots(id) on
  delete cascade`, `user_id uuid references users(id) on delete cascade`, `organization_id uuid
  null references organizations(id)`, `site_id uuid null references sites(id)`, `context jsonb
  not null default '{}'::jsonb`, `title text not null default ''`, `message_count int not null
  default 0`, `cost_cents int not null default 0`, `created_at`, `last_message_at`.
- `ai_copilot_messages` — `id bigint identity pk`, `conversation_id uuid references
  ai_copilot_conversations(id) on delete cascade`, `role text not null check (role in
  ('user','assistant','tool'))`, `content text not null default ''`, `tool_calls jsonb not null
  default '[]'::jsonb`, `citations jsonb not null default '[]'::jsonb`, `feedback smallint null
  check (feedback in (-1,1))`, `feedback_note text`, `tokens_in int not null default 0`,
  `tokens_out int not null default 0`, `cost_cents int not null default 0`, `created_at`.
- Indexes: `ai_copilot_conversations_user_idx (user_id, last_message_at desc)`,
  `ai_copilot_conversations_copilot_idx (copilot_id, last_message_at desc)`,
  `ai_copilot_messages_conversation_idx (conversation_id, id)`.
- The migration seeds the six presets idempotently (disabled-cap free, organization scoped).

### Events

- Emitted: `copilot.conversation.started`, `copilot.message.completed`,
  `copilot.tool.approval_requested`, `copilot.tool.executed`, `copilot.budget.exceeded`.
- Consumed: `ai.provider.disabled` and `ai.model.removed` disable affected copilots and raise a
  panel banner.
- Webhook relevance: approval requests and budget overruns are useful to operations tooling;
  transcripts and message bodies are never webhooked.

### Acceptance criteria

- [ ] Six presets exist after migration and appear in `/copilots`.
- [ ] Creating a preset with an invalid key or empty prompt shows inline validation and blocks save.
- [ ] A tool outside the preset's allowlist cannot be invoked, even by prompt injection.
- [ ] Chat responses stream token by token and render markdown safely.
- [ ] A write tool pauses for approval and resumes on approve, cancels cleanly on reject.
- [ ] Approval state, permission check and result summary are visible on each tool card.
- [ ] RAG answers list their source documents with links that open in the panel.
- [ ] The context chip shows the current module and selected record and follows navigation.
- [ ] Suggested prompts differ per module (CRM vs HR vs Support).
- [ ] Conversation list, transcript drawer and JSON export work for the caller's own history.
- [ ] 👍/👎 plus note persist and are visible after reload.
- [ ] Usage reports messages, tokens and cost per day per copilot.
- [ ] The daily cost cap blocks a further message with a clear notice when reached.
- [ ] The panel is keyboard reachable end to end; focus returns to the composer after send.
- [ ] The mobile sheet opens, scrolls and closes without covering header controls.
- [ ] Copilot keys exist in the permission catalogue and the panel hides without `copilots.use`.
- [ ] `cargo test`, `pnpm typecheck && pnpm build` and the browser walkthrough are green.

### QA plan

- Walkthrough: `/copilots` list → create a copilot → edit temperature and cap → disable and
  re-enable; preview a draft plan and confirm tool names appear without results; open
  conversation history and export it.
- Embedded panel: on the contacts screen ask a seeded prompt, watch streaming, trigger a write
  tool and resolve the approval, then send feedback; repeat once on the content screen to prove
  context switching.
- Visual check: the panel does not squeeze the main table at 1440 px, streaming shows a caret,
  tool cards have distinct approval states, feedback state persists, and the mobile sheet has no
  clipped buttons at 390 px.
- Regression: `/ai` (providers, models, chat) still works with no console errors.

### Slices

1. **Registry + presets** — migration `0012_ai_copilots.sql`, preset CRUD API, permission keys,
   `/copilots` table and editor with validation. **Done when:** the six seeds list, a preset is
   created and edited through the UI, and key/prompt validation is covered by tests.
2. **Panel + chat** — `CopilotPanel`, streaming chat with context injection, tool-call cards,
   citations, feedback, transcript drawer. **Done when:** the panel answers on a module screen
   with a visible tool card and stored feedback (walkthrough passes).
3. **Governance** — approval flow for write tools, daily cap, usage panel, provider revocation
   handling, conversation screen. **Done when:** a write tool blocks on approval and a capped
   copilot refuses further messages with a clear notice.

### Risks / notes

- Context injection can leak unrelated records into a prompt: build the block from explicit
  field allowlists per module, never by serializing a whole row.
- Prompt injection through record content is the realistic attack surface; tool permissions, not
  prompt wording, are the enforcement point.
- Cost is unattributable without a per-message write at completion time — attribute to copilot
  and caller, or the cap is meaningless.
- Transcripts stay out of webhooks and logs; mask secrets/PII fields per the AI data policy
  (docs/06-AI-HUB.md §18).
- Preset keys are stable identifiers used by seeds and tests; changing them is a migration.
