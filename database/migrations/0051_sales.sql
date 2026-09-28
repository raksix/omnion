-- Omnion · 0051 · Sales: sellable catalog, price lists and quote documents
-- (docs/requests/REQ-052, slice 1 — catalog + price lists)
--
-- The selling side of the platform (docs/08-BUSINESS-SUITE.md): what a business offers, at what
-- price to whom, and the quote that binds the two. Every table is tenant-leading and carries
-- `organization_id` first in its index, because the API resolves the caller's organization first
-- and never scans by id alone.
--
-- Facts this schema encodes, so the module and the screens cannot disagree:
--
-- * **Money is `numeric`, never floating point.** Every amount is `numeric(14,2)` and every
--   quantity is `numeric(14,3)`. A binary float cannot represent 0.1, so a quote whose total is
--   computed in Rust and one computed in SQL would disagree in the last cent — and the printed
--   PDF, the panel and the future invoice all have to agree to the cent (the module's own
--   rounding note). Storing the computed total as `numeric` is what makes that a guarantee rather
--   than a convention.
-- * **A sent quote is immutable; editing it writes a new version.** `sales_quote_versions`
--   holds a full snapshot of the lines and the totals as they were at the moment of sending, so
--   the document the customer read is still readable after the catalogue moves on. A quote's
--   `status = 'sent'` is therefore a promise, and `sent_at` is when the promise was made.
-- * **The tax rate on a line is a snapshot.** `tax_percent` is copied onto the line and there is
--   deliberately **no** foreign key to an accounting tax rate: REQ-054 owns rate *definitions*,
--   and a historical document must not change because somebody edited a rate today. The optional
--   `tax_rate_id` records which rate it came from without depending on it.
-- * **A public link is a credential, so only its hash is stored.** `public_token_hash` holds a
--   SHA-256 of the token; the token itself is shown once and never again. Rotating it (on send,
--   or on demand) replaces the hash, which invalidates the previous link — the same shape as
--   REQ-010's share links.
-- * **Nothing is deleted.** `archived_at` is the only removal, for the same reason CRM uses it: a
--   product is referenced by quote lines and price rows that must keep reading as they were.

-- --------------------------------------------------------------------------------------------
-- Products
-- --------------------------------------------------------------------------------------------

create table sales_products (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    sku               text           not null,
    name              text           not null,
    description       text           not null default '',
    category          text,
    unit              text           not null default 'piece',
    tax_percent       numeric(5,2)   not null default 0,
    default_price     numeric(14,2)  not null default 0,
    currency          char(3)        not null default 'TRY',
    active            boolean        not null default true,
    archived_at       timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint sales_products_sku_format check (sku ~ '^[A-Za-z0-9._-]{2,32}$'),
    constraint sales_products_name_not_blank check (length(btrim(name)) > 0),
    constraint sales_products_name_length check (length(name) <= 160),
    constraint sales_products_description_length check (length(description) <= 4000),
    constraint sales_products_unit_not_blank check (length(btrim(unit)) > 0 and length(unit) <= 24),
    constraint sales_products_tax_percent_range check (tax_percent >= 0 and tax_percent <= 100),
    constraint sales_products_default_price_positive check (default_price >= 0),
    constraint sales_products_currency_format check (currency ~ '^[A-Z]{3}$')
);

-- A SKU is one product per organization, case-insensitively, among the live rows. An archived
-- duplicate does not block a fresh product — same rule as a CRM e-mail.
create unique index sales_products_sku_per_organization
    on sales_products (organization_id, lower(sku))
    where archived_at is null;

create index sales_products_list
    on sales_products (organization_id, archived_at, active, name);

create index sales_products_category
    on sales_products (organization_id, category)
    where archived_at is null;

-- --------------------------------------------------------------------------------------------
-- Price lists
-- --------------------------------------------------------------------------------------------

create table sales_price_lists (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    name              text           not null,
    currency          char(3)        not null default 'TRY',
    active            boolean        not null default true,
    valid_from        date,
    valid_until       date,
    archived_at       timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint sales_price_lists_name_not_blank check (length(btrim(name)) > 0),
    constraint sales_price_lists_name_length check (length(name) <= 120),
    constraint sales_price_lists_currency_format check (currency ~ '^[A-Z]{3}$'),
    -- A window that ends before it starts is a list nobody can buy from; the forms refuse it and
    -- the constraint refuses it for a caller that does not use them.
    constraint sales_price_lists_window_ordered check (
        valid_from is null or valid_until is null or valid_until >= valid_from
    )
);

