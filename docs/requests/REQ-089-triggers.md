# REQ-089 — Triggers (webhook, schedule, polling)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows` + `crates/events`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

How workflows start.

- Webhook triggers: live and test URLs, response modes (immediate, wait-for-workflow, custom), path/method/auth config.
- Schedule trigger: cron and interval expressions with timezone and a next-runs preview.
- Polling triggers with cursor state, deduplication of seen items and backoff on failure.
- App-event triggers fed by the internal event bus (REQ-016) and the automation engine (REQ-003).
- Trigger activation registry: which triggers are armed, last fired, error state.

## Implementation spec

### Scope (in / out)

**In**

- **Registry and arming.** Every trigger is a row with kind (`webhook`, `schedule`, `poll`, `event`, `manual`), a config document, an enabled flag and runtime state (`armed`, `disarmed`, `degraded`, `error`). Arming is explicit and idempotent — saving a graph with a trigger never arms it — and it validates first: the workflow compiles, the referenced credential exists and is healthy, and webhook paths are free. Arm and disarm are audited and emit events.
- **Webhook triggers.** A production path in the public hook namespace plus a test URL valid only inside a test session (docs/09-N8N-TEARDOWN.md §13, lesson 10). Config: path slug with methods as a multi-select, auth (`none`, shared-secret header, basic, HMAC with header, algorithm and replay window), optional IP allow-list, CORS for browser callers, response mode (`immediate`, `after_workflow` with the mapped result, `custom` status and body), an `after_workflow` timeout after which the caller gets a pending response and a status URL, payload size cap, raw-body capture for signature verification, and a per-trigger rate limit. Requests are sanitised first: hop-by-hop headers dropped, body parsed per content type, multipart and binary bodies spilled to storage.
- **Test sessions.** One test URL per session with a live call feed — method, masked headers, body and timing per call — plus **Pin as sample data** (REQ-092) and **Run from trigger**. Sessions never create production runs, expire on a timer, and leave no routable URL behind.
- **Schedule triggers.** Cron (five fields, minute granularity) and interval expressions (`every 15 minutes`, `every 3 days at 09:00`) with a per-trigger timezone defaulting to the organization's, a ten-entry next-runs preview, DST-safe computation, and a catch-up policy (`skip` missed occurrences — the default — or `run once` for the last missed one). The existing `workflows.schedule` and `next_run_at` columns stay the scheduling contract; the trigger row points at them.
- **Polling triggers.** Interval with a minimum, a poll operation from an integration node's `poll` capability, a cursor advanced only after a successful poll, deduplication by hashing a stable item key from an expression, exponential backoff with jitter on consecutive failures (capped) that parks the trigger, a degraded state at the threshold, a disabled state at the hard cap, a max-lookback policy for polls after downtime, and recorded timing and item counts per cycle.
- **App-event triggers.** Subscription to dotted events from the internal bus (REQ-016) and the automation engine (REQ-003): a name pattern (exact or prefix such as `content.page.*`), an optional filter condition evaluated through REQ-092 against the payload, and a scope (organization or site). Delivery is at-least-once with dedup on the event id so a redelivery starts no second run; armed workflows own their subscriptions (arming subscribes, disarming unsubscribes), a filter that throws records `trigger.error` instead of starting a run, and bounded audited replay starts runs marked as replays.
- **Activation registry.** One screen and API over every trigger: kind, workflow, state, last fired, fires in 24 h, consecutive failures, error text, next scheduled fire or poll, event backlog, and arm/disarm/fire-now/poll-now actions. Per-trigger counters (received, started, rejected, failed) feed the observability stack (REQ-126), and every rejection carries a reason code.

**Out**

- Worker pools, per-workflow concurrency and multi-instance deduplication of scheduled fires (REQ-096) — the registry records state, the queue decides who fires, leader election is REQ-096's.
- Cancellation, in-run retries and the wait sweeper (REQ-091, REQ-090).
- Public API keys, external rate-limit policy and API product surface (REQ-130).
- Connector implementations behind pollers (REQ-015, REQ-048) and expression semantics (REQ-092).

### Screens (UI)

| Route | Screen |
|---|---|
| `/triggers` | Activation registry across workflows (default landing for trigger operations) |
| `/triggers/webhooks` | Webhook table: path, methods, workflow, mode, auth, last call, 24 h count, errors |
| `/triggers/polling` | Poller state: cursor age, last poll, failures, next attempt, backlog |
| `/triggers/test` | Test-webhook sessions: start/stop, copy test URL, live call log, pin a call |
| `/workflows/<id>/triggers` | Per-workflow trigger panel: list, arm/disarm, configuration forms |

