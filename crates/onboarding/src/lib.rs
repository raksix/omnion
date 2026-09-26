//! Omnion onboarding — the first run of an installation.
//!
//! A fresh Omnion has no accounts, no organization and no site, and the platform must be usable
//! without anyone touching SQL by hand (docs/requests/REQ-050). This crate owns that flow:
//!
//! ```text
//! owner account → organization → first site (+ domain) → theme → (optional) AI provider → done
//! ```
//!
//! Two front ends drive it and share every rule:
//!
//! * `apps/api` (`/api/v1/onboarding`) — the admin wizard;
//! * `tools/cli` (`omnion setup`) — a server without a browser.
//!
//! The state lives in the `onboarding_state` singleton (migration `0007`), every step is
//! audited, and the progress is *derived* from the platform's own rows, so the wizard resumes
//! after a refresh and an installation that was set up through the API directly still reports
//! what it has finished.

#![forbid(unsafe_code)]

pub mod checklist;
pub mod error;
pub mod state;
pub mod steps;
pub mod themes;

pub use checklist::ChecklistItem;
pub use error::{OnboardingError, Result};
pub use state::{OnboardingState, Status, Steps, Summary, open_steps};
pub use steps::{
    FirstOrganization, FirstOwner, FirstSite, choose_theme, complete, create_organization,
    create_owner, create_site, decide_ai, derive_slug, owner_account,
};
pub use themes::{BUNDLED_THEMES, BundledTheme};
