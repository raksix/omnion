-- Omnion · 0007 · onboarding: the first-run state and the site theme
--
-- The installation wizard (docs/BUILD-BACKLOG.md P10, docs/requests/REQ-050) needs two things
-- the schema did not carry yet: the presentation setting a site renders with, and the singleton
-- row that records how far the first run has come. Released migrations are append-only
-- (docs/05-VERSIONING.md).

-- A site renders through exactly one theme. The value is the manifest key of a bundled theme
-- (`themes/<key>/omnion.theme.json`); the renderer falls back to the default theme for a key it
-- does not know, so a stale key degrades instead of failing a request.
alter table sites
    add column theme text not null default 'minimal';

alter table sites
    add constraint sites_theme_format check (theme ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$');

-- The first-run record. An installation has exactly one first run, hence the singleton row
-- (`id = 1`). Each step is recorded when it happens, so the wizard resumes from the first step
-- that has none — and an installation that got its first account from the environment bootstrap
-- can still finish the flow (the earliest active account acts as the owner then).
create table onboarding_state (
    id              smallint    primary key default 1,
    owner_user_id   uuid        references users (id) on delete set null,
    organization_id uuid        references organizations (id) on delete set null,
    site_id         uuid        references sites (id) on delete set null,
    theme_at        timestamptz,
    ai_step         text        not null default 'pending',
    completed_at    timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint onboarding_state_singleton check (id = 1),
    constraint onboarding_state_ai_step_check check (ai_step in ('pending', 'skipped'))
);
