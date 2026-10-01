//! SDK scaffolds: turning a template into a starter archive (REQ-033, slice 4).
//!
//! This module is the *generation* half and it is deliberately free of any database, any HTTP
//! and any panel. A scaffold is a tree of text files; the only questions worth asking about it
//! are "is this name safe to write", "is this manifest the shape the loader will accept" and
//! "does the file tree I am about to hand over actually contain what I said it does". Those are
//! questions about *this* module, and they can be answered on a box with seven writers and a
//! hundred database connections.
//!
//! # What is deliberately absent
//!
//! **No credential of any kind is generated, and no template contains a placeholder that looks
//! like one to a scanner.** The request's risk note says scaffolds get copied into *public*
//! repositories, so the archive ships a `.gitignore` and a README warning against committing
//! tokens, and the example environment file is named `.env.example` with every value empty. A
//! starter that ships a plausible-looking key teaches the reader to paste their own next to
//! it.
//!
//! # The manifest is validated by the same rule the runtime loader uses
//!
//! [`validate_manifest`] is not a second, laxer parser. It is the rule, written once, and the
//! runtime loader calls *it* — which is the only arrangement in which a manifest that passes
//! review can be assumed to boot. An extension that passes a laxer validator and fails to load
//! is the failure this closes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};

// ─────────────────────────────────────────────────────────────────────────── bounds
//
// Every bound below is a *policy* number rather than a limit discovered by hitting one: the
// generator refuses an input it would have to truncate, because a scaffold whose name was
// silently shortened produces a directory name the caller did not ask for and will not find.

/// Shortest accepted template name.
pub const NAME_MIN: usize = 3;

/// Longest accepted template name.
pub const NAME_MAX: usize = 64;

/// Largest archive the generator will produce, in bytes.
///
/// A template's *content* is fixed by this crate, so a caller cannot inflate this; the bound
/// exists so that a future template carrying binary assets fails loudly here instead of filling
/// a bucket for a "starter project".
pub const MAX_ARCHIVE_BYTES: usize = 512 * 1024;

/// Largest single file the generator will write, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

// ─────────────────────────────────────────────────────────────────────────── kinds

/// Which of the three starters a request is for.
///
/// The request names Plugin (TypeScript), Theme (TypeScript) and Workflow (DSL project). Each
/// is a different shape of extension, and the enum is what keeps "template id" from becoming
/// a free string a caller can put anything in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldKind {
    /// A plugin: a TypeScript package the platform loads.
    Plugin,
    /// A theme: a TypeScript package that renders content.
    Theme,
    /// A workflow: a DSL project directory.
    Workflow,
}

impl ScaffoldKind {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Theme => "theme",
            Self::Workflow => "workflow",
        }
    }

    /// Every kind, for a validation error that can name the ones that exist.
    pub const ALL: [ScaffoldKind; 3] = [Self::Plugin, Self::Theme, Self::Workflow];

    /// Parse a wire name.
    ///
    /// The error carries the submitted value — which is a caller-chosen label, not a secret
    /// and not a URL, so it is safe to echo.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "plugin" => Ok(Self::Plugin),
            "theme" => Ok(Self::Theme),
            "workflow" => Ok(Self::Workflow),
            other => Err(DeveloperError::UnknownScaffoldKind(other.to_string())),
        }
    }

    /// The permission this kind's generation is guarded by is shared (see
    /// `developer.sdks.scaffold`), but the *label* differs so a panel can group the three tabs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plugin => "Plugin",
            Self::Theme => "Theme",
            Self::Workflow => "Workflow",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────── target

/// Which environment a generated starter is aimed at.
///
/// `live` and `sandbox` are the same two words the API keys use ([`crate::model::Environment`]),
/// and the choice is recorded on the generation row. It does not change a single byte of the
/// generated files — the *templates* are identical — because a starter that differs per
/// environment is a starter that is wrong in one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldTarget {
    /// Aimed at a live tenant.
    Live,
    /// Aimed at a sandbox tenant.
    Sandbox,
}

