//! Errors of the migration-safety layer.

use uuid::Uuid;

/// Anything that can go wrong while applying, reversing or auditing a migration.
#[derive(Debug, thiserror::Error)]
pub enum MigrationSafetyError {
    /// The database refused or could not run a query.
    #[error("migration store: {0}")]
    Store(#[from] sqlx::Error),
    /// A migration file is not named `NNNN_name.sql`.
    #[error("migration file {version:?}: {reason}")]
    InvalidVersion {
        /// The version as it was read off the file.
        version: String,
        /// What is expected of it.
        reason: String,
    },
    /// A migration's checksum does not match what the ledger recorded, or the file is gone.
    ///
    /// The message is [`crate::ledger::Drift::message`]'s, because this is the case where the
    /// wording IS the product: an operator reading it has to know the fix is a new migration and
    /// not an edit.
    #[error("{0}")]
    Drift(String),
    /// Another migration run holds the advisory lock.
    ///
    /// A `Conflict` and not an `Unavailable`: the caller is expected to retry or to render
    /// "a migration is running", and neither is a fault.
    #[error("{0}")]
    Locked(String),
    /// A migration without a down script, where the policy requires one and no waiver exists.
    #[error("{version} ({name}) has no down script: {reason}")]
    MissingDownScript {
        /// The version.
        version: String,
        /// The name after the underscore.
        name: String,
        /// The waiver text when one exists, or the policy's refusal when it does not.
        reason: String,
    },
    /// A banned shape the policy enforces and no waiver covers.
    #[error("{0}")]
    PolicyViolation(String),
    /// A migration the ledger has never heard of.
    #[error("no ledger row for migration {version}")]
    UnknownMigration {
        /// The version asked for.
        version: String,
    },
    /// Somebody already rehearsed this reversal.
    ///
    /// Refused rather than overwritten, and the reason is worth stating: the ledger's whole value
    /// is that `down_verified_at` means *this* reversal was run by *that* person. Re-verifying
    /// would be legitimate — but it is a second event with its own row in the journal, not an
    /// update of the first one.
    #[error("migration {version} was already marked as reversal-verified")]
    AlreadyVerified {
        /// The version asked for.
        version: String,
    },
    /// A row the caller addressed is not there.
    #[error("no {what} {id}")]
    NotFound {
        /// What was addressed (`violation`, `backfill job`, …).
        what: &'static str,
        /// Its identifier.
        id: String,
    },
}

impl MigrationSafetyError {
    /// A missing row, built from the two things a reader needs.
    pub fn not_found(what: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            what,
            id: id.to_string(),
        }
    }
}

/// Result alias of the migration crate.
pub type Result<T> = std::result::Result<T, MigrationSafetyError>;

/// Kept so the id type appears in this module's surface: a `uuid` in a store signature is an
/// account, and a reader of the error enum should not have to guess which kind.
const _: fn(Uuid) -> Uuid = |id| id;
