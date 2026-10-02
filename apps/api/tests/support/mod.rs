//! Shared fixtures for the API integration walks.
//!
//! Today this is one module — the identity provider the enterprise sign-in walk runs against —
//! but it is a module and not an inline block so the next walk that needs a directory, a mail
//! catcher or a queue can share the same place rather than growing its own copy.
pub mod stub_idp;