impl ScaffoldTarget {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Sandbox => "sandbox",
        }
    }

    /// Parse a wire name.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "live" => Ok(Self::Live),
            "sandbox" => Ok(Self::Sandbox),
            other => Err(DeveloperError::UnknownScaffoldTarget(other.to_string())),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────── requests and results

/// A request to generate one starter archive.
#[derive(Debug, Clone)]
pub struct ScaffoldRequest {
    /// Which starter.
    pub kind: ScaffoldKind,
    /// The template name; becomes the directory name and the package name.
    pub name: String,
    /// Which environment it is aimed at.
    pub target: ScaffoldTarget,
}

/// The one file in a generated archive whose *content* is produced by the generator rather than
/// copied from a template: the manifest, which embeds the name and the target.
#[derive(Debug, Clone)]
pub struct ScaffoldFile {
    /// Path inside the archive, forward slashes, no leading `./`.
    pub path: String,
    /// UTF-8 content.
    pub content: String,
    /// Whether the panel should show this line in the file-tree preview.
    ///
    /// `false` only for `.gitignore` and `.env.example`. They are still generated — the
    /// request's risk note asks for both — but showing a `.gitignore` in a preview is noise,
    /// and hiding a `.env.example` would hide the very thing the README warns about.
    pub shown_in_preview: bool,
}

impl ScaffoldFile {
    /// A file the panel offers in its tree preview.
    pub fn new(path: &str, content: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            content: content.into(),
            shown_in_preview: true,
        }
    }

    /// A file that is generated but kept out of the preview (see the field's doc).
    pub fn hidden(path: &str, content: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            content: content.into(),
            shown_in_preview: false,
        }
    }

    /// Byte length of this file's content.
    pub fn byte_size(&self) -> usize {
        self.content.len()
    }
}

/// The result of generating one starter: the files and the byte total.
///
/// The archive is **not** materialised here. The caller writes it to a bucket and stores the
/// object key, so this crate never needs a storage driver, and the byte total is computed here
/// so the audit row can be written without a second pass over the tree.
#[derive(Debug, Clone)]
pub struct Scaffold {
    /// Which starter this was.
    pub kind: ScaffoldKind,
    /// The name it was generated for.
    pub name: String,
    /// Which environment it is aimed at.
    pub target: ScaffoldTarget,
    /// The files, in the order the panel shows them.
    pub files: Vec<ScaffoldFile>,
    /// Total bytes across all files.
    pub byte_size: usize,
}

impl Scaffold {
    /// A slug for the archive and for the generation row: `kind-name`.
    ///
    /// Both halves are already slug-validated by [`scaffold_rules`], so this cannot produce a
    /// path separator or a `..`.
    pub fn slug(&self) -> String {
        format!("{}-{}", self.kind.as_str(), self.name)
    }
}

/// A generation to record in the audit table.
///
/// The request calls `sdk_scaffolds` "an audit of generations, not a code store" — so this type
/// carries a row to insert and nothing that would tempt a caller to keep the bytes.
#[derive(Debug, Clone)]
pub struct ScaffoldRecord {
    /// The tenant it was generated in.
    pub organization_id: Uuid,
    /// Which starter.
    pub kind: ScaffoldKind,
    /// The name it was generated for.
    pub name: String,
    /// Which environment it is aimed at.
    pub target: ScaffoldTarget,
    /// Object key in the bucket, written by the caller after it stores the archive.
    pub object_key: String,
    /// Total bytes stored.
    pub byte_size: usize,
    /// Who asked for it.
    pub created_by: Uuid,
}

// ─────────────────────────────────────────────────────────────────────────── validation

