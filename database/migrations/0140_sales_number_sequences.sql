-- Sales: the per-organization document numbering (docs/requests/REQ-052, slice 2).
--
-- `0053_sales.sql` already declares the quote tables, so this adds only what the quote *writer*
-- needs and that table did not: somewhere to keep the next document number.
--
-- Why a table and not a PostgreSQL sequence or `max(number) + 1`:
--
--   * A **sequence** cannot be gap-free, and the acceptance criteria ask for gap-free numbering.
--     Every rollback — and every failed create — burns a number, so `Q-2026-0007` would be
--     missing from a folder of otherwise consecutive documents and somebody would ask why.
--   * **`max(number) + 1`** has two concurrent creates read the same maximum and both insert it;
--     the unique index on `(organization_id, number)` then turns that race into a 500 on the
--     second request rather than a second number.
--
-- A row per `(organization_id, kind)` taken `for update` inside the caller's transaction gives
-- both: the lock serializes the readers, and because the increment is in the same transaction as
-- the insert, a rolled-back quote takes its number back with it.

create table sales_number_sequences (
    organization_id  uuid    not null references organizations (id) on delete cascade,
    -- 'quote' or 'order'. A text key rather than a boolean because a third document kind
    -- (REQ-054's invoices) is certain and a boolean would have to be migrated at that point.
    kind             text    not null,
    -- The number the **next** document of this kind gets. Named after the column the module
    -- binds so a reader of the SQL does not have to guess which side of the increment this is.
    next_quote       bigint  not null default 1,
    updated_at       timestamptz not null default now(),

    primary key (organization_id, kind),
    constraint sales_number_sequences_kind_check check (kind in ('quote', 'order')),
    constraint sales_number_sequences_positive check (next_quote >= 1)
);

comment on table sales_number_sequences is
    'The next document number per organization and kind; the counter is taken for update inside the creating transaction, which is what makes the numbering gap-free.';

-- Read-heavy, written twice per document at most: the index the lock reads is the primary key,
-- and this one serves the (rare) "how many documents has this organization issued" question the
-- organization screen asks.
create index sales_number_sequences_by_kind
    on sales_number_sequences (kind, updated_at desc);

-- Every organization starts at 1. Existing installations that already carry documents are seeded
-- from their own highest number instead, so landing this migration on a live installation cannot
-- reissue `Q-1`.
insert into sales_number_sequences (organization_id, kind, next_quote)
select id, 'quote',
       coalesce((
           select max(nullif(regexp_replace(number, '[^0-9]', '', 'g'), '')::bigint) + 1
             from sales_quotes q
            where q.organization_id = organizations.id
       ), 1)
  from organizations
on conflict (organization_id, kind) do nothing;

insert into sales_number_sequences (organization_id, kind, next_quote)
select id, 'order',
       coalesce((
           select max(nullif(regexp_replace(number, '[^0-9]', '', 'g'), '')::bigint) + 1
             from sales_orders o
            where o.organization_id = organizations.id
       ), 1)
  from organizations
on conflict (organization_id, kind) do nothing;

-- An organization created after this migration gets its counters with it, so the first quote it
-- ever writes does not have to create its own sequence row (and cannot forget to).
create or replace function sales_number_sequences_for_new_organization() returns trigger
language plpgsql as $$
begin
    insert into sales_number_sequences (organization_id, kind, next_quote)
    values (new.id, 'quote', 1), (new.id, 'order', 1)
    on conflict (organization_id, kind) do nothing;
    return new;
end;
$$;

create trigger sales_number_sequences_on_organization
    after insert on organizations
    for each row execute function sales_number_sequences_for_new_organization();
