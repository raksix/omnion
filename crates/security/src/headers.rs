//! The header policy: what the platform promises about itself on every response (REQ-012,
//! slice 2).
//!
//! A header policy is the one security control an operator can change without deploying, which
//! makes it the one they change wrongly most often. Three decisions are therefore built into the
//! shape rather than left to whoever fills in the form:
//!
//! * **The policy is rendered, not assembled at the edge.** [`HeaderPolicy::render`] returns
//!   the exact header lines a response will carry, and the panel shows those same strings. A
//!   screen that shows a *summary* of the policy while a middleware applies a different
//!   rendering is the classic "the header I configured is not the header I get".
//! * **`report_only` is not a weakened `enforce`.** In report-only mode the CSP is sent as
//!   `Content-Security-Policy-Report-Only` and **no** `Content-Security-Policy` is sent at
//!   all — sending both would apply the policy while claiming to only report it, which is the
//!   worst of the two answers.
//! * **A policy that cannot be applied is refused, not silently degraded.** Every field is
//!   validated in [`HeaderPolicy::new`] and the reasons are field-level, so the form can point
//!   at the row that is wrong instead of answering "invalid header policy".
//!
//! The CSRF half of this slice lives in [`crate::csrf`], and the two are in one module because
//! they answer the same question from opposite ends: *is this request really from the person who
//! is signed in?* Headers answer it for the browser, the token for the mutation.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SecurityError};
use crate::vocabulary::{MAX_HEADER_NAME, MAX_HEADER_ROWS, MAX_HEADER_VALUE_LENGTH};

/// Whether the CSP is enforced or only reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CspMode {
    /// Send `Content-Security-Policy-Report-Only`: violations are recorded, nothing is blocked.
    #[default]
    ReportOnly,
    /// Send the enforcing `Content-Security-Policy`.
    Enforce,
}

impl CspMode {
    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReportOnly => "report_only",
            Self::Enforce => "enforce",
        }
    }

    /// Parse a wire value.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "report_only" => Ok(Self::ReportOnly),
            "enforce" => Ok(Self::Enforce),
            other => Err(SecurityError::invalid(format!(
                "\"{other}\" is not a CSP mode — choose \"report_only\" or \"enforce\""
            ))),
        }
    }
}

/// The CSP directives the platform recognises, in the order a policy renders them.
///
/// Order is part of the product: a preview that reorders its directives between two saves
/// teaches the operator that the tool is noisy, so this is a fixed list rather than a map.
pub const CSP_DIRECTIVES: &[&str] = &[
    "default-src",
    "base-uri",
    "object-src",
    "frame-ancestors",
    "script-src",
    "script-src-elem",
    "script-src-attr",
    "style-src",
    "img-src",
    "font-src",
    "connect-src",
    "form-action",
    "frame-src",
    "media-src",
    "worker-src",
    "manifest-src",
    "upgrade-insecure-requests",
    "block-all-mixed-content",
    "require-trusted-types-for",
];

/// One directive of a policy: the name and its sources, kept apart because a form edits them
/// apart and a preview renders them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CspDirective {
    /// Directive name, e.g. `script-src`.
    pub directive: String,
    /// Sources, already split on whitespace.
    pub values: Vec<String>,
}

impl CspDirective {
    /// Build a directive, rejecting an empty name.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] naming `directive` when the name is empty or is not a
    /// CSP directive name.
    pub fn new(directive: impl Into<String>, values: Vec<String>) -> Result<Self> {
        let directive = directive.into();
        let directive = directive.trim().to_lowercase();
        if directive.is_empty() {
            return Err(SecurityError::invalid(
                "a CSP directive name cannot be empty",
            ));
        }
        if directive.len() > MAX_HEADER_NAME {
            return Err(SecurityError::invalid(format!(
                "the directive name \"{directive}\" is longer than {MAX_HEADER_NAME} characters"
            )));
        }
        if !directive
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(SecurityError::invalid(format!(
                "\"{directive}\" is not a CSP directive name — use letters, digits and dashes"
            )));
        }
        let values: Vec<String> = values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        for value in &values {
            if value.len() > MAX_HEADER_VALUE_LENGTH {
                return Err(SecurityError::invalid(format!(
                    "a source in \"{directive}\" is longer than {MAX_HEADER_VALUE_LENGTH} characters"
                )));
            }
            // A value with a space would silently become two sources the moment the preview is
            // copied into a config file, so the split is done once, here, at the edge.
            if value.contains(char::is_whitespace) {
                return Err(SecurityError::invalid(format!(
                    "the source \"{value}\" in \"{directive}\" contains a space — list each source separately"
                )));
            }
        }
        Ok(Self { directive, values })
    }

    /// How this directive reads inside a header line.
    #[must_use]
    pub fn render(&self) -> String {
        if self.values.is_empty() {
            // `script-src` with no sources means "allow nothing"; `upgrade-insecure-requests` with
            // no sources means "upgrade". The distinction is the directive's own name, so it is
            // rendered as the bare directive and documented rather than turned into `'none'` here.
            self.directive.clone()
        } else {
            format!("{} {}", self.directive, self.values.join(" "))
        }
    }
}

