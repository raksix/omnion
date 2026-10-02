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
-- The predicate says "a row that CLAIMS the send", and it is written as `detail ? 'sent'`
-- rather than the earlier `coalesce(detail->>'sent' = 'false', false) = false` for a reason
-- the gate measured. A *skip* line (`record_skip`) carries `reason` and `source` and no
-- `sent` key at all, so `detail->>'sent' = 'false'` is NULL for it, `coalesce` turns that
-- into `false`, and the line looked like a CLAIM to the index — occupying the one slot a
-- lead has. A lead whose autoresponder skipped once could then never be answered at all, and
-- `prepare` would report `AlreadySent` for a message that had never gone anywhere.
--
-- `detail ? 'sent'` is the honest question: does this row claim the send? A reservation and a
-- completed send both do; a note about why nothing went out does not.
create unique index if not exists crm_lead_autoresponder_claim_idx
  on crm_lead_events (lead_id)
  where kind = 'autoresponder_sent' and detail ? 'sent';

-- The claim is read by the lead's detail and by the send path, both of which filter on the
-- kind, so the partial index above cannot serve them. This one can.
create index if not exists crm_lead_events_kind_idx
  on crm_lead_events (lead_id, kind, id desc);

-- The due sweep of the autoresponder worker (REQ-117, slice 3).
--
-- A source can configure a send delay so the acknowledgement lands after the salesperson's
-- own reply. `capture` then *reserves* the slot and nothing sends it until the delay elapses,
-- so the worker's query is "what is due", and it runs once a minute against every
-- installation. Three properties of that query decide what this index must be:
--
--   * It filters on `kind = 'autoresponder_sent'` AND on the *jsonb* keys `sent` and
--     `due_at`. A plain `(lead_id, kind, id)` index cannot serve it, because the predicate it
--     needs is on the row's JSON, and a worker that scans the whole trail once a minute
--     re-reads every lead's entire history for ever.
--   * It orders by the due instant. Ordering by an indexed expression is what lets Postgres
--     stop at the LIMIT rather than sort the table.
--   * The partial predicate is the point: the trail is the busiest table a lead has (every
--     assignment, every status change, every conversion), and this index covers only the rows
--     that are reservations — one per lead, and only while it is waiting.
--
-- **The index is on the TEXT, not on the timestamp, and that is not a compromise.** The first
-- version cast the expression — `on crm_lead_events (((detail->>'due_at')::timestamptz), id))`
-- — and Postgres refused the whole migration with `functions in index expression must be
-- marked IMMUTABLE`, because `text::timestamptz` is `STABLE`: it depends on the session's
-- `DateStyle` and `TimeZone`, so the same row could sort to a different instant on two
-- machines. The migration therefore aborts and the worker never starts, which is a far worse
-- outcome than a slightly larger index — and a `CREATE INDEX` on a STABLE expression that
-- *had* been accepted would be silently wrong, not merely slow.
--
-- So the index sorts the stored string and the query compares the stored string. That is
-- correct only because the value is always written in one format by one writer
-- (`autoresponder_store::claim` formats `due_at` with `date_header`, RFC 2822, zero-padded
-- and fixed-width), and RFC 2822 sorts chronologically for the same zone. The one real
-- hazard is the empty string, which `nullif` absorbs in the predicate and the query's own
-- `nullif` guards against: rows written before the delay feature existed have no key at all
-- (NULL), and a hand-edited row with `""` would otherwise be `invalid input syntax for type
-- timestamp` — a predicate that raises turns the whole worker into a 500.
--
-- The cast still happens in the query's `where` clause, where STABLE is allowed and the
-- DateStyle of the session is a constant for the life of the statement.
-- The double parentheses are not optional: `on t (detail->>'due_at', id)` parses as a *column
-- list* with a stray operator and is a syntax error, while `on t ((detail->>'due_at'), id)`
-- parses as an expression list. The first version of this line had one pair and the
-- migration aborted at `syntax error at or near "->>"`.
create index if not exists crm_lead_autoresponder_due_idx
  on crm_lead_events (((detail->>'due_at')), id)
  where kind = 'autoresponder_sent'
    and detail->>'sent' = 'false'
    and nullif(detail->>'due_at', '') is not null;
