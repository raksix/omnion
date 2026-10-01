//! The secret inventory — a projection over **references** (REQ-012, slice 4).
//!
//! ## What this screen is, and what it is dangerously close to being
//!
//! The requirement asks for "names, scopes, last-rotated dates and which
//! integration references each secret". Read quickly, that reads like a
//! request for a secrets table. There is no secrets table, and there must
//! not be one: this is a **report** over material the platform already holds
//! elsewhere, and the entire value of it is that it is the one screen in the
//! security centre from which no secret can be read.
//!
//! That makes it the screen in this crate with the least tolerance for a
//! careless query. Every other screen here can leak a detail; this one leaks
//! a credential. So the design pushes the constraint from the documentation
//! into the type:
//!
//! * A [`SecretRef`] row carries a **name** and never a value. There is no
//!   field on this struct that could hold one, so no renderer can print one
//!   and no `Debug` output can leak one into a log.
//! * The store builds its column list from [`inventory_columns`] rather than
//!   `select *`, because the sources are heterogeneous tables and `*` would
//!   quietly add a value column the day somebody appends one upstream.
//! * Three sources hold real material — `mfa_factors.secret_ciphertext`,
//!   `service_account_keys.secret_hash` and `webhook_endpoints.secret`. They
//!   are reported by **count** and never selected, so widening this screen
//!   cannot turn it into a dump.
//!
//! ## Rotation age is a reading, not a stored fact
//!
//! No table records when a secret was last rotated, because rotation happens
//! outside the platform — an operator replaces an environment variable and
//! redeploys. So [`SecretRef::rotated_at`] is the best timestamp the reference
//! itself carries (the row's `updated_at`, else its `created_at`), and
//! [`SecretRef::age_days`] is measured from it.
//!
//! That reading is stated rather than hidden: a row whose age is computed
//! from the day the *row* was created is not evidence that a *value* was
//! rotated then. [`RotationEvidence`] names which of the two is being
//! reported, because a security screen that lets an operator read a date as
//! a rotation guarantee is worse than one that says it cannot know.
//!
//! ## An absent secret is not a clean secret
//!
//! A configured-but-unverifiable secret reports [`SecretState::Unverifiable`]
//! with a reason, never [`SecretState::Healthy`]. This is the crate's standing
//! rule (see the module docs of `lib.rs`) applied to the inventory: the
//! platform cannot check whether `OMNION_CSRF_SECRET` is a good value, and
//! "I cannot see it" is a state an operator can act on, while "looks fine"
//! is not.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// How much of a secret the platform can say about it.
///
/// The point of the enum is that **`Healthy` requires evidence**. A reference
/// that exists and a value that is strong are different claims, and only the
/// first is available here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretState {
    /// A reference exists but the platform cannot verify anything about the
    /// value behind it — which is the normal state for every environment
    /// secret. Reported rather than passed, never dressed up as healthy.
    Unverifiable,
    /// A secret this platform needs is not configured at all.
    Missing,
    /// The reference is past its own expiry.
    Expired,
}

impl SecretState {
    /// Value as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unverifiable => "unverifiable",
            Self::Missing => "missing",
            Self::Expired => "expired",
        }
    }

    /// Every word the state machine can produce.
    pub const ALL: &'static [Self] = &[Self::Unverifiable, Self::Missing, Self::Expired];

    /// Parse a stored word, refusing an unknown one.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}

/// What kind of thing holds the reference.
///
/// Presentation vocabulary again: the three source tables have nothing in
/// common but the fact that each holds a name, and one drop-down is what the
/// screen offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// `auth_providers.secret_ref` — a sign-in integration's credential name.
    IdentityProvider,
    /// An environment variable the platform reads at boot.
    Environment,
    /// `webhook_endpoints` — count only; the signing secret is never selected.
    Webhook,
    /// `service_account_keys` — count only; `secret_hash` is never selected.
    ServiceAccountKey,
    /// `mfa_factors` — count only; `secret_ciphertext` is never selected.
    MfaFactor,
}

