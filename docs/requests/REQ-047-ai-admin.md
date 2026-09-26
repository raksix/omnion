# REQ-047 — AI Admin *(headline)*

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** AI Hub × audit/logs
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

> "What changed in the system in the last 24 hours?"

or:

> "Why didn't this workflow run?"

AI investigates across audit + logs + workflow history + database metadata.

## Notes

- Investigative agent over REQ-039 (Advanced Audit), execution history and docs/09-style
  lifecycle events; inherits read-only scopes by default (docs/07-IAM.md §14).

## Implementation spec

### Scope (in / out)

**In**

- An investigative agent that answers operational questions from the platform's own record: the
  audit trail (REQ-039), the event bus (`events`), workflow run history (`workflow_executions` /
  `workflow_steps`) and read-only schema metadata (table and column names, never row data).
- A **closed, read-only tool registry** — `audit.search`, `events.search`, `executions.search`,
  `execution.get`, `schema.catalog`. No SQL tool, no write tool, no HTTP tool; every call is
  recorded as an investigation step.
- Answer integrity: the answer must cite identifiers the tools actually returned. A citation no
  step produced is a validation failure, not a footnote.
- The AI usage ledger (`ai_invocations`) with cost roll-ups, per-organization monthly limits and
  one admin route with four tabs (Investigate, Usage, Limits, Ledger) — docs/06 §16–§17 on screen.
- New permission keys `ai.usage.read` and `ai.usage.manage` (category `ai`) in the catalogue
  (`crates/permissions/src/catalogue.rs` + role seed).

**Out**

- Remediation: the console never mutates the platform; an answer links to the screen that owns the
  fix, and applying it there is a separate audited action.
- Provider/model management and the chat playground (P11, `/ai`); agent authoring and multi-agent
  orchestration (REQ-001); module copilots (REQ-042).
- Raw prompt/response inspection — payload bodies stay out of v1 (docs/06 §18); SIEM export and
  retention policy belong to REQ-038.

### Screens (UI)

Admin panel, `RequireAuth` + `AppShell`, one route with URL-driven tabs
(`/ai/admin?tab=investigate|usage|limits|ledger`) so every tab is linkable and testable.

- **Investigate**: question box (2–1000 chars) with click-to-fill suggestions (“What changed in
  the system in the last 24 hours?”, “Why didn't this workflow run?”, “Which AI calls failed
  today and why?”), window selector (1 h / 24 h / 7 d / custom), optional site scope, Run and
  Cancel. While running, a live trace column lists each tool call with arguments, row count and
  duration, and the result panel shows the markdown answer followed by citation chips (`audit …`,
  `event …`, `execution …`) that deep-link to the owning screen. History rail on the left: status
  badges, `j`/`k` to move, `Enter` to open.
- **Usage**: date range + group-by (provider / model / agent / site / user) → table with calls,
  tokens in/out, cost, error rate; a totals strip per provider (docs/06 §16 shape); the CSV button
  routes to the import/export centre (REQ-031) rather than a bespoke exporter.
- **Limits**: usage vs limit with a progress bar; a form for monthly limit (USD > 0), alert
  threshold (1–100 %, default 80) and a hard-stop toggle that warns it blocks all AI calls.
- **Ledger**: `ai_invocations` rows — Time, Actor, Kind, Provider/Model, Tokens in/out, Cost,
  Latency, Result — filtered by actor, kind, result, model and date range, with a detail drawer
  holding the AI Audit fields of docs/06 §17. No prompt or response body is shown anywhere.
