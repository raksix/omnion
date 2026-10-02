-- 0150_crm_lead_submission_claims.sql — make "one submission, one lead" a fact of the data,
-- not a promise the writer makes and two deliveries can break.
--
-- REQ-117, slice 1 (capture and inbox). The acceptance line "a REQ-064 form bound to a source
-- submits, produces one lead ... (one submission, one lead, no duplicates)" was the last
-- unticked box in the request, and it was unticked because it is the only one whose failure is
-- invisible: a second lead does not raise, does not log an error, and does not appear on any
-- screen as anything other than two rows that both look right.
--
-- **What the code actually guaranteed.** `store::capture` opened with a read:
--
--     if let Some(submission_id) = submission.submission_id.as_deref() {
--         if let Some(existing) = find_lead_by_submission(pool, source.id, submission_id).await? {
--             return Ok(/* the earlier lead */);
--         }
--     }
--
-- and then inserted. Between the two statements there is no lock, no constraint and nothing for
-- a second delivery to collide with. The platform's own events are at-least-once by contract
-- (`crm.contact.merged` re-points leads, `sales.quote.accepted` is idempotent on
-- `converted_at` precisely because the bus may deliver twice), so two deliveries of the same
-- `content.form.submitted` event arriving together both read "not found" and both write a
-- lead. The second one is a real lead with a real dedupe verdict — it is a duplicate, not an
-- error, which is why no gate in this crate ever went red.
--
-- Read-then-write is the shape that passes every sequential test. The concurrency half of
-- `scripts/qa/run-crm-assignment.sh` exists for exactly this reason ("a read-then-write cursor
-- passes every sequential test and fails here, intermittently") and the same defect in a
-- different column was the reason that gate exists. This one had no such gate.
--
-- **The claim is the fix, and the key is the claim.** A row per delivery identity, with the
-- primary key doing the work:
--
--   crm_lead_submissions (source_id, submission_id, lead_id, claimed_at)
--     primary key (source_id, submission_id)
--
-- The insert is `on conflict do nothing`, so of two simultaneous deliveries exactly one inserts
-- a claim and the other reads the claim the winner wrote. The key is scoped to the *source* and
-- not to the organization, because a source id is already organization-scoped and the narrower
-- key is the one that lets the index answer "has this source already seen this submission"
-- with a single probe.
--
-- **Why `lead_id` is nullable and why that is the whole design.** The claim is taken *before*
-- the work, so it has to survive a capture that fails afterwards — if the claim and the lead
-- were one statement, a mapping error would release the claim and a retry would be correct, but
-- so would two simultaneous retries. Instead the claim is written alone, the lead is written
-- with it, and the claim is completed. A claim that is still `lead_id is null` is therefore one
-- of two things, and the columns say which:
--
--   * a capture in flight — `claimed_at` is seconds old;
--   * a capture that died — `claimed_at` is older than `STALE_AFTER_SECONDS`.
--
-- The first reader waits. The second takes it over, with a compare-and-swap on `claimed_at` so
-- two recoverers cannot both take over. This is what stops a crashed capture from wedging the
-- form for ever, which is the failure mode a bare unique index would have: the submission is
-- permanently un-capturable and the only remedy is a manual database write.
--
-- **The migration adds a table and touches no lead row.** Existing leads are not backfilled:
-- a claim can only be created by a delivery, and a delivery that already happened cannot be
-- distinguished from one that never did. Backfilling from `crm_lead_events` would invent claim
-- rows for submissions whose delivery was retried and lost, and the honest state for those is
-- "unknown", which is the state an absent row already means.
--
-- No index on `lead_id`: nothing reads a lead's claim by lead, and the reverse lookup would be
-- a second thing to keep in step with the first. The one index that is needed is for the
-- recovery sweeper, and it is partial — a completed claim is never stale and must not be
-- scanned.

create table crm_lead_submissions (
    source_id     uuid        not null references crm_intake_sources (id) on delete cascade,
    submission_id text        not null,
    lead_id       uuid        references crm_leads (id) on delete set null,
    claimed_at    timestamptz not null default now(),
    completed_at  timestamptz,
    constraint crm_lead_submissions_key
        primary key (source_id, submission_id),
    -- The id is the caller's, not ours: it travels in a header and lands in a text column, and
    -- a value longer than this is either a mistake or an attempt to make the claim key wide
    -- enough to be expensive. `capture` truncates at the same bound, so the ceiling is stated
    -- in the database and enforced in the writer rather than only in the writer.
    constraint crm_lead_submissions_id_length check (char_length(submission_id) between 1 and 128),
    -- A completed claim points at a lead; an open one does not. A claim that is neither is the
    -- one shape this table must never hold, and the check is what makes "completed" mean it.
    constraint crm_lead_submissions_completion
        check ((lead_id is null) = (completed_at is null))
);

comment on table crm_lead_submissions is
    'One row per delivery identity of a form submission. The primary key is what makes one submission, one lead true of the data: a retried or duplicated content.form.submitted delivery collides with the winner''s claim instead of writing a second lead.';
comment on column crm_lead_submissions.lead_id is
    'The lead this claim produced. Null while the capture is in flight, and null forever if the capture died - claimed_at is what separates the two, and a stale open claim is taken over rather than left to wedge the form.';
comment on column crm_lead_submissions.claimed_at is
    'When the claim was taken. A compare-and-swap takeover compares against this value, so a recoverer can only take a claim that was already stale when it looked.';

-- The recovery sweeper's index: only an *open* claim can be stale, and a completed one is the
-- overwhelmingly common row. A partial index keeps the sweeper's scan proportional to the
-- crashes rather than to the traffic.
create index if not exists crm_lead_submissions_open_idx
    on crm_lead_submissions (claimed_at)
    where lead_id is null;

-- The takover reads "this source's claim, and is it still the one I looked at". The primary key
-- already answers the first half; this index exists so the sweeper's `where lead_id is null`
-- is an index-only scan on a growing table.
create index if not exists crm_lead_submissions_lead_idx
    on crm_lead_submissions (lead_id)
    where lead_id is not null;