create unique index sales_price_lists_name_per_organization
    on sales_price_lists (organization_id, lower(name))
    where archived_at is null;

create index sales_price_lists_list
    on sales_price_lists (organization_id, archived_at, active, name);

-- --------------------------------------------------------------------------------------------
-- Price rows
-- --------------------------------------------------------------------------------------------

-- One row per (list, product). `min_quantity` exists so a list can say "from 100 units the price
-- is lower"; the module picks the row with the highest `min_quantity` that the ordered quantity
-- satisfies, which is why it is part of the unique key rather than a column beside it.
create table sales_price_list_items (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    price_list_id     uuid           not null references sales_price_lists (id) on delete cascade,
    product_id        uuid           not null references sales_products (id) on delete cascade,
    min_quantity      numeric(14,3)  not null default 1,
    price             numeric(14,2)  not null,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint sales_price_list_items_min_quantity_positive check (min_quantity > 0),
    constraint sales_price_list_items_price_positive check (price >= 0)
);

create unique index sales_price_list_items_one_per_product
    on sales_price_list_items (price_list_id, product_id, min_quantity);

create index sales_price_list_items_lookup
    on sales_price_list_items (organization_id, price_list_id, product_id);

-- A price row may only point at a product of its own organization, and a list may only carry rows
-- of its own organization. Without these, a bug in a query that forgets the tenant predicate would
-- quietly sell one organization's catalogue to another's — the join is a pure convenience, so the
-- invariant is enforced where the rows are written.
create or replace function sales_price_item_tenant_matches() returns trigger
language plpgsql as $$
declare
    list_org   uuid;
    product_org uuid;
begin
    select organization_id into list_org from sales_price_lists where id = new.price_list_id;
    select organization_id into product_org from sales_products where id = new.product_id;
    if list_org is null or product_org is null then
        raise exception 'sales: price row points at a record that does not exist'
            using errcode = 'foreign_key_violation';
    end if;
    if list_org <> new.organization_id or product_org <> new.organization_id then
        raise exception 'sales: price row may not mix organizations'
            using errcode = 'check_violation';
    end if;
    return new;
end;
$$;

create trigger sales_price_list_items_tenant_guard
    before insert or update on sales_price_list_items
    for each row execute function sales_price_item_tenant_matches();

-- --------------------------------------------------------------------------------------------
-- Quotes
-- --------------------------------------------------------------------------------------------

create table sales_quotes (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    number            text           not null,
    -- The customer is a CRM record. The reference is deliberately not a foreign key constraint:
    -- REQ-051 is a separate module and a quote must still be readable if a contact is hard
    -- deleted by an operator's own script. The column is resolved at read time and a dangling
    -- reference renders as "customer removed", not as a broken quote screen.
    customer_type     text           not null default 'company',
    customer_id       uuid,
    customer_name     text           not null default '',
    title             text           not null default '',
    status            text           not null default 'draft',
    currency          char(3)        not null default 'TRY',
    price_list_id     uuid           references sales_price_lists (id) on delete set null,
    owner_user_id     uuid           references users (id) on delete set null,
    valid_until       date           not null,
    payment_terms     text           not null default '',
    reference         text           not null default '',
    notes             text           not null default '',

    -- Totals are stored, not derived at read time. They are recomputed by the module on every
    -- write and echoed back, because a client that computed its own total must not be able to
    -- present a number the server never agreed to.
    subtotal         numeric(14,2)   not null default 0,
    discount_total   numeric(14,2)   not null default 0,
    tax_total        numeric(14,2)   not null default 0,
    grand_total      numeric(14,2)   not null default 0,
    max_discount     numeric(5,2)   not null default 0,

    version           integer        not null default 1,
    public_token_hash text,
    public_token_expires_at timestamptz,
    sent_at          timestamptz,
    accepted_at      timestamptz,
    declined_at      timestamptz,
    decline_reason   text,
    cancelled_at     timestamptz,
    cancel_reason    text,
    archived_at      timestamptz,
    created_at       timestamptz    not null default now(),
    updated_at       timestamptz    not null default now(),

    constraint sales_quotes_status_check check (
        status in ('draft', 'pending_approval', 'approved', 'sent', 'accepted', 'declined', 'expired', 'cancelled')
    ),
    constraint sales_quotes_customer_kind_check check (customer_type in ('company', 'contact')),
    constraint sales_quotes_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint sales_quotes_totals_non_negative check (
        subtotal >= 0 and discount_total >= 0 and tax_total >= 0 and grand_total >= 0
    ),
    constraint sales_quotes_max_discount_range check (max_discount >= 0 and max_discount <= 100),
    constraint sales_quotes_version_positive check (version >= 1),
    -- A decline that does not say why is a refusal a person cannot act on, and an acceptance with
    -- a decline reason is a contradiction. Both are refused at the row, not in a form.
    constraint sales_quotes_decline_consistent check (
        (status = 'declined') = (decline_reason is not null and btrim(decline_reason) <> '')
    ),
    -- A sent quote is the moment the customer was given a document, so it must say when — and a
    -- draft that claims to have been sent is a link that resolves to a promise nobody made.
    constraint sales_quotes_sent_consistent check ((status = 'draft') = (sent_at is null))
);

