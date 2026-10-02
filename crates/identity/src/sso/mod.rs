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
//!
//! [`directory`] is the odd one out. LDAP and Active Directory do not hand you an identity, they
//! answer queries over a connection you operate, so what lives there is a *configuration*
//! language (what a usable directory configuration is, and every way it can be wrong, each
//! attached to the field that owns it) and a connection test that is a **ladder of steps** rather
//! than a boolean. REQ-065.
//!
//! Two more modules make that ladder real rather than decidable. [`ber`] is the wire codec — the
//! narrow, bounded dialect RFC 4511 defines — and [`connection`] is the live half: resolve, dial,
//! negotiate TLS, bind, page a search, walk the group graph. The split is deliberate: the
//! configuration questions stay unit tests that need no directory, and the questions that are
//! about bytes (does a lying length become a partial attribute? does a cycle terminate?) are
//! tests over captured frames rather than integration tests against somebody else's CI.
//!
//! [`attributes`] is shared by all five kinds: whatever hands us the claims, somebody has to say
//! which of them is the email. [`mappings`] stores that answer.

pub mod attributes;
pub mod ber;
pub mod challenges;
pub mod claims;
pub mod connection;
pub mod directory;
pub mod group_context;
pub mod mappings;
pub mod oidc;
pub mod protocol_steps;
pub mod providers;
pub mod provisioning;
pub mod role_rule_store;
pub mod role_rules;
pub mod saml;
pub mod scim_runs;
pub mod sync_runs;

pub use attributes::{
    AttributeMap, AttributeMapping, MapProblem, Projection, TargetField, Transform,
};
pub use challenges::{CHALLENGE_TTL_MINUTES, IssuedChallenge, SsoChallenge, hash_state};
pub use claims::{Identity, RoleMapping, identity_from_claims, resolve_roles};
pub use connection::{BindFailure, ConnectionReport, DirectoryConnection, GroupWalk, TransportError};
pub use directory::{
    ConfigProblem, DirectoryConfig, DirectoryKind, Problem, StepReport, TestOutcome, TestStep,
    test_steps,
};
pub use providers::{AuthProvider, NewProvider, ProviderChanges, ProviderKind};
pub use provisioning::{ProvisionOutcome, Provisioned, provision as provision_account};
