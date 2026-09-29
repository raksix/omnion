//! The node package: what a third party ships, how it is checked, and what installing it does
//! to the ledger (docs/requests/REQ-087, slice 4).
//!
//! A node package is a bundle of [`NodeDefinition`]s and [`CredentialDefinition`]s plus a
//! manifest. The definitions in `registry.rs` are *the* contract, so a package is parsed into
//! the same types the bundled registry uses rather than into a parallel shape that would drift
//! on the first field added: a node a platform can render is a node that satisfies the same lint
//! the bundled set satisfies.
//!
//! Three decisions define this module, and each is a place the obvious version is wrong:
//!
//! 1. **Validation is a total function of the manifest, and install is refused on any
//!    finding.** A package that fails the validator never reaches the ledger, because a ledger
//!    row is the only thing that makes nodes *appear*; a row for a package whose definitions
//!    do not compile is a palette entry that fails when placed.
//! 2. **A node key is namespaced by its package, and the namespace is checked rather than
//!    assumed.** `[Integration]` naming `http_request` would silently shadow a bundled node,
//!    and the version a workflow recorded would then point at different code. The check is
//!    `package.node`, which is also what the registry key space already looks like.
//! 3. **Removal degrades, never edits.** `RemovalPlan` names the workflows that lose a node and
//!    the nodes that lose their package, and nothing in this module writes a workflow. The
//!    REQ's words are "removal disables them and flags dependent workflows instead of
//!    breaking them" — a removal that edited somebody's automation to keep it running has
//!    broken it, silently and in a way the author never sees.
//!
//! ```text
//! manifest.json ──validate──▶ Package (ok | findings)
//!     │
//!     └──pack──▶ checksum ──install──▶ workflow_node_packages row
//!                                        │
//!                                        └──lookup──▶ package's node keys for the palette
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{Result, WorkflowError};
use crate::registry::{
    Capability, CredentialDefinition, CredentialField, CredentialKind, LintFinding, NodeCategory,
    NodeDefinition, OAuthConfig, ParamHint, ParamSpec, Port, PortKind, Sandbox,
};

// ---------------------------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------------------------

/// Where a package came from. The REQ's own vocabulary, and the ledger's `source` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSource {
    /// Shipped with the release.
    Bundled,
    /// Installed from the marketplace (REQ-048).
    Marketplace,
    /// Installed from a file or a path.
    Local,
}

impl PackageSource {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Marketplace => "marketplace",
            Self::Local => "local",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "bundled" => Some(Self::Bundled),
            "marketplace" => Some(Self::Marketplace),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// Whether the source is a string the ledger's check constraint accepts.
    ///
    /// The check constraint is on the *row*, the parse is on the *manifest*, and a manifest that
    /// says `Marketplace` in a hand-edited file must be refused here rather than at the insert
    /// where the reader sees a constraint violation instead of a sentence.
    #[must_use]
    pub const fn is_ledger_value(self) -> bool {
        matches!(self, Self::Bundled | Self::Marketplace | Self::Local)
    }
}

/// A semver-shaped version, compared the way a package updater needs it.
///
/// Not a dependency: the platform needs exactly one comparison — "is this install newer or
/// equal" — and a full semver implementation brings pre-release and build-metadata rules that
/// no ledger row needs. A version that does not parse is refused by the validator rather than
/// compared as text, because `"1.10" < "1.9"` as a string comparison is the bug this type
/// exists to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Version {
    // Spelled `core::result::Result` rather than `std::Result` because the crate imports
    // `crate::error::Result` as `Result` at the top of this module, and the shadowing is
    // exactly the kind of thing that reads as a typo rather than a real error.
    type Error = String;

    fn try_from(raw: String) -> core::result::Result<Self, Self::Error> {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            return Err("a version is required".into());
        }
        let core = trimmed
            .split(['-', '+'])
            .next()
            .unwrap_or_default()
            .to_string();
        let parts: Vec<&str> = core.split('.').collect();
        if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(format!(
                "{trimmed:?} is not a version: expected MAJOR[.MINOR[.PATCH]]"
            ));
        }
        if !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
            return Err(format!(
                "{trimmed:?} is not a version: every part must be a number"
            ));
        }
        Ok(Self(trimmed))
    }
}

impl From<Version> for String {
    fn from(value: Version) -> Self {
        value.0
    }
}

impl Version {
    /// Parse, for a caller that holds a `&str`.
    ///
    /// # Errors
    ///
    /// The reason the version is not a version.
    pub fn parse(raw: &str) -> std::result::Result<Self, String> {
        Self::try_from(raw.to_string())
    }

    /// Compare by numeric part, then let a pre-release sort *below* its release.
    ///
    /// `1.2.0-beta.1 < 1.2.0` is the rule that matters here: a pre-release is how a package
    /// ships something to a test tenant, and an updater that installs it over a released
    /// version has downgraded a tenant without saying so.
    pub fn is_newer_or_equal(&self, other: &Self) -> bool {
        let (mine, theirs) = (self.parts(), other.parts());
        for index in 0..3 {
            match mine[index].cmp(&theirs[index]) {
                std::cmp::Ordering::Greater => return true,
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Equal => {}
            }
        }
        if mine[3] == 1 || theirs[3] == 1 {
            // Same core version: the one *without* a pre-release wins.
            return mine[3] == 0;
        }
        true
    }

    /// `(major, minor, patch, has_pre_release)`.
    fn parts(&self) -> [u64; 4] {
        let mut core = [0u64; 3];
        let mut pre = false;
        for (index, part) in self
            .0
            .split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .enumerate()
            .take(3)
        {
            core[index] = part.parse().unwrap_or_default();
        }
        if self.0.contains('-') {
            pre = true;
        }
        [core[0], core[1], core[2], u64::from(pre)]
    }
}

/// One credential definition, as a manifest carries it.
///
/// Every field of [`CredentialField`] is an owned `String`/`Vec` here, where the bundled
/// registry uses `&'static str`. The conversion into the shared contract happens in
/// [`Package::validate`], and a leak would have to be reported as a finding — so a manifest can
/// never install a definition the registry's own type would not accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCredentialField {
    /// Field name.
    pub name: String,
    /// Field label.
    pub label: String,
    /// `string` | `secret` | `url` | `number` | `boolean` | `select`.
    #[serde(rename = "type", default = "string_field")]
    pub kind: String,
    /// Whether the form refuses to save without it.
    #[serde(default)]
    pub required: bool,
    /// Allowed values for a `select`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// One-line help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Whether the field is kept out of every log line.
    #[serde(default)]
    pub never_log: bool,
}

fn string_field() -> String {
    "string".to_string()
}

