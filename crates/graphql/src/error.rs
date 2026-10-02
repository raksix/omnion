//! The one error surface a GraphQL client branches on.
//!
//! The request is explicit that a client must be able to branch precisely: *"Limit refusals use
//! error codes (`DEPTH_LIMIT`, `COST_LIMIT`, `PERSISTED_QUERY_NOT_FOUND`, `RATE_LIMITED`) so
//! clients branch precisely."* A transport status is too coarse for that — a `400` cannot say
//! which of the four refusals happened, and a `200` envelope carrying an error still needs a
//! stable machine-readable string inside it.
//!
//! So [`Code`] is a closed set, not a free-text message. [`Error::code`] returns the exact
//! spelling the request names, [`Error::extensions`] returns it with the data a client needs to
//! act (the cap and the actual value for a limit refusal, the limit itself and its top
//! contributors for a cost refusal), and the message is for a human. A new refusal has to be
//! added here deliberately — which is the point: an undefined code is a compile error rather than
//! a typo in a handler.

use serde::Serialize;

/// The closed set of codes a caller can branch on.
///
/// Distinct codes for distinct refusals is the whole requirement: "the query was too deep" and
/// "the query was too expensive" are different client bugs and a client that cannot tell them
/// apart can only back off from both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Code {
    /// A syntax or validation failure — the document does not name a real field.
    GraphqlValidationFailed,
    /// The selection set nests deeper than the configured maximum.
    DepthLimit,
    /// The priced selection costs more than the caller's budget.
    CostLimit,
    /// More aliases than the configured maximum.
    AliasLimit,
    /// More fragments (defined or spread) than the configured maximum.
    FragmentLimit,
    /// A page size above the cap.
    PageSizeLimit,
    /// The query outran the request timeout.
    Timeout,
    /// The caller's rate bucket for this operation class is spent.
    RateLimited,
    /// Ad-hoc documents are refused because the installation is in persisted-only mode, or the
    /// requested document is not on the allowlist.
    PersistedQueryNotFound,
    /// The caller holds no read permission for the type, so the type is absent from their schema
    /// and validation fails naming it.
    TypeNotVisible,
    /// The caller holds no write permission for the field, so the mutation does not exist for
    /// them. Nothing is written.
    Forbidden,
    /// The surface was deprecated, its sunset passed, and it is gone (REQ-130 slice 4).
    ///
    /// A distinct code rather than a `404`: the whole point of announcing a deprecation is that
    /// the integrator can tell "moved" from "never existed", and a client that cannot branch on
    /// this cannot migrate off a date it was told about.
    Removed,
    /// The request is well-formed but the values are not one this feature accepts — an extension
    /// with no reason, a sunset inside the minimum window.
    InvalidInput,
    /// Anything else.
    Internal,
}

impl Code {
    /// The wire spelling, stable and part of the contract.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GraphqlValidationFailed => "GRAPHQL_VALIDATION_FAILED",
            Self::DepthLimit => "DEPTH_LIMIT",
            Self::CostLimit => "COST_LIMIT",
            Self::AliasLimit => "ALIAS_LIMIT",
            Self::FragmentLimit => "FRAGMENT_LIMIT",
            Self::PageSizeLimit => "PAGE_SIZE_LIMIT",
            Self::Timeout => "TIMEOUT",
            Self::RateLimited => "RATE_LIMITED",
            Self::PersistedQueryNotFound => "PERSISTED_QUERY_NOT_FOUND",
            Self::TypeNotVisible => "TYPE_NOT_VISIBLE",
            Self::Forbidden => "FORBIDDEN",
            Self::Removed => "REMOVED",
            Self::InvalidInput => "INVALID_INPUT",
            Self::Internal => "INTERNAL",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal or a failure, carrying the code and the data a client acts on.
///
/// `extensions` is deliberately not free-form: [`Code`] picks the fields, so two refusals of the
/// same kind always carry the same shape. The cost refusal in particular ships
/// `limit` + `contributors`, because the request asks the meter to *"name the top contributors
/// and their weights"* — a cost error that only says "over budget" makes the caller guess which
/// field to drop.
/// `PartialEq` is derived because an error is a **value** here: the endpoint and the playground
/// both compare a decision against an expected one, and a refusal that cannot be compared forces
/// every caller to match on the message string instead of on the code — which is how a
/// reworded message silently breaks a client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{message}")]
    Validation { code: Code, message: String },
    #[error("{message} (limit {limit}, requested {actual})")]
    Limit {
        code: Code,
        message: String,
        limit: u64,
        actual: u64,
    },
    #[error("{message}")]
    Cost {
        message: String,
        limit: u32,
        cost: u32,
        /// Highest-weighted fields, largest first. Never empty on a cost refusal: the meter has
        /// to be able to say what to drop.
        contributors: Vec<(String, u32)>,
    },
    #[error("{message}")]
    Simple { code: Code, message: String },
}

