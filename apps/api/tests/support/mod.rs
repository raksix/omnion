//! Shared fixtures for the API integration walks.
//!
//! Today this is the identity provider the enterprise sign-in walk runs against, and `walk_state`
//! — the one place a walk is allowed to refuse to run. It is a module and not an inline block so
//! the next walk that needs a directory, a mail catcher or a queue can share the same place
//! rather than growing its own copy.
pub mod stub_idp;
pub mod walk_state;
