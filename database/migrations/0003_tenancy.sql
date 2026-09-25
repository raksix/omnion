-- Omnion · 0003 · tenancy: sites, domains and the site-scoped binding key
--
-- v0 of the tenancy model (docs/01-VISION.md §10, docs/07-IAM.md §7). An organization is the
-- tenant boundary and already exists (0001); this migration adds the properties a tenant
-- publishes: sites and the domains that address them. A site is one property of one
-- organization; hosts are unique platform-wide, so resolving an incoming request to its site
-- is a single indexed lookup. Released migrations are append-only (docs/05-VERSIONING.md).

-- Sites of an organization. `key` is the stable handle used by the admin API and later by
-- content rows; the display name can change freely.
create table sites (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    key             text        not null,
    name            text        not null,
    status          text        not null default 'active',
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint sites_key_format check (key ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    constraint sites_name_not_blank check (length(btrim(name)) > 0),
    constraint sites_status_check check (status in ('active', 'archived')),
    constraint sites_organization_key_key unique (organization_id, key)
);

create index sites_organization_id_idx on sites (organization_id);

-- Domains address a site. One host belongs to exactly one site platform-wide, and each site
-- carries at most one primary domain (`is_primary`), which the public renderer prefers when
-- it needs to build absolute links.
create table site_domains (
    id         uuid        primary key default gen_random_uuid(),
    site_id    uuid        not null references sites (id) on delete cascade,
    host       text        not null,
    is_primary boolean     not null default false,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint site_domains_host_format check (
        host ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$' and host = lower(host)
    )
);

create unique index site_domains_host_key on site_domains (host);
create index site_domains_site_id_idx on site_domains (site_id);
create unique index site_domains_primary_key on site_domains (site_id) where is_primary;

-- The site-scoped role binding gains its foreign key now that `sites` exists (see 0002).
-- Bindings written before the table existed carry a site id no site ever owned; they cannot
-- survive the constraint, so the narrow cleanup below retires them first.
delete from role_bindings
 where scope_type = 'site'
   and site_id is not null
   and not exists (select 1 from sites s where s.id = role_bindings.site_id);

alter table role_bindings
    add constraint role_bindings_site_id_fkey
    foreign key (site_id) references sites (id) on delete cascade;
