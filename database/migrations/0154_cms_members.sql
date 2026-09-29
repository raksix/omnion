-- Omnion · 0154 · content: visitor membership accounts and page/block gating (REQ-064, slice 4c).
--
-- A member is a **site visitor who made an account**, and the single most important property of
-- this file is the one the REQ calls "the most important boundary in this REQ": a member is
-- NOT a panel identity. So, in order:
--
-- 1. **Three tables, no foreign key to `users`, no column shared with `sessions`.**
--    `cms_members` has no `organization_id` that a binding could be attached to, no role row
--    points at it, and nothing in `crates/identity` reads it. A member cannot be promoted to a
--    user because there is no path in the schema for a promotion to travel, and a promotion
--    added later would have to add that path deliberately rather than inherit it from a
--    `users.id`-shaped id. `roles text[]` is a free-text list owned by the SITE, not the IAM
--    system, which is why a member can hold `editor` without that meaning anything to `roles`.
--
-- 2. **The password is an Argon2id PHC string, and the store never sees the plaintext twice.**
--    Same parameters as `omnion_identity::password` (19 MiB, t=2) so a member's credential cost
--    matches a user's, and a dump of this table is not cheaper to attack than a dump of
--    `users`. The hash is what the unique/blank checks see; a row with no hash is a member the
--    site invited by e-mail and has not claimed yet — which is why the column is nullable
--    rather than a NOT NULL empty string that would look like a valid hash.
--
-- 3. **A verification token is stored hashed, is single-use and expires, and both are facts
--    about the ROW.** `used_at`/`expires_at` are columns, not rules in a writer, so re-verifying
--    cannot succeed by going around whichever path forgot to check. A signup link that works
--    forever is not a verification.
--
-- 4. **A session is a row, not a JWT.** `cms_member_sessions.token_hash` is unique, so
--    sign-out is a delete and a stolen cookie is revocable — the property a self-contained token
--    cannot have. It is a SEPARATE table from `sessions` on purpose: the platform's session
--    loader must never accept a member cookie as a panel session, and it cannot, because it
--    never looks here.
--
-- 5. **Gating is a column on the page, and the roles it names are the site's own.**
--    `pages.visibility` moves from a free-text SEO field it never had to a real enum, and
--    `visibility_roles` names member roles. A page that requires `editor` and a member who
--    holds `subscriber` is a 404 — not a 403 — because "you may not see this" and "this does
--    not exist" are the same answer to a visitor who was never told the page was there.

-- ---------------------------------------------------------------------------------------------
-- Members
-- ---------------------------------------------------------------------------------------------

create table cms_members (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    -- Stored lowercased by the store, so uniqueness is exact. `unique (site_id, lower(email))`
    -- cannot be a CONSTRAINT because it is an expression, and a plain `unique (site_id, email)`
    -- would accept the same person twice with two capitalisations of the same address.
    email text not null,
    name text,
    -- Argon2id PHC string (`$argon2id$…`), or NULL while the member is an invited address that
    -- has not claimed the account yet. NULL means "no password exists yet", which is a
    -- different fact from "the password is the empty string".
    password_hash text,
    -- Free-text roles OWNED BY THE SITE. Deliberately not `roles.id`: a site may call its own
    -- member "editor" and that word must never resolve against the IAM role table, or a site
    -- would be able to hand a visitor the platform's own `content.pages.update`.
    roles text[] not null default '{}',
    status text not null default 'pending',
    verified_at timestamptz,
    last_signin_at timestamptz,
    -- The sign-in log the panel's detail view lists. Bounded by the store, like every other
    -- free-text column a caller can grow.
    signin_note text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cms_members_email_not_blank check (length(btrim(email)) > 0),
    constraint cms_members_status_check check (status in ('pending', 'verified', 'blocked')),
    -- A verified member has a moment, a pending one has not been verified and a blocked one is
    -- refused rather than merely unverified. Without this, "verified_at is set" and
    -- "status = 'verified'" are two answers to one question that can disagree.
    constraint cms_members_verified_consistent check (
        (status = 'verified') = (verified_at is not null)
    ),
    -- A password hash is either a PHC string or absent. `password_hash = ''` would verify
    -- against nothing while looking like a claimed account.
    constraint cms_members_password_shape check (
        password_hash is null or password_hash like '$argon2id$%'
    )
);

create unique index cms_members_site_email_idx on cms_members (site_id, lower(email));

-- The members table is "this site, these states, newest first".
create index cms_members_site_status_idx on cms_members (site_id, status, created_at desc);

-- ---------------------------------------------------------------------------------------------
-- Tokens (verification and password reset)
-- ---------------------------------------------------------------------------------------------

create table cms_member_tokens (
    id uuid primary key default gen_random_uuid(),
    member_id uuid not null references cms_members (id) on delete cascade,
    kind text not null,
    -- sha256 of the token. The raw token is returned to the caller (which mails it) and is
    -- never persisted, so a leaked table contains no link anybody can click.
    token_hash text not null,
    -- The instant the link stops working. There is deliberately NO CHECK on this column
    -- comparing it to `created_at`: the first draft had one (`expires_at > created_at - 1
    -- minute`), and it is wrong in a way only a test found. A CHECK against `created_at` says
    -- "a token may not be created already dead", which is a real rule, but it also forbids
    -- **ageing a live token** — moving a live row's expiry into the past is the only way to
    -- test the 48-hour window without waiting 48 hours, so the constraint made the expiry
    -- untestable and, in the field, would have to be dropped to be reachable. Expiry is judged
    -- against `now()` by the store, in the same transaction that consumes the row, and
    -- `used_at` is the schema's half of single-use.
    expires_at timestamptz not null,
    -- Set when the link is used. Part of the row rather than a rule in the writer, so a second
    -- use cannot succeed by going around whichever code path forgot to check.
    used_at timestamptz,
    created_at timestamptz not null default now(),
    constraint cms_member_tokens_kind_check check (kind in ('verify', 'reset'))
);