/// One credential type, as a manifest carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCredential {
    /// Stable key, unique within the package.
    pub key: String,
    /// How secrets are obtained.
    pub kind: String,
    /// Label on the type picker.
    pub label: String,
    /// One sentence on the detail screen.
    pub description: String,
    /// Lucide icon name.
    pub icon: String,
    /// Documentation link.
    pub docs_url: String,
    /// The form's fields, in order.
    #[serde(default)]
    pub fields: Vec<ManifestCredentialField>,
    /// OAuth settings, for an `oauth2` type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<ManifestOAuth>,
    /// Seconds the test hook may take.
    #[serde(default = "five_seconds")]
    pub test_timeout_seconds: i64,
}

fn five_seconds() -> i64 {
    5
}

/// OAuth settings as a manifest carries them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestOAuth {
    /// Authorization endpoint.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Scopes requested, space-separated.
    #[serde(default)]
    pub scopes: String,
    /// Whether the flow uses PKCE.
    #[serde(default)]
    pub pkce: bool,
    /// Whether the token set is refreshed before expiry.
    #[serde(default = "yes")]
    pub refresh: bool,
}

fn yes() -> bool {
    true
}

/// One node, as a manifest carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestNode {
    /// Key *within the package*, e.g. `send_message`. The installed key is `package.key`.
    pub key: String,
    /// Version a workflow is recorded against.
    pub version: String,
    /// Label in the palette.
    pub label: String,
    /// One sentence on the detail screen.
    pub description: String,
    /// `trigger` | `flow` | `code` | `data` | `integration` | `helper` | `error_handler`.
    pub category: String,
    /// Lucide icon name.
    pub icon: String,
    /// Documentation link.
    pub docs_url: String,
    /// Input ports.
    #[serde(default)]
    pub inputs: Vec<Port>,
    /// Output ports.
    #[serde(default)]
    pub outputs: Vec<Port>,
    /// Inspector parameters.
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// Credential types this node can use, by key — *within* the package.
    #[serde(default)]
    pub credential_types: Vec<String>,
    /// `execute` | `poll` | `webhook` | `trigger`.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `none` or `required`.
    #[serde(default = "sandbox_required")]
    pub sandbox: String,
    /// Attempts a run allows by default.
    #[serde(default = "one_attempt")]
    pub default_max_attempts: i32,
    /// Whether this version is deprecated.
    #[serde(default)]
    pub deprecated: bool,
    /// The node key that replaces a deprecated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

fn sandbox_required() -> String {
    "required".to_string()
}

fn one_attempt() -> i32 {
    1
}

/// What a package declares it may do. The REQ's ledger records `permissions`, and this is the
/// closed set that set is checked against.
///
/// A permission is not a decoration: a package that says `network` can make the engine's
/// outbound calls on a node's behalf, and a package that says `credentials` can ask the
/// installer for a credential *type*. Anything outside the set is a validator finding rather
/// than a stored string, because a string nobody has a chip for is a capability the reader
/// cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Make outbound HTTP calls.
    Network,
    /// Read and write the workflow store.
    Workflows,
    /// Offer a credential type of its own.
    Credentials,
    /// Register a trigger.
    Triggers,
    /// Run code out of process.
    Sandbox,
}

impl Permission {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Workflows => "workflows",
            Self::Credentials => "credentials",
            Self::Triggers => "triggers",
            Self::Sandbox => "sandbox",
        }
    }

    /// The whole closed set, for a validator message and a UI's permission picker.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Network,
            Self::Workflows,
            Self::Credentials,
            Self::Triggers,
            Self::Sandbox,
        ]
    }

    /// Parse a declared permission.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "network" => Some(Self::Network),
            "workflows" => Some(Self::Workflows),
            "credentials" => Some(Self::Credentials),
            "triggers" => Some(Self::Triggers),
            "sandbox" => Some(Self::Sandbox),
            _ => None,
        }
    }
}

/// The package manifest — the file a third party writes and a tenant installs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Package key, e.g. `acme-tools`. This is the namespace of every node inside it.
    pub key: String,
    /// Package version, `MAJOR[.MINOR[.PATCH]]` with an optional pre-release.
    pub version: String,
    /// Name in the installer's list.
    pub name: String,
    /// One sentence on the install screen.
    #[serde(default)]
    pub description: String,
    /// Documentation link.
    pub docs_url: String,
    /// Where it came from; a manifest may not claim to be bundled.
    #[serde(default = "local_source")]
    pub source: String,
    /// What the package asks for, as strings — checked against [`Permission::all`].
    #[serde(default)]
    pub permissions: Vec<String>,
    /// The nodes it ships.
    #[serde(default)]
    pub nodes: Vec<ManifestNode>,
    /// The credential types it ships.
    #[serde(default)]
    pub credentials: Vec<ManifestCredential>,
}

fn local_source() -> String {
    "local".to_string()
}

// ---------------------------------------------------------------------------------------------
// The validated package
// ---------------------------------------------------------------------------------------------

/// A manifest that passed validation, with its installed keys resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    /// Package key.
    pub key: String,
    /// Package version.
    pub version: Version,
    /// Name in the installer's list.
    pub name: String,
    /// One sentence.
    pub description: String,
    /// Documentation link.
    pub docs_url: String,
    /// Where it came from.
    pub source: PackageSource,
    /// The permissions it asked for, as the closed set, sorted and de-duplicated.
    pub permissions: Vec<Permission>,
    /// The nodes, with every key already namespaced to `package.key`.
    pub nodes: Vec<NodeDefinition>,
    /// The credential types, namespaced the same way.
    pub credentials: Vec<CredentialDefinition>,
}

impl Package {
    /// The installed key of a node this package ships.
    ///
    /// The namespacing lives here, in one function, because a second place that assembles a
    /// key is a second answer: the ledger's `node_keys[]` event payload, the palette's
    /// `package missing` check and the installer's own listing all read through this.
    #[must_use]
    pub fn node_key(&self, local_key: &str) -> String {
        format!("{}.{}", self.key, local_key)
    }

    /// The installed key of a credential type this package ships.
    #[must_use]
    pub fn credential_key(&self, local_key: &str) -> String {
        format!("{}.{}", self.key, local_key)
    }

    /// The package's node keys, in manifest order, as the install event names them.
    #[must_use]
    pub fn node_keys(&self) -> Vec<String> {
        self.nodes.iter().map(|node| node.key.to_string()).collect()
    }