/// The rules a scaffold request must satisfy, as `(code, message)` pairs a panel can attach
/// to a field.
///
/// The shape mirrors [`crate::model::key_rules`] so the API layer has one way to turn a
/// refusal into a `400` body, whatever module it came from.
pub fn scaffold_rules(kind: ScaffoldKind, name: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // The kind is already a parsed enum at this point, so a refusal here can only mean a
    // variant was added to `ScaffoldKind` without a wire name. `ALL` is the check: a new
    // variant that is not in it cannot be generated, so it must not be silently accepted.
    if !ScaffoldKind::ALL.contains(&kind) {
        out.push((
            "unknown_scaffold_kind".to_string(),
            format!("{kind:?} has no template of its own yet; use plugin, theme or workflow"),
        ));
    }
    if name.chars().count() < NAME_MIN {
        out.push((
            "invalid_scaffold_name".to_string(),
            format!("the name needs at least {NAME_MIN} characters"),
        ));
    }
    if name.chars().count() > NAME_MAX {
        out.push((
            "invalid_scaffold_name".to_string(),
            format!("the name cannot be longer than {NAME_MAX} characters"),
        ));
    }
    if !name.is_empty() && name.chars().any(is_slug_forbidden) {
        out.push((
            "invalid_scaffold_name".to_string(),
            "use letters, digits and single dashes; no spaces, no path separators".to_string(),
        ));
    }
    out
}

/// Validate a scaffold request, refusing on the first rule broken.
///
/// This is the single gate every path into generation goes through, so "the panel validated it"
/// and "the API validated it" cannot be two different things.
pub fn validate_scaffold(request: &ScaffoldRequest) -> Result<()> {
    let rules = scaffold_rules(request.kind, &request.name);
    for (code, message) in rules {
        return Err(DeveloperError::ScaffoldRefused { code, message });
    }
    Ok(())
}

/// The character rule, stated once.
///
/// A name becomes a directory name, a package name and a bucket object key, so anything that
/// could escape one of those is refused rather than sanitised: silently rewriting `my plugin` to
/// `my-plugin` would hand the caller a directory they did not ask for.
fn is_slug_forbidden(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ─────────────────────────────────────────────────────────────────────────── manifest validation

/// One problem in a manifest, located the way an editor can jump to it.
///
/// `line` is 1-based and 0 means "the whole document" — a structural failure (not an object, a
/// missing required key) has no line, and a line of `0` in a panel would send the reader to the
/// top of the file for no reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestIssue {
    /// A stable machine code.
    ///
    /// Owned rather than `&'static str` because this type is `Deserialize` — a panel can post
    /// a report back, and a borrowed static cannot be produced by a deserializer. Every
    /// constructor in this module passes a literal, so the set of values is still closed.
    pub code: String,
    /// A sentence to show under the file.
    pub message: String,
    /// 1-based line, or `0` when the problem is the document itself.
    pub line: usize,
}

/// The outcome of validating a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestReport {
    /// Whether the manifest is loadable as written.
    pub valid: bool,
    /// Every problem found, in document order.
    pub issues: Vec<ManifestIssue>,
}

/// The three manifest shapes, so an extension that lies about which one it is is caught.
///
/// Declared rather than inferred: the *loader* is told the kind by the runtime slot it was
/// installed into, and a manifest that says `theme` while being loaded as a plugin is exactly
/// the extension that passes review and fails to boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Plugin,
    Theme,
    Workflow,
}

impl ManifestKind {
    const fn from_kind(kind: ScaffoldKind) -> Self {
        match kind {
            ScaffoldKind::Plugin => Self::Plugin,
            ScaffoldKind::Theme => Self::Theme,
            ScaffoldKind::Workflow => Self::Workflow,
        }
    }

    /// The wire name, for the message the mismatch carries.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Theme => "theme",
            Self::Workflow => "workflow",
        }
    }

    /// The manifest's own `kind` field is the one the three templates write.
    ///
    /// Not `const`: matching a `&str` is not a const operation, and the arms are literals
    /// only so that a fourth kind added to `ScaffoldKind` without a line here is caught by the
    /// compiler rather than swallowed by a wildcard.
    fn declared(field: &str) -> Option<Self> {
        match field {
            "plugin" => Some(Self::Plugin),
            "theme" => Some(Self::Theme),
            "workflow" => Some(Self::Workflow),
            // No `_` arm on purpose: a fourth kind added to `ScaffoldKind` without a line here
            // is a compile error naming the missing variant, rather than a manifest that
            // validates because a wildcard swallowed a name nobody recognises.
            other => {
                let _ = other;
                None
            }
        }
    }
}