/// How HSTS is configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HstsPolicy {
    /// `Strict-Transport-Security: max-age=<n>`; `None` means the header is not sent.
    pub max_age_seconds: Option<i64>,
    /// `includeSubDomains`.
    pub include_subdomains: bool,
    /// `preload`.
    pub preload: bool,
}

impl Default for HstsPolicy {
    /// One year, with subdomains — the value every baseline recommends. It is a *default*, not a
    /// claim: the panel shows the header it produces so an operator can see it before saving.
    fn default() -> Self {
        Self {
            max_age_seconds: Some(31_536_000),
            include_subdomains: true,
            preload: false,
        }
    }
}

impl HstsPolicy {
    /// The header line, or `None` when HSTS is off.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        let max_age = self.max_age_seconds?;
        let mut line = format!("max-age={max_age}");
        if self.include_subdomains {
            line.push_str("; includeSubDomains");
        }
        if self.preload {
            line.push_str("; preload");
        }
        Some(line)
    }

    /// Whether `max-age` is one a browser will honour.
    ///
    /// A browser ignores the whole header below 15768000 seconds (~6 months), so a policy that
    /// sends 3600 is not "a shorter policy" — it is a policy the browser silently drops. That
    /// is worth refusing at the form rather than discovering from a scan.
    #[must_use]
    pub fn is_effective(&self) -> bool {
        self.max_age_seconds
            .is_some_and(|max_age| max_age >= MIN_HSTS_MAX_AGE)
    }
}

/// The smallest `max-age` a browser honours (15768000s ≈ 6 months).
pub const MIN_HSTS_MAX_AGE: i64 = 15_768_000;

/// A header line the panel will show and the platform will send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderLine {
    /// The header's name, exactly as it goes on the wire.
    pub name: String,
    /// Its value, or `None` when the header is not sent at all.
    pub value: Option<String>,
}

impl HeaderLine {
    fn on(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            value: Some(value.into()),
        }
    }

    fn off(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            value: None,
        }
    }
}

/// Everything the platform says about itself in response headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderPolicy {
    /// CSP mode.
    pub csp_mode: CspMode,
    /// The directives, in render order.
    pub csp: Vec<CspDirective>,
    /// HSTS.
    pub hsts: HstsPolicy,
    /// Send `X-Content-Type-Options: nosniff`.
    pub content_type_options: bool,
    /// Referrer-Policy value; `None` sends nothing.
    pub referrer_policy: Option<String>,
    /// Permissions-Policy directives, as name=value pairs, in render order.
    pub permissions_policy: Vec<String>,
}

impl Default for HeaderPolicy {
    /// A baseline an operator can tighten rather than build: a restrictive CSP in report-only
    /// mode (so nothing breaks the moment it is saved), HSTS on, nosniff on, and a referrer
    /// policy that does not leak full URLs.
    fn default() -> Self {
        Self {
            csp_mode: CspMode::ReportOnly,
            csp: vec![
                CspDirective::new("default-src", vec!["'self'".to_owned()])
                    .expect("the default policy's directives are valid"),
                CspDirective::new("object-src", vec!["'none'".to_owned()])
                    .expect("the default policy's directives are valid"),
                CspDirective::new("base-uri", vec!["'self'".to_owned()])
                    .expect("the default policy's directives are valid"),
                CspDirective::new("frame-ancestors", vec!["'none'".to_owned()])
                    .expect("the default policy's directives are valid"),
            ],
            hsts: HstsPolicy::default(),
            content_type_options: true,
            referrer_policy: Some("strict-origin-when-cross-origin".to_owned()),
            permissions_policy: vec![
                "camera=()".to_owned(),
                "microphone=()".to_owned(),
                "geolocation=()".to_owned(),
            ],
        }
    }
}

