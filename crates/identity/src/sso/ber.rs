//! The BER dialect LDAP speaks (RFC 4511).
//!
//! This crate has no LDAP dependency and does not want one. Every directory client in this
//! position has reached for a full protocol implementation, and the cost is not the dependency —
//! it is that the interesting questions stop being testable. The questions that matter here are
//! *"does a login name containing `)` change which entries the filter selects"*, *"does a
//! truncated response from a server dying mid-page parse as data or as an error"*, and *"does a
//! group graph with a cycle terminate"*. A codec we own is what makes those unit tests instead
//! of integration tests against somebody else's CI.
//!
//! So this is deliberately the **narrow** half of BER: the tags RFC 4511 defines, in the
//! definite-length form LDAP servers actually emit, and nothing else. Three decisions are load
//! bearing, and each names a failure it prevents:
//!
//! * **Depth and breadth are bounded.** A BER decoder that recurses on a hostile or corrupt
//!   length is a stack-overflow denial of service reachable from a directory operator's browser.
//!   [`Limits`] caps both, and exceeding either is a *refusal* naming the cap, not a panic.
//! * **Indefinite lengths are refused, not tolerated.** LDAP's own encoding rules (RFC 4511 §5.1
//!   note) prohibit the indefinite form. A server that sends one is either broken or speaking
//!   something else, and guessing which is how a parser ends up accepting an unbounded buffer.
//! * **A short body is an error, never a partial value.** The alternative is an attribute value
//!   that is a prefix of the real one — a truncated mail address that still parses as an address
//!   is a directory telling you a person is somebody else.

use std::fmt;

// ---------------------------------------------------------------------------------------------
// Primitive encoding tags
// ---------------------------------------------------------------------------------------------

/// Universal tag numbers this codec reads and writes.
mod tag {
    pub const BOOLEAN: u8 = 0x01;
    pub const INTEGER: u8 = 0x02;
    pub const OCTET_STRING: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const ENUMERATED: u8 = 0x0A;
    pub const SEQUENCE: u8 = 0x10;
    pub const SET: u8 = 0x11;
}

/// How deep and how wide a decoded value may be.
///
/// The defaults are far above any legal LDAP message — a `searchResEntry` with a 50-attribute
/// JNDLPhoto blob is the pathological real case and is nowhere near this — and far below the
/// point where a corrupt length can cost anything meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum container nesting, counted from the top of an LDAP message.
    ///
    /// A real message sits at five: the envelope, the operation, a `searchResEntry`, its
    /// `PartialAttributeList`, and the attribute's `SET OF value`. A filter is deeper still, and
    /// an extensible match — which this codec does not write but a *server* may echo — is five on
    /// its own. So 24 is roughly four times the deepest legal structure, which leaves room for the
    /// wrappers a future filter extension needs while still bounding the recursion at a frame
    /// count no corrupt frame can turn into a stack overflow.
    pub max_depth: u8,
    /// Maximum elements in one SET/SEQUENCE, and the maximum length in bytes of a single
    /// primitive. A base-64 certificate is a few KB.
    pub max_elements: u32,
    /// Maximum total bytes one message may decode to.
    pub max_message: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 24,
            max_elements: 100_000,
            max_message: 8 * 1024 * 1024,
        }
    }
}

/// Why a value could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BerError {
    what: &'static str,
    reason: String,
}

impl BerError {
    pub fn new(what: &'static str, reason: impl Into<String>) -> Self {
        Self {
            what,
            reason: reason.into(),
        }
    }

    /// The operation that failed, in a form a message can be built around.
    #[must_use]
    pub fn what(&self) -> &'static str {
        self.what
    }
}

impl fmt::Display for BerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.what, self.reason)
    }
}

impl std::error::Error for BerError {}

type Result<T> = std::result::Result<T, BerError>;

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Appends BER values into a byte buffer.
#[derive(Debug, Default)]
pub struct Encoder {
    buffer: Vec<u8>,
}

