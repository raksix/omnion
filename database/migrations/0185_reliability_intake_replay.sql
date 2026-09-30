-- The intake guard's replay window (REQ-127, slice 4).
--
-- `0162_reliability.sql` declared the endpoints and the rejection log, and its own comment on
-- `evaluate` promises a replay defence — but nothing in the schema held the ids a replay is
-- detected from. That gap is this file, and it is worth naming why the replay set is a **table**
-- and not something the guard holds in memory:
--
-- * A process that restarts between two deliveries of the same signed request forgets the first
--   id, and the second delivery is accepted. A replay window that does not survive a restart is
--   not a replay window.
-- * The set must be bounded. "Every id ever seen" is a table that grows forever and eventually
--   refuses a legitimate delivery whose provider reuses an id, so each row carries the moment it
--   expires and the prune removes it rather than the guard deciding whether it is still fresh.
--
-- The unique index is the actual defence. The obvious implementation reads "have I seen this id?"
-- and then writes it, and two deliveries of the same request arrive together often enough that
-- both read "no" and both proceed — the guard preventing exactly the thing it exists for. Here
-- the write *is* the question, and `rows_affected() == 0` is the replay.

create table intake_seen_signatures (
    endpoint_id  uuid        not null references intake_endpoints (id) on delete cascade,
    -- The `v1,<id>` half of a `v1,<id>:<tag>` signature. Scoped to the endpoint on purpose: two
    -- endpoints may legitimately be given the same id by the same provider, and scoping the
    -- uniqueness is what stops one endpoint's traffic from refusing another's.
    signature_id text        not null,
    seen_at      timestamptz not null default now(),
    -- `seen_at` plus the endpoint's tolerance. The window is the *endpoint's*, not a constant,
    -- because a request outside its endpoint's tolerance is already refused as stale and
    -- remembering it longer can only refuse something that was never going to be accepted.
    expires_at   timestamptz not null
);

create unique index intake_seen_signatures_key
    on intake_seen_signatures (endpoint_id, signature_id);

-- The prune's access path. Partial on nothing: the table is small by construction, and the
-- index that matters is the one that makes the "is it still fresh" read cheap.
create index intake_seen_signatures_expiry
    on intake_seen_signatures (expires_at);

-- A rejection log with no retention grows without bound and holds one row per hostile request,
-- which makes it a log an attacker can fill. The request's own data model says "short
-- retention"; the number is stated here rather than left as a comment because the screen shows
-- the window it reads.
--
--   drop table if exists intake_seen_signatures;
--
-- Commented out, like every reversal in this tree: `Db::migrate` executes a file's LIVE
-- statements on apply, so a down script written as live SQL would drop the table this file
-- created and record a success doing it.