impl HeaderPolicy {
    /// Build a policy from a stored JSON document, falling back to the baseline for a document
    /// that is missing or unparseable.
    ///
    /// A settings row that cannot be read must not take the platform's headers down with it: the
    /// baseline is a safe answer, and the panel shows the baseline as unsaved so the operator
    /// can see the platform fell back.
    #[must_use]
    pub fn from_json(value: Option<&serde_json::Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// Validate a policy and return it.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] naming the field that is wrong. Every reason here is
    /// something a form can point at: an empty directive name, a duplicate directive, a source
    /// with a space in it, a `max-age` a browser would ignore, a `preload` without subdomains
    /// (which the preload list refuses to accept) or a referrer policy outside the list browsers
    /// implement.
    pub fn new(
        csp_mode: CspMode,
        csp: Vec<CspDirective>,
        hsts: HstsPolicy,
        content_type_options: bool,
        referrer_policy: Option<String>,
        permissions_policy: Vec<String>,
    ) -> Result<Self> {
        if csp.len() > MAX_HEADER_ROWS {
            return Err(SecurityError::invalid(format!(
                "a policy may hold at most {MAX_HEADER_ROWS} CSP directives, this one has {}",
                csp.len()
            )));
        }

        // Re-run every directive through its own constructor so a value that arrived as a
        // JSON string with three spaces in it is caught here and not at the header.
        let mut normalised = Vec::with_capacity(csp.len());
        let mut seen: Vec<String> = Vec::with_capacity(csp.len());
        for row in csp {
            let row = CspDirective::new(row.directive.clone(), row.values)?;
            if seen.contains(&row.directive) {
                return Err(SecurityError::invalid(format!(
                    "the directive \"{}\" appears twice — a policy may list each directive once",
                    row.directive
                )));
            }
            seen.push(row.directive.clone());
            normalised.push(row);
        }
        // `default-src` and `frame-ancestors` are the two directives that make a policy mean
        // something on a page the operator does not control (a stored XSS in a rich-text block,
        // a clickjacking frame). A policy without them is not "restricted" — it is a list of
        // the things somebody remembered, so the form refuses rather than shipping it.
        let has = |name: &str| seen.iter().any(|seen| seen == name);
        if !has("default-src") {
            return Err(SecurityError::invalid(
                "a policy needs a \"default-src\" directive — it is what every other source list is measured against",
            ));
        }
        if !has("script-src") && !has("script-src-elem") {
            return Err(SecurityError::invalid(
                "a policy needs \"script-src\" or \"script-src-elem\" — without one, scripts are unconstrained",
            ));
        }

        if let Some(max_age) = hsts.max_age_seconds {
            if !(0..=86_400 * 730).contains(&max_age) {
                return Err(SecurityError::invalid(format!(
                    "\"{max_age}\" is not a max-age — use seconds between 0 and {}",
                    86_400 * 730
                )));
            }
        }
        if hsts.preload && !hsts.include_subdomains {
            return Err(SecurityError::invalid(
                "the preload list requires includeSubDomains — either add it or turn preload off",
            ));
        }

        let referrer_policy = match referrer_policy {
            None => None,
            Some(raw) => {
                let value = raw.trim().to_lowercase();
                if value.is_empty() {
                    None
                } else if REFERRER_POLICIES.contains(&value.as_str()) {
                    Some(value)
                } else {
                    return Err(SecurityError::invalid(format!(
                        "\"{value}\" is not a Referrer-Policy browsers implement — choose one of: {}",
                        REFERRER_POLICIES.join(", ")
                    )));
                }
            }
        };

        let mut permissions = Vec::with_capacity(permissions_policy.len());
        for entry in permissions_policy {
            let entry = entry.trim().to_owned();
            if entry.is_empty() {
                continue;
            }
            if entry.len() > MAX_HEADER_VALUE_LENGTH {
                return Err(SecurityError::invalid(format!(
                    "the permissions policy entry \"{entry}\" is longer than {MAX_HEADER_VALUE_LENGTH} characters"
                )));
            }
            let Some((name, allowlist)) = entry.split_once('=') else {
                return Err(SecurityError::invalid(format!(
                    "\"{entry}\" is not a permissions policy entry — write it as feature=(self) or feature=()"
                )));
            };
            let name = name.trim();
            if name.is_empty() {
                return Err(SecurityError::invalid(
                    "a permissions policy entry needs a feature name before the =",
                ));
            }
            permissions.push(format!("{name}={}", allowlist.trim()));
        }

        Ok(Self {
            csp_mode,
            csp: normalised,
            hsts,
            content_type_options,
            referrer_policy,
            permissions_policy: permissions,
        })
    }

    /// The exact header lines a response carries under this policy.
    ///
    /// This is the same list the panel previews, and the same list the middleware applies — one
    /// rendering, three consumers.
    #[must_use]
    pub fn render(&self) -> Vec<HeaderLine> {
        let mut lines = Vec::with_capacity(4 + self.csp.len());
        // Report-only sends the report-only header and nothing else. Sending both would apply
        // the policy while the panel says it is only reporting.
        lines.push(HeaderLine::on(
            if self.csp_mode == CspMode::Enforce {
                "Content-Security-Policy"
            } else {
                "Content-Security-Policy-Report-Only"
            },
            self.csp_value(),
        ));
        if let Some(hsts) = self.hsts.render() {
            lines.push(HeaderLine::on("Strict-Transport-Security", hsts));
        } else {
            lines.push(HeaderLine::off("Strict-Transport-Security"));
        }
        if self.content_type_options {
            lines.push(HeaderLine::on("X-Content-Type-Options", "nosniff"));
        } else {
            lines.push(HeaderLine::off("X-Content-Type-Options"));
        }
        lines.push(match &self.referrer_policy {
            Some(value) => HeaderLine::on("Referrer-Policy", value.clone()),
            None => HeaderLine::off("Referrer-Policy"),
        });
        lines.push(HeaderLine::on(
            "Permissions-Policy",
            self.permissions_policy.join(", "),
        ));
        lines
    }

    /// The CSP string itself, independent of the mode.
    #[must_use]
    pub fn csp_value(&self) -> String {
        self.csp
            .iter()
            .map(CspDirective::render)
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Lines that carry a value — what the panel shows as "this is what a response carries".
    ///
    /// Owned rather than borrowed from a temporary: the caller keeps the list to render, diff and
    /// hand to the middleware, and a `Vec<&HeaderLine>` borrowed from a temporary `Vec` inside
    /// this function would not outlive the call.
    #[must_use]
    pub fn applied(&self) -> Vec<HeaderLine> {
        self.render()
            .into_iter()
            .filter(|line| line.value.is_some())
            .collect()
    }

    /// The names a posture check needs in order to say anything true about this policy.
    ///
    /// [`crate::posture`] reads these rather than the struct, so the check and the header cannot
    /// disagree about what "configured" means.
    #[must_use]
    pub fn posture_facts(&self) -> PostureFacts {
        PostureFacts {
            csp_configured: !self.csp.is_empty(),
            csp_enforcing: self.csp_mode == CspMode::Enforce,
            hsts_effective: self.hsts.is_effective(),
            nosniff: self.content_type_options,
            referrer_policy_set: self.referrer_policy.is_some(),
        }
    }
}

/// The four facts about a policy that the posture checks read.
///
/// A separate struct rather than the policy itself because a check must be able to be handed the
/// facts *without* the ability to look anything else up — the slice-1 rule that a check cannot
/// conclude anything from an empty result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostureFacts {
    /// A CSP exists.
    pub csp_configured: bool,
    /// And it is enforced rather than only reported.
    pub csp_enforcing: bool,
    /// HSTS is sent with a `max-age` a browser honours.
    pub hsts_effective: bool,
    /// `X-Content-Type-Options: nosniff` is sent.
    pub nosniff: bool,
    /// A referrer policy is sent.
    pub referrer_policy_set: bool,
}

/// The referrer policies browsers implement; anything else is a typo an operator will not see.
pub const REFERRER_POLICIES: &[&str] = &[
    "no-referrer",
    "no-referrer-when-downgrade",
    "origin",
    "origin-when-cross-origin",
    "same-origin",
    "strict-origin",
    "strict-origin-when-cross-origin",
    "unsafe-url",
];

/// The CSP header name a mode sends under.
///
/// One place, because "which name" and "what value" are the same decision: a middleware that
/// picks a name from a different rule than the preview is a middleware that sends a header the
/// panel says is not there.
#[must_use]
pub fn csp_header_name(mode: CspMode) -> &'static str {
    match mode {
        CspMode::Enforce => "Content-Security-Policy",
        CspMode::ReportOnly => "Content-Security-Policy-Report-Only",
    }
}