impl Error {
    /// The code a client branches on.
    pub fn code(&self) -> Code {
        match self {
            Self::Validation { code, .. }
            | Self::Limit { code, .. }
            | Self::Simple { code, .. } => *code,
            Self::Cost { .. } => Code::CostLimit,
        }
    }

    /// The wire spelling of [`Error::code`].
    pub fn code_str(&self) -> &'static str {
        self.code().as_str()
    }

    /// The `extensions` object. Same code, same shape, every time.
    pub fn extensions(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("code".into(), self.code_str().into());
        match self {
            Self::Limit { limit, actual, .. } => {
                map.insert("limit".into(), (*limit).into());
                map.insert("actual".into(), (*actual).into());
            }
            Self::Cost {
                limit,
                cost,
                contributors,
                ..
            } => {
                map.insert("limit".into(), (*limit).into());
                map.insert("cost".into(), (*cost).into());
                map.insert(
                    "contributors".into(),
                    serde_json::Value::Array(
                        contributors
                            .iter()
                            .map(|(name, weight)| {
                                serde_json::json!({ "field": name, "weight": weight })
                            })
                            .collect(),
                    ),
                );
            }
            _ => {}
        }
        serde_json::Value::Object(map)
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_spells_exactly_as_the_request_documents_it() {
        // These four strings are named in the request's API section. A rename here is a
        // breaking change for every client, so they are asserted literally rather than
        // re-derived from the enum.
        for (code, wire) in [
            (Code::DepthLimit, "DEPTH_LIMIT"),
            (Code::CostLimit, "COST_LIMIT"),
            (Code::PersistedQueryNotFound, "PERSISTED_QUERY_NOT_FOUND"),
            (Code::RateLimited, "RATE_LIMITED"),
        ] {
            assert_eq!(code.as_str(), wire);
        }
    }

    #[test]
    fn distinct_refusals_get_distinct_codes() {
        // "so clients branch precisely": the pair most likely to be conflated is depth and
        // cost, because a deep query is usually also an expensive one.
        assert_ne!(Code::DepthLimit.as_str(), Code::CostLimit.as_str());
        assert_ne!(Code::AliasLimit.as_str(), Code::FragmentLimit.as_str());
        assert_ne!(
            Code::PersistedQueryNotFound.as_str(),
            Code::RateLimited.as_str()
        );
        assert_ne!(Code::TypeNotVisible.as_str(), Code::Forbidden.as_str());
    }

    #[test]
    fn a_cost_refusal_carries_the_contributors_a_meter_must_show() {
        let err = Error::Cost {
            message: "query costs 1400 over a budget of 1000".into(),
            limit: 1000,
            cost: 1400,
            contributors: vec![
                ("articles.list".into(), 1200),
                ("articles.byId".into(), 200),
            ],
        };
        assert_eq!(err.code_str(), "COST_LIMIT");
        let ext = err.extensions();
        assert_eq!(ext["code"], "COST_LIMIT");
        assert_eq!(ext["limit"], 1000);
        assert_eq!(ext["cost"], 1400);
        // The meter names the top contributor and its weight, in the order given.
        assert_eq!(ext["contributors"][0]["field"], "articles.list");
        assert_eq!(ext["contributors"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(ext["contributors"][1]["weight"], 200);
        // Ordered largest-first, so the meter's top-3 is a slice and not a sort at render time.
        assert!(
            ext["contributors"][0]["weight"].as_u64() >= ext["contributors"][1]["weight"].as_u64()
        );
    }

    #[test]
    fn a_limit_refusal_reports_the_cap_and_the_value_that_broke_it() {
        let err = Error::Limit {
            code: Code::DepthLimit,
            message: "selection nests 12 levels, the limit is 10".into(),
            limit: 10,
            actual: 12,
        };
        let ext = err.extensions();
        assert_eq!(ext["code"], "DEPTH_LIMIT");
        assert_eq!(ext["limit"], 10);
        assert_eq!(ext["actual"], 12);
        // A limit refusal has no cost shape: the keys are the ones that apply to it and no
        // others, so a client can switch on presence without guessing.
        assert!(ext.get("contributors").is_none());
    }

    #[test]
    fn a_simple_refusal_still_carries_its_code() {
        let err = Error::Simple {
            code: Code::Forbidden,
            message: "the caller holds no write permission for this field".into(),
        };
        assert_eq!(err.extensions(), serde_json::json!({ "code": "FORBIDDEN" }));
    }
}
