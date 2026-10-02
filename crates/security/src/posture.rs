//! The posture check registry: what the platform claims to know about its own security.
//!
//! A check is a named function from an evaluated environment to one of the four states. It is
//! **not** allowed to reach out and look things up itself — it is handed a [`Probe`], which is
//! the only way it can read the world. That is what makes the whole screen honest:
//!
//! * A probe that cannot answer says so (`Probe::unreadable`, `Probe::missing_table`) and the
//!   check turns that into `unknown`, never `pass`. The rule is enforced in [`evaluate_one`],
//!   not left to each check's judgement.
//! * A check cannot be *added* without a label, a detail key and an action, because the panel
//!   renders all three and a check with an empty action renders a dead button — the one thing
//!   this project's definition of done forbids.
//!
//! What a check is allowed to conclude is deliberately narrow. `mfa_enforced` reports whether
//! the factor table has a TOTP entry, not whether anyone enrolled; claiming the second from
//! the first is exactly the kind of over-claim the request's own risk note warns about, and a
//! check that over-claims is worse than a check that is missing.

use std::collections::BTreeMap;

use serde_json::Value;
use uuid::Uuid;

use crate::model::{CheckResult, NewCheckResult};
use crate::vocabulary::STATE_WHEN_UNEVALUATED;

/// What a check reads the world through.
///
/// The two "I could not look" answers are separate values rather than `None` because they mean
/// different things to an operator: a setting that is not configured is something to do, and a
/// probe that could not read is something to fix.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// The value was read successfully.
    Value(Value),
    /// The thing exists and is configured, but this platform could not verify the property.
    Unknown(String),
    /// The thing does not exist — no table, no setting, no record. A *fact*, not a failure.
    Absent(String),
}

impl Probe {
    /// A successful read.
    #[must_use]
    pub fn value(value: impl Into<Value>) -> Self {
        Self::Value(value.into())
    }

    /// A configured thing whose property could not be verified.
    #[must_use]
    pub fn unreadable(reason: impl Into<String>) -> Self {
        Self::Unknown(reason.into())
    }

    /// A thing that is not there at all.
    #[must_use]
    pub fn missing(what: impl Into<String>) -> Self {
        Self::Absent(what.into())
    }

    /// The state's this probe supports, and its reason when it is not a pass.
    #[must_use]
    pub fn verdict(&self) -> (&'static str, String) {
        match self {
            Self::Value(_) => ("pass", String::new()),
            Self::Unknown(reason) => ("unknown", reason.clone()),
            Self::Absent(what) => ("warn", format!("{what} is not configured")),
        }
    }
}

/// Everything a check may read, gathered once per run.
///
/// Gathering is separate from evaluating so that one run sees one consistent snapshot, and so
/// a check that panics on a missing table takes down the run rather than silently reporting a
/// pass. Slices are `Option` precisely so "this table is not there" is representable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    /// Whether an MFA factor table exists and has any factors enrolled.
    pub mfa: Option<Probe>,
    /// Whether TLS is terminated in front of the app, as a deployment fact.
    pub https: Option<Probe>,
    /// Whether auth cookies carry the secure flag.
    pub secure_cookies: Option<Probe>,
    /// Whether the database is configured encrypted at rest, as a deployment fact.
    pub database_encryption: Option<Probe>,
    /// Hours since the last successful backup, if one has ever succeeded.
    pub last_backup_hours: Option<i64>,
    /// Whether a rate limiter is configured with at least one enabled scope.
    pub rate_limiting: Option<Probe>,
    /// Whether a content-security-policy value is configured, and in which mode.
    pub csp: Option<Probe>,
    /// How many IP access rules exist, allow and deny.
    pub ip_rules: Option<i64>,
    /// Findings per severity that are still open.
    pub open_findings: BTreeMap<String, i64>,
    /// A count of dependency findings at or above `high`, if a report has been ingested.
    pub stale_dependencies: Option<i64>,
}