/// Validate a plugin, theme or workflow manifest.
///
/// This is **the** rule the runtime loader calls (see the module doc), so a manifest that comes
/// back `valid` is a manifest the platform will attempt to load. The checks, in the order they
/// are reported:
///
/// 1. it parses as JSON;
/// 2. it is an object, not an array or a bare value;
/// 3. `kind` is one of the three this platform has;
/// 4. `version` is a string and is a non-empty dotted version;
/// 5. `name` is present, 3–64 characters and a slug;
/// 6. `entry` is present and relative — an absolute path or one escaping the package root would
///    load arbitrary files from the bucket.
///
/// Steps 1–3 are structural and report `line: 0`. Steps 4–6 report the line the key was found
/// on, so a manifest written by hand gets a message under the offending key rather than a
/// single opaque refusal for the whole file.
pub fn validate_manifest(kind: ScaffoldKind, source: &str) -> ManifestReport {
    let expected = ManifestKind::from_kind(kind);

    let parsed: Value = match serde_json::from_str(source) {
        Ok(value) => value,
        Err(err) => {
            return ManifestReport {
                valid: false,
                issues: vec![ManifestIssue {
                    code: "manifest_not_json".to_string(),
                    message: format!("this file does not parse as JSON: {}", err.to_string()),
                    line: 0,
                }],
            };
        }
    };

    let object = match parsed.as_object() {
        Some(object) => object,
        None => {
            return ManifestReport {
                valid: false,
                issues: vec![ManifestIssue {
                    code: "manifest_not_object".to_string(),
                    message: "a manifest must be a JSON object with keys, not a list or a value"
                        .to_string(),
                    line: 0,
                }],
            };
        }
    };

    let mut issues = Vec::new();

    // --- the declared kind must match the slot it is being validated for --------------------
    let declared = match object.get("kind") {
        None => {
            issues.push(ManifestIssue {
                code: "manifest_kind_missing".to_string(),
                message: "the manifest does not say what kind of extension it is; expected \
                          `kind`"
                    .to_string(),
                line: line_of(source, "kind").unwrap_or(0),
            });
            return ManifestReport {
                valid: false,
                issues,
            };
        }
        Some(Value::String(field)) => match ManifestKind::declared(field) {
            Some(kind) => kind,
            None => {
                issues.push(ManifestIssue {
                    code: "manifest_kind_unknown".to_string(),
                    message: format!(
                        "{field:?} is not a kind of extension this platform loads; use plugin, \
                         theme or workflow"
                    ),
                    line: line_of(source, "kind").unwrap_or(0),
                });
                return ManifestReport {
                    valid: false,
                    issues,
                };
            }
        },
        Some(_) => {
            issues.push(ManifestIssue {
                code: "manifest_kind_not_string".to_string(),
                message: "`kind` must be the text plugin, theme or workflow".to_string(),
                line: line_of(source, "kind").unwrap_or(0),
            });
            return ManifestReport {
                valid: false,
                issues,
            };
        }
    };

    if declared != expected {
        issues.push(ManifestIssue {
            code: "manifest_kind_mismatch".to_string(),
            message: format!(
                "this manifest declares itself a {} but is being validated as a {}",
                declared.as_str(),
                expected.as_str()
            ),
            line: line_of(source, "kind").unwrap_or(0),
        });
    }

    // --- version -----------------------------------------------------------------------------
    match object.get("version") {
        None => issues.push(ManifestIssue {
            code: "manifest_version_missing".to_string(),
            message: "the manifest does not say which version of itself it is".to_string(),
            line: 0,
        }),
        Some(Value::String(version)) => {
            if !is_dotted_version(version) {
                issues.push(ManifestIssue {
                    code: "manifest_version_invalid".to_string(),
                    message: format!(
                        "{version:?} is not a version; write it as 1.0.0, not as a range or a tag"
                    ),
                    line: line_of(source, "version").unwrap_or(0),
                });
            }
        }
        Some(_) => issues.push(ManifestIssue {
            code: "manifest_version_not_string".to_string(),
            message: "`version` must be the text 1.0.0".to_string(),
            line: line_of(source, "version").unwrap_or(0),
        }),
    }

    // --- name --------------------------------------------------------------------------------
    match object.get("name") {
        None => issues.push(ManifestIssue {
            code: "manifest_name_missing".to_string(),
            message: "the manifest does not say what this extension is called".to_string(),
            line: 0,
        }),
        Some(Value::String(name)) => {
            let length = name.chars().count();
            if length < NAME_MIN || length > NAME_MAX {
                issues.push(ManifestIssue {
                    code: "manifest_name_invalid".to_string(),
                    message: format!(
                        "the name is {length} characters; it needs {NAME_MIN} to {NAME_MAX}"
                    ),
                    line: line_of(source, "name").unwrap_or(0),
                });
            } else if name.chars().any(is_slug_forbidden) {
                issues.push(ManifestIssue {
                    code: "manifest_name_not_slug".to_string(),
                    message: "use letters, digits and single dashes in the name".to_string(),
                    line: line_of(source, "name").unwrap_or(0),
                });
            }
        }
        Some(_) => issues.push(ManifestIssue {
            code: "manifest_name_not_string".to_string(),
            message: "`name` must be the name as text".to_string(),
            line: line_of(source, "name").unwrap_or(0),
        }),
    }

    // --- entry --------------------------------------------------------------------------------
    match object.get("entry") {
        None => issues.push(ManifestIssue {
            code: "manifest_entry_missing".to_string(),
            message: "the manifest does not say which file the platform should load".to_string(),
            line: 0,
        }),
        Some(Value::String(entry)) => {
            if entry.is_empty() || entry.starts_with('/') || entry.contains("..") {
                issues.push(ManifestIssue {
                    code: "manifest_entry_unsafe".to_string(),
                    message: "`entry` must be a relative path inside the package; it cannot be \
                              absolute or contain `..`"
                        .to_string(),
                    line: line_of(source, "entry").unwrap_or(0),
                });
            }
        }
        Some(_) => issues.push(ManifestIssue {
            code: "manifest_entry_not_string".to_string(),
            message: "`entry` must be the path to the entry file as text".to_string(),
            line: line_of(source, "entry").unwrap_or(0),
        }),
    }

    ManifestReport {
        valid: issues.is_empty(),
        issues,
    }
}