    /// The keys of the nodes whose capability or category implies a permission the package
    /// did not declare.
    ///
    /// This is the check that makes `permissions` mean something: a package whose nodes
    /// declare `trigger` but which asked for neither `triggers` nor `sandbox` is refused,
    /// because the alternative is a ledger that records what the package *said* while the
    /// engine does what the package *built*.
    fn implied_permissions(&self) -> BTreeSet<Permission> {
        let mut implied = BTreeSet::new();
        for node in &self.nodes {
            if node.is_trigger() {
                implied.insert(Permission::Triggers);
            }
            if !node.credential_types.is_empty() {
                implied.insert(Permission::Credentials);
            }
            if node.sandbox == Sandbox::Required {
                implied.insert(Permission::Sandbox);
            }
            if node
                .params
                .iter()
                .any(|param| param.kind == "url" || param.name.contains("endpoint"))
            {
                implied.insert(Permission::Network);
            }
        }
        if !self.credentials.is_empty() {
            implied.insert(Permission::Credentials);
        }
        implied
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// The outcome of validating a manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Validation {
    /// The package, when nothing blocked it.
    pub package: Option<Package>,
    /// Everything that is wrong, whether or not the install is refused.
    pub findings: Vec<LintFinding>,
}

impl Validation {
    /// Whether a package may be installed.
    #[must_use]
    pub fn is_installable(&self) -> bool {
        self.package.is_some()
    }