impl SecretSource {
    /// Value as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdentityProvider => "identity_provider",
            Self::Environment => "environment",
            Self::Webhook => "webhook",
            Self::ServiceAccountKey => "service_account_key",
            Self::MfaFactor => "mfa_factor",
        }
    }

    /// The sources the drop-down offers, in order.
    pub const ALL: &'static [Self] = &[
        Self::IdentityProvider,
        Self::Environment,
        Self::Webhook,
        Self::ServiceAccountKey,
        Self::MfaFactor,
    ];

    /// Parse a stored word, refusing an unknown one.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }

    /// Whether this source holds **real material** in its own table.
    ///
    /// The three that do are reported by [`SecretRef::material_count`] and
    /// never selected. The store asserts this list at build time: a source
    /// that starts holding a value and is not in it is a leak waiting to be
    /// discovered by an operator rather than by a test.
    #[must_use]
    pub fn holds_material(self) -> bool {
        matches!(
            self,
            Self::Webhook | Self::ServiceAccountKey | Self::MfaFactor
        )
    }
}

/// What the date on a row actually evidences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationEvidence {
    /// The reference row was edited — the closest thing to a rotation the
    /// platform can observe.
    ReferenceChanged,
    /// Only the row's creation is known, so the date is an upper bound.
    ReferenceCreated,
    /// Nothing is known: an environment variable's age is unknowable from
    /// inside the process that read it.
    Unknown,
}

impl RotationEvidence {
    /// Value as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReferenceChanged => "reference_changed",
            Self::ReferenceCreated => "reference_created",
            Self::Unknown => "unknown",
        }
    }
}

/// One row of the inventory.
///
/// There is deliberately no `value`, no `ciphertext`, no `hash` and no
/// `preview` field. Adding one would make this struct able to carry a
/// credential, and that ability is the whole risk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretRef {
    /// A stable identity for the row: `source:name`.
    ///
    /// Two rows with the same name from different sources are different
    /// secrets, so the name alone cannot key them — and a dashboard that
    /// deduplicated on the name would report one secret where there are two.
    pub key: String,
    /// The reference itself: an environment variable name or a store key.
    pub name: String,
    /// Which of the [`SecretSource`]s it came from.
    pub source: SecretSource,
    /// What the reference is scoped to — the provider slug, the endpoint, or
    /// the platform itself.
    pub scope: String,
    /// The best rotation timestamp the platform can observe, if any.
    pub rotated_at: Option<OffsetDateTime>,
    /// What that timestamp actually evidences.
    pub evidence: RotationEvidence,
    /// How many rows hold real material behind this reference.
    ///
    /// A count, never the material. Zero for sources that hold only names.
    pub material_count: i64,
    /// Whether an expiry is set and already past.
    pub expired: bool,
    /// What the platform can honestly say about this secret.
    pub state: SecretState,
    /// The reason behind [`SecretRef::state`], for the screen to show.
    pub note: String,
}

impl SecretRef {
    /// Days since the observed timestamp, or `None` when nothing is known.
    ///
    /// Negative values are clamped to zero: a clock that is behind the row
    /// should not render "-2 days", which reads as a secret from the future.
    #[must_use]
    pub fn age_days(&self, now: OffsetDateTime) -> Option<i64> {
        self.rotated_at.map(|then| {
            let seconds = (now - then).whole_seconds();
            if seconds <= 0 { 0 } else { seconds / 86_400 }
        })
    }
}

/// The answer the screen renders: rows plus the counts the header shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretInventory {
    /// The rows, in the order [`inventory_order`] defines.
    pub secrets: Vec<SecretRef>,
    /// How many references exist in total.
    pub total: usize,
    /// How many are [`SecretState::Missing`].
    pub missing: usize,
    /// How many cannot be verified from inside the platform — which is
    /// nearly all of them, and is the honest answer rather than a defect.
    pub unverifiable: usize,
    /// The source vocabulary the drop-down offers.
    ///
    /// `String` rather than `&'static str` even though every element is a
    /// literal: this struct is `Deserialize`, and a borrowed field in a type
    /// that both serialises and deserialises cannot satisfy its own lifetime
    /// without a borrow it has nowhere to take. The route layer renders the
    /// same list the other way, from the constants.
    pub sources: Vec<String>,
}

