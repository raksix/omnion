//! Directory providers: LDAP and Active Directory (REQ-065, slice 1; docs/07-IAM.md §11).
//!
//! A directory is not a protocol provider, and pretending otherwise is where this design earns
//! its keep. OIDC and SAML *hand you* the identity — a signed token arrives and the work is
//! verifying it. A directory is a **live query interface somebody else operates**: you bind with
//! a service account, you search a base, you walk a group graph that can be a million edges deep,
//! and at any moment the server can be slow, unreachable, or serving a certificate for a
//! hostname you did not type. So this module is built around three ideas the protocol modules do
//! not have:
//!
//! * **A test is a list of steps, not a boolean.** [`test_steps`] walks DNS → TCP → TLS →
//!   bind → search and records each one separately. A half-configured directory fails at
//!   exactly one of them, and an operator who is told "connection failed" has nothing to act on;
//!   an operator who is told "the bind step was refused" does.
//! * **No secret ever touches a row.** The bind password is named by a reference and resolved
//!   from the environment by the caller, exactly as the client secret is for the protocol
//!   providers. [`DirectoryConfig`] is the non-secret half and is what gets serialized.
//! * **Nothing here performs a search yet.** Slice 1 is the registry and the connection test;
//!   the paged search, nested-group resolution and the sync runner are slice 4. What lives here
//!   is everything that must be *decided* before a search can exist: what a valid configuration
//!   looks like, what the depth cap is, and what the steps mean.

use serde_json::{Value, json};

use crate::error::Result;

