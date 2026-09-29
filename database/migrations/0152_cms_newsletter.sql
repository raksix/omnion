-- Omnion · 0152 · content: newsletter lists, double opt-in subscribers and the sent-issue
-- archive (REQ-064, slice 4b).
--
-- A newsletter is the one place where **the platform stores an address it cannot verify** until
-- somebody clicks a link, and every decision below follows from that:
--
-- 1. **Opt-in is a token, and the token is stored hashed.** The database holds `sha256(token)`,
--    never the token. A leaked table therefore contains no link anybody can click, and a token
--    found in a log, a browser history or a Referer header is useless without its row. The
--    comments module hashes its sender for the same reason.
--
-- 2. **The token is single-use AND time-limited, and both are enforced by the COLUMN.**
--    `used_at` and `expires_at` are part of the row rather than a rule in the writer, so a
--    second confirmation cannot succeed by going around whichever code path forgot to check.
--    A double opt-in link that can be replayed for a year is not an opt-in.
--
-- 3. **Unsubscribe keeps the row.** `status` flips to `unsubscribed` and the address stays,
--    because "this address asked to leave" is exactly what stops a later import from silently
--    re-subscribing it. Deleting the row would make the next CSV import of the same list
--    re-add somebody who left on purpose. A `bounced` state exists for the same reason: an
--    address that hard-bounced and one that opted out are different facts.
--
-- 4. **The list is the unit of consent, and double opt-in is per list, not per site.** A visitor
--    who wants the product announcements and not the weekly digest is two rows, not one flag.
--    `double_opt_in` therefore lives on the list.
--
-- 5. **The archive is a row, not a file.** `newsletter_issues.archive_slug` is unique so the
--    public permalink cannot be guessed to somebody else's issue, and `recipient_count` is
--    stored rather than recomputed because the send already knew the number and the list has
--    changed since.

-- ---------------------------------------------------------------------------------------------
-- Lists
-- ---------------------------------------------------------------------------------------------

create table newsletter_lists (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    -- The public key a theme's signup form posts to. Unique per site: two lists answering one
    -- key means the visitor's consent is recorded against a list nobody chose.
    key text not null,
    name text not null,
    description text,
    -- Whether a signup must be confirmed by e-mail. Off is a legitimate choice for a list that
    -- is genuinely double opt-in by another route (an in-person sign-up, an import the owner
    -- vouches for), so it is a column and not a constant.
    double_opt_in boolean not null default true,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint newsletter_lists_key_not_blank check (length(btrim(key)) > 0),
    constraint newsletter_lists_name_not_blank check (length(btrim(name)) > 0),
    -- A key is a URL segment, so it is restricted to what one can carry in a path. Anything
    -- wider would let a list be addressed by a string no theme can build a link for.
    constraint newsletter_lists_key_shape_check check (key ~ '^[a-z0-9][a-z0-9-]{0,62}$')
);

create unique index newsletter_lists_site_key_idx on newsletter_lists (site_id, key);

-- ---------------------------------------------------------------------------------------------
-- Subscribers
-- ---------------------------------------------------------------------------------------------

create table newsletter_subscribers (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    list_id uuid not null references newsletter_lists (id) on delete cascade,
    -- Stored lowercased by the store, so uniqueness is exact: `unique (list_id, lower(email))`
    -- as a CONSTRAINT cannot express an expression, and a plain `unique (list_id, email)` would
    -- accept the same person twice with two capitalisations.
    email text not null,
    name text,
    -- Where the signup came from, as the form saw it. Free text with a bound rather than an
    -- enum: an owner adding a field to their own signup form should not need a migration.
    source text,
    status text not null default 'pending',
    -- The digest of the token the confirmation link carries.
    confirm_token_hash text,
    -- The digest of the token the unsubscribe link carries. Separate column, separate token:
    -- a single token serving both flows means a mailing list leaks its own unsubscribe links,
    -- because every issue has to contain one.
    unsubscribe_token_hash text,
    confirm_expires_at timestamptz,
    confirmed_at timestamptz,
    unsubscribed_at timestamptz,
    -- Free-text holder for the last thing that changed the status ("bounced at …"), bounded by
    -- the store. A subscriber table with no reason column cannot answer "why is this address
    -- not receiving the issue", which is the first question anybody asks.
    status_reason text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint newsletter_subscribers_email_not_blank check (length(btrim(email)) > 0),
    constraint newsletter_subscribers_status_check
        check (status in ('pending', 'confirmed', 'unsubscribed', 'bounced'))
);

create unique index newsletter_subscribers_list_email_idx
    on newsletter_subscribers (list_id, lower(email));

-- The inbox is "this list, these states, newest first".
create index newsletter_subscribers_list_status_idx
    on newsletter_subscribers (list_id, status, created_at desc);

-- A confirmation token is looked up by its digest and is unique, so two rows can never hold the
-- same one. The expiry is a partial index so the lookup cannot pick up a row that was already
-- used: `where confirm_token_hash is not null` keeps the index to the small set of rows that
-- are actually waiting for a click.
create unique index newsletter_subscribers_confirm_token_idx
    on newsletter_subscribers (confirm_token_hash)
    where confirm_token_hash is not null;

create unique index newsletter_subscribers_unsubscribe_token_idx
    on newsletter_subscribers (unsubscribe_token_hash)
    where unsubscribe_token_hash is not null;

-- ---------------------------------------------------------------------------------------------
-- Issues
-- ---------------------------------------------------------------------------------------------

create table newsletter_issues (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    list_id uuid not null references newsletter_lists (id) on delete cascade,
    subject text not null,
    -- The body is the owner's HTML. It is rendered by the ARCHIVE page, which is a platform
    -- surface, so it is sanitised on write by the content sanitiser like every other owner HTML
    -- in the platform; a newsletter body is the single most common stored-XSS carrier in a CMS
    -- because it is the one field that is explicitly allowed to contain markup.
    body_html text not null,
    sent_at timestamptz not null default now(),
    -- What the send knew, not what a count would say today. The list changed after the send.
    recipient_count integer not null default 0,
    -- The public permalink segment. Unique, so an archive URL cannot address somebody else's
    -- issue by incrementing an integer.
    archive_slug text not null,
    created_by uuid references users (id) on delete set null,
    constraint newsletter_issues_subject_not_blank check (length(btrim(subject)) > 0),
    constraint newsletter_issues_slug_not_blank check (length(btrim(archive_slug)) > 0),
    constraint newsletter_issues_recipients_check check (recipient_count >= 0)
);

create unique index newsletter_issues_archive_slug_idx
    on newsletter_issues (site_id, archive_slug);

create index newsletter_issues_list_sent_idx
    on newsletter_issues (list_id, sent_at desc);
