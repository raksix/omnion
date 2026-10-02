-- 0156_crm_lead_sla_reminder_claim.sql — one reminder per lead, enforced by the database.
--
-- REQ-117, slice 3 (the SLA worker). The acceptance line "SLA: a lead assigned under a
-- business-hours policy has a due time computed inside the window ... a breach emits
-- crm.lead.sla_breached and notifies the escalation target exactly once" has been partly
-- proved for several slices, but the parts that are *exactly once* were proved on a predicate
-- rather than on a guarantee:
--
--     and not exists (select 1 from crm_lead_events e
--                     where e.lead_id = l.id and e.kind = 'sla_reminded')
--
-- That predicate is correct for one caller and wrong for two. The platform's workers are
-- separate processes that may both run a tick at the same second, and the read that drives the
-- notification happens *before* the line that would make the filter true — so both workers see
-- "no reminder yet", both notify the same owner about the same lead, and the trail records one
-- line for two notifications. The owner sees the deadline twice and learns to ignore the
-- reminder, which is the outcome this whole column exists to prevent.
--
-- This is the third time this module has met this exact shape, and the other two are named in
-- the code that already handles them: the round-robin cursor (`claim_assignment`, locked with
-- `select … for update`) and the autoresponder reservation (`autoresponder_store::claim`).
-- Both answers were "make the claim an insert whose row count is the decision". So:
--
--   create unique index crm_lead_events_sla_reminded_idx
--     on crm_lead_events (lead_id) where kind = 'sla_reminded';
--
-- and `assignment_store::mark_reminded` inserts with `on conflict do nothing`, returning
-- `rows_affected() == 1`. A duplicate key makes the insert match zero rows, so the loser's
-- `false` is a fact read out of the database rather than an inference from a query it ran
-- microseconds earlier.
--
-- **Why a partial index and not a unique constraint on (lead_id, kind).** The trail is
-- append-only and every other kind is *legitimately* repeatable: an `assigned` line appears
-- once per hand-over, a `responded` line once per response, and a generic unique constraint on
-- the pair would refuse the second real hand-over. Only `sla_reminded` is once-per-lead by
-- definition, so only that kind is indexed — and the partial predicate is what keeps a
-- constraint on a subset from becoming a constraint on the table.
--
-- **Additive, and safe on an installation that has already reminded somebody by hand.** The
-- index is built over the existing rows, so an organization that already holds two
-- `sla_reminded` lines for one lead would fail this migration. That is the correct outcome
-- rather than a quiet `create index concurrently` retry: a duplicate reminder is a fact about
-- the data, and the operator who wants it gone can delete the line the worker wrote twice
-- (they are identical apart from `created_at`) and re-run. The alternative — dropping the
-- duplicates silently inside a migration — is a migration that edits history.

create unique index if not exists crm_lead_events_sla_reminded_idx
  on crm_lead_events (lead_id)
  where kind = 'sla_reminded';

comment on index crm_lead_events_sla_reminded_idx is
  'One reminder per lead. The SLA worker inserts this line with on conflict do nothing and '
  'uses the row count as the decision, so two workers racing over one deadline notify once.';
