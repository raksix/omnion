-- 0191_health_incident_open_shape.sql — the constraint that made the incidents table unusable.
--
-- `0188_system_health.sql` shipped the incidents table with slice 1 and gave it this constraint:
--
--     check ((resolved_at is null) = (to_state = 'healthy'))
--
-- with the comment *"A run that is still open has no end; a resolved one always does."* The
-- comment describes the truth; the expression is its **inverse**. Read it as written:
--
--   * a row that is still open (`resolved_at is null` → true) must have `to_state = 'healthy'`,
--   * a row that has been resolved (`resolved_at is null` → false) must have `to_state` be
--     something *other* than `healthy`.
--
-- Both halves are backwards, and between them they make the table unable to store the one thing
-- it exists to store. Opening an incident writes `to_state = 'degraded'` with `resolved_at` null,
-- which the constraint refuses with `23514 health_incidents_resolved_shape` — so slice 3's very
-- first `insert` fails, and the walk that proves "a scripted outage produces one incident" fails
-- on the insert rather than on anything about incidents.
--
-- It went unnoticed for three reasons worth recording, because each one is a way a green gate
-- can hide a table nobody has ever written to:
--
--   1. **No test inserted an open, non-healthy incident.** Slices 1 and 2 are about probes and
--      samples; neither touches this table. A constraint on a table with no writer is a comment
--      with a `check` in front of it.
--   2. **`cargo check` cannot see it.** It is a data constraint, not a type error.
--   3. **It is a migration, so the test that would have caught it is the one thing slice 3 adds.**
--      The tick that writes the first row into a table is the tick that finds out what the table
--      allows — which is an argument for writing the row *earlier*, not for trusting the comment.
--
-- The corrected constraint says what the comment always meant: a row is open exactly when it has
-- no resolution time, and a row that has been resolved carries the state it recovered *to*.
--
--     check ((resolved_at is not null) = (to_state = 'healthy'))
--
-- Every row this platform writes satisfies that: an open incident's `to_state` is `degraded` or
-- `down` (never `healthy` — `Transition::is_event` refuses a no-op, and `apply` only opens on an
-- event), and `apply`'s recovery branch is the only thing that sets `to_state = 'healthy'` and it
-- sets `resolved_at = now()` in the same statement. So the new predicate is not a relaxation; it
-- is the same invariant pointed the right way.

alter table health_incidents
    drop constraint if exists health_incidents_resolved_shape;

alter table health_incidents
    add constraint health_incidents_resolved_shape
        check ((resolved_at is not null) = (to_state = 'healthy'));

comment on constraint health_incidents_resolved_shape on health_incidents is
    'A row is open exactly when it has no resolved_at, and a resolved row carries the state it recovered to. (0188 wrote the inverse of this, which made every open non-healthy incident unrepresentable.)';

-- ---------------------------------------------------------------------------------------------
-- The same defect in the sibling table, applied to databases that already ran 0190.
-- ---------------------------------------------------------------------------------------------
--
-- `0190`'s `health_thresholds_pair_check` was `check (warn < crit)`, which is right for an
-- `above` pair and exactly backwards for a `below` one — and `Threshold::classify` reads a
-- `below` pair with `value <= crit` / `value <= warn`, i.e. it *expects* warn > crit. So the
-- store could classify a `below` threshold correctly while the database refused to store it,
-- and the refusal arrived as a bare `23514` with no metric in the message.
--
-- 0190 has already been applied on deployed databases, so editing that file would only fix a
-- fresh install. This statement is what brings an existing deployment to the same shape, and
-- the edit to 0190 above keeps a fresh install from ever needing it.

alter table health_thresholds
    drop constraint if exists health_thresholds_pair_check;

alter table health_thresholds
    add constraint health_thresholds_pair_check check (
        (direction = 'above' and warn < crit) or (direction = 'below' and warn > crit)
    );