//! Persisted documents: the canonical hash, the registry row shape, and the allowlist decisions.
//!
//! ## Why hashing is a *library* decision and not a handler detail
//!
//! Three places need the same hash and none of them can afford to compute it differently: the
//! manager screen that rejects a duplicate "with a link to the existing row", the endpoint that
//! executes a registered document by hash, and CI, which registers the documents a release needs.
//! A hash computed in a handler is a hash that drifts. So the digest is here, and it is
//! **canonical**: whitespace and comments are removed before the digest is taken, so a document
//! that differs only in formatting gets the hash of the query a developer actually wrote.
//!
//! ## Why the hash is truncated to 32 hex characters
//!
//! The request says: *"execute it later by `documentId` or hash with variables"* — a hash is a
//! lookup key and an identifier a client copies out of the manager. A full SHA-256 is 64
//! characters, which is a key rather than a handle, and full-digest keys end up in URL query
//! strings. 128 bits is far past any collision concern for a per-tenant set of documents, and the
//! row stores the full digest too, so nothing is lost — the short form is only what a client
//! types. The uniqueness constraint is on the **full** digest, so a truncation can never merge two
//! registered documents.
//!
//! ## The allowlist is fail-closed by construction
//!
//! `persisted_only` refuses anything that is not registered. A revoked document is not
//! registered for execution purposes, so a revocation takes effect on the next read — which is
//! what the acceptance line means by *"within one cache cycle"*. There is deliberately no code
//! path that treats an unknown document as "probably fine": an allowlist that fails open is not an
//! allowlist.

use crate::error::{Code, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// How many hex characters of the digest a client types.
pub const SHORT_HASH_LEN: usize = 32;

/// A document's registry identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentId {
    /// The full lower-case hex digest. Unique inside one organization.
    pub hash: String,
    /// The copyable short form. A prefix of [`Self::hash`], so it can never name a different
    /// document than the full digest it came from.
    pub short_hash: String,
    /// How many bytes the document is, before canonicalisation — what the byte cap is checked
    /// against.
    pub byte_len: usize,
}

impl DocumentId {
    /// The document a registry entry is keyed by.
    ///
    /// The digest is taken over the **canonical** text, so two formattings of the same query
    /// share one entry. That is the difference between a registry and a text archive: the client
    /// asks for its query by the query it means, and reformatting on the client side does not
    /// create a second document to prune.
    pub fn of(source: &str) -> Self {
        let canonical = canonicalize(source);
        let full = hex(Sha256::digest(canonical.as_bytes()));
        Self {
            short_hash: full[..SHORT_HASH_LEN].to_owned(),
            hash: full,
            byte_len: source.len(),
        }
    }

    /// Whether `requested` names this document.
    ///
    /// Accepts the full digest, the short form, and a unique prefix of either — because a client
    /// that pasted a truncated hash out of a log is asking the question the endpoint exists to
    /// answer, and refusing it with "not found" would be a `404` about a document that exists.
    /// An **ambiguous** prefix is not a match: two registered documents sharing a prefix means the
    /// caller's reference cannot identify one, and picking the first row in index order would
    /// execute a document the caller did not name.
    pub fn matches(&self, requested: &str) -> bool {
        let requested = requested.trim().to_ascii_lowercase();
        if requested.is_empty() {
            return false;
        }
        self.hash == requested || self.short_hash == requested || self.hash.starts_with(&requested)
    }
}

