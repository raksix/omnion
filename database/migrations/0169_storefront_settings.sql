-- 0169_storefront_settings.sql — the one table the storefront owns outright.
--
-- REQ-118, slice 1a. Twelve of this request's nineteen acceptance lines end in a number a
-- customer sees, and every one of those numbers is read from a per-site setting that lives
-- here: the page size of a listing, whether a stranger may check out without an account, how
-- a tax-inclusive price is worded, the cap a quantity stepper will not go past, the stock
-- number under which a card grows a "Low" badge, and how long a cart may sit before support
-- is allowed to call it abandoned.
--
-- **This table has no foreign key into the commerce engine and that is the point.** The rest
-- of the storefront — catalogue, cart, checkout, orders — is defined by REQ-118 as a *surface
-- over* REQ-008's `commerce_products`, `commerce_variants`, `commerce_orders`,
-- `commerce_shipping_methods`, `commerce_payment_providers` and `commerce_customers`, and the
-- request is explicit that it never reimplements any of them. REQ-008 is unbuilt, so those
-- tables do not exist on any branch. A cart migration that declared those keys would apply
-- cleanly against a fresh database and then be un-runnable forever, because the tables it
-- points at would arrive with a different shape and a different owner, and the rewrite would
-- have to happen under a released version number.
--
-- So this is the one piece of REQ-118 that is *hers alone*: eleven columns, no dependency on
-- a module that does not exist yet, and the constraint checks that make the panel's own
-- bounds real in the database rather than in a `<input max>`.
--
-- Style: the `0009` house style — a purpose header, a comment on every constraint that states
-- the failure it exists to stop, and no secret material.

create table storefront_settings (
    id                          uuid        primary key default gen_random_uuid(),
    site_id                     uuid        not null references sites (id) on delete cascade,

    -- Checkout.
    guest_checkout              boolean     not null default true,

    -- Tax presentation. The stored amounts never change with this flag: it decides the
    -- *wording and the arithmetic shown*, never the money recorded (REQ-118 §Risks, "Tax
    -- display is presentation"). A value the crate accepts and the database refuses reads
    -- as "nothing happened", so the list is a check constraint rather than a free text column.
    tax_display                 text        not null default 'inclusive'
                                constraint storefront_settings_tax_display_check
                                check (tax_display in ('inclusive', 'exclusive')),

    -- Catalogue listing. `page_size` is bounded because it is multiplied by a page index: a
    -- page size of zero is an empty page forever, and a page size of ten thousand is a way to
    -- turn a public catalogue into a memory exhaustion. 4..=96 is the band the theme's own
    -- declared variants are built for.
    listing_variant             text        not null default 'grid',
    page_size                   integer     not null default 24
                                constraint storefront_settings_page_size_check
                                check (page_size between 4 and 96),
    pagination                  text        not null default 'pagination'
                                constraint storefront_settings_pagination_check
                                check (pagination in ('pagination', 'load_more', 'infinite')),

    -- Quantity ceiling for one line. Capped at the same 100 the cart items table allows, so
    -- a setting cannot permit a quantity the row refuses to store.
    per_order_item_max          integer     not null default 20
                                constraint storefront_settings_per_order_item_max_check
                                check (per_order_item_max between 1 and 100),

    -- Wishlist and the low-stock badge. A threshold of zero would badge every product "Low",
    -- which is the same as having no badge at all — so it is bounded from below at one.
    wishlist_enabled            boolean     not null default true,
    low_stock_badge_threshold    integer     not null default 5
                                constraint storefront_settings_low_stock_threshold_check
                                check (low_stock_badge_threshold between 1 and 100),

    -- How long an active cart with items may sit before the sweep may call it abandoned. The
    -- abandoned-cart view and REQ-060's campaign both key off this number, and a value of
    -- zero would make every cart abandoned the moment it is created — the campaign would then
    -- e-mail people about a basket they are still filling. 1..=720 hours.
    abandonment_hours           integer     not null default 24
                                constraint storefront_settings_abandonment_hours_check
                                check (abandonment_hours between 1 and 720),

    -- Reference to the mail template used for the order confirmation. A *reference*, not the
    -- template body: templates belong to the notification centre (REQ-021), and duplicating
    -- them here is how two e-mails of the same order start to disagree.
    confirmation_template       text        not null default 'order_confirmation',

    currency                    char(3)     not null default 'EUR'
                                constraint storefront_settings_currency_check
                                check (currency ~ '^[A-Z]{3}$'),

    updated_by                  uuid        references users (id) on delete set null,
    created_at                  timestamptz not null default now(),
    updated_at                  timestamptz not null default now(),

    -- One settings row per site. This is the constraint the whole request rests on: the
    -- storefront reads settings by `site_id` alone, so a second row would make "the page
    -- size" ambiguous and the answer would depend on which row the query happened to return.
    constraint storefront_settings_site_key unique (site_id)
);

-- The settings screen lists a site's storefront, and the abandoned-cart sweep walks every
-- organization with an active cart. Both are (organization, recency) reads through the site.
create index storefront_settings_site_idx
    on storefront_settings (site_id);

comment on table storefront_settings is
    'Per-site storefront configuration for the customer-facing surface (REQ-118). Holds no '
    'commerce foreign keys: catalogue, cart, checkout and orders are a surface over the '
    'commerce engine (REQ-008) and belong with it.';

comment on column storefront_settings.tax_display is
    'Presentation only. The stored amounts and the invoice are identical either way; the flag '
    'chooses whether a price is shown as "incl. VAT" or "excl. VAT" with the tax added.';

comment on column storefront_settings.abandonment_hours is
    'How long a cart with items may sit before the sweep may mark it abandoned and emit '
    'storefront.cart.abandoned once. Bounded from below so a cart is never abandoned at birth.';

-- Seed one row per existing site. A migration that adds a per-site table and seeds nothing
-- leaves every existing installation with a storefront that reads no settings and silently
-- serves platform defaults that no operator chose — and "my page size is 24 and I never set
-- it" is a support ticket nobody can close. `on conflict do nothing` keeps this idempotent.
insert into storefront_settings (site_id)
select id from sites
on conflict (site_id) do nothing;
