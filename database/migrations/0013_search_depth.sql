-- Omnion · 0013 · search depth: reindex runs and the wider provider set
--
-- docs/requests/REQ-002, slice 3. Two things the depth pass needs that 0012 did not carry:
--
-- * **A record of reindex passes.** `/settings/search` shows each provider's documents, when its
--   rows were last written and whether a pass is running, failed or has gone stale — and the
--   progress line is fed by the status endpoint while a pass runs. One row per pass (started,
--   finished, counted, timed, and the error when it failed) is what makes that honest: a state
--   is read from what the indexer did, never guessed from a document count.
-- * **The wider provider set.** Activity (audit entries and recorded events that point at an
--   entity the panel can open), translations and settings join pages, media, users and sites.
--   The default list of enabled providers grows with them, and an existing installation picks
--   the new keys up — it cannot have switched off a provider that did not exist yet.
--
-- Released migrations are append-only (docs/05-VERSIONING.md).

create table search_reindex_runs (
    id          bigserial   primary key,
    provider    text        not null,
    started_at  timestamptz not null default now(),
    finished_at timestamptz,
    indexed     bigint,
    pruned      bigint,
    duration_ms bigint,
    error       text,
    constraint search_reindex_runs_provider_format check (provider ~ '^[a-z][a-z0-9_]{0,62}$')
);

create index search_reindex_runs_provider_idx on search_reindex_runs (provider, started_at desc);

-- New installations enable every provider the build knows.
alter table search_settings
    alter column enabled_providers set default
        '{pages,media,users,sites,logs,translations,settings}'::text[];

-- An installation that already carries the 0012 default has never seen these keys, so they are
-- appended; a key an operator removed by hand is never re-added, because the append only adds
-- what the row does not already carry.
update search_settings s
set enabled_providers = s.enabled_providers || missing.keys,
    updated_at = now()
from (
    select coalesce(array_agg(k), '{}'::text[]) as keys
    from unnest(array['logs', 'translations', 'settings']) as k
    where not (k = any(coalesce((select enabled_providers from search_settings where id = 1),
                                '{}'::text[])))
) as missing
where s.id = 1
  and array_length(missing.keys, 1) >= 1;
