-- 0163_crm_source_sla_policy.sql — the column the SLA policy lookup has been reading since
-- slice 2 shipped.
--
-- REQ-117, slice 2. Found by the entry-point gate `scripts/qa/run-crm-capture-routing.sh`.
--
-- ## The defect, stated as it was found
--
-- `crm_intake_sources` has never had an `sla_policy_id` column. `0055_crm_lead_intake.sql`
-- created the source table without it, `0056_crm_assignment_sla.sql` created
-- `crm_sla_policies` and the lead-side `sla_policy_id`, and the two were never joined up:
-- the *lead* column exists, the *source* column does not.
--
-- `assignment_store::policy_for_source` — the function that answers "which policy does this
-- source's leads run on" — opens with
--
--     select p.… from crm_sla_policies p join crm_intake_sources s on s.sla_policy_id = p.id
--
-- against a column that is not there. Every execution of that arm raises PostgreSQL `42703`,
-- `column "sla_policy_id" of relation "crm_intake_sources" does not exist`.
--
-- ## Why twenty-four ticks of green gates did not see it
--
-- Because **nothing called it.** `policy_for_source` had exactly one caller and it was a test
-- — and that test passes a `source_id` of `None` or an organization with no policy, so the
-- first arm is not always reached and the fallback answers. The whole slice-2 routing chain
-- (`claim_assignment`, `stamp_assignment`, `policy_for_source`) shipped without a production
-- caller; this tick wires it into `store::capture`, and the very first capture in the new gate
-- raised `42703` where the test expected a deadline.
--
-- That is the seventh time this module shipped a correct, documented, unit-tested function
-- that no installation could reach. The lesson is now in the gate's header rather than in this
-- migration: **a gate that begins at the function proves the function.** A function with no
-- caller can be perfect and still never run.
--
-- ## What this column is for
--
-- It is the per-source override for the SLA clock. Without it the REQ's own spec — "SLA:
-- policies table with `Applies to` (source/rule)" — cannot be expressed at all: an
-- organization has one catch-all policy and every source runs on it, so a phone enquiry and a
-- 200 000-quote enquiry get the same clock.
--
-- Nullable on purpose, and `on delete set null` rather than `on delete cascade`: a source's
-- policy is a *default* the lead then inherits. Deleting a policy must hand its sources back
-- to the organization's first active policy — which is exactly what `policy_for_source`'s
-- second arm answers — and must never delete a source, least of all one that has already
-- produced leads somebody is working on.
--
-- The check constraint is on the *organization* rather than on a cross-row rule on purpose: a
-- source may only name a policy of its own organization, and a foreign key cannot say that
-- across two tables without a composite unique index. That check belongs in the store's
-- validator, and `policy_for_source` is already scoped by organization on both sides of its
-- join, so a policy of another organization is not found here either.
--
-- The additive form is used (`add column if not exists`) so that a database which somehow has
-- the column — a developer's branch that got there first — applies cleanly rather than
-- failing on `42701`. There is no existing row to backfill: every source's value is null,
-- which is the documented "use the organization's first active policy" answer.

alter table crm_intake_sources
    add column if not exists sla_policy_id uuid
        references crm_sla_policies (id) on delete set null;

-- The lookup joins source to policy on this column inside an organization's own rows. Without
-- the index that join is a sequential scan of every source in the installation, on the path
-- of every captured lead — the one query on this path that cannot use a primary key.
create index if not exists crm_intake_sources_sla_policy_idx
    on crm_intake_sources (sla_policy_id) where sla_policy_id is not null;