/// A directory provider, as configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryConfig {
    /// `ldap` or `active_directory` — the two behaviours that differ are UPN matching and the
    /// account-disabled flag, so they are one enum rather than a boolean an operator can set
    /// backwards.
    pub kind: DirectoryKind,
    /// `ldap://` or `ldaps://` as typed. Kept verbatim because the port and the scheme are two
    /// independent mistakes and the test wants to report which one.
    pub host: String,
    /// StartTLS on a plaintext connection. Mutually exclusive with `ldaps://` in practice, and
    /// [`DirectoryConfig::validate`] refuses the combination rather than letting a caller open
    /// a port 389 connection it believes it upgraded.
    pub start_tls: bool,
    /// The DN a service account binds as, e.g. `cn=omnion,ou=svc,dc=example,dc=com`.
    pub bind_dn: String,
    /// **A reference, never a value.** The environment variable holding the bind password.
    pub bind_secret_ref: String,
    /// The subtree the search runs in.
    pub base_dn: String,
    /// The filter that selects a person, e.g. `(objectClass=person)`. The user's login name is
    /// substituted into `{username}`; a filter with no placeholder is refused, because it would
    /// return the whole directory for every sign-in.
    pub user_filter: String,
    /// The filter that selects a group. Same placeholder rule.
    pub group_filter: Option<String>,
    /// How deep nested group resolution goes.
    ///
    /// A cap is not a limitation, it is the only thing that makes the feature safe: AD's
    /// `memberOf` is not transitive, so "the groups of a user" means walking the graph, and an
    /// unbounded walk on a cyclic or wide graph is a denial of service this module would be
    /// handing to a stranger. Ten is well past any real corporate structure.
    pub group_depth_cap: u8,
    /// The largest number of subjects one run will touch before it stops and says so.
    pub subject_cap: u32,
    /// Reject a certificate whose name does not match the host.
    pub verify_tls: bool,
    /// Accept a certificate the platform does not trust. Off unless somebody has a reason, and
    /// refused outright when combined with `ldaps://` on a host that is an IP literal.
    pub allow_insecure: bool,
    /// Page size for the search. AD's default max page size is 1000 and asking for more is
    /// refused by the server, so the default sits under it.
    pub page_size: u16,
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        Self {
            kind: DirectoryKind::Ldap,
            host: String::new(),
            start_tls: false,
            bind_dn: String::new(),
            bind_secret_ref: String::new(),
            base_dn: String::new(),
            user_filter: "(objectClass=person)".to_owned(),
            group_filter: Some("(objectClass=group)".to_owned()),
            group_depth_cap: DEFAULT_GROUP_DEPTH_CAP,
            subject_cap: DEFAULT_SUBJECT_CAP,
            verify_tls: true,
            allow_insecure: false,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

/// Default cap on nested group resolution. See [`DirectoryConfig::group_depth_cap`].
pub const DEFAULT_GROUP_DEPTH_CAP: u8 = 10;
/// Default cap on subjects per sync run. High enough for a mid-size company, low enough that a
/// runaway filter is stopped rather than followed.
pub const DEFAULT_SUBJECT_CAP: u32 = 5_000;
/// Default LDAP page size, under AD's server-side ceiling of 1000.
pub const DEFAULT_PAGE_SIZE: u16 = 500;
/// Above this a page is not a page, it is a memory problem.
pub const MAX_PAGE_SIZE: u16 = 5_000;
/// Above this the depth cap stops being a cap.
pub const MAX_GROUP_DEPTH: u8 = 32;
/// Hard ceiling on subjects per run, independent of the configured value.
pub const MAX_SUBJECT_CAP: u32 = 500_000;

impl DirectoryConfig {
    /// Read a configuration out of a provider's `config` JSON.
    ///
    /// Every field is optional and falls back to the default rather than erroring: a provider
    /// written before this module existed has a config with none of these keys, and a registry
    /// that cannot *read* its own rows is worse than one that shows a default.
    pub fn from_value(value: &Value) -> Result<Self> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
        };
        let number = |key: &str| value.get(key).and_then(Value::as_u64);
        let flag = |key: &str| value.get(key).and_then(Value::as_bool);

        let kind = match text("directory_kind").as_deref() {
            Some("active_directory") => DirectoryKind::ActiveDirectory,
            _ => DirectoryKind::Ldap,
        };

        Ok(Self {
            kind,
            host: text("host").unwrap_or_default(),
            start_tls: flag("start_tls").unwrap_or(false),
            bind_dn: text("bind_dn").unwrap_or_default(),
            bind_secret_ref: text("bind_secret_ref").unwrap_or_default(),
            base_dn: text("base_dn").unwrap_or_default(),
            user_filter: text("user_filter").unwrap_or_else(|| "(objectClass=person)".to_owned()),
            group_filter: Some(
                text("group_filter").unwrap_or_else(|| "(objectClass=group)".to_owned()),
            ),
            group_depth_cap: number("group_depth_cap").map_or(DEFAULT_GROUP_DEPTH_CAP, |value| {
                value.min(u64::from(MAX_GROUP_DEPTH)) as u8
            }),
            subject_cap: number("subject_cap").map_or(DEFAULT_SUBJECT_CAP, |value| {
                value.min(u64::from(MAX_SUBJECT_CAP)) as u32
            }),
            verify_tls: flag("verify_tls").unwrap_or(true),
            allow_insecure: flag("allow_insecure").unwrap_or(false),
            page_size: number("page_size").map_or(DEFAULT_PAGE_SIZE, |value| {
                value.min(u64::from(MAX_PAGE_SIZE)) as u16
            }),
        })
    }

    /// Serialize back into the `config` JSON a provider row stores.
    ///
    /// Only the non-secret half is written, by construction: this struct has no field that
    /// could hold the bind password, so a round trip through it cannot leak one.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "directory_kind": self.kind.as_str(),
            "host": self.host,
            "start_tls": self.start_tls,
            "bind_dn": self.bind_dn,
            "bind_secret_ref": self.bind_secret_ref,
            "base_dn": self.base_dn,
            "user_filter": self.user_filter,
            "group_filter": self.group_filter,
            "group_depth_cap": self.group_depth_cap,
            "subject_cap": self.subject_cap,
            "verify_tls": self.verify_tls,
            "allow_insecure": self.allow_insecure,
            "page_size": self.page_size,
        })
    }

    /// Every reason this configuration cannot work, in the order an operator fixes them.
    ///
    /// A `Vec` rather than the first error, because the wizard shows the list under the fields:
    /// making somebody resubmit to discover the next problem is the reason wizards like this are
    /// abandoned halfway. Ordering is deliberate — the earlier a mistake is found, the cheaper
    /// it is to fix, so the order is the order the answers come in.
    pub fn validate(&self) -> Vec<ConfigProblem> {
        let mut problems = Vec::new();
        let host = self.host.trim();
        let secure = host.starts_with("ldaps://");

        if host.is_empty() {
            problems.push(ConfigProblem::new(
                "host",
                "the directory host is required",
                Problem::Missing,
            ));
        } else if !host.starts_with("ldap://") && !host.starts_with("ldaps://") {
            problems.push(ConfigProblem::new(
                "host",
                "the host must start with ldap:// or ldaps://",
                Problem::Invalid,
            ));
        } else if !self.validate_host_shape() {
            problems.push(ConfigProblem::new(
                "host",
                "the host is not `scheme://host[:port]`",
                Problem::Invalid,
            ));
        } else if let Some(port) = self.explicit_port()
            && !(1..=65_535).contains(&port)
        {
            problems.push(ConfigProblem::new(
                "host",
                "the port must be between 1 and 65535",
                Problem::Invalid,
            ));
        }

        // `ldaps://` and StartTLS are two answers to one question. Accepting both means a
        // connection the operator believes is encrypted and the server sees as plaintext.
        if secure && self.start_tls {
            problems.push(ConfigProblem::new(
                "start_tls",
                "an ldaps:// connection is already encrypted, so StartTLS must be off",
                Problem::Conflict,
            ));
        }

        if self.bind_dn.trim().is_empty() {
            problems.push(ConfigProblem::new(
                "bind_dn",
                "a service account DN is required to search the directory",
                Problem::Missing,
            ));
        } else if !self.bind_dn.trim().ends_with('=') && !self.bind_dn.contains('=') {
            problems.push(ConfigProblem::new(
                "bind_dn",
                "a bind DN looks like `cn=omnion,ou=svc,dc=example,dc=com`",
                Problem::Invalid,
            ));
        }

        // A reference is an environment variable name, so it is upper case by construction —
        // which is exactly what makes a pasted *password* visible as a mistake instead of a
        // silent "the bind step failed" three steps later.
        let reference = self.bind_secret_ref.trim();
        if reference.is_empty() {
            problems.push(ConfigProblem::new(
                "bind_secret_ref",
                "name the environment variable holding the bind password",
                Problem::Missing,
            ));
        } else if !reference
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            problems.push(ConfigProblem::new(
                "bind_secret_ref",
                "this is the NAME of an environment variable, so it may only hold A–Z, 0–9 and \
                 underscores — the password itself is never stored here",
                Problem::Invalid,
            ));
        }

        if self.base_dn.trim().is_empty() {
            problems.push(ConfigProblem::new(
                "base_dn",
                "a base DN is required so the search has a subtree to run in",
                Problem::Missing,
            ));
        }

        if self.user_filter.trim().is_empty() {
            problems.push(ConfigProblem::new(
                "user_filter",
                "a user filter is required",
                Problem::Missing,
            ));
        } else if !self.user_filter.contains(USERNAME_PLACEHOLDER) {
            // Without the placeholder the filter is constant, so every sign-in returns every
            // person in the directory and the first match wins. That is not a slow sign-in,
            // that is the wrong person getting in.
            problems.push(ConfigProblem::new(
                "user_filter",
                format!(
                    "the user filter must contain `{USERNAME_PLACEHOLDER}`, otherwise every \
                         sign-in would match every person in the directory"
                ),
                Problem::Invalid,
            ));
        } else if let Some(message) = unbalanced_filter(&self.user_filter) {
            problems.push(ConfigProblem::new("user_filter", message, Problem::Invalid));
        }

        if let Some(filter) = self.group_filter.as_deref()
            && let Some(message) = unbalanced_filter(filter)
        {
            problems.push(ConfigProblem::new(
                "group_filter",
                message,
                Problem::Invalid,
            ));
        }

        if self.group_depth_cap == 0 {
            problems.push(ConfigProblem::new(
                "group_depth_cap",
                "a depth cap of 0 would resolve no groups at all; use 1 for direct membership only",
                Problem::Invalid,
            ));
        }
        if self.subject_cap == 0 {
            problems.push(ConfigProblem::new(
                "subject_cap",
                "a subject cap of 0 would sync nobody",
                Problem::Invalid,
            ));
        }
        if self.page_size == 0 {
            problems.push(ConfigProblem::new(
                "page_size",
                "a page size of 0 would return no entries at all",
                Problem::Invalid,
            ));
        }

        // Turning certificate verification off is a real answer on a private CA nobody has
        // installed. Doing it against a bare IP is not: there is no name to match, so there is
        // nothing left to verify and the connection is encrypted to whoever answered.
        if self.allow_insecure && self.verify_tls {
            problems.push(ConfigProblem::new(
                "allow_insecure",
                "allow_insecure is only meaningful with verify_tls turned off",
                Problem::Conflict,
            ));
        }
        if self.allow_insecure && self.host_is_ip_literal() {
            problems.push(ConfigProblem::new(
                "allow_insecure",
                "an IP address has no name to match a certificate against, so an insecure \
                 connection to one cannot be made safe",
                Problem::Conflict,
            ));
        }

        problems
    }

    /// The host with the scheme stripped — what a resolver is actually asked for.
    #[must_use]
    pub fn hostname(&self) -> &str {
        let (host, _) = split_host_port(self.host.trim());
        host
    }

    /// The port, defaulted by scheme: 636 for `ldaps://`, 389 otherwise.
    #[must_use]
    pub fn port(&self) -> u16 {
        match self.explicit_port() {
            Some(port) => u16::try_from(port).unwrap_or(if self.is_secure() { 636 } else { 389 }),
            None if self.is_secure() => 636,
            None => 389,
        }
    }

    /// Whether the connection is encrypted on the wire, by scheme or by StartTLS.
    #[must_use]
    pub fn is_secure(&self) -> bool {
        self.start_tls || self.host.starts_with("ldaps://")
    }

    /// Replace `{username}` in the user filter and escape the value.
    ///
    /// The escaping is the point of this existing. An LDAP filter is not SQL, but it has the
    /// same injection shape: `*`, `(`, `)`, `\` and NUL are metacharacters, and a login name
    /// containing them changes which entries the filter selects. `*` is escaped too, so a name
    /// of `*` cannot become a wildcard that matches the directory.
    #[must_use]
    pub fn user_filter_for(&self, username: &str) -> String {
        self.user_filter
            .replace(USERNAME_PLACEHOLDER, &escape_filter_value(username))
    }

    /// Substitute the login name into the group filter, if one is configured.
    #[must_use]
    pub fn group_filter_for(&self, username: &str) -> Option<String> {
        self.group_filter
            .as_ref()
            .map(|filter| filter.replace(USERNAME_PLACEHOLDER, &escape_filter_value(username)))
    }

    /// The login attribute this kind matches on.
    ///
    /// AD answers to a user principal name, an `sAMAccountName` or a mail address and treats
    /// them differently; plain LDAP has one `uid`. A sign-in form that guesses wrong here gives
    /// "no such user" to a person who exists, which is the single most common directory bug.
    #[must_use]
    pub const fn login_attribute(&self) -> &'static str {
        match self.kind {
            DirectoryKind::Ldap => "uid",
            DirectoryKind::ActiveDirectory => "userPrincipalName",
        }
    }

    /// The attribute an AD account-disabled flag arrives in. `None` for plain LDAP, which has no
    /// such concept — a directory that has one must say so itself.
    #[must_use]
    pub const fn disabled_attribute(&self) -> Option<&'static str> {
        match self.kind {
            DirectoryKind::Ldap => None,
            DirectoryKind::ActiveDirectory => Some("userAccountControl"),
        }
    }

    /// Whether an AD `userAccountControl` value means the account is disabled.
    ///
    /// Bit 1 (0x2) is `ACCOUNTDISABLE`; every other bit is somebody else's business, so the
    /// test is a mask rather than an equality. A directory that reported the whole number as
    /// "disabled or not" would disable every account on the planet.
    #[must_use]
    pub const fn ad_account_disabled(value: i64) -> bool {
        value & 0x2 == 0x2
    }

    /// Whether a bind DN sits at or under a base DN.
    ///
    /// Used to refuse a configuration that can read the whole tree: a service account outside
    /// the subtree an operator declared is either a mistake or a wider grant than intended, and
    /// both deserve a word before the first sign-in.
    #[must_use]
    pub fn bind_within_base(&self) -> bool {
        let bind = normalize_dn(&self.bind_dn);
        let base = normalize_dn(&self.base_dn);
        !base.is_empty() && (bind == base || bind.ends_with(&format!(",{base}")))
    }

    fn validate_host_shape(&self) -> bool {
        let host = self.host.trim();
        let rest = host
            .strip_prefix("ldaps://")
            .or_else(|| host.strip_prefix("ldap://"))
            .unwrap_or_default();
        !rest.is_empty() && !rest.contains('/') && !rest.contains(' ')
    }

    fn explicit_port(&self) -> Option<u32> {
        split_host_port(self.host.trim()).1?.parse().ok()
    }
    fn host_is_ip_literal(&self) -> bool {
        let host = self.hostname();
        if host.starts_with('[') {
            return true;
        }
        host.split('.').count() == 4
            && host
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    }
}