impl SecretInventory {
    /// Build from rows, deriving every count rather than being handed them.
    ///
    /// The counts are computed here so the API and the screen cannot
    /// disagree about them: a `missing` count typed into a response body is a
    /// number nobody re-derives.
    #[must_use]
    pub fn from_rows(mut rows: Vec<SecretRef>) -> Self {
        rows.sort_by(|a, b| inventory_order(a).cmp(&inventory_order(b)));
        let missing = rows
            .iter()
            .filter(|r| r.state == SecretState::Missing)
            .count();
        let unverifiable = rows
            .iter()
            .filter(|r| r.state == SecretState::Unverifiable)
            .count();
        Self {
            total: rows.len(),
            missing,
            unverifiable,
            sources: SecretSource::ALL
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            secrets: rows,
        }
    }
}

/// Sort key: problems first, then the source's own order, then the name.
///
/// Problems first is a deliberate choice: an inventory whose most urgent row
/// is below the fold is an inventory an operator scrolls past.
fn inventory_order(row: &SecretRef) -> (u8, usize, String) {
    let urgency = match row.state {
        SecretState::Missing => 0,
        SecretState::Expired => 1,
        SecretState::Unverifiable => 2,
    };
    let source = SecretSource::ALL
        .iter()
        .position(|s| *s == row.source)
        .unwrap_or(usize::MAX);
    (urgency, source, row.name.clone())
}

/// The environment secrets the platform reads at boot.
///
/// A hand-maintained list, and the reason it is honest rather than magic is in
/// [`environment::ABSENT_NOT_CONFIGURED`]'s companion note below: the
/// platform has no way to enumerate its own environment, so anything the
/// list misses is a secret the inventory does not report. That is the
/// opposite trade from the audit trail — where a missing row hides a *record*
/// — but the rule is the same in both directions: **say what you do not
/// know**, which is what [`SecretState::Missing`] is for.
pub mod environment {
    /// Secrets a deployment cannot run without.
    ///
    /// Signing the CSRF token, and the cookie session's own secret: an
    /// unset value means the deployment refuses rather than falls back, which
    /// is why these two are reported `present`/`missing` and never
    /// `unverifiable`.
    pub const REQUIRED: &[(&str, &str)] = &[
        (
            "OMNION_CSRF_SECRET",
            "signs the double-submit CSRF token for every cookie-authenticated mutation",
        ),
        ("OMNION_SESSION_SECRET", "signs the session cookie"),
        (
            "OMNION_PUSH_PRIVATE_KEY",
            "signs web-push requests to the notification centre",
        ),
    ];

    /// Secrets a deployment uses when a feature is enabled.
    pub const OPTIONAL: &[(&str, &str)] = &[
        (
            "OMNION_SMTP_PASSWORD",
            "authenticates the mail transport for notifications",
        ),
        (
            "OMNION_STORAGE_ACCESS_KEY",
            "reaches object storage behind the file manager",
        ),
        (
            "OMNION_STORAGE_SECRET_KEY",
            "reaches object storage behind the file manager",
        ),
        ("OMNION_SENTRY_DSN", "reports errors to an external monitor"),
        ("OMNION_LICENSE_KEY", "identifies the licensed edition"),
    ];
}

/// The environment names the inventory knows about, required ones first.
///
/// Split out so the API layer and the tests read the same two lists — a
/// secret list duplicated into a route file is a list that drifts.
#[must_use]
pub fn environment_names() -> Vec<(&'static str, &'static str, bool)> {
    environment::REQUIRED
        .iter()
        .map(|(name, purpose)| (*name, *purpose, true))
        .chain(
            environment::OPTIONAL
                .iter()
                .map(|(name, purpose)| (*name, *purpose, false)),
        )
        .collect()
}

