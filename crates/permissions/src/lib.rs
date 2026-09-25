//! Omnion permissions.
//!
//! The access-control store of the platform (docs/07-IAM.md): a granular permission catalogue,
//! fully custom roles with a priority value and inheritance, explicit allow/deny entries and
//! role bindings that attach a role to an account at a scope.
//!
//! Two layers, kept apart on purpose:
//!
//! * [`evaluate::RoleGraph`] — pure, deterministic resolution of "which permissions does this
//!   principal hold, and which role says so". Everything about precedence is here, and it is
//!   unit-tested without a database.
//! * [`roles`], [`bindings`], [`seed`] — the persistence side, which only stores and loads.
//!
//! Deciding on a request is RBAC only: the ABAC policy engine (docs/07-IAM.md §11) plugs in
//! later as `crates/policy-engine`, consuming the same [`evaluate::EffectivePermissions`].

#![forbid(unsafe_code)]

pub mod bindings;
pub mod catalogue;
pub mod error;
pub mod evaluate;
pub mod model;
pub mod roles;
pub mod seed;

pub use catalogue::{CATALOGUE, PermissionDef, get as permission, is_known};
pub use error::{PermissionsError, Result};
pub use evaluate::{
    Decision, DenyReason, EffectivePermissions, Grant, RoleGraph, Via, authorize,
    effective_permissions,
};
pub use model::{
    Effect, MAX_PRIORITY, MIN_PRIORITY, NewBinding, NewRole, PermissionSummary, Role, RoleBinding,
    RolePermission, RolePermissionInput, Scope,
};
pub use seed::SeedReport;
