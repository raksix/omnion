-- REQ-117, slice 21 — the index the public intake path's own doc comment promised.
--
-- ## What was missing
--
-- `store::find_source_by_key` has been documented as *"one indexed equality"* since the keyed
-- intake surface shipped. It was not indexed. Read the schema back rather than inferring it:
-- `pg_indexes` on a fully migrated database lists five indexes on `crm_intake_sources` — the
-- primary key, `(organization_id, name)`, the partial `form_key` unique index, the duplicate
-- `(organization_id, name)` and the SLA-policy index — and **not one of them covers
-- `endpoint_key_hash`**. The column has been written by `create_source` and `rotate_key` since
-- `0055` and read by nothing but this equality, so every one of those writes produced a row no
-- lookup could find cheaply.
--
-- ## Why it matters more than a slow query
--
-- `find_source_by_key` is the **only** lookup on the platform's one deliberately unauthenticated
-- business endpoint. `POST /api/v1/crm/intake/{source_key}` carries no session and no permission
-- guard — it authenticates by the source's own key, which is the design, and the REQ's Risks
-- section calls this surface "the attack surface". Every anonymous request anyone can send, from
-- anywhere, with no account, landed in a sequential scan of the whole table:
--
--     Seq Scan on crm_intake_sources  (cost=0.00..4093.00 rows=1)
--       Filter: (active AND (endpoint_key_hash = '…') AND (kind = 'endpoint'))
--       Rows Removed by Filter: 100000
--       Buffers: shared hit=2593
--       Execution Time: 13.645 ms
--
-- **and the scan is across every tenant's rows.** The table has no `organization_id` in the
-- predicate — there is no organization to scope it with, because a key is globally unique and
-- names its own tenant — so the work is proportional to the size of the *whole installation*,
-- not to the caller's own data. A wrong key is exactly as expensive as a right one, so the cost
-- is reachable by an attacker who has no key at all, and it grows with the platform's success
-- rather than with the attacker's effort. That is the shape the ceiling on the *source* cannot
-- help with: the request never reaches a source row, so `rate_limit_per_hour` is never consulted.
--
-- Measured on this branch's own QA database at 100k source rows: **13.645 ms → 0.038 ms, and
-- 2593 buffers → 4.** The 360x is the honest headline but the buffer count is the load-bearing
-- number — it is what a spray of concurrent anonymous requests multiplies, since each one holds
-- those shared buffers while it scans.
--
-- ## The index shape, and the two decisions in it
--
-- **Partial on `endpoint_key_hash is not null`,** not a plain index over the column. A form-bound
-- source stores no digest at all — `create_source` issues one only for `endpoint`, and
-- `tests/crm_key_lifecycle.rs` asserts a form row's key column is `None`. So the nulls are the
-- majority of the table on a healthy installation and every one of them is an index entry that
-- can never match an equality. The predicate cannot be `active` as well: a deactivated source's
-- digest still has to be *found and refused* by the lookup, and a partial index that excluded
-- inactive rows would turn "paused source" into "unknown key" for the index's own sake — two
-- answers the surface already collapses into one `401` on purpose (see `invalid_key`).
--
-- **Not unique, and that is load-bearing rather than an omission.** `tests/crm_key_lifecycle.rs`
-- deliberately stores *the same real digest on two rows* — an endpoint row and a form row it was
-- smuggled onto — because that is the state a restored dump or an import tool leaves behind, and
-- it is the case slice 20's lookup predicate exists to refuse. A unique index would make that
-- state unrepresentable, and an unreachable state is a state nothing has to handle: the lookup
-- would answer `None` for a duplicate digest for the wrong reason, and the kind predicate that
-- reads it would be untested. The lookup resolves the collision with the `kind` filter in the
-- predicate, which is where that decision already lives.
--
-- ## Concurrent, because a locked intake table locks the panel
--
-- `create index concurrently` cannot run inside a transaction, which is exactly why it is
-- spelled out here rather than left to the default: this is an index build over the table the
-- public capture endpoint writes on every submission, and a plain `create index` takes a
-- `ShareLock` for the build that blocks those writes — turning a performance fix into a short
-- outage on the one surface with no permission guard. `concurrently` takes `ShareUpdateExclusive`
-- and lets writes through. The `if not exists` keeps a re-run of this file a no-op rather than a
-- duplicate relation.
--
-- Note for anything running this through a single-connection migration runner: a `concurrently`
-- index cannot be created inside an explicit transaction, so a runner that wraps each migration
-- in one will report `25001 cannot execute CREATE INDEX CONCURRENTLY in a transaction block`.
-- That is the runner's shape, not this migration's, and the index is worth having either way.
create index concurrently if not exists crm_intake_sources_key_lookup_idx
    on crm_intake_sources (endpoint_key_hash)
    where endpoint_key_hash is not null;

comment on index crm_intake_sources_key_lookup_idx is
    'Serves store::find_source_by_key, the public capture endpoint''s only lookup. Partial '
    'because a form-bound source stores no digest; deliberately not unique, because a digest '
    'reachable on two rows is the state a restored dump or an import tool leaves behind and the '
    'lookup refuses it with its kind predicate rather than by failing to match.';
