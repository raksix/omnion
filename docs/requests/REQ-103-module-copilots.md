# REQ-103 — Per-Module AI Copilots

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** modules + `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

An assistant wherever the work happens.

- Named copilots: SEO, CRM, Support, Translation, Content, Analytics, Security, DevOps, Image, Review.
- Copilot surface: side panel on module screens with suggested prompts and context awareness.
- Actions each copilot may take (wired to REQ-100 tools) and its approval gates.
- Conversation history per module and per user; share a conversation with a teammate.
- Metrics: usage, acceptance rate of suggestions, cost per module.

## Implementation spec

### Scope (in / out)

**In** — a copilot is data, not code: one row that names a module, a prompt, a model preference, a
tool allow-list and its approval gates, plus the suggested prompts the panel offers. Ten ship
seeded (SEO, CRM, Support, Translation, Content, Analytics, Security, DevOps, Image, Review) and an
operator can edit them or add one.

- **Copilot record** — `key` (unique per organization), name, module, description, system prompt,
  model override (falls back to the task router), temperature, tool allow-list with a per-tool
  approval flag, knowledge collection set, memory scopes it may read, suggested prompts, enabled.
- **Context awareness** — the panel always sends a context envelope: module key, screen route, the
  selected record (id, type, title, and the fields the copilot's allow-list permits reading), site
  and organization, plus the caller's language. The envelope is recorded on the message row so a
  later reviewer can see what the answer was based on.
- **Actions** — a proposed action renders as a card with the tool name, a readable summary of its
  arguments and a diff-style preview of the effect. Running it calls the same endpoint a human
  would, under the caller's permissions (REQ-100 tool registry); a tool on the copilot's approval
  list parks the action as an approval request (docs/06 §9) and the panel shows "waiting for
  approval" until it is decided.
- **Conversations** — per module and per user; a conversation is resumable, renameable, archivable
  and deletable. Sharing creates a read-only link with an expiry (default seven days) that renders
  the transcript without the context envelope's raw values.

**Out**

- New tools or permissions of its own: copilots are a *surface* over the tool registry, they add no
  authority and no endpoint.
- Autonomous loops — a copilot answers and proposes; running a long multi-step plan is the agent
  runtime (REQ-001) and its run screen.
- Cross-organization sharing; a shared conversation link is tenant-scoped and revocable.
- Mobile-native shells: the panel becomes a bottom sheet in the existing responsive admin.
- Chat-with-your-documents grounding beyond the collections a copilot names (REQ-102 owns
  indexing).

### Screens (UI)

- **`/ai/copilots`** — table: Key, Name, Module, Model, Tools (count), Approvals (count), Prompts,
  Conversations 30d, Acceptance, Cost 30d, Status, Updated. Filters: module, status, model,
  free-text name/key. Row actions Edit, Duplicate, Enable/Disable, Delete (confirm by typing the
  key). Empty state "No copilot yet" with Seed the defaults / New copilot.
- **`/ai/copilots/new` and `/ai/copilots/[key]`** — full-page editor with tabs. **Basics**: key
  (`[a-z0-9-]{2,40}`, immutable after creation), name (1–80), module (select), description (≤400),
  enabled. **Prompt**: system prompt (≤8000 with a live counter and a "reset to default" action),
  model override (select of enabled models, blank = router default), temperature (0.00–1.00 step
  0.05, default 0.20). **Tools**: grouped multi-select where each tool row names the permission it
  needs and is disabled when the caller lacks it, plus an "requires approval" checkbox per tool.
  **Knowledge & memory**: collection multi-select, memory scopes checkboxes. **Prompts**: an ordered
  list of suggested prompts (label 1–60, text ≤300, drag to reorder, max 8). Validation is
  field-level; the API repeats every rule and names the offending field.
- **Copilot panel (module screens)** — header with copilot name, scope chip and a "new chat"
  action; message list with streamed answers, citation chips and action cards; suggested prompts
  shown while the conversation is empty; a composer with send on `Enter`, newline on `Shift+Enter`
  and a stop control while streaming. Panel states: unavailable (module not installed), not
  permitted (names the missing permission), loading history (skeleton), error (message + Retry),
  empty (suggested prompts, no placeholder prose).
- **`/ai/shared/[token]`** — read-only transcript view: copilot name, created/expiry date, messages
  without context-envelope values, a notice that the link expires, and no composer. An expired or
  revoked token renders a neutral "this link is no longer available".
- **Keyboard** — `⌘J` (`Ctrl+J`) toggles the panel from any module screen, `Esc` closes it and
  returns focus to the invoking control, `↑/↓` move through suggested prompts, `Enter` sends/accepts,
  `⌘Enter` runs a proposed action, `⌘⇧N` opens a new conversation, `/` focuses the copilots search.
- **Mobile (<1024px)** — the panel opens as a full-height sheet with a visible close control, action
  cards stack with their preview collapsed behind a "Show preview" toggle, the editor tabs scroll
  horizontally, the conversations table becomes cards and the composer sticks above the safe area.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/ai/copilots` | List / create copilots (`module`, `status`, `q`) | `ai.copilots.read` / `ai.copilots.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/copilots/{key}` | Read / change / remove one copilot | `ai.copilots.read` / `ai.copilots.manage` |
