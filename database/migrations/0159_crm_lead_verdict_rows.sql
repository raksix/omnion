-- 0159_crm_lead_verdict_rows.sql — a refusal is a row, so the contactable check has to allow one.
--
-- REQ-117 acceptance 5: "A submission missing both e-mail and phone is refused with a readable
-- reason, and no partial lead row is written." The store's half of that shipped in slice 1 and
-- reads correctly in isolation:
--
--     // 3. Nothing to contact: a rejected row, never a partial lead.
--     if !mapped.missing_required.is_empty() || !contactable(email, phone) {
--         let reason = ...;
--         let lead = insert_lead(..., LeadWrite { status: "rejected", rejection_reason: Some(reason), ... });
--
-- **On this branch that line raises `23514` and the row is never written.** `crm_leads` carried
--     constraint crm_leads_contactable_check check (coalesce(email,'') <> '' or coalesce(phone,'') <> '')
-- and the row being inserted is by construction a row with neither. So the one path the REQ
-- promises a row for is the one path the database refuses, and the failure lands on the visitor
-- as a 500 rather than as a readable reason.
--
-- It was invisible for twenty ticks because nothing drove it: the `rejected` branch needs a
-- submission whose *mapping* produces no e-mail and no phone, and every CRM gate's fixture maps
-- `email`. Two sibling test files document the absence instead of testing it —
-- `crm_autoresponder.rs` says the case is "unreachable through this fixture on purpose" and
-- `crm_binding_health.rs` says the health check's `Unknown` state "is not reachable through
-- capture on this branch" — each in the exact words of a workaround, neither of them a test.
--
-- ## What the constraint was actually claiming
--
-- Its comment says: "A lead with neither e-mail nor phone cannot be contacted, so it is not a
-- lead." That is a true claim about **work** and a false claim about **rows**. The same file's
-- own header, and the REQ's data model, say the opposite: "Leads are never hard-deleted by
-- automation; the reject/spam paths keep the row and record the reason." A hole where a refusal
-- belongs is worse than the row: a broken mapping that has not submitted since the rename reads
-- as a form nobody used, and the operator goes looking for a traffic problem that is not there.
--
-- So the check is narrowed to what it means. The platform refuses a row it is willing to put in
-- somebody's inbox as work, and records a row it is not:
--
--     check (
--         status in ('rejected', 'spam', 'duplicate')
--         or coalesce(email, '') <> ''
--         or coalesce(phone, '') <> ''
--     )
--
-- **Why those three and not `converted`.** `vocabulary::STATUSES` calls `converted`, `duplicate`,
-- `spam` and `rejected` terminal, and the SLA index agrees. But `converted` is a lead that WAS
-- work: it has a `contact_id` and a `deal_id` from a conversion, so its addressability was proven
-- before it got there, and `store::patch_lead` still refuses an edit that would clear both. A
-- `duplicate` is a lead somebody might still link, so it keeps the rule; the one edit that turns
-- a duplicate into an answerable lead is the whole point of the duplicate queue.
--
-- **The store's own guard is unchanged and is the real editor-side rule.** `patch_lead` computes
-- the merged e-mail and phone and refuses the edit before the database sees it, so an operator
-- cannot strip the last address off a rejected row either. That is deliberate and it is now the
-- *only* thing refusing that particular edit, which is why the constraint's comment is rewritten
-- to name it rather than claim the job twice.
--
-- ## Additive and reversible
--
-- `drop constraint if exists` then `add constraint` in one statement, so an installation that
-- renamed or removed the check does not fail the migration. Every row that satisfies the old
-- check satisfies the new one — the predicate is a superset, which is the definition of additive
-- — so no existing lead is re-examined and no data is rewritten.

alter table crm_leads
  drop constraint if exists crm_leads_contactable_check,
  add constraint crm_leads_contactable_check
    check (
      status in ('rejected', 'spam', 'duplicate')
      or coalesce(email, '') <> ''
      or coalesce(phone, '') <> ''
    );

comment on constraint crm_leads_contactable_check on crm_leads is
  'An open lead must be answerable. rejected/spam/duplicate rows are verdicts rather than work — '
  'REQ-117 requires a submission with no e-mail and no phone to be recorded with its reason, and '
  'a check that refuses the row loses the only evidence that the submission arrived. store::patch_lead '
  'refuses the matching edit in the application, where the message can name the rule.';
