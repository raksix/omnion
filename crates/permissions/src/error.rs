//! Error type of the permissions store.
//!
//! It never decides HTTP status codes — the API layer maps it onto the HTTP surface.

/// Errors returned by the permissions store.
#[derive(Debug, thiserror::Error)]
pub enum PermissionsError {
    /// A database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The role key is not a usable slug.
    #[error("invalid role key: {0}")]
    InvalidRoleKey(String),
    /// The role name is empty or too long.
    #[error("invalid role name: {0}")]
    InvalidRoleName(String),
    /// Priorities run from 0 (lowest) to 1000 (highest, the Owner role).
    #[error("priority must be between {min} and {max}")]
    InvalidPriority {
        /// Lowest accepted priority.
        min: i32,
        /// Highest accepted priority.
        max: i32,
    },
    /// A permission key outside the catalogue was used.
    #[error("unknown permission: {0}")]
    UnknownPermission(String),
    /// No role carries this id.
    #[error("role not found")]
    RoleNotFound,
    /// A role with this key already exists in the same scope.
    #[error("a role with this key already exists")]
    RoleKeyTaken,
    /// A role cannot inherit itself.
    #[error("a role cannot inherit itself")]
    SelfInheritance,
    /// Inheritance chains must stay acyclic.
    #[error("role inheritance would create a cycle")]
    InheritanceCycle,
    /// Inheritance chains may not grow beyond the documented depth.
    #[error("role inheritance would exceed {max} levels")]
    InheritanceDepthExceeded {
        /// Longest accepted chain.
        max: usize,
    },
    /// A role that still carries live bindings cannot be removed.
    #[error("the role still carries {0} live binding(s)")]
    RoleHasBindings(i64),
    /// The role changed since the caller read it (the matrix save compares versions).
    #[error("role version conflict: expected {expected}, current {current}")]
    VersionConflict {
        /// Version the caller worked from.
        expected: i32,
        /// Version the role carries now.
        current: i32,
    },
    /// A permission set carries keys the catalogue does not know, duplicates, or both.
    #[error("invalid permission entries: unknown {unknown:?}, duplicates {duplicates:?}")]
    InvalidEntries {
        /// Keys outside the catalogue.
        unknown: Vec<String>,
        /// Keys repeated inside one set.
        duplicates: Vec<String>,
    },
    /// A role can only inherit from a role of its own organization or a platform role.
    #[error("inherited role belongs to another organization")]
    CrossOrganizationInheritance,
    /// Platform (system) roles are managed by the platform, not by customers.
    #[error("system roles cannot be edited")]
    SystemRole,
    /// The binding scope does not match its columns.
    #[error("invalid scope: {0}")]
    InvalidScope(String),
    /// The binding contradicts the role or the account it references.
    #[error("invalid binding: {0}")]
    InvalidBinding(String),
    /// The account already holds this role at this scope.
    #[error("the role is already assigned at this scope")]
    AlreadyBound,
}

/// Result alias used across the permissions crate.
pub type Result<T, E = PermissionsError> = std::result::Result<T, E>;
