-- Omnion · 0051 · content: site menus and the scheduled publishing queue (REQ-064, slice 1)
--
-- Navigation and scheduling are the two halves of "when does this content appear, and how does a
-- visitor get to it". They ship together because a scheduled post almost always has a navigation
-- entry, and a menu item almost always points at a page whose publication is scheduled.
--
-- Three choices are worth stating, because each is a place the obvious table is wrong:
--
-- 1. **A menu is site-scoped, not organization-scoped.** A menu says "the header of *this*
--    site" — the same navigation in two sites of one organization is two menus, because their
--    pages, their slugs and their visitors differ. Organization-scoped menus would let a menu
--    name a page that belongs to a sibling site, and the renderer would have to answer "which of
--    my sites?" at render time.
--
-- 2. **`locations` is a `text[]` and uniqueness across menus is enforced in the store, not by
--    an index.** The rule is "one location holds one menu": the natural database expression is a
--    partial unique index over an unnested array, which PostgreSQL cannot express, and the second
--    candidate — a `menu_locations` table — buys a second write path and a second place for the
--    conflict to hide. `menu::save` claims every location in one transaction and refuses a
--    location another menu already holds, so the invariant is checked where it is written and
--    the check is a test rather than a hope.
--
-- 3. **The queue is claim-safe by `for update skip locked`, and the claim is what makes the
--    result meaningful.** A runner that reads due rows and then writes them can double-fire a
--    publish across two workers, and a runner that claims them in a transaction can neither.
--    `claimed_at` is therefore not decoration: it is the column the sweep asks about, and the
--    partial index on pending rows is the only reason the sweep is a range scan instead of a
--    table scan once a site has scheduled for a year.

-- ---------------------------------------------------------------------------------------------
-- Menus
-- ---------------------------------------------------------------------------------------------

create table cms_menus (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    site_id         uuid        not null references sites (id) on delete cascade,
    key             text        not null,
    name            text        not null,
    -- Which theme slots render this menu. One location holds one menu (see note 2 above); the
    -- vocabulary is closed in `crates/content/src/menus.rs` and the database repeats it so a
    -- hand-written row cannot name a location no renderer reads.
    locations       text[]      not null default '{}',
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint cms_menus_key_format check (key ~ '^[a-z][a-z0-9_-]{0,61}[a-z0-9]$'),
    constraint cms_menus_name_not_blank check (length(btrim(name)) > 0),
    -- The closed location vocabulary, as array containment. Without it a hand-written row could
    -- claim a slot no theme renders, and the menu would then be invisible from the panel and
    -- from the site at the same time. `LOCATIONS` in `crates/content/src/menus.rs` is the same
    -- list, and a unit test reads this file to prove they agree.
    constraint cms_menus_locations_check
        check (locations <@ array['header', 'footer', 'sidebar', 'mobile']),
    constraint cms_menus_site_key_key unique (site_id, key)
);

create index cms_menus_site_id_idx on cms_menus (site_id);

create table cms_menu_items (
    id               uuid        primary key default gen_random_uuid(),
    menu_id          uuid        not null references cms_menus (id) on delete cascade,
    -- Self-reference for nesting. `on delete cascade` so removing a parent item removes the
    -- branch under it rather than orphaning rows the renderer would still try to draw.
    parent_id        uuid        references cms_menu_items (id) on delete cascade,
    position         integer     not null default 0,
    label            text        not null,
    item_type        text        not null default 'url',
    page_id          uuid        references pages (id) on delete set null,
    url              text        not null default '',
    target           text        not null default '_self',
    rel              text        not null default '',
    css_class        text        not null default '',
    enabled          boolean     not null default true,
    -- Insertion order. A menu's items are written parent-first inside one transaction, so this
    -- is the order the editor built the tree in — which is exactly the tie-break `list_items`
    -- needs, because two items with the same `position` (two rows added in one drag, or the
    -- "start at" case) must come back in a stable, author-meaningful order rather than in
    -- whatever order a UUID index returns them.
    created_at       timestamptz not null default now(),
    -- Who may see the item. `everyone` is the default; `members` and `logged_out` are the two
    -- audience-specific values, and `roles` defers to `visibility_roles`.
    visibility       text        not null default 'everyone',
    visibility_roles text[]      not null default '{}',
    constraint cms_menu_items_label_not_blank check (length(btrim(label)) > 0),
    constraint cms_menu_items_position_positive check (position >= 0),
    constraint cms_menu_items_type_check check (item_type in ('page', 'url', 'anchor', 'index')),
    constraint cms_menu_items_target_check check (target in ('_self', '_blank')),
    constraint cms_menu_items_visibility_check
        check (visibility in ('everyone', 'members', 'logged_out', 'roles'))
);

-- The tree read: every item of one menu, ordered by position and then by insertion. `created_at`
-- is in the index because it is in the order by — without it PostgreSQL sorts the ties in the
-- heap and two rows with the same position come back in a different order after a rewrite.
create index cms_menu_items_menu_parent_idx
    on cms_menu_items (menu_id, parent_id, position, created_at, id);

-- A page can be in a menu exactly once per menu. Without this, `Add pages…` run twice adds the
-- page twice and the navigation shows the same link twice — which reads as the editor's bug and
-- is the store's.
create unique index cms_menu_items_menu_page_key
    on cms_menu_items (menu_id, page_id)
    where page_id is not null;

-- ---------------------------------------------------------------------------------------------
-- Scheduled publishing
-- ---------------------------------------------------------------------------------------------

create table cms_publishing_queue (
    id           uuid        primary key default gen_random_uuid(),
    organization_id uuid     references organizations (id) on delete cascade,
    page_id      uuid        not null references pages (id) on delete cascade,
    action       text        not null,
    -- The instant, in UTC, the entry becomes due. The *author's* timezone is kept beside it so
    -- the queue screen can show "14:30 in Europe/Istanbul" next to the stored UTC value, which
    -- is the difference between a schedule an author trusts and one they check twice.
    scheduled_at timestamptz not null,
    timezone     text        not null default 'UTC',
    status       text        not null default 'pending',
    result       text        not null default '',
    error        text        not null default '',
    created_by   uuid        references users (id) on delete set null,
    claimed_at   timestamptz,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    constraint cms_publishing_queue_action_check check (action in ('publish', 'unpublish')),
    constraint cms_publishing_queue_status_check
        check (status in ('pending', 'done', 'failed', 'cancelled'))
);

-- A page holds one pending entry per action: scheduling a publish twice is a reschedule, and two
-- rows would publish the same revision twice and show two "scheduled" lines in the queue for one
-- page. This is a partial unique *index*, not a constraint — PostgreSQL does not accept a
-- `where` clause on a `unique` constraint, and writing it as one fails the whole migration
-- file rather than the one statement.
create unique index cms_publishing_queue_pending_key
    on cms_publishing_queue (page_id, action)
    where status = 'pending';

-- The sweep's range scan. Partial on `status = 'pending'`, because the done, failed and
-- cancelled rows are the history the screen shows and must never be scanned to find work.
create index cms_publishing_queue_due_idx
    on cms_publishing_queue (scheduled_at, id)
    where status = 'pending';

-- One pending entry per page overall, so the queue screen's "next publish" and "next unpublish"
-- columns cannot both be filled by the same page twice over.
create index cms_publishing_queue_page_idx on cms_publishing_queue (page_id, status);
