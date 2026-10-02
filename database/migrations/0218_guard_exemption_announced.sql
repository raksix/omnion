-- One marker per exemption lapse, so `ai.guard.exemption.expired` is announced exactly once.
--
-- WHY THIS EXISTS: REQ-105 asks for "an expired exemption stops applying on the next request and
-- emits `ai.guard.exemption.expired`". Announcing it is *not* a side effect of the read that
-- decides liveness — a row is lapsed from its `expires_at` onward and stays that way forever, so
-- any caller that re-reads it (the exemptions screen, the policy panel, the next request) would
-- announce the same lapse again. Announcing on a schedule would therefore mean announcing the
-- same event N times, and a webhook subscriber counting "how many exemptions expired today"
-- would get a number that depends on how often the process runs.
--
-- The marker is the de-duplication key, and it is *idempotent by construction*: the insert is
-- `on conflict do nothing`, and the caller announces only when the insert actually inserted.
-- So the marker's presence — not a read of "have I announced this?" — is the proof, and two
-- processes racing on the same exemption produce one announcement between them rather than two.
--
-- `announced_at` rather than a boolean so the row says WHEN the announcement went out, which is
-- the question an operator asks when a subscriber reports they never got the event. `expires_at`
-- is carried as a copy because the announcement's watermark read is `expires_at > since`, and
-- keeping the value here lets that read be served from this table alone if the read is ever
-- indexed by it.
--
-- IDEMPOTENCE: guarded on **both** statements, deliberately. A guard on the table alone is the
-- half-protected shape, and it is the worst of the two: applying the file to a database that
-- already has the table dies on the index, which means the ledger row is never written, which
-- means every later `migrate()` re-runs this file and dies in the same place. A migration that
-- is idempotent has to be idempotent *as a file*, not as its first statement. Both were proved by
-- running this file twice against a live database.
create table if not exists ai_guard_exemption_announced (
    exemption_id   uuid        primary key references ai_guard_exemptions (id) on delete cascade,
    organization_id uuid       not null references organizations (id) on delete cascade,
    label          text        not null,
    expires_at     timestamptz not null,
    announced_at   timestamptz not null default now()
);

-- The announcement sweep reads by organization and watermark; without this it is a seq scan over
-- every lapse this tenant has ever had, once per request that runs it.
create index if not exists ai_guard_exemption_announced_org_idx
    on ai_guard_exemption_announced (organization_id, expires_at);