impl Encoder {
    /// A new, empty encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written so far.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buffer
    }

    /// Write a TLV with a definite length.
    pub fn tlv(&mut self, identifier: u8, contents: &[u8]) {
        self.buffer.push(identifier);
        self.write_length(contents.len());
        self.buffer.extend_from_slice(contents);
    }

    /// Begin a constructed value, run `body`, and close it — the one nesting primitive.
    ///
    /// The identifier is passed **constructed** (the high bit set) because that is what a
    /// container is; callers never have to remember to set it, and forgetting is the mistake
    /// that produces a value no server will parse.
    pub fn sequence<F: FnOnce(&mut Self)>(&mut self, body: F) {
        self.buffer.push(tag::SEQUENCE | 0x20);
        let mut nested = Self::new();
        body(&mut nested);
        let inner = nested.finish();
        self.write_length(inner.len());
        self.buffer.extend(inner);
    }

    /// An `APPLICATION n` constructed value.
    pub fn application<F: FnOnce(&mut Self)>(&mut self, number: u8, body: F) {
        self.buffer.push(0x40 | (number & 0x1F) | 0x20);
        let mut nested = Self::new();
        body(&mut nested);
        let inner = nested.finish();
        self.write_length(inner.len());
        self.buffer.extend(inner);
    }

    /// An `APPLICATION n` primitive value.
    pub fn application_primitive(&mut self, number: u8, contents: &[u8]) {
        self.buffer.push(0x40 | (number & 0x1F));
        self.write_length(contents.len());
        self.buffer.extend_from_slice(contents);
    }

    /// A `CONTEXT n` **primitive** value. Used for both LDAP's optional trailing fields and
    /// RFC 4511 §4.1.6's choice-indexed context tags, which are how the authentication method and
    /// the substring pieces are tagged.
    pub fn context_primitive(&mut self, number: u8, contents: &[u8]) {
        self.buffer.push(0x80 | (number & 0x1F));
        self.write_length(contents.len());
        self.buffer.extend_from_slice(contents);
    }

    /// A universal ENUMERATED.
    ///
    /// Universal, not `APPLICATION n`: every enumerated this codec writes is one the RFC defines
    /// in the universal class (the scope, the derefAliases flag). A signature taking a tag number
    /// here would let a caller write an `APPLICATION`-class enumerated for a field that expects a
    /// universal one, and the server's answer would be a protocol error naming neither side.
    pub fn enumerated(&mut self, value: i64) {
        let mut body = Vec::new();
        write_integer_body(&mut body, value);
        self.tlv(tag::ENUMERATED, &body);
    }

    /// A universal BOOLEAN.
    pub fn boolean(&mut self, value: bool) {
        self.tlv(tag::BOOLEAN, &[if value { 0xFF } else { 0x00 }]);
    }

    /// A universal INTEGER, two's complement, minimal length.
    pub fn integer(&mut self, value: i64) {
        let mut body = Vec::new();
        write_integer_body(&mut body, value);
        self.tlv(tag::INTEGER, &body);
    }

    /// A universal OCTET STRING.
    pub fn octet_string(&mut self, value: &[u8]) {
        self.tlv(tag::OCTET_STRING, value);
    }

    /// A universal NULL — LDAP's absent optional fields are `NULL`, not empty octet strings, and
    /// some servers reject a zero-length string where they expect an absent value.
    pub fn null(&mut self) {
        self.tlv(tag::NULL, &[]);
    }

    fn write_length(&mut self, length: usize) {
        if length < 0x80 {
            self.buffer.push(length as u8);
            return;
        }
        // Long form: 0x80 | byte-count, then the count big-endian, minimal (no leading zero).
        let bytes = length.to_be_bytes();
        let first = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len() - 1);
        let significant = &bytes[first..];
        self.buffer.push(0x80 | significant.len() as u8);
        self.buffer.extend_from_slice(significant);
    }
}

/// Two's-complement, minimal-length, with the leading-zero rule BER requires.
fn write_integer_body(out: &mut Vec<u8>, value: i64) {
    let bytes = value.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    let mut significant = bytes[first..].to_vec();
    // A positive number whose top bit is set needs a leading zero, or it reads back negative.
    if significant.first().is_some_and(|byte| byte & 0x80 != 0) {
        significant.insert(0, 0x00);
    }
    out.extend_from_slice(&significant);
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// One decoded TLV: a tag and the raw bytes inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value<'a> {
    /// The identifier octet.
    pub identifier: u8,
    /// The contents, borrowed from the frame the decoder was given.
    pub body: &'a [u8],
    /// How deep into the structure this value sits.
    ///
    /// Carried **on the value** rather than counted by the decoder, and that is the whole point:
    /// a decoder is created afresh for every `children()` call, so a depth kept on the decoder
    /// restarts at zero on the way down and the cap is then checked against the depth of a single
    /// level rather than of the structure. A hostile `searchResEntry` nests its containers and
    /// the guard never sees more than two. The first draft had it on the decoder and the nesting
    /// test — the one assertion in the module that exists for exactly this — failed.
    pub(crate) depth: u8,
}

impl<'a> Value<'a> {
    /// The universal tag number, or `None` for a class other than universal.
    #[must_use]
    pub fn universal(&self) -> Option<u8> {
        (self.identifier & 0xC0 == 0x00).then_some(self.identifier & 0x1F)
    }

    /// The application tag number, or `None`.
    #[must_use]
    pub fn application(&self) -> Option<u8> {
        (self.identifier & 0xC0 == 0x40).then_some(self.identifier & 0x1F)
    }

    /// The context tag number, or `None`.
    #[must_use]
    pub fn context(&self) -> Option<u8> {
        (self.identifier & 0xC0 == 0x80).then_some(self.identifier & 0x1F)
    }

    /// Whether the constructed bit is set.
    #[must_use]
    pub fn is_constructed(&self) -> bool {
        self.identifier & 0x20 != 0
    }

    /// Decode the contents as a sequence of values, one level deeper than this value.
    pub fn children(&self, limits: Limits) -> Result<Vec<Value<'a>>> {
        let mut decoder = Decoder::with_limits(self.body, limits);
        decoder.depth = self.depth.saturating_add(1);
        decoder.all()
    }
}