/// Lower-case hex of a digest.
///
/// Hand-rolled rather than pulling in the `hex` crate for one call site: this crate's dependency
/// list is deliberately short because the whole decision layer is pulled into the playground's
/// cost meter, and one 12-line function is cheaper than another crate in that graph.
fn hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Strip what does not change the meaning of a document.
///
/// Removes comments and collapses runs of whitespace outside string literals to a single space,
/// so `# fetch the shell` and `{ pages { id } }` and `{\n  pages {\n    id\n  }\n}` share a hash.
/// Whitespace **inside** a string argument is meaning (`title: "a  b"` is not `title: "a b"`) and
/// is preserved — a canonicaliser that trims inside strings produces two hashes for one query,
/// which is precisely the duplicate the unique constraint would then have to reject.
pub fn canonicalize(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    // Block-string literals (`"""…"""`) may contain `#` and newlines that are NOT comments.
    let mut block_string = false;

    while let Some(ch) = chars.next() {
        if block_string {
            out.push(ch);
            // A block string closes on `"""`. The opening run already consumed two of the three
            // quotes, so a `""` at this point is the start of the terminator and a lone `"` is
            // content — which is why this cannot be "a quote followed by a quote".
            if ch == '"' && chars.peek() == Some(&'"') {
                out.push(chars.next().expect("peeked"));
                if chars.peek() == Some(&'"') {
                    out.push(chars.next().expect("peeked"));
                    block_string = false;
                }
            }
            continue;
        }
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                // Three quotes open a block string; two or fewer are an empty string. `peek()` for
                // the second and `nth(1)` for the third — `nth` yields the item, not a reference,
                // because `Peekable` mixes the two.
                if chars.peek() == Some(&'"') && chars.clone().nth(1) == Some('"') {
                    out.push(chars.next().expect("peeked"));
                    out.push(chars.next().expect("peeked"));
                    block_string = true;
                } else {
                    in_string = true;
                    out.push(ch);
                }
            }
            '#' => {
                // To end of line — and the newline is deliberately LEFT for the whitespace arm
                // below. Pushing it here kept a raw `\n` in the canonical form, so a commented
                // document hashed differently from the same document without the comment: the
                // comment is not meaning, and the newline it ended on is whitespace like any
                // other, which the arm below collapses to a single space.
                for next in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                // Collapse a run of whitespace into one space, and never emit a leading space.
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                while let Some(next) = chars.peek() {
                    if next.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            c => out.push(c),
        }
    }
    out.trim().to_owned()
}

/// A registered document as the manager lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub short_hash: String,
    /// `query` or `mutation` — the **heaviest** kind in the document, so a mixed document cannot
    /// be filed as a read and slip through a review that only looks at reads.
    pub kind: String,
    pub status: String,
    pub required_for_callers: bool,
    pub hits: i64,
    pub operations: Vec<OperationSummary>,
    /// Whether the installation refuses ad-hoc documents outright.
    pub persisted_only: bool,
}

impl RegistryEntry {
    /// Whether this document may execute right now.
    ///
    /// The two refusals are different answers to different questions and the manager screen has to
    /// be able to tell them apart: a **revoked** document is one this caller or an administrator
    /// turned off and it can be turned back on, while `persisted_only` with the document *not*
    /// registered is an installation-wide policy that lists nothing here at all. Conflating them
    /// produces a manager full of rows that say "revoked" for a flag nobody set.
    pub fn executable(&self) -> bool {
        self.status == "active"
    }

    /// Why this document may not execute, in the words the manager shows.
    ///
    /// `None` when it may — so the screen renders a control rather than an explanation, and the
    /// two are never both present.
    pub fn blocked_reason(&self) -> Option<String> {
        match self.status.as_str() {
            "active" => None,
            "revoked" => Some(format!(
                "`{}` was revoked; clients that send it receive PERSISTED_QUERY_NOT_FOUND until it is activated again",
                self.name
            )),
            other => Some(format!(
                "`{}` is {other}; only an active document executes",
                self.name
            )),
        }
    }

    /// The heaviest operation in the document, for the manager's cost column.
    #[must_use]
    pub fn heaviest(&self) -> Option<&OperationSummary> {
        self.operations
            .iter()
            .max_by_key(|operation| operation.cost)
    }
}

