-- 0243 — the three request-log columns `origin/main`'s middleware needs, on this branch's table.
--
-- ## Why this migration exists
--
-- `origin/main` shipped a request-log *middleware* (`apps/api/src/request_log_middleware.rs`) that
-- records every request the platform serves, and it needs four things from a row that migration
-- `0240` explicitly refused to add — the file's own table says:
--
--     | `api_request_logs.client_fingerprint`, `.permission`, `.actor_name` | not present |
--
-- `0240` was right when it was written: this branch's `0223` owns the table, and re-declaring
-- another migration's table is a migration whose success depends on which branch ran first. But
-- "the columns are not present" is a statement about that moment, and this file is what makes it
-- untrue. The recorder is now on the request path, so a row it writes has to carry what the
-- recorder knows.
--
-- ## What each column is for
--
-- * `actor_name` — the person's name, **copied in**, not joined at read time. The log's retention
--   window outlives some of its subjects, and a log row that renders a blank actor because the
--   user was deleted is a row that cannot answer "who was this" for a request somebody is
--   investigating. The key side already stores `api_key_prefix` for the same reason.
-- * `api_key_prefix` — the key's displayable prefix, on the same argument. The key list prints
--   it precisely so an operator can match a log row to a key, and a log screen that cannot
--   print it makes the key list's prefix decorative.
-- * `permission` — **the permission the guard resolved, not the one the route declares.** This
--   is the column that makes a `403` explainable: a row that recorded only "403" leaves an
--   operator to guess between a missing scope, a wrong tenant and a route that is simply not
--   public. The guard already knows the answer at the moment it refuses, so recording it later
--   means guessing.
--
-- `client_fingerprint` is deliberately **not** added. Main's recorder computes an HMAC over the
-- client address keyed by `OMNION_LOG_PEPPER`, and this branch's compliance position (REQ-033's
-- request-log risk note) is that client identifiers are masked according to policy rather than
-- stored by default. Adding the column now would make it a nullable field nothing writes, and a
-- nullable privacy field is a privacy field with no policy attached. When a tenant's policy asks
-- for it, it arrives with that policy — see `client_identity` in `crates/developer/src/store.rs`,
-- where the fingerprint is computed and deliberately not persisted.
--
-- ## The write side is unchanged
--
-- `store::log_request` binds the columns it names, so these three are nullable and default to
-- `null`; the key-authenticated path that already fills `api_key_id` is the one that fills
-- `api_key_prefix`, and both stay nullable because a session-authenticated request has neither.
-- Nothing that reads the table today changes shape: every reader projects the columns it wants.

alter table api_request_logs
    add column if not exists actor_name text not null default '',
    add column if not exists api_key_prefix text,
    add column if not exists permission text;

comment on column api_request_logs.actor_name is
    'The person''s display name, copied in so a deleted user does not leave a blank actor in a row that outlives them.';

comment on column api_request_logs.api_key_prefix is
    'The authenticating key''s displayable prefix. Never a token; this is the value the key list already prints.';

comment on column api_request_logs.permission is
    'The permission the guard resolved when it decided this request. Makes a 403 explainable without guessing.';

-- The compliance screen filters a log by outcome, and the recorded permission is the fastest
-- route from "every refusal in this window" to the set worth reading. Partial: most rows are
-- `200`s with no need to index them, and an index over a column that is null on the common
-- path buys nothing.
create index if not exists api_request_logs_permission_refusals_idx
    on api_request_logs (organization_id, permission, created_at desc)
    where permission is not null;
