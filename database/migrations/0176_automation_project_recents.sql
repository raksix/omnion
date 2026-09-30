-- Automation project switcher recents (REQ-133, acceptance 3) -- the "persists per user" half.
--
-- Acceptance 3 asks for a switcher whose "selection persists per user and is written into URLs".
-- The URL half is client state and this file is not about it. The persistence half had nowhere to
-- live: the query parameter `ProjectListQuery.mine` existed and the REQ's API table documented
-- `GET /api/v1/projects?mine=1` as "the switcher's list" -- and no code on this branch read the
-- parameter, so the switcher had no list to offer and no way to remember a choice.
--
-- The decision that needs a table at all: persistence is **per user, server-side**, not a cookie
-- and not localStorage. The two alternatives were both rejected on the same ground -- a browser
-- holding it means the choice is lost on a new device and invisible to a colleague, and this is
-- the setting a person is most likely to want to arrive already made. The cost of the server-side
-- answer is a write on every switch, which is why `set_selection` and `record_use` are the same
-- statement (see `select_project`).
--
-- `recents` is an **ordered** set, not a set: "recents first" in the switcher is a ranking, and a
-- table without a position cannot answer it. The position is dense and zero-based, shifted on
-- every write, and the *demotion* half is what a plain upsert gets wrong: a `primary key
-- (user_id, project_id)` with an `on conflict do nothing` would pin the first project ever
-- selected at rank 0 for ever, so the entry a person is working in today would sit below one they
-- touched once a fortnight ago. The shift is done inside the transaction that demotes the old
-- holder of rank 0, so a user can never hold two entries at the same rank -- which a unique index
-- on (user_id, rank) would have caught, and which is now also the constraint.
--
-- Selection is a single nullable column rather than a rank of zero: "nothing selected yet" and
-- "the default project is selected" are different facts, and the first is a state the switcher
-- has to be able to say out loud.

create table if not exists automation_project_recent (
    user_id uuid not null references users (id) on delete cascade,
    project_id uuid not null references automation_projects (id) on delete cascade,
    -- 0 is the most recently used. Dense, so the first eight are a window and not a sparse list.
    rank smallint not null check (rank >= 0 and rank <= 7),
    used_at timestamptz not null default now(),
    primary key (user_id, project_id)
);

-- The uniqueness that makes "shift then insert" safe: a user can never hold two entries at the
-- same rank, so the promotion is a claim rather than a hope. Without it two concurrent switches
-- would both believe they wrote rank 0 and the recents list would grow a duplicate that no
-- reader could explain.
create unique index if not exists automation_project_recent_rank_key
    on automation_project_recent (user_id, rank);

-- The switcher's own query: this user's eight, most recent first. The index answers it directly.
create index if not exists automation_project_recent_used_idx
    on automation_project_recent (user_id, used_at desc);

comment on table automation_project_recent is
    'The per-user switcher history: at most eight projects, ranked 0 (newest) to 7. Replaces a '
    'preference row per user because the ordering IS the feature -- a switcher that shows a set '
    'with no order is a project list wearing a switcher''s clothes.';
comment on index automation_project_recent_rank_key is
    'A user holds at most one entry per rank, so the shift-and-claim is atomic in the database '
    'rather than in the caller''s read-then-write.';

-- The selection itself: one row per user, at most one. `nullable project_id` is not used -- a
-- person who chooses "All projects" gets NO ROW rather than a row pointing at nothing, because
-- "no row" is the only state that can be reached by exactly one path (never chosen, or cleared) and
-- a nullable column would have three: never chosen, cleared, and pointing at a project that was
-- since deleted. `on delete cascade` therefore removes a selection whose project is gone, which
-- leaves the user in the correct default state instead of in a switcher whose current entry 404s.
create table if not exists automation_project_selection (
    user_id uuid primary key references users (id) on delete cascade,
    project_id uuid not null references automation_projects (id) on delete cascade,
    updated_at timestamptz not null default now()
);

comment on table automation_project_selection is
    'The project a person has switched into, server-side rather than in a cookie. Absent row means '
    'the default view (everything), which is a state the switcher says out loud rather than '
    'guessing from a null.';

