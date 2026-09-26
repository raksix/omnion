# REQ-104 — AI Cost Manager & AI Logs

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Knowing what AI costs and where it went.

- Token and cost accounting per request, model, feature, organization, site and user.
- Organization budget limit with warning thresholds and hard stop; per-site allocation.
- AI logs screen: request, model, latency, tokens in/out, cost, status, retention settings.
- Export for accounting; dashboards for spend trend.
- Per-provider data policy toggles (what may leave the instance).

## Implementation spec

### Scope (in / out)

**In** — the money and the paper trail for everything REQ-001, REQ-102, REQ-103, REQ-106, REQ-107
and REQ-108 send to a model.

- **Accounting ledger** — one immutable row per provider call in `ai_request_logs`, carrying the
  dimensions the reports group by: organization, site, user, provider, model, feature
  (`chat`, `task:summarize`, `copilot:seo`, `agent:content`, `knowledge:embed`, `eval:judge`,
  `computer_use`), copilot key, agent id, conversation id and run id.
- **Cost model** — cost is tokens × the price stored on the model at call time. The price used is
  snapshotted onto the log row, so a later price edit does not rewrite history. A model without a
  price records tokens at `cost_micros = 0` with `price_source = 'none'` and is flagged on the cost
  screen and in the nightly reconciliation note.
- **Budgets** — per organization: monthly limit, warn percent, action (`warn` or `block`); per site:
  an optional allocation under the organization limit, with over-allocation refused. A call is
  checked before it starts against the current period's spend; a blocking budget answers
  `402 ai_budget_exceeded` naming the limit and the spend, and the attempt is logged with
  `status = 'blocked_budget'`. A warn budget lets the call through and emits the threshold event
  once per threshold per period.
- **Streaming accounting** — token usage is written on stream completion **and** on stream failure
  or client disconnect; a cancelled stream records the partial usage a provider reported, or an
  estimate flagged `estimated = true`, so budgets cannot be silently undercounted.
- **Retention** — prompt and response previews are opt-in per organization and bounded (≤500
  characters, masked by REQ-105 before they are stored); log rows past the retention window are
  purged by the runner with a counted run recorded on an admin-visible job row.
- **Per-provider data policy** — which categories may leave the instance (user text, media,
  analytics, private content) plus the PII masking switch, enforced at the call site with a stable
  refusal code and an audit row naming the policy; every change is audited with before/after.

**Out**

- Any billing or invoicing: figures are an estimate from token counts and operator-entered prices.
- Provider-side billing reconciliation and invoice import (a later request can read the same
  ledger).
- Detection rules and masking mechanics (REQ-105), local model resource accounting beyond
  zero-cost rows (REQ-106), eval scoring (REQ-107).
- Log shipping to an external observability product; the export file and the webhook events are the
  integration surface.

### Screens (UI)

- **`/ai/costs`** — four stat cards (Spend this month, Requests, Tokens in/out, Projected month end),
  a spend trend chart over the chosen range with a budget line and warning mark, a breakdown table
  driven by a group-by control (Provider / Model / Feature / Copilot / Agent / Organization / Site /
  User) with Tokens in, Tokens out, Requests, Cost, Share bar and a totals row. Range picker
  (7/30/90 days, this month, custom) and Export. Rows link through to `/ai/logs` pre-filtered.
  Budgets panel: organization limit, warn percent, action, current spend, per-site allocation list
  with a sum check. Empty state "No usage in this range" with a link to `/ai/logs`; skeleton cards
  while loading; error banner with Retry and a "Last updated" stamp.
- **`/ai/logs`** — table: Time, Status, Feature, Provider/Model, User, Site, Latency, Tokens in,
  Tokens out, Cost, Error. Filters: range, status, feature, provider, model, organization, site,
  user, free-text request id or goal; saved filter sets per user. Detail drawer: request id,
  effective price snapshot, latency and first-token time, token counts, cost, status and error
  code, tool call count, the masked prompt preview (when retention allows), citation ids, and links
  to the run, conversation or eval run that produced it. Row actions Copy request id, Open related,
  and (with the retention permission) Delete this row. Retention settings live behind a "Retention"
  button: preview days (0–365), log days (7–730), prompt preview on/off, with a warning that
  turning previews on stores customer text.
- **`/ai/providers/[id]/policy`** — the four data-policy toggles, the masking switch, a "what this
  means" table naming which features are affected, the last change with actor and time, and a
  "Policy changes" list. Non-owners see the values read-only with the reason.
- **Keyboard** — `/` focuses search, `F` opens the filter sheet, `G` then `C` goes to costs, `G`
  then `L` to logs, `E` opens the export dialog, `R` refreshes, `Esc` closes the drawer, `↑/↓` +
  `Enter` move through rows.
