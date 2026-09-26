# REQ-126 — Observability Stack

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Seeing the system from outside.

- Structured JSON logging with request ids and user/org attribution.
- Prometheus metrics (HTTP, DB, queue, workflow, AI usage) and a /metrics endpoint.
- OpenTelemetry tracing across HTTP, DB, queue and AI calls.
- Grafana dashboard bundle shipped in infra/ with alerts for the usual suspects.
- Health/readiness/liveness endpoints + graceful shutdown, wired into the deployment tooling.

## Implementation spec

New crate `crates/telemetry` (log schema, metric registry, tracing wiring, exporter buffers, alert evaluator) consumed by `apps/api` and every worker; the dashboard, rule and collector assets ship in `infra/observability/` as a versioned bundle. The probe states and incident store already exist in `crates/health` (REQ-014) — this request reads them and never rebuilds them. The admin surface is `/observability/*`, guarded by `observability.*` permissions.

### Scope (in / out)

**In**

- **Structured logging.** One JSON line per event on stdout: `ts` (RFC 3339, UTC), `level`, `target` (Rust module path), `msg`, `request_id`, `trace_id`, `span_id`, `user_id`, `organization_id`, `route`, `method`, `status`, `duration_ms`, `source` (`api|worker|cli`), `host`, `version`, and an allow-listed `fields` object. A middleware assigns the request id at the edge (REQ-040 already returns it as a header), binds the user/org context after authentication, and every log line inside that task inherits both. Human-readable local mode stays available through a config flag; production default is JSON. A runtime level switch raises a module's level temporarily with an automatic expiry so debugging does not need a redeploy.
- **Metrics.** A registry with documented families: `omnion_http_requests_total{route,method,status}`, `omnion_http_request_duration_seconds` (histogram), `omnion_db_pool_connections{state}`, `omnion_db_query_duration_seconds{statement}`, `omnion_queue_depth{queue,state}`, `omnion_queue_job_duration_seconds{kind}`, `omnion_queue_job_failures_total{kind}`, `omnion_workflow_steps_total{status}`, `omnion_ai_requests_total{provider,model,status}`, `omnion_ai_tokens_total{kind}`, `omnion_ai_cost_micros_total{provider,model}`, `omnion_webhook_deliveries_total{result}`, `omnion_outbound_retries_total{subsystem,outcome}`, `omnion_circuit_state{provider}` (REQ-127), `omnion_rate_limit_refusals_total{scope}`, `omnion_cache_hits_total{cache}`, plus `omnion_build_info{version,commit}`. Route labels use route templates, never raw paths; model labels come from a bounded set with an `other` overflow bucket; user, organization and request ids are never labels. A cardinality budget (max distinct series per family) is enforced at registration and reported when exceeded. `GET /metrics` serves the Prometheus text format.
- **Tracing.** OpenTelemetry spans for: inbound HTTP (server span with route template), SQLx queries (statement name, never parameter values), Redis commands, queue consume (linked to the producer span through the job's stored context), outbound HTTP (webhooks, integrations, AI providers), workflow steps, and background job runs. Context propagates inbound and outbound via W3C `traceparent`; the request id is attached as a span attribute so a request id search lands on the trace. Sampling is parent-based with an always-load-ahead policy: 100 % of errors, a configurable ratio otherwise. AI spans carry provider, model, token counts and cost — never prompt or completion text.
- **Exporter pipeline.** OTLP (gRPC or HTTP) for traces and logs, optional Prometheus remote-write, optional syslog or webhook log export, each configured as an exporter row with a secret reference (REQ-037) rather than an inline credential. Buffers are bounded; when an exporter is down the buffer drops oldest-first and increments `omnion_exporter_dropped_total{exporter}` — the request path is never blocked by telemetry.
- **Bundled assets.** `infra/observability/` ships: Grafana dashboard JSON (API overview, database, queue and workers, workflows, AI usage and cost, outbound reliability), Prometheus alert rules (error rate, p95 latency, queue depth and oldest job age, worker heartbeat, DB connections, disk, AI spend, webhook failure ratio, exporter down), an OpenTelemetry collector example configuration, and a README mapping each dashboard panel to the metric families above. The bundle has a version and a compatibility note for the instance version.
- **Lifecycle.** `/healthz` (liveness semantics: process up), `/readyz` (dependencies reachable and migrations current — flips to `503` during shutdown), `/livez` (alias of liveness for platforms that expect it), plus the existing unversioned probes kept untouched. Graceful shutdown on SIGTERM: readiness fails first, the listener stops accepting, in-flight requests drain to a deadline, telemetry flushes, pools close, one-line shutdown summary is logged, exit 0. The deployment tooling (REQ-128) wires `preStop`, probe paths and the termination grace; the deployment centre (REQ-024) waits for readiness before its health verification step.
- **Admin centre.** Screens below; the log explorer reads only the bounded store, the trace search reads the span index and deep-links to the operator's tracing backend, the metric catalogue documents every family, exporters and alert rules are managed here, settings cover sampling, retention, levels and the cardinality budget.

**Out**

- Replacing an operator's Grafana, Tempo, Jaeger or Loki; Omnion emits telemetry and ships assets, it does not become the backend.
- Business analytics dashboards (REQ-007), audit-log exploration (REQ-039), system health probes and incidents (REQ-014).
- Long-term cold storage beyond the retention window — logs and traces older than retention belong to the operator's backend.
- Synthetic external uptime checks and third-party APM agents.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/observability` | Overview: request rate, error ratio, p95, queue depth, AI spend today, exporter health, links into each area |
| `/observability/logs` | Bounded log explorer: level, target, request id, trace id, window, text, source filters |
| `/observability/traces` | Trace search (request id, route, status, min duration, window) and a span waterfall for one trace |
| `/observability/metrics` | Metric catalogue with unit, labels, cardinality, source; a bounded chart for one selector |
| `/observability/exporters` | Exporter list with health and drop counters; add/edit/test |
| `/observability/alerts` | Alert rules and state: firing, pending, resolved, silences; create/edit/test |
| `/observability/settings` | Sampling ratio, retention, log levels, cardinality budget, collector guidance |

- Log explorer rows: time · level chip · target · message · request id (copy, click to filter) · trace id (link) · user/org when present · expandable fields (rendered from the redacted object). A search by request id shows every line from that request across API and workers in order.
- Trace waterfall: span name · service · start offset · duration bar · status; selecting a span shows attributes and events with values already redacted. When no tracing backend is configured the screen says so and offers the collector example instead of an empty waterfall.
- Metric chart: selector builder fed from the catalogue (only known families and bounded label values), range selector, max points guard, "copied as PromQL" affordance. An unknown selector is refused with a message, never a blank graph.
- Exporter form: name, kind, endpoint, protocol, auth secret (pick from the secret store, never an inline value), batch interval, timeout, enabled. `Test` sends a synthetic batch and reports the backend's response; the form never renders the secret back.
- Alert rule form: name, expression (validated against the catalogue), severity, for-duration, summary, runbook link, labels; a `Preview` shows whether the rule is currently firing against live data. Silences take a rule, a duration and a reason.
- Settings: sampling ratio (0.0–1.0), retention days per signal within documented caps, per-module level overrides with an expiry, cardinality budget, and an explicit statement of what leaves the instance when an exporter is configured.
- States: skeleton rows; "no telemetry yet — the exporter was just enabled" empty state; exporter down shows the last successful flush and the drop count; permission misses render inline; every error carries a request id.
- Keyboard: `/` search, `t` switches a log row to its trace, `Esc` closes drawers. Mobile: log rows become stacked cards, the waterfall scrolls horizontally inside its own region.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/observability/overview` | Counters and exporter health for the dashboard head | `observability.read` |
| GET | `/api/v1/observability/logs` | Bounded log search (level, target, request id, trace id, window, text) | `observability.read` |
| GET | `/api/v1/observability/traces` | Trace search over the span index | `observability.read` |
| GET | `/api/v1/observability/traces/{trace_id}` | Span index for one trace + backend deep link | `observability.read` |
| GET | `/api/v1/observability/metrics/catalog` | Metric families with unit, labels, source, cardinality | `observability.read` |
| GET | `/api/v1/observability/metrics/query` | Bounded range query for a catalogue selector | `observability.read` |
| GET | `/api/v1/observability/exporters` | Exporter list with health and drop counters | `observability.read` |
| POST | `/api/v1/observability/exporters` | Add an exporter | `observability.exporters.manage` |
| PATCH | `/api/v1/observability/exporters/{id}` | Edit an exporter | `observability.exporters.manage` |
| POST | `/api/v1/observability/exporters/{id}/test` | Send a test batch | `observability.exporters.manage` |
| GET | `/api/v1/observability/alert-rules` | Rule list with state | `observability.read` |
| POST | `/api/v1/observability/alert-rules` | Create a rule | `observability.manage` |
| PATCH | `/api/v1/observability/alert-rules/{id}` | Edit or toggle a rule | `observability.manage` |
| POST | `/api/v1/observability/alert-rules/preview` | Evaluate against live data | `observability.manage` |
| GET | `/api/v1/observability/alerts` | Firing, pending, resolved states | `observability.read` |
| POST | `/api/v1/observability/silences` | Create a silence window | `observability.manage` |
| GET | `/api/v1/observability/settings` | Sampling, retention, levels, budget | `observability.read` |
| PUT | `/api/v1/observability/settings` | Save settings | `observability.manage` |
| GET | `/api/v1/observability/bundle` | Bundled dashboard/rule manifest with version | `observability.read` |
| GET | `/metrics` | Prometheus exposition (internal interface by default) | unauthenticated on the private interface; token or CIDR allow-list when public |
| GET | `/healthz` · `/livez` · `/readyz` | Probes; readiness fails during shutdown | unauthenticated |

Errors: `400` unknown metric selector or unbounded range, `403` permission miss, `422` invalid alert expression, `429` on the export endpoint when a caller exceeds its own budget.

### Data model

Migration: `database/migrations/0027_observability.sql` (next free slot at tick time), additive, with a down script per the REQ-129 policy.

- `obs_log_entries` — `id bigint generated always as identity pk`, `ts timestamptz not null default now()`, `level text not null check (level in ('trace','debug','info','warn','error'))`, `target text not null`, `message text not null`, `request_id uuid`, `trace_id text`, `span_id text`, `user_id uuid`, `organization_id uuid`, `route text`, `method text`, `status int`, `duration_ms int`, `source text not null default 'api'`, `host text`, `version text`, `fields jsonb not null default '{}'` (post-redaction only). Indexes `(ts desc)`, `(request_id)`, `(trace_id)`, `(level, ts desc)`; pruned by the retention job via the REQ-038 retention classes.
- `obs_trace_index` — `trace_id text pk`, `root_name text not null`, `service text not null`, `route text`, `request_id uuid`, `started_at timestamptz not null`, `duration_ms int not null`, `span_count int not null default 1`, `status text not null check (status in ('ok','error'))`, `sampled boolean not null default true`, `attributes jsonb not null default '{}'` (redacted). Indexes `(started_at desc)`, `(request_id)`, `(status, started_at desc)`; spans themselves stay in the operator's backend.
- `obs_metric_catalog` — `id uuid pk`, `name text not null unique`, `kind text not null check (kind in ('counter','gauge','histogram'))`, `unit text not null default ''`, `description text not null default ''`, `labels text[] not null default '{}'`, `source text not null` (`core|module|worker`), `cardinality_estimate int`, `budgeted boolean not null default true`, `last_seen_at`. Seeded from the registry at boot; the panel reads this, not a hard-coded list.
- `obs_settings` — single row: `id smallint pk default 1 check (id = 1)`, `sampling_ratio double precision not null default 0.1 check (sampling_ratio between 0 and 1)`, `logs_retention_days int not null default 14`, `traces_retention_days int not null default 7` (both range-checked against documented caps), `log_level_default text not null default 'info'`, `log_level_overrides jsonb not null default '{}'`, `cardinality_budget int not null default 10000`, `prometheus_public boolean not null default false`, `updated_by uuid`, `updated_at timestamptz not null default now()`.
- `obs_exporters` — `id uuid pk`, `name text not null`, `kind text not null check (kind in ('otlp','prometheus_remote_write','syslog','webhook'))`, `endpoint text not null`, `protocol text`, `auth_secret_id uuid references secrets(id) on delete set null`, `batch_ms int not null default 5000`, `timeout_ms int not null default 10000`, `enabled boolean not null default true`, `health text not null default 'unknown' check (health in ('unknown','ok','degraded','down'))`, `last_flush_at`, `last_error text`, `dropped_total bigint not null default 0`, `created_by uuid`, `created_at`. Unique `(name)`.
- `obs_alert_rules` — `id uuid pk`, `name text not null unique`, `expr text not null`, `severity text not null check (severity in ('info','warning','critical'))`, `for_seconds int not null default 300 check (for_seconds between 0 and 86400)`, `summary text not null default ''`, `runbook_url text`, `labels jsonb not null default '{}'`, `source text not null default 'bundled' check (source in ('bundled','custom'))`, `checksum text`, `enabled boolean not null default true`, `updated_by uuid`, `updated_at`.
- `obs_alert_events` — `id bigserial pk`, `rule_id uuid not null references obs_alert_rules(id) on delete cascade`, `state text not null check (state in ('pending','firing','resolved'))`, `value double precision`, `labels jsonb not null default '{}'`, `started_at timestamptz not null default now()`, `ended_at`, `notified boolean not null default false`; index `(rule_id, started_at desc)` and `(state, started_at desc)`. Bounded retention; the evaluator coalesces flapping into one event per rule per window.
- `obs_silences` — `id uuid pk`, `rule_id uuid references obs_alert_rules(id) on delete cascade`, `reason text not null`, `starts_at`, `ends_at timestamptz not null`, `created_by uuid`; check `ends_at > coalesce(starts_at, now())` enforced in the service, index `(ends_at)`.

### Events

- **Emitted:** `observability.exporter.degraded`, `observability.exporter.recovered`, `observability.alert.fired` (rule, severity, value, runbook), `observability.alert.resolved`, `observability.silence.created`, `observability.sampling.changed`, `observability.log_level.changed`, `observability.retention.pruned`.
- **Consumed:** `health.service.degraded`/`recovered` (REQ-014) annotate alert evaluation so a probe incident and an alert are not two unrelated stories; `deployment.started`/`succeeded`/`failed` (REQ-024) annotate the timeline and gate the deploy's health verification on readiness; `secret.rotated` (REQ-037) re-resolves exporter auth references.
- Webhook relevance: `observability.alert.fired` and `.resolved` are the payloads an operations endpoint subscribes to — rule name, severity, value, window, runbook link, and nothing else. Log lines, user data and secret fragments never appear in a payload.
- Notification relevance: critical alerts route through REQ-021 to the configured on-call recipients; a degraded exporter notifies holders of `observability.exporters.manage` once per state change, not per retry.

### Acceptance criteria

- [ ] `database/migrations/0027_observability.sql` applies on a fresh and a populated database, and its down script reverses it.
- [ ] Every request emits one JSON log line to stdout with `request_id`, and authenticated requests also carry `user_id` and `organization_id`.
- [ ] A worker log line carries the `trace_id` of the request that enqueued the job.
- [ ] `GET /metrics` exposes the documented families with real values after traffic; route labels are templates (`/api/v1/secrets/{id}`, not the literal id).
- [ ] Registering a metric that would exceed the cardinality budget is reported and labelled, not silently dropped.
- [ ] A traced request produces spans for HTTP → SQLx → queue publish, and the consumer span links back to the producer.
- [ ] An AI call produces a span with provider, model and token counts and no prompt or completion text.
- [ ] Error requests are always sampled regardless of the ratio, and a sampled trace is findable by request id in `/observability/traces`.
- [ ] The bundled Grafana dashboards import cleanly against Prometheus, and each panel in the README's mapping query returns data.
- [ ] A shipped alert rule fires in the QA stack (stop Redis), creates a `firing` event, notifies once, and resolves when the dependency returns.
- [ ] The alert preview endpoint reports firing state against live data without saving anything.
- [ ] Enabling an exporter with an unreachable backend does not stall a single request; the drop counter rises and the exporter flips to `degraded`.
- [ ] The syslog or webhook log exporter sends redacted fields only (asserted by a test against a fixture secret value and a fixture e-mail address).
- [ ] Settings reject out-of-range sampling, retention beyond the cap and unknown level names with field-level messages.
- [ ] A temporary log-level raise expires back to the configured default without a restart.
- [ ] SIGTERM flips `/readyz` to `503`, `/healthz` stays `200`, in-flight requests finish, telemetry flushes, and the process exits 0 with a shutdown summary line.
- [ ] Retention prunes log rows and trace-index rows past the window without touching audit or incident data.
- [ ] `observability.read` cannot create exporters, rules or silences (`403`); every mutation writes an audit row.
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The walkthrough visits `/observability`, `/observability/logs`, `/observability/traces`, `/observability/metrics`, `/observability/exporters`, `/observability/alerts` and `/observability/settings`, and clicks: a request-id filter that lands on all lines of one request, a trace id that opens the waterfall, a metric selector and range change, exporter `Test` against a deliberately wrong endpoint (expects a degraded report, not an error page), alert preview while a rule is firing, silence creation and expiry, and an invalid settings save (expects field-level messages). The API-level pass curls `/metrics` and asserts the families, drives traffic with the load tool of the QA stack, and greps recorded stdout for a fixture secret value and e-mail address to prove redaction.

The lifecycle pass sends SIGTERM while requests are in flight and asserts the readyz/healthz flip, the drain, the flush and exit 0. The Grafana import check runs against the dev-stack Prometheus with the sample dashboard set. The visual check must see: readable log rows with monospace message text, chips distinguishable without colour, a waterfall that scrolls inside its region, no `NaN` or `Infinity` in any chart, and card layout under 640 px.

### Slices

1. **Logging + request id + redaction.** Log schema crate, middleware binding request id and user/org context, worker propagation, the shared redaction pass on fields, log explorer screen. *Done when:* one request produces ordered lines across API and worker, findable by request id, with the redaction test green.
2. **Metrics.** Registry, families for HTTP/DB/queue/workflow/AI, cardinality guard, `/metrics`, metric catalogue and chart screen. *Done when:* a scrape returns every documented family and the catalogue matches the registry.
3. **Tracing + exporter pipeline.** Span coverage, W3C propagation, queue span links, sampling policy, OTLP/remote-write/syslog exporters with bounded buffers, trace search screen and deep links. *Done when:* a request id finds its trace, and a dead exporter degrades without blocking traffic.
4. **Lifecycle + bundle + alerts.** Graceful shutdown sequence, probes and their contract, Grafana/Prometheus bundle in `infra/observability/`, alert rules, evaluator with silences and notifications, settings screen, deployment wiring for probes and preStop. *Done when:* an alert fires and resolves through a real dependency outage, the bundle imports, and SIGTERM drains cleanly.

### Risks / notes

- Cardinality is the failure mode of every metric system: route templates only, bounded model and provider labels, no user or organization identifiers as labels, and a registration-time budget so a module cannot quietly create a million series.
- Telemetry is the most likely place for a secret or personal datum to leak: one shared redaction helper (REQ-037/REQ-039) runs over log fields, span attributes and exporter payloads, and a test greps recorded output rather than trusting reviewers.
- Tracing adds overhead: keep sampling parent-based with an error bias, cap spans per trace, and keep the exporter on a bounded background buffer so a slow backend can never slow a request.
- Buffered telemetry loses data by design when a backend is down; the drop counter and the health chip make that honest instead of silent, and the export screen states the trade-off.
- The log store is a convenience for recent debugging, not a log platform: capped retention, pruned rows, and a clear pointer to the operator's own backend for anything older.
- `/metrics` and the probe paths are the only unauthenticated surfaces; bind them to the internal interface by default and document the token or allow-list option when an instance exposes them publicly.
- Clock skew between services breaks naive ordering; timestamps stay UTC RFC 3339, durations come from monotonic clocks, and the waterfall draws from span-relative offsets.
- Alert thresholds must state their window and their deduplication behaviour, or a flapping dependency floods every subscriber.
