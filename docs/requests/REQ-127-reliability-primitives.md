# REQ-127 — Reliability Primitives

> **Status:** in-progress (slice 3 is CODE COMPLETE — the scheduler, the retry + breaker routes (including the outbound gate that makes `provider_unavailable` a real refusal rather than a status a handler invents), and both screens shipped this tick, with the acceptance box for the scheduler ticked. The walk earned its keep a THIRD time: `a_sequence_is_due_once_and_then_stops_being_due` failed with 'a succeeded sequence is still being offered', and the machine was right and the SQL was wrong — `where next_attempt_at is not null` sat in the SAME query as the `distinct on`, so a finished sequence's terminal row was filtered out FIRST and 'newest per subject' then resolved to the last row still owing an attempt. A delivery that succeeded on attempt four would have been offered a fifth: a DUPLICATE SEND, from the subsystem whose whole purpose is to prevent one. `due_count` carried the same shape and had the same bug. Both fixed by picking the newest row in a subquery and filtering it outside. Gates: `omnion-reliability --lib` 116/0, `omnion-api --lib` 281/0, `pnpm typecheck` clean, and the eight scheduler walks 8/0 against `omnion_w6_dev` (7 green + 1 red on the first run, 8 green after the fix). STILL OWED for slice 3: the focused QA browser pass — `QA_ONLY=reliability-retries,reliability-breakers` — which is now written and the last gate before the intake guard. It is NOT a skipped-on-purpose pass: the run is launched and correctly QUEUED behind `qa-slot.sh`,
whose live holder is w3's own pass (verified by reading the holder's pid and its /proc cwd, not by
the command's exit code). w6's API came up healthy on :18085; the admin dev server has not been
given the slot yet. The stale `omnion-qa-admin-w6` pm2 logs from 05:10 show `ENOSPC` in Turbopack
and a `.next/dev` deleted mid-run — that is a FOUR-TICK-OLD run of mine, not this one, and its
conclusion is recorded rather than reused: a dev-server build that dies on a full `/` needs its
own `.next` cleared before the retry
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

- [x] `database/migrations/0028_reliability.sql` applies on a fresh and a populated database, and its down script reverses it. *(Shipped as `0162_reliability.sql`, the slot above the shared high-water mark at write time. Verified on a scratch database: the up half creates all nine tables, the commented reversal drops all nine, and — the reason the reversal is COMMENTED — `Db::migrate` executes a file's live statements on apply, so a down script written as live SQL would drop what it had just created and record a success doing it. That was a defect in the first draft, caught by the migration test rather than by reading the file.)*
- [x] Login, password-reset and public form routes ship with conservative default budgets. *(Migration `0165_reliability_default_budgets.sql` seeds four rows, marked `is_default` so `store::delete_policy` DISABLES rather than removes them: sign-in 10/min +2 burst, password-reset 3/hour with no burst, the public subtree 120/min +20, and a per-user 600/min on the authenticated API. The last one is the row that makes the `user` scope reachable at all — without it a deployment has no user budget and the screen's scope dropdown offers a scope nothing can ever spend. Applied twice against a scratch database: four rows after two applies.)*
- [x] Replaying a keyed write with the same key and body returns the stored response with `Idempotent-Replay: true` and does not execute the handler twice (asserted by counting side effects). *(Over HTTP against the real router: `a_replayed_key_returns_the_first_response_and_the_handler_runs_once` sends the same key and the same body twice, asserts the replay carries `Idempotent-Replay: true` and the ORIGINAL execution's request id, and then counts `workflows` rows — one. The count is the assertion and not the status code, because a replay that re-ran the handler and returned the same body is indistinguishable from a correct one by status alone.)*
- [x] The same key with a different body is `409 idempotency_conflict`. *(Over HTTP: `a_changed_body_is_a_conflict_and_runs_nothing` asserts the code, asserts the refused request created NO row, and asserts the `reliability.idempotency.conflict` event reached the bus **without** the body in its payload — an event payload lands in every webhook the platform writes.)*
- [x] A replay while the original attempt is running is `409` with `Retry-After`, and resolves once the original completes. *(Over HTTP: `a_replay_while_the_first_attempt_runs_is_a_409_with_a_wait` claims the key through the store — which is what a running handler leaves behind — asserts `409 idempotency_in_progress` **with** a `Retry-After`, asserts the second attempt created no row, then commits the first attempt and asserts the same key now replays.)*
- [x] A keyed request that is refused by a permission check never consumes an idempotency key. *(Enforced by POSITION, not by a branch: the keyed layer is installed INSIDE the permission guard, so a refused request never reaches the code that inserts a row and there is nothing to roll back. `a_refused_request_never_consumes_a_key` asserts both halves — the table holds no row after the refusal, and the SAME key still works for a permitted caller. Rolling back would have been a second bug: it would let a refused request delete the winner's `in_progress` row, which belongs to a different request running concurrently.)*
- [x] Expired keys are pruned, and an in-progress key past the execution deadline is released so it can be retried. *(Both halves proved: `an_expired_key_is_freed_and_taken_by_the_next_caller` (the upsert takes an expired row over in ONE statement — deleting first would open a window where two callers both see no row), `an_in_progress_key_is_released_and_then_runs_again` and `a_completed_key_is_never_released` (releasing a completed key would destroy a real stored response). This walk also found that `decide` mapped a `failed` key to `ReturnStored` — a `200` with a NULL body for a write that never happened; it returns `Proceed` now.)*
- [x] A `429` refusal carries `Retry-After`, `X-RateLimit-Limit/Remaining/Reset` and the standard error code. *(Proved over HTTP against a live router: a request past the ceiling gets `429` with `Retry-After`, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` and the `rate_limited` code. The refusal names its own document through `details.limiter`, because the gateway limiter sits in the same chain and an unattributable `429` is a `429` an operator widens the wrong document over.)*
- [x] A served request carries the same headers, so a client does not have to spend the ceiling discovering it. *(This is the one this tick found: it had never run, because the previous tick's link step died on a full `/dev/shm` before a single assertion executed. `decide_request` returned `Option<ApiError>`, so the allowed path computed a verdict, spent the budget, and dropped the verdict — `apply_headers` was written and documented for exactly that path and was reachable only from a refusal. It now returns a `Decision` enum carrying the verdict and the winning policy. `Unlimited` is returned explicitly for "no policy" rather than a bare "proceed", because that variant carries no number and is what stops the header publishing `Limit: 0` for a deployment nobody capped.)*
- [x] Limits apply per user, per organization, per IP and per route; the most specific policy wins and the dry-run endpoint names it. *(The dry-run resolves through the same `pick`/`decide` the middleware calls and does not spend the budget it measures, so the screen cannot drift from the refusal a caller is actually seeing. It is a SERVER call, not a client-side reimplementation: a local copy agrees on the day it is written and disagrees the first time somebody tunes a limit. Specificity is the scope list's order, so "most specific wins" is a property of the data rather than of the resolver.)*
- [x] A burst above the configured burst allowance is refused, and refusals roll up into one entry per window rather than one per request. *(The ceiling is `limit + burst` inside one window and is carried on the row, so a table never makes the reader do the addition. The rollup's single-row-per-window is enforced by a `unique nulls not distinct` constraint and the emission hangs off the upsert asking PostgreSQL `xmax = 0` whether it created the row — so "one aggregated event per window" is a RETURN VALUE rather than a promise in a comment. Note the deliberate deviation, stated rather than implied: the counter is a FIXED window, not a token bucket with a sliding log, because a sliding log keeps every timestamp of every request in the window, which is unbounded memory under exactly the load the limiter exists to survive.)*
- [x] A retry policy with full jitter produces a spread of delays across attempts in a distribution test, and a policy of `none` is deterministic. *(Proved in `retry.rs`'s own unit tests, before this tick and independent of it: `full_jitter_spreads_the_delays_while_none_is_deterministic` draws a thousand samples per attempt and asserts both the spread and the exact repeatability of `none`. The point of `delay_for` taking an explicit `draw` rather than reading a random source is that this claim is testable at all — "jitter is on" is a claim a constant implementation passes.)*
- [x] Retry attempts survive a worker restart (next-attempt time is persisted, no double execution). *(Over a real database: `a_restart_resumes_from_the_persisted_next_attempt_time` writes the ledger, then drops every piece of in-process state it holds and asks a fresh connection what the next attempt time is — so the walk proves persistence rather than a field a scheduler owns. The attempt COUNT is asked the same way, because a scheduler that counts in memory loses the count with the process. `record_outcome` writes `next_attempt_at` in the SAME statement as the outcome; writing them separately opens a window where a crash between them loses the retry entirely.)*
- [x] A non-retryable error class is not retried; an exhausted delivery produces one dead letter with the full timeline and a working `retry now`. *(Three walks. `a_non_retryable_failure_never_becomes_a_dead_letter`: a 422 ends the sequence on attempt one with no delay and no dead letter — a permanent failure is not an exhausted one. `one_exhausted_sequence_writes_exactly_one_dead_letter`: asserted by COUNTING the flagged rows, not by checking the returned row is flagged, because a store that wrote the flag twice still returns a flagged row; the timeline is then read back and ordered `[1, 2, 3]`, ordered by `attempt` rather than `created_at` because `now()` is transaction-stable and a time-ordered timeline is one the reader cannot reproduce. `retry_now_adds_an_attempt_and_leaves_the_failure_in_the_timeline`: the action APPENDS rather than clearing the flag, because a timeline the retry erased is a sequence with no failure in it and one unexplained success.)*
- [x] A breaker opens after the configured failure count within its window, fails fast with `provider_unavailable`, and emits one opened event. *(Partly the state machine, proved in `breaker.rs`, and partly the store: `a_breaker_trips_on_failures_and_probes_its_way_back` asserts the trip, that a reload of the persisted row is still `open`, and — by counting — that the trip produced exactly ONE `to_state = 'open'` event. The `provider_unavailable` refusal itself is the `admit` path in `breaker.rs` and is not exercised by a store walk: it needs a caller, and no outbound subsystem in the platform routes through a breaker yet. That is the honest limit of this tick's evidence and it is the next tick's work alongside the scheduler.)*
- [x] In half-open, the configured number of probes decides: successes close it, a failure reopens it with an incremented trip count. *(The walk this tick CORRECTED rather than merely wrote: it first asserted that the success that moved the breaker to `half_open` counted toward `success_threshold`, and it failed with `left: half_open, right: closed`. The machine was right and the walk was wrong — entering half-open RESETS the counter, so the probe that opens the window is not one of the successes that close it, and a threshold of two must not be satisfied by a provider that has recovered exactly once. The walk now asserts the counter is zero on entry, that one success of two is still probing, and that the second closes it.)*
- [x] A restart does not reset an open breaker, and `Reset` and `Force open` both require confirmation and write audit rows. *(`a_forced_open_breaker_survives_a_reload_and_only_a_reset_closes_it`: the row is reloaded from the database rather than kept, then a SUCCESS is fed to the machine and the state is asserted still `open` — a state recomputed from `opened_at` would have closed itself once the cooldown elapsed with nobody watching. `forced_open` is a COLUMN the machine checks before every other rule, and a reset that only wrote `state = 'closed'` would leave the flag set so every later observation refused; the walk asserts the flag is cleared. Both manual actions append to `breaker_events` through the same statement an automatic trip uses, and the reset's row says `reset:` where a trip says nothing, so an operator reading the timeline can tell the two apart. The CONFIRMATION half is a screen behaviour and is unproven: this is store and machine only, and the confirmation dialog ships with the breaker screen.)*
- [x] The scheduler reads the persisted next-attempt time and resumes it, and two workers cannot run the same attempt. *(Slice 3's scheduler shipped this tick (`7adf181f`) — `crates/reliability/src/scheduler.rs` plus migration `0184_reliability_scheduler_lease.sql`. Four decisions are properties of the schema rather than promises: the due scan is `distinct on (subject_kind, subject_id) … order by attempt desc`, so a three-attempt sequence is offered ONCE and claiming the middle of it would replay an attempt the timeline already shows; the claim is a compare-and-swap on ONE `row_id` and not on a `(subject_kind, subject_id, attempt)` predicate, because PostgreSQL has no `UPDATE … ORDER BY` and the predicate version would claim every matching row — which is how a scheduler writes attempt 4 three times; the claim is a LEASE rather than a lock, because a permanent claim turns a worker that dies mid-attempt into a job nobody runs again, which is the one failure this subsystem exists to prevent; and `elapsed_ms` is measured from the sequence's FIRST row, because a restarted worker handing every resumed sequence a fresh budget is how a policy outlives the job it belongs to. Eight walks green: `a_second_worker_cannot_claim_a_sequence_the_first_one_holds` proves it by LOSING a claim and then COUNTING the live claims, because a scheduler that wrote the flag unconditionally would also answer `false` without ever excluding anybody; `an_expired_claim_is_taken_over_and_a_live_one_is_not` carries the counter-assertion that a claim inside its lease is NOT offered, since "an expired claim is taken over" is trivially satisfiable by a claim that is never stored.)*
- [ ] An inbound webhook with a valid signature is accepted; a tampered body, a signature outside the tolerance window and a replayed signature are each rejected with the documented code and a rejection row.
- [ ] A payload above the size cap is refused before authentication with `413 payload_too_large`.
- [ ] Sanitisation removes control characters from headers and stored strings without altering a legitimate JSON payload (fixture comparison test).
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The walkthrough visits `/settings/reliability` and its five sub-screens, and clicks: policy create and edit, the dry-run evaluate (expects a named winning policy and a remaining budget), the retry delay preview at factor 1.0 and 3.0, a dead-letter `retry now`, a breaker `Reset` and a `Force open` with a reason, intake endpoint creation, `Verify sample` with a wrong signature (expects a reason, not a secret), and the rejection log. API-level checks run against the dev stack: a shell loop hits a capped route to collect a real `429` with headers; a keyed write is replayed with the same and a changed body; a signed webhook is posted to a declared intake endpoint through the mock receiver with four variants (valid, tampered, stale, replayed); an oversized body is posted; a mock provider is flipped to fail so the breaker opens, probes, and closes after a recovery.

The same pass asserts the counters move in Redis and the rollup rows appear, that no response body or log line contains the fixture signature secret, and that a second identical keyed write after the TTL executes normally. The visual check must see: refusals charted with real numbers, breaker chips distinguishable without colour, readable tables at 1280 px, card layout under 640 px, and no raw Rust error text surfaced as a toast.

### Slices

1. **Limits + 429 semantics.** Policy model, Redis token buckets, middleware wiring, headers and error code, refusal rollup and event aggregation, `/settings/reliability/limits` and the dry-run tool. *Done when:* a scripted loop gets a real `429` with correct headers and the screen names the winning policy.
2. **Idempotency.** *Core shipped* (`efbae1b`): `decide` (the five-sentence contract in one
   function), a fingerprint that canonicalises key order recursively so a client that serialises
   the same object twice replays rather than conflicting, `StoredResponse::seal` with the
   oversize rule stated (**never truncate** — a truncated replay is a silent corruption), and
   `release_stale` so a crashed attempt cannot block a key forever. Table in `0162`. **Store
   shipped** (`d167629a`): `idem_store`, 11 walks including one where exactly one of eight
   concurrent claims wins. **The request path and the screen shipped** (`bbf97947`):
   `idempotency_middleware` on `POST /api/v1/automations`, the three panel routes, the
   `/settings/reliability/idempotency` screen, and five HTTP walks. Two more decisions are
   written down rather than implied: a response that **cannot** be replayed (past the inline cap
   with no object-store reference) is recorded `failed` rather than stored, because a truncated
   replay is a lie and `failed` is the one state that says "run it again"; and the stored header
   subset is `location`, `etag` and the original request id, with `set-cookie`,
   `www-authenticate` and every `X-RateLimit-*` excluded **on purpose** — a replayed session
   cookie is a second authentication written by a request that already finished, and the
   rate-limit layer sits outside this one, so a replay gets fresh numbers whatever the store
   holds.
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

### Slices — progress

1. **Limits + 429 semantics.** Policy model, Redis token buckets, middleware wiring, headers and
   error code, refusal rollup and event aggregation, `/settings/reliability/limits` and the
   dry-run tool. *Done when:* a scripted loop gets a real `429` with correct headers and the
   screen names the winning policy.
   — **The decision layer shipped** (`efbae1b`): `crates/reliability::limits` with
   `LimitPolicy`, `Subject`, `pick` (specificity is the scope list's *order*, so "most specific
   wins" is a property of the data rather than of the resolver), `decide` (pure; the same
   function the panel's dry-run calls), `route_matches` and `should_emit` (one event per
   window, structurally, because the emission is on the rollup's first write). Migration
   `0162_reliability.sql` with `rate_limit_policies` and `rate_limit_refusals`, both carrying a
   `unique nulls not distinct` constraint — which is what makes "one policy per scope" and "one
   refusal row per window" properties of the schema rather than promises. **Verified on a scratch
   database:** the file applies, a second `null/null` policy row is refused, a second refusal row
   for the same window is refused, and the reversal drops all nine tables.

   — **The request path shipped this tick** (`limiter_redis.rs`, `store.rs`,
   `reliability_middleware.rs`, `routes/reliability_limits.rs`, migration `0165`): the Redis
   counter, the policy store, the middleware, the `429` with `Retry-After` and the three
   `X-RateLimit-*` headers, the panel's policy CRUD, and the dry-run that resolves the same
   policy the middleware resolves **without spending the budget it measures**.

   **Two things this slice is deliberately NOT, both stated rather than implied.** The counter is
   a **fixed window**, not a token bucket with a sliding window: the request says "token-bucket …
   with a sliding window", and a sliding log keeps every timestamp of every request for the
   window, which is unbounded memory under exactly the load the limiter exists to survive. What is
   shipped keeps the burst allowance — `limit + burst` is headroom inside one window, which is
   what a bucket's burst bucket provides — and what it costs is a peak of 2× the limit across a
   window boundary. Every fixed-window limiter makes that trade; writing it down is what keeps the
   request's wording from becoming a promise the code cannot keep. And a **`route`-scoped policy
   cannot be enforced by this layer**, because it runs before the router publishes
   `MatchedPath`: the `PolicyBody` therefore carries an `enforced_here` flag and the dry-run says
   so in its own answer, because a row that is stored but never spent is the "documented but
   unreachable" shape this request's sibling produced four times.
2. **Idempotency.** *Core shipped* (`efbae1b`): `decide` (the five-sentence contract in one
   function), a fingerprint that canonicalises key order recursively so a client that serialises
   the same object twice replays rather than conflicting, `StoredResponse::seal` with the
   oversize rule stated (**never truncate** — a truncated replay is a silent corruption), and
   `release_stale` so a crashed attempt cannot block a key forever. Table in `0162`. Store,
   middleware and screen to come.
3. **Retries + breakers.** *Core shipped* (`efbae1b`): `retry::delay_for` takes an explicit
   draw so full jitter is a distribution test and none is a determinism test; `classify` makes
   4xx permanent except `408`/`429`; `next_attempt` derives the outcome and the dead-letter flag
   from the policy rather than taking them, and its budget check is **cumulative**. `breaker` is
   the state machine with counters that persist across a restart and a `forced_open` flag that
   no success, cooldown or probe can clear. Tables in `0162`. Store, loop and screens to come.
4. **Intake guard.** *Core shipped* (`efbae1b`): `evaluate` in the order the request requires
   (size → content type → signature presence → timestamp → replay → constant-time compare →
   sanitise), `sanitize` asserted **byte-identical on a real payload** because the request's own
   risk note says a rewriting guard is worse than none, and `verify_sample` — deliberately the
   same function the request path uses, since a tester with its own signature check would tell
   an operator "valid" for a body the platform would refuse. Tables in `0162`. Store and screen
   to come.

### Six defects the tests caught while writing this

Recorded because each one was a claim a doc comment was already making, and the code was not
doing: the breaker had **no failure counter at all** (its threshold could never be reached, so
it never opened, and the success path cleared the window, so only a total outage would have
opened it); the half-open path **never counted successes**, so `success_threshold: 3` closed on
the first probe; **event names were emitted by array index** and index 4 was `retry.exhausted`,
so a breaker that opened announced a dead letter; a **route-scoped budget matched a subject with
no route**, spending a page-load budget on background work; the sanitiser **walked the input with
the output's cursor**, so one removed control character silently disabled every later check; and
the **down script was live statements**, so `Db::migrate` applied the file and then dropped every
table it had just created.

### Slices — progress, second pass

**The tick that could not read its own gate.** The previous tick ended with the HTTP walk
recorded as *not run* because the link step died with `signal 7 [Bus error]` while `/dev/shm`
was 100% full. That is the honest report and it was correct, but it means every claim in the
slice rested on unit tests and a scratch database, and a test that has never executed is a
comment. The first thing this tick did was run it.

**The box was the blocker, and `/` was the answer.** Eight writer worktrees each park a
multi-gigabyte `target/` in one 32 GB tmpfs. My own was 3.8 G; the tmpfs had 562 MB free. The
root filesystem had 19 G free the entire time. Moving this worktree's target to
`/opt/omnion-w6-target` took `/dev/shm` from 562 MB free to 4.2 GB free — which unblocked seven
sibling writers as a side effect, and dropped the box's load from 87 to 14. The invariant that
puts `target/` on tmpfs is right (the loop image is at 94%); it just needed somebody to check the
other filesystem before blaming the build.

**Five defects, and every one of them was a documented promise the code was not keeping.**

1. **A served request carried no headers.** `decide_request` returned `Option<ApiError>`, so the
   allowed path computed a verdict, spent the budget, and threw the verdict away.
   `apply_headers` was written and documented as "split out so the ALLOWED path can carry them
   too" and was reachable only from a refusal. Fixed by returning a `Decision` enum that carries
   the verdict and the winning policy.
2. **The `429` carried no headers either.** The one response where the caller most needs the
   ceiling, the reset and the deciding policy was the only response the limiter left bare. This
   was true *before* the tick and its assertion had never run; fixing half the contract exposed
   it, which is the ordinary way these two are found.
3. **One dead Redis socket disabled the limiter.** A pooled connection handed out already dead
   fails with `broken pipe` on the first write and nothing wrong with Redis. The counter became
   unreadable, an unreadable counter means **fail open**, and the only symptom was real traffic
   that was never limited. `count` now retries once on a fresh connection and reports a persistent
   failure rather than resolving it.
4. **`Verdict::Limited` could not say what was left.** It carried `ceiling` and `retry_after` but
   no `remaining`, so each consumer had to decide what a refused caller's remainder is. The walk
   expected `X-RateLimit-Remaining: 0` and the variant could not produce it.
5. **`pick` ordered by scope only.** "The most specific matching policy wins" means *narrower*
   wins inside a scope, and two rows in one scope had the same score — so `priority` silently
   became the specificity rule, and a broad default could outrank the narrow row written to
   override it. Both rows render as configured, so nothing on screen contradicted the outcome.

**Two harness defects that produced product-shaped symptoms**, recorded because both cost real
time this tick: the suite's counter clearing swallowed a failed connection with `if let Ok(..)`,
leaving the previous test's counter in Redis and making every later assertion read a refusal's
numbers; and all eight tests shared one `CLIENT_IP`, so they shared one counter — which is
*correct* product behaviour (the counter key carries no policy id on purpose, or raising a limit
would hand a subject a fresh budget and turn the screen into a bypass) and wrong harness design.
Per-test addresses, not a key change.

**The screen shipped** (`8b3ba7d`): `/settings/reliability/limits` beside
`/settings/security/rate-limits` on purpose, because both limiters are live in the same chain and
an operator who cannot tell which document refused a caller widens the wrong one. It renders the
four distinctions the API's enum exists to keep apart — a stored policy is not an enforced one,
`Unlimited` is not zero remaining, `Uncounted` is not `Allowed`, and a `429` from this layer is
not a `429` from the gateway — and the dry-run is a server call so it cannot drift from the
resolver. The walkthrough route is registered.

**Suite status, stated honestly: 6/8 in one sequential run, 8/8 individually.** The two
stragglers both pass alone and fail only in sequence, and the run times out at 700 s on a box
with seven other writers — `sign_in` alone costs ~9 s (an organization, a user, a role binding
and a session, per call) and four walks call it. That is a scheduling problem in the harness, not
a product defect, and it is recorded as open rather than dressed up as green. Next tick: one
shared session per scope so a full sequential run fits the tick's budget, then run it end to end.

**Two more assertions that turned out to be wrong about the product, both corrected toward the
product.** The rollup walk asserted "exactly one row for this subject", but the rollup is keyed
`(scope, target, route, window_start)` — a subject refused in three windows correctly has three
rows, and the assertion only passed on a fresh database. It now asserts one row **per window**,
scoped to the window the walk just wrote, plus that older windows are retained. And the dry-run
walk expected `count: 2` after two spending requests, where the third is the dry-run's own
request: the middleware counts it before the handler peeks, exactly as it counts every other API
call. That is the platform rate-limiting its own tool, which is right. "Does not spend the
budget" is now asserted on the DELTA between two tool calls — a tool that consumed what it
measured would show 2, one that is merely counted as a request shows 1.

**One thing I wrote and then deleted before committing.** A `SCAN`/`DEL` over `omnion:rlx:*` to
clear the suite's counters, which would have wiped seven sibling writers' budgets and every
production app's on the shared Redis. It is the same class of mistake as touching another
writer's `target/`, and the policy purge is keyed on this suite's own `w6 %` name for exactly
that reason.

**Next.** (1) The refusal rollup's chart is on the screen but the QA browser pass still has not
run against it — the walk is a `qa-slot.sh` acquisition plus a box with room to breathe, and a
screen nobody has opened in a browser is not finished. (2) Split the suite's `sign_in` cost so
a full sequential run fits the tick and close the two stragglers. (3) Then REQ-127 slice 2
(idempotency): `decide`, the fingerprint and `StoredResponse::seal` are in; the store, the
middleware and the screen are not.

### Slices — progress, third pass: what the two stragglers actually are

Three ticks recorded the suite at "8/8 individually, 6/8 in one sequential run" and named the two
failures as *`sign_in` cost against seven sibling writers*. That was a reasonable reading and it
was **wrong**. This pass measured both, on an idle box where the whole suite runs in 125 s, and
neither failure is a scheduling artefact.

**The evidence, in order.**

- A full sequential run on an idle box: **7/8, then 6/8** — the two names are
  `a_saved_policy_takes_effect_on_the_next_request_without_a_restart` and
  `refusals_roll_up_into_one_row_per_window`. Timing is not the variable; the failing PAIR is.
- The rollup one, isolated: the row said `1` for 3 real refusals. Reproduced the `insert … on
  conflict … do update … returning xmax = 0` in `psql` by hand: it counts 1 → 2 → 3 correctly. So
  the store is right, and the walk was reading a different **window** than the one it wrote — the
  900-second bucket rolled mid-burst, which is the table doing its job (one row *per window*), not
  a defect. The walk recomputed `window_start` from the clock *after* the burst.
- The policy one, isolated with the response printed: the first request is `200` with **no
  `X-RateLimit-*` headers at all**, while the second carries the right policy id and
  `remaining: 0`. `apply_headers` withholds the headers on exactly one verdict — `Uncounted` — and
  `enforce` produces it when the winning policy's subject has **no key**, which for a `user`-scoped
  policy means `resolve_user_id` returned `None`.

**The defect, and it is a product one, in `apps/api/src/reliability_middleware.rs`.**
`resolve_user_id` is written as `resolve_session(...).await.ok().flatten()?`. `.ok()` turns
*every* failure into `None`, and the doc comment above it says the `None` is deliberate: "a request
whose session cannot be resolved … spends the IP budget in the meantime". That reasoning is right
about the IP budget and wrong about the user budget, because the two failures look identical
downstream:

- a session that genuinely does not resolve should spend the **IP** budget and nothing else — the
  request is unattributable and that is a real condition;
- a session that failed to resolve because PostgreSQL was busy — a pool `acquire_timeout` of 5 s on
  a box at load 250 — produces the **same** answer, and the signed-in caller's request is then
  served **uncounted**, with no header saying so, and its budget is gone.

A caller that cannot spend its budget is the failure this whole slice exists to prevent, and it
arrives from a saturated connection pool rather than from an outage anyone would see. The fix is
to stop treating "could not ask" as "does not exist": `resolve_user_id` needs to distinguish the
two, and a request that failed to resolve its session must not be answered `Unlimited` quietly.
This is **not fixed in this tick** — the diagnosis is measured and the fix has not been written,
so it is recorded as open rather than claimed.

**Two harness defects found and fixed on the way, both committed with the suite.**

- `install_suite_policy` called `layer.reload(vec![saved])`, and `reload` REPLACES the list, so
  installing an `ip` policy silently evicted every other budget the process was enforcing. A walk
  that installs a policy must not disable the budgets around it. The merging alternative was
  written and measured too, and is **also wrong**: the layer is process-wide and shared by every
  walk in the binary, so a merge inherits the previous walk's rows, a stale tighter ceiling
  outranks this walk's, and 5/8 walks then failed with a count of zero. Both are recorded because
  both fail, and the reason one is right is not obvious from the code.
- The walk asserted only that the *second* request was refused. A first request served **without
  being counted** passed that line, the second then arrived as the counter's first, and the walk
  reported "the saved policy did not take effect" — three lines later, pointing at the store and
  the reload, neither of which was wrong. The served request's own budget is now asserted, and
  that is the assertion that located this defect.

**One measurement worth keeping, because it is the most expensive kind of green.** The obvious
regression test for the suspected cause — a `RedisClient::forget_connection` eviction, so a
limiter's "retry on a fresh connection" is genuinely fresh — **passes with the fix deleted**. A
`ConnectionManager` heals a dropped socket transparently, and the server accepted 14 connections
for 2 attempts, so neither the cache state nor the accept count can tell the fixed code from the
broken one. That fix was therefore **not** written: it was a hypothesis, it was tested, and it was
wrong. A green regression test for a defect it never reproduces is worth less than no test.

**Not done, and named.** The QA browser pass against `/settings/reliability/limits` still has not
run — the `qa-slot.sh` place was held by a sibling for the whole tick and `uptime` read 256 during
the investigation. A screen nobody has opened in a browser is not finished, and that is unchanged.

**Next.** (1) Fix `resolve_user_id` so a session that could not be asked is not a session that does
not exist, and make the served request's headers the thing a walk asserts. (2) Get the QA slot and
run the browser pass against the limits screen. (3) Then REQ-127 slice 2 (idempotency): `decide`,
the fingerprint and `StoredResponse::seal` are in; the store, the middleware and the screen are not.

**The walk did not run, and the reason is worth more than the slice.** `apps/api/tests/
reliability_limits.rs` reaches the link step and the link dies with
`collect2: fatal error: ld terminated with signal 7 [Bus error]`. At that moment `/dev/shm` read
**100% full with 60 KB free of 32 GB** and `uptime` read **load average 214** with five `rustc`
processes alive: eight writer worktrees park a multi-gigabyte `target/` in that one tmpfs and
their combined target directories no longer fit. The three boxes this tick CAN quote are the ones
that ran — 110/110 unit tests, `0162` applied and reversed on a scratch database, `0165` applied
twice with four rows after two applies — and the walk is recorded as **not run** rather than
passed. A saturated box fails a build with a signal that does not name the cause, and a red gate
reported as a test failure sends the next operator to the wrong file.

### Slices — progress, fourth pass: three answers, and a claim the index arbitrates

**The straggler is fixed, and the fix was already half-written.** The previous tick's diff had the
`Result<Option<Uuid>, IdentityError>` signature, the call site's downgrade branch and the new test —
but the function body still ended in `.ok().flatten()`, so nothing could ever produce an `Err` and the
downgrade was unreachable. The signature changed and the behaviour did not, which is the shape a
half-finished tick leaves: it compiles, it has a test that looks aimed at the defect, and the test
was red for exactly the right reason. The mutation proof came free — **the test failed against the
previous body and passes against this one**, which is the only kind of green worth quoting.

**`resolve_user_id` has three answers now.** No cookie is `Ok(None)` — a fact about the request, not a
lookup that failed. A found session is `Ok(Some(_))`. A database that cannot be asked is `Err`, and it
travels to `decide_request`, which logs it, still spends the address budget (the old comment was right
about that half) and downgrades the served verdict to `Uncounted`. **`Unlimited` was the wrong
verdict**, not because it is wrong on its own but because it means "no budget is written for this
scope" — a statement about the deployment — while `Uncounted` means "this request was not counted",
which is the truth and is non-authoritative, so `apply_headers` withholds the numbers rather than
publishing a measurement nobody took. The downgrade only fires when an enabled user-scoped policy
existed, because a deployment with no user budget lost nothing and reporting an anomaly that cannot
happen trains an operator to ignore the log line.

**Slice 2's store, and the four defects its walks found.** `crates/reliability/src/idem_store.rs`
claims a key with one `INSERT … ON CONFLICT DO UPDATE … WHERE` and reads `rows_affected`; a
read-then-write cannot work because the read is not part of the decision the database makes.

1. **The takeover predicate compared the wrong bound** — `where idempotency_keys.expires_at <= $8`,
   where `$8` is the expiry the statement was about to *write*. Every live row satisfies that, so the
   `do update` fired on every claim and **eight of eight concurrent claims returned `Claim::Claimed`**.
   The unique index never arbitrated, because the upsert had already converted the conflict into an
   update. This is the single most valuable thing in the slice: it is invisible to review, it is
   invisible to every unit test, and one walk of eight simultaneous claims found it on its first run.
2. **`jsonb` on both sides.** `response_body` is `jsonb` in the migration and `String` in
   `KeyRecord`, so the write refused with `42804` and the read with a `ColumnDecode` — every replay of
   a row carrying a body failed while every claim-only test was green. Two of the three halves of the
   store are storage, and both directions need the cast.
3. **`decide` mapped a `failed` key to `ReturnStored`** — status 200, NULL body, for a write that
   never happened. A client told a write succeeded that did not, which is the worst answer the
   subsystem can give. It returns `Proceed`, and the upsert takes a `failed` row over, so the retry
   runs and the row is the evidence rather than a permanent silent refusal.
4. **Two of my own tests asserted states the constructor cannot produce.** Both were resolved by asking
   which side was right before editing anything: the release test expected a retry to *find* the
   released row, while the takeover makes it a fresh claim (product right, test wrong), and the cap
   test hand-built an oversized inline body that `seal` always drops to `None` (test wrong). Two
   tests and two code fixes in one slice, and the mutations are recorded in the commit messages.

**Where the slice actually stands.** The store is the *persistence* half. The middleware that claims
after permission, the `Idempotent-Replay: true` header, the `409 idempotency_conflict`, the two
events and the screen are **not written** — so the five idempotency acceptance lines are annotated
rather than ticked, and each annotation names which half is proved. The QA browser pass against
`/settings/reliability/limits` **still has not run**: the single slot belonged to the main writer's
live pass for the whole tick (holder cwd verified as `/mnt/apopic/omnion`, alive, 46 s old at first
check), and taking it is not mine to do.

**Env.** `/` was at **100% with 9.9 MB free** on entry and this worktree's `target/` sits on it.
Reclaiming only this worktree's own stale walk binaries from `debug/deps` — 5.7 GB, 38 executables,
the API binary and the two this tick runs explicitly kept — took it to 95%. That reclaim is safe
because it is name-shaped and confined to one worktree's own target; the six siblings' targets are
never touched, and neither is anything under `/mnt/apopic` (83%).

**Next.** (1) Take the QA slot and run the browser pass on the limits screen. (2) Slice 2's
middleware and screen. (3) Slices 3 and 4.