/// Split `scheme://host[:port]` into its host and its *written* port.
///
/// A colon inside the bracketed form of an IPv6 literal is not a port separator, and treating it
/// as one produces a hostname of `[2001:db8::1` and a port of `1]`. That is why the bracket is
/// checked before the last colon is taken rather than after: by the time the string is cut it is
/// no longer possible to tell which colon meant what.
fn split_host_port(url: &str) -> (&str, Option<&str>) {
    let rest = url
        .strip_prefix("ldaps://")
        .or_else(|| url.strip_prefix("ldap://"))
        .unwrap_or(url);

    // A bracketed literal keeps every colon it contains; only a colon *after* the closing
    // bracket is a port.
    if let Some(close) = rest.find(']') {
        let host = &rest[..=close];
        let after = rest[close + 1..].strip_prefix(':');
        return (host, after);
    }
    match rest.rfind(':') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    }
}

/// Which directory it is. One enum rather than a boolean, because the two differ in three
/// places and a boolean gets one of them right at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryKind {
    /// Plain LDAP: one `uid` attribute, no account-disabled flag.
    Ldap,
    /// Active Directory: UPN / `sAMAccountName` matching and `userAccountControl`.
    ActiveDirectory,
}

impl DirectoryKind {
    /// The string the database and the API store.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ldap => "ldap",
            Self::ActiveDirectory => "active_directory",
        }
    }

    /// Whether this kind syncs a user and group graph on a schedule. Both do; the difference is
    /// in *what* it resolves, not whether it resolves anything.
    #[must_use]
    pub const fn syncs(self) -> bool {
        true
    }
}