- **Registry table.** Columns: Trigger (kind chip + label), Workflow, State, Last fired, Fires 24 h (sparkline), Failures, Next fire or poll, Detail; filters for kind, state, workflow and text, bulk arm and disarm, and an error row that expands with the last error and a link to the failed run.
- **Webhook form.** Routing (path slug with full production and test URL previews and copy, methods, enabled), Authentication (mode; a generated secret is shown once and stored in the secret store; HMAC adds header, algorithm and replay window; IP allow-list), Response (mode, custom status and body expression, `after_workflow` timeout with the pending fallback explained), Limits (payload cap, rate limit), and **Send a test call**; a path collision blocks the save and names the holding workflow.
- **Test sessions and event subscriptions.** "Listen for test event" starts a session showing the test URL with a countdown and a live call table (time, method, source, size, status) with an expandable body and masked headers, each call offering **Pin as sample data** and **Run from trigger**; the event panel adds a pattern input with a domain-grouped picker, scope selector, filter builder, a "matching events in the last hour" preview and per-event replay.
- **Schedule and polling panels.** Schedule: cron or interval input with a human-readable summary, timezone selector, a ten-entry next-runs list in trigger and local time, catch-up radio with consequences spelled out, and a warning when the expression fires more often than the minimum interval. Polling: interval, poll node picker, read-only cursor summary, dedup key expression with the match count from the last poll, backoff settings, max lookback, failure thresholds, and **Poll now** with a dry-run item count.
- **States and mobile.** Empty states name the next action ("add a trigger node to a workflow"); loading uses table skeletons; URLs and secrets are copyable on mobile; the registry is read-only at ≤ 900 px with arm/disarm and detail still reachable, while configuration forms need a wider screen.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/triggers` · `/{id}` | Activation registry (`?kind=&state=&workflow=&search=`) · one trigger's runtime state and counters | `workflows.read` |
| POST | `/api/v1/triggers/{id}/arm` · `/disarm` | Arm or disarm (validates path and credential first) | `workflows.triggers.manage` |
| POST | `/api/v1/triggers/{id}/fire` | Fire a manual or schedule trigger now | `workflows.run` |
| POST | `/api/v1/triggers/{id}/poll-now` | Run one poll cycle now (`dry_run=true` supported) | `workflows.run` |
| GET · PUT | `/api/v1/workflows/{id}/triggers` | List · replace the trigger set for a workflow | `workflows.read` · `workflows.triggers.manage` |
| POST | `/api/v1/workflows/{id}/schedule/preview` | Next ten occurrences for a cron/interval and timezone | `workflows.read` |
| POST · DELETE | `/api/v1/workflows/{id}/webhooks/test-session` | Start (returns test URL and expiry) · end a session | `workflows.run` |
| GET | `/api/v1/workflows/{id}/webhooks/test-session/calls` | Calls for the session | `workflows.run` |
| POST | `/api/v1/public/hooks/{path}` | Production webhook ingress (auth per trigger) | — |
| POST | `/api/v1/public/hooks/{path}/test/{token}` | Test webhook ingress for a session | — |
| POST | `/api/v1/events/{id}/replay` | Replay one event into armed subscriptions | `workflows.run` |

Codes: `hook_unknown_path`, `hook_method_not_allowed`, `hook_auth_missing`, `hook_auth_invalid`, `hook_signature_invalid`, `hook_signature_expired`, `hook_ip_denied`, `hook_payload_too_large`, `hook_rate_limited`, `hook_workflow_not_armed`, `hook_pending_timeout`, `trigger_credential_unhealthy`, `trigger_path_conflict`, `poll_backoff_active`, `event_filter_failed`, `event_duplicate`, `schedule_expression_invalid`.

### Data model

Migrations `0034_workflow_webhooks.sql`, `0035_workflow_trigger_state.sql` (reserved band 0030–0039 for the workflow editor family, REQ-086–096; append-only ledger — take the next free number if taken).

```sql
-- 0034: public ingress configuration plus an auditable call log
create table workflow_webhooks (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    workflow_id uuid not null references workflows (id) on delete cascade,
    trigger_id uuid, path text not null, methods text[] not null default '{POST}',
    enabled boolean not null default false,          -- true only while armed
    auth text not null default 'none', secret_id uuid,
    hmac_header text, hmac_algorithm text, replay_window_seconds integer,
    ip_allowlist inet[], cors_origin text, response_mode text not null default 'immediate',
    response_status integer not null default 200, response_body jsonb,
    response_timeout_seconds integer not null default 30,
    payload_limit_bytes integer not null default 1048576, rate_limit_per_minute integer,
    last_call_at timestamptz, call_count bigint not null default 0,
    created_at timestamptz not null default now(),
    constraint workflow_webhooks_auth_valid check (auth in ('none', 'header', 'basic', 'hmac')),
    constraint workflow_webhooks_response_valid check (response_mode in ('immediate', 'after_workflow', 'custom')),
    constraint workflow_webhooks_path_shape check (path ~ '^[a-z0-9][a-z0-9/_-]{1,120}$'),
    constraint workflow_webhooks_hmac_shape check (
        (auth = 'hmac') = (hmac_header is not null and hmac_algorithm is not null)));
