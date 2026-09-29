-- REQ-127 slice 1 — platform-wide rate limits, plus the tables slices 2–4 build on.
--
-- Additive, per docs/05-VERSIONING.md: new tables only, no existing table is touched, no
-- column is dropped. The down script reverses exactly this file and nothing else.
--
-- **Slot choice.** Every writer shares one PUBLIC repository, so a migration number is a
-- SHARED namespace: "the next free number in my worktree" is a number another worktree is
-- about to take. 0162 is the first number above the high-water mark across all ten writer
-- worktrees (0161 in omnion-w5) at the time of writing, and every list in the crate's
-- vocabulary module is mirrored in the comments below so the two cannot drift.
--
-- The five error codes the request declares stable are written here as comments on the
-- tables that produce them, because a code that exists only in Rust is a code an integrator
-- cannot discover:
--   idempotency_conflict  -> idempotency_keys
--   rate_limited         -> rate_limit_policies
--   payload_too_large    -> intake_endpoints
--   signature_invalid    -> intake_rejections
--   provider_unavailable -> circuit_breakers

-- ---------------------------------------------------------------------------
-- Rate limits (slice 1)
-- ---------------------------------------------------------------------------

-- One scope's budget. `route_pattern` is a TEMPLATE (`/api/v1/posts/{id}`), never a literal
-- path, for the same reason the metric families use templates: a literal id in a unique key
-- is one policy per id.
create table rate_limit_policies (
    id            uuid primary key default gen_random_uuid(),
    name          text        not null,
    scope         text        not null check (scope in ('user', 'organization', 'ip', 'route')),
    target_id     text,
    route_pattern text,
    limit_count   int         not null check (limit_count > 0),
    window_seconds int        not null check (window_seconds between 1 and 86400),
    burst         int         not null default 0 check (burst between 0 and 10000),
    priority      int         not null default 100,
    is_default    boolean     not null default false,
    enabled       boolean     not null default true,
    created_by    uuid,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    -- One policy per scope/target/route. `nulls not distinct` so two rows for "every user, every
    -- route" collide instead of coexisting: without it PostgreSQL treats NULLs as distinct and
    -- the uniqueness this table exists to provide does not exist.
    unique nulls not distinct (scope, target_id, route_pattern)
);

-- The resolver walks the scope list most-specific-first, so the hot read is the enabled set.
create index rate_limit_policies_resolution
    on rate_limit_policies (enabled, priority);

-- ONE row per refused window. The unique constraint is what makes the request's "one aggregated
-- event per target and window rather than a per-request flood" a property of the schema: the
-- second refusal in a window updates this row instead of inserting another, so there is only
-- ever one row to emit from. `nulls not distinct` again, for a refusal with no target.
create table rate_limit_refusals (
    id               bigserial primary key,
    scope            text        not null check (scope in ('user', 'organization', 'ip', 'route')),
    target_id        text,
    route            text        not null,
    window_start     timestamptz not null,
    refusals         int         not null default 0,
    last_refusal_at  timestamptz not null default now(),
    unique nulls not distinct (scope, target_id, route, window_start)
);

create index rate_limit_refusals_window
    on rate_limit_refusals (window_start desc);

-- ---------------------------------------------------------------------------
-- Idempotency keys (slice 2)
-- ---------------------------------------------------------------------------

create table idempotency_keys (
    id               bigserial primary key,
    -- The endpoint FAMILY plus the subject. A key is scoped so one tenant's key cannot collide
    -- with another's, and so a key replayed against a different route is a conflict rather
    -- than a hit.
    scope            text        not null,
    subject_id       text        not null,
    key              text        not null check (char_length(key) between 1 and 255),
    method           text        not null,
    path             text        not null,
    request_hash     text        not null,
    state            text        not null default 'in_progress'
                     check (state in ('in_progress', 'completed', 'failed')),
    response_status  smallint,
    response_headers jsonb       not null default '{}'::jsonb,
    -- Inline only while the body fits the cap; the reference carries it beyond. A truncated
    -- replay would be worse than a refusal, so there is no truncation here at all.
    response_body    jsonb,
    response_body_ref text,
    replay_count     int         not null default 0,
    expires_at       timestamptz not null,
    created_at       timestamptz not null default now(),
    completed_at     timestamptz,
    unique (scope, subject_id, key)
);