| POST | `/api/v1/ai/copilots/{key}/seed-defaults` | Create the ten seeded copilots | `ai.copilots.manage` |
| GET | `/api/v1/ai/copilots/{key}/metrics` | Usage, acceptance, cost for a range | `ai.usage.read` |
| GET/POST | `/api/v1/ai/conversations` | List (own, or all with `ai.conversations.read`) / start one | `ai.conversations.read` / `ai.chat` |
| GET/PATCH/DELETE | `/api/v1/ai/conversations/{id}` | Read / rename-archive / delete | `ai.conversations.read` / `ai.conversations.manage` |
| POST | `/api/v1/ai/conversations/{id}/messages` | Send a turn (SSE: `start`, `delta`, `action`, `citation`, `done`, `error`) | `ai.chat` |
| POST/DELETE | `/api/v1/ai/conversations/{id}/share` | Create / revoke the read-only link | `ai.conversations.share` |
| GET | `/api/v1/ai/shared/{token}` | Read a shared transcript (no session required) | — (token is the credential) |
| POST | `/api/v1/ai/conversations/{id}/messages/{mid}/feedback` | Accept / reject / rate an answer or action | `ai.chat` |
| POST | `/api/v1/ai/conversations/{id}/actions/{aid}/run` | Execute a proposed action | the action tool's own permission |

New catalogue keys: `ai.copilots.read`, `ai.copilots.manage`, `ai.conversations.read`,
`ai.conversations.manage`, `ai.conversations.share`. Running a proposed action checks the tool's
own permission (`page.update`, `ticket.reply`, …) as well as `ai.chat`, so a copilot can never
widen what its user may do.

### Data model

Migration `database/migrations/00NN_copilots.sql` (00NN = next free integer at land time; 0017 was
free when this was written). It also adds the `conversation_id` foreign key REQ-102 deferred, once
`ai_conversations` exists, and reuses `ai_agents`/`ai_runs` for anything a copilot runs through the
agent runtime.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_copilots` | id uuid pk, organization_id uuid → organizations cascade, key text, name text, module text, description text default '', system_prompt text, model_id uuid null → ai_models set null, temperature numeric(3,2) default 0.20, tools jsonb default '[]' (`[{tool, approval_required}]`), collections jsonb default '[]', memory_scopes text[] default '{site,user}', icon text null, enabled bool default true, seeded bool default false, created_by uuid null → users set null, created_at, updated_at | unique `(organization_id, key)`; `(organization_id, module, enabled)` |
| `ai_copilot_prompts` | id uuid pk, copilot_id uuid → cascade, label text, prompt text, position smallint default 0, created_at | unique `(copilot_id, position)`; `(copilot_id, label)` |
| `ai_conversations` | id uuid pk, organization_id, site_id uuid null → cascade, copilot_id uuid null → ai_copilots set null, module text, user_id uuid → users cascade, title text, context jsonb default '{}', status text ('active','archived'), last_message_at timestamptz, shared_token_hash text null, shared_expires_at timestamptz null, shared_by uuid null → users set null, message_count int default 0, created_at, updated_at | `(organization_id, module, last_message_at desc)`; `(user_id, last_message_at desc)`; `(organization_id, status)`; unique `(shared_token_hash)` where not null |
| `ai_messages` | id bigserial pk, conversation_id uuid → cascade, organization_id, role text ('system','user','assistant','tool'), content text, model_id uuid null → ai_models set null, prompt_tokens int, completion_tokens int, cost_micros bigint, citations jsonb default '[]', context_used jsonb default '{}', run_id uuid null, status text ('ok','error','stopped'), error_code text null, created_at | `(conversation_id, id)`; `(organization_id, created_at desc)` |
| `ai_message_actions` | id uuid pk, message_id bigint → cascade, conversation_id uuid, tool text, permission text, arguments jsonb, preview jsonb, status text ('proposed','approved','rejected','done','failed','cancelled'), approval_id uuid null, run_step_id uuid null, decided_by uuid null → users set null, decided_at, result jsonb, error text, created_at | `(conversation_id, created_at desc)`; `(status)` where status in ('proposed','approved') |
| `ai_message_feedback` | id uuid pk, message_id bigint → cascade, user_id uuid → users cascade, kind text ('accepted','rejected','rated_up','rated_down','edited'), note text null, created_at | unique `(message_id, user_id, kind)`; `(user_id, created_at desc)` |

Acceptance rate is derived, never stored as a counter: accepted proposals ÷ proposals over the
range, computed from `ai_message_actions` and `ai_message_feedback`. Conversation deletion cascades
to messages, actions and feedback; a shared link dies with the conversation (the hash row is the
conversation row, so there is no orphan token table).

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.copilot.created` / `.updated` / `.disabled` | emitted | key, module, actor — an operator audit trail |
| `ai.conversation.started` | emitted | copilot, module, user, site — the denominator of usage metrics |
| `ai.action.proposed` | emitted | tool, permission, conversation — feeds the approval inbox (REQ-021) |
| `ai.action.approved` / `.rejected` / `.failed` | emitted | decision, actor, tool, error code |

