//! Enterprise sign-in: OIDC, generic OAuth2 and SAML 2.0 (docs/07-IAM.md §11; REQ-006, slice
//! 4b-2).
//!
//! The four modules split the feature along the seams that actually differ:
//!
//! * [`providers`] — the configuration rows. No secrets: a client secret lives behind
//!   `secret_ref`, a name this crate never resolves (the API layer does, from the environment).
//! * [`challenges`] — the `state` every round trip is bound to, hashed at rest and single use.
//! * [`oidc`] — discovery, the token exchange, the RS256 verification and the registered-claim
//!   checks. This is the only place the identity crate reaches the network.
//! * [`claims`] — the protocol-neutral [`Identity`](claims::Identity) every flow reduces to, and
//!   the claim → role rules that read it.
//!
//! [`saml`] completes the trio for SAML 2.0, which has no token endpoint and no JWKS: its
//! evidence is a signed XML assertion, so it gets its own parser and its own signature path.

pub mod challenges;
pub mod claims;
pub mod oidc;
pub mod providers;
pub mod provisioning;
pub mod saml;

pub use challenges::{CHALLENGE_TTL_MINUTES, IssuedChallenge, SsoChallenge, hash_state};
pub use claims::{Identity, RoleMapping, identity_from_claims, resolve_roles};
pub use providers::{AuthProvider, NewProvider, ProviderChanges, ProviderKind};
pub use provisioning::{ProvisionOutcome, Provisioned, provision as provision_account};
