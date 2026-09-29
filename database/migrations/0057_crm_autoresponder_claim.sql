-- 0057_crm_autoresponder_claim.sql — the autoresponder's exactly-once claim.
--
-- REQ-117, slice 3 (the autoresponder). The REQ says the reply is "sent once per lead", and
-- the obvious way to get that is a conditional insert that checks the trail first:
--
--   insert into crm_lead_events (lead_id, kind, detail)
--   select $1, 'autoresponder_sent', $2
--   where not exists (select 1 from crm_lead_events where lead_id = $1 and kind = '...')
--
-- It reads like a lock and it is not one. Under READ COMMITTED each statement sees the rows
-- committed when *that* statement began, so ten concurrent callers can each evaluate the
-- subquery before any of them commits, and all ten insert. The slice-3 gate measured it:
-- 8 of 10 sends, on a run where 9 of 10 would have been the honest answer to a flaky test.
-- This is the same class as the round-robin cursor in slice 2 — a read-then-write passes every
-- sequential test and fails only under concurrency — and it is why the claim gets a database
-- constraint instead of clever SQL.
--
-- The claim is enforced by a **partial unique index**, which is the one primitive in Postgres
-- that arbitrates a race without a lock the callers can skip:
--
-- 1. `unique (lead_id) where kind = 'autoresponder_sent'` — only one send line per lead, ever.
--    A second concurrent insert raises `unique_violation` and that caller knows, from the
--    error itself, that somebody else is answering. There is no window: the index is checked
--    inside the same statement that inserts.
--
-- 2. `detail->>'sent' = 'false'` rows — a *reservation* for a delayed message — are exempt,
--    because a pending claim and a completed send are different facts about one lead and the
--    worker that sends a reserved message updates the row rather than adding a second one.
--    The `sent` key is written by `autoresponder_store::claim` on every line it writes, so an
--    older row that predates this column reads as `null` and is not `false`, which is why the
--    predicate is on the string and not on a boolean cast that would trip over it.
--
-- `release_claim` deletes the line, so a mailer that refused does not silence a lead forever:
-- the next attempt re-claims. That is why the index is a constraint on a row rather than a
-- sticky column on `crm_leads` — a claim that has to be released is genuinely a row's absence.

-- A lead's autoresponder is claimed at most once, and a pending claim does not block the send
-- that completes it.
create unique index if not exists crm_lead_autoresponder_claim_idx
  on crm_lead_events (lead_id)
  where kind = 'autoresponder_sent' and coalesce(detail->>'sent' = 'false', false) = false;

-- The claim is read by the lead's detail and by the send path, both of which filter on the
-- kind, so the partial index above cannot serve them. This one can.
create index if not exists crm_lead_events_kind_idx
  on crm_lead_events (lead_id, kind, id desc);