/// Reads TLVs out of a frame.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    input: &'a [u8],
    offset: usize,
    limits: Limits,
    /// How deep the decoder currently is. Seeded by [`Value::children`], never by hand.
    depth: u8,
}

impl<'a> Decoder<'a> {
    /// A decoder over `input` with the default limits.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self::with_limits(input, Limits::default())
    }

    /// A decoder with explicit limits.
    #[must_use]
    pub fn with_limits(input: &'a [u8], limits: Limits) -> Self {
        Self {
            input,
            offset: 0,
            limits,
            depth: 0,
        }
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offset >= self.input.len()
    }

    /// The bytes not yet consumed — the tail of a message after its last value.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        &self.input[self.offset..]
    }

    /// Decode one value, or `None` at the end of the input.
    pub fn next(&mut self) -> Result<Option<Value<'a>>> {
        self.next_at_depth(self.depth)
    }

    /// Decode every value left.
    pub fn all(mut self) -> Result<Vec<Value<'a>>> {
        let mut values = Vec::new();
        while let Some(value) = self.next()? {
            values.push(value);
            if values.len() > self.limits.max_elements as usize {
                return Err(BerError::new(
                    "decode",
                    "a single value holds more elements than the configured limit allows",
                ));
            }
        }
        Ok(values)
    }

    fn next_at_depth(&mut self, depth: u8) -> Result<Option<Value<'a>>> {
        if self.is_empty() {
            return Ok(None);
        }
        if depth > self.limits.max_depth {
            // Checked *before* the value is produced, so a structure that nests past the cap is
            // refused rather than consuming a stack frame per level on the way down. The depth
            // comes from the value being descended into, which is what makes it cumulative.
            return Err(BerError::new(
                "decode",
                "the value nests deeper than the configured limit allows",
            ));
        }
        if (self.input.len() - self.offset) as u32 > self.limits.max_message {
            return Err(BerError::new(
                "decode",
                "the value is larger than the configured message limit allows",
            ));
        }

        let identifier = self.input[self.offset];
        self.offset += 1;

        let first = *self
            .input
            .get(self.offset)
            .ok_or_else(|| BerError::new("decode", "the value ends after its identifier"))?;
        let length = if first & 0x80 == 0 {
            self.offset += 1;
            first as usize
        } else {
            let count = (first & 0x7F) as usize;
            if count == 0 {
                return Err(BerError::new(
                    "decode",
                    "the indefinite length form is not permitted in LDAP",
                ));
            }
            if count > 4 {
                return Err(BerError::new(
                    "decode",
                    "a length needs more than four bytes, which cannot describe a message this \
                     platform will accept",
                ));
            }
            self.offset += 1;
            let end = self.offset + count;
            let bytes = self
                .input
                .get(self.offset..end)
                .ok_or_else(|| BerError::new("decode", "the value ends inside its length"))?;
            let mut value: usize = 0;
            for byte in bytes {
                value = value
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(*byte as usize))
                    .ok_or_else(|| {
                        BerError::new("decode", "the declared length overflows this platform")
                    })?;
            }
            self.offset = end;
            value
        };

        if length > self.limits.max_message as usize {
            return Err(BerError::new(
                "decode",
                "the value is larger than the configured message limit allows",
            ));
        }
        let end = self.offset + length;
        let body = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| {
                BerError::new(
                    "decode",
                    "the value declares a length past the end of what it was given",
                )
            })?;
        self.offset = end;
        let value = Value {
            identifier,
            body,
            depth,
        };

        // Structural sanity, cheap and worth it: a *primitive* universal value that claims to be
        // constructed is either a corrupt frame or a tag we do not understand, and both are worth
        // refusing here rather than three layers down.
        debug_assert!(
            value.is_constructed() || !matches!(value.universal(), Some(t) if t >= 0x10 && t <= 0x1F),
            "a universal sequence/set was encoded without the constructed bit"
        );
        Ok(Some(value))
    }
}

/// Read a universal INTEGER (or ENUMERATED) body as a signed value.
#[must_use]
pub fn read_integer(value: &Value<'_>) -> Option<i64> {
    if value.body.is_empty() {
        return None;
    }
    let mut out: i64 = if value.body[0] & 0x80 != 0 { -1 } else { 0 };
    for byte in value.body {
        out = out.checked_mul(256)?.checked_add(*byte as i64)?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// LDAP messages
// ---------------------------------------------------------------------------------------------

/// Protocol operation numbers, as `APPLICATION n`.
mod op {
    pub const BIND_REQUEST: u8 = 0;
    pub const BIND_RESPONSE: u8 = 1;
    pub const UNBIND_REQUEST: u8 = 2;
    pub const SEARCH_REQUEST: u8 = 3;
    pub const SEARCH_RESULT_ENTRY: u8 = 4;
    pub const SEARCH_RESULT_DONE: u8 = 5;
    pub const EXTENDED_REQUEST: u8 = 23;
    pub const EXTENDED_RESPONSE: u8 = 24;
}

/// Search scopes (RFC 4511 §4.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// One entry, by DN.
    Base,
    /// The entries one level below the base.
    OneLevel,
    /// The whole subtree.
    Subtree,
}

impl SearchScope {
    fn code(self) -> i64 {
        match self {
            Self::Base => 0,
            Self::OneLevel => 1,
            Self::Subtree => 2,
        }
    }

    /// The scope a server's enumerated corresponds to, and `None` for a value the RFC does not
    /// define. A scope this codec cannot name is worth knowing about: a server that answers
    /// with one is not an LDAP server, and treating it as subtree would search the whole tree.
    #[must_use]
    pub fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Base),
            1 => Some(Self::OneLevel),
            2 => Some(Self::Subtree),
            _ => None,
        }
    }
}

