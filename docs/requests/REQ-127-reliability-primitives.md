# REQ-127 — Reliability Primitives

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The boring guarantees that keep a platform honest.

- Idempotency keys for mutating endpoints and job submissions.
- Retry policies with exponential backoff and jitter per subsystem (webhooks, e-mail, AI, workflows).
- Rate limiting per user/organization/IP with configurable budgets and 429 semantics.
- Circuit breaking for outbound providers with half-open probing.
- Request sanitisation, HMAC verification for inbound webhooks, and payload size limits.

## Implementation spec

New crate `crates/reliability` (idempotency store, limiter, retry scheduler, breaker state machine, intake guard) wired into `apps/api` middleware and every outbound subsystem. The per-key rate limits of the API gateway (REQ-040) stay where they are: this request adds the **platform-wide** budgets (user, organization, IP, route), the idempotency contract, retry policies, breaker state and the inbound intake guard. Admin surface is `/settings/reliability/*`; counters live in Redis with PostgreSQL as the store of record for configuration and ledgers.

### Scope (in / out)

**In**

- **Idempotency keys.** Mutating endpoints and job submissions accept an `Idempotency-Key` header (or a body field on job submissions). The first attempt stores a keyed record: a fingerprint of method + path + body, an `in_progress` state, then the final response (status, headers subset, body within a size cap, or an object-store reference beyond it). A replay of the same key with the same fingerprint returns the stored response with `Idempotent-Replay: true`; a different fingerprint is `409 idempotency_conflict`; a replay while the first attempt is still running is `409` with `Retry-After`. Keys carry a scope (endpoint family + subject) and a TTL (default 24 h). Endpoints opt in through a route annotation, and the response for a keyed write always states the request id of the original execution.
- **Retry policies per subsystem.** Webhooks, e-mail, AI calls, workflow steps, integration deliveries and storage operations each read a policy: max attempts, base delay, backoff factor, jitter mode (`none` `equal` `full`, default `full`), maximum elapsed time, and the retryable error classes (HTTP statuses, provider error codes, transport errors). A policy is editable per subsystem and per provider override without a deploy; the scheduler persists the next attempt time on the job row so a restart cannot lose or double an attempt. Exhausted retries produce a dead-letter record with the full attempt timeline and a `retry now` action.
- **Rate limiting.** Token-bucket limits per scope (`user` `organization` `ip` `route`), evaluated in Redis with a sliding window, configurable limit, window, burst, and priority; the most specific matching policy wins and the panel says which one is winning on a dry-run evaluate. Refusals return `429` with `Retry-After` and `X-RateLimit-Limit`/`Remaining`/`Reset`, are counted per scope and route, and feed one aggregated event per target and window rather than a per-request flood. Login, password-reset and public form routes ship with conservative default budgets.
- **Circuit breaking.** One breaker per outbound provider key (an AI provider, a webhook destination host, an SMTP relay, a payment provider, the search backend). Configurable failure threshold within a window, cooldown, half-open probe count and success threshold to close. Open state fails fast with a typed `provider_unavailable` error, or fails a job to its retry queue when the caller is asynchronous. State transitions are logged and persisted so a restart does not pretend a broken provider is healthy; a manual `Reset` and an explicit `Force open` (drain a provider deliberately) both exist and are audited. Health events (REQ-014) may pre-open a breaker for a dependency already known to be degraded.
- **Intake guard.** Inbound webhook endpoints are declared with an HMAC scheme (algorithm, signature header name, encoding), a replay tolerance window, a secret reference from the store, a maximum payload size, and a sanitisation profile. Verification compares in constant time, rejects a stale timestamp or a replayed signature id, and logs the rejection reason without echoing the payload. Body size caps apply before authentication so an oversized request costs nothing; JSON bodies are size-limited per field depth as well. Sanitisation strips control characters from headers and stored strings, refuses unknown content types, and records what it changed in the request log — it is a narrow guard, not a content filter, and never rewrites legitimate business payloads.
- **Shared contracts.** Error codes are stable and documented (`idempotency_conflict`, `rate_limited`, `payload_too_large`, `signature_invalid`, `provider_unavailable`); a `reliability.head` read model exposes the counters the panel and the observability stack consume.

**Out**