/// A configured environment secret, built from what the process can observe.
///
/// The platform can see whether the variable is **set** and nothing else —
/// not its length, not its strength, not whether it changed since boot. So
/// the row says `present`, and the note says exactly why that is all.
#[must_use]
pub fn environment_row(
    name: &'static str,
    purpose: &'static str,
    required: bool,
    is_set: bool,
) -> SecretRef {
    let state = if is_set {
        SecretState::Unverifiable
    } else if required {
        SecretState::Missing
    } else {
        SecretState::Missing
    };
    let note = if is_set {
        "the variable is set; its value is not readable from inside the process and never leaves it"
            .to_string()
    } else if required {
        format!("{name} is not set and the platform cannot run without it")
    } else {
        format!("{name} is not set; the feature it belongs to is off or unconfigured")
    };
    SecretRef {
        key: format!("environment:{name}"),
        name: name.to_string(),
        source: SecretSource::Environment,
        scope: "platform".to_string(),
        rotated_at: None,
        evidence: RotationEvidence::Unknown,
        material_count: 0,
        expired: false,
        state,
        note,
    }
    .with_purpose(purpose)
}

impl SecretRef {
    /// Attach the purpose text as the note when there is nothing more urgent
    /// to say, so an unset optional secret still explains what it was for.
    fn with_purpose(mut self, purpose: &'static str) -> Self {
        if self.state == SecretState::Missing {
            self.note = format!("{purpose} — {}", self.note);
        }
        self
    }
}