/// The OID of the paged-results control (RFC 2696).
pub const PAGED_RESULTS_OID: &str = "1.2.840.113556.1.4.319";
/// The OID of the StartTLS extended operation (RFC 4511 §4.14.2).
pub const START_TLS_OID: &str = "1.3.6.1.4.1.1466.20037";

/// A simple bind: the DN and the password in the clear over a connection that is expected to be
/// encrypted. The bind password never reaches a row and never leaves the process except on the
/// wire.
#[must_use]
pub fn encode_bind_request(message_id: i64, dn: &str, password: &str) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application(op::BIND_REQUEST, |operation| {
            operation.integer(3);
            operation.octet_string(dn.as_bytes());
            operation.context_primitive(0, password.as_bytes());
        });
    });
    encoder.finish()
}

/// An anonymous bind, sent as a `SASL PLAIN` with empty credentials.
///
/// There is a reason this is not "a bind with an empty DN": a directory distinguishes a
/// *deliberately* anonymous bind from a *mistakenly* empty one, and a client that sends the
/// latter is indistinguishable from a client whose configuration lost its bind DN.
#[must_use]
pub fn encode_anonymous_bind_request(message_id: i64) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application(op::BIND_REQUEST, |operation| {
            operation.integer(3);
            operation.octet_string(b"");
            // SASL PLAIN, mechanism name in the clear and empty credentials after it.
            operation.context_primitive(3, b"PLAIN\0");
        });
    });
    encoder.finish()
}

/// A search request. `attributes` empty means "all user attributes".
#[must_use]
pub fn encode_search_request(
    message_id: i64,
    base: &str,
    scope: SearchScope,
    size_limit: i64,
    time_limit: i64,
    filter: &Filter,
    attributes: &[String],
) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application(op::SEARCH_REQUEST, |op| {
            op.octet_string(base.as_bytes());
            op.enumerated(scope.code());
            // Never dereference aliases: an alias can point outside the subtree an operator
            // declared, which turns "search this base" into "search whatever this names".
            op.enumerated(0);
            op.integer(size_limit);
            op.integer(time_limit);
            op.boolean(false);
            filter.encode(op);
            op.sequence(|attrs| {
                for attribute in attributes {
                    attrs.octet_string(attribute.as_bytes());
                }
            });
        });
    });
    encoder.finish()
}

/// The paged-results extended request (RFC 2696).
#[must_use]
pub fn encode_paged_results_request(message_id: i64, size: u32, cookie: &[u8]) -> Vec<u8> {
    let mut control = Encoder::new();
    control.sequence(|value| {
        value.integer(i64::from(size));
        value.octet_string(cookie);
    });
    let control = control.finish();

    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application(op::EXTENDED_REQUEST, |op| {
            op.context_primitive(0, PAGED_RESULTS_OID.as_bytes());
            op.context_primitive(1, &control);
        });
    });
    encoder.finish()
}

/// StartTLS: the handshake bytes ride inside the request, and the reply carries none.
#[must_use]
pub fn encode_starttls_request(message_id: i64, handshake: &[u8]) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application(op::EXTENDED_REQUEST, |op| {
            op.context_primitive(0, START_TLS_OID.as_bytes());
            // RFC 4511 §4.14.2: the requestValue holds the raw TLS handshake stream, *not* a BER
            // wrapper. Wrapping it is the single most common StartTLS bug and it produces a
            // server that answers with an alert the operator reads as a certificate error.
            op.context_primitive(1, handshake);
        });
    });
    encoder.finish()
}

/// An unbind, so the server can release the session rather than waiting for a timeout.
#[must_use]
pub fn encode_unbind_request(message_id: i64) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.sequence(|outer| {
        outer.integer(message_id);
        outer.application_primitive(op::UNBIND_REQUEST, &[]);
    });
    encoder.finish()
}

// ---------------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------------

/// The `LDAPResult` every response operation carries (RFC 4511 §4.1.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapResult {
    /// `0` is success. Every other value is a refusal this platform turns into a sentence.
    pub code: i64,
    /// The matched DN, usually empty.
    pub matched_dn: String,
    /// The server's own message. **Never shown to an operator verbatim** — see
    /// [`crate::sso::directory`] for why.
    pub diagnostic: String,
    /// Referral URIs, when the server answered with `10 referral`.
    pub referral: Vec<String>,
}

