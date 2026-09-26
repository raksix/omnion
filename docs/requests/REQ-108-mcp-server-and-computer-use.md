# REQ-108 — MCP Server & Computer Use

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Omnion as a tool for other agents — and browsers as a tool for ours.

- MCP server exposing platform capabilities as tools (content, media, workflow start, analytics query).
- MCP workflow-builder tools so an external agent can draft a workflow.
- Computer-use adapter: drive a browser session from an agent with screenshot/click/type steps.
- Permissions and audit apply to MCP calls exactly as to panel actions.
- Documentation of the tool schemas and a sandbox mode for testing.

## Implementation spec

### Scope (in / out)

**In** — two directions of the same idea: the platform as a tool server, and the browser as a tool
for the platform's agents.

- **MCP server** — a JSON-RPC surface (`initialize`, `tools/list`, `tools/call`, `ping`) over HTTP
  at `/api/v1/mcp` and over stdio for local development, exposing a curated tool set: content read
  and write (pages, revisions, content types), media (search, upload by URL, read metadata),
  workflow start and status, analytics query, global search, translations, tickets and contacts —
  each tool documented with a JSON schema, an example and the permission it requires.
- **Workflow-builder tools** — `workflow_schema`, `workflow_validate`, `workflow_draft` and
  `workflow_save_draft`: an external agent can read the node catalogue, validate a graph and save a
  draft; publishing and activating a workflow are deliberately *not* tools, so a draft always waits
  for a human in the builder (REQ-095 owns versions and sharing).
- **Computer-use adapter** — a browser session owned by the platform, driven step by step
  (`navigate`, `screenshot`, `click`, `type`, `key`, `scroll`, `wait`, `extract`, `handoff`, `done`)
  with an allowed-origin list, a step budget, a viewport, rate limits and a transcript of every step
  with its result. Password fields and payment fields are refused by the `type` step; the adapter
  raises a handoff request instead, and the session waits for the human.
  client can be developed against the sandbox and switched over by one switch.

**Out**

- Consuming third-party MCP servers (the reverse direction) — a separate request; the registry shape
  leaves room for it.
- Desktop or OS-level automation beyond the browser sandbox; no shell, no file system, no clipboard
  beyond what a `key` step emits.
- Credentials in the loop: the adapter never types a password and never reads one; sign-in that needs
  a human is a handoff.
- Captcha or bot-protection circumvention in any form — a session that meets a challenge pauses and
  asks for a handoff.
- Unbounded browsing: origins outside the allow-list are refused, including redirect targets.

### Screens (UI)

- **`/settings/mcp-clients`** — table: Name, Description, Token prefix (last four characters), Tools
  (granted/available), Sandbox, Rate limit/min, Last used, Created, Status, Revoked at. Row actions
  Rotate secret, Edit grants, Toggle sandbox, Revoke, Delete (confirm by typing the name). Filters
  status, sandbox, free text. Create dialog: Name (1–60, unique per organization), Description
  (≤200), Scopes (checkbox list, each naming its permission), Tool grants (a two-pane picker showing
  tool, permission and approval flag; tools the client's scopes cannot support are hidden with a
  note), Sandbox (default on), Rate limit (1–600). The created token is shown exactly once with a
  copy control and a "store this now" warning.
  the JSON arguments, press Test, and read the would-be request and the validation result.
- **`/ai/computer-use`** — sessions table: Started, Status, Start URL, Steps, Origin, Agent/Client,
  User, Duration. Filters status, range, origin, agent. Row actions Open, Stop, Delete (confirm).
- **`/ai/computer-use/[id]`** — session header (start URL, viewport, step budget used, origin
  allow-list, status), a step timeline (step number, kind, input summary, result, duration,
  screenshot thumbnail with an expandable view, error), a live "next step" view while running with a
  Stop and a Take over control, and the handoff state rendered as a banner with the reason. A "Save
  as case" action turns a session into an eval case (REQ-107).
- **`/docs/mcp`** — the generated tool reference (rendered from the registry; searchable; each tool
  with its schema, permission, sandbox support and one example call/response) plus the connect guide.
- **Keyboard** — `/` focuses search, `N` opens the create dialog, `R` rotates the focused client's
  secret (with confirmation), `T` opens the sandbox test panel, `S` stops the focused session, `↑/↓`
  + `Enter` move and open, `Esc` closes dialogs and shifts focus back to the invoking control.
