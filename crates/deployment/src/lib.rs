//! Omnion deployment tooling — the release cache, the artifacts, the bundles and the upgrade
//! plan (docs/requests/REQ-128).
//!
//! ## What this crate is
//!
//! It holds the **decision layer** of an upgrade: which steps run, in which order, and — the
//! part that matters most — what is *known* about whether the database can be rolled back. The
//! HTTP layer renders that decision; it does not make it. A screen that computed its own step
//! list would agree with itself on the day it was written and disagree with the operator's
//! reality the first time somebody changed a migration.
//!
//! ## The three verdicts, and why there are three
//!
//! `reversible`, `destructive` and `unknown`. The third is the one this crate exists to be
//! able to say, and collapsing it into the first is the failure the whole design is written
//! against:
//!
//! * **A publisher's `migrations_destructive: false` is a claim, not a verification.** It is
//!   evidence that a build asserted something; it is not evidence that a down script was ever
//!   executed against a real database. So a manifest saying `false` never produces
//!   `reversible` on its own — see [`manifest::destructiveness`].
//! * **A migration with no `-- omnion:no-down` marker has not been proven reversible.** It has
//!   been *not declared* irreversible, which is a different statement, and the difference is
//!   exactly the one an operator is making when they decide whether to take a backup.
//! * **So the answer is `unknown` until something proves it**, and the plan renders that as a
//!   warning rather than as a green tick. `unknown` and `reversible` share nothing: the first
//!   means *take the backup*, the second means *you may not need it*.
//!
//! The policy that turns `unknown` into one of the other two is REQ-129's up → down → up
//! migration gate. It has not landed, so this repository's own plans are `unknown` today, and
//! the crate reports that rather than softening it.
//!
//! ## The point of no return is a fact about the migration, not about the deploy
//!
//! An *application* rollback is always available: it is a tag change. A *database* rollback
//! is a restore, and a restore needs a backup taken **before** the first migration. So the
//! marker attaches to the first migration step whenever the verdict is not `reversible`, and to
//! the last one when it is — and a `reversible` upgrade has no marker at all, because marking a
//! reversible upgrade would teach operators to ignore the marker.

#![forbid(unsafe_code)]

pub mod bundle;
pub mod error;
pub mod manifest;
pub mod plan;
pub mod store;
pub mod upgrade;

pub use error::{DeploymentError, Result};