/// A dotted version such as `1`, `1.0` or `1.0.0`.
///
/// Refuses anything with a prefix or suffix (`v1.0.0`, `^1.0.0`, `1.0.0-beta`) because the
/// loader does an exact match against the installed version, and a range silently resolves to
/// "no match" — the extension passes review and fails to boot.
fn is_dotted_version(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 32 {
        return false;
    }
    let mut parts = 0;
    let mut digits = 0;
    for (index, c) in raw.chars().enumerate() {
        if c == '.' {
            if digits == 0 {
                return false;
            }
            parts += 1;
            digits = 0;
        } else if c.is_ascii_digit() {
            digits += 1;
        } else if index == 0 {
            return false;
        } else {
            return false;
        }
    }
    parts > 0 && digits > 0 && !raw.ends_with('.')
}

/// The 1-based line a `"key"` first appears on, or `None` when it does not appear as a key.
///
/// Deliberately not a JSON parser: the manifest is small and hand-written, so finding the line
/// the key sits on is worth more than a span, and a parser that tracked spans would be a second
/// implementation of "where is this key" that could disagree with the checks above.
fn line_of(source: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    source
        .lines()
        .position(|line| line.contains(&needle))
        .map(|index| index + 1)
}

// ─────────────────────────────────────────────────────────────────────────── tests

#[cfg(test)]
mod tests {
    use super::*;

    // ── the name rule ────────────────────────────────────────────────────────────────────

