-- Omnion · 0040 · The trace index and the queue's trace context (REQ-126, slice 3)
-- (docs/requests/REQ-126-observability-stack.md).
--
-- Additive by design (docs/05-VERSIONING.md): two new tables and one nullable column added to an
-- existing table. Nothing is rewritten, nothing is dropped, so a fresh installation and a populated
-- one reach the same schema and no row written by an earlier release is touched.
--
-- ## Numbering note
--
-- The slot is global across the parallel waves. 0038 and 0039 are held by wave 5, 0037 by this
-- wave's own metric catalogue, so the trace index is 0040. The name came from an `ls` of every
-- sibling worktree and `origin/main`, re-run after the merge — two branches can each be internally
-- consistent and still collide in their union, and sqlx keys migrations on version AND checksum,
-- so a collision makes the API refuse to boot at all.
--
-- ## Why the index is a TABLE and the spans are not
--
-- The request is explicit that spans stay in the operator's backend: "spans themselves stay in the
-- operator's backend", and "Long-term cold storage beyond the retention window — logs and traces
-- older than retention belong to the operator's backend." Omnion emits; it does not become the
-- backend (the request's own "Out" section).
--
-- So this is an INDEX, not a store. One row per trace, enough to search by request id, route,
-- status, duration and window, and enough to draw a waterfall — which means the row carries the
-- spans. That is a deliberate, bounded exception and it needs a cap, or "one row per trace with its
-- spans inline" is a log platform by accident. `MAX_SPANS_PER_TRACE` in the crate is the limit, a
-- trace over it keeps its first N spans and records `spans_truncated`, and the *search* is what
-- this table is for — the operator's backend is where the full trace is read.
--
-- ## Why `webhook_deliveries` gains a column
--
-- The acceptance line says "the consumer span links back to the producer". A link is an identity,
-- not a timestamp: the consumer has to be handed the producer's trace id and span id at enqueue
-- time, because by the time the consumer runs the producing request's task-local context is long
-- gone — the producer and the consumer are different processes, possibly minutes apart. There is
-- no way to reconstruct the link afterwards, which is why it is stored on the job row itself.
--
-- The column is nullable, and that is not an oversight: a delivery queued by a CLI, by a seed, or
-- by anything that is not inside a traced request has no producer span. The consumer then starts a
-- root span rather than inventing a parent, and `linked` in the UI is honestly false.
--
-- ## Down script
--
-- Written as executable statements per the REQ-129 migration-safety policy rather than as a second
-- file, because the runner is a single forward-only `sqlx::migrate!` bundle: a rollback has to be
-- applied deliberately, by an operator, and written down where the forward migration is read.
--
--   alter table webhook_deliveries drop column if exists trace_context;
--   drop table if exists obs_trace_index;

-- The trace context a producer hands its consumer, stored as one JSON object.
--
-- The fields are the W3C pair and the request id. `request_id` is here as well as in the queue's
-- own event row because the consumer's log line is written long after the producing request's
-- request id has scrolled off an operator's screen: an operator who finds a failing webhook
-- delivery wants the request that caused it, and this is the only place the two survive together.
create table if not exists obs_trace_index (
    trace_id        text        primary key,
    root_name       text        not null,
    service         text        not null,
    route           text,
    request_id      uuid,
    started_at      timestamptz not null default now(),
    duration_ms     integer     not null default 0,
    span_count      integer     not null default 1,
    -- How many spans the inline waterfall actually kept. `span_count` above is how many the trace
    -- really had, so a truncated trace is visible as the two disagreeing rather than as a short
    -- waterfall that looks complete.
    spans_kept      integer     not null default 0,
    spans_truncated boolean     not null default false,
    status          text        not null default 'ok',
    sampled         boolean     not null default true,
    -- The link to the operator's tracing backend for the full trace. Omnion knows the convention
    -- (Tempo/Jaeger/Grafana) but not the instance, so the value is a template the settings row
    -- fills — a link that 404s is worse than an honest "no backend configured".
    backend_trace_url text,
    -- The spans, already through the redaction pass. This is the column a leak would live in, and
    -- the test that greps a fixture secret asserts on it directly.
    spans           jsonb       not null default '[]'::jsonb,
    attributes      jsonb       not null default '{}'::jsonb,
    constraint obs_trace_index_status_check check (status in ('ok', 'error'))
);

-- Search paths, in the order the screen's filters are applied.
create index if not exists obs_trace_index_started_idx
    on obs_trace_index (started_at desc);
create index if not exists obs_trace_index_request_idx
    on obs_trace_index (request_id);
create index if not exists obs_trace_index_status_started_idx
    on obs_trace_index (status, started_at desc);
create index if not exists obs_trace_index_route_started_idx
    on obs_trace_index (route, started_at desc);

comment on table obs_trace_index is
    'One row per traced request: enough to search traces and draw a bounded waterfall. The full span '
    'set lives in the operator''s tracing backend (REQ-126, "Out": Omnion emits, it is not the backend).';
comment on column obs_trace_index.spans is
    'Redacted span records for the waterfall, capped by MAX_SPANS_PER_TRACE; spans_truncated says when the cap bit.';

-- The producer's context, handed to the consumer at enqueue time.
--
-- Nullable by design: a delivery queued outside any traced request (CLI, seed, a replayed row from
-- an older release) has no producer, and the consumer starts a root span rather than inventing one.
alter table webhook_deliveries
    add column if not exists trace_context jsonb;

comment on column webhook_deliveries.trace_context is
    'The producing request''s W3C trace id/span id and request id, written at enqueue so the consumer '
    'span can link back (REQ-126 slice 3). Null when the delivery was queued outside a traced request.';
