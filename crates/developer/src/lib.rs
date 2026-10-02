//! Omnion developer portal — the credentials an organization hands to code (REQ-022).
//!
//! ## What is new here, and why it is a separate crate
//!
//! Everything else in the platform authenticates a **person** or a **machine identity that has
//! roles bound to it** (`service_account_keys`, REQ-006). A developer API key is neither: it is
//! a *delegation* the operator configured, carrying its own scope list and no role at all. That
//! difference is invisible in a request log — both arrive as `Authorization: Bearer …` — and
//! fatal in the authorization check, so the two are kept in separate crates with separate
//! stores rather than as two branches of one table.
//!
//! ## The one rule this crate keeps
//!
//! **No secret is ever returned twice, and none is ever stored in a form that can be read
//! back.** A key's plaintext exists in exactly one place: the response that creates or rotates
//! it. Everything the platform keeps is [`keys::KeyPrefix`] (ten characters, the lookup
//! namespace) and a SHA-256 hash. That is not a convention — it is why
//! [`keys::ApiKey`] has no field that could hold a usable credential, so no handler, no
//! serializer, no `Debug` print and no CSV export can leak one even by accident.
//!
//! The corollary is that every read path is written to be **safe by construction**: the list
//! returns prefixes, the detail returns usage, and the authentication path compares hashes
//! with a constant-time equality rather than `==` on strings (see [`keys::verify_secret`]).
//!
//! ## Scope delegation is not role assignment
//!
//! A key's scopes are a **narrowing**, never a widening. [`keys::mintable_from`] refuses to
//! create a key whose scopes contain anything the creating caller did not hold, which is what
//! stops a read-only integration from minting an integration that can rotate keys. The rule is
//! checked against the catalogue rather than against the database: an unknown scope is a
//! refused key, because a scope the platform cannot resolve is a scope nothing can enforce.

#![forbid(unsafe_code)]

pub mod error;
pub mod keys;
pub mod keys_store;
pub mod logs;
pub mod logs_store;
pub mod model;

pub use error::{DeveloperError, Result};
pub use keys::{
    ApiKey, ENVIRONMENT_SANDBOX, ENVIRONMENT_LIVE, ENVIRONMENTS, KEY_NAMESPACE, KeyPrefix, KeyStatus,
    MAX_NAME_LENGTH, MAX_SCOPES, MIN_NAME_LENGTH, NewKey, Secret, dedupe_scopes, hash_token,
    is_live_environment, mintable_from, name_is_valid, parse_environment, scope_names_valid,
    verify_secret,
};
pub use logs::MAX_PAGE as MAX_LOG_PAGE;
pub use model::{IssuedKey, KeyView, UsagePoint};
pub use keys_store::{
    KeyPage, KeyQuery, authenticate, create, find, list, revoke, rotate, touch_last_used, usage,
};
pub use logs::{ClientIdentity, LogPage, LogQuery, LogRow, STATUS_CLASSES, class_of, path_without_query};
pub use logs_store::{record, search, window_days};