/// One operation inside a registered document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub name: Option<String>,
    pub kind: String,
    /// Priced cost against the default catalogue, captured at registration so the manager shows a
    /// number instead of re-pricing on every row.
    pub cost: u32,
    pub depth: u32,
}

/// Build the registry entries a document produces, priced against `catalogue`.
///
/// A document that fails to parse produces **no** entries and no panic: registration is the one
/// place a client-supplied string arrives from a screen or a CI step, and a manager that crashes on
/// a pasted query has no way to report why.
#[must_use]
pub fn describe(source: &str, catalogue: &crate::cost::Catalogue) -> Vec<OperationSummary> {
    let Ok(document) = crate::document::parse(source) else {
        return Vec::new();
    };
    let limits = crate::limits::Limits::default();
    // The caps lifted. `measure_operation_with` REFUSES rather than reports, and that refusal is
    // correct on the request path — but a manager that refused to price the one dangerous document
    // an administrator needs to read carefully would show a blank cost on precisely the row that
    // matters. So the manager measures against a policy with no caps and enforces nothing.
    let uncapped = crate::limits::Limits {
        max_depth: u32::MAX,
        max_cost: u32::MAX,
        ..limits
    };
    document
        .operations
        .iter()
        .map(|operation| {
            // With the caller's catalogue, not the default one: this is the one place the price is
            // shown BEFORE execution, so an installation that re-weighted a field must see its own
            // number rather than the shipped default's.
            let measured =
                crate::limits::measure_operation_with(&document, operation, &uncapped, catalogue);
            OperationSummary {
                name: operation.name.clone(),
                kind: operation.kind.as_str().to_owned(),
                cost: measured.as_ref().map(|m| m.cost).unwrap_or(0),
                depth: measured.as_ref().map(|m| m.depth).unwrap_or(0),
            }
        })
        .collect()
}

/// The heaviest operation kind in a document, for the registry row's `kind` column.
///
/// A document with both a query and a mutation is filed as a mutation. A registry that files it
/// as a query puts a writing document in the read list, and a review that only looks at reads
/// approves it — so the conservative answer wins the tie.
#[must_use]
pub fn heaviest_kind(source: &str) -> &'static str {
    let Ok(document) = crate::document::parse(source) else {
        return "query";
    };
    document
        .operations
        .iter()
        .any(|operation| operation.is_mutation())
        .then_some("mutation")
        .unwrap_or("query")
}

/// The decision one request makes about a document id or hash.
///
/// Split from the store on purpose: the endpoint needs the *answer*, the screen needs the
/// *reasons*, and neither should have to know that the answer is a lookup. Returning a value
/// rather than a `Result` keeps the caller from inventing a third interpretation — the failure
/// modes below are all outcomes, not errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The document is registered and active; execution may proceed.
    Active(RegistryEntry),
    /// The document is registered but not active.
    Blocked(RegistryEntry, String),
    /// Nothing is registered under that id or hash.
    Unknown(String),
    /// The reference matches more than one registered document, so it cannot identify one.
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
}

impl Lookup {
    /// The wire code a client branches on.
    #[must_use]
    pub fn code(&self) -> Code {
        match self {
            Self::Active(_) => Code::Internal,
            Self::Blocked(..) | Self::Unknown(_) | Self::Ambiguous { .. } => {
                Code::PersistedQueryNotFound
            }
        }
    }

    /// The message a client reads.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Active(_) => "the document is registered and active".to_owned(),
            Self::Blocked(_, reason) => reason.clone(),
            Self::Unknown(requested) => format!(
                "no registered document matches `{requested}`; register it in the developer \
                 portal or run with persisted-only mode off"
            ),
            Self::Ambiguous {
                requested,
                candidates,
            } => format!(
                "`{requested}` matches {} registered documents ({}) — send the full hash",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }

    /// Whether execution may proceed.
    #[must_use]
    pub fn is_executable(&self) -> bool {
        matches!(self, Self::Active(_))
    }
}