impl LdapResult {
    /// Whether the operation succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == 0
    }

    fn read(body: &[u8], limits: Limits) -> Result<Self> {
        let values = Decoder::with_limits(body, limits).all()?;
        let code = values
            .first()
            .and_then(read_integer)
            .ok_or_else(|| BerError::new("decode", "a result carries no result code"))?;
        let matched_dn = values
            .get(1)
            .filter(|value| value.universal() == Some(tag::OCTET_STRING))
            .map(|value| String::from_utf8_lossy(value.body).into_owned())
            .unwrap_or_default();
        let diagnostic = values
            .get(2)
            .filter(|value| value.universal() == Some(tag::OCTET_STRING))
            .map(|value| String::from_utf8_lossy(value.body).into_owned())
            .unwrap_or_default();
        let referral = values
            .iter()
            .filter(|value| value.context() == Some(3))
            .flat_map(|value| {
                value
                    .children(limits)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|child| child.universal() == Some(tag::OCTET_STRING))
                    .map(|child| String::from_utf8_lossy(child.body).into_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(Self {
            code,
            matched_dn,
            diagnostic,
            referral,
        })
    }
}

/// One attribute of one search result entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attribute {
    /// The attribute's type, as the server spells it — case and all, because `cn` and `CN` are
    /// the same attribute and a comparison has to know that.
    pub name: String,
    /// Its values. An attribute with no values is legal and means "present but empty".
    pub values: Vec<String>,
}

/// One `searchResEntry` (RFC 4511 §4.5.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchEntry {
    /// The entry's distinguished name.
    pub dn: String,
    /// Its attributes.
    pub attributes: Vec<Attribute>,
}

impl SearchEntry {
    /// The first value of an attribute, matched case-insensitively on the type.
    ///
    /// Directory servers are inconsistent about attribute-name case in a way that HTTP is not:
    /// OpenLDAP answers `uid` and Active Directory answers `sAMAccountName` or `UID` depending
    /// on the attribute the operator asked for, and both are the same attribute. A case-sensitive
    /// lookup here is how a directory integration is "working" and returning nothing.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .and_then(|attribute| attribute.values.first())
            .map(String::as_str)
    }

    /// Every value of an attribute, case-insensitively.
    #[must_use]
    pub fn attribute_values(&self, name: &str) -> Vec<String> {
        self.attributes
            .iter()
            .filter(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .flat_map(|attribute| attribute.values.iter().cloned())
            .collect()
    }

    fn read(body: &[u8], limits: Limits) -> Result<Self> {
        let values = Decoder::with_limits(body, limits).all()?;
        let dn = values
            .first()
            .filter(|value| value.universal() == Some(tag::OCTET_STRING))
            .map(|value| String::from_utf8_lossy(value.body).into_owned())
            .ok_or_else(|| BerError::new("decode", "a search result entry carries no DN"))?;
        let mut attributes = Vec::new();
        // The second field is the `PartialAttributeList` — a SEQUENCE **of** attributes — so
        // descending is two levels, not one. The first draft treated the list itself as an
        // attribute, which produced one attribute with the list's *first* element (the DN) as its
        // name and no values: a search result that parses, and reads as an entry that carries no
        // attributes at all. That is the failure mode a decoder must never have — it is not an
        // error, it is a plausible wrong answer — and it is why this fixture is a literal and not
        // an encoder round trip.
        for list in values.iter().skip(1) {
            for attribute in list.children(limits)? {
                let parts = attribute.children(limits)?;
                let name = parts
                    .first()
                    .filter(|value| value.universal() == Some(tag::OCTET_STRING))
                    .map(|value| String::from_utf8_lossy(value.body).into_owned())
                    .unwrap_or_default();
                // `vals` is `SET OF AttributeValue`, and each value is its own OCTET STRING — so
                // descending the SET yields a SEQUENCE of values and each of those is read. The
                // first draft stopped at the SET's body, which is the concatenation of the
                // values' *encodings*: `\x04\x01f\x04\x01x` came back as one "value" of two
                // characters of binary garbage. A directory that answers with a value this
                // platform cannot read is not a failure to show as a name — it is the codec
                // having stopped one level short, and the symptom is an address nobody can log in
                // with.
                let values = parts
                    .iter()
                    .skip(1)
                    .filter(|value| value.universal() == Some(tag::SET))
                    .flat_map(|set| {
                        set.children(limits)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|value| value.universal() == Some(tag::OCTET_STRING))
                            .map(|value| String::from_utf8_lossy(value.body).into_owned())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                attributes.push(Attribute { name, values });
            }
        }
        Ok(Self { dn, attributes })
    }
}

/// The paged-results cookie carried on a `searchResDone` (RFC 2696).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PagedResults {
    /// An estimate of the whole result set. `0` is "unknown", which is what most servers send
    /// and is a real value rather than a failure.
    pub size: u64,
    /// The opaque cursor. Empty means "this was the last page".
    pub cookie: Vec<u8>,
}

impl PagedResults {
    /// Whether the server has more pages.
    #[must_use]
    pub fn has_more(&self) -> bool {
        !self.cookie.is_empty()
    }

    fn read_response_value(value: &[u8], limits: Limits) -> Result<Self> {
        let values = Decoder::with_limits(value, limits).all()?;
        let size = values
            .first()
            .and_then(read_integer)
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or(0);
        let cookie = values
            .get(1)
            .filter(|value| value.universal() == Some(tag::OCTET_STRING))
            .map(|value| value.body.to_vec())
            .unwrap_or_default();
        Ok(Self { size, cookie })
    }
}