All events ride the existing signed webhook bus with `organization_id` attached, so an
organization-scoped endpoint receives only its own deliveries.

### Acceptance criteria

- [ ] The migration seeds the ten named copilots for the existing organization, each with a prompt, at least one suggested prompt and a conservative tool list.
- [ ] `/ai/copilots` lists them with module, tools, approvals, conversations 30d and cost 30d; filtering by module and status works and is reflected in the URL.
- [ ] A tool row the caller cannot use is disabled and names the missing permission; a copilot whose tool is not installed renders "unavailable" instead of failing at send time.
- [ ] The context envelope carries the selected record only for fields the copilot's allow-list permits; an out-of-scope field is absent from the stored `context_used` (asserted in a test).
- [ ] A proposed action renders its arguments and preview; running it performs the real operation and writes `ai_message_actions.status='done'` with the result.
- [ ] A tool flagged `approval_required` parks the action as pending, shows "waiting for approval", and cannot be executed by the requester before a decision.
- [ ] Conversations persist per user and per module; a second user cannot list or open them (404), and an admin with `ai.conversations.read` can.
- [ ] Feedback (accept/reject/rate) is recorded once per user per message and changes the acceptance metric on `/ai/copilots`.
- [ ] `/ai/copilots/{key}/metrics` answers usage, acceptance and cost, and the cost matches the `ai_usage` rows for that copilot's conversations over the same range.
- [ ] Deleting a conversation removes its messages, actions and feedback (asserted by SQL count after the call).
- [ ] Every screen and the panel have empty, loading and error states with a real action; the mobile sheet (390×844) is usable with one thumb.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/ai/copilots`, seed the defaults, edit the SEO copilot (clear the
name → field error, set it again), disable one tool and enable its approval flag, reorder a
suggested prompt and save; open a page in the pages module, toggle the panel with `⌘J`, answer two
suggested prompts, send a free-text question and stop a streaming answer mid-flight; propose an
action, run a permitted one, then run an approval-gated one and decide it from the approvals screen;
rename and share a conversation, open the shared link in a private window and confirm it renders
read-only, then revoke it and reload (expect the neutral message); give feedback on two answers and
confirm the acceptance number moves on the copilot row; archive a conversation and open
`/ai/copilots/conversations` with each filter. Mobile pass over the panel, the editor and the
conversation list.

The visual check must see: one primary action per screen, the panel not covering the page's own
toolbar, action cards with readable argument summaries and a collapsed preview on mobile, aligned
conversation counts, readable status badges, no raw i18n keys and no text overlapping the composer
while streaming.

### Slices

1. **Copilot records and editor** — `ai_copilots`, `ai_copilot_prompts`, seeds, CRUD endpoints,
   `/ai/copilots` list and editor screens, permission keys, audit events.
   *Done when:* the ten copilots exist, one can be created, edited, duplicated and deleted, and the
   tool multi-select hides what the caller cannot use.
2. **Conversations and the panel** — `ai_conversations`, `ai_messages`, send-turn SSE, the context
   envelope, the shared panel component mounted on module screens, history, rename, archive.
   *Done when:* a question asked from a page screen answers with the page in context and the
   conversation reappears after a reload under the same user.
3. **Actions, approvals and feedback** — `ai_message_actions`, proposal previews, tool permission
   checks, approval routing, run/decide/cancel, feedback capture.
   *Done when:* a permitted action performs a real write, a gated one waits for a decision, and a
   refusal names the missing permission.
4. **Sharing, metrics and polish** — share tokens and the read-only view, metrics endpoint and
   panel numbers, filters, empty/loading/error states, keyboard, mobile sheet.
   *Done when:* the shared link is revocable and expiring, the acceptance rate moves with feedback,
   and both layouts pass the visual check.

### Risks / notes

- Context envelopes can leak: only fields the copilot's allow-list names may be attached, and the
  stored `context_used` must hold references (type, id, field keys) rather than raw sensitive
  values — this is also what REQ-105 watches.
- Cost per module is only meaningful if every copilot call records the `module` and `copilot_key`
  dimensions on the usage row — wire that at the point the message is written, not later.
- Sharing must not become a data export: the transcript hides context values, carries an expiry and
  is revocable; a deleted conversation kills the link.
- The seeded tool lists must stay conservative — a copilot that can publish or delete on day one is
  a bad first impression of the platform's safety model.
