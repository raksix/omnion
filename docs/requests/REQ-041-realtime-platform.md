# REQ-041 — Real-time Platform

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (WS/SSE layer) + admin
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

WebSocket / SSE:

```text
Admin A
   │
   ├── Page updated
   │
   └──────────→ Admin B
```

Notifications, workflow executions, AI streaming, logs — all update live.

## Notes

- n8n's push/event patterns (docs/09 §6 lifecycle events) are prior art; keep one event bus
  for UI, webhooks (REQ-016) and automations.

## Implementation spec

### Scope (in / out)

**In**

- One transport crate (`crates/realtime`): topic registry, subscription authorization, SSE
  (`text/event-stream`) and a WebSocket upgrade endpoint over the same fan-out.
- Fan-out from the existing event bus (`crates/events`) plus direct publishes; Redis pub/sub
  carries each message to every API replica, so any replica may serve any subscriber.
- Replay window: bounded per-topic ring buffer with monotonically increasing sequence numbers
  and resume via `Last-Event-ID` (SSE) or `?since=<seq>` (WebSocket).
- Live panel surfaces: notification badge and bell list, workflow execution log, AI chat
  tokens, audit/log tail, presence avatars on entity detail screens, live counters on the
  overview dashboard.
- Admin console: connected sessions, topic table with traffic counters, live tail with
  pause/copy, force-disconnect, connection-health panel.
- Heartbeats (SSE comment frame every 20 s), idle timeout, bounded per-connection send queue
  (slow consumers are disconnected with a reason instead of stalling the replica), client
  reconnect with exponential backoff and jitter.

**Out**

- Replacing REST for reads or writes — the stream is an accelerator, never the source of truth.
- Anonymous public visitors on `apps/web` (follows with REQ-011/REQ-018).
- Mobile push, e-mail and digest channels (REQ-021 owns them).
- Streaming media bytes; the media library keeps plain HTTP delivery.
- Cross-organization topics; every topic is scoped to one organization.

### Screens (UI)

Admin routes:

- `/realtime` — hub with three tabs: **Sessions**, **Topics**, **Health**; header shows totals
  (connected sessions, events/min, replica count) and a "Live tail" button that opens a right
  drawer.
- `/realtime/sessions` (same screen, tab state kept in `?tab=`) — table columns: User
  (avatar + display name + e-mail), Transport (`SSE`/`WS` badge), Topics (chips, `+N` overflow),
  Device (parsed UA label), IP (masked last octet), Connected at (relative), Last event, Idle
  (seconds), Actions. Filters: transport select, topic multi-select, text search over user,
  "connected only" toggle. Sort: connected at, last event, idle. Bulk actions: disconnect
  selected, copy session ids, export CSV. Row action: **Disconnect** (confirm dialog naming
  the user and topic count).
- `/realtime/topics` — table columns: Topic, Scope (org/site), Subscribers, Events (1 h),
  Events (24 h), Buffer fill (bar, `used/capacity`), Last event, Retention. Filters: scope
  select, text search. Actions: **Tail** (opens drawer pre-filtered), **Clear buffer**
  (confirm; buffer only, never the event log), **Copy topic name**.
- Live tail drawer: monospaced event rows (`time · topic · seq · payload preview`), `Pause`
  and `Resume`, `Clear`, `Copy last JSON`, payload expander with pretty-printed JSON,
  filter input, "autoscroll" toggle. Max 500 rows in DOM, older rows dropped.
- Empty states: no sessions → "Nothing is connected right now" + hint to open a second tab;
  no topics → "Topics appear as soon as a feature publishes"; loading → skeleton rows;
  error → inline banner with `Retry` and the API error code.
- Embedded surfaces: notification bell badge updates without reload; workflow execution detail
  appends log lines live with a "Live" pill that degrades to "Reconnecting…"; AI chat streams
  tokens (already SSE today — this REQ unifies it onto the same transport).
- Keyboard: `/` focus filter, `j`/`k` move row selection, `Space` toggle row, `t` open live
  tail, `p` pause/resume tail, `c` copy focused row JSON, `Esc` close drawer.
