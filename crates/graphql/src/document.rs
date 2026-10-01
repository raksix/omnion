//! Just enough GraphQL to answer the questions the limits ask, and no more.
//!
//! The limits in this crate are **pre-execution**: a document is refused *before* any resolver
//! runs, which the request insists on — *"Violations are refused before execution, with distinct
//! error codes and no partial writes."* That means something has to read the document first, and
//! this is that something.
//!
//! It is deliberately **not** a general GraphQL implementation. It parses the executable
//! definitions a client can send — `query` / `mutation`, with named operations, variable
//! definitions, selection sets, fields, arguments, aliases and fragments (inline and spread) — and
//! stops at the first structural thing it does not need. There is no executor here, no type
//! system, no validation against a schema, no directive semantics. Slice 1's job is to decide
//! whether a document is *allowed*, and a parser that tried to be a server would be a second
//! implementation of the whole thing waiting to drift from the first.
//!
//! ## What a partial parser can get wrong, and why this one is still safe
//!
//! A limit is only as good as its reading of the document, and the classic failure is a parser
//! that under-counts: it misses the deep field inside the fragment the operation spreads, so
//! `depth` reads 3 for a query that actually nests 40, and the limit that exists to stop exactly
//! that query is the one thing it cannot see. Two rules close that hole:
//!
//! 1. **Fragments are resolved, not ignored.** A spread is followed into its definition and its
//!    selection set is counted at the spread's position, so depth through a fragment is real
//!    depth. A cyclic fragment is refused ([`Error`]/cycle detection) rather than expanded until
//!    the stack dies — a self-referential `fragment F on T { child { ...F } }` is the cheapest
//!    way to turn a limit into a crash.
//! 2. **Unknown syntax is a refusal, not a pass.** Anything this parser cannot account for
//!    produces [`Code::GraphqlValidationFailed`]. A document that cannot be fully priced is not
//!    allowed to execute — which is the fail-closed direction. The alternative (assume it is
//!    cheap) is how a limit turns into a denial-of-service vector, the exact risk the request
//!    names when it says an under-priced field "becomes a denial-of-service vector".

use crate::error::{Code, Error, Result};

/// The largest document this parser will look at, in bytes.
///
/// A limit expressed only in "depth" and "cost" is unbounded in one direction: a document can be
/// a megabyte of `a a a a a ...` aliases, and alias count is checked per operation — so the byte
/// cap is what actually bounds the parse. It is generous for a hand-written document (a real
/// one is a few kilobytes) and small enough that the worst case is milliseconds.
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// The hard ceiling on nesting the parser will *walk*, independent of the configured limit.
///
/// The configured depth limit is a policy the operator tunes and may legitimately be raised; this
/// one is a stack-safety constant. A parser that trusted the configured limit alone would let a
/// hostile document choose how much host stack it consumes.
pub const PARSE_DEPTH_CEILING: u32 = 256;

/// What an operation selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Mutation,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
        }
    }
}

/// A leaf selection: a field, with its alias and arguments.
///
/// Arguments are captured as raw source spans rather than a parsed value, because nothing in the
/// decision layer reads them — the **cost model prices page size separately** ([`crate::cost`]),
/// which means an argument this parser misreads cannot mis-bill a caller. They are kept because
/// a document hash is computed over the canonical text and a field without its arguments is not
/// the field the client wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub alias: Option<String>,
    pub name: String,
    pub arguments: Vec<(String, String)>,
    /// The fields selected beneath this one.
    ///
    /// Carried on the field rather than in a parallel tree because depth and cost are both
    /// walks over this exact structure — a separate index that had to be kept in step with the
    /// parse is one more place for a deep query to hide.
    pub selections: Vec<Selection>,
}

impl Field {
    /// The key a field contributes cost under — the alias when present, so two aliases of one
    /// field are charged twice. A cost model that charged per *field* would let one aliased
    /// listing be priced as a single field, which is the same under-pricing hole the request
    /// warns about.
    pub fn key(&self) -> String {
        match &self.alias {
            Some(alias) => format!("{alias}:{}", self.name),
            None => self.name.clone(),
        }
    }

