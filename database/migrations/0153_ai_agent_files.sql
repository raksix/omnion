-- REQ-099 · Agent runtime & tool loop — slice 2 (the per-agent workspace).
--
-- A workspace is a scratch area an agent is told to read from and write to: the inputs somebody
-- uploaded for a run, and the outputs the run decided to keep. The index below is the *only*
-- thing that makes it a workspace rather than a folder of blobs — the bytes live in object
-- storage under an opaque key, and this table is what a run is permitted to name.
--
-- Decisions worth stating, because each closes a way the index can lie:
--
-- 1. **`storage_key` is opaque and derived, never the path.** The path is what a human types
--    and what a goal text quotes; the key is what the bucket is asked for. Deriving the key from
--    the agent id and a checksum rather than from the path means a path collision, a rename and
--    a traversal attempt all fail at the *validation* layer, before a key is ever assembled —
--    and a stored key cannot be read back as a different file's address.
--
-- 2. **The caps are columns and therefore checkable, not a memory of a running sum.** `size_bytes`
--    is on the row, so the 100 MB per-agent ceiling is `sum(size_bytes) where agent_id = $1`
--    rather than a counter that a crashed upload leaves permanently off by one file. A cap that
--    lives in process memory is a cap that a restart silently lifts.
--
-- 3. **A path is unique per agent, not per organization.** Two agents in one organization each
--    keep their own `inputs.csv`; they address different objects under different keys. Making
--    the index span the organization would force every agent to invent a namespace around a
--    human-readable name — a rule that exists for the storage layer's benefit and is paid for
--    by every person who names a file.
--
-- 4. **The path rules are re-checked on read, not only on write.** A row written by an older
--    build (or by a migration) can carry a path today's code would refuse. `list_files` and the
--    download route therefore go through the same validator, so the rule has exactly one
--    implementation and a row cannot become reachable by being written under a different
--    spelling.
--
-- 5. **`run_id` is `set null` and `last_used_at` is nullable.** A file outlives the run that
--    wrote it — that is the point of a workspace — and "which run last read this" is a
--    convenience column, not ownership. Cascading a delete through `run_id` would delete an
--    agent's only copy of a report because a run row was pruned by retention.

create table if not exists ai_agent_files (
    id           uuid primary key default gen_random_uuid(),
    agent_id     uuid        not null references ai_agents (id) on delete cascade,
    run_id       uuid        references ai_runs (id) on delete set null,
    path         text        not null,
    size_bytes   bigint      not null,
    content_type text        not null default 'application/octet-stream',
    storage_key  text        not null,
    checksum     text        not null default '',
    created_by   uuid        references users (id) on delete set null,
    created_at   timestamptz not null default now(),
    last_used_at timestamptz,

    -- The per-file ceiling, restated as a constraint so a row written by anything other than
    -- the upload route cannot claim a size the route would have refused.
    constraint ai_agent_files_size_range check (size_bytes >= 0 and size_bytes <= 10485760),
    -- The path is an identifier, not prose: no control characters (they survive a log line and
    -- break a terminal), no leading slash (absolute), and a length that keeps a storage key
    -- derivable from it inside every backend's own key limit.
    constraint ai_agent_files_path_clean check (
        char_length(path) between 1 and 512
        and path !~ '[[:cntrl:]]'
        and path !~ '(^|/)\.\.(/|$)'
        and path !~ '^/'
        -- A drive letter, restated from the Rust validator. `C:\notes.md` clears every clause
        -- above — no slash, no leading slash, no `..` — and it is the shape a Windows client
        -- sends when a path is concatenated by hand rather than taken from an API.
        and path !~ '^[A-Za-z]:'
    ),
    -- A row with no storage key is a row whose bytes cannot be fetched, and a download route
    -- that answers "not found" for it is indistinguishable from a file that was deleted.
    constraint ai_agent_files_storage_key_present check (char_length(storage_key) between 1 and 512)
);

-- One path per agent: the second upload of `notes.md` replaces the index row (and its object),
-- which is the only way two rows can claim the same address.
create unique index ai_agent_files_agent_path_uidx on ai_agent_files (agent_id, path);

-- The list screen's ordering, and the cap query's own shape — both are "this agent's rows,
-- newest first", so one index serves the table and the quota.
create index ai_agent_files_agent_created_idx on ai_agent_files (agent_id, created_at desc);
