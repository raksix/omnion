//! Endpoint settings: the values an operator can change, and the ones they cannot.
//!
//! The request puts these under the platform settings store (REQ-112) under a `graphql.*`
//! prefix "so changes are versioned like every other setting". This module owns the **shape and
//! the validation**, not the persistence — the settings store reads and writes the row, and this
//! decides what a legal row looks like.
//!
//! ## Why validation lives here and not in the handler
//!
//! The playground's cost meter has to refuse an over-budget query **before** it is sent, using the
//! same numbers the endpoint will use. If the limits were validated in the handler, the meter
//! would have to trust a value it could not check, and the two would drift. Both call
//! [`Settings::validate`], so a meter that says "allowed" is a meter the endpoint agrees with.

use serde::{Deserialize, Serialize};

use crate::error::{Code, Error, Result};

/// The stored shape of `graphql.*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Maximum selection nesting. The request's default is 10.
    #[serde(default = "default_depth")]
    pub max_depth: u32,
    /// Per-operation cost budget. The request's default is 1000.
    #[serde(default = "default_cost")]
    pub cost_budget: u32,
    /// Maximum aliases in one operation.
    #[serde(default = "default_aliases")]
    pub max_aliases: u32,
    /// Maximum fragment definitions in one document.
    #[serde(default = "default_fragments")]
    pub max_fragments: u32,
    /// Page-size cap.
    #[serde(default = "default_page_size")]
    pub max_page_size: u32,
    /// Execution timeout.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Whether ad-hoc documents are refused and only allowlisted documents execute.
    ///
    /// The request: *"A `persisted_only` setting per environment refuses ad-hoc documents in
    /// production, so a leaked read scope cannot run arbitrary queries."*
    #[serde(default)]
    pub persisted_only: bool,
    /// Whether the authenticated playground is reachable. Not the same as `persisted_only`: an
    /// installation may run persisted-only with the playground open for typing new documents,
    /// or close the playground while still serving registered ones.
    #[serde(default = "default_true")]
    pub playground_enabled: bool,
}

fn default_depth() -> u32 {
    10
}
fn default_cost() -> u32 {
    1000
}
fn default_aliases() -> u32 {
    15
}
fn default_fragments() -> u32 {
    20
}
fn default_page_size() -> u32 {
    100
}
fn default_timeout() -> u64 {
    10_000
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_depth: default_depth(),
            cost_budget: default_cost(),
            max_aliases: default_aliases(),
            max_fragments: default_fragments(),
            max_page_size: default_page_size(),
            timeout_ms: default_timeout(),
            persisted_only: false,
            playground_enabled: true,
        }
    }
}

impl Settings {
    /// The limits these settings describe.
    pub fn limits(&self) -> crate::limits::Limits {
        crate::limits::Limits::from_settings(self)
    }

    /// Whether ad-hoc documents may execute.
    ///
    /// Split out because "persisted_only" is checked in two places with different consequences
    /// — the endpoint refuses, and the playground labels its own editor — and a single boolean
    /// read in each keeps them from disagreeing about what the flag means.
    pub fn allows_ad_hoc(&self) -> bool {
        !self.persisted_only
    }

    /// Validate a settings write, naming the field and the accepted range.
    ///
    /// Every field gets its own message. The request's slice-4 precedent is explicit that a
    /// generic "invalid settings" is what makes a settings screen unusable: the operator needs
    /// to know *which* number and *what range*.
    pub fn validate(&self) -> Result<()> {
        if self.max_depth == 0 {
            return Err(field("max_depth", "must be at least 1"));
        }
        if self.max_depth > crate::document::PARSE_DEPTH_CEILING {
            return Err(field(
                "max_depth",
                &format!(
                    "must be at most {}, the parser's own ceiling",
                    crate::document::PARSE_DEPTH_CEILING
                ),
            ));
        }
        if self.cost_budget == 0 {
            return Err(field("cost_budget", "must be at least 1"));
        }
        if self.max_aliases == 0 {
            return Err(field("max_aliases", "must be at least 1"));
        }
        if self.max_fragments == 0 {
            return Err(field("max_fragments", "must be at least 1"));
        }
        if self.max_page_size == 0 {
            return Err(field("max_page_size", "must be at least 1"));
        }
        if self.timeout_ms < 100 {
            // Not zero-for-allowed: a sub-100 ms timeout refuses every query including a trivial
            // one, which looks like an outage rather than a misconfiguration.
            return Err(field("timeout_ms", "must be at least 100"));
        }
        Ok(())
    }
}