- Empty states per tab, skeletons while loading, failed investigations show the error plus Retry;
  numbers are right-aligned tabular figures, and mobile stacks the trace above the answer.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/ai/investigations` | Start an investigation; streams trace + answer (`text/event-stream`) | `ai.chat` |
| GET | `/api/v1/ai/investigations` | History (`status`, `limit`, `offset`) | `audit.read` |
| GET | `/api/v1/ai/investigations/{id}` | One investigation with steps, citations and usage | `audit.read` |
| POST | `/api/v1/ai/investigations/{id}/cancel` | Cancel a running investigation | `ai.chat` |
| GET | `/api/v1/ai/usage` | Roll-up (`from`, `to`, `group_by`) | `ai.usage.read` |
| GET | `/api/v1/ai/usage/invocations` | Paged ledger rows with filters | `ai.usage.read` |
| GET | `/api/v1/ai/usage/limits` | Current period usage vs limit | `ai.usage.read` |
| PUT | `/api/v1/ai/usage/limits` | Set limit, alert threshold and hard stop | `ai.usage.manage` |

Every route is limited to the caller's organization plus the site selection, like the rest of the
panel. Starting an investigation spends AI (one key); reading its record is an audit read (another
key), because an investigation is audit data and must not be readable by a key that cannot read the
audit trail.

### Data model

Migration `database/migrations/00NN_ai_admin.sql` (number = next free at land time).

- `alter table ai_models` — add `price_input_per_mtok numeric(12,6)`, `price_output_per_mtok
  numeric(12,6)` (nullable). Rates are metadata; a ledger row keeps the cost it was computed with,
  so editing a rate never rewrites history.
- `ai_invocations` — the shared AI usage ledger, **created here**; writers are the `/ai/chat` path,
  the investigation runner and agent tool runs (REQ-042/045/046 read it, never redefine it):
  `id uuid pk`; `organization_id uuid not null fk` / `site_id uuid fk set null`;
  `user_id uuid fk users (id) on delete set null`; `agent_key text`; `kind text not null` check in
  (`chat`,`agent`,`investigation`,`embedding`,`image`,`audio`,`transcription`); `provider_id uuid
  fk ai_providers (id) on delete set null` / `model_key text not null`; `request_id text`;
  `tokens_input`, `tokens_output`, `latency_ms integer not null default 0`; `cost_usd
  numeric(12,6) not null default 0`; `status text not null` check in (`ok`,`error`,`cancelled`)
  with `error_code text`; `created_at timestamptz not null default now()`. Indexes:
  `(organization_id, created_at desc)`, `(provider_id, created_at desc)`, `(model_key, created_at
  desc)`, and `(created_at desc) where status = 'error'`.
- `ai_investigations` — `id uuid pk`, `organization_id uuid not null fk`, `question text not null`
  (1–1000), `window_from`/`window_to timestamptz`, `status text` check in
  (`running`,`completed`,`failed`,`cancelled`), `answer_md text`, `citations jsonb not null
  default '[]'`, `model_key text`, `tokens_input`/`tokens_output integer`, `cost_usd
  numeric(12,6)`, `invocation_id uuid fk ai_invocations (id) on delete set null`, `error text`,
  `created_by uuid fk users (id) on delete set null`, `created_at`, `finished_at` (index
  `(organization_id, created_at desc)`).
- `ai_investigation_steps` — `id uuid pk`, `investigation_id uuid not null fk on delete cascade`,
  `step_no integer not null > 0`, `tool text not null`, `args jsonb not null default '{}'`,
  `rows_returned integer`, `duration_ms integer`, `status text` check in (`ok`,`error`),
  `error text`, `created_at`; unique `(investigation_id, step_no)`.
- `ai_usage_limits` — `organization_id uuid pk fk organizations (id) on delete cascade`,
  `monthly_limit_usd numeric(12,2) not null check > 0`, `alert_at_percent integer not null
  default 80 check between 1 and 100`, `hard_stop boolean not null default false`,
  `period_start date not null`, `notified_at timestamptz`.

### Events

| Event | When | Payload |
|---|---|---|
| `ai.investigation.started` | the run begins | `investigation_id`, `question`, `window_from`, `window_to` |
| `ai.investigation.completed` | answer validated | `investigation_id`, `citation_count`, `cost_usd` |
| `ai.investigation.failed` | tools or validation failed | `investigation_id`, `reason` |
| `ai.usage.limit_reached` | the period crosses `alert_at_percent` | `organization_id`, `period_start`, `used_usd`, `limit_usd` |

The first three are organization-scoped and fan out to subscribed webhook endpoints (P12) —
“investigation finished” can land in chat. `ai.usage.limit_reached` is the alerting hook and fires
at most once per period (`notified_at` guards it). Nothing is consumed by subscription: the agent
*reads* the `events` table through `events.search`, which keeps replay and ordering out of scope.

### Acceptance criteria

- [ ] “What changed in the system in the last 24 hours?” after a `page.published` event cites that
      event (id and timestamp), and the citation chip opens the right screen.
- [ ] A citation no step produced fails validation and the run ends `failed` with a readable error
      (hallucination guard, unit-tested against a stubbed answer).
- [ ] The tool registry is closed and read-only: an integration test asserts a full investigation
      leaves `events`, `workflow_executions` and the audit trail row counts unchanged.
- [ ] A caller without the deployment/plugin read keys gets “evidence out of scope” instead of
      leaked rows (permission-scoped tools, docs/07 §14).
- [ ] “Why didn't this workflow run?” names the real cause for a disabled workflow, one with no
      schedule, and one whose step failed — each asserted in tests.
- [ ] Ledger rows are written for chat, agent and investigation calls with tokens, latency, cost
      and status; a provider error lands `status = 'error'` with `error_code`, and editing a
      model's rates later leaves existing `cost_usd` values unchanged (test).
- [ ] Usage roll-ups group correctly by provider, model, site and user; a bad `group_by` answers
      `400`, and crossing the threshold emits `ai.usage.limit_reached` exactly once per period.
- [ ] With `hard_stop = true` a new AI call past the limit is refused with `429` and code
      `ai_budget_exhausted`; the run cap (≤ 12 tool steps, token budget) ends a runaway
      investigation `completed` with a note.
- [ ] `PUT /ai/usage/limits` validates ranges and requires `ai.usage.manage` (`ai.usage.read` gets
      `403` on the write).
- [ ] Every tab is linkable via `?tab=`, filters survive reload, and all four have empty states;
      no prompt or response body appears in any UI or API response (response-shape tests).
- [ ] All four tabs render in light and dark mode with no clipped numbers at 1280 px and as cards
      at 390 px; Lucide icons only.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build`, `bash scripts/qa/run.sh` green;
      `/ai/admin` is in the walkthrough inventory.