- Distributed transactions and sagas across services; idempotency covers single-write replays.
- WAF features, bot detection, CAPTCHA and DDoS mitigation (edge concern).
- Rewriting existing subsystem job tables — policies drive them through shared helpers; the rows stay where they are.
- Inbound rate limiting for third-party webhooks that Omnion calls out to (their problem, and a breaker protects us from it).

### Screens (UI)

| Route | Purpose |
|---|---|
| `/settings/reliability` | Overview: refusals 24 h, idempotency conflicts, breaker states, retry backlog, rejected intake |
| `/settings/reliability/limits` | Rate-limit policies per scope with priority, burst, window; refusal rollup; dry-run evaluate |
| `/settings/reliability/idempotency` | Recent keys with state, subject, latency, replay count; release a stuck in-progress key |
| `/settings/reliability/retries` | Per-subsystem policy editor with a delay preview; attempt log; dead-letter list with retry now |
| `/settings/reliability/breakers` | Breaker per provider: state, failure rate, threshold, cooldown, last trip, reset, force open |
| `/settings/reliability/intake` | Inbound endpoints with HMAC settings, size caps, sanitisation profile, rejection log |

- Limits screen: policy table sorted by specificity with a badge showing which policy wins for a sample request; the dry-run form takes scope, target, route and returns the resolved policy plus the remaining budget — a tool an operator can use under pressure. Defaults are marked as such and a delete on a default is a disable, not a removal.
- Retry editor: number inputs with validation, a chart of the resulting delay sequence for attempts 1–8 (so a wrong factor is visible before saving), jitter mode with a plain-language explanation, and a warning when a policy could retry past the job's own deadline.
- Breaker screen: state chip with icon and text, trip count, a 24 h state timeline, threshold and cooldown fields, `Reset` (confirm) and `Force open` (typed reason, confirm). A forced-open breaker shows a banner until it is reset.
- Intake screen: per endpoint, HMAC scheme, header, tolerance seconds, secret reference (picker, mask shown), max payload bytes and sanitisation profile; a `Verify sample` action takes a signature and payload from the operator, answers valid/invalid with the reason, and never echoes the secret. Rejection log columns: time · endpoint · reason · source address · request id.
- States: empty states with a call to action; skeletons; a Redis-unavailable banner explaining the documented fallback behaviour (see risks); every error carries a request id. Nothing on these screens writes to a production policy without a confirmation that names the scope.
- Keyboard: `/` search, `n` new policy, `Esc` closes drawers. Mobile: tables become cards, numeric forms keep their validation messages visible.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/reliability/overview` | Counters for the head of the centre | `reliability.read` |
| GET | `/api/v1/reliability/rate-limits` | Policy list with resolved order | `reliability.read` |
| POST | `/api/v1/reliability/rate-limits` | Create a policy | `reliability.manage` |
| PATCH | `/api/v1/reliability/rate-limits/{id}` | Edit a policy | `reliability.manage` |
| DELETE | `/api/v1/reliability/rate-limits/{id}` | Remove a custom policy | `reliability.manage` |
| POST | `/api/v1/reliability/rate-limits/evaluate` | Dry-run: which policy applies, remaining budget | `reliability.manage` |
| GET | `/api/v1/reliability/rate-limits/refusals` | Refusal rollups per scope, route and window | `reliability.read` |
| GET | `/api/v1/reliability/idempotency` | Recent keys: subject, state, latency, replays | `reliability.read` |
| GET | `/api/v1/reliability/idempotency/{key}` | One key with stored response metadata | `reliability.read` |
| DELETE | `/api/v1/reliability/idempotency/{key}` | Release a stuck in-progress key (reason required) | `reliability.manage` |
| GET | `/api/v1/reliability/retry-policies` | Policies per subsystem with provider overrides | `reliability.read` |
| PUT | `/api/v1/reliability/retry-policies/{subsystem}` | Save a subsystem policy | `reliability.manage` |
| GET | `/api/v1/reliability/retry-attempts` | Attempt log and dead-letter list | `reliability.read` |
| POST | `/api/v1/reliability/retry-attempts/{id}/retry-now` | Requeue a dead-lettered attempt | `reliability.manage` |
| GET | `/api/v1/reliability/breakers` | Breaker states and configuration | `reliability.read` |
| PATCH | `/api/v1/reliability/breakers/{key}` | Edit thresholds, cooldown, probe counts | `reliability.manage` |
| POST | `/api/v1/reliability/breakers/{key}/reset` | Close a breaker manually | `reliability.manage` |
| POST | `/api/v1/reliability/breakers/{key}/force-open` | Open deliberately with a reason | `reliability.manage` |
| GET | `/api/v1/reliability/intake` | Inbound endpoint guard configuration | `reliability.read` |
| POST | `/api/v1/reliability/intake` | Declare an intake endpoint | `reliability.intake.manage` |
| PATCH | `/api/v1/reliability/intake/{id}` | Edit HMAC, caps, sanitisation profile | `reliability.intake.manage` |
| POST | `/api/v1/reliability/intake/{id}/verify-sample` | Check an operator-supplied signature sample | `reliability.intake.manage` |
| GET | `/api/v1/reliability/intake/rejections` | Rejection log | `reliability.read` |

Data-plane behaviour: the keyed write path adds `Idempotency-Key` handling inside the authenticated middleware chain of REQ-040, after the scope and IP checks so a refused request never consumes a key. Refusals use the standard error shape (`code`, `message`, `request_id`). The intake guard runs before authentication on declared inbound paths, with size caps applied at the edge.

### Data model

Migration: `database/migrations/0028_reliability.sql` (next free slot at tick time), additive, with a down script per the REQ-129 policy.

- `idempotency_keys` — `id bigserial pk`, `scope text not null`, `subject_id text not null` (user, api key or organization id), `key text not null`, `method text not null`, `path text not null`, `request_hash text not null`, `state text not null default 'in_progress' check (state in ('in_progress','completed','failed'))`, `response_status int`, `response_headers jsonb not null default '{}'`, `response_body jsonb`, `response_body_ref text` (object-store key when the body exceeds the inline cap), `replay_count int not null default 0`, `expires_at timestamptz not null`, `created_at timestamptz not null default now()`, `completed_at`. Unique `(scope, subject_id, key)`; index `(expires_at)` for pruning and `(state) where state = 'in_progress'`. Retention job deletes expired rows; in-progress rows older than the execution deadline are flipped to `failed` so a crashed request cannot block a key forever.
- `rate_limit_policies` — `id uuid pk`, `name text not null`, `scope text not null check (scope in ('user','organization','ip','route'))`, `target_id text` (null = all), `route_pattern text`, `limit_count int not null check (limit_count > 0)`, `window_seconds int not null check (window_seconds between 1 and 86400)`, `burst int not null default 0`, `priority int not null default 100`, `is_default boolean not null default false`, `enabled boolean not null default true`, `created_by uuid`, `created_at`, `updated_at`. Unique `(scope, target_id, route_pattern)`; index `(enabled, priority)`.
- `rate_limit_refusals` — `id bigserial pk`, `scope text not null`, `target_id text`, `route text not null`, `window_start timestamptz not null`, `refusals int not null default 0`, `last_refusal_at timestamptz not null default now()`. Unique `(scope, target_id, route, window_start)`; short retention. Live counters stay in Redis; this table is the rollup the panel and events read.
- `retry_policies` — `id uuid pk`, `subsystem text not null check (subsystem in ('webhook','email','ai','workflow','integration','storage'))`, `provider_override text`, `max_attempts int not null default 5 check (max_attempts between 1 and 20)`, `base_delay_ms int not null default 1000`, `factor numeric(4,2) not null default 2.0 check (factor between 1.0 and 10.0)`, `jitter text not null default 'full' check (jitter in ('none','equal','full'))`, `max_elapse_ms int not null default 3600000`, `retry_on jsonb not null default '{}'` (status codes and error classes), `enabled boolean not null default true`, `updated_by uuid`, `updated_at`. Unique `(subsystem, provider_override)`.
- `retry_outcomes` — `id bigserial pk`, `subsystem text not null`, `subject_kind text not null`, `subject_id text`, `attempt int not null`, `scheduled_at timestamptz`, `executed_at timestamptz`, `outcome text not null check (outcome in ('succeeded','failed_retryable','failed_permanent','exhausted'))`, `error_class text`, `next_delay_ms int`, `dead_letter boolean not null default false`; index `(subsystem, dead_letter, executed_at desc)`.
- `circuit_breakers` — `key text pk` (provider key), `name text not null`, `failure_threshold int not null default 5 check (failure_threshold between 1 and 100)`, `window_seconds int not null default 60`, `cooldown_seconds int not null default 30`, `half_open_probes int not null default 3`, `success_threshold int not null default 3`, `state text not null default 'closed' check (state in ('closed','open','half_open'))`, `forced_open boolean not null default false`, `opened_at timestamptz`, `state_changed_at timestamptz not null default now()`, `trips_total bigint not null default 0`, `updated_by uuid`.
- `breaker_events` — `id bigserial pk`, `key text not null`, `from_state text not null`, `to_state text not null`, `reason text`, `failure_rate double precision`, `created_at timestamptz not null default now()`; index `(key, created_at desc)`.
- `intake_endpoints` — `id uuid pk`, `path text not null unique`, `name text not null`, `hmac_scheme text not null check (hmac_scheme in ('sha256_hex','sha256_base64','sha1_hex'))` `signature_header text not null`, `timestamp_header text`, `tolerance_seconds int not null default 300`, `secret_id uuid references secrets(id) on delete restrict`, `max_payload_bytes int not null default 1048576 check (max_payload_bytes between 1024 and 10485760)`, `sanitize_profile text not null default 'strict' check (sanitize_profile in ('strict','balanced'))` `enabled boolean not null default true`, `created_by uuid`, `created_at`.
- `intake_rejections` — `id bigserial pk`, `endpoint_id uuid references intake_endpoints(id) on delete cascade`, `reason text not null check (reason in ('signature_missing','signature_invalid','timestamp_stale','replay','payload_too_large','content_type_refused','malformed'))` `source_ip inet`, `request_id uuid`, `created_at timestamptz not null default now()`; short retention; index `(endpoint_id, created_at desc)`.

### Events

- **Emitted:** `reliability.limit.exceeded` (aggregated per scope, target, route and window), `reliability.idempotency.conflict`, `reliability.idempotency.keys.released`, `reliability.retry.scheduled`, `reliability.retry.exhausted` (dead letter), `reliability.breaker.opened`, `reliability.breaker.half_opened`, `reliability.breaker.closed`, `reliability.intake.rejected`, `reliability.policy.updated`.
- **Consumed:** `health.service.degraded` (REQ-014) may pre-open a breaker for the matching outbound provider; `deployment.started` (REQ-024) raises the limiter's reserved budget for the deploying actor so a rollout is not throttled by its own limits; `secret.rotated` (REQ-037) re-resolves intake HMAC secrets for the endpoints that reference them.
- Webhook relevance: `reliability.breaker.opened`, `reliability.retry.exhausted` and `reliability.intake.rejected` are the operator-worthy payloads; `limit.exceeded` is aggregated and never emitted per request. Payloads carry scope, target, counts and reason — never the request body, a signature or a secret.
- Notification relevance: an opened breaker and an exhausted dead letter notify holders of `reliability.manage` once per state change through the REQ-021 router.

### Acceptance criteria

- [ ] `database/migrations/0028_reliability.sql` applies on a fresh and a populated database, and its down script reverses it.
- [ ] Replaying a keyed write with the same key and body returns the stored response with `Idempotent-Replay: true` and does not execute the handler twice (asserted by counting side effects).
- [ ] The same key with a different body is `409 idempotency_conflict`.
- [ ] A replay while the original attempt is running is `409` with `Retry-After`, and resolves once the original completes.
- [ ] A keyed request that is refused by a permission check never consumes an idempotency key.
- [ ] Expired keys are pruned, and an in-progress key past the execution deadline is released so it can be retried.
- [ ] A `429` refusal carries `Retry-After`, `X-RateLimit-Limit/Remaining/Reset` and the standard error code.
- [ ] Limits apply per user, per organization, per IP and per route; the most specific policy wins and the dry-run endpoint names it.
- [ ] A burst above the configured burst allowance is refused, and refusals roll up into one entry per window rather than one per request.
- [ ] A retry policy with full jitter produces a spread of delays across attempts in a distribution test, and a policy of `none` is deterministic.
- [ ] Retry attempts survive a worker restart (next-attempt time is persisted, no double execution).
- [ ] A non-retryable error class is not retried; an exhausted delivery produces one dead letter with the full timeline and a working `retry now`.
- [ ] A breaker opens after the configured failure count within its window, fails fast with `provider_unavailable`, and emits one opened event.
- [ ] In half-open, the configured number of probes decides: successes close it, a failure reopens it with an incremented trip count.
- [ ] A restart does not reset an open breaker, and `Reset` and `Force open` both require confirmation and write audit rows.
- [ ] An inbound webhook with a valid signature is accepted; a tampered body, a signature outside the tolerance window and a replayed signature are each rejected with the documented code and a rejection row.
- [ ] A payload above the size cap is refused before authentication with `413 payload_too_large`.
- [ ] Sanitisation removes control characters from headers and stored strings without altering a legitimate JSON payload (fixture comparison test).
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The walkthrough visits `/settings/reliability` and its five sub-screens, and clicks: policy create and edit, the dry-run evaluate (expects a named winning policy and a remaining budget), the retry delay preview at factor 1.0 and 3.0, a dead-letter `retry now`, a breaker `Reset` and a `Force open` with a reason, intake endpoint creation, `Verify sample` with a wrong signature (expects a reason, not a secret), and the rejection log. API-level checks run against the dev stack: a shell loop hits a capped route to collect a real `429` with headers; a keyed write is replayed with the same and a changed body; a signed webhook is posted to a declared intake endpoint through the mock receiver with four variants (valid, tampered, stale, replayed); an oversized body is posted; a mock provider is flipped to fail so the breaker opens, probes, and closes after a recovery.

The same pass asserts the counters move in Redis and the rollup rows appear, that no response body or log line contains the fixture signature secret, and that a second identical keyed write after the TTL executes normally. The visual check must see: refusals charted with real numbers, breaker chips distinguishable without colour, readable tables at 1280 px, card layout under 640 px, and no raw Rust error text surfaced as a toast.

### Slices

1. **Limits + 429 semantics.** Policy model, Redis token buckets, middleware wiring, headers and error code, refusal rollup and event aggregation, `/settings/reliability/limits` and the dry-run tool. *Done when:* a scripted loop gets a real `429` with correct headers and the screen names the winning policy.
2. **Idempotency.** Key store, fingerprinting, replay, conflict and in-progress semantics, response capture with the size cap, release action, screen. *Done when:* a replayed keyed write returns the original response with zero duplicate side effects and a changed body is refused.
3. **Retries + breakers.** Policy model and scheduler, jitter, persisted next-attempt times, dead letters, breaker state machine with half-open probing, events, both screens. *Done when:* a restarted worker resumes retries exactly once, and a tripped breaker fails fast, probes and closes on recovery.
4. **Intake guard.** Endpoint declarations, HMAC verification with tolerance and replay defence, size caps, sanitisation profile, rejection log, screen with `Verify sample`. *Done when:* the four fixture variants produce the documented codes and no rejected payload is echoed anywhere.

### Risks / notes

- Retries amplify outages: jitter is mandatory in practice (default `full`), per-subsystem budgets must respect the job's own deadline, and a breaker must exist on every outbound path that retries, or a retry storm turns a slow provider into a downed platform.
- Idempotency responses may contain sensitive data, so stored bodies pass the shared redaction helper, are capped, and expire; the store must never become a second request archive.
- The limiter depends on Redis. Instances must choose one documented behaviour per scope — fail open for availability or fail closed for strict protection — and the panel must state which mode is active rather than letting the choice hide in a config file.
- Default budgets can lock out a legitimate integration spike; the dry-run tool, the refusal rollup and a documented override path exist so an operator can widen one scope without disabling protection globally.
- HMAC tolerance is a security dial: too wide invites replay, too narrow breaks clients with clock skew. Rejections name the reason, and the rejection log is the evidence an operator uses to tune it.
- Sanitisation must stay narrow (control characters, unsupported content types, depth limits) — a "helpful" rewriting guard corrupts real payloads and is worse than no guard.
- Breaker thresholds need per-deployment tuning; the defaults ship conservative, every threshold is editable on the screen, and `Force open` exists for a maintainer who needs the platform to stop calling a provider *now*.
