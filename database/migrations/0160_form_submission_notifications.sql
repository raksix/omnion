-- The record of a form submission's notification (REQ-064 slice 2).
--
-- The builder stores who to notify; the submit route sends; and until now neither half left
-- anything behind, so "the contact form's mail is not arriving" could be answered only by
-- guessing between "SMTP is down", "the address is wrong" and "the feature was never wired to
-- anything". These three columns are the answer: `notified_at` is when the attempt happened,
-- `notify_status` is what it was, and `notify_error` carries the reason for the two that have
-- one.
--
-- Three states rather than a boolean, because the three failures need three different actions
-- and a boolean collapses them into "did not send":
--
--   * `sent`    — every recipient was reached.
--   * `skipped` — nothing was attempted, and the reason is in `notify_error`: the builder
--                 named nobody (`no recipients configured`) or platform mail is switched off
--                 (`platform email is switched off`). The first is the owner's doing and is
--                 fixed in the builder; the second is the operator's and is fixed in the
--                 environment. They are not the same ticket.
--   * `failed`  — it was attempted and the transport refused.
--
-- The columns are NULL on every row that predates this migration, and NULL means "no attempt
-- was ever recorded" rather than "not sent": a submission row written before the send existed
-- is not a failed send, and a CHECK that called it one would be a lie in the data.
alter table cms_form_submissions
    add column if not exists notified_at timestamptz,
    add column if not exists notify_status text,
    add column if not exists notify_error text;

-- A status is only ever one of the three the route writes, and a row that has a status but no
-- timestamp is a partial write — which is worse than no write, because it looks like a decision
-- somebody made. The constraint is what makes "attempted" mean attempted.
alter table cms_form_submissions
    drop constraint if exists cms_form_submissions_notify_status_check;
alter table cms_form_submissions
    add constraint cms_form_submissions_notify_status_check
    check (notify_status is null or notify_status in ('sent', 'skipped', 'failed'));

alter table cms_form_submissions
    drop constraint if exists cms_form_submissions_notify_recorded_check;
alter table cms_form_submissions
    add constraint cms_form_submissions_notify_recorded_check
    check (notified_at is null or notify_status is not null);

-- The inbox lists one form's submissions newest first and a moderator asks "which of these did
-- we already mail about?" only when a notification went missing, so the index is over the
-- rows that HAVE a record rather than over the whole table: indexing every submission on a
-- column that is null for all of them buys the planner nothing.
create index if not exists cms_form_submissions_notified_idx
    on cms_form_submissions (form_id, notified_at desc)
    where notified_at is not null;
