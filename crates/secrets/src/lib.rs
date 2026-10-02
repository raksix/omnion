//! Omnion secrets.
//!
//! The platform surface for stored credentials (docs/requests/REQ-125, depth layer over
//! REQ-037). This crate is the *pure* half of it: the key hierarchy, the rotation ceremony's
//! decisions and the per-kind credential validators — no database, no HTTP, no decision that
//! belongs to a handler.
//!
//! # The key hierarchy
//!
//! An installation root key is never stored by the platform. The operator supplies a
//! **key-encryption key** through the environment (`OMNION_KEY_ENCRYPTION_KEY`) or an
//! operator-controlled key file, and the ring stores only:
//!
//! * the root key **wrapped** by that operator key,
//! * the `key_id` that sealed each stored version,
//! * a `seal_checksum` and a `fingerprint` so the UI can answer "is my operator key still the
//!   right one?" without ever handling the key material itself.
//!
//! The consequence that shapes every API here: unsealing reads the `key_id` recorded on the
//! version, **not** the currently active key. A version sealed under a retired key therefore
//! keeps resolving while a re-wrap job is still walking the ring — and keeps resolving after it,
//! until the job reaches it. That is the whole reason a rotation can be an online ceremony
//! instead of a maintenance window.
//!
//! Two primitives carry the cryptography, both already in the workspace: SHA-256 in counter mode
//! for confidentiality and HMAC-SHA256 over the header and ciphertext for integrity
//! (encrypt-then-MAC — the construction that fails closed, because a tampered envelope never
//! decrypts, it errors). Envelope shape: `v1.<nonce hex>.<ciphertext hex>.<tag hex>`.
//!
//! # Losing the operator key
//!
//! An installation that loses its key-encryption key cannot read its own secrets. Nothing here
//! can paper over that, so the crate states it plainly in [`KeyRing::self_check`]'s refusal and
//! in the rotation wizard's copy: the ceremony is only safe because the new key is wrapped
//! before the old one is retired.

#![forbid(unsafe_code)]

pub mod audit;
pub mod credentials;
pub mod error;
pub mod keyring;
pub mod leases;
pub mod redaction;
pub mod store;
pub mod validators;

pub use audit::{
    AnomalyRow, AuditFilter, AuditRow, DetectorSettings, Detectors, RevealCounts,
    RevealObservation, SiemRecord, acknowledge_anomaly, list_anomalies, list_audit, load_settings,
    record_reveal_anomalies, reveal_counts, siem_export, siem_record, tracked_actions,
};
pub use credentials::{
    CredentialRow, Resolution, SCOPES, SLOTS, SecretOwner, SlotRow, assign_slot, attach_profile,
    check_scope_and_slot, find_credential, find_secret_owner, find_slot, list_credentials,
    list_slots, parse_kind, record_validation, resolve_slot, sanitize_fields, slot_catalog,
};
pub use error::{Result, SecretsError};
pub use keyring::{
    KEY_ENCRYPTION_ENV, KEY_ENCRYPTION_FILE_ENV, KeyRing, KeyStatus, OperatorKey, RootKey,
    WrapOutcome, generate_key_id, wrap_key,
};
pub use redaction::{hint_for, mask_value, redact};
pub use store::{
    BatchReport, REWRAP_BATCH, RewrapJob, RootKeyRow, active_key_row, ensure_active_key,
    finish_job, list_root_keys, live_rewrap_job, load_ring, operator_key, pause_job,
    recover_missing_job, resume_job, rewrap_batch, ring_coverage, self_check, start_rotation,
};
pub use validators::{CredentialKind, ValidationOutcome, validate};
