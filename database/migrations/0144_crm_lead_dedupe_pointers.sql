-- 0144_crm_lead_dedupe_pointers.sql — give the duplicate verdict the two facts it needs to be
-- reversible: *which contact* it matched and *how well*.
--
-- REQ-117, slice 1, the duplicate queue. This migration exists because a defect could not be
-- observed on the branch that introduced it, and a defect a test cannot reach is a defect that
-- ships.
--
-- **`crm_leads.duplicate_of` is a foreign key to `crm_leads`, and `capture` wrote a
-- `crm_contacts` id into it.** The column was designed for "this submission duplicates that
-- earlier *lead*" (REQ-117's own data model, and the natural reading of the name), but the
-- code resolved a match to a contact and stored the contact id there. A contact is not a lead,
-- so on any installation that has the CRM module the write raises
-- `crm_leads_duplicate_of_fkey` (23503) and the *entire submission* fails — a
-- `reject_duplicate` source answers `500` to every visitor who writes in.
--
-- Why fourteen ticks missed it: `crm_contacts` belongs to REQ-051 and is **not on this
-- branch**, so `fetch_candidates` takes its documented module-absence path and returns no
-- candidates. The verdict is then `Unique`, the `duplicate` arm never runs, and the gate is
-- green precisely because the feature was not exercised. The absence branch that is supposed to
-- be the *default* is what hid the bug on the *non-default* installation. That is the same shape
-- as the conversion defect in `run-crm-convert.sh`'s header, and the lesson is now written into
-- the gate that covers it: the duplicate path gets its own gate with a CRM present, because
-- "the module is absent" and "the module is present" are two different products and only one of
-- them was ever tested.
--
-- Two columns are added rather than one, because "duplicate of somebody" is not a decision an
-- operator can check. The REQ asks the queue to show the matched key and the score; `dedupe_key`
-- holds the first, and nothing stored the second at all — the score existed only in the value
-- the dedupe pass returned, so a duplicate filed today could not be re-examined tomorrow.
--
--   dedupe_contact_id — the contact the verdict matched (no FK: `crm_contacts` is another
--                       module's table and this migration must apply on an installation that
--                       does not have it, so a foreign key would make intake un-installable
--                       exactly where the degradation path is the only path). The pointer is
--                       validated by the same `to_regclass` guard the conversion path uses.
--   dedupe_score      — the confidence the verdict was made on, 0.0–1.0, as a real column
--                       rather than a jsonb field: it is a number the queue sorts and a check
--                       constraint can bound, and a check constraint is the only thing that
--                       makes "a score is between 0 and 1" true of the data instead of true of
--                       the code. `double precision` and **not** `real`: `real` is FLOAT4, Rust's
--                       `f64` is FLOAT8, and sqlx refuses to decode one into the other at
--                       runtime — the gate caught it on its first run, on every test that stored
--                       a score. A score with 7 significant digits does not need single
--                       precision, and the column type that cannot be read by the language that
--                       writes it is not a trade-off, it is a broken column.
--
-- `duplicate_of` is left exactly as it is — the lead-to-lead pointer the REQ documents, for a
-- submission that duplicates an *earlier lead from the same visitor* rather than an existing
-- contact. Nothing writes it today, and repointing a released column would break the row
-- semantics an operator may already have data against.

alter table crm_leads
    add column if not exists dedupe_contact_id uuid,
    add column if not exists dedupe_score double precision;

comment on column crm_leads.dedupe_contact_id is
    'The contact the dedupe verdict matched. No foreign key on purpose: crm_contacts belongs to REQ-051 and is absent on an installation without the CRM, where this column must still be writable (and stay null).';
comment on column crm_leads.dedupe_score is
    'Confidence the dedupe verdict was made on, 0.0-1.0. Stored so a duplicate filed today can be re-examined tomorrow without re-running the match against the contact table.';

-- A score outside the unit interval is not a weak match, it is a broken one: it means a caller
-- passed something that was never a confidence, and a queue that sorts by it would rank a
-- garbage row above a real one.
alter table crm_leads
    drop constraint if exists crm_leads_dedupe_score_check,
    add constraint crm_leads_dedupe_score_check
        check (dedupe_score is null or (dedupe_score >= 0.0 and dedupe_score <= 1.0));

-- The queue is read by "leads that are a duplicate claim", and `duplicate_of` alone is not that
-- set: a lead that *points at* a duplicate is not itself a duplicate, and listing it would
-- offer an operator a decision about a row that never claimed to be one. The index mirrors the
-- predicate `store::list_duplicates` uses, so the two cannot drift into different answers.
create index if not exists crm_leads_duplicates_idx
    on crm_leads (organization_id, received_at desc)
    where status = 'duplicate' or duplicate_of is not null;