/// The placeholder a user filter must contain.
pub const USERNAME_PLACEHOLDER: &str = "{username}";

// ---------------------------------------------------------------------------------------------
// The connection test
// ---------------------------------------------------------------------------------------------

/// One step of the connection test.
///
/// A test is a list of these, in the order they run, and a failure names the step that failed
/// rather than collapsing the whole thing into a message. The alternative — one boolean — is
/// what a connection helper has always returned, and it is the reason configuring a directory
/// is the part of enterprise sign-in everybody delegates to a contractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStep {
    /// The host name resolves to at least one address.
    Dns,
    /// A TCP connection to the host and port is accepted.
    Tcp,
    /// TLS negotiates, and the certificate matches when verification is on.
    Tls,
    /// The service account authenticates.
    Bind,
    /// A search in the base returns the expected shape.
    Search,
    /// The required attributes are present on what came back.
    Attributes,
}

impl TestStep {
    /// Every step, in the order [`test_steps`] runs them.
    pub const ALL: [Self; 6] = [
        Self::Dns,
        Self::Tcp,
        Self::Tls,
        Self::Bind,
        Self::Search,
        Self::Attributes,
    ];

    /// The machine name, for the panel's step list.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Tcp => "tcp",
            Self::Tls => "tls",
            Self::Bind => "bind",
            Self::Search => "search",
            Self::Attributes => "attributes",
        }
    }
}