-- Unique across the whole table, not per member: a token that is the digest of two rows is a
-- token whose click verifies whichever row loads first.
create unique index cms_member_tokens_hash_idx on cms_member_tokens (token_hash);

-- A lookup only ever wants a token that is still live, so the index is partial: used rows are
-- the majority after a while and none of them can be clicked again.
create index cms_member_tokens_live_idx
    on cms_member_tokens (member_id, kind)
    where used_at is null;

-- ---------------------------------------------------------------------------------------------
-- Sessions
-- ---------------------------------------------------------------------------------------------

create table cms_member_sessions (
    id uuid primary key default gen_random_uuid(),
    member_id uuid not null references cms_members (id) on delete cascade,
    -- The cookie carries `token`; the table holds sha256(token). Revocable, and a dump of this
    -- table cannot be replayed as a session.
    token_hash text not null,
    expires_at timestamptz not null,
    last_seen_at timestamptz not null default now(),
    -- Hashed, like every other stored visitor identifier in the content modules: the comments
    -- and newsletter tables store digests for the same reason.
    ip_hash text,
    ua_hash text,
    created_at timestamptz not null default now()
    -- No CHECK on `expires_at` here either, for the same reason as `cms_member_tokens`: a
    -- session that has aged out must be prunable, and a constraint that compares the expiry to
    -- the creation time would make "aged out" a state the row cannot be moved into.
);

create unique index cms_member_sessions_hash_idx on cms_member_sessions (token_hash);
create index cms_member_sessions_member_idx on cms_member_sessions (member_id, created_at desc);

-- ---------------------------------------------------------------------------------------------
-- Page gating
-- ---------------------------------------------------------------------------------------------

-- The gate lives on the page rather than in a side table so that the public render reads it in
-- the same query that already fetches the page, and so a page cannot be published with a gate
-- that points at a row that no longer exists.
alter table pages
    add column visibility text not null default 'public',
    add column visibility_roles text[] not null default '{}';

-- Every CHECK is its own ALTER rather than a clause in the block above, because a `--` comment
-- between two clauses makes PostgreSQL drop the rest of the line, which here is the entire
-- remaining statement.
alter table pages
    add constraint pages_visibility_check
        check (visibility in ('public', 'members', 'roles'));

-- A page gated on roles must NAME them. `visibility = 'roles'` with an empty array is a page
-- nobody can reach, which reads in the panel as "gated" and behaves as "deleted".
alter table pages
    add constraint pages_visibility_roles_check check (
        visibility <> 'roles' or cardinality(visibility_roles) > 0
    );

-- A role name is a menu-item audience already in this platform, so it is bounded the same way
-- and in the same shape rather than accepting arbitrary text.
alter table pages
    add constraint pages_visibility_roles_shape check (
        visibility_roles <@ array['subscriber', 'editor', 'author', 'contributor', 'owner', 'admin']
    );

-- The settings a site's signup form reads. One row per site, created with the site, so the
-- public signup never has to answer "no such settings" and fall back to a guess.
create table cms_member_settings (
    site_id uuid primary key references sites (id) on delete cascade,
    -- Whether the public signup form exists at all. A site that only ever invites by e-mail
    -- turns this off, and the public route answers 404 rather than accepting a signup nobody
    -- will ever read.
    signup_enabled boolean not null default true,
    -- Whether a new member must click a verification link before signing in. Off is a
    -- legitimate choice for a low-stakes members area, so it is a column and not a constant.
    require_verification boolean not null default true,
    -- Roles handed to every new member. The site's own words; see `cms_members.roles`.
    default_roles text[] not null default '{}',
    -- Where the theme sends a visitor after a successful sign-in.
    post_signin_redirect text,
    -- What a signed-out visitor meets on a gated page: a 404 that discloses nothing, or a
    -- sign-in prompt that admits the page is there.
    --
    -- **`not_found` is the DEFAULT, and the choice of default is the acceptance criterion.**
    -- The REQ asks that "a gated page returns 404 to a signed-out visitor" with no
    -- configuration step, so a platform that shipped `prompt` here would answer 401 by default
    -- and quietly fail its own criterion on every fresh install. `prompt` stays available and is
    -- a legitimate product decision — a paid members area often *wants* to advertise itself — but
    -- an owner has to choose it, because choosing it is what discloses the page.
    gated_page_behaviour text not null default 'not_found',
    updated_at timestamptz not null default now(),
    updated_by uuid references users (id) on delete set null,
    constraint cms_member_settings_gated_behaviour_check
        check (gated_page_behaviour in ('prompt', 'not_found'))
);

-- `pages.visibility_roles` and `cms_members.roles` must agree about which words exist, or a
-- page can name a role no signup will ever grant and be permanently unreachable with no
-- diagnostic. One CHECK cannot span two tables, so the shared list is asserted in the store's
-- validation and pinned by the integration walk; the shape constraint above is the half the
-- database can enforce on its own.

-- ---------------------------------------------------------------------------------------------
-- Events
-- ---------------------------------------------------------------------------------------------

-- `members.member.created` · `.verified` · `.blocked` are emitted by the route, which is the
-- only place that knows an action was taken by somebody. The catalogue entry lives in
-- `crates/events`, not here, and an event name that is emitted but absent from the catalogue is
-- exactly the failure `every_emitted_name_is_in_the_catalogue` exists to catch.
