-- 0204_ai_change_set_content_hash.sql — REQ-101 slice 3e: the set names what it now says.
--
-- # What this column is for
--
-- "Editing a change set records the editor and time, re-renders the diff and updates the
-- preview hash." The editor and the time landed in `0201`; the hash did not, and without it
-- the criterion is only half a sentence: a reviewer has no value to compare against, so
-- "the set changed since you last read it" is a thing the client decides.
--
-- The approval side already has this column (`ai_approvals.preview_hash`, since `0189`) and
-- the review screen already reads it. This brings the **set** to the same footing: the
-- editor shows `content_hash`, the reviewer compares it, and `PATCH /ai/change-sets/{id}`
-- refuses an edit whose base hash does not match what the server holds.
--
-- # Why it is a stored column and not a generated one
--
-- A `generated always as (encode(digest(operations::text || base_revisions::text,
-- 'sha256'), 'hex')) stored` column is the obvious way to make the row impossible to
-- desynchronise from its own operations, and it is **wrong** — silently, and in a way that
-- only shows up as a hash that never matches.
--
-- `jsonb::text` is not the same string `serde_json::to_string` produces for the same value.
-- PostgreSQL orders object keys by **length first, then bytewise**; serde_json's `Map` is a
-- `BTreeMap` and orders them by bytewise alone. For a one-key object the two agree, which
-- is exactly what makes it a trap: a test over one operation passes, and the first set with
-- a two-character key next to a one-character key disagrees.
--
--     '{"zz":1,"a":2,"mmm":3,"bb":4}'      -- serde_json, bytewise
--     '{"a": 2, "bb": 4, "zz": 1, "mmm": 3}' -- jsonb::text, length-first
--
-- So the hash has to be computed by the same process that serialises the operations, and
-- stored. The drift this leaves possible — an editor that writes operations without writing
-- the hash — is closed by the only other writer being a store function (see the module
-- header of `crates/ai-hub/src/change_sets.rs`), and by a walk that asserts the column
-- equals `ChangeSet::content_hash()` of the row it was read from.
--
-- # Why not a checksum check
--
-- A `check` constraint cannot compare a stored column with anything computed over another
-- column in Postgres, so the alternative would be to recompute the hash on every read — a
-- per-row sha256 in Rust on a path that lists 50 sets, to protect against a bug the store
-- function already makes impossible. The column is not null and defaulted so that a row
-- written before this migration reads as *unhashed* rather than as *unchanged*: `''` is not
-- what `content_hash()` returns for any set, so "this set predates the column" and "this
-- set did not change" cannot be confused.

begin;

alter table ai_change_sets
    add column if not exists content_hash text not null default '';

comment on column ai_change_sets.content_hash is
    'sha256 over the canonical JSON of the operations and their base revisions; '
    'recomputed by the store on every write, and what a reviewer compares before editing.';

-- The index the editor's "has this moved under me" lookup wants is the primary key, so there
-- is nothing to add here. An index on a column nothing queries by status or time would be
-- a write cost on a table that is written once and read by primary key.

commit;
