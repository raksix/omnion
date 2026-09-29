//! Shared fixtures for the API integration walks.
//!
//! `stub_idp` is the identity provider the enterprise sign-in walk runs against; `walk_auth` is
//! the sign-in half every cookie-authenticated walk needs; `walk_state` is the one place a walk
//! is allowed to refuse to run. All three live here rather than as inline blocks so the next walk
//! that needs a directory, a mail catcher or a queue can share the same place — and so a security
//! change is made in one file instead of twenty.
pub mod stub_idp;
pub mod walk_auth;
pub mod walk_state;
