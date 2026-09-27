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
//! Deciding on a request is one path: [`evaluate::authorize_subject`] resolves the role bindings
//! and then hands the answer to [`policies::apply`], where the organization's ABAC policies get
//! the last word (docs/07-IAM.md §11). The evaluation itself is `omnion-policy-engine`, which
//! stays pure and database-free; the guard, the effective-permissions screen and the simulator
//! all come through the same function, so no two callers can disagree.

#![forbid(unsafe_code)]

pub mod bindings;
pub mod catalogue;
pub mod error;
pub mod evaluate;
pub mod groups;
pub mod invariants;
pub mod matching;
pub mod model;
pub mod policies;
pub mod roles;
pub mod seed;
pub mod service_accounts;
pub mod simulate;
pub mod versions;

pub use catalogue::{CATALOGUE, PermissionDef, get as permission, is_known};
pub use error::{PermissionsError, Result};
pub use evaluate::{
    Decision, DenyReason, EffectivePermissions, Grant, PolicyStamp, RoleGraph, Trace, Via, authorize,
    authorize_subject, effective_permissions, effective_permissions_for, effective_permissions_in,
    load_role_graph,
};
pub use invariants::{InvariantRefusal, PRIVILEGED_ROLE_KEYS, check_binding_revocation};
pub use matching::glob_matches;
pub use policies::{PolicyDraft, PolicyRecord, PolicyVersionRecord};
pub use model::{
    Effect, MAX_INHERITANCE_DEPTH, MAX_PRIORITY, MIN_PRIORITY, NewBinding, NewRole,
    NewSubjectBinding, ParentChange, PermissionChange, PermissionSummary, ResourceContext, Role,
    RoleBinding, RoleDiff, RolePermission, RolePermissionInput, RoleSaveOutcome, RoleUpdate,
    RoleVersion, Scope, Subject, validate_priority, validate_role_key, validate_role_name,
};
pub use seed::SeedReport;
pub use simulate::{SimulationReport, SimulationStep, simulate};