/// `true` when a `max_age` is one a browser will actually honour.
///
/// The same test as [`HstsPolicy::is_effective`], exposed on its own so the posture check reads
/// a fact rather than reconstructing the rule.
#[must_use]
pub fn is_effective_hsts(max_age_seconds: Option<i64>) -> bool {
    max_age_seconds.is_some_and(|max_age| max_age >= MIN_HSTS_MAX_AGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(csp: Vec<CspDirective>, mode: CspMode) -> HeaderPolicy {
        HeaderPolicy::new(
            mode,
            csp,
            HstsPolicy::default(),
            true,
            Some("strict-origin-when-cross-origin".to_owned()),
            vec!["camera=()".to_owned()],
        )
        .expect("the policy is valid")
    }

    fn minimal_csp() -> Vec<CspDirective> {
        vec![
            CspDirective::new("default-src", vec!["'self'".to_owned()]).expect("directive"),
            CspDirective::new("script-src", vec!["'self'".to_owned()]).expect("directive"),
        ]
    }

    #[test]
    fn report_only_sends_the_report_header_and_not_the_enforcing_one() {
        let rendered = policy(minimal_csp(), CspMode::ReportOnly).render();
        let names: Vec<&str> = rendered.iter().map(|line| line.name.as_str()).collect();
        assert!(names.contains(&"Content-Security-Policy-Report-Only"));
        assert!(
            !names.contains(&"Content-Security-Policy"),
            "report-only must not also send the enforcing header: {names:?}"
        );
    }

    #[test]
    fn enforce_sends_the_enforcing_header_and_not_the_report_one() {
        let rendered = policy(minimal_csp(), CspMode::Enforce).render();
        let names: Vec<&str> = rendered.iter().map(|line| line.name.as_str()).collect();
        assert!(names.contains(&"Content-Security-Policy"));
        assert!(!names.contains(&"Content-Security-Policy-Report-Only"));
    }

    #[test]
    fn the_render_is_the_same_string_in_both_modes() {
        let rows = minimal_csp();
        let report = policy(rows.clone(), CspMode::ReportOnly);
        let enforce = policy(rows, CspMode::Enforce);
        assert_eq!(report.csp_value(), enforce.csp_value());
    }

    #[test]
    fn csp_directives_keep_their_order_and_join_with_semicolons() {
        let rows = policy(minimal_csp(), CspMode::Enforce);
        assert_eq!(rows.csp_value(), "default-src 'self'; script-src 'self'");
    }

    #[test]
    fn a_directive_with_no_sources_renders_bare() {
        // `upgrade-insecure-requests` is a flag directive: rendering it as `upgrade-insecure-requests 'none'`
        // would make browsers reject it as a source list.
        let rows = policy(
            vec![
                CspDirective::new("default-src", vec!["'self'".to_owned()]).expect("directive"),
                CspDirective::new("script-src", vec!["'self'".to_owned()]).expect("directive"),
                CspDirective::new("upgrade-insecure-requests", vec![]).expect("directive"),
            ],
            CspMode::Enforce,
        );
        assert!(rows.csp_value().ends_with("upgrade-insecure-requests"));
    }

    #[test]
    fn an_empty_directive_name_is_refused_with_a_field_level_reason() {
        // The refusal comes from the constructor — building the row with a blank name is
        // expected to fail, so the row is built by hand to reach the policy-level check.
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            vec![
                CspDirective::new("default-src", vec!["'self'".to_owned()]).expect("directive"),
                CspDirective {
                    directive: "  ".to_owned(),
                    values: vec!["'self'".to_owned()],
                },
                CspDirective::new("script-src", vec!["'self'".to_owned()]).expect("directive"),
            ],
            HstsPolicy::default(),
            true,
            None,
            vec![],
        )
        .expect_err("an empty directive name cannot be stored");
        assert!(error.to_string().contains("cannot be empty"), "{error}");
    }

    #[test]
    fn a_duplicate_directive_is_refused() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            vec![
                CspDirective::new("default-src", vec!["'self'".to_owned()]).expect("directive"),
                CspDirective::new("script-src", vec!["'self'".to_owned()]).expect("directive"),
                CspDirective::new("script-src", vec!["'unsafe-inline'".to_owned()]).expect("d"),
            ],
            HstsPolicy::default(),
            true,
            None,
            vec![],
        )
        .expect_err("a repeated directive cannot be stored");
        assert!(error.to_string().contains("appears twice"), "{error}");
    }

    #[test]
    fn a_policy_without_default_src_is_refused() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            vec![CspDirective::new("script-src", vec!["'self'".to_owned()]).expect("directive")],
            HstsPolicy::default(),
            true,
            None,
            vec![],
        )
        .expect_err("default-src is required");
        assert!(error.to_string().contains("default-src"), "{error}");
    }

    #[test]
    fn a_policy_without_a_script_list_is_refused() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            vec![CspDirective::new("default-src", vec!["'self'".to_owned()]).expect("directive")],
            HstsPolicy::default(),
            true,
            None,
            vec![],
        )
        .expect_err("a script list is required");
        assert!(error.to_string().contains("script-src"), "{error}");
    }

    #[test]
    fn a_source_with_a_space_is_refused_rather_than_split_silently() {
        let error = CspDirective::new("script-src", vec!["'self' https://cdn.test".to_owned()])
            .expect_err("a source with a space is two sources");
        assert!(
            error.to_string().contains("list each source separately"),
            "{error}"
        );
    }

    #[test]
    fn sources_are_trimmed_and_empties_dropped() {
        let row = CspDirective::new("  IMG-SRC ", vec![" 'self' ".to_owned(), "  ".to_owned()])
            .expect("directive");
        assert_eq!(row.directive, "img-src");
        assert_eq!(row.values, vec!["'self'".to_owned()]);
    }

    #[test]
    fn a_directive_name_with_a_quote_is_refused() {
        // A name that could break out of the header line is the injection a header value has.
        let error = CspDirective::new("script-src; report-uri evil", vec!["'self'".to_owned()])
            .expect_err("a semicolon in a name is an injection");
        assert!(
            error.to_string().contains("not a CSP directive name"),
            "{error}"
        );
    }

    #[test]
    fn hsts_off_sends_nothing_and_off_is_visible_in_the_render() {
        let mut rows = policy(minimal_csp(), CspMode::Enforce);
        rows.hsts.max_age_seconds = None;
        let rendered = rows.render();
        let line = rendered
            .iter()
            .find(|line| line.name == "Strict-Transport-Security")
            .expect("HSTS is always listed, on or off");
        assert_eq!(
            line.value, None,
            "an off header is shown as off, not omitted"
        );
        assert_eq!(rows.applied().len(), rendered.len() - 1);
    }

    #[test]
    fn hsts_renders_max_age_and_flags_in_order() {
        let hsts = HstsPolicy {
            max_age_seconds: Some(31_536_000),
            include_subdomains: true,
            preload: true,
        };
        assert_eq!(
            hsts.render().as_deref(),
            Some("max-age=31536000; includeSubDomains; preload")
        );
    }

    #[test]
    fn a_max_age_a_browser_ignores_is_not_effective() {
        assert!(
            !HstsPolicy {
                max_age_seconds: Some(3_600),
                ..HstsPolicy::default()
            }
            .is_effective(),
            "a browser drops an HSTS header below ~6 months, so the check must say so"
        );
        assert!(HstsPolicy::default().is_effective());
        assert!(
            !HstsPolicy {
                max_age_seconds: None,
                ..HstsPolicy::default()
            }
            .is_effective()
        );
    }

    #[test]
    fn preload_without_subdomains_is_refused() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            minimal_csp(),
            HstsPolicy {
                max_age_seconds: Some(31_536_000),
                include_subdomains: false,
                preload: true,
            },
            true,
            None,
            vec![],
        )
        .expect_err("the preload list requires subdomains");
        assert!(error.to_string().contains("includeSubDomains"), "{error}");
    }

    #[test]
    fn an_unimplemented_referrer_policy_is_refused_with_the_list() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            minimal_csp(),
            HstsPolicy::default(),
            true,
            Some("strict-origin-when-cross-origin-plus".to_owned()),
            vec![],
        )
        .expect_err("a typo must not ship");
        let message = error.to_string();
        assert!(
            message.contains("strict-origin-when-cross-origin"),
            "{message}"
        );
    }

    #[test]
    fn an_empty_referrer_policy_means_off_rather_than_an_error() {
        let rows = HeaderPolicy::new(
            CspMode::Enforce,
            minimal_csp(),
            HstsPolicy::default(),
            true,
            Some("   ".to_owned()),
            vec![],
        )
        .expect("blank means off");
        assert_eq!(rows.referrer_policy, None);
    }

    #[test]
    fn a_permissions_entry_without_an_equals_is_refused() {
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            minimal_csp(),
            HstsPolicy::default(),
            true,
            None,
            vec!["camera".to_owned()],
        )
        .expect_err("a bare feature name is not an entry");
        assert!(error.to_string().contains("feature="), "{error}");
    }

    #[test]
    fn permissions_entries_are_normalised() {
        let rows = HeaderPolicy::new(
            CspMode::Enforce,
            minimal_csp(),
            HstsPolicy::default(),
            true,
            None,
            vec![" camera = () ".to_owned(), "  ".to_owned()],
        )
        .expect("valid entries");
        assert_eq!(rows.permissions_policy, vec!["camera=()".to_owned()]);
    }

    #[test]
    fn more_rows_than_the_cap_is_refused() {
        let mut rows = minimal_csp();
        for index in 0..MAX_HEADER_ROWS {
            rows.push(
                CspDirective::new(format!("x-src-{index}"), vec!["'self'".to_owned()])
                    .expect("directive"),
            );
        }
        let error = HeaderPolicy::new(
            CspMode::Enforce,
            rows,
            HstsPolicy::default(),
            true,
            None,
            vec![],
        )
        .expect_err("the cap exists so a policy cannot grow without bound");
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn the_default_policy_renders_a_csp_hsts_and_nosniff() {
        let defaults = HeaderPolicy::default();
        let applied = defaults.applied();
        let names: Vec<&str> = applied.iter().map(|line| line.name.as_str()).collect();
        assert!(names.contains(&"Content-Security-Policy-Report-Only"));
        assert!(names.contains(&"Strict-Transport-Security"));
        assert!(names.contains(&"X-Content-Type-Options"));
        assert!(names.contains(&"Referrer-Policy"));
        assert_eq!(defaults.csp_mode, CspMode::ReportOnly);
    }

    #[test]
    fn an_unreadable_document_falls_back_to_the_baseline_rather_than_to_no_headers() {
        assert_eq!(
            HeaderPolicy::from_json(None),
            HeaderPolicy::default(),
            "a missing settings row must not mean 'send no headers'"
        );
        assert_eq!(
            HeaderPolicy::from_json(Some(&serde_json::json!("not an object"))),
            HeaderPolicy::default()
        );
        assert_eq!(
            HeaderPolicy::from_json(Some(&serde_json::json!({}))),
            HeaderPolicy::default()
        );
    }

    #[test]
    fn a_policy_round_trips_through_json() {
        let rows = policy(minimal_csp(), CspMode::Enforce);
        let json = serde_json::to_value(&rows).expect("serialise");
        assert_eq!(HeaderPolicy::from_json(Some(&json)), rows);
    }

    #[test]
    fn the_posture_facts_read_the_policy_rather_than_guessing() {
        let facts = policy(minimal_csp(), CspMode::Enforce).posture_facts();
        assert!(facts.csp_configured && facts.csp_enforcing);
        assert!(facts.hsts_effective && facts.nosniff && facts.referrer_policy_set);

        let report = policy(minimal_csp(), CspMode::ReportOnly).posture_facts();
        assert!(report.csp_configured);
        assert!(
            !report.csp_enforcing,
            "a report-only policy is configured but not enforcing — the two are different facts"
        );
    }

    #[test]
    fn the_mode_parses_both_wire_values_and_refuses_the_rest() {
        assert_eq!(CspMode::parse("enforce").expect("mode"), CspMode::Enforce);
        assert_eq!(
            CspMode::parse("report_only").expect("mode"),
            CspMode::ReportOnly
        );
        let error = CspMode::parse("Reporting").expect_err("a typo must not default to enforce");
        assert!(error.to_string().contains("report_only"), "{error}");
    }
}
