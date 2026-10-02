-- Fixture for run-crm-sla-sweep-plan.sh.
--
-- The shape is the one that makes an SLA sweep expensive, and it is deliberately the opposite of
-- the phone-index fixture. That fixture loaded twenty thousand LIVE leads in one tenant, so the
-- answer a planner gave was forced by the row count. Here the live set is tiny and the answered
-- set is enormous, because that is the shape a real installation is in six months after launch:
-- almost every lead has been answered, and the sweep's *answer* is a handful of rows.
--
-- Two things are being measured, and they are different measurements:
--
--   * `crm_leads_sla_idx` is partial on `first_response_at is null and status not in
--     ('spam','rejected','duplicate')`. Whether PostgreSQL will OFFER it depends on the planner
--     being able to prove that every row the query would read satisfies that predicate. A
--     query that says `status in ('new','assigned','contacted','qualified')` implies the
--     negation; a query that says nothing about status does not, however obviously true the
--     omission looks to a reader.
--
--   * The sweep's first read — `assignment_store::organizations_with_leads` — has no
--     `organization_id` predicate at all. It asks "which organizations have a live clock", so
--     the leading column of the index is the very column it does not constrain, and whether the
--     planner can still serve it with an ordered index-only walk is a question about the planner,
--     not about reading the code.
--
-- Ten tenants, so a plan that quietly degrades into "scan everything and group" has somewhere
-- to degrade into. Nine of them have nothing live, which is what makes the ninth-and-tenth
-- organization's row look expensive if the plan is wrong.

insert into organizations (id, name, slug, created_at, updated_at)
select ('33333333-3333-3333-3333-' || lpad(g::text, 12, '0'))::uuid,
       'sla org ' || g,
       'sla-org-' || g,
       now(), now()
from generate_series(1, 10) g
on conflict (id) do nothing;

-- Twenty thousand ANSWERED leads. These are the rows a sweep must not read: the partial
-- predicate excludes them, and the whole argument for a partial index is that they cost nothing.
insert into crm_leads
    (organization_id, first_name, last_name, email, phone, status, first_response_at,
     first_response_due_at, received_at, updated_at)
select ('33333333-3333-3333-3333-' || lpad((1 + (g % 9))::text, 12, '0'))::uuid,
       'Answered', g::text,
       'answered' || g || '@example.test',
       null,
       case when g % 3 = 0 then 'converted' else 'contacted' end,
       now() - interval '30 minutes',
       now() - interval '60 minutes',
       now() - (g || ' minutes')::interval,
       now()
from generate_series(1, 20000) g;

-- Two hundred TERMINATED leads: a real installation's spam and rejections, which the partial
-- predicate excludes by status rather than by having been answered.
insert into crm_leads
    (organization_id, first_name, last_name, email, status, first_response_at,
     first_response_due_at, received_at, updated_at)
select ('33333333-3333-3333-3333-' || lpad((1 + (g % 9))::text, 12, '0'))::uuid,
       'Junk', g::text,
       'junk' || g || '@example.test',
       case when g % 2 = 0 then 'spam' else 'rejected' end,
       null,
       now() - interval '90 minutes',
       now() - (g || ' minutes')::interval,
       now()
from generate_series(1, 200) g;

-- The LIVE set: four breached leads in organization 10 only, so the sweep's answer is four rows
-- and every one of them is in the tenant the first read has to reach.
insert into crm_leads
    (organization_id, first_name, last_name, email, status, first_response_at,
     first_response_due_at, escalated_at, received_at, updated_at)
select '33333333-3333-3333-3333-000000000010',
       'Live', g::text,
       'live' || g || '@example.test',
       'new',
       null,
       now() - interval '15 minutes',
       null,
       now() - (g || ' hours')::interval,
       now()
from generate_series(1, 4) g;

-- A policy for the reminder read, which joins one. `due_reminders` cannot run without it, and a
-- join to a table with no rows is a plan a different fixture would have produced.
insert into crm_sla_policies (id, organization_id, name, first_response_minutes, reminder_minutes,
                              active, created_at, updated_at)
select '44444444-4444-4444-4444-000000000001',
       '33333333-3333-3333-3333-000000000010',
       'fixture policy', 60, 30, true, now(), now()
on conflict (id) do nothing;

update crm_leads set sla_policy_id = '44444444-4444-4444-4444-000000000001'
 where organization_id = '33333333-3333-3333-3333-000000000010';

analyze crm_leads;
analyze crm_sla_policies;
analyze organizations;