create unique index sales_quotes_number_per_organization
    on sales_quotes (organization_id, number)
    where archived_at is null;

create unique index sales_quotes_public_token
    on sales_quotes (public_token_hash)
    where public_token_hash is not null;

create index sales_quotes_list
    on sales_quotes (organization_id, archived_at, status, updated_at desc);

create index sales_quotes_customer
    on sales_quotes (organization_id, customer_id)
    where archived_at is null;

-- --------------------------------------------------------------------------------------------
-- Quote lines
-- --------------------------------------------------------------------------------------------

create table sales_quote_lines (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    quote_id          uuid           not null references sales_quotes (id) on delete cascade,
    position          integer        not null,
    product_id        uuid           references sales_products (id) on delete set null,
    description       text           not null default '',
    unit              text           not null default 'piece',
    quantity          numeric(14,3)  not null default 1,
    unit_price        numeric(14,2)  not null default 0,
    discount_percent  numeric(5,2)   not null default 0,
    -- Snapshot of the rate that applied when the line was written (see the file header).
    tax_percent       numeric(5,2)   not null default 0,
    tax_rate_id       uuid,
    line_total        numeric(14,2)  not null default 0,

    constraint sales_quote_lines_position_positive check (position >= 1),
    constraint sales_quote_lines_quantity_positive check (quantity > 0),
    constraint sales_quote_lines_unit_price_positive check (unit_price >= 0),
    constraint sales_quote_lines_discount_range check (discount_percent >= 0 and discount_percent <= 100),
    constraint sales_quote_lines_tax_range check (tax_percent >= 0 and tax_percent <= 100),
    constraint sales_quote_lines_total_non_negative check (line_total >= 0),
    -- A line with neither a product nor a description is a blank row the builder would let a
    -- person save, and an empty line on a quote the customer reads is worse than no quote.
    constraint sales_quote_lines_has_content check (product_id is not null or length(btrim(description)) > 0)
);

create unique index sales_quote_lines_position
    on sales_quote_lines (quote_id, position);

create index sales_quote_lines_by_quote
    on sales_quote_lines (organization_id, quote_id, position);

-- --------------------------------------------------------------------------------------------
-- Version snapshots
-- --------------------------------------------------------------------------------------------

-- What the customer read. Written when a quote is sent, and never updated afterwards: this is the
-- immutability the spec's "a PATCH on a sent quote returns 409" rule is protecting.
create table sales_quote_versions (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    quote_id          uuid           not null references sales_quotes (id) on delete cascade,
    version           integer        not null,
    currency          char(3)        not null,
    lines             jsonb          not null default '[]'::jsonb,
    subtotal          numeric(14,2)  not null,
    discount_total    numeric(14,2)  not null,
    tax_total         numeric(14,2)  not null,
    grand_total       numeric(14,2)  not null,
    max_discount      numeric(5,2)   not null default 0,
    sent_at           timestamptz    not null default now(),
    created_at        timestamptz    not null default now(),

    constraint sales_quote_versions_version_positive check (version >= 1)
);

create unique index sales_quote_versions_one_per_version
    on sales_quote_versions (quote_id, version);

-- --------------------------------------------------------------------------------------------
-- Orders
-- --------------------------------------------------------------------------------------------

