//! What the API hands the panel, and what it refuses to.
//!
//! The REQ's own list of "never appears anywhere" is the reason this module exists as a set of
//! *view* types rather than the route serialising [`crate::keys::ApiKey`] directly. The row type
//! holds `key_hash` — authentication needs it — and a `Serialize` derive on that type would put
//! it one refactor away from a response body. These views have no such field, so the guarantee
//! holds without anybody remembering it.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::keys::{ApiKey, KeyStatus};

/// One row of the key list, and the detail header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyView {
    /// Primary key.
    pub id: Uuid,
    /// Operator-facing name.
    pub name: String,
    /// `live` or `sandbox`.
    pub environment: String,
    /// The displayable lookup namespace. Never the token.
    pub key_prefix: String,
    /// The delegation, sorted and deduped.
    pub scopes: Vec<String>,
    /// Who issued it, by name. Empty string when the issuer was deleted.
    pub created_by_name: String,
    /// Creation timestamp.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last authentication.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    /// When it stops authenticating.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// The key this one replaced.
    pub rotated_from: Option<Uuid>,
    /// One word for the badge.
    pub status: KeyStatus,
}

impl From<ApiKey> for KeyView {
    fn from(key: ApiKey) -> Self {
        let status = key.status_at(time::OffsetDateTime::now_utc());
        Self {
            id: key.id,
            name: key.name,
            environment: key.environment,
            key_prefix: key.key_prefix,
            scopes: key.scopes,
            created_by_name: key.created_by_name,
            created_at: key.created_at,
            last_used_at: key.last_used_at,
            expires_at: key.expires_at,
            revoked_at: key.revoked_at,
            rotated_from: key.rotated_from,
            status,
        }
    }
}

/// A create/rotate response. **This is the only shape in the crate that carries a token**, and
/// it exists so that "reveal once" is a property of a type rather than of a route that
/// remembered to include it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedKey {
    /// The row as the list shows it, from this moment on.
    pub key: KeyView,
    /// The full token. Shown once, never stored, never returned again.
    pub token: String,
}

/// Per-day counters for the usage chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePoint {
    /// The day.
    ///
    /// **Serialised as a string by the route that publishes it**, not by an attribute here: the
    /// workspace enables `serde-well-known` but not `serde-human-readable`, so a bare
    /// `time::Date` would cross the wire as a three-element array — and this field is the React
    /// `key` of every bar in the usage chart, so an array there is not a cosmetic fault. `time`
    /// exposes no `Date` codec under the features this crate enables, so the route formats it.
    pub day: time::Date,
    /// Requests that day.
    pub requests: i32,
    /// Refusals (4xx/5xx) that day.
    pub errors: i32,
    /// Mean duration that day.
    pub avg_duration_ms: i32,
}