/// The outcome of one step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StepReport {
    /// Which step this is.
    pub step: TestStep,
    /// `pending` until the step runs, then `ok` or `failed`.
    pub status: &'static str,
    /// A sentence the panel shows next to the step. Never a raw server response: a directory
    /// error string routinely contains the base DN, the filter and sometimes the bind DN.
    pub detail: String,
}

impl StepReport {
    /// A `pending` row, for the steps after the one that failed. Public because the live ladder
    /// builds the same shape: a report that is not `ok` is not `pending` by accident, it is
    /// pending because nothing has been asked of the server yet.
    #[must_use]
    pub fn pending_for(step: TestStep) -> Self {
        Self::pending(step)
    }

    fn pending(step: TestStep) -> Self {
        Self {
            step,
            status: "pending",
            detail: String::new(),
        }
    }

    fn done(step: TestStep, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            step,
            status,
            detail: detail.into(),
        }
    }
}

/// What the caller saw when the test ran.
///
/// Slice 1 does not open a socket — the network half lands with the search runner in slice 4, and
/// a test that cannot reach anything must say *which* part of the configuration it could not
/// even get as far as. So the answer here is: the steps, the configuration problems, and a clear
/// `pending` on the steps that need a live server.
#[derive(Debug, Clone, PartialEq)]
pub struct TestOutcome {
    /// One of three values, and the difference between them is the whole point:
    ///
    /// * `ok` — every step passed, including the ones that needed the server.
    /// * `incomplete` — the configuration is sound but nothing has been asked of the server yet.
    /// * `failed` — a configuration problem, named against the field that owns it.
    ///
    /// Collapsing `incomplete` into `ok` is the bug this type exists to prevent: a registry that
    /// calls an untested directory "working" is a registry whose `Last test` column is a lie, and
    /// an enable gate built on that lie lets an unreachable directory be switched on.
    pub status: &'static str,
    /// Every step, `pending` ones included, so the panel can render the whole ladder and grey
    /// out the part that has not been tried rather than hiding it.
    pub steps: Vec<StepReport>,
    /// Configuration problems, each attached to the field it belongs to.
    pub problems: Vec<ConfigProblem>,
    /// Whether the server answered. `None` in slice 1, and the reason is in the code.
    pub reached_server: Option<bool>,
}

impl TestOutcome {
    /// Whether the provider may be enabled: a test that reached the server **and** every step
    /// passing. A clean form is not a passing test, which is why this is not `status == "ok"`
    /// alone.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == "ok" && self.reached_server == Some(true)
    }

    /// The first failure, or the first step that has not been tried. Both are "where did this
    /// stop", which is the one question the ladder exists to answer.
    #[must_use]
    pub fn failing_step(&self) -> Option<TestStep> {
        self.steps
            .iter()
            .find(|step| step.status == "failed")
            .or_else(|| self.steps.iter().find(|step| step.status == "pending"))
            .map(|step| step.step)
    }
}

/// Why a configuration problem exists, which the panel turns into a colour and an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    /// A required field is empty.
    Missing,
    /// A field is present but unusable.
    Invalid,
    /// Two fields contradict each other.
    Conflict,
}

/// One configuration problem, attached to the field that owns it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigProblem {
    /// The form field, so the wizard can underline the right input.
    pub field: &'static str,
    /// What is wrong, in a sentence an operator can act on.
    pub message: String,
    /// Which kind of problem it is.
    pub kind: Problem,
}

impl ConfigProblem {
    fn new(field: &'static str, message: impl Into<String>, kind: Problem) -> Self {
        Self {
            field,
            message: message.into(),
            kind,
        }
    }
}

/// Run the connection test's *decidable* half.
///
/// The steps that need a live server stay `pending` in this slice; the configuration ladder is
/// walked first, so a provider with three field problems is reported as three field problems
/// rather than as a connection failure caused by the first of them.
#[must_use]
pub fn test_steps(config: &DirectoryConfig) -> TestOutcome {
    let problems = config.validate();

    // A TLS step only exists when the connection is encrypted. A plaintext LDAP directory has
    // no TLS to negotiate, and showing a greyed-out "TLS" step forever would read as a problem
    // that never resolves.
    let steps: Vec<TestStep> = TestStep::ALL
        .iter()
        .copied()
        .filter(|step| {
            *step != TestStep::Tls
                || config.is_secure()
                || problems.is_empty() && config.is_secure()
        })
        .collect();

    let mut reports = steps
        .into_iter()
        .map(StepReport::pending)
        .collect::<Vec<_>>();

    // The first problem decides the first step that can even be attempted: there is no point
    // resolving a name when there is no name.
    let mut blocked_at: Option<TestStep> = None;
    for problem in &problems {
        let step = match problem.field {
            "host" => TestStep::Dns,
            "bind_dn" | "bind_secret_ref" => TestStep::Bind,
            "base_dn" | "user_filter" | "group_filter" => TestStep::Search,
            _ => continue,
        };
        if let Some(position) = reports.iter().position(|report| report.step == step) {
            reports[position] = StepReport::done(step, "failed", problem.message.clone());
            blocked_at.get_or_insert(step);
        }
    }

    let status = if !problems.is_empty() {
        "failed"
    } else {
        // Sound form, nothing asked of the server. `incomplete`, not `ok`: a registry that calls
        // an untested directory "working" cannot build an honest enable gate on top of it.
        "incomplete"
    };
    TestOutcome {
        status,
        steps: reports,
        problems,
        reached_server: None,
    }
}

