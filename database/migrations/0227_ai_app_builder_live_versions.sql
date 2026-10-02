-- Omnion · 0227 · AI app builder: one LIVE version per (plan, kind, key)
--
-- 0224 declared `app_builder_artifacts_unique_key (plan_id, kind, key)` as a table constraint.
-- That is right for `insert_artifact` and **wrong for the regeneration path**, and the
-- mismatch is not subtle: `supersede_artifact` keeps the previous version (the request asks
-- for it — "kept plan versions so a rejected attempt can be compared") and marks it
-- `rejected`, which the index still counts as occupying `(plan_id, kind, key)`.
--
-- So the first shape a regeneration takes is `duplicate key value violates unique constraint`,
-- surfacing as a `500` from the review screen's Regenerate button — and it cannot be fixed by
-- reordering the statements inside the transaction, because **both rows exist at the end no
-- matter which one is written first**. The transaction makes the write atomic; it cannot make
-- a row disappear.
--
-- The replacement is a PARTIAL unique index over the **live** versions only:
--
--     status in ('pending', 'accepted', 'edited', 'invalid')
--
-- That is the property the store has actually been enforcing all along, stated in the one
-- place the database can enforce it: two live versions of one artifact is the bug the index
-- exists to prevent (apply would count it twice and the review tree would draw it twice), while
-- an arbitrary number of *retired* versions is the version history the request asks for.
--
-- `rejected` is the only excluded status and that is not a loophole — a rejected row is not
-- applicable (`artifact_is_resolved` admits only `accepted` and `edited`), so a plan cannot
-- apply a version nobody is looking at. A second regeneration of the same artifact therefore
-- retires the already-rejected predecessor again rather than colliding, which is the correct
-- reading: the newest live version is the one a reviewer sees.

-- The absolute constraint goes first; its name is kept for the partial index below so a
-- database that has already reported the violation by name still reads coherently.
alter table app_builder_artifacts
    drop constraint app_builder_artifacts_unique_key;

create unique index app_builder_artifacts_live_key
    on app_builder_artifacts (plan_id, kind, key)
    where status in ('pending', 'accepted', 'edited', 'invalid');

comment on index app_builder_artifacts_live_key is
    'One LIVE version per (plan, kind, key). Retired (rejected) versions are kept as history, which is why this is partial rather than a table constraint.';