/// The allowlist decision for one incoming request.
///
/// This is the function that makes *"Persisted-only mode refuses arbitrary documents with
/// `PERSISTED_QUERY_NOT_FOUND`"* true rather than aspirational: `settings.persisted_only` decides
/// whether the text in the request body is even looked at, and when the flag is on, the ONLY
/// thing that can make a request executable is a lookup that came back `Active`.
///
/// Returns `Ok(None)` when execution may proceed, and the document that authorised it when the
/// decision was made on a registered row — so the caller can attribute the request to a document
/// without looking it up a second time and risking a different answer.
#[must_use]
pub fn admit(
    settings: &crate::settings::Settings,
    lookup: &Lookup,
    registered: Option<&RegistryEntry>,
) -> Result<Option<String>> {
    if !settings.persisted_only {
        return Ok(None);
    }
    match lookup {
        Lookup::Active(_) => Ok(registered.map(|entry| entry.id.clone())),
        Lookup::Blocked(_, reason) => Err(Error::Simple {
            code: Code::PersistedQueryNotFound,
            message: reason.clone(),
        }),
        Lookup::Unknown(requested) => Err(Error::Simple {
            code: Code::PersistedQueryNotFound,
            message: format!(
                "persisted-only mode is on and no registered document matches `{requested}`; \
                 register it in the developer portal first"
            ),
        }),
        Lookup::Ambiguous { .. } => Err(Error::Simple {
            code: Code::PersistedQueryNotFound,
            message: lookup.message(),
        }),
    }
}

/// A registry row's request shape, as the API accepts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub document: String,
    /// Activate on registration. Default `true`, because a document registered and left as a
    /// draft is a dead row: nothing executes it and nothing explains why.
    #[serde(default = "default_active")]
    pub active: bool,
    #[serde(default)]
    pub required_for_callers: bool,
}

fn default_active() -> bool {
    true
}

impl RegisterRequest {
    /// Validate a registration, naming the field that is wrong.
    ///
    /// The name is checked for emptiness and length rather than for uniqueness: two documents may
    /// legitimately share a display name (a client that regenerates them per release), and
    /// refusing the second would push the caller toward a name that carries no meaning.
    #[must_use]
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: "`name` must not be empty".into(),
            });
        }
        if name.chars().count() > 120 {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: "`name` must be at most 120 characters".into(),
            });
        }
        if self.document.trim().is_empty() {
            return Err(Error::Validation {
                code: Code::GraphqlValidationFailed,
                message: "`document` must not be empty".into(),
            });
        }
        Ok(())
    }

    /// The registry identity this registration produces.
    #[must_use]
    pub fn identity(&self) -> DocumentId {
        DocumentId::of(&self.document)
    }
}

/// A client's per-document view: what the manager shows when it explains why a control is
/// unavailable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSummary {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub short_hash: String,
    pub kind: String,
    pub status: String,
    pub required_for_callers: bool,
    pub hits: i64,
    /// Heaviest operation's cost, or `None` when the document defines no operation the pricer
    /// could measure.
    pub cost: Option<u32>,
    pub depth: Option<u32>,
}

impl From<&RegistryEntry> for DocumentSummary {
    fn from(entry: &RegistryEntry) -> Self {
        let heaviest = entry.heaviest();
        Self {
            id: entry.id.clone(),
            name: entry.name.clone(),
            hash: entry.hash.clone(),
            short_hash: entry.short_hash.clone(),
            kind: entry.kind.clone(),
            status: entry.status.clone(),
            required_for_callers: entry.required_for_callers,
            hits: entry.hits,
            cost: heaviest.map(|operation| operation.cost),
            depth: heaviest.map(|operation| operation.depth),
        }
    }
}

