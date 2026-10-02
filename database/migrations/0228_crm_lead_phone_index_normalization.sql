-- The phone index must normalize the phone EXACTLY as the query that uses it does.
--
-- `dedupe::PHONE_DIGITS_SQL` is the single SQL spelling of the phone normalization rule: strip
-- the punctuation, keep a leading `+` when the stored value has one. `store::merge_attribution`
-- and `store::fetch_candidates` both compare that expression against `dedupe::normalize_phone`,
-- which keeps the `+` — so the expression above is the only spelling of the key those two
-- predicates can be indexed by.
--
-- `crm_leads_phone_idx`, created by `0055_crm_lead_intake.sql`, normalizes with a bare
-- `regexp_replace(phone, '[^0-9]', '', 'g')`, which strips the `+` along with the spaces. Its
-- key for `+90 (532) 111 22 33` is therefore `905****2233` where the predicate's value is
-- `+905****2233`, and an expression index only serves an identical expression — so the phone
-- arm of the first-touch lookup seq-scanned every lead in the tenant on every returning
-- visitor's second submission, and the duplicate-match arm did the same against every contact.
--
-- The index was correct when it was written and became wrong when the two queries were fixed:
-- this is the same rule written a third time, in the schema rather than in a query, and it
-- survived two ticks of fixing the other two because nothing on this branch had ever asked the
-- planner about a phone.
--
-- Measured on a 20k-lead fixture with the statement copied from `store.rs`:
-- `Seq Scan on crm_leads`, `Rows Removed by Filter: 20000`, 41.8 ms. After this migration the
-- same statement is an `Index Scan using crm_leads_phone_idx`. `scripts/qa/run-crm-phone-index.sh`
-- asserts both, and its negative control drops the index in a rolled-back transaction.
--
-- Plain `create index`, not `concurrently`, and the same reason as `0200`: a migration runs
-- inside a transaction, `concurrently` cannot, and making it work took a fresh install to
-- unbootable. The lock is a one-off rebuild of one index on one table.
drop index if exists crm_leads_phone_idx;

-- The expression is character-for-character the crate's, `coalesce(phone, '')` included: an
-- index that agrees with the query in every respect except one is not an index the planner
-- can use, and that failure is silent — the plan is simply never offered the index. The
-- partial predicate still holds only real phone numbers, so a lead captured without one costs
-- no index entry.
create index crm_leads_phone_idx on crm_leads
    (organization_id,
     (case when btrim(coalesce(phone, '')) like '+%' then '+' else '' end
      || regexp_replace(coalesce(phone, ''), '[^0-9]', '', 'g')))
    where phone is not null;
