//! Omnion storefront — selling from the public site (docs/requests/REQ-118).
//!
//! A storefront is a *surface*, not an engine. Everything it shows a customer — a product, its
//! variants, its price, its stock, an order's totals — belongs to the commerce engine
//! (REQ-008), and REQ-118 is explicit that this request never reimplements any of it: one
//! totals function, one coupon validator, one signed payment webhook, three engines, not
//! three of each. What lives here is the part a shop owns that a catalogue does not: the
//! per-site configuration that decides how the surface behaves, and later the cart, the
//! checkout session and the visitor account layer.
//!
//! ## What is in this slice, and what is not, and why
//!
//! Slice 1a is [`settings`] — the eleven per-site knobs. It is the whole of the storefront
//! that has **no foreign key into the commerce engine**, and that is not a smaller ambition
//! than the request: acceptance line 16 is "admin storefront settings persist per site and
//! the client honours them — disabling guest checkout forces the sign-in step, switching tax
//! display changes the labels and amounts presentation, and raising the per-order maximum
//! changes the stepper cap", and every figure in every other customer-visible number on the
//! site is read from this row.
//!
//! The catalogue, the cart and the checkout are **not** built here yet, and the reason is a
//! dependency rather than a difficulty. Their tables declare keys into `commerce_products`,
//! `commerce_variants`, `commerce_orders`, `commerce_shipping_methods`,
//! `commerce_payment_providers` and `commerce_customers`. REQ-008 is unbuilt and ships on no
//! branch, so a cart migration written today would declare keys against tables that do not
//! exist — which fails to apply on a fresh database, and if it were made to apply by dropping
//! the keys, the cart would be a second commerce engine with its own idea of what a product
//! is. Both are worse than not shipping the cart. The build log for tick 28 carries the
//! evidence; the settings slice is deliberately the part that survives REQ-008 arriving.
//!
//! ## The rules the rest of the crate is built around
//!
//! 1. **A setting is read on the write path, not only in the panel.** A number a customer
//!    sees — the page size, the tax wording, the quantity cap — is served from the row on
//!    every request that shows it. Caching it "for performance" is how a shop raises its
//!    per-order maximum and the stepper still refuses the new value for an hour.
//! 2. **Presentation is not money.** [`settings::StorefrontSettings::tax_display`] decides how
//!    a total is *worded and shown*. The stored amounts and the invoice are identical either
//!    way, because a display flag that changed a stored amount would be a second totals
//!    function (see docs/requests/REQ-118 §Risks, "One totals function").
//! 3. **The database is the last validator, and it is checked against.** The closed lists and
//!    the numeric bands are written twice — here and in the `check` constraints of
//!    `0169_storefront_settings.sql` — and [`vocabulary::tests_agree_with_the_migration`]
//!    reads the migration file. A value added to one and not the other reads as "nothing
//!    happened", every time.
//! 4. **A foreign organization's id is a 404, not a 403.** A 403 is an oracle for "that
//!    exists", and the storefront's settings row is keyed by site.

#![forbid(unsafe_code)]

pub mod error;
pub mod settings;
pub mod store;
pub mod vocabulary;

pub use error::{EcommerceError, Result};
pub use settings::{SettingError, StorefrontSettings};
