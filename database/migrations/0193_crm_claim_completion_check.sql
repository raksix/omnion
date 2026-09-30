-- REQ-117, slice 19b — a lead with a submission claim can be deleted.
--
-- ## The contradiction
--
-- Two constraints on `crm_lead_submissions` disagree about exactly one case, and the
-- disagreeing case is the one an operator reaches for:
--
--   constraint crm_lead_submissions_completion
--       check ((lead_id is null) = (completed_at is null))
--   constraint crm_lead_submissions_lead_id_fkey
--       foreign key (lead_id) references crm_leads (id) on delete set null
--
-- `capture` finishes a claim by writing **both** `lead_id` and `completed_at`, so a completed
-- claim is a row with neither null. Now delete the lead that claim names. The FK's
-- `on delete set null` fires, `lead_id` becomes NULL, `completed_at` stays set — and the check
-- evaluates `true = false` and raises:
--
--   23514 new row for relation "crm_lead_submissions" violates check constraint
--         "crm_lead_submissions_completion"
--
-- The FK's action is not a bad intention and the check is not badly written in isolation:
-- each enforces a real property, and together they leave no state for a deleted lead.
-- **A check and a referential action that can never both hold is not a constraint, it is a
-- contradiction that only the pair reveals.**
--
-- ## The first fix, and why it was wrong
--
-- The obvious repair is to *narrow* the check to `lead_id is not null or completed_at is
-- null` — permit a claim that names no lead. **That does not fix it, and it fails in exactly
-- the direction it was written for:** the narrowed check *forbids* `lead_id null` with
-- `completed_at` set, which is precisely the row `on delete set null` produces. It reads as a
-- fix because it weakens the constraint, and the delete still raises `23514`. The lesson is
-- worth more than the fix: *weakening a constraint until it stops complaining about a
-- referential action is how a contradiction gets made permanent.* The question is which of
-- the two the product actually wants, not which one is easier to relax.
--
-- ## The question
--
-- Does a claim outlive the lead it names? No, and that is the whole argument. A claim exists
-- to make "one submission, one lead" hold across an at-least-once delivery. Once the lead is
-- deleted there is no result left to deduplicate *against*, so a surviving claim row asserts a
-- completeness the platform can no longer vouch for — and for a visitor who asked to be
-- removed it is also one more durable trace of them, which is the opposite of what the
-- deletion is for.
--
-- So the claim goes with the lead: `on delete cascade`. The completion check keeps every bit
-- of its meaning, because the row that would break it no longer exists to be updated.
--
-- The redelivery answer is unchanged either way, which is why the tests pin it explicitly: a
-- claim that is gone and a claim that is open both read as "not done", so a redelivery of a
-- deleted lead's submission captures again. That is correct — the operator removed the
-- result, so a fresh delivery should produce a fresh result.
--
-- ## Additive in effect
--
-- Every existing row keeps the same completion check; only the referential action changes. No
-- row is re-read and no row changes status.
alter table crm_lead_submissions
    drop constraint if exists crm_lead_submissions_lead_id_fkey;

alter table crm_lead_submissions
    add constraint crm_lead_submissions_lead_id_fkey
    foreign key (lead_id) references crm_leads (id) on delete cascade;

comment on constraint crm_lead_submissions_lead_id_fkey on crm_lead_submissions is
    'Cascade, not set null: a claim exists to make "one submission, one lead" hold, and it '
    'cannot outlive the lead it deduplicates against. With on delete set null, deleting a '
    'lead updated the claim to lead_id null while completed_at stayed set, which '
    'crm_lead_submissions_completion refuses — so deleting a lead that has a claim raised '
    '23514, on the route an operator reaches when a visitor asks to be removed.';