fn field(name: &str, requirement: &str) -> Error {
    Error::Validation {
        code: Code::GraphqlValidationFailed,
        message: format!("`{name}` {requirement}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_numbers_the_request_names() {
        let settings = Settings::default();
        assert_eq!(settings.max_depth, 10, "the request's depth default");
        assert_eq!(settings.cost_budget, 1000, "the request's cost default");
        assert_eq!(settings.max_page_size, 100, "the request's page cap");
        assert_eq!(settings.timeout_ms, 10_000, "the request's timeout");
        settings.validate().expect("the defaults are legal");
    }

    #[test]
    fn settings_translate_into_the_limits_the_endpoint_enforces() {
        let settings = Settings {
            max_depth: 4,
            cost_budget: 250,
            ..Settings::default()
        };
        let limits = settings.limits();
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_cost, 250);
    }

    #[test]
    fn every_out_of_range_field_is_refused_by_name_with_its_own_message() {
        // One message per field: a settings screen that says "invalid settings" for four
        // different numbers is a settings screen the operator cannot use.
        let cases: Vec<(fn(&mut Settings), &str)> = vec![
            (|s| s.max_depth = 0, "max_depth"),
            (|s| s.cost_budget = 0, "cost_budget"),
            (|s| s.max_aliases = 0, "max_aliases"),
            (|s| s.max_fragments = 0, "max_fragments"),
            (|s| s.max_page_size = 0, "max_page_size"),
            (|s| s.timeout_ms = 50, "timeout_ms"),
        ];
        for (mutate, field_name) in cases {
            let mut settings = Settings::default();
            mutate(&mut settings);
            let err = settings
                .validate()
                .expect_err(&format!("`{field_name}` must be refused"));
            assert!(
                err.to_string().contains(field_name),
                "the message for `{field_name}` does not name it: {err}"
            );
        }
    }

    #[test]
    fn a_depth_above_the_parsers_own_ceiling_is_refused_with_the_ceiling_named() {
        let settings = Settings {
            max_depth: crate::document::PARSE_DEPTH_CEILING + 1,
            ..Settings::default()
        };
        let err = settings
            .validate()
            .expect_err("above the ceiling is refused");
        assert!(err.to_string().contains("ceiling"), "{err}");
        // This is the guard that stops an operator from raising the policy above the bound the
        // parser actually enforces — the setting that promises a protection it cannot deliver.
        Settings {
            max_depth: crate::document::PARSE_DEPTH_CEILING,
            ..Settings::default()
        }
        .validate()
        .expect("the ceiling itself is legal");
    }

    #[test]
    fn persisted_only_and_the_playground_toggle_are_separate_choices() {
        let registered_only = Settings {
            persisted_only: true,
            playground_enabled: true,
            ..Settings::default()
        };
        assert!(!registered_only.allows_ad_hoc());
        assert!(registered_only.playground_enabled);

        let closed = Settings {
            persisted_only: false,
            playground_enabled: false,
            ..Settings::default()
        };
        assert!(
            closed.allows_ad_hoc(),
            "a closed playground still serves ad-hoc queries"
        );
        assert!(!closed.playground_enabled);
    }

    #[test]
    fn a_settings_write_round_trips_through_json() {
        // The playground keeps unsent text and the settings screen reads the row; a shape that
        // does not survive serialisation would make the defaults below it silently wrong.
        let settings = Settings {
            max_depth: 6,
            persisted_only: true,
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).expect("settings serialise");
        let back: Settings = serde_json::from_str(&json).expect("settings deserialise");
        assert_eq!(settings, back);
    }

    #[test]
    fn a_row_missing_every_field_takes_the_defaults_rather_than_zero() {
        // A settings store that returns an empty object must not hand the endpoint a limit of
        // zero, which would refuse every query including `__typename`.
        let back: Settings = serde_json::from_str("{}").expect("an empty object deserialises");
        assert_eq!(back, Settings::default());
        back.validate().expect("the defaults validate");
    }
}