- **Mobile (<1024px)** — client rows become cards with the token prefix and sandbox state leading,
  the grant picker becomes a full-screen sheet with per-tool switches, the session timeline becomes
  cards with screenshots inline, the live step view stacks under the controls, and the docs page
  keeps its per-tool anchors.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/mcp/clients` | List / create MCP clients and their token | `mcp.clients.read` / `mcp.clients.manage` |
| GET/PATCH/DELETE | `/api/v1/mcp/clients/{id}` | Read / change / remove a client | `mcp.clients.read` / `mcp.clients.manage` |
| POST | `/api/v1/mcp/clients/{id}/rotate-secret` | Issue a new token and drop the old one | `mcp.clients.manage` |
| POST | `/api/v1/mcp/clients/{id}/revoke` | Revoke without deleting (keeps the audit trail) | `mcp.clients.manage` |
| GET/PUT | `/api/v1/mcp/clients/{id}/tools` | Read / replace the tool grant list | `mcp.clients.read` / `mcp.clients.manage` |
| POST | `/api/v1/mcp` | JSON-RPC: `initialize`, `tools/list`, `tools/call`, `ping` | client token scope ∩ tool permission |
| POST | `/api/v1/ai/computer-use/sessions` | Start a session (start URL, origin list, budget) | `ai.computer_use.run` |
| GET | `/api/v1/ai/computer-use/sessions` | List sessions with filters | `ai.computer_use.read` |
| GET | `/api/v1/ai/computer-use/sessions/{id}` | One session with its steps | `ai.computer_use.read` |
| POST | `/api/v1/ai/computer-use/sessions/{id}/steps` | Append and execute one step (SSE result) | `ai.computer_use.run` |
| POST | `/api/v1/ai/computer-use/sessions/{id}/stop` | Stop a session | `ai.computer_use.run` |
| GET/PUT | `/api/v1/ai/computer-use/settings` | Origin allow-list, viewport, budget, retention | `ai.computer_use.read` / `ai.computer_use.manage` |

New catalogue keys: `mcp.clients.read`, `mcp.clients.manage`, `ai.computer_use.read`,
`ai.computer_use.run`, `ai.computer_use.manage`. An MCP token authenticates a *client*, and every
tool call resolves to the same permission guard the panel endpoint uses; a denial answers a JSON-RPC
error with `code = -32003`, the missing permission name and no side effect.

### Data model

Migration `database/migrations/00NN_mcp_and_computer_use.sql` (00NN = next free integer at land time;
0022 was free when this was written). Client tokens are stored as a SHA-256 hash with a short prefix
kept in clear for recognition, in the same shape as the platform's other API keys.

| Table | Columns (types) | Indexes |
|---|---|---|
| `mcp_clients` | id uuid pk, organization_id uuid → organizations cascade, name text, description text default '', token_prefix text (8 chars), token_hash text, scopes jsonb default '[]', sandbox bool default true, rate_limit_per_min int default 60 check (1–600), enabled bool default true, last_used_at timestamptz null, created_by uuid null → users set null, created_at, updated_at, revoked_at timestamptz null | unique `(organization_id, name)`; unique `(token_hash)`; `(organization_id, enabled)`; `(token_prefix)` |
| `mcp_client_tools` | client_id uuid → cascade, tool text, permission text, approval_required bool default false, enabled bool default true, added_at | pk `(client_id, tool)`; `(tool)` |
| `mcp_invocations` | id bigserial pk, organization_id, client_id uuid null → cascade, jsonrpc_id text null, tool text, permission text null, arguments_sha256 text, arguments_preview jsonb default '{}' (masked by REQ-105), status text ('ok','error','denied','sandbox','blocked_airgap'), error_code text null, duration_ms int, approval_id uuid null, run_id uuid null, created_at | `(organization_id, created_at desc)`; `(client_id, created_at desc)`; `(tool, status, created_at desc)`; `(created_at)` for purge |
| `computer_use_sessions` | id uuid pk, organization_id, site_id uuid null, user_id uuid null → users set null, client_id uuid null → mcp_clients set null, agent_id uuid null → ai_agents set null, status text ('running','waiting_for_human','paused','completed','failed','stopped'), start_url text, allowed_origins jsonb default '[]', viewport text default '1280x800', step_count int default 0, max_steps int default 60, goal text null, error text null, started_at, finished_at, last_step_at | `(organization_id, started_at desc)`; `(status)` where status in ('running','waiting_for_human'); `(organization_id, user_id, started_at desc)` |
| `computer_use_steps` | id bigserial pk, session_id uuid → cascade, organization_id, step_no int, kind text ('navigate','screenshot','click','type','key','scroll','wait','extract','handoff','done'), input jsonb, result jsonb, screenshot_media_id uuid null → media set null, url_after text null, status text ('ok','error','refused','pending_approval'), error_code text null, duration_ms int, created_at | unique `(session_id, step_no)`; `(session_id, created_at)`; `(status)` where status <> 'ok' |
| `computer_use_origins` | id uuid pk, organization_id → cascade, pattern text, note text, created_by uuid null → users set null, created_at | unique `(organization_id, pattern)` |

Steps are append-only and numbered per session; the session row carries the counters and the status
so a reload renders the whole timeline from two queries. Screenshots land in the existing media
store with a retention window and are deleted with the session when retention says so.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `mcp.client.created` / `.secret_rotated` / `.revoked` | emitted | name, prefix, actor — a security-relevant audit trail |
| `mcp.tool.invoked` | emitted | client, tool, status, duration — the denominator of client usage |
| `mcp.tool.denied` | emitted | client, tool, missing permission — the signal that a grant list is too narrow |
| `mcp.approval_requested` | emitted | client, tool, arguments summary — routes to the approval inbox (REQ-021) |

### Acceptance criteria

- [ ] `POST /api/v1/mcp` with a valid client token answers `initialize` with the server name and protocol version, and an unknown token answers a JSON-RPC error without leaking whether the token exists.
- [ ] `tools/list` for a client with three grants returns exactly those three tools, each with a JSON schema, a permission and a sandbox flag.
- [ ] `tools/call` for a tool the client's scopes cannot support is denied with the missing permission named and no side effect (asserted by row counts before and after).
- [ ] A gated write tool parks an approval request and returns a "pending approval" response; approving it from the approvals screen executes the write and the invocation row moves to `ok`.
- [ ] Every invocation writes one audit entry and one `ai_request_logs` row with feature `mcp:<tool>`; the arguments stored are masked by REQ-105 (an email in the arguments is not readable in the log).
- [ ] A revoked client's token stops working immediately while its invocation history remains readable.
- [ ] Starting a computer-use session stores the start URL, origin list and budget; the first `navigate` refuses an origin outside the list, and the refusal is written as a step with `status = 'refused'`.
- [ ] `type` into a password or payment field is refused with the refusal reason recorded, and the session raises a handoff request instead.
- [ ] Screenshots follow the retention setting: with retention off, no screenshot is stored; with retention on, deleting the session removes its screenshots from the media store.
- [ ] A handoff keeps the session in `waiting_for_human` until a human resolves it, and the resolution is recorded with the actor.
- [ ] Every screen has empty, loading and error states with a real action; the token is shown once and never again.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/settings/mcp-clients`, create a client with a duplicate name
(field error) and then a valid one, copy the token once, reload and confirm it is not shown again;
open the sandbox test panel, run a read tool with malformed arguments (validation error naming the
field) and then a valid call, reading the would-be request; from a terminal and then from the built
documentation page, run `initialize` and `tools/list` against `/api/v1/mcp` with the token and
compare the returned catalogue with the page; call a read tool live and confirm the record changes
in the panel; call a gated write tool, then approve it from the approvals screen and confirm the
change landed; rotate the secret, confirm the old token fails and the new one works; revoke the
client and confirm its history is still readable; open `/ai/computer-use`, start a session against
an allowed origin, walk a navigate → screenshot → click → extract path, then attempt a
non-allow-listed origin (refused step), attempt a password field (refused + handoff), take over,
resolve the handoff and stop the session; open `/ai/computer-use/settings` and change the retention,
then delete a session and confirm its screenshots are gone.

