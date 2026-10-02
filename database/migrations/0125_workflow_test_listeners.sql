-- 0125_workflow_test_listeners.sql — "Listen for a real event" on the visual builder
-- (REQ-004 slice 3, criterion 5).
--
-- REQ-003 already has a one-shot listener: `automation_test_events` (migration 0020) with
-- `POST /automations/{id}/listen`. That row is **rule-shaped** — it stores no node and has no
-- expiry, and it lives on the linear editor's surface. The builder's listener is a different
-- thing with different requirements, so it is a different table rather than three more
-- columns on a row that already has a job:
--
--   * it is armed **for a node**, not for a rule. The builder's toolbar is pressed with a
--     trigger node selected, and what an author wants to see is "what would *this* node
--     receive?" — a rule-level capture cannot answer that, because the payload the node
--     sees is the payload the *upstream* node produced, which is not the bus event.
--   * it **expires**. REQ-004 names fifteen minutes. The existing row has no expiry at all,
--     which means an author who arms a listener, closes the laptop and comes back tomorrow
--     finds a row that has been waiting for a day and captures an event nobody is watching.
--   * it carries a **token**, because a builder listener can be armed from outside the panel
--     (a `curl` in a second terminal, an n8n-style hit) and the reply has to name the thing
--     that was armed. Only the hash is stored, for the same reason the approval gate's
--     decision tokens are: a database dump must not hand over every armed listener.
--
-- Four decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **One live listener per (workflow, node).** The obvious constraint is "one listener per
--     workflow", which is what the rule-shaped table does. On a canvas that is wrong: an
--     author debugging two nodes of the same rule at once is doing the one thing this
--     feature exists for, and a rule-level unique index would make the second press silently
--     replace the first and report success. The partial unique index is on the *node*, so
--     arming a second node keeps the first one alive.
--   * **Expiry is a column, not a filter.** `expires_at` is `not null` and armed rows are
--     selected `where consumed_at is null and expires_at > now()`. A listener that is past its
--     expiry is therefore *not armed* in the only sense that matters — a query that filtered
--     on `consumed_at` alone would hand the matcher a dead row and report a capture for an
--     event nobody was watching. The matcher asks the same question the panel asks, from one
--     predicate, and the partial index is built on it.
--   * **A consumed row is kept, not deleted.** The panel shows "captured 40s ago" and the
--     payload; deleting the row on capture would make the answer a flash of text that
--     disappears before it can be read, and would leave the author re-arming in a loop. The
--     row is pruned by the sweeper after the retention window, which is the same 24 hours the
--     rule-shaped test rows get.
--   * **The payload is bounded by a constraint, not by the application.** `octet_length`
--     against `payload::text` refuses a row the matcher could not have written because the
--     application checked first and the two would eventually disagree. `send_email` payloads
--     carry a whole page revision; a bus event with a 2 MB body must fail the capture, not
--     the matcher's memory.
--
-- The token is a *handle*, never a capability: it names the armed listener so a caller can
-- read it back, and it grants nothing that the session guard did not already grant.

create table if not exists workflow_test_listeners (
    id uuid primary key default gen_random_uuid(),
    workflow_id uuid not null references workflows (id) on delete cascade,
    -- The node the listener was armed for. Not foreign-keyed to a node: a node id is a
    -- string in a jsonb graph, not a row, and a listener armed for a node the author has
    -- since deleted is a fact worth keeping until it expires.
    node_id text not null,
    organization_id uuid not null references organizations (id) on delete cascade,
    -- The event name the listener is armed for. Stored rather than derived from the rule so
    -- the armed row says out loud what it is waiting for — the panel shows this sentence, and
    -- a row that had to be joined back to a rule to answer "what is it listening for?" is a
    -- row whose answer changes when the rule's trigger is edited under it.
    event_name text not null,
    -- Only the hash is stored. `arm_listener` mints 32 characters of CSPRNG output and the
    -- caller gets the cleartext exactly once, in the arming response.
    token_hash text not null,
    -- The account that armed it. `on delete set null`: the listener outlives the person, and
    -- the captured payload is the fact worth keeping.
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    -- Fifteen minutes, as REQ-004 names. `not null` so a row can never be armed for ever.
    expires_at timestamptz not null,
    -- Set the moment an event fills it. Single use.
    consumed_at timestamptz,
    event_id bigint references events (id) on delete set null,
    event_name_captured text,
    payload jsonb,

    constraint workflow_test_listeners_token_unique unique (token_hash),
    constraint workflow_test_listeners_expiry_after_creation
        check (expires_at > created_at),
    -- An unconsumed listener carries no payload and no event: a row claiming to be waiting
    -- while already holding a capture is a row the one-shot predicate cannot reason about.
    constraint workflow_test_listeners_armed_shape check (
        (consumed_at is null and payload is null and event_id is null)
        or (consumed_at is not null and payload is not null)
    ),
    -- The capture is bounded here, not in the matcher.
    constraint workflow_test_listeners_payload_size
        check (payload is null or octet_length(payload::text) <= 65536)
);

-- One live listener per node of a rule. `where consumed_at is null` is what makes a second
-- arm of the *same* node replace the first, while a second node is armed alongside it.
create unique index if not exists workflow_test_listeners_armed_idx
    on workflow_test_listeners (workflow_id, node_id)
    where consumed_at is null;

-- The panel's read: "is anything armed for this rule, and what did it capture?"
create index if not exists workflow_test_listeners_workflow_idx
    on workflow_test_listeners (workflow_id, created_at desc);

-- The matcher's probe is the same predicate the panel uses, and the partial index is built on
-- the *live* side of it — a query that filtered on `consumed_at` alone would scan the
-- historical rows too, and the historical rows grow without bound.
create index if not exists workflow_test_listeners_live_idx
    on workflow_test_listeners (expires_at)
    where consumed_at is null;