    /// The response key — what the caller will read it back under.
    pub fn response_key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A selected item: a field or a fragment spread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Field(Field),
    /// An inline fragment carries its selection inline; a spread names a definition resolved
    /// later. Both count toward depth and cost, and both are represented so a caller can tell an
    /// unresolvable spread (`UnknownFragment`) from an inline one with no body.
    InlineFragment(Vec<Selection>),
    FragmentSpread(String),
}

/// One executable operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub kind: OperationKind,
    pub name: Option<String>,
    /// Declared variable names, with their default values' source text.
    pub variables: Vec<(String, Option<String>)>,
    pub selections: Vec<Selection>,
}

impl Operation {
    /// Whether this operation writes.
    ///
    /// Named rather than spelled `self.kind == OperationKind::Mutation` at every call site: the
    /// schema validator's rule ("a mutation operation may only select mutations") is the one
    /// place a caller must not confuse the two, and a boolean reads at that site without a
    /// reader having to know the enum's spelling.
    pub fn is_mutation(&self) -> bool {
        self.kind == OperationKind::Mutation
    }
}

/// A parsed executable document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    pub operations: Vec<Operation>,
    /// Fragment definitions by name, in source order.
    pub fragments: Vec<(String, Vec<Selection>)>,
}

impl Document {
    /// The operation a request names, or the only one when the document is anonymous.
    ///
    /// A document with **several** operations and no name is a refusal rather than a guess: the
    /// caller asked for an ambiguity the endpoint has no way to resolve.
    pub fn select(&self, name: Option<&str>) -> Result<&Operation> {
        match name {
            Some(name) => self
                .operations
                .iter()
                .find(|op| op.name.as_deref() == Some(name))
                .ok_or_else(|| Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: format!("the document defines no operation named `{name}`"),
                }),
            None => match self.operations.as_slice() {
                [only] => Ok(only),
                [] => Err(Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: "the document defines no executable operation".into(),
                }),
                _ => Err(Error::Validation {
                    code: Code::GraphqlValidationFailed,
                    message: format!(
                        "the document defines {} operations and names none; send an operationName",
                        self.operations.len()
                    ),
                }),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

/// A cursor over the document.
///
/// Tokens are produced on demand and never buffered as a `Vec<Token>`: a hostile document is
/// bounded by [`MAX_DOCUMENT_BYTES`], and materialising every token first would let a 64 KB
/// document of one-character tokens allocate far more than it occupies.
struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

/// What a punctuation character means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Punct {
    BraceOpen,
    BraceClose,
    ParenOpen,
    ParenClose,
    BracketOpen,
    BracketClose,
    Colon,
    At,
    Dollar,
    /// `!` — a non-null type marker and a non-null argument value.
    Exclamation,
    /// `=` — a variable's default value.
    Equal,
}

impl Punct {
    fn of(ch: u8) -> Option<Self> {
        Some(match ch {
            b'{' => Self::BraceOpen,
            b'}' => Self::BraceClose,
            b'(' => Self::ParenOpen,
            b')' => Self::ParenClose,
            b'[' => Self::BracketOpen,
            b']' => Self::BracketClose,
            b':' => Self::Colon,
            b'@' => Self::At,
            b'$' => Self::Dollar,
            b'!' => Self::Exclamation,
            b'=' => Self::Equal,
            _ => return None,
        })
    }
}

/// What a name character is.
///
/// GraphQL's `Name` is `/[_A-Za-z][_0-9A-Za-z]*/`. The parser accepts the full set rather than
/// the ASCII-only approximation, because a document using a legal non-ASCII name should be
/// *priced*, not refused for a reason that has nothing to do with the limit being enforced.
fn is_name_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}