/// Documents an installation should prune, with why.
///
/// The request calls out the rot explicitly: *"Persisted-document allowlists rot when clients
/// change: CI registers the documents a release needs and the manager shows unused entries for
/// pruning."* So this is computed, not a filter the screen applies.
#[must_use]
pub fn prunable(entries: &[RegistryEntry], idle_since_hits: i64) -> Vec<DocumentSummary> {
    entries
        .iter()
        .filter(|entry| entry.hits <= idle_since_hits)
        .map(DocumentSummary::from)
        .collect()
}

/// The environments' persisted-only state, as the settings screen edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyView {
    pub persisted_only: bool,
    pub playground_enabled: bool,
}

/// A map of registered hashes, used by the store's cache and by tests that need no database.
pub type Registry = BTreeMap<String, RegistryEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = "{ pages(first: 5) { id title } }";

    #[test]
    fn a_document_hashes_the_same_however_it_is_formatted() {
        let tight = DocumentId::of("{ pages(first: 5) { id title } }");
        let loose = DocumentId::of("{\n  pages(first: 5) {\n    id\n    title\n  }\n}\n");
        let commented = DocumentId::of(
            "# the list a client needs\n{\n  pages(first: 5) { id title } # trailing\n}",
        );
        assert_eq!(tight.hash, loose.hash, "formatting is not meaning");
        assert_eq!(tight.hash, commented.hash, "a comment is not meaning");
        assert_eq!(tight.short_hash, tight.hash[..SHORT_HASH_LEN]);
    }

    #[test]
    fn whitespace_inside_a_string_argument_is_meaning_and_changes_the_hash() {
        // `title: "a  b"` and `title: "a b"` are different queries. A canonicaliser that trims
        // inside strings produces one hash for two queries, and the unique constraint then rejects
        // the second registration as a duplicate — which is a refusal with no true statement in it.
        let one = DocumentId::of(r#"{ pages(first: 1, filter: { title: "a  b" }) { id } }"#);
        let two = DocumentId::of(r#"{ pages(first: 1, filter: { title: "a b" }) { id } }"#);
        assert_ne!(
            one.hash, two.hash,
            "a string literal must survive canonicalisation intact"
        );
    }

    #[test]
    fn a_hash_inside_a_string_is_not_read_as_a_comment() {
        // `r##"…"##`, not `r#"…"#`: the literal contains the two characters `"#`, which is
        // exactly what terminates a one-hash raw string. A test written with the shorter form
        // does not compile, and the assertion it was going to make is the one that guards string
        // literals against comment stripping.
        let hashed = DocumentId::of(r##"{ pages(first: 1, filter: { title: "#1" }) { id } }"##);
        let without = DocumentId::of(r##"{ pages(first: 1, filter: { title: "" }) { id } }"##);
        assert_ne!(
            hashed.hash, without.hash,
            "the `#` inside a string is a character, not a comment marker"
        );
    }

    #[test]
    fn the_full_digest_is_what_is_stored_and_the_short_form_is_its_prefix() {
        let id = DocumentId::of(QUERY);
        assert_eq!(id.hash.len(), 64, "the row keeps the whole digest");
        assert_eq!(id.short_hash.len(), SHORT_HASH_LEN);
        assert!(id.hash.starts_with(&id.short_hash));
        assert!(
            id.matches(&id.short_hash),
            "the short form names the document"
        );
        assert!(id.matches(&id.hash), "the full digest names the document");
        assert!(id.matches(&id.hash[..12]), "a unique prefix names it too");
    }

    #[test]
    fn an_empty_reference_names_nothing() {
        let id = DocumentId::of(QUERY);
        assert!(!id.matches(""));
        assert!(!id.matches("   "));
        assert!(!id.matches("nonsense"));
    }

    fn entry(status: &str) -> RegistryEntry {
        RegistryEntry {
            id: "doc-1".into(),
            name: "Page list".into(),
            hash: DocumentId::of(QUERY).hash,
            short_hash: DocumentId::of(QUERY).short_hash,
            kind: "query".into(),
            status: status.into(),
            required_for_callers: false,
            hits: 4,
            operations: vec![OperationSummary {
                name: None,
                kind: "query".into(),
                cost: 20,
                depth: 2,
            }],
            persisted_only: false,
        }
    }

    #[test]
    fn a_revoked_document_is_blocked_and_says_why() {
        let revoked = entry("revoked");
        assert!(!revoked.executable());
        let reason = revoked
            .blocked_reason()
            .expect("a revoked row explains itself");
        assert!(reason.contains("PERSISTED_QUERY_NOT_FOUND"), "{reason}");
        assert!(reason.contains("revoked"), "{reason}");

        let active = entry("active");
        assert!(active.executable());
        assert_eq!(
            active.blocked_reason(),
            None,
            "a usable row shows a control, not a note"
        );
    }

    #[test]
    fn a_draft_document_is_not_executable_and_does_not_claim_to_be_revoked() {
        // The two refusals mean different things: one is somebody's decision, the other is a row
        // nobody finished. Reporting a draft as "revoked" would show an administrator a toggle
        // that says "revoke" on a document that was never allowed in the first place.
        let draft = entry("draft");
        assert!(!draft.executable());
        let reason = draft.blocked_reason().expect("a draft explains itself");
        assert!(reason.contains("draft"), "{reason}");
        assert!(!reason.contains("revoked"), "{reason}");
    }

    #[test]
    fn the_heaviest_operation_is_the_one_the_manager_shows() {
        let mut entry = entry("active");
        entry.operations.push(OperationSummary {
            name: Some("Export".into()),
            kind: "mutation".into(),
            cost: 900,
            depth: 4,
        });
        assert_eq!(entry.heaviest().expect("two operations").cost, 900);
        assert_eq!(entry.heaviest().expect("two operations").kind, "mutation");
    }

    #[test]
    fn a_document_holding_a_mutation_is_filed_as_a_mutation() {
        assert_eq!(heaviest_kind("{ pages { id } }"), "query");
        assert_eq!(
            heaviest_kind("mutation Publish { publishPage(id: 1) { id } }"),
            "mutation"
        );
        assert_eq!(
            heaviest_kind(
                "query Read { pages { id } } mutation Write { publishPage(id: 1) { id } }"
            ),
            "mutation",
            "a mixed document is filed by its heaviest kind, so a read-only review cannot miss it"
        );
        assert_eq!(heaviest_kind("this is not a document"), "query");
    }

    #[test]
    fn registering_names_the_field_that_is_wrong() {
        let cases: Vec<(RegisterRequest, &str)> = vec![
            (
                RegisterRequest {
                    name: "  ".into(),
                    document: QUERY.into(),
                    active: true,
                    required_for_callers: false,
                },
                "`name`",
            ),
            (
                RegisterRequest {
                    name: "x".repeat(121),
                    document: QUERY.into(),
                    active: true,
                    required_for_callers: false,
                },
                "`name`",
            ),
            (
                RegisterRequest {
                    name: "ok".into(),
                    document: "   ".into(),
                    active: true,
                    required_for_callers: false,
                },
                "`document`",
            ),
        ];
        for (request, field) in cases {
            let err = request
                .validate()
                .expect_err(&format!("{field} must be refused"));
            assert!(
                err.to_string().contains(field),
                "the message does not name {field}: {err}"
            );
        }

        RegisterRequest {
            name: "Page list".into(),
            document: QUERY.into(),
            active: true,
            required_for_callers: false,
        }
        .validate()
        .expect("a legal registration validates");
    }

    #[test]
    fn a_registration_activates_unless_it_asks_not_to() {
        let request: RegisterRequest =
            serde_json::from_str(r#"{"name":"Page list","document":"{ pages { id } }"}"#)
                .expect("deserialises");
        assert!(
            request.active,
            "a document registered as a draft executes nothing and explains nothing — that is a \
             dead row, so activation is the default"
        );
    }

    #[test]
    fn persisted_only_refuses_an_unregistered_document_with_the_documented_code() {
        let settings = crate::settings::Settings {
            persisted_only: true,
            ..crate::settings::Settings::default()
        };
        let unknown = Lookup::Unknown("abc123".into());
        let err = admit(&settings, &unknown, None).expect_err("persisted-only refuses it");
        assert_eq!(err.code(), Code::PersistedQueryNotFound);
        assert!(err.to_string().contains("persisted-only"), "{err}");
    }

    #[test]
    fn persisted_only_lets_a_registered_active_document_through_and_blocks_a_revoked_one() {
        let settings = crate::settings::Settings {
            persisted_only: true,
            ..crate::settings::Settings::default()
        };
        let active = Lookup::Active(entry("active"));
        assert!(
            admit(&settings, &active, None)
                .expect("registered and active is allowed")
                .is_none()
        );

        let revoked = Lookup::Blocked(entry("revoked"), "revoked by an administrator".into());
        let err = admit(&settings, &revoked, None).expect_err("a revoked document is refused");
        assert_eq!(err.code(), Code::PersistedQueryNotFound);
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    #[test]
    fn without_persisted_only_the_lookup_is_never_consulted() {
        // With the flag off, an unregistered document is just an ad-hoc one. Consulting the
        // registry anyway would refuse documents the installation has deliberately allowed.
        let settings = crate::settings::Settings::default();
        let unknown = Lookup::Unknown("abc".into());
        assert!(
            admit(&settings, &unknown, None)
                .expect("ad-hoc is allowed")
                .is_none()
        );
    }

    #[test]
    fn an_ambiguous_reference_names_its_candidates_rather_than_picking_one() {
        // Two documents whose digests share a prefix cannot be told apart by that reference.
        // Picking the first row in index order would execute a document the caller did not name,
        // which is the worst possible failure for a client-driven API.
        let lookup = Lookup::Ambiguous {
            requested: "ab".into(),
            candidates: vec!["abc…".into(), "abd…".into()],
        };
        assert!(!lookup.is_executable());
        assert!(
            lookup.message().contains("matches 2"),
            "{}",
            lookup.message()
        );
        assert!(lookup.message().contains("abc"), "{}", lookup.message());
        assert_eq!(lookup.code(), Code::PersistedQueryNotFound);
    }

    #[test]
    fn a_document_nobody_calls_is_offered_for_pruning() {
        let mut used = entry("active");
        used.hits = 500;
        let mut idle = entry("active");
        idle.id = "doc-2".into();
        idle.name = "Unused".into();
        idle.hits = 0;
        let entries = vec![used, idle];
        let prunable = prunable(&entries, 5);
        assert_eq!(prunable.len(), 1);
        assert_eq!(prunable[0].name, "Unused");
        assert_eq!(
            prunable[0].cost,
            Some(20),
            "the summary carries the priced cost"
        );
        assert_eq!(prunable[0].depth, Some(2));
    }

    #[test]
    fn describe_reports_an_operation_cost_rather_than_nothing() {
        let operations = describe(
            "{ pages(first: 5) { id title } }",
            &crate::cost::Catalogue::catalogued(),
        );
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].kind, "query");
        assert!(
            operations[0].cost > 0,
            "a zero cost on a real selection means the pricer never ran"
        );
        assert!(operations[0].depth >= 1);
    }

    #[test]
    fn describe_survives_a_document_that_does_not_parse() {
        // A manager that crashes on a pasted query has no way to report why it was refused.
        let operations = describe("{ pages(", &crate::cost::Catalogue::catalogued());
        assert!(operations.is_empty());
    }

    #[test]
    fn a_summary_survives_a_round_trip_because_the_screen_renders_the_row() {
        let summary = DocumentSummary::from(&entry("active"));
        let json = serde_json::to_string(&summary).expect("serialises");
        let back: DocumentSummary = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(summary, back);
    }
}
