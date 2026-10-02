-- Fixture for run-crm-phone-index.sh: one tenant, twenty thousand leads whose phone numbers
-- are stored the way an operator's mapping produces them (verbatim from the payload, `+` and
-- all), so the stored-side expression has real work to do.
insert into organizations (id, name, slug, created_at, updated_at)
values ('22222222-2222-2222-2222-222222222222', 'phone idx org', 'phone-idx-org', now(), now())
on conflict (id) do nothing;

insert into crm_leads (organization_id, first_name, last_name, email, phone, status, dedupe_key, received_at, updated_at)
select '22222222-2222-2222-2222-222222222222',
       'Lead', g::text,
       'lead' || g || '@example.test',
       '+90 5' || lpad(g::text, 6, '0') || ' ' || lpad((g * 7 % 100)::text, 2, '0') || ' 11 22',
       'new',
       '+905' || lpad(g::text, 6, '0'),
       now() - (g || ' minutes')::interval,
       now()
from generate_series(1, 20000) g;

analyze crm_leads;