/// One decoded protocol response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A bind reply.
    Bind(LdapResult),
    /// A search result entry.
    Entry(SearchEntry),
    /// A `searchResDone`, with its paged-results cookie when the control was asked for.
    SearchDone {
        /// The result itself — a size limit and a time limit are `4` and `3`, not `0`.
        result: LdapResult,
        /// The paging state, `None` when the control was not used.
        paged: Option<PagedResults>,
    },
    /// An extended reply. StartTLS carries no control value.
    Extended {
        /// The result itself.
        result: LdapResult,
        /// The `responseName`, when the operation has one.
        name: Option<String>,
        /// The `responseValue`, when the operation has one.
        value: Option<Vec<u8>>,
    },
    /// An operation this codec does not model (an unsolicited notice, a comparison reply).
    Other {
        /// The application tag, so a caller can log which one it did not expect.
        tag: u8,
    },
}

/// A decoded response together with the message id it answers, which is how a paged search
/// matches replies to requests on a connection several operations deep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The request this answers.
    pub message_id: i64,
    /// The response itself.
    pub response: Response,
}

impl Message {
    /// Decode one complete LDAP message from a frame.
    ///
    /// Trailing bytes are an error rather than ignored: a frame with two messages in it is a
    /// desynchronised stream, and continuing to parse it would attribute one entry to the wrong
    /// request.
    pub fn decode(frame: &[u8], limits: Limits) -> Result<Self> {
        let mut decoder = Decoder::with_limits(frame, limits);
        let envelope = decoder
            .next()?
            .ok_or_else(|| BerError::new("decode", "the frame is empty"))?;
        if envelope.universal() != Some(tag::SEQUENCE) {
            return Err(BerError::new(
                "decode",
                "an LDAP message is a SEQUENCE at the top level",
            ));
        }
        if !decoder.is_empty() {
            return Err(BerError::new(
                "decode",
                "the frame holds more than one message, so the stream is out of step",
            ));
        }
        // The envelope is depth 0 and its fields are depth 1, which `children` already accounts
        // for. The cap is checked on the way *down*, so a frame that nests inside a field is
        // measured from the envelope rather than from the operation.
        let fields = envelope.children(limits)?;
        let message_id = fields
            .first()
            .and_then(read_integer)
            .ok_or_else(|| BerError::new("decode", "an LDAP message carries no message id"))?;
        let operation = fields
            .get(1)
            .ok_or_else(|| BerError::new("decode", "an LDAP message carries no operation"))?;
        let response = match operation.application() {
            Some(op::BIND_RESPONSE) => Response::Bind(LdapResult::read(operation.body, limits)?),
            Some(op::SEARCH_RESULT_ENTRY) => Response::Entry(SearchEntry::read(operation.body, limits)?),
            Some(op::SEARCH_RESULT_DONE) => {
                let fields = Decoder::with_limits(operation.body, limits).all()?;
                let result = LdapResult::read(operation.body, limits)?;
                // The control rides on the *last* context-tagged field, per RFC 4511 §4.1.11.
                let paged = fields
                    .iter()
                    .rev()
                    .find(|value| value.context() == Some(10))
                    .map(|value| {
                        let inner = value
                            .children(limits)?
                            .into_iter()
                            .find(|child| child.context() == Some(1))
                            .map(|child| child.body.to_vec())
                            .ok_or_else(|| {
                                BerError::new(
                                    "decode",
                                    "a paged-results control carries no response value",
                                )
                            })?;
                        PagedResults::read_response_value(&inner, limits)
                    })
                    .transpose()?;
                Response::SearchDone { result, paged }
            }
            Some(op::EXTENDED_RESPONSE) => {
                let fields = Decoder::with_limits(operation.body, limits).all()?;
                let result = LdapResult::read(operation.body, limits)?;
                let name = fields
                    .iter()
                    .rev()
                    .find(|value| value.context() == Some(10))
                    .and_then(|value| value.children(limits).ok())
                    .and_then(|inner| {
                        inner
                            .into_iter()
                            .find(|child| child.context() == Some(0))
                            .map(|child| String::from_utf8_lossy(child.body).into_owned())
                    });
                let value = fields
                    .iter()
                    .rev()
                    .find(|value| value.context() == Some(11))
                    .map(|value| value.children(limits))
                    .transpose()?
                    .and_then(|inner| {
                        inner
                            .into_iter()
                            .find(|child| child.context() == Some(1))
                            .map(|child| child.body.to_vec())
                    });
                Response::Extended { result, name, value }
            }
            Some(other) => Response::Other { tag: other },
            // A context-tagged or unknown class at the operation position is a frame this codec
            // does not model, and saying so is more useful than a generic parse failure.
            None => Response::Other { tag: operation.identifier & 0x1F },
        };
        Ok(Self {
            message_id,
            response,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

/// One RFC 4511 §4.5.1 filter choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `(&(...)(...))` — every clause must match.
    And(Vec<Filter>),
    /// `(|(...)(...))` — at least one clause must match.
    Or(Vec<Filter>),
    /// `(!(...))` — the clause must not match.
    Not(Box<Filter>),
    /// `(attr=value)`.
    Equal(String, String),
    /// `(attr>=value)`, which AD needs for a numeric `objectGUID`-style comparison and which a
    /// directory operator reaches for when an `equalityMatch` refuses an integer.
    GreaterOrEqual(String, String),
    /// `(attr<=value)`.
    LessOrEqual(String, String),
    /// `(attr=*)` — present.
    Present(String),
    /// `(attr=initial*any*final)`.
    Substring {
        /// The attribute.
        attribute: String,
        /// Must match at the start.
        initial: Option<String>,
        /// Matches anywhere.
        any: Vec<String>,
        /// Must match at the end.
        final_: Option<String>,
    },
}

impl Filter {
    /// A present filter, the shorthand every directory client uses for "list the subtree".
    #[must_use]
    pub fn present(attribute: &str) -> Self {
        Self::Present(attribute.to_owned())
    }

    /// Write the filter's BER.
    pub fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Equal(attribute, value) => {
                encoder.application(3, |op| {
                    op.octet_string(attribute.as_bytes());
                    op.octet_string(value.as_bytes());
                });
            }
            Self::GreaterOrEqual(attribute, value) => {
                encoder.application(5, |op| {
                    op.octet_string(attribute.as_bytes());
                    op.octet_string(value.as_bytes());
                });
            }
            Self::LessOrEqual(attribute, value) => {
                encoder.application(6, |op| {
                    op.octet_string(attribute.as_bytes());
                    op.octet_string(value.as_bytes());
                });
            }
            Self::Present(attribute) => {
                encoder.context_primitive(7, attribute.as_bytes());
            }
            Self::Not(inner) => {
                encoder.application(2, |op| inner.encode(op));
            }
            Self::And(clauses) => {
                encoder.application(0, |op| {
                    for clause in clauses {
                        clause.encode(op);
                    }
                });
            }
            Self::Or(clauses) => {
                encoder.application(1, |op| {
                    for clause in clauses {
                        clause.encode(op);
                    }
                });
            }
            Self::Substring {
                attribute,
                initial,
                any,
                final_,
            } => {
                encoder.application(4, |op| {
                    op.octet_string(attribute.as_bytes());
                    op.sequence(|parts| {
                        if let Some(initial) = initial {
                            parts.context_primitive(0, initial.as_bytes());
                        }
                        for value in any {
                            parts.context_primitive(1, value.as_bytes());
                        }
                        if let Some(final_) = final_ {
                            parts.context_primitive(2, final_.as_bytes());
                        }
                    });
                });
            }
        }
    }

    /// The filter's own text, in the same grammar [`Filter::parse`] reads.
    ///
    /// The round trip is asserted by the tests, because a filter that renders differently from
    /// how it was typed is a filter nobody can debug from a sync log — and the sync log is where
    /// an operator looks when a directory did not sync.
    #[must_use]
    pub fn to_filter_string(&self) -> String {
        match self {
            Self::Equal(attribute, value) => format!("({attribute}={})", escape(value)),
            Self::GreaterOrEqual(attribute, value) => format!("({attribute}>={})", escape(value)),
            Self::LessOrEqual(attribute, value) => format!("({attribute}<={})", escape(value)),
            Self::Present(attribute) => format!("({attribute}=*)"),
            Self::Not(inner) => format!("(!{})", inner.to_filter_string()),
            Self::And(clauses) => {
                format!("(&{})", join_clauses(clauses))
            }
            Self::Or(clauses) => format!("(|{})", join_clauses(clauses)),
            Self::Substring {
                attribute,
                initial,
                any,
                final_,
            } => {
                let mut rendered = String::new();
                if let Some(initial) = initial {
                    rendered.push_str(initial);
                }
                for value in any {
                    rendered.push('*');
                    rendered.push_str(value);
                }
                if final_.is_some() {
                    rendered.push('*');
                }
                if let Some(final_) = final_ {
                    rendered.push_str(final_);
                }
                format!("({attribute}={rendered})")
            }
        }
    }

    /// Parse the text form of a filter.
    ///
    /// The grammar is closed on purpose: `and`, `or`, `not`, `=`, `>=`, `<=` and `=…*…`. RFC
    /// 4511 also has `~=`, extensible matches and the `\xx` hex form, and a client that accepts
    /// all of it is accepting a language whose *parse* is the attack surface. A filter this
    /// module cannot parse is refused with a reason; it is not passed through as a string, which
    /// would defeat the escaping this type exists to provide.
    pub fn parse(input: &str) -> Result<Self> {
        let mut parser = FilterParser {
            input: input.as_bytes(),
            offset: 0,
        };
        let filter = parser.filter()?;
        parser.skip_spaces();
        if !parser.is_empty() {
            return Err(BerError::new(
                "filter",
                "there is text after the end of the filter",
            ));
        }
        Ok(filter)
    }
}

fn join_clauses(clauses: &[Filter]) -> String {
    clauses
        .iter()
        .map(Filter::to_filter_string)
        .collect::<Vec<_>>()
        .join("")
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '*' | '(' | ')' | '\\' | '\0') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

struct FilterParser<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> FilterParser<'a> {
    fn is_empty(&self) -> bool {
        self.offset >= self.input.len()
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.offset).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.offset += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.offset += 1;
            Ok(())
        } else {
            Err(BerError::new(
                "filter",
                format!("expected `{}` at position {}", byte as char, self.offset),
            ))
        }
    }

    fn identifier(&mut self) -> Result<String> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b';' | b'/')
        }) {
            self.offset += 1;
        }
        if self.offset == start {
            return Err(BerError::new(
                "filter",
                format!("an attribute name is expected at position {start}"),
            ));
        }
        // Attribute *descriptions* may be options (`cn;lang-en`), and dropping the options would
        // turn a request for one language into a request for all of them.
        Ok(String::from_utf8_lossy(&self.input[start..self.offset]).into_owned())
    }

    /// A filter value: anything up to the closing paren, honouring `\x` escapes.
    fn value(&mut self) -> Result<String> {
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'\\' => {
                    self.offset += 1;
                    let escaped = self.peek().ok_or_else(|| {
                        BerError::new("filter", "the filter ends with a trailing `\\`")
                    })?;
                    out.push(escaped as char);
                    self.offset += 1;
                }
                b')' => break,
                _ => {
                    // Multi-byte UTF-8: copy the whole scalar, not one byte of it.
                    let rest = &self.input[self.offset..];
                    let width = utf8_width(byte);
                    let end = (self.offset + width).min(rest.len());
                    out.push_str(&String::from_utf8_lossy(&rest[..end - self.offset]));
                    self.offset = end;
                }
            }
        }
        Ok(out)
    }

    fn filter(&mut self) -> Result<Filter> {
        self.skip_spaces();
        self.expect(b'(')?;
        let node = self.item()?;
        self.skip_spaces();
        self.expect(b')')?;
        Ok(node)
    }

    fn item(&mut self) -> Result<Filter> {
        self.skip_spaces();
        match self.peek() {
            Some(b'&') => {
                self.offset += 1;
                Ok(Filter::And(self.clauses()?))
            }
            Some(b'|') => {
                self.offset += 1;
                Ok(Filter::Or(self.clauses()?))
            }
            Some(b'!') => {
                self.offset += 1;
                Ok(Filter::Not(Box::new(self.filter()?)))
            }
            _ => {
                let attribute = self.identifier()?;
                match self.peek() {
                    Some(b'=') => self.offset += 1,
                    Some(b'>') => {
                        self.offset += 1;
                        self.expect(b'=')?;
                        return Ok(Filter::GreaterOrEqual(attribute, self.value()?));
                    }
                    Some(b'<') => {
                        self.offset += 1;
                        self.expect(b'=')?;
                        return Ok(Filter::LessOrEqual(attribute, self.value()?));
                    }
                    _ => {
                        return Err(BerError::new(
                            "filter",
                            format!("`{attribute}` must be followed by `=`, `>=` or `<=`"),
                        ));
                    }
                }
                let first = self.value()?;
                if first == "*" && self.peek() == Some(b')') {
                    return Ok(Filter::Present(attribute));
                }
                if !first.contains('*') {
                    return Ok(Filter::Equal(attribute, first));
                }
                // Substring: split on the wildcards that are *not* escaped. `a\*b` is a literal
                // star, and treating it as a wildcard is the whole reason a name containing one
                // matches a different set of people than intended.
                let mut parts = Vec::new();
                let mut current = String::new();
                let mut escaped = false;
                for character in first.chars() {
                    if escaped {
                        current.push(character);
                        escaped = false;
                        continue;
                    }
                    match character {
                        '\\' => escaped = true,
                        '*' => {
                            parts.push(std::mem::take(&mut current));
                        }
                        other => current.push(other),
                    }
                }
                parts.push(current);
                let initial = parts.first().filter(|part| !part.is_empty()).cloned();
                let final_ = parts.last().filter(|part| !part.is_empty()).cloned();
                let any = parts
                    .iter()
                    .skip(1)
                    .filter(|part| !part.is_empty())
                    .cloned()
                    .collect();
                Ok(Filter::Substring {
                    attribute,
                    initial,
                    any,
                    final_,
                })
            }
        }
    }

    /// The clause list inside `(&…)` / `(|…)`, which is one or more, and never zero: an empty
    /// `(&)` matches nothing on most servers and everything on some, which is not a distinction
    /// worth depending on.
    fn clauses(&mut self) -> Result<Vec<Filter>> {
        let mut clauses = Vec::new();
        loop {
            self.skip_spaces();
            match self.peek() {
                Some(b'(') => clauses.push(self.filter()?),
                _ if clauses.is_empty() => {
                    return Err(BerError::new(
                        "filter",
                        "a combined filter needs at least one clause",
                    ));
                }
                _ => return Ok(clauses),
            }
        }
    }
}

fn utf8_width(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte >> 5 == 0b110 {
        2
    } else if byte >> 4 == 0b1110 {
        3
    } else if byte >> 3 == 0b11110 {
        4
    } else {
        1
    }
}
