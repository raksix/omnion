-- Omnion · 0026 · Deployment-driven lease revocation
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
    'move each other''s position. Added by 0026 for the deployment-driven lease revocation of '
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

-- The runner reads forward by id, so the hot path is "everything newer than the cursor for this
-- one name". The partial index keeps the read to the unread tail instead of the whole table
-- once the stream is large.
create index event_consumer_cursors_unread_idx
    on event_consumer_cursors (last_event_id)
    where last_event_id < 9223372036854775807;

-- Why a lease went away is worth keeping even after the deploy event itself has been
-- compacted, so the reason is written onto the lease row (the UPDATE in
-- `revoke_environment_leases` does that) rather than reconstructed from the event log.
--
-- The deployment key use log is the second place a deploy shows up: revoking a deployment key
-- writes a `revoke` row, and an environment-wide revocation has no key to attribute it to. The
-- existing `deployment_key_uses` table already accepts that shape, so no column is needed here
-- — this migration exists only to give the consumer a cursor.

-- A lease whose environment is empty would be revoked by a deploy that named no environment,
-- which `revoke_environment_leases` refuses. The 0019 default already covers it, and this
-- index makes the "live leases of one environment" read a single scan.
create index if not exists secret_leases_environment_live_idx
    on secret_leases (environment, issued_at desc)
    where revoked_at is null and expires_at > now();

-- Reversal, in the order REQ-129's policy asks for: drop what this file created, nothing else.
-- The partial index goes with its table, and the seeded cursor row is removed on its own since
-- nothing else references it.
--
--   drop index if exists event_consumer_cursors_unread_idx;
--   drop index if exists secret_leases_environment_live_idx;
--   drop table if exists event_consumer_cursors;
--
-- The index on `secret_leases` is dropped explicitly rather than implicitly because the table
-- it belongs to predates this migration: dropping `event_consumer_cursors` would not remove it,
-- and a reversal that leaves an index behind is not a reversal.