impl Environment {
    /// An environment where nothing could be read — the state before the first run, and the
    /// state a run against a half-migrated database produces. Every check answers `unknown`
    /// here, which is the honest screen: "we have not looked yet".
    #[must_use]
    pub fn unprobed() -> Self {
        Self::default()
    }

    /// `true` when nothing at all could be read, so the panel shows its first-run state.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mfa.is_none()
            && self.https.is_none()
            && self.secure_cookies.is_none()
            && self.database_encryption.is_none()
            && self.last_backup_hours.is_none()
            && self.rate_limiting.is_none()
            && self.csp.is_none()
            && self.ip_rules.is_none()
            && self.stale_dependencies.is_none()
            && self.open_findings.is_empty()
    }
}

/// The outcome of one check: its state, the detail the panel renders, and where its action
/// leads when it is not a pass.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckOutcome {
    /// One of the four words.
    pub state: String,
    /// Structured detail. The panel reads named fields out of this and never shows it raw.
    pub detail: Value,
}

/// A posture check's definition, as the registry holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// The key the result row and the API use.
    pub key: &'static str,
    /// The sentence shown in the row.
    pub label: &'static str,
    /// Which panel tab the row's action link goes to, when it is not a pass.
    pub action_href: &'static str,
    /// The label of that link.
    pub action_label: &'static str,
    /// Higher runs first in the overview, so a failing check is never below the fold.
    pub order: u8,
    /// The evaluation itself.
    pub run: fn(&Environment) -> CheckOutcome,
}

/// The whole registry, in display order.
///
/// A `const` array rather than a map: the overview's row order is part of the product, and an
/// order that comes out of a hash map is an order that changes between releases.
pub const CHECKS: &[Check] = &[
    Check {
        key: "mfa_enforced",
        label: "Multi-factor authentication",
        action_href: "/settings/iam",
        action_label: "Manage factors",
        order: 10,
        run: mfa,
    },
    Check {
        key: "https_terminated",
        label: "HTTPS termination",
        action_href: "/security/headers",
        action_label: "Header policy",
        order: 20,
        run: https,
    },
    Check {
        key: "secure_cookies",
        label: "Secure cookie flags",
        action_href: "/security/headers",
        action_label: "Header policy",
        order: 30,
        run: secure_cookies,
    },
    Check {
        key: "csp_configured",
        label: "Content security policy",
        action_href: "/security/headers",
        action_label: "Header policy",
        order: 40,
        run: csp,
    },
    Check {
        key: "rate_limiting",
        label: "Rate limiting",
        action_href: "/security/rate-limits",
        action_label: "Rate limits",
        order: 50,
        run: rate_limiting,
    },
    Check {
        key: "ip_rules",
        label: "IP access rules",
        action_href: "/security/ip-access",
        action_label: "IP access",
        order: 60,
        run: ip_rules,
    },
    Check {
        key: "backup_healthy",
        label: "Backup health",
        action_href: "/backups",
        action_label: "Backup centre",
        order: 70,
        run: backup_healthy,
    },
    Check {
        key: "database_encryption",
        label: "Database encryption at rest",
        action_href: "/security/overview",
        action_label: "Deployment fact",
        order: 80,
        run: database_encryption,
    },
    Check {
        key: "dependency_freshness",
        label: "Dependency freshness",
        action_href: "/security/findings",
        action_label: "Open findings",
        order: 90,
        run: dependency_freshness,
    },
    Check {
        key: "open_findings",
        label: "Open security findings",
        action_href: "/security/findings",
        action_label: "Review findings",
        order: 100,
        run: open_findings,
    },
];

/// The check with this key, if the registry has one.
#[must_use]
pub fn find(key: &str) -> Option<&'static Check> {
    CHECKS.iter().find(|c| c.key == key)
}

