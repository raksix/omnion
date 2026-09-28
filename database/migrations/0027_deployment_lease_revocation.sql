-- Omnion · 0027 · Deployment-driven lease revocation
-- (REQ-125, slice 3 · docs/requests/REQ-125-secrets-management-depth.md).
--
-- Additive by design (docs/05-VERSIONING.md): one table, no existing table is touched, nothing
-- is rewritten and no column is dropped. The leases and deployment-key tables this request
-- needs already exist (0019); what was missing is the bridge between a deploy and the leases
-- that were handed out for it.
--
-- The rule the request states is one sentence long and has an obvious implementation that is
-- wrong: "deployment.started revokes live leases for the environment". The trap is doing that
-- work *inside* the deployment handler. A deploy is recorded by whichever writer owns the
-- deployment centre, and coupling the secrets store to that handler means a deploy path that
-- does not know about secrets silently leaves live leases behind — exactly the case the rule
-- exists for.
--
-- So the revocation is a consumer: this table records how far the secrets runner has read the
-- event stream, and each tick it reads past the cursor, revokes the environment's leases and
-- moves the cursor. A deploy is therefore honoured even when the deploy was recorded by a
-- writer that has never heard of this request, and a restart resumes from the cursor rather
-- than re-revoking the same leases.
--
-- Numbering note: the version slot is global across the parallel waves, and wave 7 had already
-- opened 0026 for the AI model capabilities. sqlx keys a migration on version *and* checksum, so
-- two files claiming 0026 would make every database that applied one refuse the other. 0027 was
-- chosen from `ls` of the sibling worktrees at write time, not from a counter that pretends the
-- branch is alone.
--
-- How far one consumer has read the event stream.
--
-- The event stream is a single table shared by every consumer (crates/events), so the cursor is
-- the only way two readers can coexist without one of them moving the other's position. The
-- primary key is the consumer's own name, never a global row id, so a new consumer cannot
-- inherit another's position by accident.
create table event_consumer_cursors (
    -- The consumer's stable name, e.g. `secrets.lease_revocation`.
    consumer       text        primary key,
    -- The last event id this consumer has acted on. 0 means "has read nothing yet".
    last_event_id  bigint      not null default 0,
    -- When it last moved, so a stalled cursor is visible in a health check.
    updated_at     timestamptz not null default now(),
    -- How many events it has acted on in total, for the same reason.
    processed      bigint      not null default 0
);

comment on table event_consumer_cursors is
    'Per-consumer read position in the events stream, so two consumers of the same events do not '
    'move each other''s position. Added by 0027 for the deployment-driven lease revocation of '
    'REQ-125; the mechanism is generic so the next consumer does not need its own migration.';

-- The seeds are data, not schema, and they are the whole content of this migration's data half:
-- a fresh installation has never read an event, so its cursor starts at 0 and the runner will
-- read the entire existing history on its first tick. That is correct but wasteful, so the
-- cursor is seeded at the current tail instead: an installation that has never had a
-- deployment.started event does not need to walk back through every event ever recorded to
-- discover that.
insert into event_consumer_cursors (consumer, last_event_id, processed)
select 'secrets.lease_revocation', coalesce(max(id), 0), 0
from events
on conflict (consumer) do nothing;

-- Reversal, in the order REQ-129's policy asks for: drop what this file created, nothing else.
-- The seeded cursor row goes with its table and nothing else references it.
--
--   drop table if exists event_consumer_cursors;

-- A note on the index this file deliberately does *not* create. The obvious addition is a partial
-- index over one environment's outstanding leases:
--
--   create index on secret_leases (environment, issued_at desc)
--     where revoked_at is null and expires_at > now();
--
-- and it is wrong twice over. The predicate is refused outright — `42P17 functions in index
-- predicate must be marked IMMUTABLE`, because `now()` is STABLE, not IMMUTABLE: its answer
-- depends on the statement's timestamp rather than its arguments, and a partial index may only
-- test a column. And the thing it was supposed to add already exists: 0019 creates
-- `secret_leases_live_idx` on exactly those columns with exactly the `revoked_at is null`
-- predicate. A second index over the same rows on the same read is a write tax, not a speed-up.
--
-- Expiry is therefore left to the scan, and the scan is a range read of at most one
-- environment's outstanding leases — the set the revocation updates anyway.