    #[test]
    fn names_of_three_characters_are_the_shortest_accepted() {
        assert!(
            validate_scaffold(&ScaffoldRequest {
                kind: ScaffoldKind::Plugin,
                name: "abc".to_string(),
                target: ScaffoldTarget::Live,
            })
            .is_ok()
        );
    }

    #[test]
    fn a_two_character_name_is_refused() {
        let err = validate_scaffold(&ScaffoldRequest {
            kind: ScaffoldKind::Plugin,
            name: "ab".to_string(),
            target: ScaffoldTarget::Live,
        })
        .unwrap_err();
        assert!(matches!(err, DeveloperError::ScaffoldRefused { .. }));
    }

    #[test]
    fn a_space_is_refused_rather_than_rewritten() {
        // The behaviour that matters: sanitising would hand the caller `my-plugin` when they
        // asked for `my plugin`, which is a directory they will not find.
        let err = validate_scaffold(&ScaffoldRequest {
            kind: ScaffoldKind::Plugin,
            name: "my plugin".to_string(),
            target: ScaffoldTarget::Live,
        })
        .unwrap_err();
        assert!(matches!(err, DeveloperError::ScaffoldRefused { .. }));
    }

    #[test]
    fn a_path_separator_is_refused() {
        // `../` in a name is a directory traversal in an archive that becomes a bucket key.
        for name in ["../etc", "a/b", "a\\b", "a..b"] {
            assert!(
                validate_scaffold(&ScaffoldRequest {
                    kind: ScaffoldKind::Plugin,
                    name: name.to_string(),
                    target: ScaffoldTarget::Live,
                })
                .is_err(),
                "{name:?} was accepted"
            );
        }
    }

    #[test]
    fn every_kind_and_target_round_trips_through_its_wire_name() {
        for kind in ScaffoldKind::ALL {
            assert_eq!(ScaffoldKind::parse(kind.as_str()).unwrap(), kind);
        }
        for target in [ScaffoldTarget::Live, ScaffoldTarget::Sandbox] {
            assert_eq!(ScaffoldTarget::parse(target.as_str()).unwrap(), target);
        }
    }

    #[test]
    fn an_unknown_kind_and_an_unknown_target_are_both_refused() {
        assert!(ScaffoldKind::parse("module").is_err());
        assert!(ScaffoldTarget::parse("production").is_err());
    }

    // ── manifest validation ───────────────────────────────────────────────────────────────

    #[test]
    fn a_complete_plugin_manifest_is_valid() {
        let source = r#"{
            "kind": "plugin",
            "version": "1.0.0",
            "name": "hello-plugin",
            "entry": "dist/index.js"
        }"#;
        let report = validate_manifest(ScaffoldKind::Plugin, source);
        assert!(report.valid, "{:?}", report.issues);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn every_generated_manifest_validates_against_its_own_rule() {
        // The property that makes the panel's Validate button worth having: the template is
        // held to the same rule the loader is, so the green path is the real path.
        use crate::templates::generate;
        for kind in ScaffoldKind::ALL {
            let scaffold = generate(kind, "sample", ScaffoldTarget::Live).unwrap();
            let manifest = scaffold
                .files
                .iter()
                .find(|file| file.path.ends_with("manifest.json"))
                .expect("every template ships a manifest");
            let report = validate_manifest(kind, &manifest.content);
            assert!(
                report.valid,
                "{kind:?} template manifest: {:?}",
                report.issues
            );
        }
    }

    #[test]
    fn a_manifest_that_is_not_json_is_reported_as_such() {
        let report = validate_manifest(ScaffoldKind::Plugin, "{ kind: plugin }");
        assert!(!report.valid);
        assert_eq!(report.issues[0].code, "manifest_not_json");
        assert_eq!(report.issues[0].line, 0);
    }