-- The retention sweep reads this, and so does the panel's recent-keys list.
create index idempotency_keys_expires on idempotency_keys (expires_at);

-- A partial index, because the stuck-key sweep only ever looks at `in_progress` and a full
-- index would carry a completed row for every write the platform has ever done.
create index idempotency_keys_in_progress
    on idempotency_keys (created_at)
    where state = 'in_progress';

-- ---------------------------------------------------------------------------
-- Retry policies and the attempt ledger (slice 3)
-- ---------------------------------------------------------------------------

create table retry_policies (
    id               uuid primary key default gen_random_uuid(),
    subsystem        text        not null
                     check (subsystem in ('webhook', 'email', 'ai', 'workflow', 'integration', 'storage')),
    provider_override text,
    max_attempts     int         not null default 5 check (max_attempts between 1 and 20),
    base_delay_ms    int         not null default 1000 check (base_delay_ms >= 0),
    factor           numeric(4,2) not null default 2.00 check (factor between 1.00 and 10.00),
    jitter           text        not null default 'full' check (jitter in ('none', 'equal', 'full')),
    max_elapse_ms    bigint      not null default 3600000 check (max_elapse_ms >= 0),
    -- The error classes this policy retries. A class nobody lists is never retried, so an
    -- empty object is a policy that retries nothing, which is a legitimate choice.
    retry_on         jsonb       not null default '{}'::jsonb,
    enabled          boolean     not null default true,
    updated_by       uuid,
    updated_at       timestamptz not null default now(),
    unique nulls not distinct (subsystem, provider_override)
);

create table retry_outcomes (
    id              bigserial primary key,
    subsystem       text        not null,
    subject_kind    text        not null,
    subject_id      text,
    attempt         int         not null check (attempt >= 1),
    scheduled_at    timestamptz,
    executed_at     timestamptz,
    outcome         text        not null
                    check (outcome in ('succeeded', 'failed_retryable', 'failed_permanent', 'exhausted')),
    error_class     text,
    next_delay_ms   int,
    dead_letter     boolean     not null default false,
    -- The next attempt's time lives HERE, on the job row, not in a queue's head position: a
    -- restart reads this and resumes, rather than replaying an attempt or losing one.
    next_attempt_at timestamptz,
    created_at      timestamptz not null default now()
);

-- The dead-letter list is a small slice of this table, so the index is partial on it.
create index retry_outcomes_dead_letter
    on retry_outcomes (subsystem, executed_at desc)
    where dead_letter;

create index retry_outcomes_subject
    on retry_outcomes (subject_kind, subject_id, attempt);

-- ---------------------------------------------------------------------------
-- Circuit breakers (slice 3)
-- ---------------------------------------------------------------------------

create table circuit_breakers (
    -- The provider key, e.g. `openai` or `webhook:hooks.example.com`. The primary key IS the
    -- provider: one breaker per outbound destination, and no separate row to keep in step.
    key             text primary key,
    name            text        not null,
    failure_threshold int       not null default 5 check (failure_threshold between 1 and 100),
    window_seconds  int         not null default 60 check (window_seconds between 1 and 3600),
    cooldown_seconds int        not null default 30 check (cooldown_seconds between 1 and 3600),
    half_open_probes int        not null default 3 check (half_open_probes between 1 and 100),
    success_threshold int       not null default 3 check (success_threshold between 1 and 100),
    state           text        not null default 'closed' check (state in ('closed', 'open', 'half_open')),
    -- Deliberately durable, NOT derived from a timestamp. A restart reads `open` and keeps
    -- refusing, which is the whole point: a breaker that came back healthy after a deploy
    -- would send a burst at a provider that is already down.
    forced_open     boolean     not null default false,
    opened_at       timestamptz,
    state_changed_at timestamptz not null default now(),
    -- The counters `record` advances. A fixed window, not a sliding one: the cost of a sliding
    -- window is a per-call history read, and this module's job is to stop calling something
    -- that is down, where a trip one window early is the cheap direction to be wrong in.
    failures_in_window int      not null default 0,
    successes_in_half_open int  not null default 0,
    window_started_at  timestamptz not null default now(),
    trips_total       bigint     not null default 0,
    updated_by        uuid,
    updated_at        timestamptz not null default now()
);

