-- Fixture for run-crm-terminal-status.sh.
--
-- ## The shape
--
-- Ten tenants. Nine of them are quiet: every lead they hold is `converted`, every one of them
-- unanswered, and every one of them past its deadline. The tenth holds four live breached
-- leads in `new`.
--
-- ## Why `converted` and not `spam`
--
-- Tick 71's fixture proved that `spam`/`rejected`/`duplicate` are named by the sweep's first
-- read and answered by neither consumer. `converted` is the **fourth** terminal status and it is
-- the one `not_closed_statuses_sql` still admits — because the negated list was written by
-- hand as the three statuses the migration's own index predicate names, and the index predicate
-- was written before `converted` stopped meaning "still work".
--
-- ## Why converted leads are UNANSWERED here
--
-- Because that is what the code produces. `convert_lead` and `mark_quote_accepted` write
-- `status`, `converted_at`, `contact_id` and `deal_id`. Neither writes `first_response_at`, and
-- no other path does: `record_response` is reached only from the operator's respond route. A
-- lead converted through the panel's own Convert button is therefore permanently unanswered —
-- which is what makes the counter below permanent rather than merely wrong.

insert into organizations (id, name, slug, created_at, updated_at)
select ('44444444-4444-4444-4444-' || lpad(g::text, 12, '0'))::uuid,
       'terminal org ' || g,
       'terminal-org-' || g,
       now(), now()
from generate_series(1, 10) g
on conflict (id) do nothing;

-- Nine quiet tenants: converted, never answered, long past the deadline.
insert into crm_leads
    (organization_id, first_name, last_name, email, status, first_response_at,
     first_response_due_at, converted_at, received_at, updated_at)
select ('44444444-4444-4444-4444-' || lpad(g::text, 12, '0'))::uuid,
       'Won', g::text,
       'won' || g || '@example.test',
       'converted',
       null,
       now() - interval '90 minutes',
       now() - interval '80 minutes',
       now() - (g || ' minutes')::interval,
       now()
from generate_series(1, 9) g;

-- One busy tenant: four unanswered leads past their deadline, still `new`.
insert into crm_leads
    (organization_id, first_name, last_name, email, status, first_response_at,
     first_response_due_at, received_at, updated_at)
select ('44444444-4444-4444-4444-000000000010')::uuid,
       'Live', g::text,
       'live' || g || '@example.test',
       'new',
       null,
       now() - interval '45 minutes',
       now() - (g || ' minutes')::interval,
       now()
from generate_series(1, 4) g;

-- A converted lead that WAS answered, as a control: it is excluded by `first_response_at`, so
-- a gate that passes because the index predicate happens to catch this row is passing for the
-- wrong reason.
insert into crm_leads
    (organization_id, first_name, last_name, email, status, first_response_at,
     first_response_due_at, converted_at, received_at, updated_at)
values ('44444444-4444-4444-4444-000000000010',
        'Answered', '1', 'answered1@example.test',
        'converted', now() - interval '100 minutes', now() - interval '120 minutes',
        now() - interval '110 minutes', now() - interval '130 minutes', now());