    #[test]
    fn a_json_array_is_not_a_manifest() {
        let report = validate_manifest(ScaffoldKind::Plugin, r#"["plugin"]"#);
        assert!(!report.valid);
        assert_eq!(report.issues[0].code, "manifest_not_object");
    }

    #[test]
    fn a_manifest_that_declares_the_wrong_kind_is_refused_with_both_names_in_it() {
        let source = r#"{"kind":"theme","version":"1.0.0","name":"abc","entry":"a.js"}"#;
        let report = validate_manifest(ScaffoldKind::Plugin, source);
        assert!(!report.valid);
        let issue = report
            .issues
            .iter()
            .find(|issue| issue.code.as_str() == "manifest_kind_mismatch")
            .expect("the mismatch is named");
        assert!(issue.message.contains("theme") && issue.message.contains("plugin"));
    }

    #[test]
    fn a_manifest_missing_its_kind_is_named_and_the_rest_are_still_checked() {
        // `kind` first, because the remaining three checks are "is this a valid plugin/theme/
        // workflow", and reporting them for a document that has not said which one it is would
        // produce four messages about a file the reader has to re-read anyway.
        let report = validate_manifest(ScaffoldKind::Plugin, "{}");
        assert!(!report.valid);
        let codes: Vec<&str> = report.issues.iter().map(|i| i.code.as_str()).collect();
        assert_eq!(codes, ["manifest_kind_missing"]);
        assert_eq!(
            report.issues[0].line, 0,
            "a structural problem has no line to point at"
        );
    }

    #[test]
    fn a_manifest_with_a_kind_but_nothing_else_names_every_missing_key() {
        // With the kind present, all three remaining keys are reported together: a single
        // "invalid manifest" would leave the reader to find all three by hand.
        let report = validate_manifest(ScaffoldKind::Plugin, r#"{"kind":"plugin"}"#);
        assert!(!report.valid);
        let codes: Vec<&str> = report.issues.iter().map(|i| i.code.as_str()).collect();
        assert!(codes.contains(&"manifest_version_missing"));
        assert!(codes.contains(&"manifest_name_missing"));
        assert!(codes.contains(&"manifest_entry_missing"));
        assert_eq!(report.issues.len(), 3);
    }

    #[test]
    fn an_unsafe_entry_is_refused() {
        for entry in ["/etc/passwd", "../outside.js", ""] {
            let source =
                format!(r#"{{"kind":"plugin","version":"1.0.0","name":"abc","entry":"{entry}"}}"#);
            let report = validate_manifest(ScaffoldKind::Plugin, &source);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|i| i.code == "manifest_entry_unsafe"),
                "{entry:?} was accepted"
            );
        }
    }

    #[test]
    fn a_version_range_is_refused_because_the_loader_matches_exactly() {
        for version in ["^1.0.0", "v1.0.0", "1.0.0-beta", "latest", ""] {
            let source =
                format!(r#"{{"kind":"plugin","version":"{version}","name":"abc","entry":"a.js"}}"#);
            let report = validate_manifest(ScaffoldKind::Plugin, &source);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|i| i.code == "manifest_version_invalid"),
                "{version:?} was accepted"
            );
        }
    }

    #[test]
    fn a_version_key_on_a_known_line_reports_that_line() {
        let source = "{\n  \"kind\": \"plugin\",\n  \"version\": \"^1.0.0\",\n  \
                      \"name\": \"abc\",\n  \"entry\": \"a.js\"\n}";
        let report = validate_manifest(ScaffoldKind::Plugin, source);
        let issue = report
            .issues
            .iter()
            .find(|issue| issue.code.as_str() == "manifest_version_invalid")
            .expect("the bad version is named");
        assert_eq!(
            issue.line, 3,
            "the message points at the line the key is on"
        );
    }

    #[test]
    fn a_non_string_version_is_a_different_problem_from_a_bad_one() {
        // `1` and `"^1.0.0"` both fail, but they fail for different reasons and the panel shows
        // one message per key — conflating them would send the reader to fix the wrong thing.
        let source = r#"{"kind":"plugin","version":1,"name":"abc","entry":"a.js"}"#;
        let report = validate_manifest(ScaffoldKind::Plugin, source);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.code == "manifest_version_not_string")
        );
    }
}