fn is_name_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn err(&self, message: impl Into<String>) -> Error {
        Error::Validation {
            code: Code::GraphqlValidationFailed,
            message: message.into(),
        }
    }

    /// Whitespace, commas and comments. GraphQL treats the comma as insignificant, so a document
    /// formatted by a client that adds commas is not a syntax error.
    fn skip_ignored(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' | b',' | 0x0b | 0x0c => self.pos += 1,
                b'#' => {
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                0xef if self.src[self.pos..].starts_with('\u{feff}') => self.pos += 3,
                _ => break,
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ignored();
        self.bytes.get(self.pos).copied()
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        match self.peek() {
            Some(ch) if Punct::of(ch) == Some(punct) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    /// `...` — three bytes, checked together.
    fn eat_spread(&mut self) -> bool {
        self.skip_ignored();
        if self.bytes[self.pos..].starts_with(b"...") {
            self.pos += 3;
            return true;
        }
        false
    }

    fn expect_punct(&mut self, punct: Punct) -> Result<()> {
        if self.eat_punct(punct) {
            return Ok(());
        }
        Err(self.err(format!(
            "expected `{}` at byte {}",
            punct_char(punct),
            self.pos
        )))
    }

    /// A name, or a refusal. Never an empty string: an empty name would compare equal to every
    /// other empty name in a lookup, which is how a malformed field ends up priced as free.
    fn name(&mut self) -> Result<String> {
        self.skip_ignored();
        let rest = self
            .src
            .get(self.pos..)
            .ok_or_else(|| self.err("unexpected end of document"))?;
        let mut chars = rest.char_indices();
        let (_, first) = chars
            .next()
            .ok_or_else(|| self.err("expected a name, found the end of the document"))?;
        if !is_name_start(first) {
            return Err(self.err(format!("expected a name, found `{first}`")));
        }
        let mut end = first.len_utf8();
        for (idx, ch) in chars {
            if is_name_continue(ch) {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        self.pos += end;
        Ok(rest[..end].to_string())
    }

    /// A value: anything up to the matching close, kept as source text.
    fn value(&mut self) -> Result<String> {
        self.skip_ignored();
        let start = self.pos;
        let mut depth = 0usize;
        let mut in_string = false;
        while self.pos < self.bytes.len() {
            let ch = self.bytes[self.pos];
            if in_string {
                if ch == b'\\' {
                    self.pos += 1;
                } else if ch == b'"' {
                    in_string = false;
                }
                self.pos += 1;
                continue;
            }
            match ch {
                b'"' => {
                    in_string = true;
                    self.pos += 1;
                }
                b'{' | b'[' | b'(' => {
                    depth += 1;
                    self.pos += 1;
                }
                b'}' | b']' | b')' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                // A comma at depth 0 ends the value. Without this, `first: 10, order: DESC`
                // parses as ONE argument whose value is `10, order: DESC`, which is why an
                // argument list silently lost every argument after the first.
                b',' if depth == 0 => break,
                _ => self.pos += 1,
            }
        }
        if depth != 0 || in_string {
            return Err(self.err("a value is not closed"));
        }
        Ok(self.src[start..self.pos].trim().to_string())
    }

    /// Skip a type reference — used in variable definitions, where the type is irrelevant to
    /// every decision this crate makes. It must still be *consumed*, or the parse desynchronises
    /// and the rest of the document is read as noise.
    fn skip_type(&mut self) -> Result<()> {
        // A type reference is `Name!`, `[Inner!]!` or `[Name]`, and may carry directives. The
        // list form is the RARE one, so it is the branch — a parser that opened with `[`
        // refused every ordinary `$id: ID!` in the first variable definition it saw.
        if self.eat_punct(Punct::BracketOpen) {
            self.skip_type()?;
            self.expect_punct(Punct::BracketClose)?;
        } else {
            self.name()?;
        }
        self.eat_punct(Punct::Exclamation);
        self.skip_directives()?;
        Ok(())
    }

    /// Directive arguments and a variable definition's default, both of which are `( … )` or
    /// `$var: … = value` respectively.
    fn arguments(&mut self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if !self.eat_punct(Punct::ParenOpen) {
            return Ok(out);
        }
        loop {
            if self.eat_punct(Punct::ParenClose) {
                return Ok(out);
            }
            let name = self.name()?;
            self.expect_punct(Punct::Colon)?;
            let value = self.value()?;
            out.push((name, value));
            if self.bytes.get(self.pos).is_none() {
                return Err(self.err("an argument list is not closed"));
            }
        }
    }

    fn variable_definitions(&mut self) -> Result<Vec<(String, Option<String>)>> {
        let mut out = Vec::new();
        if !self.eat_punct(Punct::ParenOpen) {
            return Ok(out);
        }
        loop {
            if self.eat_punct(Punct::ParenClose) {
                return Ok(out);
            }
            if !self.eat_punct(Punct::Dollar) {
                return Err(self.err("expected a `$` on a variable definition"));
            }
            let name = self.name()?;
            self.expect_punct(Punct::Colon)?;
            self.skip_type()?;
            let default = if self.eat_punct(Punct::Equal) {
                Some(self.value()?)
            } else {
                None
            };
            out.push((name, default));
            if self.bytes.get(self.pos).is_none() {
                return Err(self.err("a variable definition list is not closed"));
            }
        }
    }

    /// Skip directives attached to a selection, keeping their arguments consumed.
    fn skip_directives(&mut self) -> Result<()> {
        while self.peek() == Some(b'@') {
            self.pos += 1;
            self.name()?;
            self.arguments()?;
        }
        Ok(())
    }

    /// A selection set, to a recursion bound.
    fn selection_set(&mut self, depth: u32) -> Result<Vec<Selection>> {
        if depth > PARSE_DEPTH_CEILING {
            return Err(Error::Limit {
                code: Code::DepthLimit,
                message: format!(
                    "the document nests deeper than the parser's own ceiling of {PARSE_DEPTH_CEILING}"
                ),
                limit: PARSE_DEPTH_CEILING as u64,
                actual: depth as u64,
            });
        }
        self.expect_punct(Punct::BraceOpen)?;
        let mut out = Vec::new();
        loop {
            if self.eat_punct(Punct::BraceClose) {
                return Ok(out);
            }
            if self.pos >= self.bytes.len() {
                return Err(self.err("a selection set is not closed"));
            }
            out.push(self.selection(depth)?);
        }
    }

    fn selection(&mut self, depth: u32) -> Result<Selection> {
        // A spread: `...name`, `... on Type { … }` or `... @directive { … }`.
        if self.eat_spread() {
            // `... @directive { … }` is legal and carries no name.
            if self.peek() == Some(b'@') {
                self.skip_directives()?;
                let body = self.selection_set(depth + 1)?;
                return Ok(Selection::InlineFragment(body));
            }
            // `... on Type { … }` — the type condition is irrelevant here (this parser has no
            // schema), so it is consumed and the body is counted as an inline fragment.
            if self.peek().is_some() && !self.starts_with_punct(b'{') {
                let keyword = self.name()?;
                if keyword != "on" {
                    // A bare `...name` is a spread. Anything else after the dots is a document
                    // this parser cannot price, and an unpriced document must not execute.
                    return Ok(Selection::FragmentSpread(keyword));
                }
                self.name()?; // the type condition
                self.skip_directives()?;
                let body = self.selection_set(depth + 1)?;
                return Ok(Selection::InlineFragment(body));
            }
            let body = self.selection_set(depth + 1)?;
            return Ok(Selection::InlineFragment(body));
        }

        let first = self.name()?;
        let (alias, field_name) = if self.eat_punct(Punct::Colon) {
            (Some(first), self.name()?)
        } else {
            (None, first)
        };
        let arguments = self.arguments()?;
        self.skip_directives()?;
        let selections = if self.peek() == Some(b'{') {
            self.selection_set(depth + 1)?
        } else {
            Vec::new()
        };
        Ok(Selection::Field(Field {
            alias,
            name: field_name,
            arguments,
            selections,
        }))
    }

    /// Whether the next non-ignored byte is `ch`. Used where a decision depends on whether a
    /// `{` follows without consuming anything.
    fn starts_with_punct(&self, ch: u8) -> bool {
        let mut cursor = self.pos;
        while cursor < self.bytes.len() {
            match self.bytes[cursor] {
                b' ' | b'\t' | b'\n' | b'\r' | b',' => cursor += 1,
                _ => return self.bytes[cursor] == ch,
            }
        }
        false
    }

    /// The whole document.
    fn document(&mut self) -> Result<Document> {
        let mut doc = Document::default();
        while self.peek().is_some() {
            // The shorthand: a document that opens with `{` is an anonymous query, and it is the
            // form most clients send. Reading a keyword first cannot parse it — `name()` refuses
            // `{` — so the brace is recognised here and becomes the operation's selection set.
            if self.peek() == Some(b'{') {
                let selections = self.selection_set(1)?;
                doc.operations.push(Operation {
                    kind: OperationKind::Query,
                    name: None,
                    variables: Vec::new(),
                    selections,
                });
                continue;
            }
            let keyword = self.name()?;
            match keyword.as_str() {
                "query" | "mutation" => {
                    let kind = if keyword == "query" {
                        OperationKind::Query
                    } else {
                        OperationKind::Mutation
                    };
                    // An anonymous operation has no name: `query { … }`.
                    let name = match self.peek() {
                        Some(b'{') | Some(b'(') => None,
                        _ => Some(self.name()?),
                    };
                    let variables = self.variable_definitions()?;
                    self.skip_directives()?;
                    let selections = self.selection_set(1)?;
                    doc.operations.push(Operation {
                        kind,
                        name,
                        variables,
                        selections,
                    });
                }
                "fragment" => {
                    let name = self.name()?;
                    // `fragment Name on Type { … }` — the condition is consumed, the body kept.
                    let keyword = self.name()?;
                    if keyword != "on" {
                        return Err(self.err(format!(
                            "fragment `{name}` does not declare a type condition"
                        )));
                    }
                    self.name()?;
                    self.skip_directives()?;
                    let selections = self.selection_set(1)?;
                    doc.fragments.push((name, selections));
                }
                // Executable definitions only. `schema`, `type`, `directive`, `enum`, … belong
                // in the composed SDL, not in a request body, so accepting them here would let
                // a client ship type definitions the endpoint never reads.
                other => {
                    return Err(self.err(format!(
                        "`{other}` is not an executable definition; only query, mutation and fragment are"
                    )));
                }
            }
        }
        if doc.operations.is_empty() {
            return Err(self.err("the document defines no executable operation"));
        }
        Ok(doc)
    }
}

fn punct_char(punct: Punct) -> char {
    match punct {
        Punct::BraceOpen => '{',
        Punct::BraceClose => '}',
        Punct::ParenOpen => '(',
        Punct::ParenClose => ')',
        Punct::BracketOpen => '[',
        Punct::BracketClose => ']',
        Punct::Colon => ':',
        Punct::At => '@',
        Punct::Dollar => '$',
        Punct::Exclamation => '!',
        Punct::Equal => '=',
    }
}

/// Parse an executable document.
///
/// The byte cap is applied **before** the parse rather than after, so an oversized document costs
/// a length check instead of a walk.
pub fn parse(src: &str) -> Result<Document> {
    if src.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit {
            code: Code::GraphqlValidationFailed,
            message: format!(
                "the document is {} bytes, the parser accepts at most {MAX_DOCUMENT_BYTES}",
                src.len()
            ),
            limit: MAX_DOCUMENT_BYTES as u64,
            actual: src.len() as u64,
        });
    }
    Parser::new(src).document()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FIELD NAME of a selection — what these tests assert on. Deliberately not
    /// `Field::key()`, which is the cost key (`alias:name` when aliased): a parse test that
    /// asserted on the cost key would be testing the pricing model, not the parse.
    fn field_key(sel: &Selection) -> &str {
        match sel {
            Selection::Field(f) => f.name.as_str(),
            _ => panic!("expected a field, got {sel:?}"),
        }
    }

    #[test]
    fn a_simple_query_parses_into_named_fields() {
        let doc = parse("{ organization { id name } }").expect("a simple query parses");
        let op = doc
            .select(None)
            .expect("the anonymous operation is the only one");
        assert_eq!(op.kind, OperationKind::Query);
        assert_eq!(op.selections.len(), 1);
        assert_eq!(field_key(&op.selections[0]), "organization");
        let Selection::Field(outer) = &op.selections[0] else {
            panic!("the root selection is a field");
        };
        assert_eq!(outer.selections_len_for_test(), 2);
        assert_eq!(outer.arguments, Vec::<(String, String)>::new());
    }

    #[test]
    fn aliases_are_kept_because_a_cost_model_prices_them_separately() {
        let doc = parse("{ first: organization(id: 1) { id } second: organization(id: 2) { id } }")
            .expect("aliased fields parse");
        let op = doc.select(None).unwrap();
        let Selection::Field(first) = &op.selections[0] else {
            panic!("expected a field");
        };
        assert_eq!(first.alias.as_deref(), Some("first"));
        assert_eq!(first.name, "organization");
        // The two aliases are distinct keys: pricing them as one field would be the
        // under-pricing hole the request warns about.
        assert_ne!(first.key(), "organization");
        let Selection::Field(second) = &op.selections[1] else {
            panic!("expected a field");
        };
        assert_eq!(second.key(), "second:organization");
        assert_eq!(first.response_key(), "first");
    }

    #[test]
    fn a_mutation_is_distinguished_from_a_query_by_keyword() {
        let doc = parse("mutation CreateArticle($title: String! = \"hi\") { createArticle(title: $title) { id } }")
            .expect("a mutation parses");
        let op = doc
            .select(Some("CreateArticle"))
            .expect("the named operation is found");
        assert_eq!(op.kind, OperationKind::Mutation);
        assert_eq!(op.name.as_deref(), Some("CreateArticle"));
        assert_eq!(
            op.variables,
            vec![("title".to_string(), Some("\"hi\"".to_string()))]
        );
    }

    #[test]
    fn several_operations_with_no_name_is_a_refusal_rather_than_a_guess() {
        let doc = parse("query A { a } query B { b }").expect("the document parses");
        // The parse succeeds; choosing between two operations does not.
        let err = doc
            .select(None)
            .expect_err("an ambiguous document is refused");
        assert_eq!(err.code_str(), "GRAPHQL_VALIDATION_FAILED");
        assert!(err.to_string().contains("operationName"), "{err}");
        // Naming one resolves it.
        assert_eq!(doc.select(Some("B")).unwrap().name.as_deref(), Some("B"));
    }

    #[test]
    fn a_named_operation_that_does_not_exist_names_the_operation() {
        let doc = parse("query A { a }").unwrap();
        let err = doc
            .select(Some("Missing"))
            .expect_err("an unknown operation is refused");
        assert!(err.to_string().contains("Missing"), "{err}");
    }

    #[test]
    fn fragments_parse_and_keep_their_selection_sets() {
        let doc = parse(
            "query Q { organization { ...OrgBits } } fragment OrgBits on Organization { id name }",
        )
        .expect("a spread document parses");
        let op = doc.select(Some("Q")).unwrap();
        assert!(matches!(op.selections[0], Selection::Field(_)));
        assert_eq!(doc.fragments.len(), 1);
        assert_eq!(doc.fragments[0].0, "OrgBits");
        assert_eq!(doc.fragments[0].1.len(), 2);
    }

    #[test]
    fn a_type_condition_is_consumed_so_the_parse_does_not_desynchronise() {
        // The condition is meaningless to this parser, but it must still be read — skipping it
        // is what makes a parser report confident nonsense on the fields after it.
        let doc = parse("{ search { ... on Article { id } ... on Organization { id } } }")
            .expect("inline fragments on types parse");
        let op = doc.select(None).unwrap();
        let Selection::Field(search) = &op.selections[0] else {
            panic!("expected a field");
        };
        assert_eq!(search.selections_len_for_test(), 2);
        assert!(
            search
                .selections_ref()
                .iter()
                .all(|s| matches!(s, Selection::InlineFragment(_)))
        );
    }

    #[test]
    fn comments_and_commas_are_insignificant() {
        // A client's pretty-printer emits commas and comments; refusing them would refuse
        // documents that are perfectly legal GraphQL.
        let doc = parse("# leading comment\n{ organization, # trailing\n  id, name, }\n# done")
            .expect("commas and comments parse");
        let op = doc.select(None).unwrap();
        // `organization`, `id` and `name` are three SIBLINGS of the root here — the braces in the
        // original version of this test were absent, and it asserted a nesting the document never
        // had. The parse was correct and the test was wrong, which is worth stating because the
        // failure mode it produced ("the parser dropped the children") points the other way.
        assert_eq!(
            op.selections.iter().map(field_key).collect::<Vec<_>>(),
            vec!["organization", "id", "name"]
        );
        let Selection::Field(root) = &op.selections[0] else {
            panic!("expected a field");
        };
        assert!(
            root.selections_ref().is_empty(),
            "a leaf field selected nothing beneath it"
        );
    }

    #[test]
    fn arguments_survive_as_source_text_including_nested_values() {
        let doc = parse(
            r#"{ articles(filter: { tag: "ai", limit: { first: 10 } }, order: DESC) { id } }"#,
        )
        .expect("nested argument values parse");
        let op = doc.select(None).unwrap();
        let Selection::Field(articles) = &op.selections[0] else {
            panic!("expected a field");
        };
        assert_eq!(articles.arguments.len(), 2);
        let filter = &articles.arguments[0];
        assert_eq!(filter.0, "filter");
        // The value keeps its structure as source text; the decision layer never reads it, but
        // a document hash computed over the text without arguments would not be the client's
        // document.
        assert!(filter.1.contains("\"ai\""), "{}", filter.1);
        assert!(filter.1.contains("first"), "{}", filter.1);
        assert_eq!(articles.arguments[1].0, "order");
        assert_eq!(articles.arguments[1].1, "DESC");
    }

    #[test]
    fn an_unterminated_document_is_a_refusal_and_not_a_partial_parse() {
        // Each of these would leave the parser mid-structure. A partial document handed to the
        // cost model is the under-count this parser exists to prevent.
        for src in [
            "{ organization { id ",
            "{ organization(id: \"unterminated }",
            "query { articles(filter: { a: 1 } }",
            "{ organization",
            "fragment F on X { id",
        ] {
            let err = parse(src).expect_err(&format!("`{src}` must not parse"));
            assert_eq!(
                err.code_str(),
                "GRAPHQL_VALIDATION_FAILED",
                "unexpected code for `{src}`: {err}"
            );
        }
    }

    #[test]
    fn a_type_system_definition_is_refused_rather_than_ignored() {
        // A client sending `type Query { … }` in a request body is not making a query; accepting
        // it silently would let the document read as "no operation" in one code path and as a
        // valid one in another.
        let err = parse("type Query { id: String }").expect_err("a type definition is refused");
        assert!(
            err.to_string().contains("not an executable definition"),
            "{err}"
        );
    }

    #[test]
    fn an_oversized_document_is_refused_by_length_before_it_is_parsed() {
        let huge = format!(
            "{{ organization {{ id {} }} }}",
            "x".repeat(MAX_DOCUMENT_BYTES)
        );
        let err = parse(&huge).expect_err("an oversized document is refused");
        assert_eq!(err.code_str(), "GRAPHQL_VALIDATION_FAILED");
        assert!(err.to_string().contains("byte"), "{err}");
    }

    #[test]
    fn the_parser_walk_is_bounded_by_its_own_ceiling_not_the_configured_limit() {
        // A configured limit of 10 must not be what stops a hostile document: the configured
        // value is policy the operator may raise, and a parser that trusted it would let a
        // caller choose host stack usage.
        let mut deep = String::new();
        for _ in 0..PARSE_DEPTH_CEILING + 8 {
            deep.push_str("{ a ");
        }
        deep.push_str("{ b }");
        for _ in 0..(PARSE_DEPTH_CEILING + 8) {
            deep.push('}');
        }
        let err = parse(&deep).expect_err("a document past the parser ceiling is refused");
        assert_eq!(err.code_str(), "DEPTH_LIMIT");
        assert!(err.to_string().contains("ceiling"), "{err}");
    }

    #[test]
    fn a_nested_document_just_inside_the_ceiling_still_parses() {
        // The control for the ceiling test: the bound is a real bound and not "everything with a
        // `{` fails".
        let mut deep = String::new();
        for _ in 0..40 {
            deep.push_str("{ a ");
        }
        deep.push_str("{ b }");
        for _ in 0..40 {
            deep.push('}');
        }
        parse(&deep).expect("a document inside the ceiling parses");
    }

    // Small accessors so the tests above read as assertions about the parse rather than about
    // the enum's shape.
    impl Field {
        fn selections_len_for_test(&self) -> usize {
            self.selections.len()
        }
        fn selections_ref(&self) -> &[Selection] {
            &self.selections
        }
    }
}