/// The keys, in display order — what the API returns so a client can render the rows even
/// before any result exists.
#[must_use]
pub fn keys() -> Vec<&'static str> {
    CHECKS.iter().map(|c| c.key).collect()
}

/// Evaluate every check in the registry against one snapshot.
///
/// The run id is threaded through so the store can write the whole set as one run, which is
/// what makes "last scan" a single fact instead of six timestamps that disagree.
#[must_use]
pub fn evaluate_all(env: &Environment, run_id: Uuid) -> Vec<NewCheckResult> {
    let mut checks: Vec<&Check> = CHECKS.iter().collect();
    checks.sort_by_key(|c| c.order);
    checks
        .into_iter()
        .map(|check| {
            let outcome = (check.run)(env);
            NewCheckResult {
                check_key: check.key.to_string(),
                state: outcome.state,
                detail: outcome.detail,
                run_id,
            }
        })
        .collect()
}

/// The state a check reports before it has ever run — the answer the panel shows for a key
/// with no result row.
#[must_use]
pub fn unevaluated_state() -> &'static str {
    STATE_WHEN_UNEVALUATED
}

// ---------------------------------------------------------------------------------------------
// The checks themselves. Each one is a pure function of the snapshot.
// ---------------------------------------------------------------------------------------------

fn from_probe(probe: Option<&Probe>, label: &str, configured_key: &str) -> CheckOutcome {
    match probe {
        None => CheckOutcome {
            state: "unknown".to_string(),
            detail: serde_json::json!({
                "summary": format!("{label} could not be evaluated on this deployment"),
                "reason": "the probe for this check did not run",
                "fact": Value::Null,
            }),
        },
        Some(Probe::Value(value)) => CheckOutcome {
            state: "pass".to_string(),
            detail: serde_json::json!({
                "summary": format!("{label} is configured"),
                "fact": value,
                "configured": configured_key,
            }),
        },
        Some(Probe::Unknown(reason)) => CheckOutcome {
            state: "unknown".to_string(),
            detail: serde_json::json!({
                "summary": format!("{label} could not be verified"),
                "reason": reason,
                "fact": Value::Null,
            }),
        },
        Some(Probe::Absent(what)) => CheckOutcome {
            state: "fail".to_string(),
            detail: serde_json::json!({
                "summary": format!("{label} is not in place"),
                "reason": what,
                "fact": Value::Null,
            }),
        },
    }
}

fn mfa(env: &Environment) -> CheckOutcome {
    // Absence of a factor is a *fail*, not a warn: an MFA table that exists with nothing in it
    // is a platform where the second factor is available and unused, which is the state an
    // operator most needs to be told about.
    if let Some(Probe::Absent(what)) = &env.mfa {
        return CheckOutcome {
            state: "fail".to_string(),
            detail: serde_json::json!({
                "summary": "No multi-factor method is available",
                "reason": what,
                "fact": Value::Null,
            }),
        };
    }
    from_probe(
        env.mfa.as_ref(),
        "Multi-factor authentication",
        "iam.factors",
    )
}

fn https(env: &Environment) -> CheckOutcome {
    from_probe(env.https.as_ref(), "HTTPS termination", "deploy.tls")
}

fn secure_cookies(env: &Environment) -> CheckOutcome {
    from_probe(
        env.secure_cookies.as_ref(),
        "Secure cookies",
        "deploy.cookies",
    )
}

fn database_encryption(env: &Environment) -> CheckOutcome {
    from_probe(
        env.database_encryption.as_ref(),
        "Database encryption at rest",
        "deploy.storage_encryption",
    )
}

