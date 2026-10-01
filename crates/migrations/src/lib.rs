//! Omnion migration safety (docs/requests/REQ-129-migration-safety.md).
//!
//! Schema changes that never lose a row. That is a promise with three parts, and each part is
//! something that can quietly be false:
//!
//! * **Nothing is lost.** Every change is additive — add nullable, backfill, constrain in a later
//!   migration — and the lint in [`lint`] refuses the destructive shapes so that is enforced
//!   rather than documented. The dangerous shapes are not the ones somebody chooses; they are the
//!   ones that are *convenient* at 2am.
//! * **You can tell what happened.** [`ledger`] is the operator-facing record of every migration
//!   this installation applied: the checksum of the file as applied, who ran it, how long it took
//!   and — the column that matters most — whether anybody has ever *rehearsed its reversal*.
//! * **You can go back.** [`down`] reads a migration's reversal out of the migration file, and a
//!   reversal that is prose rather than statements is reported as absent. That distinction is the
//!   difference between "the database can be rolled back" and "somebody wrote about rolling it
//!   back", and only the first one may be called `reversible`.
//!
//! ## The one rule that shapes this crate
//!
//! **A claim is not a verification, and this crate never records a claim as one.** A publisher's
//! `migrations_destructive: false`, a file with no `-- omnion:no-down` marker, a manifest that
//! lists no migrations — all three are *silence*, and silence is not evidence. So the only way
//! `down_verified_at` gets a value is [`ledger::mark_down_verified`], which exists to be called
//! after a reversal has actually run against a real database. `omnion_deployment` reads the
//! resulting column and renders `unknown` until it is set, and that is the correct answer rather
//! than a missing feature: `unknown` and `reversible` share nothing — the first means *take the
//! backup*, the second means *you may not need it*.
//!
//! ## Why a crate and not modules in the CLI
//!
//! The CLI, the deploy job, the CI gate and the admin panel all need the same answers, and each of
//! them runs in a process that does not link the others. A second checksum implementation is not
//! a refactor, it is a divergence — and the divergence here is the expensive kind, because it
//! shows up as a green build over a schema that is not what anybody believes it is.
//!
//! ## The rule every module obeys
//!
//! **The decision is pure and its inputs are visible in its answer.** `detect_drift` never reads a
//! database, `extract_down` never touches a file, the lint never counts. Clocks, pools and files
//! arrive as arguments. That is what lets `--dry-run` answer honestly without connecting to
//! anything, and it is why most of this crate is unit-tested with no database at all.

#![forbid(unsafe_code)]

pub mod down;
pub mod error;
pub mod ledger;
pub mod lint;
pub mod lock;
pub mod policy;
pub mod runner;

pub use down::DownScript;
pub use error::{MigrationSafetyError, Result};
pub use ledger::{Drift, LedgerRow, NewLedgerRow, checksum, detect_drift};
pub use lint::{PATTERNS, Pattern, Violation, gate_fails, lint};
pub use lock::{LockView, lock_id, lock_id_text};
pub use policy::Policy;
pub use runner::{
    created_objects, reversed_objects, ApplyReport, Direction, Plan, RunActor, VerifyReport,
    structure_restored, verify_down,
};
