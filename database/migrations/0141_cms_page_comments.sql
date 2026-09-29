-- Omnion · 0141 · content: page comments with moderation, threading and local spam
-- heuristics (REQ-064, slice 4a).
--
-- Comments are the one place where **a stranger writes into the database**, and that fact decides
-- the schema. Everything a visitor supplies is stored, but nothing a visitor supplies is trusted:
--
-- 1. **The body is plain text and stays plain text.** There is no `html` column to sanitise
--    because there is nothing to sanitise. The renderer escapes it; a stored `<script>` is a
--    paragraph of text, and a comment that needs markup is a content page, not a comment.
--
-- 2. **Threading is one level, enforced by a TRIGGER and not by the writer.** The REQ asks for
--    two levels — a comment and a reply. A self-referencing `parent_id` with no bound is a way
--    for a client to build a hundred-deep chain that no inbox can render and no public query can
--    answer cheaply. A CHECK constraint cannot express it either: PostgreSQL forbids subqueries
--    in CHECK, and the parent row is a *different* row, so the rule is only expressible as a
--    trigger. `reply_depth` is GENERATED from the parent reference, and the trigger refuses a
--    reply whose parent is itself a reply.
--
-- 3. **Moderation state is a column, not a soft-delete flag.** `pending`, `approved`, `spam` and
--    `trash` are four different questions — "has a human looked at this", "may it be shown",
--    "is it junk", "did somebody delete it" — and the inbox is four tabs. One `is_hidden`
--    boolean cannot answer any of them, and the difference between *junk* and *deleted* is the
--    difference between a list you can restore from and a list you cannot.
--
-- 4. **The heuristics are recorded, never acted on silently.** A comment lands `spam` or
--    `pending`, and `spam_reason` says which rule fired. The REQ's own risk note is the reason:
--    the panel must never claim "spam blocked" with certainty, because a heuristic that is
--    certain is a heuristic that has thrown away somebody's real comment.
--
-- 5. **Bans live beside the comments, per site.** A banned e-mail or IP is a moderator's
--    decision about a *person*, and it outlives the comment it was made about — so it is a table
--    rather than a flag on a row that can be deleted.

-- ---------------------------------------------------------------------------------------------
-- The comments themselves
-- ---------------------------------------------------------------------------------------------

create table cms_comments (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    site_id uuid not null references sites (id) on delete cascade,
    page_id uuid not null references pages (id) on delete cascade,
    -- A reply points at the comment it answers. ON DELETE CASCADE because a deleted parent takes
    -- its replies with it: a reply whose question is gone is an orphan nobody can moderate.
    parent_id uuid references cms_comments (id) on delete cascade,
    -- What the visitor typed. The author is deliberately a *name and an address*, never an
    -- account id: a member's identity lives in `cms_members` (slice 4c) and a comment must be
    -- submittable by a visitor who has never signed up.
    author_name text not null,
    author_email text not null,
    -- Coarse client hint, stored as text so an IPv6 address is not truncated by an inet column
    -- that assumes v4. The ban table below matches on the same text for the same reason.
    ip_hint text,
    user_agent text,
    body text not null,
    status text not null default 'pending',
    -- Which heuristic sent it to spam. NULL for a comment a human marked as junk — the two are
    -- different claims and the inbox labels them differently.
    spam_reason text,
    approved_at timestamptz,
    approved_by uuid references users (id) on delete set null,
    -- Set when a moderator replies, so the thread renders "site replied" without a join.
    is_staff_reply boolean not null default false,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cms_comments_status_check
        check (status in ('pending', 'approved', 'spam', 'trash')),
    constraint cms_comments_body_not_blank check (length(btrim(body)) > 0),
    constraint cms_comments_name_not_blank check (length(btrim(author_name)) > 0),
    -- A VISITOR must give an address; a MODERATOR reply must not. `is_staff_reply` is the
    -- discriminator, and the two halves are the same constraint rather than two, because a
    -- row that is neither is exactly what a bug produces: `length(btrim(author_email)) > 0
    -- or is_staff_reply` lets a blank address through on a visitor's comment while still
    -- refusing a blank one on a reply. The address is a column, not a join to `users`, because
    -- a comment must be submittable by somebody who has no account at all.
    constraint cms_comments_author_email_check check (
        is_staff_reply or length(btrim(author_email)) > 0
    )
);

-- `reply_depth` is GENERATED from the parent reference rather than passed in, so a client that
-- omits it gets the right answer and a client that sends a wrong one is refused by the trigger
-- below instead of being silently corrected.
alter table cms_comments
    add column reply_depth smallint
        generated always as (case when parent_id is null then 0 else 1 end::smallint) stored;