create table sales_orders (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    number            text           not null,
    quote_id          uuid           references sales_quotes (id) on delete set null,
    customer_type     text           not null default 'company',
    customer_id       uuid,
    customer_name     text           not null default '',
    status            text           not null default 'draft',
    currency          char(3)        not null default 'TRY',
    owner_user_id     uuid           references users (id) on delete set null,
    subtotal          numeric(14,2)  not null default 0,
    discount_total    numeric(14,2)  not null default 0,
    tax_total         numeric(14,2)  not null default 0,
    grand_total       numeric(14,2)  not null default 0,
    -- 'none' until REQ-053 (inventory) is installed; the spec asks for a visible note rather than
    -- a silent failure, so the state is explicit and the screen reads it.
    reservation_state text           not null default 'none',
    invoice_state     text           not null default 'none',
    confirmed_at      timestamptz,
    cancelled_at      timestamptz,
    archived_at       timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint sales_orders_status_check check (
        status in ('draft', 'confirmed', 'invoiced', 'delivered', 'cancelled')
    ),
    constraint sales_orders_customer_kind_check check (customer_type in ('company', 'contact')),
    constraint sales_orders_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint sales_orders_reservation_state_check check (
        reservation_state in ('none', 'partial', 'total', 'released')
    ),
    constraint sales_orders_invoice_state_check check (
        invoice_state in ('none', 'draft', 'issued')
    ),
    constraint sales_orders_totals_non_negative check (
        subtotal >= 0 and discount_total >= 0 and tax_total >= 0 and grand_total >= 0
    )
);

create unique index sales_orders_number_per_organization
    on sales_orders (organization_id, number)
    where archived_at is null;

create index sales_orders_list
    on sales_orders (organization_id, archived_at, status, updated_at desc);

-- Order lines are a copy of the quote's lines at the moment of confirmation, for the same reason
-- the version snapshot exists: the order is a document in its own right.
create table sales_order_lines (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    order_id          uuid           not null references sales_orders (id) on delete cascade,
    position          integer        not null,
    product_id        uuid           references sales_products (id) on delete set null,
    description       text           not null default '',
    unit              text           not null default 'piece',
    quantity          numeric(14,3)  not null default 1,
    unit_price        numeric(14,2)  not null default 0,
    discount_percent  numeric(5,2)   not null default 0,
    tax_percent       numeric(5,2)   not null default 0,
    line_total        numeric(14,2)  not null default 0,

    constraint sales_order_lines_position_positive check (position >= 1),
    constraint sales_order_lines_quantity_positive check (quantity > 0),
    constraint sales_order_lines_has_content check (product_id is not null or length(btrim(description)) > 0)
);

create unique index sales_order_lines_position
    on sales_order_lines (order_id, position);

-- --------------------------------------------------------------------------------------------
-- Status history — the order's own audit trail, kept beside the order rather than in the audit
-- table so the detail screen can render it without a permission it does not hold.
-- --------------------------------------------------------------------------------------------

create table sales_status_history (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    order_id          uuid           not null references sales_orders (id) on delete cascade,
    from_status       text,
    to_status         text           not null,
    note              text           not null default '',
    actor_user_id     uuid           references users (id) on delete set null,
    created_at        timestamptz    not null default now()
);

create index sales_status_history_by_order
    on sales_status_history (organization_id, order_id, created_at);

-- --------------------------------------------------------------------------------------------
-- Settings — one row per organization, created with the first quote.
-- --------------------------------------------------------------------------------------------

create table sales_settings (
    organization_id          uuid        primary key references organizations (id) on delete cascade,
    currency                 char(3)     not null default 'TRY',
    -- The spec's default approval threshold. A quote whose largest line discount exceeds it needs
    -- a manager; the module compares `max_discount` against this value, so changing the threshold
    -- never rewrites a quote that was already decided.
    discount_approval_threshold numeric(5,2) not null default 15,
    quote_validity_days      integer     not null default 30,
    quote_number_prefix      text        not null default 'Q',
    order_number_prefix      text        not null default 'SO',
    updated_at               timestamptz not null default now(),

    constraint sales_settings_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint sales_settings_threshold_range check (
        discount_approval_threshold >= 0 and discount_approval_threshold <= 100
    ),
    constraint sales_settings_validity_positive check (quote_validity_days between 1 and 365),
    constraint sales_settings_prefix_not_blank check (
        length(btrim(quote_number_prefix)) > 0 and length(btrim(order_number_prefix)) > 0
    )
);

-- Every organization gets its settings row the moment it exists, so the quote builder can read a
-- currency without a lookup that can miss. The same pattern CRM uses for the default pipeline:
-- seeded for the organizations that exist, and called by the trigger for the ones that come later.
insert into sales_settings (organization_id)
select id from organizations
on conflict (organization_id) do nothing;

create or replace function sales_settings_for_new_organization() returns trigger
language plpgsql as $$
begin
    insert into sales_settings (organization_id) values (new.id)
    on conflict (organization_id) do nothing;
    return new;
end;
$$;

create trigger sales_settings_on_organization
    after insert on organizations
    for each row execute function sales_settings_for_new_organization();
