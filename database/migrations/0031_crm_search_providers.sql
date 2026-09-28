-- The CRM joins the search index (docs/requests/REQ-051 · REQ-002).
--
-- A search provider is enabled per installation, and the set is a stored array rather than "every
-- provider this build knows" — which is why a new module needs this statement the day it ships.
-- The rule is the same one migration 0013 set for the slice-3 providers: the **column default**
-- changes for a fresh installation, and an existing row is only *appended to*, so a key an
-- operator deliberately removed by hand comes back.
--
-- The keys are appended in one statement and the check below makes it a no-op when they are
-- already there, so re-running the file is as harmless as re-running any other migration.

alter table search_settings
    alter column enabled_providers set default
        '{pages,media,users,sites,logs,translations,settings,contacts,companies,deals}'::text[];

update search_settings s
set enabled_providers = s.enabled_providers || missing.keys,
    updated_at = now()
from (
    select coalesce(array_agg(k order by k), '{}'::text[]) as keys
    from unnest(array['companies', 'contacts', 'deals']) as k
    where not (k = any(coalesce((select enabled_providers from search_settings where id = 1),
                                '{}'::text[])))
) as missing
where s.id = 1
  and array_length(missing.keys, 1) >= 1;