- **Mobile (<1024px)** — stat cards stack two-up, the trend chart becomes a compact sparkline with a
  tap-through, the breakdown table becomes label/value cards, the log table becomes cards with the
  detail drawer as a bottom sheet, and the filter sheet scrolls as one column.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/usage` | Roll-ups (`from`, `to`, `group_by`, org/site filters) | `ai.usage.read` |
| GET | `/api/v1/ai/logs` | Paged request log with filters | `ai.logs.read` |
| GET | `/api/v1/ai/logs/{id}` | One log row with the masked preview | `ai.logs.read` |
| DELETE | `/api/v1/ai/logs/{id}` | Remove one row (retention allow-list) | `ai.logs.manage` |
| GET/PUT | `/api/v1/ai/logs/settings` | Read / change retention and preview settings | `ai.logs.read` / `ai.settings.manage` |
| POST | `/api/v1/ai/usage/export` | Queue or stream a CSV/JSON export | `ai.usage.export` |
| GET | `/api/v1/ai/budgets` | Organization limit, warn percent, action, per-site allocations | `ai.usage.read` |
| PUT | `/api/v1/ai/budgets` | Replace the budget (validates allocations ≤ limit) | `ai.budgets.manage` |
| GET/PUT | `/api/v1/ai/data-policy` | Per-provider category toggles + masking switch | `ai.providers.read` / `ai.settings.manage` |

New catalogue keys: `ai.logs.read`, `ai.logs.manage`, `ai.usage.export`, `ai.budgets.manage`.
`ai.usage.read` and `ai.settings.manage` already exist (REQ-001). A blocking budget refusal answers
`402` with `code = "ai_budget_exceeded"`; a policy refusal answers `403` with
`code = "ai_data_policy_denied"` and the policy name in the message.

### Data model

Migration `database/migrations/00NN_ai_cost.sql` (00NN = next free integer at land time; 0018 was
free when this was written). It extends REQ-001's `ai_usage` (adding `feature`, `copilot_key`,
`conversation_id`, `latency_ms`, `status`, `error_code`, `price_source`, `estimated`) and adds the
tables below. `ai_budgets` (REQ-001) gains `period` text default `'month'` and `hard_stop` bool
derived from `action`.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_request_logs` | id bigserial pk, request_id uuid, organization_id, site_id uuid null, user_id uuid null → users set null, provider_id uuid null → ai_providers set null, model_id uuid null → ai_models set null, feature text, copilot_key text null, agent_id uuid null, conversation_id uuid null, run_id uuid null, kind text ('chat','task','embed','rerank','run','eval','vision'), status text ('ok','error','refused','blocked_budget','blocked_policy','blocked_airgap','cancelled'), http_status int, error_code text null, latency_ms int, first_token_ms int null, prompt_tokens int default 0, completion_tokens int default 0, cost_micros bigint default 0, price_source text ('registry','manual','none'), estimated bool default false, prompt_sha256 text null, prompt_preview text null, response_preview text null, tool_calls int default 0, cited_chunks int default 0, created_at timestamptz not null | unique `(request_id)`; `(organization_id, created_at desc)`; `(organization_id, feature, created_at desc)`; `(provider_id, created_at desc)`; `(status, created_at)` where status <> 'ok'; `(user_id, created_at desc)`; `(created_at)` for purge |
| `ai_spend_daily` | day date, organization_id uuid, site_id uuid (`'00000000-…'` for none), provider_id uuid, model_id uuid, feature text, requests int, prompt_tokens bigint, completion_tokens bigint, cost_micros bigint, refreshed_at | pk `(day, organization_id, site_id, provider_id, model_id, feature)`; `(organization_id, day desc)` |
| `ai_budget_events` | id bigserial pk, budget_id uuid null → ai_budgets cascade, organization_id, site_id uuid null, period text, period_start date, threshold_percent int, action text ('warn','block'), spent_micros bigint, limit_micros bigint, created_at | unique `(organization_id, coalesce(site_id, …), period, period_start, threshold_percent)`; `(organization_id, created_at desc)` |
| `ai_log_settings` | organization_id uuid pk → organizations cascade, preview_storage_enabled bool default false, preview_days int default 30 check (0–365), log_retention_days int default 180 check (7–730), export_ttl_days int default 7, updated_by uuid null → users set null, updated_at | pk only |
| `ai_policy_history` | id bigserial pk, provider_id uuid → ai_providers cascade, organization_id, field text, old_value text null, new_value text null, changed_by uuid null → users set null, changed_at | `(provider_id, changed_at desc)`; `(organization_id, changed_at desc)` |
| `ai_purge_runs` | id bigserial pk, organization_id uuid null, kind text ('logs','previews','exports'), deleted_rows int, started_at, finished_at, status text, error text | `(started_at desc)` |

