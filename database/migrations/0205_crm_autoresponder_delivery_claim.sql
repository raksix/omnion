-- 0205_crm_autoresponder_delivery_claim.sql — the send is claimed before it is attempted, not
-- after, and a claim a dead worker left behind is recoverable.
--
-- ## The defect this column exists for
--
-- REQ-117 slice 3 says the autoresponder is "sent once per accepted lead". Two mechanisms
-- claim that, and until this file they claimed different halves of it at different times:
--
--   * `0058_crm_autoresponder_claim.sql` — a partial UNIQUE index on the claim rows, arbitrating
--     the *reservation*. `prepare` takes it, so the immediate path was arbitrated correctly
--     before the mailer was ever touched.
--   * the worker — `due_reservations` is a plain READ (`sent = 'false' and due_at <= now`, no
--     lock), and `send_one` then called the mailer FIRST and `mark_sent` afterwards. `mark_sent`
--     is the completion, so the only arbiter the send path had was consulted **after** the
--     irreversible act.
--
-- The runner's own header states the rule the code broke, one sentence before the code: *"a
-- claim is taken **before** the send, and a completion is recorded **after** it, or two workers
-- both mail."* Two app instances on one database both receive the same due row. Both called the
-- mailer. The loser's `Ok(false)` branch then logged "another worker had already completed this
-- autoresponder" — and that log line is worse than no log at all, because the trail showed one
-- send while the visitor received two.
--
-- ## Why a column and not a jsonb key
--
-- The obvious repair writes `detail->>'delivery_claimed_at'` and reads it back, and that is
-- what the rest of this file's jsonb does. It cannot carry the second half of the fix, which is
-- **recovery**: a worker that dies between claiming and sending leaves a claim that must become
-- claimable again, or the reservation is skipped for ever by the very mechanism meant to answer
-- it.
--
-- Comparing "older than the staleness window" against a stored RFC 2822 string means lexical
-- text order standing in for time order — the same trick `due_at` plays, and it is correct only
-- because every writer shares one formatter *in one zone*. A claim written by a worker running
-- under a different `TimeZone` sorts to the wrong side of the threshold, and the bias is toward
-- never recovering. The due sweep can afford that: being early costs nothing. A send claim
-- cannot — the failure is a lead that is never answered.
--
-- So the claim is a real `timestamptz` and the comparison is a real comparison. Nullable, so
-- every existing row — on every installation — reads as unclaimed without a backfill, and
-- additive: no row is re-read, and no row changes status.
--
-- ## The window, and the bias behind it
--
-- One minute. The bound it must respect is `OMNION_SMTP_TIMEOUT_MS` (default 10s, and an
-- operator may raise it): a claim cannot be reclaimed before the send it guards could possibly
-- still be running, or two workers really do mail. One minute is comfortably past a
-- pathological 50-second SMTP conversation while still being recovered by the *next* worker
-- tick, which is itself one minute — so a crashed worker's reservation is answered within two
-- ticks and never depends on a restart.
--
-- The bias is the module's own, stated in `claims.rs` and unchanged here: **a duplicate is a
-- permanent, invisible data defect; a delayed send is a temporary, visible one.** The window is
-- therefore biased long. Understating it turns a crashed worker into two visitors receiving the
-- same acknowledgement; overstating it turns one into a lead answered a minute late.
--
-- ## Why the index is partial
--
-- `crm_lead_events` is the busiest table a lead has: every assignment, every status change, every
-- conversion writes a line, for ever. This index covers only rows a worker has claimed and not
-- yet resolved — at most one per lead, and only while a send is in flight, which is bounded by
-- the SMTP timeout. Building it on the whole table would put a second copy of the trail's hot
-- path on disk to serve a scan that is nearly always empty.
alter table crm_lead_events
    add column if not exists delivery_claimed_at timestamptz;

comment on column crm_lead_events.delivery_claimed_at is
    'When a worker claimed the right to attempt this line''s autoresponder send. NULL on every '
    'other kind of trail line, and NULL on an autoresponder claim nobody is sending, which is '
    'what "claimable" means. Set BEFORE the mailer is touched and cleared by the release that '
    'follows a failed send; a row whose value is older than the staleness window is reclaimable, '
    'because the worker that wrote it died between claiming and sending. Written as jsonb on '
    'the detail instead of as a column would make that comparison lexical, and a claim written '
    'in another session timezone would sort to the wrong side of the threshold — the one bias '
    'this cannot afford, since it is toward never recovering.';

create index if not exists crm_lead_events_delivery_claimed_idx
    on crm_lead_events (delivery_claimed_at)
    where delivery_claimed_at is not null;
