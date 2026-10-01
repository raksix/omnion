//! Shared fixtures for the API integration walks.
//!
//! `stub_idp` is the identity provider the enterprise sign-in walk runs against; `walk_auth` is
//! the sign-in half every cookie-authenticated walk needs; `isolated_db` gives a walk a
//! throwaway database of its own instead of the shared QA one every other writer is using.
//! All three live here rather than as inline blocks so the next walk that needs a directory, a
//! mail catcher or a queue can share the same place — and so a security change is made in one
//! file instead of twenty.
pub mod isolated_db;
pub mod stub_idp;
pub mod walk_auth;
