-- 0151_security_rate_limits.sql — the limiter and the lockout documents (REQ-012, slice 3).
--
-- Two JSON documents land on the `security_settings` singleton that slice 2 created, and one
-- index is added to a table that already exists. That is the whole change, and each part of it
-- exists because of a specific way a rate limiter goes wrong:
--
--   * `rate_limits` is a LIST, not an object, and the list's members are the five scopes the
--     platform resolves requests into. Storing it as an object keyed by scope would be tidier
--     and would be worse: it would make a missing scope indistinguishable from a scope set to
--     `{}`, and a scope that silently means "allow everything" is the failure mode this whole
--     feature exists to remove. A list makes "this scope has no row" a state the Rust layer has
--     to answer for, which is what `merge_with_defaults` does.
--   * `lockout` is an object because it is one document with named fields, and a missing field
--     is filled by the default policy rather than by the document itself.
--   * The columns are NOT NULL with a literal default, so a row inserted by anything other than
--     this migration — including a bare `insert into security_settings (id) values (1)` — still
--     holds a *valid* policy. A nullable column would push "what does null mean" into every
--     reader, and the four answers a nullable settings column admits (unset / zero / empty /
--     broken) are four times the places a rate limit can be silently wrong.
--
-- The range checks live in Rust (`crates/security/src/limiter.rs` and `lockout.rs`), not here,
-- and the reason is the same one slice 2 gave: a SQL check cannot say *which field* is wrong in
-- a message an operator can act on. The one thing this file does assert in SQL is the shape —
-- an array, and an object — because a shape check is exactly what SQL is good at and a bad
-- shape cannot be turned into a field-level message at all.

alter table security_settings
    -- The limiter document: one row per scope, in `RATE_SCOPES` order. `[]` would be legal SQL and
    -- a policy with no scopes; the Rust default merge is what turns it into the baseline, and
    -- the tests pin that it does.
    add column rate_limits jsonb not null default '[]'::jsonb,
    add constraint security_settings_rate_limits_array
        check (jsonb_typeof(rate_limits) = 'array'),
    -- The brute-force document: window, threshold, lockout minutes, progressive delay.
    add column lockout jsonb not null default '{}'::jsonb,
    add constraint security_settings_lockout_object
        check (jsonb_typeof(lockout) = 'object');

-- Who last changed the limiter, and when.
--
-- The header policy got a full history table in 0135 because a CSP is a document a human reads
-- and diffs. The limiter does not: it is five numbers, and the audit trail already records every
-- settings change with its actor and its before/after payload. Adding a second history here
-- would create two places to answer "when did the sign-in limit change", and they would not be
-- written by the same statement.
alter table security_settings
    add column rate_limits_updated_by uuid references users (id) on delete set null,
    add column rate_limits_updated_at timestamptz not null default now();

-- The "currently locked" list behind `/security/sign-in-protection`.
--
-- It reads `users.locked_until`, which slice 3 does not add — the column and the increment
-- already exist for `crates/identity`'s lockout. The index is what the screen needs and the
-- existing table does not have: a predicate on a nullable timestamp, filtered to the rows that
-- are actually locked, so the list is an index scan rather than a sequential read of every
-- account on a platform with millions of them.
--
-- A partial index, deliberately: `where locked_until is not null` keeps it to the locked rows,
-- which are a vanishing fraction of the table, and it means the index is *only* usable for the
-- question it was built for. An operator asking "which accounts were ever locked" wants the
-- audit trail, not this index — and an index that pretended to answer that would make the
-- second query silently slow.
create index users_locked_until_live_idx
    on users (locked_until)
    where locked_until is not null;
