-- REQ-127 slice 1 — the shipped default budgets, and the refusal rollup's retention.
--
-- Additive, per docs/05-VERSIONING.md: rows only, no schema change, no column dropped. The
-- tables it inserts into were created by `0162_reliability.sql`; this file only seeds them.
--
-- **Slot choice.** Every writer shares one PUBLIC repository, so a migration number is a SHARED
-- namespace: "the next free number in my worktree" is a number another worktree is about to take.
-- 0165 is above the high-water mark across all ten writer worktrees (0163 in omnion-w8 at the
-- time of writing), skipping 0164 so a writer that was mid-tick at 0164 is not collided with.
--
-- **Why defaults are seeded here and not created on first boot.** The request says "login,
-- password-reset and public form routes ship with conservative default budgets", and the
-- alternative — a first-boot insert that has to be idempotent against a table that already
-- carries the `unique nulls not distinct (scope, target_id, route_pattern)` constraint — is a
-- second copy of the seed logic living in Rust. A seed that runs once, in order, with the schema,
-- cannot disagree with itself.
--
-- The rows are `is_default = true`, and `store::delete_policy` treats that flag as "disable, do
-- not remove" — so a deployment can switch a shipped budget off without the next boot restoring
-- it, and cannot delete a row the platform needs to make a decision at all.

-- Sign-in: the route the request names first, and the one an attacker reaches without an account.
-- Ten attempts per minute per address is enough for a person who mistyped a few times and slow
-- enough that a scripted spray spends minutes rather than seconds. `burst` is 2, because a browser
-- retrying a submit is not a spray and the alternative is a legitimate user locked out for
-- mistiming a double-click.
insert into rate_limit_policies
       (name, scope, target_id, route_pattern, limit_count, window_seconds,
        burst, priority, is_default, enabled)
values ('sign-in', 'ip', null, '/api/v1/auth/login', 10, 60, 2, 10, true, true)
on conflict (scope, target_id, route_pattern) do nothing;

-- Password reset: the same reasoning with a much tighter window, because each request sends mail
-- and mail is the expensive part. Three per hour per address, no burst: a user who clicks twice
-- because the first mail was slow gets the second request refused and a support ticket about it.
insert into rate_limit_policies
       (name, scope, target_id, route_pattern, limit_count, window_seconds,
        burst, priority, is_default, enabled)
values ('password-reset', 'ip', null, '/api/v1/auth/password-reset', 3, 3600, 0, 10, true, true)
on conflict (scope, target_id, route_pattern) do nothing;

-- The public renderer and the collection endpoint. `/public/analytics/collect` carries its own
-- per-site budget from REQ-007, so this row is the *fallback* for every other public path rather
-- than a second limit on the same route — two limiters on one request means the reader has to
-- know which one fired, and the more specific one is the site's.
insert into rate_limit_policies
       (name, scope, target_id, route_pattern, limit_count, window_seconds,
        burst, priority, is_default, enabled)
values ('public forms', 'ip', null, '/api/v1/public/*', 120, 60, 20, 20, true, true)
on conflict (scope, target_id, route_pattern) do nothing;

-- The authenticated API, per USER rather than per address. One office behind one NAT must not
-- exhaust a shared budget, and the user is the identity a signed-in request actually has. The
-- route pattern is left null so this is every authenticated route: the point of a user budget is
-- that a runaway client is stopped even while it is walking the whole surface.
--
-- This row is the one that makes the `user` scope reachable at all. Without it a deployment has
-- no user budget, and the screen's scope dropdown offers a scope nothing can ever spend.
insert into rate_limit_policies
       (name, scope, target_id, route_pattern, limit_count, window_seconds,
        burst, priority, is_default, enabled)
values ('authenticated API', 'user', null, null, 600, 60, 60, 50, true, true)
on conflict (scope, target_id, route_pattern) do nothing;

-- ---------------------------------------------------------------------------------------------
-- The reversal
-- ---------------------------------------------------------------------------------------------
-- Commented out, like every other migration in this tree and for the reason REQ-127 slice 1
-- found the hard way: a down script written as LIVE statements is executed by `Db::migrate` on
-- the next boot, immediately after it applied this file — so the "reversal" drops the rows the
-- migration had just inserted and records a success doing it. The first draft of this file did
-- exactly that and the seed silently disappeared on the following run.
--
--   delete from rate_limit_policies
--    where is_default
--      and name in ('sign-in', 'password-reset', 'public forms', 'authenticated API');