/// Escape a value for an LDAP filter (RFC 4515 §3).
///
/// A filter is a string, but five characters inside it are structure, and a login name
/// containing them changes what the filter selects. `*` is escaped as well: without it, a name
/// of `*` is the wildcard and every sign-in would match every person in the subtree.
#[must_use]
pub fn escape_filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '*' | '(' | ')' | '\\' | '\0') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Whether an RFC 4515 filter has balanced parentheses, and no premature closing bracket.
///
/// Written as a scan rather than a count: `(a)(b)` has the same count of open and close as
/// `(a(b)` is broken, and only the scan notices.
#[must_use]
pub fn unbalanced_filter(filter: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut escaped = false;
    for character in filter.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Some("the filter has a `)` without a matching `(`".to_owned());
                }
            }
            _ => {}
        }
    }
    if escaped {
        return Some("the filter ends with a trailing `\\`".to_owned());
    }
    if depth > 0 {
        return Some("the filter has `(` without a matching `)`".to_owned());
    }
    None
}

/// Compare DNs structurally rather than textually.
///
/// A DN is a sequence of RDNs, and `CN=Omnion, OU=Svc, DC=example, DC=com` and
/// `cn=omnion,ou=svc,dc=example,dc=com` name the same entry — directory servers do not promise
/// to echo back the case an operator typed. Comparing the raw strings would refuse a service
/// account that works, which is the kind of "misconfiguration" that costs a day.
#[must_use]
pub fn normalize_dn(dn: &str) -> String {
    dn.split(',')
        .filter_map(|rdn| {
            let (key, value) = rdn.split_once('=')?;
            // Escaped separators are data, not structure: `cn=Doe\, Jane` is one RDN whose value
            // contains a comma, and splitting it in half invents a second one.
            let value = value
                .split('\\')
                .next()
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            Some(format!("{}={}", key.trim().to_lowercase(), value))
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration that is valid in every respect, so a test can spoil exactly one field.
    fn valid() -> DirectoryConfig {
        DirectoryConfig {
            host: "ldaps://dir.example.com".to_owned(),
            bind_dn: "cn=omnion,ou=svc,dc=example,dc=com".to_owned(),
            bind_secret_ref: "OMNION_LDAP_BIND_PASSWORD".to_owned(),
            base_dn: "ou=people,dc=example,dc=com".to_owned(),
            user_filter: "(uid={username})".to_owned(),
            ..DirectoryConfig::default()
        }
    }

    #[test]
    fn a_well_formed_configuration_has_no_problems() {
        assert_eq!(valid().validate(), Vec::new());
    }

    /// The round trip that keeps a secret out of a row is worth more than any single field check.
    #[test]
    fn the_configuration_never_holds_a_password() {
        let config = valid();
        let serialized = config.to_value();
        let keys = serialized
            .as_object()
            .expect("the config is an object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();

        for suspicious in ["password", "bind_password", "secret", "credential", "token"] {
            assert!(
                !keys.contains(&suspicious),
                "`{suspicious}` must not be a field of the directory configuration"
            );
        }
        // The reference *name* survives the round trip, because the sign-in path needs it.
        assert_eq!(serialized["bind_secret_ref"], "OMNION_LDAP_BIND_PASSWORD");

        let restored = DirectoryConfig::from_value(&serialized).expect("our own output reads back");
        assert_eq!(restored, config);
    }

    #[test]
    fn a_config_written_before_this_module_existed_still_reads() {
        // A provider row from REQ-006 carries none of these keys. A registry that cannot read
        // its own rows would show an empty screen for every directory an operator had added.
        let config = DirectoryConfig::from_value(&json!({ "issuer": "https://idp.example" }))
            .expect("unknown keys are ignored");
        assert_eq!(config, DirectoryConfig::default());
        assert!(
            !config.validate().is_empty(),
            "a blank directory is not a valid directory"
        );
    }

    #[test]
    fn the_user_filter_must_carry_the_placeholder() {
        let config = DirectoryConfig {
            user_filter: "(objectClass=person)".to_owned(),
            ..valid()
        };
        let problems = config.validate();
        assert!(
            problems
                .iter()
                .any(|problem| problem.field == "user_filter"),
            "a constant filter would match every person in the directory: {problems:?}"
        );
    }

    /// A login name is attacker-controlled. This is the one test in the module that is a
    /// security claim rather than a shape claim.
    #[test]
    fn a_login_name_cannot_rewrite_the_filter() {
        let config = valid();
        let hostile = "admin)(objectClass=*";
        let rendered = config.user_filter_for(hostile);
        // Every metacharacter in the name is escaped, so the name contributes exactly one
        // attribute value and cannot close the filter to add a second clause.
        assert_eq!(rendered, r"(uid=admin\)\(objectClass=\*)");
        assert_eq!(
            unbalanced_filter(&rendered),
            None,
            "the escaped filter must still be a well-formed filter"
        );
        assert_ne!(rendered, config.user_filter_for("admin"));

        // A lone `*` must not become a wildcard.
        assert_eq!(config.user_filter_for("*"), r"(uid=\*)");
        // A backslash is data too: unescaped, it would escape the quote that follows it.
        assert_eq!(config.user_filter_for("a\\b"), r"(uid=a\\b)");
    }

    #[test]
    fn an_unbalanced_filter_is_reported_with_its_reason() {
        assert_eq!(
            unbalanced_filter("(uid={username})"),
            None,
            "a balanced filter is not a problem"
        );
        assert!(
            unbalanced_filter("(uid=x").is_some(),
            "an unclosed bracket is refused"
        );
        assert!(
            unbalanced_filter("uid=x)").is_some(),
            "a stray close is refused"
        );
        assert!(
            unbalanced_filter("(uid=\\)").is_some(),
            "a trailing escape is refused"
        );
        // Same number of opens and closes, still broken: only the scan sees this.
        assert!(
            unbalanced_filter("(a)(b(").is_some(),
            "counting parentheses would call this balanced"
        );
    }

    #[test]
    fn ldaps_and_start_tls_are_two_answers_to_one_question() {
        let config = DirectoryConfig {
            start_tls: true,
            ..valid()
        };
        let problems = config.validate();
        assert!(
            problems.iter().any(|problem| {
                problem.field == "start_tls" && problem.kind == Problem::Conflict
            }),
            "an operator who believes a port 389 connection is encrypted is worse off than one \
             who gets an error: {problems:?}"
        );
    }

    #[test]
    fn a_pasted_password_in_the_reference_field_is_refused() {
        let config = DirectoryConfig {
            bind_secret_ref: "hunter2".to_owned(),
            ..valid()
        };
        let problems = config.validate();
        let problem = problems
            .iter()
            .find(|problem| problem.field == "bind_secret_ref")
            .expect("a lowercase reference is a pasted password, and is named as one");
        assert_eq!(problem.kind, Problem::Invalid);
        assert!(
            problem.message.contains("never stored here"),
            "the message has to say why: {}",
            problem.message
        );
    }

    #[test]
    fn verification_cannot_be_turned_off_against_a_bare_address() {
        let config = DirectoryConfig {
            host: "ldaps://10.0.0.7".to_owned(),
            verify_tls: false,
            allow_insecure: true,
            ..valid()
        };
        let problems = config.validate();
        assert!(
            problems
                .iter()
                .any(|problem| problem.field == "allow_insecure"
                    && problem.kind == Problem::Conflict),
            "an IP has no name to match a certificate against: {problems:?}"
        );
    }

    #[test]
    fn a_plaintext_connection_has_no_tls_step() {
        let config = DirectoryConfig {
            host: "ldap://dir.example.com".to_owned(),
            start_tls: false,
            ..valid()
        };
        let outcome = test_steps(&config);
        assert_eq!(outcome.problems.len(), 0);
        assert_eq!(outcome.status, "incomplete");
        assert!(
            !outcome.steps.iter().any(|step| step.step == TestStep::Tls),
            "a greyed-out TLS step forever reads as an unresolved problem: {:?}",
            outcome.steps
        );
    }

    /// The ladder is the feature: a failure has to name the step it belongs to.
    #[test]
    fn a_failure_names_the_step_it_belongs_to() {
        let config = DirectoryConfig {
            bind_dn: String::new(),
            ..valid()
        };
        let outcome = test_steps(&config);
        assert_eq!(outcome.status, "failed");
        assert!(!outcome.passed());
        let bind = outcome
            .steps
            .iter()
            .find(|step| step.step == TestStep::Bind)
            .expect("every step is reported, passed or not");
        assert_eq!(bind.status, "failed");
        assert!(
            bind.detail.contains("service account"),
            "the step says what is wrong, not that something is: {}",
            bind.detail
        );
        assert_eq!(outcome.failing_step(), Some(TestStep::Bind));
    }

    /// A clean configuration is not a passing test. Slice 1 cannot reach a server, and a gate
    /// that lets a provider through on a clean form is the gate doing nothing.
    #[test]
    fn a_clean_configuration_is_not_a_passing_test() {
        let outcome = test_steps(&valid());
        assert_eq!(outcome.problems.len(), 0, "the configuration is sound");
        assert_eq!(
            outcome.status, "incomplete",
            "`ok` here would put a green check on a directory nobody has ever reached"
        );
        assert!(!outcome.passed());
        assert_eq!(
            outcome.reached_server, None,
            "slice 1 does not open a socket, and says so instead of faking an answer"
        );
        assert_eq!(
            outcome.failing_step(),
            Some(TestStep::Dns),
            "with a sound form the ladder starts at DNS, still untried"
        );
    }

    #[test]
    fn a_service_account_outside_the_declared_subtree_is_visible() {
        let mut config = valid();
        assert!(
            !config.bind_within_base(),
            "the service account is not under ou=people"
        );
        config.base_dn = "dc=example,dc=com".to_owned();
        assert!(config.bind_within_base());
        // Case and spacing are the directory's business, not the operator's.
        config.bind_dn = "CN=Omnion , OU=Svc ,  DC=Example ,DC=Com".to_owned();
        assert!(config.bind_within_base(), "a DN is compared structurally");
    }

    #[test]
    fn the_port_follows_the_scheme_unless_it_is_written() {
        let mut config = valid();
        assert_eq!(config.port(), 636);
        config.host = "ldap://dir.example.com".to_owned();
        assert_eq!(config.port(), 389);
        config.host = "ldap://dir.example.com:1389".to_owned();
        assert_eq!(config.port(), 1389);
        assert_eq!(config.hostname(), "dir.example.com");
    }

    /// A colon inside an IPv6 literal is not a port separator. Getting this wrong produces a
    /// hostname of `[::1]` and a port of `389]`, which fails with a message about the port.
    #[test]
    fn an_ipv6_literal_is_not_mistaken_for_a_port() {
        let mut config = valid();
        config.host = "ldaps://[2001:db8::1]:389".to_owned();
        assert_eq!(config.hostname(), "[2001:db8::1]");
        assert_eq!(config.port(), 389);
        config.host = "ldaps://[2001:db8::1]".to_owned();
        assert_eq!(config.hostname(), "[2001:db8::1]");
        assert_eq!(config.port(), 636);
        assert!(config.host_is_ip_literal());
    }

    #[test]
    fn an_ad_account_is_disabled_by_a_bit_not_by_a_number() {
        assert!(
            DirectoryConfig::ad_account_disabled(0x2),
            "the plain disabled flag"
        );
        assert!(
            DirectoryConfig::ad_account_disabled(0x0200 | 0x2),
            "a normal account also carries the 'not required to change password' bit"
        );
        assert!(
            !DirectoryConfig::ad_account_disabled(512),
            "512 alone is not disabled"
        );
        assert!(
            !DirectoryConfig::ad_account_disabled(0),
            "0 is an enabled account"
        );
    }

    #[test]
    fn each_kind_matches_the_login_attribute_its_server_uses() {
        let mut config = valid();
        assert_eq!(config.login_attribute(), "uid");
        assert_eq!(
            config.disabled_attribute(),
            None,
            "plain LDAP has no such flag"
        );

        config.kind = DirectoryKind::ActiveDirectory;
        assert_eq!(config.login_attribute(), "userPrincipalName");
        assert_eq!(config.disabled_attribute(), Some("userAccountControl"));
    }

    #[test]
    fn the_depth_and_subject_caps_have_a_floor_and_a_ceiling() {
        let mut config = valid();
        config.group_depth_cap = 0;
        config.subject_cap = 0;
        let problems = config.validate();
        assert!(problems.iter().any(|p| p.field == "group_depth_cap"));
        assert!(problems.iter().any(|p| p.field == "subject_cap"));

        // A value from the editor larger than the ceiling is clamped rather than refused, so a
        // provider row written by an older build cannot brick the sign-in path.
        let huge = DirectoryConfig::from_value(&json!({
            "group_depth_cap": 100_000,
            "subject_cap": 100_000_000,
            "page_size": 100_000,
        }))
        .expect("out-of-range numbers are clamped, not fatal");
        assert_eq!(huge.group_depth_cap, MAX_GROUP_DEPTH);
        assert_eq!(huge.subject_cap, MAX_SUBJECT_CAP);
        assert_eq!(huge.page_size, MAX_PAGE_SIZE);
    }

    /// The migration and the Rust lists have to agree. SQL cannot import a constant, so they are
    /// written twice and a test reads the migration file — the failure this catches is otherwise
    /// invisible until the database refuses a row in production.
    #[test]
    fn the_migration_and_the_struct_agree_about_the_columns() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../database/migrations/0116_identity_providers.sql"
        );
        let migration = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("cannot read 0116_identity_providers.sql ({error}); the closed lists live in it")
        });
        for column in [
            "last_test_at",
            "last_test_ok",
            "last_sync_at",
            "last_sync_status",
            "sync_interval_minutes",
            "plugin_key",
        ] {
            assert!(
                migration.contains(column),
                "the registry row carries `{column}` in Rust, so the migration must add it"
            );
        }
        for kind in ["ldap", "active_directory"] {
            assert!(
                migration.contains(kind),
                "`{kind}` is a provider kind in Rust, so the check constraint must accept it"
            );
        }
        // The old constraint is dropped rather than left in place, because a `check` cannot be
        // widened and two of them would leave the old, narrower one winning.
        assert!(
            migration.contains("drop constraint auth_providers_kind_check"),
            "the narrow three-kind check has to go or a directory can never be created"
        );
    }
}