    /// A one-line verdict, for the installer screen and the CLI.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.package {
            Some(package) => format!(
                "{} {} is installable — {} node(s), {} credential type(s), permissions: {}",
                package.key,
                package.version,
                package.nodes.len(),
                package.credentials.len(),
                if package.permissions.is_empty() {
                    "none".to_string()
                } else {
                    package
                        .permissions
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
            None => format!(
                "refused — {} finding(s); nothing reached the ledger",
                self.findings.len()
            ),
        }
    }
}

/// One validator finding, built from the three things every finding carries.
///
/// A function rather than a closure: a closure capturing `findings` holds the vector
/// borrowed for the whole validator, so every later `findings.extend(...)` — the bundled
/// registry's lint, for one — is a second simultaneous borrow and the borrow checker refuses
/// the entire function.
///
/// The parameters are `String`, not `impl Into<String>`, because a call site writing
/// `"a package needs a name".into()` against a generic parameter is ambiguous on this
/// workspace: both `bytes::Bytes` and `sqlx_core`'s `UStr` implement `From<&str>`, so inference
/// has two answers and neither is the `String` the call site meant. Taking the concrete type
/// makes every `.into()` at the call sites resolve on the first try.
fn finding(code: &'static str, subject: String, message: String) -> LintFinding {
    LintFinding {
        code,
        subject,
        message,
    }
}

/// Validate a manifest and, if nothing blocks it, resolve it into an installable package.
///
/// The bundled registry's own lint is used for the definitions rather than a second copy of the
/// rules, so a package cannot satisfy a weaker check than a bundled node: [`lint_node`] and
/// [`lint_credential`] are the *same* functions the bundled table passes, called with the
/// already-namespaced key.
pub fn validate(manifest: &Manifest) -> Validation {
    let mut findings: Vec<LintFinding> = Vec::new();

    // --- the manifest itself ------------------------------------------------------------
    let key_ok = package_key_is_valid(&manifest.key);
    if !key_ok {
        findings.push(finding(
            "package_key_invalid",
            manifest.key.clone(),
            format!(
                "{:?} is not a package key: lower-case letters, digits and dashes, starting with \
                 a letter",
                manifest.key
            ),
        ));
    }
    if manifest.name.trim().is_empty() {
        findings.push(finding(
            "package_name_blank",
            manifest.key.clone(),
            "a package needs a name".into(),
        ));
    }
    if manifest.docs_url.trim().is_empty() {
        findings.push(finding(
            "package_docs_missing",
            manifest.key.clone(),
            "a package needs a documentation link".into(),
        ));
    }

    let version = match Version::parse(&manifest.version) {
        Ok(version) => Some(version),
        Err(reason) => {
            findings.push(finding(
                "package_version_invalid",
                manifest.key.clone(),
                reason,
            ));
            None
        }
    };

    let source = match PackageSource::parse(&manifest.source) {
        Some(source) if source.is_ledger_value() => Some(source),
        _ => {
            findings.push(finding(
                "package_source_invalid",
                manifest.key.clone(),
                format!(
                    "{:?} is not a source; the ledger accepts bundled, marketplace or local",
                    manifest.source
                ),
            ));
            None
        }
    };

    // A package that claims to be bundled is asking to be a first-party release, and the
    // ledger's `bundled` rows are what the platform itself ships. Refusing it here means the
    // installer's "Bundled" list is a fact rather than a claim.
    if PackageSource::parse(&manifest.source) == Some(PackageSource::Bundled) {
        findings.push(finding(
            "package_source_not_installable",
            manifest.key.clone(),
            "a package cannot install itself as `bundled`: the bundled set ships with the \
             platform, not from a tenant"
                .into(),
        ));
    }

    if manifest.nodes.is_empty() && manifest.credentials.is_empty() {
        findings.push(finding(
            "package_empty",
            manifest.key.clone(),
            "a package ships at least one node or credential type".into(),
        ));
    }

    // --- permissions --------------------------------------------------------------------
    let mut permissions: Vec<Permission> = Vec::new();
    for raw in &manifest.permissions {
        match Permission::parse(raw) {
            Some(permission) => {
                if !permissions.contains(&permission) {
                    permissions.push(permission);
                }
            }
            None => {
                findings.push(finding(
                    "package_permission_unknown",
                    manifest.key.clone(),
                    format!(
                        "{raw:?} is not a permission; the set is {}",
                        Permission::all()
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
    }
    permissions.sort_unstable();

    // --- nodes --------------------------------------------------------------------------
    let mut nodes: Vec<NodeDefinition> = Vec::new();
    let mut seen_node_keys: BTreeSet<String> = BTreeSet::new();

    for entry in &manifest.nodes {
        let subject = if key_ok {
            format!("{}.{}", manifest.key, entry.key)
        } else {
            entry.key.clone()
        };

        if entry.key.trim().is_empty() || entry.key.contains(['.', ' ', '\t']) {
            findings.push(finding(
                "package_node_key_invalid",
                subject,
                format!(
                    "{:?} is not a node key inside a package: it is namespaced by the package key, \
                     so it carries no dot or space",
                    entry.key
                ),
            ));
            continue;
        }
        // The version a workflow records is compared by the engine, so a node whose version
        // does not parse is a node whose recorded version can never be reasoned about.
        if let Err(reason) = Version::parse(&entry.version) {
            findings.push(finding(
                "package_node_version_invalid",
                subject.clone(),
                reason,
            ));
        }

        let Some(category) = NodeCategory::parse(&entry.category) else {
            findings.push(finding(
                "package_node_category_unknown",
                subject.clone(),
                format!("{:?} is not a category", entry.category),
            ));
            continue;
        };
        let capabilities: Vec<Capability> = entry
            .capabilities
            .iter()
            .filter_map(|raw| match Capability::parse(raw) {
                Some(capability) => Some(capability),
                None => {
                    findings.push(finding(
                        "package_node_capability_unknown",
                        subject.clone(),
                        format!("{raw:?} is not a capability"),
                    ));
                    None
                }
            })
            .collect();
        // A third-party node's code never runs in the core process, so `sandbox` has exactly
        // one legal value. `Sandbox` has no `parse` because the bundled table has one shape;
        // a manifest has a *string*, and the string is checked here rather than by growing the
        // registry's enum API for a value the registry itself never accepts.
        let sandbox = match entry.sandbox.as_str() {
            "none" => Sandbox::None,
            "required" => Sandbox::Required,
            other => {
                findings.push(finding(
                    "package_node_sandbox_unknown",
                    subject.clone(),
                    format!("{other:?} is not a sandbox mode; it is `none` or `required`"),
                ));
                continue;
            }
        };

        // Credential references resolve to the *installed* key, so a node that names its own
        // package's type is namespaced the same way the type is. A reference to a credential
        // the package does not ship is a finding rather than a dangling key the palette would
        // render as a select with no options.
        let mut credential_keys: Vec<String> = Vec::new();
        for named in &entry.credential_types {
            if manifest
                .credentials
                .iter()
                .any(|candidate| candidate.key == *named)
            {
                credential_keys.push(format!("{}.{}", manifest.key, named));
            } else {
                findings.push(finding(
                    "package_node_credential_unknown",
                    subject.clone(),
                    format!(
                        "node {named:?} is not a credential type this package ships (it ships: {})",
                        if manifest.credentials.is_empty() {
                            "none".to_string()
                        } else {
                            manifest
                                .credentials
                                .iter()
                                .map(|c| c.key.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ),
                ));
            }
        }

        // The definition is built *first* and the bundled registry's own lint runs on it,
        // rather than a second copy of the definition being linted. The two shapes drifting
        // is the failure this arrangement exists to prevent: a `NodeDefinition` literal in
        // the middle of a validator is one more place a new field can be forgotten, and a
        // forgotten field is a node the palette cannot render.
        //
        // The key is namespaced here, which is why the lint sees the *installed* key: a
        // duplicate is checked on the local key below, and the lint's own key checks (no
        // spaces, not blank) must hold for the key that will actually be in the registry.
        //
        // The interning is a deliberate leak, and it is bounded: one `&'static str` per field
        // per installed package, and packages are installed by a human action rather than per
        // request. The bundled registry is a `const` table of `&'static str`, so the only two
        // honest options are interning at install time or a second representation the rest of
        // the engine has to know about. Interning keeps one representation.
        let definition = NodeDefinition {
            key: Box::leak(subject.clone().into_boxed_str()),
            version: Box::leak(entry.version.clone().into_boxed_str()),
            label: Box::leak(entry.label.clone().into_boxed_str()),
            description: Box::leak(entry.description.clone().into_boxed_str()),
            category,
            icon: Box::leak(entry.icon.clone().into_boxed_str()),
            docs_url: Box::leak(entry.docs_url.clone().into_boxed_str()),
            inputs: entry.inputs.clone(),
            outputs: entry.outputs.clone(),
            params: entry.params.clone(),
            credential_types: credential_keys
                .iter()
                .map(|key| Box::leak(key.clone().into_boxed_str()) as &'static str)
                .collect(),
            capabilities,
            sandbox,
            default_max_attempts: entry.default_max_attempts,
            deprecated: entry.deprecated,
            superseded_by: None,
        };

        // The *bundled* lint, on the *same* definition that will be installed. This is the
        // whole contract of the module in one call: a package cannot ship a node the
        // platform's own registry would refuse.
        findings.extend(crate::registry::lint_node(
            &definition,
            &definition.credential_types,
        ));

        // The namespaced key is what a second node in the same package may not repeat. The
        // check is on the *local* key because the namespace is the same for all of them.
        if !seen_node_keys.insert(entry.key.clone()) {
            findings.push(finding(
                "package_node_duplicate",
                subject.clone(),
                format!("two nodes are both named {:?}", entry.key),
            ));
            continue;
        }
        nodes.push(definition);
    }

    // --- credential types ----------------------------------------------------------------
    let mut credentials: Vec<CredentialDefinition> = Vec::new();
    let mut seen_credential_keys: BTreeSet<String> = BTreeSet::new();

    for entry in &manifest.credentials {
        let subject = if key_ok {
            format!("{}.{}", manifest.key, entry.key)
        } else {
            entry.key.clone()
        };

        if entry.key.trim().is_empty() || entry.key.contains(['.', ' ', '\t']) {
            findings.push(finding(
                "package_credential_key_invalid",
                subject,
                format!(
                    "{:?} is not a credential type key inside a package: no dot, no space",
                    entry.key
                ),
            ));
            continue;
        }
        if !seen_credential_keys.insert(entry.key.clone()) {
            findings.push(finding(
                "package_credential_duplicate",
                subject.clone(),
                format!("two credential types are both named {:?}", entry.key),
            ));
            continue;
        }

        let Some(kind) = CredentialKind::parse(&entry.kind) else {
            findings.push(finding(
                "package_credential_kind_unknown",
                subject.clone(),
                format!("{:?} is not a credential kind", entry.kind),
            ));
            continue;
        };
        let mut fields: Vec<CredentialField> = Vec::new();
        let mut field_ok = true;
        for field in &entry.fields {
            let Some(field_kind) = crate::registry::FieldType::parse(&field.kind) else {
                findings.push(finding(
                    "package_credential_field_type_unknown",
                    subject.clone(),
                    format!("field {:?} has unknown type {:?}", field.name, field.kind),
                ));
                field_ok = false;
                continue;
            };
            fields.push(CredentialField {
                name: Box::leak(field.name.clone().into_boxed_str()),
                label: Box::leak(field.label.clone().into_boxed_str()),
                kind: field_kind,
                required: field.required,
                options: field
                    .options
                    .iter()
                    .map(|option| Box::leak(option.clone().into_boxed_str()) as &'static str)
                    .collect(),
                help: field
                    .help
                    .as_ref()
                    .map(|help| &*Box::leak(help.clone().into_boxed_str())),
                never_log: field.never_log,
            });
        }
        if !field_ok {
            continue;
        }

        let oauth = entry.oauth.as_ref().map(|oauth| OAuthConfig {
            authorize_url: Box::leak(oauth.authorize_url.clone().into_boxed_str()),
            token_url: Box::leak(oauth.token_url.clone().into_boxed_str()),
            scopes: Box::leak(oauth.scopes.clone().into_boxed_str()),
            pkce: oauth.pkce,
            refresh: oauth.refresh,
        });

        let definition = CredentialDefinition {
            key: Box::leak(subject.clone().into_boxed_str()),
            kind,
            label: Box::leak(entry.label.clone().into_boxed_str()),
            description: Box::leak(entry.description.clone().into_boxed_str()),
            icon: Box::leak(entry.icon.clone().into_boxed_str()),
            docs_url: Box::leak(entry.docs_url.clone().into_boxed_str()),
            fields,
            oauth,
            test_timeout_seconds: entry.test_timeout_seconds,
        };
        // A package's own credential type is not an orphan when one of the package's nodes
        // names it. Passing an empty `used_by` — the literal default — reports every type the
        // package ships as orphaned, which is the check the bundled registry runs against the
        // *global* node list. The answer here is derived from the nodes already resolved
        // above, so a type nothing references is still refused.
        let used_by: Vec<&str> = nodes
            .iter()
            .filter(|node| {
                node.credential_types
                    .iter()
                    .any(|named| *named == definition.key)
            })
            .map(|node| node.key)
            .collect();
        findings.extend(crate::registry::lint_credential(&definition, &used_by));
        credentials.push(definition);
    }

    // A credential type declared as `oauth2` with no OAuth config is a form that cannot be
    // filled: the picker would offer a "Re-connect" button that has no endpoints to call.
    for credential in &credentials {
        if credential.kind == CredentialKind::OAuth2 && credential.oauth.is_none() {
            findings.push(finding(
                "package_credential_oauth_missing",
                credential.key.to_string(),
                format!(
                    "{:?} is an oauth2 type and needs authorize_url and token_url",
                    credential.key
                ),
            ));
        }
    }

    // --- implied permissions -------------------------------------------------------------
    let package = match (&version, &source) {
        (Some(version), Some(source)) if key_ok => Some(Package {
            key: manifest.key.clone(),
            version: version.clone(),
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            docs_url: manifest.docs_url.clone(),
            source: *source,
            permissions,
            nodes,
            credentials,
        }),
        _ => None,
    };

    if let Some(package) = &package {
        let declared: BTreeSet<Permission> = package.permissions.iter().copied().collect();
        for implied in package.implied_permissions() {
            if !declared.contains(&implied) {
                findings.push(LintFinding {
                    code: "package_permission_undeclared",
                    subject: package.key.clone(),
                    message: format!(
                        "the package ships nodes that need the {:?} permission but does not \
                         declare it",
                        implied.as_str()
                    ),
                });
            }
        }
        // A third-party package's code never runs in the core process. `sandbox: none` on an
        // installed node is a package asking to be trusted with the engine, and the REQ's
        // third-party rule (docs/09 §13 lesson 14) says it cannot be.
        for node in &package.nodes {
            if node.sandbox == Sandbox::None {
                findings.push(LintFinding {
                    code: "package_node_sandbox_not_allowed",
                    subject: node.key.to_string(),
                    message: format!(
                        "{} declares sandbox: none; an installed package's code runs out of \
                         process",
                        node.key
                    ),
                });
            }
        }
    }

    // Findings that are structural (a bad key, an unparseable version) mean there is no
    // package to install even if the definitions lint clean, so the verdict is refused
    // whenever the manifest itself failed.
    let refused_by_manifest = findings.iter().any(|finding| {
        matches!(
            finding.code,
            "package_key_invalid"
                | "package_version_invalid"
                | "package_source_invalid"
                | "package_source_not_installable"
                | "package_empty"
        )
    });

    let package = if refused_by_manifest || !findings.is_empty() {
        None
    } else {
        package
    };

    Validation { package, findings }
}

/// Whether a package key is the shape the key space uses: lower-case, digits and dashes.
#[must_use]
pub fn package_key_is_valid(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether an install is an update, i.e. equal-or-newer.
///
/// # Errors
///
/// `node_package_version_unsupported` when the requested version is older than what is
/// installed. A downgrade is refused rather than applied: the version a workflow recorded
/// would then name code that no longer exists, and "released node versions stay available for
/// a deprecation window" (REQ-087 risks) is a promise about upgrades, not about silent
/// rollbacks.
pub fn ensure_not_a_downgrade(installed: &Version, requested: &Version) -> Result<()> {
    if requested.is_newer_or_equal(installed) {
        return Ok(());
    }
    Err(WorkflowError::invalid(
        "node_package_version_unsupported",
        format!(
            "version {requested} is older than the installed {installed}; an install is \
             equal-or-newer only, so install {installed} or newer"
        ),
    ))
}

// ---------------------------------------------------------------------------------------------
// The checksum
// ---------------------------------------------------------------------------------------------

/// The content checksum the ledger records.
///
/// Computed over the *canonical* manifest rather than the file's bytes: a file re-indented or
/// re-ordered must not read as a different package, or a re-upload of the same manifest
/// installs over itself as if it were a new version. The canonical form is the JSON with every
/// object's keys sorted — which is exactly what `serde_json` does when serializing a `Value`
/// built from a `BTreeMap`, so the canonicalizer is the value walk rather than a second
/// normalizer.
#[must_use]
pub fn checksum(manifest: &Manifest) -> String {
    let value = serde_json::to_value(manifest).unwrap_or(Value::Null);
    let canonical = canonical_json(&value);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().fold(String::new(), |mut acc, byte| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

/// Recursively sort a JSON value's object keys, so the checksum is over content, not layout.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: serde_json::Map<String, Value> = map
                .iter()
                .map(|(key, child)| (key.clone(), canonical_json(child)))
                .collect();
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------------------------

/// One workflow that loses a node when a package is removed or disabled.
#[derive(Debug, Clone, PartialEq)]
pub struct AffectedWorkflow {
    /// The workflow's id.
    pub workflow_id: uuid::Uuid,
    /// Its name, so the installer can say what will be affected.
    pub workflow_name: String,
    /// The node keys that name this package.
    pub node_keys: Vec<String>,
}

/// What a removal would do, computed without doing it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovalPlan {
    /// The package key.
    pub package_key: String,
    /// The node keys the package ships.
    pub node_keys: Vec<String>,
    /// The workflows that name at least one of them.
    pub affected_workflows: Vec<AffectedWorkflow>,
}

impl RemovalPlan {
    /// Whether anything depends on the package.
    #[must_use]
    pub fn has_dependents(&self) -> bool {
        !self.affected_workflows.is_empty()
    }

    /// A sentence the installer screen shows before the button, and the remove response repeats
    /// after the fact.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.affected_workflows.is_empty() {
            return format!(
                "{} ships {} node(s) and no workflow uses them; removing it changes nothing",
                self.package_key,
                self.node_keys.len()
            );
        }
        let mut names: Vec<&str> = self
            .affected_workflows
            .iter()
            .map(|workflow| workflow.workflow_name.as_str())
            .collect();
        names.sort_unstable();
        format!(
            "{} ships {} node(s) and {} workflow(s) use them ({}). They stay loadable and their \
             nodes are disabled rather than deleted.",
            self.package_key,
            self.node_keys.len(),
            self.affected_workflows.len(),
            names.join(", ")
        )
    }
}

/// Build a removal plan from the ledger's node keys and a set of (workflow id, name, node
/// keys) references.
///
/// A pure function of its inputs, so the installer screen can render the *exact* warning the
/// remove call will return, and a test can assert the degradation without a database.
#[must_use]
pub fn removal_plan(
    package_key: &str,
    node_keys: &[String],
    references: &[(uuid::Uuid, String, Vec<String>)],
) -> RemovalPlan {
    let owned: BTreeSet<&str> = node_keys.iter().map(String::as_str).collect();
    let mut affected: Vec<AffectedWorkflow> = references
        .iter()
        .map(|(workflow_id, workflow_name, keys)| {
            let mut named: Vec<String> = keys
                .iter()
                .filter(|key| owned.contains(key.as_str()))
                .cloned()
                .collect();
            named.sort();
            named.dedup();
            AffectedWorkflow {
                workflow_id: *workflow_id,
                workflow_name: workflow_name.clone(),
                node_keys: named,
            }
        })
        .filter(|workflow| !workflow.node_keys.is_empty())
        .collect();
    affected.sort_by(|left, right| left.workflow_name.cmp(&right.workflow_name));

    RemovalPlan {
        package_key: package_key.to_string(),
        node_keys: node_keys.to_vec(),
        affected_workflows: affected,
    }
}

// ---------------------------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------------------------

/// A scaffolded package, as the CLI writes it to disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Scaffold {
    /// The manifest to write as `manifest.json`.
    pub manifest: Manifest,
    /// A sample item per node, as the fixture runner feeds it: node key → sample input.
    pub fixtures: Vec<(String, Value)>,
    /// The `README.md` the SDK writes, which documents the contract rather than the project.
    pub readme: String,
}

/// Build a working example package for `key`, carrying one action node and one
/// credential-bearing node.
///
/// "Working" is the whole point: a scaffold whose own fixtures fail is a scaffold that teaches
/// the author to skip validation. Both nodes here pass [`validate`], which the test proves by
/// running it — so the first thing a third party sees is a package that installs.
#[must_use = "used by the CLI and the SDK docs"]
pub fn scaffold(key: &str) -> Scaffold {
    // The key is checked rather than trusted: a scaffold of an invalid key would hand the
    // author a package the validator refuses, which is the worst first impression the SDK has.
    let key = if package_key_is_valid(key) {
        key.to_string()
    } else {
        "acme-tools".to_string()
    };

    let manifest = Manifest {
        key: key.clone(),
        version: "0.1.0".to_string(),
        name: "Acme tools".to_string(),
        description: format!("Two example nodes for the {key} package."),
        docs_url: "https://example.com/docs/nodes".to_string(),
        source: "local".to_string(),
        // `sandbox` is here because *both* nodes run out of process, and the validator
        // refuses a package whose nodes imply a permission it did not declare. A scaffold that
        // did not teach this would produce a package that fails its own validator on the
        // author's first run.
        permissions: vec![
            Permission::Network.as_str().to_string(),
            Permission::Credentials.as_str().to_string(),
            Permission::Sandbox.as_str().to_string(),
        ],
        nodes: vec![
            ManifestNode {
                key: "echo".to_string(),
                version: "0.1.0".to_string(),
                label: "Echo".to_string(),
                description: "Returns the text it was given, unchanged.".to_string(),
                category: "helper".to_string(),
                icon: "MessageSquare".to_string(),
                docs_url: "https://example.com/docs/nodes/echo".to_string(),
                inputs: vec![Port {
                    name: "main".to_string(),
                    kind: PortKind::Main,
                    accepts: vec!["text".to_string(), "json".to_string()],
                    open: false,
                }],
                outputs: vec![Port {
                    name: "main".to_string(),
                    kind: PortKind::Main,
                    accepts: vec!["text".to_string(), "json".to_string()],
                    open: true,
                }],
                params: vec![ParamSpec {
                    name: "text".to_string(),
                    kind: "string".to_string(),
                    label: "Text".to_string(),
                    required: true,
                    ui: ParamHint::Textarea,
                    options: Vec::new(),
                    options_source: None,
                    placeholder: Some("What should come back?".to_string()),
                    help: Some("Returned verbatim as the node's single item.".to_string()),
                    default: None,
                    secret_field: false,
                }],
                credential_types: Vec::new(),
                capabilities: vec![Capability::Execute.as_str().to_string()],
                sandbox: "required".to_string(),
                default_max_attempts: 3,
                deprecated: false,
                superseded_by: None,
            },
            ManifestNode {
                key: "send_message".to_string(),
                version: "0.1.0".to_string(),
                label: "Send message".to_string(),
                description: "Sends the text to a channel through an API-key credential."
                    .to_string(),
                category: "integration".to_string(),
                icon: "Send".to_string(),
                docs_url: "https://example.com/docs/nodes/send-message".to_string(),
                inputs: vec![Port {
                    name: "main".to_string(),
                    kind: PortKind::Main,
                    accepts: vec!["text".to_string()],
                    open: false,
                }],
                outputs: vec![Port {
                    name: "main".to_string(),
                    kind: PortKind::Main,
                    accepts: vec!["json".to_string()],
                    open: true,
                }],
                params: vec![
                    ParamSpec {
                        name: "channel".to_string(),
                        kind: "string".to_string(),
                        label: "Channel".to_string(),
                        required: true,
                        ui: ParamHint::Text,
                        options: Vec::new(),
                        options_source: None,
                        placeholder: Some("#general".to_string()),
                        help: None,
                        default: None,
                        secret_field: false,
                    },
                    ParamSpec {
                        name: "text".to_string(),
                        kind: "string".to_string(),
                        label: "Text".to_string(),
                        required: true,
                        ui: ParamHint::Textarea,
                        options: Vec::new(),
                        options_source: None,
                        placeholder: None,
                        help: None,
                        default: None,
                        secret_field: false,
                    },
                    ParamSpec {
                        name: "credential_key".to_string(),
                        kind: "string".to_string(),
                        label: "Credential".to_string(),
                        required: true,
                        // A `secret_field` parameter *is* the credential picker: the registry's
                        // own lint refuses any other ui hint, because a credential reference in
                        // a text box is a key somebody has to know rather than choose. The
                        // scaffold teaches the contract, so it has to obey it.
                        ui: ParamHint::Select,
                        options: Vec::new(),
                        options_source: Some("credentials".to_string()),
                        placeholder: None,
                        help: Some(
                            "Pick an API-key credential of the type this node names.".to_string(),
                        ),
                        default: None,
                        secret_field: true,
                    },
                ],
                credential_types: vec!["api_key".to_string()],
                capabilities: vec![Capability::Execute.as_str().to_string()],
                sandbox: "required".to_string(),
                default_max_attempts: 3,
                deprecated: false,
                superseded_by: None,
            },
        ],
        credentials: vec![ManifestCredential {
            key: "api_key".to_string(),
            kind: "api_key".to_string(),
            label: "API key".to_string(),
            description: "A single API key, sent as a bearer token.".to_string(),
            icon: "KeyRound".to_string(),
            docs_url: "https://example.com/docs/credentials/api-key".to_string(),
            fields: vec![ManifestCredentialField {
                name: "api_key".to_string(),
                label: "API key".to_string(),
                kind: "secret".to_string(),
                required: true,
                options: Vec::new(),
                help: Some("Sent as a bearer token. Never shown again after saving.".to_string()),
                never_log: true,
            }],
            oauth: None,
            test_timeout_seconds: 5,
        }],
    };

    let fixtures = vec![
        (
            "echo".to_string(),
            json!({ "text": "hello from a fixture" }),
        ),
        (
            "send_message".to_string(),
            json!({
                "channel": "#general",
                "text": "hello from a fixture",
                "credential_key": "acme.default"
            }),
        ),
    ];

    let readme = format!(
        "# {name}\n\
         \n\
         A node package for Omnion, built with the node SDK. Two nodes ship: **Echo** (no\n\
         credential) and **Send message** (needs the package's `api_key` credential type).\n\
         \n\
         ## Layout\n\
         \n\
         ```text\n\
         manifest.json      the package: key, version, permissions, nodes, credential types\n\
         fixtures/{{node}}.json   one sample item per node, fed to the fixture runner\n\
         ```\n\
         \n\
         ## Validate and pack\n\
         \n\
         ```bash\n\
         omnion node validate manifest.json\n\
         omnion node pack manifest.json --out {key}.omnion-node.json\n\
         ```\n\
         \n\
         `validate` is what an install runs: any finding refuses the package, and a refused\n\
         package never reaches the ledger. `pack` writes the manifest with its content checksum\n\
         (`checksum` field) — the same value the ledger stores, computed over the canonical\n\
         JSON so re-indenting the file does not read as a new package.\n\
         \n\
         ## The contract in one paragraph\n\
         \n\
         A node declares its ports, its parameters as a JSON Schema subset, its capabilities and\n\
         whether its code runs out of process. `sandbox: required` is the only accepted value for\n\
         an installed package: third-party code never runs in the core process. A node that needs\n\
         a credential names the credential *type* it ships, never a value; a `credential_key`\n\
         parameter holds the *key* of a credential instance, and the palette renders it as a\n\
         picker. Every node key is namespaced `{{package}}.{{node}}` at install time, so a package\n\
         cannot shadow a bundled node by naming it.\n",
        name = manifest.name,
        key = key,
    );

    Scaffold {
        manifest,
        fixtures,
        readme,
    }
}

/// A packed package: the manifest plus the checksum the ledger records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackedPackage {
    /// Format tag, so a future packer is refused rather than misread.
    pub format: String,
    /// The manifest.
    pub manifest: Manifest,
    /// Content checksum over the canonical manifest.
    pub checksum: String,
}

/// The pack format tag.
pub const PACKED_FORMAT: &str = "omnion.node-package/1";

/// Pack a manifest, refusing anything that does not validate.
///
/// # Errors
///
/// The first refusal, as a validator finding rendered in prose. A packed file is a file the
/// installer trusts enough to hash, so a package that packs is a package that passed — and the
/// refusal is the validator's own message rather than a second list.
pub fn pack(manifest: &Manifest) -> std::result::Result<PackedPackage, String> {
    let validation = validate(manifest);
    let Some(package) = validation.package else {
        return Err(validation
            .findings
            .iter()
            .map(|finding| format!("{}: {}", finding.code, finding.message))
            .collect::<Vec<_>>()
            .join("\n"));
    };
    Ok(PackedPackage {
        format: PACKED_FORMAT.to_string(),
        checksum: checksum(manifest),
        manifest: {
            // The source is recorded as what the packer knows it to be: a package on disk is a
            // local install, and letting the file claim `marketplace` is how a package walks
            // into a tenant's ledger with the wrong provenance.
            let mut manifest = manifest.clone();
            manifest.source = package.source.as_str().to_string();
            manifest
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn good() -> Manifest {
        scaffold("acme").manifest
    }

    #[test]
    fn the_scaffold_installs_itself() {
        let validation = validate(&good());
        assert!(
            validation.is_installable(),
            "the SDK's own example must install: {:?}",
            validation.findings
        );
        let package = validation.package.expect("installable");
        assert_eq!(package.key, "acme");
        assert_eq!(package.version.to_string(), "0.1.0");
        assert_eq!(package.node_keys(), vec!["acme.echo", "acme.send_message"]);
    }

    #[test]
    fn node_keys_are_namespaced_so_a_package_cannot_shadow_a_bundled_node() {
        let mut manifest = good();
        manifest.nodes[0].key = "http_request".to_string();
        let validation = validate(&manifest);
        let package = validation.package.expect("still installable");
        assert_eq!(package.node_keys()[0], "acme.http_request");
        // The bundled key is untouched, and a graph naming `http_request` still resolves to
        // the bundled node — which is the whole reason the namespace exists.
        assert!(crate::registry::find_node("http_request").is_some());
    }

    #[test]
    fn a_package_may_not_install_itself_as_bundled() {
        let mut manifest = good();
        manifest.source = "bundled".to_string();
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "package_source_not_installable")
        );
    }

    #[test]
    fn an_unknown_permission_is_refused_rather_than_stored() {
        let mut manifest = good();
        manifest.permissions.push("root".to_string());
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        let finding = validation
            .findings
            .iter()
            .find(|finding| finding.code == "package_permission_unknown")
            .expect("finding");
        assert!(finding.message.contains("network"), "{}", finding.message);
    }

    #[test]
    fn a_node_that_needs_a_permission_the_package_did_not_ask_for_is_refused() {
        let mut manifest = good();
        manifest.permissions.clear();
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        let codes: Vec<&str> = validation
            .findings
            .iter()
            .map(|finding| finding.code)
            .collect();
        assert!(
            codes.contains(&"package_permission_undeclared"),
            "{codes:?}"
        );
    }

    #[test]
    fn a_node_may_not_run_in_the_core_process() {
        let mut manifest = good();
        manifest.nodes[0].sandbox = "none".to_string();
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "package_node_sandbox_not_allowed")
        );
    }

    #[test]
    fn a_node_naming_a_credential_type_the_package_does_not_ship_is_refused() {
        let mut manifest = good();
        manifest.nodes[1].credential_types = vec!["oauth2".to_string()];
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "package_node_credential_unknown")
        );
    }

    #[test]
    fn the_definition_lint_is_the_bundled_one() {
        // A node with *no* main output at all fails the *registry's* rule, not a second copy
        // of it. If the two ever diverge, a package would install a node the palette cannot
        // use. Note the distinction this test pins down: `open: false` is legal (a node may
        // declare a main output nobody connects to, and the lint only requires one to
        // *exist*), so the fixture empties the list rather than closing a port.
        let mut manifest = good();
        manifest.nodes[0].outputs.clear();
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "node_no_main_output"),
            "{:?}",
            validation.findings
        );
    }

    #[test]
    fn a_closed_main_output_is_legal_which_is_not_the_same_as_a_missing_one() {
        // The companion to the test above, and the reason the fixture could not simply close
        // a port: `open` is about *connecting*, `main output` is about *existing*. A package
        // that declares a sink node — a node whose output nothing consumes — is valid.
        let mut manifest = good();
        manifest.nodes[0].outputs[0].open = false;
        let validation = validate(&manifest);
        assert!(validation.is_installable(), "{:?}", validation.findings);
    }

    #[test]
    fn a_secret_field_on_a_credential_type_must_be_never_logged() {
        let mut manifest = good();
        manifest.credentials[0].fields[0].never_log = false;
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "credential_secret_field_logged"),
            "{:?}",
            validation.findings
        );
    }

    #[test]
    fn an_oauth2_type_without_endpoints_is_refused() {
        let mut manifest = good();
        manifest.credentials[0].kind = "oauth2".to_string();
        let validation = validate(&manifest);
        assert!(!validation.is_installable());
        assert!(
            validation
                .findings
                .iter()
                .any(|finding| finding.code == "package_credential_oauth_missing")
        );
    }

    #[test]
    fn versions_compare_numerically_and_a_prerelease_sorts_below_its_release() {
        let pairs = [
            ("1.10.0", "1.9.0", true),
            ("1.9.0", "1.10.0", false),
            ("1.2.0", "1.2.0", true),
            ("2.0.0-beta.1", "2.0.0", false),
            ("2.0.0", "2.0.0-beta.1", true),
            ("1.2.1", "1.2", true),
        ];
        for (candidate, installed, expected) in pairs {
            let candidate = Version::parse(candidate).expect("parses");
            let installed = Version::parse(installed).expect("parses");
            assert_eq!(
                candidate.is_newer_or_equal(&installed),
                expected,
                "{candidate} vs {installed}"
            );
        }
    }

    #[test]
    fn a_version_that_is_not_one_is_refused_rather_than_compared_as_text() {
        assert!(Version::parse("latest").is_err());
        assert!(Version::parse("1.2.3.4").is_err());
        assert!(Version::parse("1.x").is_err());
        assert!(Version::parse("").is_err());
        assert!(Version::parse("1.2.0-rc.1+build.5").is_ok());
    }

    #[test]
    fn a_downgrade_is_refused_with_the_installed_version_named() {
        let installed = Version::parse("1.2.0").expect("parses");
        let requested = Version::parse("1.1.0").expect("parses");
        let error = ensure_not_a_downgrade(&installed, &requested).expect_err("refused");
        assert_eq!(error.code(), "node_package_version_unsupported");
        assert!(error.to_string().contains("1.2.0"), "{error}");
    }

    #[test]
    fn the_checksum_is_over_content_so_reformatting_does_not_read_as_a_new_package() {
        let manifest = good();
        let mut reordered = manifest.clone();
        // Same values, different field order in the struct: the canonical value walk sorts
        // object keys, so the two must hash the same.
        reordered.name = manifest.name.clone();
        reordered.key = manifest.key.clone();
        assert_eq!(checksum(&manifest), checksum(&reordered));

        let mut changed = manifest.clone();
        changed.version = "0.2.0".to_string();
        assert_ne!(checksum(&manifest), checksum(&changed));
    }

    #[test]
    fn packing_refuses_what_validation_refuses_and_names_the_finding() {
        let mut manifest = good();
        manifest.nodes[0].icon = String::new();
        let error = pack(&manifest).expect_err("refused");
        assert!(error.contains("node_icon_missing"), "{error}");

        let packed = pack(&good()).expect("packs");
        assert_eq!(packed.format, PACKED_FORMAT);
        assert_eq!(packed.checksum, checksum(&packed.manifest));
    }

    #[test]
    fn removal_names_its_dependents_and_never_edits_them() {
        let plan = removal_plan(
            "acme",
            &["acme.echo".to_string(), "acme.send_message".to_string()],
            &[
                (
                    Uuid::new_v4(),
                    "Nightly report".to_string(),
                    vec!["acme.send_message".to_string()],
                ),
                (
                    Uuid::new_v4(),
                    "Alerts".to_string(),
                    vec![
                        "acme.send_message".to_string(),
                        "manual_trigger".to_string(),
                    ],
                ),
                (Uuid::new_v4(), "Unrelated".to_string(), vec![]),
            ],
        );
        assert!(plan.has_dependents());
        assert_eq!(plan.affected_workflows.len(), 2);
        // Only the package's own nodes are listed, so a workflow's other nodes are not implied
        // to be affected.
        assert_eq!(
            plan.affected_workflows[0].node_keys,
            vec!["acme.send_message"]
        );
        let sentence = plan.describe();
        assert!(sentence.contains("Alerts"), "{sentence}");
        assert!(
            sentence.contains("disabled rather than deleted"),
            "{sentence}"
        );
    }

    #[test]
    fn removing_an_unused_package_says_so_instead_of_inventing_dependents() {
        let plan = removal_plan("acme", &["acme.echo".to_string()], &[]);
        assert!(!plan.has_dependents());
        assert!(plan.describe().contains("no workflow uses them"));
    }

    #[test]
    fn a_scaffold_of_an_invalid_key_falls_back_rather_than_teaching_a_broken_package() {
        let scaffold = scaffold("Not A Key");
        assert!(validate(&scaffold.manifest).is_installable());
        assert_eq!(scaffold.manifest.key, "acme-tools");
    }

    #[test]
    fn the_scaffold_carries_one_fixture_per_node() {
        let scaffold = scaffold("acme");
        let keys: Vec<&str> = scaffold
            .fixtures
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, vec!["echo", "send_message"]);
        for (key, sample) in &scaffold.fixtures {
            assert!(sample.is_object(), "fixture {key} is not an object");
        }
    }

    #[test]
    fn package_keys_use_the_ledger_shape() {
        assert!(package_key_is_valid("acme-tools"));
        assert!(!package_key_is_valid("Acme"));
        assert!(!package_key_is_valid("acme.tools"));
        assert!(!package_key_is_valid("1acme"));
        assert!(!package_key_is_valid(""));
    }
}
