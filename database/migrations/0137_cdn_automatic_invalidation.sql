-- Omnion · 0120 · the automatic invalidation cursor and the provenance of a queued purge
--   (REQ-011, slice 3)
--
-- Slices 1 and 2 shipped the rule engine, the headers and the queue. What is missing is the
-- reason a purge exists: `0054` gives an operator a console and a worker, and the request's
-- own diagram — "page published → purge CDN cache → new version live" — had nothing behind
-- it. This migration adds the two rows that make automatic invalidation a *mechanism* rather
-- than a sentence.
--
-- **The cursor is a table, on the same argument as `search_cursor` and for the same reason.**
-- An invalidation subscriber that remembers its place in a process variable loses that place
-- on a deploy, and the events it skipped are gone: the site serves last week's page and no
-- queue row anywhere says an invalidation was owed. A row the drain locks with
-- `for update skip locked` makes the walk exactly-once across instances, which matters
-- because two API processes (a rolling deploy, a horizontal scale-out) will otherwise turn
-- one publish into two purges and bill the provider twice for it.
--
-- **The cursor is seeded forward, not to zero.** A fresh installation watches from the head
-- of the bus; an existing one starts above the history it already has. Retroactively purging
-- every publication ever recorded would be a stampede at the provider for no benefit — the
-- content has been republished many times since, and the last purge covers the same
-- addresses. This is the same rule the search indexer follows, and for the same reason.
--
-- **`cdn_purge_sources` is a separate table rather than a column on `cdn_purges`.** An
-- automatic purge is caused by exactly one event; a manual purge by a person and no event.
-- A nullable `source_event_id` on the purge would have been one column instead of a table,
-- and it would have made the *reason* for a purge depend on which half of the product wrote
-- it — the manual half would have had to invent a sentinel, and the panel's "what caused
-- this row" line would have been a `case` over a value that is null in one of its two
-- branches. A row says what it is: a purge with a source was automatic, a purge without one
-- was asked for.
--
-- **The event id is not a foreign key, deliberately.** `events` rows are retained on their
-- own schedule and an event is, by design, the most disposable thing the platform records.
-- A purge history row must survive the event that caused it: the fact that a site was
-- invalidated because of a publication outlives the publication's bus row, and a foreign key
-- here would cascade a retention sweep into deleting the record of an act taken against a
-- production cache. The `trigger` column carries the *name* for the same reason — after the
-- event is gone the drawer can still say "a page was published", which is the part anybody
-- reads.
--
-- One table is a single row and is never `delete`d; the index is the primary key, which is
-- what makes `for update skip locked` a row lock on exactly one row.

create table if not exists cdn_invalidation_cursor (
    id int primary key,
    -- The highest event id already applied. Zero means "never advanced", which is what
    -- `seed_cursor` looks for; a cursor at zero on a bus with history would replay it.
    last_event_id bigint not null default 0,
    updated_at timestamptz not null default now(),
    constraint cdn_invalidation_cursor_singleton check (id = 1)
);

insert into cdn_invalidation_cursor (id, last_event_id)
values (1, 0)
on conflict (id) do nothing;

-- ---------------------------------------------------------------------------------------------
-- Provenance
-- ---------------------------------------------------------------------------------------------

create table if not exists cdn_purge_sources (
    purge_id uuid primary key references cdn_purges (id) on delete cascade,
    -- The event that caused the purge. Not a foreign key — see the header.
    event_id bigint not null,
    -- The event *name*, kept so the drawer can still answer "what caused this" after the
    -- event row is gone. Written by the same code that wrote `event_id`, never derived
    -- from it, so a name cannot drift from the event it describes.
    trigger text not null,
    created_at timestamptz not null default now(),
    constraint cdn_purge_sources_trigger_not_empty check (length(trim(trigger)) > 0)
);

-- The drain reads every event and asks which are triggers, and the one question it asks
-- first is "is this name one of mine". A name index makes that a lookup instead of a scan
-- of every event the platform has ever recorded.
create index if not exists cdn_invalidation_cursor_position
    on cdn_invalidation_cursor (last_event_id);
