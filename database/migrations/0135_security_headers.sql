-- 0135_security_headers.sql — the header policy and the CSRF secret (REQ-012, slice 2).
--
-- One row and one key, and the design turns on the same question slice 1 turned on: **the
-- platform must be able to say what it does, and must never be able to say something it does
-- not do.** Concretely, that means:
--
--   * `security_settings` is a SINGLETON (`id smallint primary key check (id = 1)`), not a
--     per-organization row. Header policy is a property of the deployment — a per-tenant CSP
--     would let one tenant weaken the policy the platform answers every response with, and the
--     response is served by one process for everybody. The limiter and the lockout in slice 3
--     have the same reason to be single rows; the finding who wrote this comment had the
--     alternative in front of them and chose this one.
--   * `headers jsonb` holds the whole policy as one document rather than as columns, because a
--     CSP is a list whose length is not known in advance and a column per directive would make
--     the migration that adds the nineteenth directive a schema change.
--   * The constraint below is the one honest invariant: a stored document must be an object.
--     Everything else about it is enforced in Rust (`crates/security/src/headers.rs`), which is
--     where the field-level reasons live. A SQL check cannot say "a directive name is not a
--     directive name" in a message an operator can act on.
--
-- The CSRF secret is an *environment* variable (`OMNION_CSRF_SECRET`), not a column, and that is
-- deliberate. A secret in this table would be a secret that lands in every backup, every
-- replica of the row and every `select *` a future endpoint performs by accident. The migration
-- therefore creates no secret material at all; it only records that the platform refuses to boot
-- its mutation paths without one.

-- ---------------------------------------------------------------------------------------------
-- security_settings — the singleton row of policy the platform enforces on itself.
-- ---------------------------------------------------------------------------------------------

create table security_settings (
    id          smallint   primary key default 1,
    -- The constraint is the table's own statement about being a singleton. A second row cannot
    -- be inserted by accident, so "which policy is in force" can never become a question with
    -- two answers.
    constraint security_settings_singleton check (id = 1),
    -- The whole CSP + HSTS + referrer + permissions policy, as one document. `{}` is not "no
    -- headers": `HeaderPolicy::from_json` treats an empty or unreadable document as the
    -- baseline, because a missing row must not mean "send nothing".
    headers     jsonb      not null default '{}'::jsonb,
    constraint security_settings_headers_object check (jsonb_typeof(headers) = 'object'),
    -- Who last changed the policy and when. Two columns and not one, because "who" is the whole
    -- question an audit answers and a timestamp does not carry it.
    updated_by  uuid       references users (id) on delete set null,
    updated_at  timestamptz not null default now()
);

-- The row exists from the first migration. An empty table here would mean every read has to
-- handle "no row yet" and every write has to insert-or-update, for a table that by definition
-- holds exactly one row forever.
insert into security_settings (id) values (1);

-- ---------------------------------------------------------------------------------------------
-- The policy history.
-- ---------------------------------------------------------------------------------------------
-- `security_check_results` is already append-only and already answers "when did this change
-- mind", but it records *check outcomes*, not configurations. Two questions are different:
-- "did the header check pass" and "when did somebody last edit the policy" both need an
-- answer, and answering the second from the first means inferring an edit from a run that
-- happened to notice it. So the edits get their own table — small, append-only, and the thing
-- an operator reads when a policy changed and nobody remembers who changed it.

create table security_settings_history (
    id          bigint      generated always as identity primary key,
    -- What changed: the whole previous and whole new document, so a revert is a copy rather
    -- than a reconstruction. A diff column would be smaller and would make "what did it look
    -- like before" unanswerable for a field that was deleted.
    before_headers jsonb    not null,
    after_headers  jsonb    not null,
    -- The caller's account, and the CSP mode in force after the edit so a reader can filter the
    -- history down to "when did we start enforcing" without replaying every document.
    changed_by  uuid        references users (id) on delete set null,
    csp_mode    text        not null,
    changed_at  timestamptz not null default now(),
    constraint security_settings_history_mode_check
        check (csp_mode in ('report_only', 'enforce')),
    constraint security_settings_history_before_object
        check (jsonb_typeof(before_headers) = 'object'),
    constraint security_settings_history_after_object
        check (jsonb_typeof(after_headers) = 'object')
);

-- "Show me every change, newest first" is the only query this table exists for.
create index security_settings_history_changed_at_idx
    on security_settings_history (changed_at desc);
