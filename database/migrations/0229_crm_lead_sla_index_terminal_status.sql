-- The SLA index must exclude every status `is_open` calls terminal.
--
-- `crm_leads_sla_idx`, created by `0055_crm_lead_intake.sql`, is partial on
-- `first_response_at is null and status not in ('spam','rejected','duplicate')`.
--
-- Three of the platform's four terminal statuses are named there. `converted` is not — and
-- `vocabulary::is_open` calls it terminal: "converted, duplicate, spam and rejected are terminal:
-- the SLA clock reads only the rows that are still waiting, and a clock that keeps running against
-- a lead somebody marked as spam is a breach notification nobody can act on."
--
-- The consequence is not slow, it is permanent. `organizations_with_leads` decides which tenants
-- the worker walks; a converted lead that was never answered satisfies its predicates, so its
-- tenant is named on every tick. Neither conversion path writes `first_response_at`
-- (`convert_lead` writes status/contact_id/deal_id/converted_at, `mark_quote_accepted` writes
-- status/quote_id/converted_at), and `record_response` is reached only from the operator's
-- respond route — so a lead converted through the panel's own Convert button stays unanswered
-- for ever and keeps naming its tenant. Both consumer reads filter `status in ('new', …)` and
-- return nothing for it, and the `limit` bounds organizations, so on a busy platform these
-- tenants consume the batch a live one needs.
--
-- `count_breached` counts the same rows for the panel's inbox badge, so the badge read "13
-- breached" over four real breaches on the gate's ten-tenant fixture.
--
-- The index is dropped and rebuilt because a partial index's predicate is not something a query
-- can amend: the planner proves predicates, and `not_closed_statuses_sql` — now derived from
-- STATUSES rather than hand-written, which is what let `converted` leak — names four statuses
-- where the stored predicate named three. The four-status query implies the three-status
-- predicate, so the rebuilt index serves both spellings; `scripts/qa/run-crm-terminal-status.sh`
-- asserts the plan as well as the answer, because a correct predicate that the planner cannot
-- prove is a seq scan and an answer-only gate would be green on it.
--
-- Plain `create index`, not `concurrently`, and the same reason as `0200` and `0228`: a migration
-- runs inside a transaction, `concurrently` cannot, and making it work took a fresh install to
-- unbootable. The lock is a one-off rebuild of one index on one table.
drop index if exists crm_leads_sla_idx;

-- The status list is `STATUSES` minus `is_open`, in `STATUSES`'s own order: converted, duplicate,
-- spam, rejected. `vocabulary::not_closed_statuses_sql` builds the same four in the same order,
-- and the gate reads this file and that function and asks the planner which one it will use — so
-- a ninth status added to the crate without this file naming it fails a check rather than
-- producing a slow read nobody can explain.
create index crm_leads_sla_idx on crm_leads (organization_id, first_response_due_at)
    where first_response_at is null
      and status not in ('converted', 'duplicate', 'spam', 'rejected');