The visual check must see: the token-once dialog with an unmistakable warning and a working copy
control, grant rows showing permission and approval flags without truncation, invocation status
badges readable without colour, the step timeline's screenshots aligned with their step numbers, the
handoff banner impossible to miss, the docs page anchors working for every tool, no raw i18n keys,
and a mobile pass (390×844) over clients, the grant sheet, a session timeline and the settings.

### Slices

1. **MCP transport, clients and grants** — `mcp_clients`, `mcp_client_tools`, token issuance,
   hashing and rotation, JSON-RPC `initialize`/`ping`/`tools/list`, the clients screen with the
   sandbox test panel, permission keys.
   *Done when:* a token lists exactly its granted, installed tools and a revocation takes effect on
   the next call.
2. **Tool registry, permissions and invocations** — tool catalogue generated from the endpoint
   registry with schemas, permission and approval resolution, `tools/call` with audit and log rows,
   sandbox mode, `mcp_invocations` and its list.
   *Done when:* a granted read tool returns real data, a denied one names its permission with no
   side effect, and sandbox mode proves the plan without writing.
3. **Workflow-builder tools and documentation** — schema/validate/draft/save-draft tools, the
   generated `/docs/mcp` reference, the connect guide and the build check that every tool has a
   schema.
   *Done when:* an external agent drafts a workflow that opens in the builder as a draft, and the
   docs page matches the served catalogue.
4. **Computer-use sessions** — `computer_use_sessions`, `computer_use_steps`,
   `computer_use_origins`, the step executor with refusals and handoff, the session screens,
   screenshot retention, stop control, mobile layouts.
   *Done when:* a session walks a real page inside its allow-list, refuses what it must, and hands
   over to a human when asked.

### Risks / notes

- An MCP client is a machine user with a token: grants must stay narrow, the token must be shown
  once, rotation and revocation must be one click, and every call must be attributable — this is the
  highest-value target in the platform for lateral movement.
- The sandbox flag defaulting to on is deliberate friction: a client proves its calls before it can
  write, and the switch to live should be a conscious, audited action.
- Computer use is the riskiest tool in this request: origin allow-listing, refusal of credential and
  payment fields, step budgets, rate limits and a visible stop control are the minimum, and any of
  them silently failing to work is a release blocker rather than a bug.
  refused with `blocked_airgap`, so an air-gapped installation keeps an explicitly local allow-list.