create unique index workflow_webhooks_path_uid on workflow_webhooks (organization_id, path);

create table workflow_webhook_calls (
    id uuid primary key default gen_random_uuid(),
    webhook_id uuid not null references workflow_webhooks (id) on delete cascade,
    mode text not null default 'production', method text not null, status_code integer,
    outcome text not null, duration_ms integer, bytes_in integer, source_ip inet, error text,
    execution_id uuid references workflow_executions (id) on delete set null,
    created_at timestamptz not null default now(),
    constraint workflow_webhook_calls_outcome_valid
        check (outcome in ('accepted', 'rejected', 'pending', 'failed')));
create index workflow_webhook_calls_recent_idx on workflow_webhook_calls (webhook_id, created_at desc);
create index workflow_webhook_calls_cleanup_idx on workflow_webhook_calls (created_at);

-- 0035: one registry row per trigger, plus polling state and seen-item dedup
create table workflow_triggers (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    workflow_id uuid not null references workflows (id) on delete cascade,
    node_key text not null, kind text not null, config jsonb not null default '{}'::jsonb,
    enabled boolean not null default false, state text not null default 'disarmed',
    armed_at timestamptz, last_fired_at timestamptz, last_run_id uuid,
    consecutive_failures integer not null default 0, last_error text, last_error_at timestamptz,
    next_fire_at timestamptz,
    created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
    constraint workflow_triggers_kind_valid check (kind in ('webhook', 'schedule', 'poll', 'event', 'manual')),
    constraint workflow_triggers_state_valid check (state in ('armed', 'disarmed', 'degraded', 'error')));
create unique index workflow_triggers_slot_uid on workflow_triggers (workflow_id, node_key, kind);
create index workflow_triggers_state_idx on workflow_triggers (organization_id, state, kind);
create index workflow_triggers_due_idx on workflow_triggers (next_fire_at)
    where state = 'armed' and kind in ('schedule', 'poll');
create table workflow_trigger_cursors (
    trigger_id uuid primary key references workflow_triggers (id) on delete cascade,
    cursor jsonb not null default '{}'::jsonb, last_poll_at timestamptz, next_poll_at timestamptz,
    last_poll_items integer, last_poll_duration_ms integer, updated_at timestamptz not null default now());
create index workflow_trigger_cursors_due_idx on workflow_trigger_cursors (next_poll_at);
create table workflow_seen_items (            -- hashes only, never raw payloads
    trigger_id uuid not null references workflow_triggers (id) on delete cascade,
    value_hash text not null, first_seen_at timestamptz not null default now(),
    primary key (trigger_id, value_hash));
create index workflow_seen_items_expiry_idx on workflow_seen_items (first_seen_at);
-- widen the existing trigger domains (additive; released values keep working)
alter table workflows drop constraint workflows_trigger_kind_valid;
alter table workflows add constraint workflows_trigger_kind_valid
    check (trigger_kind in ('manual', 'schedule', 'webhook', 'poll', 'event'));
alter table workflow_executions drop constraint workflow_executions_trigger_valid;
alter table workflow_executions add constraint workflow_executions_trigger_valid
    check (trigger_kind in ('manual', 'schedule', 'webhook', 'poll', 'event'));