### QA plan

- The walkthrough extends its existing flow: publish a page (already a step), then open
  `/ai/admin?tab=investigate`, click the 24-hour suggestion, and screenshot the trace and answer;
  every tab, filter and the ledger drawer get pressed, recorded in `clicks.jsonl`.
- The visual review must see right-aligned numeric columns, a trace drawer that does not overflow,
  legible citation chips and a readable limits progress bar, in both themes.
- `scripts/qa/probe-ai-admin.cjs` (new, exits non-zero) seeds one event through the API, runs an
  investigation and asserts the answer cites it and that `ai_invocations` gained a non-zero-cost
  row; it also sets a limit below current usage and asserts the alert event exists once, while a
  session without `ai.usage.read` sees permission-denied on Usage and Ledger, not an empty table.

### Slices

1. **Ledger + rates + usage API** — migration, writers on the `/ai/chat` path, `GET /ai/usage` and
   `/ai/usage/invocations`, `ai.usage.*` catalogue keys. *Done when:* `cargo test --workspace` is
   green and a chat call produces a ledger row with tokens and cost.
2. **Investigation engine** — read-only tool registry, agent loop with caps, citation validation,
   start/get/cancel routes, events. *Done when:* an integration test answers the 24-hour question
   with a citation for a seeded event.
3. **Console UI** — `/ai/admin` shell, four tabs, trace + history, usage and ledger screens with
   the detail drawer, permission-denied states. *Done when:* the walkthrough passes the new screens
   with zero high findings.
4. **Limits, alerts, probe, docs** — limits API, threshold event, hard stop, the QA probe and user
   docs. *Done when:* the probe and full QA pass are green and the limit event appears on the feed.

### Risks / notes

- Read-only is a promise made in code: the tool registry is the only path from the model to data
  and has no write or SQL member — a new tool means a new permission and a review.
- The `events` table grows without bound in v1; heavy installs need the retention policy REQ-038
  will own — until then investigations see what the bus kept.
- Answers are audit data: they inherit `audit.read` and its scoping instead of inventing a parallel
  view of who did what; prompt/response bodies stay out of the ledger by design.
