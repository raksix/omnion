-- 0050_notifications.sql — the in-app record, its preference matrix and its delivery queue.
--
-- REQ-021, slice 1. The schema is the record, the reader's own matrix, the digest settings
-- and the channel configuration. The delivery runner that *drains* `notification_deliveries`
-- is slice 2; the table exists here because a notification that has no delivery row is a
-- notification whose "sent over e-mail" question has no answer.
--
-- Three choices are worth stating, because each is a place the obvious table is wrong:
--
-- 1. **The category check constraint and the crate's `CATEGORIES` list are the same list.**
--    A category the panel can filter by but the database refuses is a filter that silently
--    returns nothing; one the database accepts but the panel cannot name is a row in an inbox
--    with no way to find it. Duplicating the list in SQL is unavoidable — it cannot import
--    Rust — so it is stated here in a comment, and `crates/notifications/src/vocabulary.rs`
--    carries a test that names this file as the list it must agree with.
--
-- 2. **`emitted_by` is a column, not a `payload->>'…'` probe.** The emit budget reads it
--    every time a module wants to talk to a person. A budget that reads its evidence out of a
--    caller-supplied JSON blob is one schema change away from counting zero forever while
--    still looking like it is enforcing a cap.
--
-- 3. **A notification belongs to a *person*, and `user_id` is `not null`.** There is no
--    "broadcast to everyone" row: a notification with no reader is not a notification, and a
--    nullable `user_id` would be a place for a module to put one by accident.

create table notifications (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        references organizations (id) on delete cascade,
    user_id         uuid        not null references users (id) on delete cascade,
    category        text        not null,
    priority        text        not null default 'normal',
    title           text        not null,
    body            text        not null default '',
    url             text,
    source_type     text,
    source_id       text,
    payload         jsonb       not null default '{}'::jsonb,
    -- Who caused the emit. Null for a fact the platform itself raised; set for a module
    -- telling somebody something, and the column the per-actor emit budget is counted over.
    emitted_by      uuid        references users (id) on delete set null,
    -- Collapsing repeats. Two emits with the same key are the *same* fact, so the partial
    -- unique index below keeps one row and `store::record` reports the second as not created.
    dedupe_key      text,
    read_at         timestamptz,
    archived_at     timestamptz,
    created_at      timestamptz not null default now(),
    constraint notifications_category_check
        check (category in ('approval', 'security', 'update', 'ticket', 'system', 'mention')),
    constraint notifications_priority_check
        check (priority in ('low', 'normal', 'high', 'critical'))
);

-- The list's own ordering, newest first. It leads with `user_id` because every read in the
-- crate is owner-scoped, and a list nobody else can read should never touch another user's
-- part of the index.
create index notifications_user_created_idx on notifications (user_id, created_at desc, id desc);

-- The unread read. Partial, because it is the one query that runs on every page load of every
-- screen with a bell in it, and the read rows are the majority of an inbox after a week.
create index notifications_unread_idx
    on notifications (user_id, created_at desc)
    where read_at is null and archived_at is null;

-- The emit budget: one count per emit, per actor, over the last minute.
create index notifications_emitted_by_idx
    on notifications (emitted_by, created_at desc)
    where emitted_by is not null;

-- Dedupe, scoped to the reader. Partial and unique for the same reason: a key that is null is
-- "this fact is not collapseable", and there are many of those.
create unique index notifications_dedupe_key
    on notifications (user_id, dedupe_key)
    where dedupe_key is not null;

-- The per-user preference matrix (slice 2 reads and writes it; it ships in this migration so
-- the settings screen has a table to save into the moment slice 2 lands).
create table notification_preferences (
    user_id    uuid        not null references users (id) on delete cascade,
    category   text        not null,
    channel    text        not null,
    enabled    boolean     not null default true,
    updated_at timestamptz not null default now(),
    primary key (user_id, category, channel),
    constraint notification_preferences_category_check
        check (category in ('approval', 'security', 'update', 'ticket', 'system', 'mention')),
    constraint notification_preferences_channel_check
        check (channel in ('in_app', 'email', 'web_push', 'webhook', 'chat'))
);

-- Quiet hours and the digest cadence. One row per person, created with the account.
create table notification_settings (
    user_id           uuid        primary key references users (id) on delete cascade,
    quiet_hours_start time,
    quiet_hours_end   time,
    timezone          text        not null default 'UTC',
    digest_cadence    text        not null default 'off',
    digest_weekday    smallint,
    digest_hour       smallint    not null default 8,
    updated_at        timestamptz not null default now(),
    constraint notification_settings_digest_check
        check (digest_cadence in ('off', 'daily', 'weekly')),
    constraint notification_settings_weekday_check
        check (digest_weekday is null or digest_weekday between 0 and 6),
    constraint notification_settings_hour_check
        check (digest_hour between 0 and 23)
);

-- One row per channel a notification was tried on. This is the table that makes "it is in the
-- panel but the e-mail never came" a *state* rather than a contradiction: the panel reads the
-- notification, the drawer reads these rows, and neither has to guess about the other.
create table notification_deliveries (
    id              uuid        primary key default gen_random_uuid(),
    notification_id uuid        not null references notifications (id) on delete cascade,
    channel         text        not null,
    status          text        not null default 'pending',
    attempts        int         not null default 0,
    max_attempts    int         not null default 3,
    next_attempt_at timestamptz not null default now(),
    response_status int,
    error           text,
    sent_at         timestamptz,
    created_at      timestamptz not null default now(),
    constraint notification_deliveries_channel_check
        check (channel in ('in_app', 'email', 'web_push', 'webhook', 'chat')),
    constraint notification_deliveries_status_check
        check (status in ('pending', 'sent', 'failed', 'skipped')),
    -- A delivery is only ever tried once per notification per channel. Without this a retried
    -- emit rows a second 'pending' row and the drawer lists the same channel twice, which
    -- reads as "we tried twice and never said so".
    constraint notification_deliveries_unique unique (notification_id, channel)
);

-- The runner's claim query (slice 2): due rows, oldest first.
create index notification_deliveries_due_idx
    on notification_deliveries (next_attempt_at, created_at)
    where status = 'pending';

create index notification_deliveries_notification_idx
    on notification_deliveries (notification_id);

-- Browser push devices (slice 3 owns the lifecycle; the row lives here so the key the server
-- signs with and the endpoint it sends to are stored together).
create table push_subscriptions (
    id           uuid        primary key default gen_random_uuid(),
    user_id      uuid        not null references users (id) on delete cascade,
    endpoint     text        not null unique,
    p256dh       text        not null,
    auth         text        not null,
    user_agent   text,
    created_at   timestamptz not null default now(),
    last_seen_at timestamptz not null default now()
);

create index push_subscriptions_user_idx on push_subscriptions (user_id);

-- Which channels this organization has switched on, and where its credentials live.
--
-- **The config column holds a reference, never a secret.** A transport credential is written
-- through the secret store (REQ-037) and this table keeps the key that points at it, so a
-- database dump of an installation does not carry anybody's mail password. That is why the
-- check below has no column for one.
create table notification_channels (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    channel         text        not null,
    config          jsonb       not null default '{}'::jsonb,
    enabled         boolean     not null default true,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint notification_channels_channel_check
        check (channel in ('in_app', 'email', 'web_push', 'webhook', 'chat')),
    constraint notification_channels_unique unique (organization_id, channel)
);