```

Runs record the trigger id, the source, and — for events — the event id, which is the dedup key that
makes at-least-once delivery safe.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `workflows.trigger.armed` · `.disarmed` · `.fired` | Arming changed · a run started from the trigger | `trigger_id`, `kind`, `source`, `event_id` |
| `workflows.trigger.failed` · `.degraded` | Validation or fire failed · failure threshold crossed | `trigger_id`, `code`, `failures`, `backoff_seconds` |
| `workflows.webhook.received` · `.rejected` · `.pending_timeout` | Ingress outcomes | `webhook_id`, `path`, `outcome`, `code` |
| `workflows.poll.completed` · `.failed` | One poll cycle finished | `trigger_id`, `items`, `duration_ms`, `backoff_seconds` |
| `workflows.event.duplicate` · `.replayed` | Duplicate dropped · replay performed | `trigger_id`, `event_id`, `actor_user_id` |

Consumed: the internal bus for event triggers (`content.page.published`, `user.created`, `order.created`,
`packages.installed`, `workflows.credential.needs_reauth`, and every other dotted event),
`workflows.credential.*` (arming validation and degradation), `node_packages.removed` (a lost poll node
degrades its trigger rather than failing silently).

### Acceptance criteria

- [ ] Arming a webhook trigger makes the production URL accept the configured methods and start exactly one run per accepted call; a path collision is refused with `trigger_path_conflict` naming the holder.
- [ ] Arming a trigger whose credential is unhealthy fails with `trigger_credential_unhealthy` before anything goes live.
- [ ] Unknown path, disallowed method, missing or wrong secret, bad signature, expired signature, denied IP and oversized body each return their named code, log a call, and start no run.
- [ ] `immediate` answers without waiting; `after_workflow` returns the mapped result; `custom` returns the configured status and body; a timeout answers with a pending status and a status URL without killing the run.
- [ ] Test sessions accept calls on the test URL, stream them, allow pinning one as sample data, and expire leaving nothing routable.
- [ ] A cron schedule fires within the expected minute for three consecutive occurrences, the next-runs preview matches actual fire times, and a timezone shift across a DST boundary produces no double fire.
- [ ] Interval expressions parse, a below-minimum interval is refused with `schedule_expression_invalid`, and catch-up follows its policy (`skip` starts nothing, `run once` starts exactly one run for the last missed window).
- [ ] A poll trigger never starts two runs for the same provider item across two cycles, a mid-poll failure and a restart (fixture provider with stable item ids).
- [ ] The poll cursor advances only after items are processed (a failing poll leaves it unchanged with the error recorded), and consecutive failures produce jittered backoff, reach `degraded` with a visible next attempt, and stop the trigger at the hard cap.
- [ ] An armed event trigger starts a run for a matching event, ignores non-matching names, honours the filter, drops a redelivered event id as `workflows.event.duplicate`, and records `trigger.error` for a throwing filter without starting a run.
- [ ] Bounded event replay starts runs marked as replays and lists them in the registry; disarming stops starts immediately (webhook calls rejected, subscription silent) and removes the trigger from the armed set, with the canvas panel reflecting registry state after reload.
- [ ] The registry shows state, last fired, 24 h fires and failures for every trigger, and an error row expands with the last error and a run link.

### QA plan

Seed two webhook workflows (shared secret, HMAC), one scheduled, one polling against the fixture provider, one event subscription and one workflow with a missing credential. Walkthrough: check registry states and counters; send a valid call, then wrong secret, bad signature, oversized body and wrong method (four named rejections in the call log); run a test session end to end (stream, pin, run from trigger, end); preview the next ten runs, save and observe a real fire; dry-poll then real-poll and force a failure to watch backoff then recovery; publish a matching event (one run), re-emit the same id (duplicate, no run), replay one; disarm everything and confirm calls are rejected.
Visual check: state chips match reality, the test log streams live, relative times are correct, URLs copy cleanly, and no secret is rendered after save.

### Slices

1. **Registry and arming** — rows, states, validated arm/disarm, registry API and screen, counters. Done: states change correctly, invalid arming fails with a named code, and the registry matches rows.
2. **Webhooks and test sessions** — ingress, auth modes, response modes, call log, sessions with pinning. Done: every rejection path and the timeout fallback behaves as specified against a real HTTP client.
3. **Schedule and polling** — cron/interval with preview, timezone and catch-up; cursor, dedup, backoff and degradation. Done: three fires land on time and the fixture poller never double-emits across a restart.
4. **Event triggers and replay** — subscription reconciliation, filters, dedup, bounded replay, degraded states. Done: matching events start runs, non-matching and duplicates do not, and replay is auditable.

### Risks / notes

- Multi-instance duplicates are the top risk: two API instances must not fire the same schedule or event twice. Local guards are event-id dedup and a conditional claim on `next_fire_at`; the full answer is REQ-096's leader election, so do not arm schedules before that interaction is settled.
- Webhook secrets live in the secret store (REQ-125) and are shown once; they never appear in logs, call records or error text.
- Signature verification needs the raw body before parsing plus a replay window, and both need tests — verifying a parsed body fails silently but open.
- Pending responses hold connections: bound the count per trigger and the timeout, or a slow workflow becomes a denial-of-service vector.
- Polling is the likely source of provider rate-limit trouble: jittered backoff, a per-provider budget and a "poll now" that respects backoff are required.
- The public hook namespace stays clear of future user-authored routes (REQ-130) with the `test` segment reserved, and test URLs stay scoped to a session owner and unroutable after expiry.
