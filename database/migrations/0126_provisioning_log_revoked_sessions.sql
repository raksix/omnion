-- Omnion · 0126 · the sync log carries how many sessions a deactivation ended (REQ-065, slice 4).
--
-- `f0246f4` made the revocation real and put the count into `detail` as English:
-- `"… now reads disabled — 3 session(s) ended"`. That reads well and is worth nothing to the
-- platform. A number inside a sentence cannot be summed, filtered, sorted, asserted on, or
-- rendered as a number by the screen that exists to show it, and every consumer that wants it has
-- to re-parse prose in the one language the sentence was written in.
--
-- The column is `not null default 0` rather than nullable. Five of the six places that write a
-- sync-log line have no revocation to report, and "none" is the true value for all of them; a
-- null would add a third state ("unknown") that nothing can produce and every reader would have
-- to decide about.
--
-- Backfill is unnecessary and deliberately so: existing rows describe deactivations from before
-- the count existed, and their true value is unknowable from the log. Zero is not a claim that
-- nothing was revoked — it is the absence of a claim, which is exactly what those rows hold.

alter table provisioning_log
    add column revoked_sessions integer not null default 0;

-- The count cannot be negative: there is no state in which ending a session produces -1, and a
-- negative value reaching the panel would render as a badge saying "−3 sessions ended", which is
-- a sentence about nothing. The database says so rather than the panel.
alter table provisioning_log
    add constraint provisioning_log_revoked_sessions_check check (revoked_sessions >= 0);

-- A count nobody reads is still a number in a table, so this is the index that makes the panel's
-- question answerable: "which push ended the most sessions" is the shape of the question an
-- operator asks after an offboarding, and it is a scan without this.
create index provisioning_log_revoked_idx
    on provisioning_log (organization_id, created_at desc)
    where revoked_sessions > 0;

comment on column provisioning_log.revoked_sessions is
    'Live sessions ended by this line. Zero means the line revoked nothing (or predates 0126 and was never counted).';