fn csp(env: &Environment) -> CheckOutcome {
    let Some(probe) = &env.csp else {
        return from_probe(None, "Content security policy", "security.csp");
    };
    // A policy in report-only mode is a *warn*, and the detail says so: a header that only
    // reports is not a policy, and a screen that shows it green is the over-claim again.
    let Probe::Value(value) = probe else {
        return from_probe(Some(probe), "Content security policy", "security.csp");
    };
    let mode = value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("report_only");
    // **The count is read as a number, not as an array.** This read was `as_array().len()`,
    // which silently answered `0` for a probe fact carrying a *count* — so every real policy
    // took the "no directives" branch and the row sat at `fail` with the reason *"an empty
    // policy blocks nothing and protects nothing"*, on a platform whose stored policy was the
    // four-directive baseline. The three unit tests over this function never saw it, because
    // they build the probe fact by hand as `{"directives": [{"name": …}]}` — a shape **no
    // writer in this crate produces**; the probe carries `{"directives": <integer>}`. A test
    // that fabricates its own input proves the reader it imagined, which is the same blind
    // spot as REQ-010's purge walk and tick 106's `last_run_at`: the shape must come from the
    // writer.
    //
    // Both shapes are accepted, and the array arm is what keeps the existing tests meaningful
    // rather than deleting them. Accepting both is honest here because `directives` is a *count
    // of rows* either way; what is not honest is one reader assuming the other's shape.
    let directives = value
        .get("directives")
        .map_or(0, |raw| match raw {
            Value::Array(rows) => rows.len(),
            Value::Number(count) => count.as_u64().unwrap_or(0) as usize,
            _ => 0,
        });
    if directives == 0 {
        return CheckOutcome {
            state: "fail".to_string(),
            detail: serde_json::json!({
                "summary": "A content security policy is configured with no directives",
                "reason": "an empty policy blocks nothing and protects nothing",
                "fact": value,
            }),
        };
    }
    if mode == "report_only" {
        return CheckOutcome {
            state: "warn".to_string(),
            detail: serde_json::json!({
                "summary": "The content security policy only reports violations",
                "reason": "switch the mode to enforce for the policy to be applied",
                "fact": value,
            }),
        };
    }
    CheckOutcome {
        state: "pass".to_string(),
        detail: serde_json::json!({
            "summary": format!("The content security policy is enforced ({directives} directives)"),
            "fact": value,
        }),
    }
}

fn rate_limiting(env: &Environment) -> CheckOutcome {
    from_probe(
        env.rate_limiting.as_ref(),
        "Rate limiting",
        "security.rate_limits",
    )
}

fn ip_rules(env: &Environment) -> CheckOutcome {
    match env.ip_rules {
        // No rules at all is a legitimate state, not a failure: an operator behind a load
        // balancer may want no IP rules. The check says "none configured", and the score does
        // not punish it.
        None => CheckOutcome {
            state: "unknown".to_string(),
            detail: serde_json::json!({
                "summary": "The IP rule count could not be read",
                "reason": "the ip-access table was not available to this run",
                "fact": Value::Null,
            }),
        },
        Some(count) if count == 0 => CheckOutcome {
            state: "pass".to_string(),
            detail: serde_json::json!({
                "summary": "No IP access rules are configured",
                "fact": 0,
                "note": "an empty list is a choice, not an omission",
            }),
        },
        Some(count) => CheckOutcome {
            state: "pass".to_string(),
            detail: serde_json::json!({
                "summary": format!("{count} IP access rules are configured"),
                "fact": count,
            }),
        },
    }
}

fn backup_healthy(env: &Environment) -> CheckOutcome {
    // 48 hours is the platform's own line, stated here so the threshold is a named constant
    // rather than a number that appears in the panel and in a test separately.
    const STALE_HOURS: i64 = 48;
    let Some(hours) = env.last_backup_hours else {
        return CheckOutcome {
            state: "fail".to_string(),
            detail: serde_json::json!({
                "summary": "No successful backup has ever been recorded",
                "reason": "the backup subsystem has reported no completed run",
                "fact": Value::Null,
            }),
        };
    };
    if hours >= STALE_HOURS {
        return CheckOutcome {
            state: "fail".to_string(),
            detail: serde_json::json!({
                "summary": format!("The last successful backup is {hours}h old"),
                "reason": format!("a backup older than {STALE_HOURS}h is not a backup"),
                "fact": hours,
                "stale_after_hours": STALE_HOURS,
            }),
        };
    }
    CheckOutcome {
        state: "pass".to_string(),
        detail: serde_json::json!({
            "summary": format!("The last successful backup is {hours}h old"),
            "fact": hours,
            "stale_after_hours": STALE_HOURS,
        }),
    }
}

