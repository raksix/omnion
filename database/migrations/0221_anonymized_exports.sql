-- Anonymised exports and the column classification map (docs/requests/REQ-129, slice 4).
--
-- **Why the map is a table and not a rule in the code.** "A forgotten column is a data incident"
-- is the whole risk this feature carries, and a rule in the code has the property that it is only
-- as good as the list somebody typed when they wrote it. So the list is DATA — one row per
-- (table, column) with a class and a default action — and the export builder FAILS CLOSED on a
-- column that has no row here. "We forgot to strip that column" moves from *unlikely* to
-- *unrepresentable*, which is the only version of this feature worth shipping.
--
-- **The fail-closed direction is the whole design.** An unclassified column blocks the export.
-- The opposite default (export it and warn) is what makes anonymisation a promise nobody can keep,
-- because the failure is invisible at the moment it happens and permanent afterwards. Note the
-- asymmetry: `keep` is a legal `default_action` here, so an operator CAN export a column raw —
-- but only by writing a row that says so, which is a reviewable act in a table with an author and
-- a timestamp rather than an omission in a code path.
--
-- **`class` is about the DATA; `default_action` is about THIS export.** They are separate columns
-- because one column can be `personal` (an e-mail address) and still be perfectly safe to export
-- once hashed — hashing a personal identifier is the standard technique, and collapsing the two
-- into one enum would make "hash the e-mail column" unrepresentable.
--
-- **`class` is TEXT rather than an enum** for the same reason `migration_backfills.state` is: an
-- enum needs an ALTER to extend, which is precisely the destructive shape this request exists to
-- keep out of this very table. The CHECK is the closed vocabulary and it sits beside the column.
--
-- **Hashed values are salted PER EXPORT, and the salt is never stored next to the file.** If the
-- salt travelled with the dump, a leaked dump plus its own salt would be reversible by
-- dictionary attack on a column of e-mail addresses — the hash would be a speed bump, not a
-- protection. So `salt_fingerprint` is a SHA-256 of the salt (enough to tell two exports apart in
-- the audit trail, useless for attacking either) and the salt itself lives only in the job's
-- memory for the life of the export. Referential integrity across tables is preserved anyway,
-- because the SAME salt hashes the SAME value to the SAME output within one export — which is
-- exactly the property a join needs and exactly the property a shared salt would destroy.
--
-- **One export, one row, and the file is described rather than attached.** `file_key` names the
-- object; `checksum` lets somebody confirm later that the bytes they hold are the bytes this row
-- describes. Neither implies the bytes live in the database: an export of a real installation is
-- a file, and a `bytea` column would make the "expiring, single-use, revocable" promises
-- unachievable, because a DELETE would have to un-write history on disk.
--
-- **Single-use is a counter with a CHECK, not a flag.** `download_count` plus
-- `expires_at > now()` is what makes the second download a `410` rather than a second copy in a
-- support inbox. `revoked_at` is a third, independent terminal state — a revoke must kill the link
-- IMMEDIATELY even when the file is fresh and the count is zero.

create table column_classifications (
    table_name      text not null check (length(btrim(table_name)) > 0),
    column_name     text not null check (length(btrim(column_name)) > 0),
    -- `personal` (an individual), `secret` (a credential — never exported, ever), `identifier`
    -- (a surrogate key; usually safe but can re-identify when combined), `safe` (operational).
    class           text not null
        check (class in ('personal', 'secret', 'identifier', 'safe')),
    -- What an export does with it WHEN THE CALLER DOES NOT SAY OTHERWISE. `keep` is legal and is
    -- the honest answer for `safe`; writing it for a `secret` is refused by a trigger below,
    -- because the one combination that must be unrepresentable is "export a credential raw".
    default_action  text not null
        check (default_action in ('remove', 'hash', 'synthetic', 'keep')),
    -- Why this column is classified this way. A row with an empty reason in a table somebody
    -- reviews quarterly is a row nobody reviewed.
    notes           text not null default '',
    reviewed_by     text not null,
    reviewed_at     timestamptz not null default now(),
    primary key (table_name, column_name),
    -- The rule the builder enforces, expressed once so a reader does not have to infer it from
    -- three separate places: a secret is never kept.
    constraint column_classifications_secret_never_kept check (
        class <> 'secret' or default_action <> 'keep'
    )
);

comment on table column_classifications is
    'Per-column data classification. The export builder FAILS CLOSED on a column with no row here, which is what makes "we forgot to strip that column" unrepresentable rather than unlikely.';