Cost micros are stored as integers (never floats) and rendered as currency in the panel's locale;
prices are entered per million tokens and converted once, at write time. The purge runner deletes in
batches of 5000 inside one transaction per batch so a long purge never holds a lock on the hot
table.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.budget.threshold_reached` | emitted | percent, action, spent, limit (organization or site) — drives REQ-021 notifications |
| `ai.budget.exceeded` | emitted | the refusal: feature, requested tokens, spend to date |
| `ai.data_policy.denied` | emitted | provider, category, feature — the audit answer for "why did this fail" |
| `ai.logs.exported` | emitted | range, filters hash, row count, actor |
| `ai.logs.purged` | emitted | kind, deleted rows, window — the retention proof |

### Acceptance criteria

- [ ] Every completed chat, task, embedding, eval-judge and agent-step call writes exactly one `ai_request_logs` row with a unique `request_id`, tokens and cost.
- [ ] Editing a model's price does not change the cost of existing log rows (price snapshot asserted in a test that edits the model and re-reads the row).
- [ ] A model with no price records `cost_micros = 0`, `price_source = 'none'` and appears in the "no price configured" list on `/ai/costs`.
- [ ] `/ai/costs` totals equal `select sum(cost_micros) from ai_request_logs` for the same range and filters (asserted against SQL in a test).
- [ ] A budget with `action='block'` refuses the next call with `402 ai_budget_exceeded` naming the limit and the spend, logs the attempt with `status='blocked_budget'`, and emits `ai.budget.exceeded`.
- [ ] A budget with `action='warn'` lets the call through, emits `ai.budget.threshold_reached` exactly once for that threshold in that period even when twenty calls cross it.
- [ ] Per-site allocations that sum above the organization limit are refused with a field error naming the offending sites.
- [ ] `/ai/logs` filters by range, status, feature, provider, model, site and user cumulatively, and a saved filter set survives a reload for the same user.
- [ ] Retention: setting log days to 30 and running the purge removes only rows older than 30 days, records an `ai_purge_runs` row and emits `ai.logs.purged`.
- [ ] A caller without `ai.usage.export` sees Export disabled naming the permission; the API answers 403 for the same request.
- [ ] Flipping a provider's "private content" toggle to off makes a task carrying a draft revision fail with `ai_data_policy_denied` naming the policy, and the change appears in the policy history with its actor.
- [ ] Organization A cannot read organization B's logs, budgets or policy history (404 on a direct id).
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/ai/costs`, switch range and group-by, export CSV and open it;
open `/ai/logs`, apply three filters at once, save the filter set, reload, open a row's drawer and
follow a link to the producing run; set retention to 30 days with previews on, run a chat to prove a
preview appears, then purge and confirm the old rows disappear (and the new one stays); raise the
organization budget above the warn threshold with a small limit to trigger the warn path and read
the notification; set it to `block` and make a call that is refused with the named limit; set a
per-site allocation, exhaust it and confirm the other site still answers; open
`/ai/providers/[id]/policy`, flip private content off, try the affected feature, read the refusal
message, and check the policy history entry.

The visual check must see: currency columns right-aligned and never clipped, the share bar summing
to 100% in the table, readable status badges including the blocked states, the trend chart scaled to
the range with a visible budget line, no raw i18n keys, and a mobile pass (390×844) over costs,
logs and the retention panel.

### Slices

1. **Ledger and cost screen** — `ai_request_logs`, `ai_usage` extension, cost computation from the
   price snapshot, `/ai/usage` and `/ai/usage/trend`, `/ai/costs` with stat cards, breakdown,
   group-by and export.
   *Done when:* a chat and an embedding each land one row, the screen total matches SQL, and the
   CSV opens.
2. **Budgets, refusals and alerts** — `ai_budgets` extensions, the pre-call check, blocking and
   warning paths, `ai_budget_events`, per-site allocations, notifications.
   *Done when:* a blocked call is refused and logged, a warn fires once per threshold, and an
   exhausted site is isolated.
3. **Logs screen, retention and policy** — `/ai/logs` with filters and the detail drawer, saved
   filter sets, retention settings, preview masking hook, purge runner, `ai_log_settings`,
   `ai_purge_runs`.
   *Done when:* filtering is cumulative, a purge deletes exactly the intended window, and a row
   without a preview states why.
4. **Data policy and polish** — per-provider policy toggles, enforcement at the call site, policy
   history, anomaly event, empty/loading/error states, mobile layouts.
   *Done when:* a denied feature names its policy, the history shows every change, and both widths
   pass the visual check.

### Risks / notes

- Money must never be invented: an unpriced model or a missing usage frame produces a visibly
  flagged estimate, never a plausible-looking number that accounting would trust.
- The pre-call budget check reads the current period; a burst of parallel calls can overshoot the
  limit slightly. Document the tolerance (one call's worth per concurrency slot) instead of
  pretending the stop is exact.
- Prompt previews are customer text; they are off by default, bounded in length, masked by REQ-105
  at write time, and covered by the purge and export paths.