- Mobile: tables become stacked cards (user + transport + idle in the first line, actions in
  an overflow menu); live tail opens full-screen; tabs become a segmented control.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/realtime/stream` | SSE subscription to one or more topics (`?topics=`, `Last-Event-ID`) | `realtime.connect` |
| GET | `/api/v1/realtime/ws` | WebSocket upgrade, same topic model | `realtime.connect` |
| GET | `/api/v1/realtime/topics` | Topic registry with traffic counters | `realtime.read` |
| GET | `/api/v1/realtime/sessions` | Connected sessions (paged, filterable) | `realtime.read` |
| DELETE | `/api/v1/realtime/sessions/{id}` | Force-disconnect one session | `realtime.manage` |
| POST | `/api/v1/realtime/sessions/disconnect` | Bulk disconnect by id list | `realtime.manage` |
| POST | `/api/v1/realtime/topics/{topic}/clear` | Clear a topic's replay buffer | `realtime.manage` |
| POST | `/api/v1/realtime/announce` | Publish an operator announcement to a topic | `realtime.manage` |
| GET | `/api/v1/realtime/health` | Subscriber counts, queue depth, dropped frames, replica fan-out lag | `realtime.read` |

Unknown topics answer `404` with the closest known topic names; a subscribe request without
`realtime.connect` answers `403` before the stream opens. Every handler applies the tenancy
scope rule through the topic's organization.

### Data model

Migration `database/migrations/0011_realtime.sql` (next free number at build time).

- `realtime_topics` — `id uuid pk default gen_random_uuid()`, `organization_id uuid null
  references organizations(id) on delete cascade`, `site_id uuid null references sites(id)`,
  `name text not null` (dotted, lower-case, same shape as event names), `description text not
  null default ''`, `retention_seconds int not null default 900 check (retention_seconds
  between 60 and 86400)`, `buffer_capacity int not null default 500`, `created_at timestamptz`,
  `updated_at timestamptz`. Unique `realtime_topics_name_key (name)`.
- `realtime_sessions` — `id uuid pk`, `user_id uuid references users(id) on delete cascade`,
  `organization_id uuid null references organizations(id)`, `transport text not null check
  (transport in ('sse','ws'))`, `client_label text not null default ''`,
  `connected_at timestamptz not null default now()`, `last_seen_at timestamptz not null
  default now()`, `disconnected_at timestamptz null`, `close_reason text null`,
  `topics jsonb not null default '[]'::jsonb`.
- `realtime_stream_events` — `id bigint generated always as identity pk`, `topic_name text not
  null`, `sequence bigint not null`, `payload jsonb not null default '{}'::jsonb`,
  `created_at timestamptz not null default now()`, unique `(topic_name, sequence)`.
- Indexes: `realtime_sessions_open_idx (organization_id, connected_at desc) where
  disconnected_at is null`; `realtime_sessions_user_idx (user_id, connected_at desc)`;
  `realtime_stream_events_topic_idx (topic_name, sequence desc)`.
- Retention: a scheduled job prunes `realtime_stream_events` past `retention_seconds` and
  marks sessions idle beyond 5 minutes as disconnected with reason `idle_timeout`.

### Events

- Emitted (bus, webhook-relevant): `realtime.session.started`, `realtime.session.ended`,
  `realtime.buffer.cleared`.
- Consumed: every bus event from `crates/events` is projected onto its topic name (event name
  = topic name for platform events), so workflow, notification and audit activity reach the
  UI without a second pipeline.
- Internal only (never webhooked): per-token chat deltas, progress ticks, presence
  heartbeats, dashboard counters.
- Webhook relevance: session lifecycle lets operations tooling alert on connection storms;
  per-token traffic must never be forwarded to webhooks.

### Acceptance criteria

- [ ] A second browser session sees a page update within 2 seconds of the first one publishing.
- [ ] SSE stream resumes after a simulated drop using `Last-Event-ID` with no missed event.
- [ ] WebSocket and SSE clients on the same topic receive identical sequences.
- [ ] Subscribing to a topic outside the caller's organization is refused with `403`.
- [ ] Subscribing to an unknown topic returns `404` listing near matches.
- [ ] `/realtime` Sessions tab lists live connections with transport, topics and idle time.
- [ ] Force-disconnect closes the socket within 1 second and logs the reason.
- [ ] Bulk disconnect works on a multi-row selection with a single confirm.
- [ ] Topics tab shows subscribers, 1 h/24 h counters and buffer fill.
- [ ] Live tail pauses, resumes, clears and copies the focused event as JSON.
- [ ] Notification badge increments live without a manual refresh.
- [ ] Workflow execution log appends lines live and shows the reconnect state.
- [ ] A slow consumer is dropped with `slow_consumer` instead of stalling other subscribers.
- [ ] Redis fan-out delivers one publish to subscribers on two API replicas.
- [ ] Retention job prunes expired buffer rows and marks idle sessions disconnected.
- [ ] `GET /realtime/health` reports subscriber count, queue depth and dropped frames.
- [ ] All new endpoints appear in the permission catalogue with their keys and descriptions.
- [ ] `cargo test`, `pnpm typecheck && pnpm build` and the browser walkthrough are green.

### QA plan

- Walkthrough: open `/realtime` twice (two browser contexts), publish a change in one, assert
  the event row appears in the other's live tail; click Pause/Resume/Clear/Copy; select rows
  and run bulk disconnect; open Topics and Tail a topic; check Health tab counters move.
- Also exercise the embedded surfaces: badge increment on notification creation, workflow run
  detail log while a run is started from `/workflows`.
- Visual check: connection states are legible (Live / Reconnecting pills), the tail drawer does
  not overlap the sidebar, transport badges are distinct, empty state copy renders, and mobile
  cards stack without horizontal scroll at 390 px width.
- Regression: `/`, `/pages`, `/media`, `/sites`, `/ai` still load with the transport enabled.

### Slices

1. **Transport core** — `crates/realtime`, migration `0011_realtime.sql`, topic projection from
   the bus, SSE + WS endpoints, sequence numbers, replay, Redis fan-out, permission keys,
   integration tests. **Done when:** two API replicas deliver one publish to both subscribers
   and a reconnecting SSE client resumes without gaps (`cargo test -p omnion-realtime` green).
2. **Admin console** — `/realtime` screen with Sessions/Topics/Health tabs, live tail drawer,
   bulk actions, disconnect, clear buffer, nav entry. **Done when:** a session opened in a
   second tab is visible, disconnectable, and the tail drawer streams rows (walkthrough passes).
3. **Live surfaces** — notification badge, workflow execution log stream, presence avatars,
   overview counters, reconnect UX, slow-consumer protection. **Done when:** badge and workflow
   log update live in the walkthrough and a slow consumer is dropped without affecting peers.

### Risks / notes

- Reverse proxies buffer responses: the deployment docs must ship the `X-Accel-Buffering: no`
  header and the nginx streaming recipe, or SSE appears dead behind an ingress.
- Bearer tokens in query strings leak into logs — authenticate the stream with the session
  cookie plus a short-lived one-time stream ticket.
- Unbounded buffers are the classic memory failure; capacity and retention are per-topic and
  enforced, and the health panel surfaces drops.
- Presence must not become a mutable presence database; sessions are ephemeral, and only
  aggregate counts are ever webhooked.
- Keep one bus: publishing to topics must go through `crates/events` so webhooks, automations
  and the UI never diverge.