comment on column column_classifications.default_action is
    'What an export does when the caller does not override. The class says what the DATA is; this says what THIS export does with it — a personal e-mail is normally hashed, and collapsing the two would make that choice unrepresentable.';

create table anonymized_exports (
    id              uuid primary key default gen_random_uuid(),
    -- Why the export exists. Required and free text because it is the only sentence a reviewer
    -- reads later: "support ticket 4471, reproducing a customer's checkout failure" is the answer
    -- to "may we send this file to the vendor", and a file with no reason has no answer.
    reason          text not null check (length(btrim(reason)) > 0),
    -- What was asked for, kept verbatim so the audit trail can show what was REQUESTED even if the
    -- classification map later changes underneath it.
    tables          text[] not null default '{}',
    column_actions  jsonb not null default '{}',
    row_limit       int check (row_limit is null or row_limit > 0),
    window_start    timestamptz,
    window_end      timestamptz,
    -- The states a support file passes through. `revoked` and `expired` are BOTH terminal and
    -- both recorded: an operator who revokes a file nobody downloaded still needs the trail to
    -- say so, and "expired" alone would imply the clock did it.
    status          text not null default 'queued'
        check (status in ('queued', 'running', 'ready', 'failed', 'revoked', 'expired')),
    file_key        text,
    file_size       bigint check (file_size is null or file_size >= 0),
    -- SHA-256 of the produced file, so "the bytes I hold are the bytes this row describes" is a
    -- question with an answer.
    checksum        text,
    -- SHA-256 OF THE SALT, never the salt. See the header: a per-export salt is what makes
    -- hashing worth doing, and storing it beside the file would undo that.
    salt_fingerprint text,
    -- Stamped INTO the file as its first line. A dump that leaves the platform should announce
    -- itself when it lands somewhere else, and a watermark that is only in the database is a
    -- watermark nobody outside the database can see.
    watermark       text not null,
    expires_at      timestamptz not null,
    -- Single-use. A `> 0` check makes a SECOND download structurally impossible rather than a
    -- rule a route has to remember, and the route reads the row after the increment so the second
    -- request sees `2` and answers `410`.
    download_count  int not null default 0 check (download_count >= 0 and download_count <= 1),
    last_downloaded_at timestamptz,
    revoked_at      timestamptz,
    requested_by    uuid,
    requested_by_name text not null,
    error           text,
    created_at      timestamptz not null default now(),
    -- Terminal states carry the timestamp that explains them, for the same reason the backfill job
    -- does: a row a screen cannot render honestly is a bug in the schema.
    constraint anonymized_exports_terminal_timestamps check (
        (status <> 'revoked' or revoked_at is not null)
        and (status <> 'failed' or error is not null)
        and (status <> 'ready' or file_key is not null)
    ),
    constraint anonymized_exports_window_ordered check (
        window_start is null or window_end is null or window_end >= window_start
    )
);

-- The list screen's query: live exports by expiry, so "what can still be downloaded" is one
-- index rather than a filtered scan of every file this installation ever produced.
create index anonymized_exports_open_idx
    on anonymized_exports (expires_at)
    where status in ('queued', 'running', 'ready');
create index anonymized_exports_recent_idx on anonymized_exports (created_at desc);

comment on table anonymized_exports is
    'Anonymised support exports. Single-use (download_count <= 1 is a CHECK, not a rule), expiring, watermarked, revocable, and never containing a classified value — the builder refuses to start while any selected column is unclassified.';

comment on column anonymized_exports.salt_fingerprint is
    'SHA-256 of the per-export salt. Enough to tell two exports apart in the audit trail; useless for attacking either, which the salt itself would not be.';

comment on column anonymized_exports.download_count is
    'Zero or one, enforced by a CHECK. A second download is refused by the route with 410 rather than by a route that remembered to ask.';

-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
--
--   drop table if exists anonymized_exports;
--   drop table if exists column_classifications;
--
-- Children first: nothing references `column_classifications` structurally (the builder reads it
-- with a query, which is not a reference), but `anonymized_exports` is the table an operator
-- looks at, so the reversal drops the artefact before the rulebook that produced it. `if exists`
-- throughout, because a down script that fails halfway leaves an instance neither script can
-- continue from.
--
-- The classified rows go with their table. They are reviewed decisions about THIS installation's
-- columns, and restoring them from a backup after a rollback would restore decisions somebody has
-- since had to change — a map that claims yesterday's classification of a column nobody
-- re-reviewed is worse than no map, because the builder fails closed on a MISSING row and would
-- therefore keep exporting with no map at all.