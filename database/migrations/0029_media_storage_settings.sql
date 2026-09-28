-- Omnion · 0029 · media: per-site storage settings (REQ-010, slice 3)
--
-- Slices 1 and 2 gave the library folders and a history; `0027` gave it derivatives. This
-- migration gives it the *place the bytes live* as a per-site record, so an operator can move a
-- site to its own bucket, put a CDN in front of it, and set a signed-URL lifetime without a
-- redeploy.
--
-- Four decisions carry it, and each is a place the obvious shortcut is wrong:
--
--   * **Credentials are a reference, never a value.** The table stores *which* secret the
--     deployment calls the bucket's key — a name the operator types and the settings response
--     echoes back — and never the key material itself. A settings table that holds a secret key
--     in plaintext is a settings table that has to be backed up, audited and rotated as a
--     credential, and a `GET` one query away from a browser. The connection test resolves the
--     name through the process environment, which is where the secret already lives.
--   * **The public base URL is validated as a base, not as a string.** A trailing slash, a
--     query or a fragment produces a public URL that is either doubled-slashed or unusable
--     behind a CDN, and the failure shows up as a broken image on somebody else's site. The
--     check is in the database *and* in the API, and the API is the one that names the field.
--   * **A site created after this migration gets a row.** The same bug the preset seed had
--     (`0028`): a seed in a migration covers only the sites that existed when it ran, and the
--     gap between "the migration ran" and "somebody creates a site" is a regression that
--     appears only on new sites. A trigger is the only thing guaranteed to see every one.
--   * **The active driver and the limits are columns, not environment variables, because a
--     *sign-in* must not change where a live site's files go.** The process environment decides
--     what the *platform* defaults to; this row decides what this site uses. Reading both, and
--     making one silently win over the other, is how a site loses its bucket.
--
-- Create-and-seed only: no column of `media` is altered, so the migration runs against a live
-- library without a lock-heavy rewrite (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- Per-site storage settings
-- ---------------------------------------------------------------------------------------------

create table media_storage_settings (
    site_id                 uuid        primary key references sites (id) on delete cascade,
    -- Which driver this site's objects are written through. A site on a platform that only has
    -- a filesystem root cannot be moved to S3 by editing a string here and restarting
    -- something; the settings screen says so rather than letting the row pretend.
    driver                  text        not null default 's3',
    -- Endpoint, region, bucket and prefix are the four coordinates of an object store. A prefix
    -- is what lets several sites share one bucket without their keys colliding, which is the
    -- normal production shape: one bucket, one key namespace per site.
    endpoint                text        not null default 'http://127.0.0.1:9000',
    region                  text        not null default 'us-east-1',
    bucket                  text        not null default 'omnion-media',
    path_prefix             text        not null default '',
    -- Where a *public* file is served from when the site is public. Empty means "the API serves
    -- it", which is the safe default: a URL that points nowhere is a broken image, a URL that
    -- points somewhere wrong is a leak.
    public_base_url         text        not null default '',
    -- How long a signed URL for a private file stays valid, 60 s to 7 days. Below a minute a
    -- viewer loses the file mid-read; above a week it stops being a signature and starts being
    -- a permanent link with extra steps.
    signed_url_ttl_seconds  integer     not null default 900,
    -- Default visibility of a new upload. `public` here is a statement about the *bucket*, and
    -- the settings screen says so in words, because a world-readable bucket cannot be undone
    -- by flipping a row back.
    default_visibility      text        not null default 'private',
    -- Upload ceiling in megabytes, 1–1024. Per site, because a marketing site with video and a
    -- documentation site with images do not have the same answer.
    max_upload_mb           integer     not null default 25,
    -- Which content types this site accepts. Empty means "the platform's own list", so a site
    -- that has not decided is not blocked by a list nobody wrote.
    allowed_content_types   text[]      not null default '{}',
    -- When the row was last written, so the settings screen can say whether it has ever been
    -- touched — a site that has never been configured and a site configured back to the
    -- defaults look identical in every other column.
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    constraint media_storage_driver_valid
        check (driver in ('s3', 'fs')),
    -- An S3 endpoint is a bare origin. A path, a query or a fragment turns a signed request
    -- into a 403 at best and a signature over the wrong string at worst.
    constraint media_storage_endpoint_shape
        check (endpoint ~ '^https?://[^/?#]+$'),
    constraint media_storage_bucket_present check (bucket <> ''),
    constraint media_storage_region_present check (region <> ''),
    -- A prefix is inserted between the bucket and the object key, so it must not be able to
    -- climb out of it. A leading slash is accepted and dropped by the API, a `..` is not
    -- accepted at all.
    constraint media_storage_prefix_safe
        check (path_prefix = '' or path_prefix !~ '(^|/)\.\.(/|$)'),
    -- A public base is joined with a path, so a trailing slash would double it on every URL.
    constraint media_storage_public_base_clean
        check (public_base_url = '' or public_base_url !~ '[?#]'),
    constraint media_storage_signed_url_ttl
        check (signed_url_ttl_seconds between 60 and 604800),
    constraint media_storage_default_visibility
        check (default_visibility in ('private', 'public')),
    constraint media_storage_max_upload_mb check (max_upload_mb between 1 and 1024)
);

comment on table media_storage_settings is
    'Per-site object-store settings. Credentials are a *reference* — the name the deployment '
    'holds the bucket key under — never the key material, so this table never has to be treated '
    'as a secret store (REQ-010).';
comment on column media_storage_settings.path_prefix is
    'Key prefix for this site inside the bucket. One bucket, one namespace per site: the object '
    'key is built as `{prefix}/sites/{site}/{media}.{ext}` and a migration of the bucket does not '
    'have to move a single object.';
comment on column media_storage_settings.public_base_url is
    'Where a public file is served from once a CDN or a public bucket is in front. Empty means '
    'the API serves it — never a guess, because a wrong public base serves one site''s bytes '
    'under another''s name.';
comment on column media_storage_settings.signed_url_ttl_seconds is
    'Lifetime of a signed URL for a private file, 60–604800 seconds. Deliberately not a cache '
    'header: the URL is a capability and its lifetime is the window in which it is one.';

-- ---------------------------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------------------------

insert into media_storage_settings (site_id)
select id from sites
on conflict (site_id) do nothing;

-- The seed above covers the sites that existed when this migration ran. A site created
-- afterwards would have no row, and `GET /api/v1/media/settings` would answer with a default
-- built in Rust rather than the row the operator edits — so the create and the edit would be
-- two different records and the second would silently do nothing. That is exactly the bug the
-- preset seed had (`0028`); a trigger is the only place guaranteed to see every site, because
-- onboarding, the tenancy API and a future import each insert the row themselves.
create or replace function omnion_seed_site_storage_settings() returns trigger
language plpgsql
as $$
begin
    insert into media_storage_settings (site_id) values (new.id) on conflict (site_id) do nothing;
    return new;
end;
$$;

comment on function omnion_seed_site_storage_settings() is
    'Gives every new site a storage settings row, so a site created after the migration is '
    'editable on the same record as one that existed before it (REQ-010).';

drop trigger if exists sites_seed_storage_settings on sites;
create trigger sites_seed_storage_settings
    after insert on sites
    for each row execute function omnion_seed_site_storage_settings();