create table breaker_events (
    id           bigserial primary key,
    key          text        not null,
    from_state   text        not null,
    to_state     text        not null,
    reason       text,
    failure_rate double precision,
    created_at   timestamptz not null default now()
);

create index breaker_events_key
    on breaker_events (key, created_at desc);

-- ---------------------------------------------------------------------------
-- The inbound intake guard (slice 4)
-- ---------------------------------------------------------------------------

create table intake_endpoints (
    id                uuid primary key default gen_random_uuid(),
    path              text        not null unique,
    name              text        not null,
    hmac_scheme       text        not null
                      check (hmac_scheme in ('sha256_hex', 'sha256_base64', 'sha1_hex')),
    signature_header  text        not null,
    timestamp_header  text,
    tolerance_seconds int         not null default 300 check (tolerance_seconds between 30 and 3600),
    -- A reference, never a value: the secret lives in the secret store and is re-resolved on
    -- `secret.rotated`, so rotating it is one operation rather than a walk over this table.
    secret_id         uuid references secrets(id) on delete restrict,
    max_payload_bytes int         not null default 1048576
                      check (max_payload_bytes between 1024 and 10485760),
    sanitize_profile  text        not null default 'strict'
                      check (sanitize_profile in ('strict', 'balanced')),
    enabled           boolean     not null default true,
    created_by        uuid,
    created_at        timestamptz not null default now()
);

create table intake_rejections (
    id          bigserial primary key,
    endpoint_id uuid references intake_endpoints(id) on delete cascade,
    -- The same vocabulary the wire answer uses, so an operator reading a row and an operator
    -- reading a `401` body are reading one list rather than translating between two.
    reason      text        not null
                check (reason in ('signature_missing', 'signature_invalid', 'timestamp_stale',
                                  'replay', 'payload_too_large', 'content_type_refused', 'malformed')),
    source_ip   inet,
    request_id  uuid,
    -- The size, never the payload. A rejection log holding bodies would be a second copy of
    -- exactly the data this guard exists to protect.
    body_bytes  int,
    created_at  timestamptz not null default now()
);

create index intake_rejections_endpoint
    on intake_rejections (endpoint_id, created_at desc);

-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
-- ---------------------------------------------------------------------------
-- Reverse order, children before parents, so no foreign key is left pointing at a dropped
-- table. `if exists` throughout: a down script that fails halfway leaves an instance in a
-- state neither script can continue from.

--   drop table if exists intake_rejections;
--   drop table if exists intake_endpoints;
--   drop table if exists breaker_events;
--   drop table if exists circuit_breakers;
--   drop table if exists retry_outcomes;
--   drop table if exists retry_policies;
--   drop table if exists idempotency_keys;
--   drop table if exists rate_limit_refusals;
--   drop table if exists rate_limit_policies;
--
-- Commented out, like every other migration in this tree, because the up half is run by
-- `Db::migrate` and a reversal written as live statements would be executed by it too — on the
-- first apply it would create and then drop every table this file defines, leaving the
-- instance with no reliability schema and a migration row claiming success. The first attempt
-- at this file did exactly that, which is worth recording: the reversal looked correct in
-- review and was a no-op that destroyed its own subject.
