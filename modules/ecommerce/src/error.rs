//! The storefront error type.
//!
//! One variant per *thing that went wrong at the boundary*, because a caller has to be able to
//! branch on them: the panel turns a validation error into a field message, a 404 becomes a
//! 404, and a database failure becomes a 500 that logs. Collapsing them into one string is how
//! "something went wrong" reaches a customer's checkout page.

use sqlx::Error as SqlxError;

use crate::settings::SettingError;

/// What can go wrong in the storefront module.
#[derive(Debug, thiserror::Error)]
pub enum EcommerceError {
    /// A field the platform knows is not acceptable. Carries the column name so the panel can
    /// put the message under the control that caused it.
    #[error(transparent)]
    Invalid(#[from] SettingError),

    /// The row does not exist, or belongs to another organization. **Never distinguished
    /// from the first in an answer**: a 403 on a foreign site id confirms that site exists,
    /// and "does an organization with this id exist" is not a question a public route
    /// answers.
    #[error("not found")]
    NotFound,

    /// A row the caller named is not in the organization the session belongs to.
    #[error("cross-organization access is refused")]
    CrossOrganization,

    /// The database refused. The cause is preserved rather than flattened, because a
    /// `check` violation here is a missing vocabulary entry and reads as nothing at all
    /// without it.
    #[error("database error: {0}")]
    Database(#[from] SqlxError),
}

/// The module's result type.
pub type Result<T> = std::result::Result<T, EcommerceError>;
