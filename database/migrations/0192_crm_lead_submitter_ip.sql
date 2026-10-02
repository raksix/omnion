-- REQ-117, slice 19 — the submitter's address, stored and countable.
--
-- ## What was missing, and why the column is the whole finding
--
-- `Submission.ip` has carried the doc comment "the submitter's IP, for the per-IP rate limit
-- and the audit trail" since the capture struct shipped. The audit half is real: the address
-- goes into the lead's `received` trail line. The **per-IP rate limit is not implemented
-- anywhere on this branch** — checked rather than assumed, with `grep` for a per-address
-- ceiling over `crm_leads`, and the only one that exists is `submissions_this_hour`, which
-- counts by `source_id`.
--
-- So the two ceilings a keyed endpoint needs are not two but ONE: the *source's* hourly
-- ceiling. A single submitter behind one address can spend an entire source's budget in
-- seconds, and a source can be shared by every page of a site, so the flood that the REQ's
-- Risks section calls "the public intake surface is the attack surface" is unbounded per
-- address. An operator who lowers `rate_limit_per_hour` to stop a flood stops their real
-- form's legitimate traffic with it, because there is no second, cheaper dial to turn.
--
-- ## Why the address is stored and not recomputed
--
-- The submission's address is a fact about *that* delivery. It cannot be recomputed later:
-- the request is gone, and the only durable record was the trail line's `detail->>'ip'`
-- jsonb, which a `where` clause cannot index and a window function cannot filter. Counting
-- from the trail instead of the row is the shape this branch has paid for repeatedly — it is
-- what `scripts/qa/merge-build-log.py` exists to avoid, in another guise.
--
-- The column is therefore written by the one insert that already has the address
-- (`store::insert_lead`) rather than by a second statement, so a lead row is written once.
--
-- ## Additive, so nothing is re-examined
--
-- A new nullable column with no constraint: no existing lead is re-read, and no existing
-- row changes status. `inet` rather than `text` because the platform already stores
-- addresses that way (`0001_initial.sql`, `0011_iam_advanced.sql`, `0015_analytics.sql`), so
-- an address written here compares equal to one written by the session log — the alternative
-- is a second spelling of the same value that no join can match.
--
-- **The column comes before the index, and that is not a style preference.** The index below
-- indexes this column, and a `create index` naming a column that does not exist is a hard
-- failure on a fresh database. The first version of this file had the index first, and the
-- gate caught it on its first run before a single test executed — which is the whole
-- argument for reading the schema back before the tests, and the reason it is written down.
alter table crm_leads
    add column if not exists submitter_ip inet;

-- ## The index is partial, and the predicate is the point
--
-- `where received_at > now() - interval '1 hour'` cannot be written in a partial index
-- (`now()` is not immutable), so the index covers the address and the instant and the window
-- is applied in the query. The predicate that *can* be partial is `submitter_ip is not null`:
-- a source reached through the events bus has no address at all, and indexing the nulls
-- would make every such lead pay for an index entry that can never match.
create index if not exists crm_leads_submitter_ip_idx
    on crm_leads (source_id, submitter_ip, received_at desc)
    where submitter_ip is not null;

comment on column crm_leads.submitter_ip is
    'The submitter''s address as the request carried it (proxy-aware, see ClientAddress). '
    'Null for a submission that arrived through the events bus rather than an HTTP request. '
    'Counted by the per-address hourly ceiling, which is why it is a column and not a '
    'trail-line detail.';