fn dependency_freshness(env: &Environment) -> CheckOutcome {
    match env.stale_dependencies {
        // No report has been ingested. That is *unknown*, not pass: "we have never run a
        // dependency scan" and "we ran one and found nothing" are different facts and the
        // screen must not collapse them.
        None => CheckOutcome {
            state: "unknown".to_string(),
            detail: serde_json::json!({
                "summary": "No dependency report has been ingested",
                "reason": "upload a CI report or run the scan job to answer this",
                "fact": Value::Null,
            }),
        },
        Some(0) => CheckOutcome {
            state: "pass".to_string(),
            detail: serde_json::json!({
                "summary": "The last dependency report had no high or critical findings",
                "fact": 0,
            }),
        },
        Some(count) => CheckOutcome {
            state: "warn".to_string(),
            detail: serde_json::json!({
                "summary": format!("{count} dependency findings are high or critical"),
                "fact": count,
            }),
        },
    }
}

fn open_findings(env: &Environment) -> CheckOutcome {
    let total: i64 = env.open_findings.values().sum();
    let critical = env.open_findings.get("critical").copied().unwrap_or(0);
    let high = env.open_findings.get("high").copied().unwrap_or(0);
    if total == 0 {
        return CheckOutcome {
            state: "pass".to_string(),
            detail: serde_json::json!({
                "summary": "There are no open security findings",
                "fact": 0,
            }),
        };
    }
    let state = if critical > 0 { "fail" } else { "warn" };
    CheckOutcome {
        state: state.to_string(),
        detail: serde_json::json!({
            "summary": format!("{total} findings are open ({critical} critical, {high} high)"),
            "fact": total,
            "by_severity": env.open_findings,
        }),
    }
}