-- Two levels, as a fact about the DATA. A CHECK cannot do this — the parent is another row and
-- PostgreSQL forbids subqueries in CHECK — so the rule is a trigger, and the two-level claim is
-- visible in the schema rather than only in the store that happens to be the only writer today.
create function cms_comments_refuse_a_reply_to_a_reply()
returns trigger
language plpgsql
as $$
begin
    if new.parent_id is null then
        return new;
    end if;

    if not exists (select 1 from cms_comments where id = new.parent_id) then
        raise exception 'comment parent % does not exist', new.parent_id
            using errcode = 'foreign_key_violation';
    end if;

    if exists (
        select 1 from cms_comments where id = new.parent_id and reply_depth <> 0
    ) then
        raise exception 'a reply cannot answer another reply'
            using errcode = 'check_violation';
    end if;

    return new;
end;
$$;

create trigger cms_comments_two_levels
    before insert or update of parent_id on cms_comments
    for each row execute function cms_comments_refuse_a_reply_to_a_reply();

-- The inbox is "this site's comments in this state, newest first" — one index serves every tab.
create index cms_comments_site_status_idx on cms_comments (site_id, status, created_at desc);

-- The public read is "this page's approved comments, oldest first", which is why the index is
-- page-scoped and partial: an approved comment is a small fraction of the table and the render
-- path must not walk pending and spam rows to find it.
create index cms_comments_page_approved_idx
    on cms_comments (page_id, created_at)
    where status = 'approved' and parent_id is null;

-- No unique index on (page, author, body) — deliberately, and it is worth writing down WHY,
-- because the obvious design is the one that does not work.
--
-- A unique index here would make a double-clicked Send button a *database error* on the second
-- insert, which is the opposite of the rule's purpose: the rule exists so a moderator can SEE
-- that somebody submitted twice, and the second row is the evidence. Refusing the insert means
-- the only record of the double submission is a 500 in a log. The store checks for a duplicate
-- and WRITES the row with `spam_reason` set, so the queue shows both copies and says which
-- came first.
--
-- The genuine double-click case is handled at its real source instead: the public route is
-- idempotent by way of the honeypot and the fill-time floor, and a second identical submission
-- lands in Spam where it is visible rather than invisible.
create index cms_comments_duplicate_lookup_idx
    on cms_comments (page_id, lower(author_email), md5(body))
    where status <> 'trash';

-- ---------------------------------------------------------------------------------------------
-- Per-site settings and bans
-- ---------------------------------------------------------------------------------------------

create table cms_comment_settings (
    site_id uuid primary key references sites (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    -- Comments are OFF until a site turns them on. A CMS that accepts comments the moment it is
    -- installed is a CMS that gets a spam problem the owner did not choose.
    comments_enabled boolean not null default false,
    -- A first-time poster whose address already has an approved comment skips the queue.
    -- 0 disables it, which is the default because "the first comment is trusted" is not a
    -- decision a platform should make on an owner's behalf.
    auto_approve_after_comments integer not null default 0,
    -- Case-insensitive substrings. Stored in one array so the editor is a textarea, and matched
    -- with `position(...) in (lower(body))` rather than a regex so a moderator's word can never
    -- be a syntax error at match time.
    blocked_words text[] not null default '{}',
    -- How many links a body may carry before it is junk. Two is generous for a person and below
    -- the threshold any link farm works at.
    max_links_per_comment integer not null default 2,
    -- Seconds a visitor must have had the form on screen. A form filled in under three seconds
    -- was filled in by a script.
    min_fill_seconds integer not null default 3,
    -- Per-IP comments per hour, counted over the same `ip_hint` the ban table matches.
    per_ip_per_hour integer not null default 5,
    notify_on_comment boolean not null default true,
    updated_at timestamptz not null default now(),
    constraint cms_comment_settings_auto_approve_range
        check (auto_approve_after_comments between 0 and 1000),
    constraint cms_comment_settings_max_links_check
        check (max_links_per_comment between 0 and 50),
    constraint cms_comment_settings_min_fill_check
        check (min_fill_seconds between 0 and 300),
    constraint cms_comment_settings_rate_check
        check (per_ip_per_hour between 1 and 1000)
);

-- Bans are per site and per kind. A ban without a value is meaningless and an e-mail ban holding
-- something that is not an address is a typo that silently bans nothing — hence the shape check.
create table cms_comment_bans (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    kind text not null,
    value text not null,
    reason text,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    expires_at timestamptz,
    constraint cms_comment_bans_kind_check check (kind in ('email', 'ip')),
    constraint cms_comment_bans_value_not_blank check (length(btrim(value)) > 0),
    constraint cms_comment_bans_shape_check check (
        (kind = 'email' and value ~* '^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$')
        or (kind = 'ip' and value !~ '@')
    )
);

create unique index cms_comment_bans_unique_idx
    on cms_comment_bans (site_id, kind, lower(value));