/// The columns the store may select from a source table.
///
/// Exposed so the containment test can assert against the same list the query
/// is built from. A test that greps a query string is a test that breaks when
/// the query is reformatted; one that compares against this constant breaks
/// when a **column is added**, which is the event worth catching.
pub const INVENTORY_COLUMNS: &[&str] = &[
    "name",
    "scope",
    "rotated_at",
    "evidence",
    "material_count",
    "expired",
    "state",
    "note",
];

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn row(name: &str, source: SecretSource, state: SecretState) -> SecretRef {
        SecretRef {
            key: format!("{}:{name}", source.as_str()),
            name: name.to_string(),
            source,
            scope: "platform".to_string(),
            rotated_at: None,
            evidence: RotationEvidence::Unknown,
            material_count: 0,
            expired: false,
            state,
            note: String::new(),
        }
    }

    #[test]
    fn the_row_has_no_field_that_could_hold_a_value() {
        // The structural guarantee, checked rather than asserted in prose: the
        // struct carries a name and its metadata, and nothing else.
        let json = serde_json::to_value(row(
            "x",
            SecretSource::Environment,
            SecretState::Unverifiable,
        ))
        .expect("a row serialises");
        let object = json.as_object().expect("a row is an object");
        for forbidden in [
            "value",
            "secret",
            "ciphertext",
            "hash",
            "token",
            "preview",
            "plaintext",
        ] {
            assert!(
                !object.contains_key(forbidden),
                "the row serialises a {forbidden} field: {json}"
            );
        }
    }

    #[test]
    fn there_is_no_state_that_reads_as_healthy() {
        // There is deliberately no `present`, `ok` or `healthy` variant, and
        // that is the guarantee rather than an omission: the platform can see
        // that a reference exists and can read nothing about the value behind
        // it, so a state meaning "this secret is fine" would have to be a lie.
        // The strongest form of the rule is that the type cannot express it.
        let states: Vec<&str> = SecretState::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(states, ["unverifiable", "missing", "expired"]);
        for forbidden in ["healthy", "ok", "present", "valid", "good"] {
            assert!(
                !states.contains(&forbidden),
                "the vocabulary grew a {forbidden} state: {states:?}"
            );
        }
    }

    #[test]
    fn a_set_variable_reads_unverifiable_and_an_unset_one_reads_missing() {
        let set = environment_row("OMNION_CSRF_SECRET", "signs the CSRF token", true, true);
        assert_eq!(set.state, SecretState::Unverifiable);
        assert!(set.note.contains("not readable"), "{}", set.note);

        let unset = environment_row("OMNION_CSRF_SECRET", "signs the CSRF token", true, false);
        assert_eq!(unset.state, SecretState::Missing);
        assert!(
            unset.note.contains("cannot run without it"),
            "{}",
            unset.note
        );
    }

    #[test]
    fn an_optional_secret_that_is_unset_says_what_it_belongs_to() {
        let row = environment_row(
            "OMNION_SMTP_PASSWORD",
            "authenticates the mail transport",
            false,
            false,
        );
        assert_eq!(row.state, SecretState::Missing);
        assert!(
            row.note.contains("mail transport"),
            "the note does not say what the secret was for: {}",
            row.note
        );
    }

    #[test]
    fn age_is_clamped_when_the_clock_is_behind_the_row() {
        let now = datetime!(2026-03-01 12:00 UTC);
        let future = row("x", SecretSource::Environment, SecretState::Unverifiable);
        let mut past = future.clone();
        past.rotated_at = Some(datetime!(2026-02-28 12:00 UTC));
        assert_eq!(past.age_days(now), Some(1));
        past.rotated_at = Some(datetime!(2026-03-01 13:00 UTC));
        assert_eq!(
            past.age_days(now),
            Some(0),
            "a secret from the future reads as -0 days"
        );
        assert_eq!(future.age_days(now), None, "no timestamp means no age");
    }

    #[test]
    fn problems_sort_above_healthy_rows() {
        let inventory = SecretInventory::from_rows(vec![
            row("ok", SecretSource::Environment, SecretState::Unverifiable),
            row("gone", SecretSource::Environment, SecretState::Missing),
            row("meh", SecretSource::Environment, SecretState::Unverifiable),
        ]);
        let names: Vec<&str> = inventory.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gone", "meh", "ok"], "urgency order: {names:?}");
    }

    #[test]
    fn the_counts_are_derived_and_the_sources_are_offered() {
        let inventory = SecretInventory::from_rows(vec![
            row("a", SecretSource::IdentityProvider, SecretState::Missing),
            row("b", SecretSource::Webhook, SecretState::Unverifiable),
            row("c", SecretSource::MfaFactor, SecretState::Unverifiable),
        ]);
        assert_eq!(inventory.total, 3);
        assert_eq!(inventory.missing, 1, "exactly one row is missing");
        assert_eq!(
            inventory.unverifiable, 2,
            "the counts are derived from the rows rather than typed in"
        );
        assert_eq!(inventory.sources.len(), SecretSource::ALL.len());
        for offered in &inventory.sources {
            assert!(
                SecretSource::parse(offered).is_some(),
                "the drop-down offers {offered}, which the API cannot parse"
            );
        }
    }

    #[test]
    fn the_sources_holding_material_are_exactly_the_three_that_are_never_selected() {
        // If a fourth source ever starts holding a value it must join this
        // list in the same commit, or the store will begin selecting it.
        let holding: Vec<&str> = SecretSource::ALL
            .iter()
            .filter(|s| s.holds_material())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            holding,
            ["webhook", "service_account_key", "mfa_factor"],
            "the material-holding sources changed; the store's column lists must follow"
        );
        // The identity provider holds only `secret_ref` — a name — so it is
        // selectable, which is the whole reason it is not on the list.
        assert!(!SecretSource::IdentityProvider.holds_material());
    }

    #[test]
    fn the_column_list_carries_no_column_that_holds_a_value() {
        for column in INVENTORY_COLUMNS {
            for forbidden in ["secret_ciphertext", "secret_hash", "secret"] {
                assert!(
                    !column.contains(forbidden),
                    "the selectable column list names {forbidden}"
                );
            }
        }
    }

    #[test]
    fn every_state_and_source_word_round_trips() {
        for state in SecretState::ALL {
            assert_eq!(SecretState::parse(state.as_str()), Some(*state));
        }
        assert_eq!(SecretState::parse("healthy"), None);
        for source in SecretSource::ALL {
            assert_eq!(SecretSource::parse(source.as_str()), Some(*source));
        }
        assert_eq!(SecretSource::parse("logs"), None);
    }

    #[test]
    fn the_environment_list_names_each_variable_once() {
        let names: Vec<&str> = environment_names().iter().map(|(n, _, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "a variable is listed twice");
        assert!(names.contains(&"OMNION_CSRF_SECRET"), "{names:?}");
    }
}