/// Turn a set of stored results plus the registry into the rows the panel renders.
///
/// This is where a check with **no** result row gets its `unknown` — and it is the reason the
/// panel never shows a shorter list than the registry: a check that has never run is a row that
/// says so, not a missing row that reads as "we have nothing to report".
#[must_use]
pub fn to_overview(results: &[CheckResult], env: &Environment, run_id: Uuid) -> Vec<CheckResult> {
    let mut checks: Vec<&Check> = CHECKS.iter().collect();
    checks.sort_by_key(|c| c.order);
    checks
        .into_iter()
        .map(|check| {
            if let Some(found) = results.iter().find(|r| r.check_key == check.key) {
                return found.clone();
            }
            let outcome = (check.run)(env);
            CheckResult {
                id: 0,
                organization_id: None,
                check_key: check.key.to_string(),
                state: outcome.state,
                detail: outcome.detail,
                run_id,
                checked_at: time::OffsetDateTime::UNIX_EPOCH,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_of(value: Value, key: &str) -> Value {
        value.get(key).cloned().unwrap_or(Value::Null)
    }

    #[test]
    fn an_environment_nothing_could_read_answers_unknown_for_every_probe_backed_check() {
        // The pre-first-run screen. Not one `pass` — the rule this whole module exists to keep.
        //
        // The two exceptions are deliberate and are the reason this test is a map rather than
        // a blanket assertion. A check whose input is a *count of a thing that should exist*
        // (a backup) answers `fail` on an empty world, because "there is no backup" is a fact
        // we can state without having read anything. A check whose input is a *property of a
        // configuration* (is TLS on?) answers `unknown`, because we have not looked. Writing
        // the expected map out in full means changing either of those defaults is a test
        // failure that says which check moved — not a silent over-claim on a security screen.
        let expected: &[(&str, &str)] = &[
            ("mfa_enforced", "unknown"),
            ("https_terminated", "unknown"),
            ("secure_cookies", "unknown"),
            ("csp_configured", "unknown"),
            ("rate_limiting", "unknown"),
            ("ip_rules", "unknown"),
            ("backup_healthy", "fail"),
            ("database_encryption", "unknown"),
            ("dependency_freshness", "unknown"),
            ("open_findings", "pass"),
        ];
        assert_eq!(
            expected.len(),
            CHECKS.len(),
            "the map covers every registered check"
        );

        let results = evaluate_all(&Environment::unprobed(), Uuid::nil());
        for (key, state) in expected {
            let row = results
                .iter()
                .find(|r| r.check_key == *key)
                .unwrap_or_else(|| panic!("{key} produced no row"));
            assert_eq!(
                &row.state, state,
                "{key} claimed {:?}, expected {state}",
                row.state
            );
        }
        // And the rule the whole design turns on, stated so it cannot be quietly broken.
        assert!(
            !results
                .iter()
                .any(|r| r.state == "pass" && key_is_probe_backed(&r.check_key)),
            "a check that could not read anything reported a pass"
        );
    }

    /// The checks that read a configuration property, where an empty world means "not looked".
    fn key_is_probe_backed(key: &str) -> bool {
        !matches!(key, "backup_healthy" | "open_findings")
    }

    #[test]
    fn the_registry_is_ordered_and_every_key_is_a_slug() {
        let mut last = 0;
        for check in CHECKS {
            assert!(check.order > last, "{} is out of display order", check.key);
            last = check.order;
            assert!(
                crate::model::is_slug(check.key),
                "{} is not a slug",
                check.key
            );
            assert!(!check.label.is_empty(), "{} has no label", check.key);
            assert!(
                check.action_href.starts_with('/'),
                "{} has an action link that is not a path",
                check.key
            );
            assert!(
                !check.action_label.is_empty(),
                "{} would render a dead button",
                check.key
            );
        }
        assert_eq!(find("mfa_enforced").map(|c| c.order), Some(10));
        assert!(find("no_such_check").is_none());
    }

    #[test]
    fn an_absent_mfa_table_is_a_failure_not_an_unknown() {
        let env = Environment {
            mfa: Some(Probe::missing("the iam factor table")),
            ..Environment::unprobed()
        };
        let results = evaluate_all(&env, Uuid::nil());
        let mfa = results
            .iter()
            .find(|r| r.check_key == "mfa_enforced")
            .expect("mfa row");
        assert_eq!(
            mfa.state, "fail",
            "an absent factor table is a fact we can state"
        );
    }

    #[test]
    fn a_read_only_csp_is_a_warn_and_an_enforced_one_is_a_pass() {
        let base = Environment {
            csp: Some(Probe::value(serde_json::json!({
                "mode": "report_only",
                "directives": [{"name": "default-src", "values": ["'self'"]}],
            }))),
            ..Environment::unprobed()
        };
        let report = evaluate_all(&base, Uuid::nil());
        assert_eq!(
            report
                .iter()
                .find(|r| r.check_key == "csp_configured")
                .map(|r| r.state.as_str()),
            Some("warn"),
            "a report-only policy is not an applied one"
        );

        let enforced = Environment {
            csp: Some(Probe::value(serde_json::json!({
                "mode": "enforce",
                "directives": [{"name": "default-src", "values": ["'self'"]}],
            }))),
            ..base.clone()
        };
        let applied = evaluate_all(&enforced, Uuid::nil());
        assert_eq!(
            applied
                .iter()
                .find(|r| r.check_key == "csp_configured")
                .map(|r| r.state.as_str()),
            Some("pass")
        );
    }


    /// The shape the *writer* produces, and the reader that disagreed with it.
    ///
    /// The three tests above build the probe fact themselves, as `{"directives": [ … ]}`. The
    /// probe in `apps/api/src/routes/security.rs` builds it as `{"directives": <count>}` — it
    /// reads `HeaderPolicy::csp.len()` and reports the number, because a count is what a row
    /// shows. So the reader and the writer each had a shape the other had never seen, every real
    /// policy read as zero directives, and `csp_configured` sat at `fail` on a platform whose
    /// stored policy was the four-directive baseline. This test is the pair that closes it:
    /// it feeds the reader the writer's shape and requires the answer to be about the policy
    /// rather than about the encoding.
    ///
    /// It is a unit test and not a walk because the defect is entirely inside one function's
    /// input contract. The walk in `apps/api/tests/security.rs` is what *found* it; this is what
    /// keeps it fixed without a browser and a database.
    #[test]
    fn the_directive_count_is_read_as_the_writer_writes_it() {
        let enforced = Environment {
            csp: Some(Probe::value(serde_json::json!({
                "mode": "enforce",
                "directives": 4,
                "saved_at": "2026-09-26 10:00:00 +00:00:00",
            }))),
            ..Environment::unprobed()
        };
        let row = evaluate_all(&enforced, Uuid::nil())
            .into_iter()
            .find(|result| result.check_key == "csp_configured")
            .expect("the csp row must exist");
        assert_eq!(
            row.state, "pass",
            "four directives read from the writer's own shape must be four directives, not zero: \
             {}",
            row.detail
        );
        assert_eq!(
            row.detail["summary"],
            serde_json::json!("The content security policy is enforced (4 directives)"),
            "and the sentence an operator reads must name the real count: {}",
            row.detail
        );

        let empty = Environment {
            csp: Some(Probe::value(serde_json::json!({
                "mode": "enforce",
                "directives": 0,
            }))),
            ..Environment::unprobed()
        };
        assert_eq!(
            evaluate_all(&empty, Uuid::nil())
                .into_iter()
                .find(|result| result.check_key == "csp_configured")
                .map(|result| result.state),
            Some("fail".to_owned()),
            "a count of zero is still a failure, so the fix did not trade a wrong pass for a \
             wrong failure"
        );
    }

    #[test]
    fn an_empty_policy_fails_because_it_protects_nothing() {
        let env = Environment {
            csp: Some(Probe::value(serde_json::json!({
                "mode": "enforce",
                "directives": [],
            }))),
            ..Environment::unprobed()
        };
        let results = evaluate_all(&env, Uuid::nil());
        let csp = results
            .iter()
            .find(|r| r.check_key == "csp_configured")
            .expect("csp row");
        assert_eq!(csp.state, "fail");
    }

    #[test]
    fn a_backup_older_than_the_line_fails_and_the_line_is_the_one_the_detail_names() {
        let fresh = evaluate_all(
            &Environment {
                last_backup_hours: Some(3),
                ..Environment::unprobed()
            },
            Uuid::nil(),
        );
        assert_eq!(
            fresh
                .iter()
                .find(|r| r.check_key == "backup_healthy")
                .map(|r| r.state.as_str()),
            Some("pass")
        );

        let stale = evaluate_all(
            &Environment {
                last_backup_hours: Some(96),
                ..Environment::unprobed()
            },
            Uuid::nil(),
        );
        let row = stale
            .iter()
            .find(|r| r.check_key == "backup_healthy")
            .expect("backup row");
        assert_eq!(row.state, "fail");
        assert_eq!(json_of(row.detail.clone(), "stale_after_hours"), 48);
    }

    #[test]
    fn no_backup_ever_is_a_failure_and_no_report_ever_is_an_unknown() {
        // The asymmetry is the point: a missing backup is a fact about the world, a missing
        // scan is a fact about us. Collapsing them into one "unknown" would let a platform
        // with no backups read as merely unverified.
        let no_backup = evaluate_all(&Environment::unprobed(), Uuid::nil());
        assert_eq!(
            no_backup
                .iter()
                .find(|r| r.check_key == "backup_healthy")
                .map(|r| r.state.as_str()),
            Some("fail")
        );
        assert_eq!(
            no_backup
                .iter()
                .find(|r| r.check_key == "dependency_freshness")
                .map(|r| r.state.as_str()),
            Some("unknown")
        );
    }

    #[test]
    fn a_critical_open_finding_fails_the_overview_and_a_low_one_only_warns() {
        let critical = Environment {
            open_findings: BTreeMap::from([("critical".to_string(), 1), ("low".to_string(), 4)]),
            ..Environment::unprobed()
        };
        let rows = evaluate_all(&critical, Uuid::nil());
        assert_eq!(
            rows.iter()
                .find(|r| r.check_key == "open_findings")
                .map(|r| r.state.as_str()),
            Some("fail")
        );

        let low = Environment {
            open_findings: BTreeMap::from([("low".to_string(), 4)]),
            ..Environment::unprobed()
        };
        let rows = evaluate_all(&low, Uuid::nil());
        assert_eq!(
            rows.iter()
                .find(|r| r.check_key == "open_findings")
                .map(|r| r.state.as_str()),
            Some("warn")
        );
    }

    #[test]
    fn no_open_findings_is_a_pass() {
        let env = Environment {
            open_findings: BTreeMap::new(),
            ..Environment::unprobed()
        };
        let rows = evaluate_all(&env, Uuid::nil());
        assert_eq!(
            rows.iter()
                .find(|r| r.check_key == "open_findings")
                .map(|r| r.state.as_str()),
            Some("pass")
        );
    }

    #[test]
    fn an_unreadable_probe_is_unknown_with_its_reason_kept() {
        let env = Environment {
            https: Some(Probe::unreadable(
                "the proxy header is not set on this host",
            )),
            ..Environment::unprobed()
        };
        let rows = evaluate_all(&env, Uuid::nil());
        let row = rows
            .iter()
            .find(|r| r.check_key == "https_terminated")
            .expect("https row");
        assert_eq!(row.state, "unknown");
        assert_eq!(
            json_of(row.detail.clone(), "reason"),
            "the proxy header is not set on this host"
        );
    }

    #[test]
    fn the_overview_has_a_row_for_every_registered_check_even_with_no_results() {
        // A screen that lists only the checks that have run makes an unevaluated check look
        // like a check that does not exist.
        let rows = to_overview(&[], &Environment::unprobed(), Uuid::nil());
        assert_eq!(rows.len(), CHECKS.len());
        assert!(
            rows.iter()
                .all(|r| r.checked_at == time::OffsetDateTime::UNIX_EPOCH)
        );
        assert_eq!(unevaluated_state(), "unknown");
    }

    #[test]
    fn the_overview_prefers_a_stored_result_over_a_fresh_evaluation() {
        let stored = CheckResult {
            id: 7,
            organization_id: None,
            check_key: "https_terminated".into(),
            state: "pass".into(),
            detail: serde_json::json!({"summary": "from the store"}),
            run_id: Uuid::nil(),
            checked_at: time::OffsetDateTime::now_utc(),
        };
        let env = Environment {
            https: Some(Probe::missing("no certificate")),
            ..Environment::unprobed()
        };
        let rows = to_overview(std::slice::from_ref(&stored), &env, Uuid::nil());
        let row = rows
            .iter()
            .find(|r| r.check_key == "https_terminated")
            .expect("row");
        assert_eq!(
            row.state, "pass",
            "the stored result is what the panel showed last time"
        );
        assert_eq!(row.id, 7);
    }

    #[test]
    fn the_keys_helper_is_the_registry_in_display_order() {
        let listed = keys();
        let mut expected: Vec<&str> = CHECKS.iter().map(|c| c.key).collect();
        expected.sort_by_key(|k| find(k).map_or(0, |c| c.order));
        assert_eq!(listed, expected);
    }
}
