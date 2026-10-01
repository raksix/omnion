-- 0202_crm_autoresponder_immediate_claim.sql — an immediate autoresponder claim is not a
-- delivered one, and the rows already written say otherwise.
--
-- REQ-117 slice 3 says the autoresponder is "sent once per lead". The claim that enforces it
-- (`0058_crm_autoresponder_claim.sql`) is a partial unique index over the rows that *claim*
-- the send, and both writers of `detail->>'sent'` disagreed about what the key meant:
--
--   * `autoresponder_store::claim` wrote `sent = !message.delayed`, so an IMMEDIATE message
--     was stored as delivered at the moment it was reserved — before the mailer was touched.
--   * `release_claim` and `mark_sent` both key on `sent <> 'true'`, i.e. "this row has not
--     been delivered yet".
--
-- Those are contradictory, and only for the immediate path, which is the default every source
-- that never touched the delay control is on. Three consequences, all live before this file:
--
--   1. A refused immediate send released nothing. The claim stayed, `prepare` answered
--      `AlreadySent` for ever, and the visitor's one reply was lost — silenced by the very
--      mechanism that exists to guarantee it.
--   2. The completion of an immediate send updated zero rows and returned `Ok(false)`, which
--      the callers cannot distinguish from "another worker won". No `sent_at` was ever
--      written for an immediate message, so the timeline showed when the line was *claimed*
--      and never when the mail *left*.
--   3. An operator reading the trail saw "sent" on a message whose delivery is unknown. The
--      line is what the REQ's PII and audit discipline is written against: it is read by the
--      lead detail screen and by every audit export.
--
-- **Why this is a migration and not only a code change.** An installation that has been
-- running has rows written by the old writer, and `sent = true` on them is precisely the
-- claim about the past that this file exists to retract. The honest repair is not to flip them
-- to `false` — that would make every one of them claimable again, and re-answering leads the
-- operator believes were answered is worse than the defect — but to mark the claim line
-- **resolved without a recorded delivery**, so the row keeps its uniqueness (the lead stays
-- answered) and stops asserting a delivery nobody can prove.
--
-- `delivered_at` is NULL for a row whose delivery was never observed, which is the truth: the
-- immediate path recorded the reservation instant in `created_at` and nothing else. `sent`
-- stays `true` — the message *is* no longer sendable, and `prepare` must keep answering
-- `AlreadySent`; changing that would reopen the lead to a second copy. What changes is that a
-- reader can now tell "delivered, and here is when" from "never claimed as delivered at all",
-- and `mark_sent` no longer refuses to complete such a row.
--
-- The partial index predicate is deliberately untouched: `detail ? 'sent'` still means "this
-- row claims the send", which remains true for every row here, and rewriting the predicate
-- would make a restored dump able to hold two claims for one lead.

update crm_lead_events
   set detail = detail || jsonb_build_object(
         'sent', true,
         'delivered_at', null,
         'delivery_unknown', true
       )
 where kind = 'autoresponder_sent'
   and detail ? 'sent'
   and detail->>'sent' = 'true'
   and detail->>'delayed' = 'false'
   and not (detail ? 'delivered_at');

comment on column crm_lead_events.detail is
  'The structured detail of one trail line. An autoresponder claim line carries '
  'to/subject/template/delayed/due_at/sent, plus delivered_at once a delivery is observed. '
  'A row written by the pre-0202 code marked an immediate claim sent at reservation time; '
  'such rows carry delivery_unknown = true and a null delivered_at, because the old code '
  'recorded no delivery instant for them. sent = true on those rows still means the lead is '
  'answered and must not be sent again.';