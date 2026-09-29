-- 0052_webhook_delivery_ops.sql — the delivery operations of REQ-016 slice 2 and 3.
--
-- Slice 1 built the bus's read side (the feed, the catalogue, the filters). This migration
-- adds the columns the delivery operations screen needs, and every one of them exists because
-- the answer the operator is asking for cannot be computed from what is already stored:
--
-- 1. **`trigger`.** A delivery row is not one kind of thing. A `test` delivery was queued by
--    an operator pressing a button and says nothing about the organization's real traffic; an
--    `event` delivery carries a fact somebody subscribed to; a `replay` delivery carries an
--    event that already went out once and is being sent again by hand. Summing all three into
--    one success rate is the one number a webhook screen must never show, because a green rate
--    bought with manual replays hides the thing the rate was supposed to reveal.
-- 2. **`duration_ms`.** A receiver that answers 200 in four seconds is a receiver that will
--    start timing out under load, and the operator learns that from the number before the
--    deliveries start failing. `response_status` alone cannot say it.
-- 3. **`redeliver_count`.** Retrying a delivery by hand is fine the first time and a loop the
--    fifth. The cap (10) turns "fix the receiver and press it again" into something the
--    platform will refuse, and a refusal with a number is more useful than a silent ceiling.
-- 4. **`replayed_at`.** *When* somebody last forced a redelivery, which is the only way to
--    tell a delivery that is genuinely late from one an operator is chasing right now.
--
-- The existing `webhook_deliveries_endpoint_event_key` unique index is deliberately **not**
-- touched: a redelivery *resets* the row rather than inserting a second one, because a queue
-- that can hold two rows for the same event sends the same fact twice and the receiver cannot
-- tell the replay from a duplicate. Resetting is also what makes `attempts` meaningful — it
-- counts the try in flight for the current round, not across the endpoint's whole life.
--
-- `duration_ms` is nullable rather than defaulted to 0: a pending row has no duration, and 0
-- would render as "answered instantly", which is a claim the platform cannot make.
alter table webhook_deliveries
    add column if not exists trigger text not null default 'event',
    add column if not exists replayed_at timestamptz,
    add column if not exists duration_ms integer,
    add column if not exists redeliver_count integer not null default 0;

alter table webhook_deliveries
    drop constraint if exists webhook_deliveries_trigger_check;
alter table webhook_deliveries
    add constraint webhook_deliveries_trigger_check
        check (trigger in ('event', 'test', 'replay'));

alter table webhook_deliveries
    drop constraint if exists webhook_deliveries_duration_check;
alter table webhook_deliveries
    add constraint webhook_deliveries_duration_check
        check (duration_ms is null or duration_ms >= 0);

alter table webhook_deliveries
    drop constraint if exists webhook_deliveries_redeliver_check;
alter table webhook_deliveries
    add constraint webhook_deliveries_redeliver_check
        check (redeliver_count between 0 and 10);

-- The deliveries screen filters by status and window before it draws anything, and the stats
-- tab counts the same three buckets. Both read `status` first: an endpoint with a large history
-- and a handful of failures must not scan its whole queue to answer "is anything failing".
create index if not exists webhook_deliveries_status_created_idx
    on webhook_deliveries (status, created_at desc);

-- The per-endpoint variant, which is the one the detail screen actually uses: it is always
-- filtered by endpoint, then usually by status, and always ordered newest first.
create index if not exists webhook_deliveries_endpoint_status_created_idx
    on webhook_deliveries (endpoint_id, status, created_at desc);

-- The feed's two own filters, added in slice 1's shape and kept here so a busy organization's
-- feed stays a keyset read rather than a scan when it is narrowed by site or by actor.
create index if not exists events_site_id_id_idx on events (site_id, id desc);
create index if not exists events_actor_user_id_id_idx on events (actor_user_id, id desc);
