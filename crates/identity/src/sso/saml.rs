//! SAML 2.0: a posted, signed assertion (docs/07-IAM.md §11; REQ-006, slice 4b-2).
//!
//! SAML differs from the `code` flows in one way that matters for security: the signature is
//! **not** over a token this server fetched, it is over bytes the browser posted back, and the
//! browser is the one that chose them. The order here is fixed and cannot be relaxed:
//!
//! 1. parse the posted XML, refusing entity declarations outright — an "assertion" carrying a
//!    billion-laughs payload is an attack, not an identity;
//! 2. locate the `<saml:Assertion>` element and keep its **exact** source bytes, because that is
//!    what a signature covers;
//! 3. verify the XML signature — the declared digest must match the referenced element, and the
//!    RSA signature over the `SignedInfo` must verify against the configured certificate;
//! 4. only then read the issuer, the audience, the subject, the window and the attributes.
//!
//! Step 3 is where the claims become trustworthy. XML Signature binds the document to the key in
//! two independent steps, and **both** are checked: the `DigestValue` ties the `SignedInfo` to
//! the assertion (so a changed claim breaks it), and the RSA signature makes the `SignedInfo`
//! unforgeable (so a recomputed digest does not help an attacker). Skipping either leaves a hole.

use base64::Engine;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::Pkcs1v15Sign;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use xmltree::{Element, XMLNode};

use crate::error::{IdentityError, Result};

/// Base64 decoder for the wire strings SAML carries (signatures, digests and certificates).
fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Largest SAML response accepted, in bytes. An assertion is a few kilobytes; megabytes are an
/// attack.
pub const MAX_ASSERTION_BYTES: usize = 512 * 1024;

/// Largest `SignatureValue` or digest accepted inside one signature.
const MAX_SIGNATURE_FIELD: usize = 16 * 1024;

/// The algorithms this platform verifies: RSA-SHA256 and RSA-SHA1, the two every directory in
/// practice offers. SHA-1 is accepted **only** for SAML, where it is what the ecosystem still
/// signs with, and never for a JWT.
const ALLOWED_ALGORITHMS: [&str; 2] = [
    "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256",
    "http://www.w3.org/2000/09/xmldsig#rsa-sha1",
];

/// The SAML time bounds, in seconds of tolerance either way.
const CLOCK_SKEW_SECONDS: i64 = 120;

/// A verified SAML assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamlAssertion {
    /// The provider's issuer (must equal the configured entity id).
    pub issuer: String,
    /// Our entity id, as the assertion named it as audience.
    pub audience: String,
    /// The provider's subject id.
    pub subject_id: String,
    /// The email the assertion carries, lowercased.
    pub email: String,
    /// The display name, when the assertion carries one.
    pub display_name: Option<String>,
    /// Group values from the configured attribute.
    pub groups: Vec<String>,
    /// Every attribute, for ABAC conditions.
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

/// The configuration a SAML provider needs to verify its assertions.
#[derive(Debug, Clone)]
pub struct SamlConfig {
    /// The provider's entity id — the exact string its assertions must carry as `Issuer`.
    pub issuer: String,
    /// Our own entity id (the audience a valid assertion must name).
    pub audience: String,
    /// The provider's signing certificate, PEM.
    pub certificate_pem: String,
    /// Attribute name carrying the email.
    pub email_attribute: String,
    /// Attribute name carrying groups, when the provider sends them.
    pub group_attribute: Option<String>,
    /// Attribute to show as the display name.
    pub display_name_attribute: Option<String>,
}

/// Whether a configured certificate can be read as the RSA key a signature needs.
///
/// The management API's `test` action needs to answer one question about a SAML provider: *can
/// this platform verify an assertion this directory signs?* — and the answer has to come from the
/// same parser, not from a second implementation of it that could drift. So this is that parser's
/// certificate step, exposed on its own.
///
/// The full [`verify_response`] cannot answer it: it refuses at the first thing that is missing,
/// and a probe document is missing its signature long before it reaches the key. Exposing the one
/// step keeps the test honest — a certificate copied with its `BEGIN` line missing, or a base64
/// blob that lost its wrapping, is reported here rather than at the first real sign-in.
pub fn certificate_is_readable(pem: &str) -> Result<()> {
    certificate_key(pem).map(|_| ())
}

/// A complete, unsigned assertion carrying every attribute a configuration names.
///
/// The `test` action needs to prove the *attribute wiring* too — a typo in `email_attribute` is
/// invisible until a real assertion arrives with no address the reader recognises. This is the
/// document that probe runs, built from the same code the reader runs on, so a name that does not
/// survive the round trip is reported at configuration time rather than at the first sign-in.
///
/// The window is deliberately far in the future: a probe is a *shape* check, and a document that
/// expired would be refused for the wrong reason.
#[must_use]
pub fn probe_document(
    issuer: &str,
    audience: &str,
    email_attribute: &str,
    group_attribute: Option<&str>,
    display_name_attribute: Option<&str>,
) -> String {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut attributes = vec![
        (email_attribute.to_owned(), "probe@omnion.test".to_owned()),
        (
            display_name_attribute.unwrap_or("displayName").to_owned(),
            "Probe User".to_owned(),
        ),
    ];
    if let Some(group) = group_attribute {
        attributes.push((group.to_owned(), "probe-group".to_owned()));
        // `read_attributes` takes the *first* value of a repeated name as a string and the rest
        // as a list, so a group attribute is written twice — otherwise the probe would only ever
        // prove the single-value path, and a configuration whose directory sends a list would look
        // fine here and lose every group at the first real sign-in.
        attributes.push((group.to_owned(), "probe-group-two".to_owned()));
    }

    let attributes_xml = attributes
        .iter()
        .map(|(name, value)| {
            format!(
                r#"<saml:Attribute Name="{name}"><saml:AttributeValue>{value}</saml:AttributeValue></saml:Attribute>"#
            )
        })
        .collect::<String>();

    // Escaped because both halves come from a provider row an operator typed.
    //
    // The `xmlns:saml` declaration is repeated **on the assertion**, not only on the response,
    // and that is not decoration: the reader parses the assertion element on its own, so a prefix
    // it uses has to be declared where it is used. Relying on an ancestor's declaration parses in
    // a browser and fails in the only reader that matters.
    format!(
        r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" Version="2.0" ID="probe-response"><saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="probe-assertion"><saml:Issuer>{issuer}</saml:Issuer><saml:Subject><saml:NameID>probe-subject</saml:NameID></saml:Subject><saml:Conditions NotBefore="{not_before}" NotOnOrAfter="{not_after}"><saml:AudienceRestriction><saml:Audience>{audience}</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AttributeStatement>{attributes_xml}</saml:AttributeStatement></saml:Assertion></samlp:Response>"#,
        issuer = xml_escape(issuer),
        audience = xml_escape(audience),
        attributes_xml = attributes_xml,
        not_before = now - 600,
        not_after = now + 600,
    )
}

/// Run the claim half of the reader over a probe document, stopping before the signature.
///
/// Everything an operator can mistype in a SAML configuration — the entity id, the audience, the
/// attribute names, the window — is checked here, and the certificate is checked separately by
/// [`certificate_is_readable`]. What is left is the one thing no configuration can be wrong about,
/// because it is the provider's own signature.
pub fn probe_claims(document: &str, config: &SamlConfig) -> Result<SamlAssertion> {
    verify_response_unverified(document, config)
}

/// Escape the five characters that change the meaning of XML text or an attribute value.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Parse and verify a posted SAML response.
pub fn verify_response(document: &str, config: &SamlConfig) -> Result<SamlAssertion> {
    let (element, raw) = checked_document(document)?;

    // The signature must be inside the assertion: signing the response and not the assertion would
    // leave the claims themselves unsigned.
    let signature = find_signature(&element)
        .ok_or_else(|| IdentityError::InvalidProvider("the assertion is not signed".into()))?;
    verify_signature(signature, &raw, &element, config)?;

    read_claims(&element, config)
}

/// The shape checks every document gets before anything is read out of it.
fn checked_document(document: &str) -> Result<(Element, String)> {
    if document.len() > MAX_ASSERTION_BYTES {
        return Err(IdentityError::InvalidProvider(
            "the assertion is implausibly large".into(),
        ));
    }
    // An XML external entity or a nested-entity bomb is refused before any parsing.
    if document.contains("<!ENTITY") || document.contains("<!DOCTYPE") {
        return Err(IdentityError::InvalidProvider(
            "the assertion carries an entity declaration".into(),
        ));
    }
    find_assertion(document)
        .ok_or_else(|| IdentityError::InvalidProvider("the response carries no assertion".into()))
}

/// The claim half of [`verify_response`], split out so a configuration probe can run it.
///
/// Everything here is a comparison between what a document says and what a provider row claims —
/// issuer, audience, window, attribute names. None of it depends on the signature, which is why
/// the probe can check the whole of it while the signature is left to the real thing.
pub fn verify_response_unverified(document: &str, config: &SamlConfig) -> Result<SamlAssertion> {
    let (element, _raw) = checked_document(document)?;
    read_claims(&element, config)
}

/// Read a verified assertion's claims, refusing anything that contradicts the configuration.
fn read_claims(element: &Element, config: &SamlConfig) -> Result<SamlAssertion> {
    let issuer = child_text(element, "Issuer")
        .ok_or_else(|| IdentityError::InvalidProvider("the assertion names no issuer".into()))?;
    if issuer.trim() != config.issuer.trim() {
        return Err(IdentityError::InvalidProvider(
            "the assertion comes from a different issuer".into(),
        ));
    }

    let audience = read_audience(element)
        .ok_or_else(|| IdentityError::InvalidProvider("the assertion names no audience".into()))?;
    if audience.trim() != config.audience.trim() {
        return Err(IdentityError::InvalidProvider(
            "the assertion is not for this application".into(),
        ));
    }

    check_timestamps(element)?;

    let subject = read_subject_id(element)
        .ok_or_else(|| IdentityError::InvalidProvider("the assertion names no subject".into()))?;

    let attributes = read_attributes(element);
    let email = attributes
        .get(&config.email_attribute)
        .or_else(|| attributes.get("email"))
        .or_else(|| attributes.get("mail"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            IdentityError::InvalidProvider("the assertion carries no email address".into())
        })?
        .to_ascii_lowercase();
    if !email.contains('@') {
        return Err(IdentityError::InvalidProvider(
            "the assertion carries an email address that is not an address".into(),
        ));
    }

    let groups = config
        .group_attribute
        .as_ref()
        .and_then(|name| attributes.get(name))
        .map(crate::sso::claims::collect_groups)
        .unwrap_or_default();

    let display_name_attribute = config
        .display_name_attribute
        .as_deref()
        .unwrap_or("displayName");
    let display_name = attributes
        .get(display_name_attribute)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    Ok(SamlAssertion {
        issuer: issuer.trim().to_owned(),
        audience: audience.trim().to_owned(),
        subject_id: subject,
        email,
        display_name,
        groups,
        attributes,
    })
}

/// The qualified name of an opening tag: the text between the `<` and the first space, `>` or `/`.
///
/// The name is located **before** any namespace prefix is dropped, because an attribute value
/// (`xmlns:saml="urn:…:assertion"`) contains colons of its own — splitting on `:` first reads the
/// attribute as if it were the element's name.
fn qualified_name(tag: &str) -> &str {
    tag.trim_start_matches('<')
        .split([' ', '\t', '\n', '\r', '>', '/'])
        .next()
        .unwrap_or_default()
}

/// The local name of an opening tag, without its namespace prefix.
fn local_name_of(tag: &str) -> &str {
    qualified_name(tag).rsplit(':').next().unwrap_or_default()
}

/// Locate the assertion element and return it with its exact source bytes, which is what the
/// signature covers.
fn find_assertion(document: &str) -> Option<(Element, String)> {
    let mut cursor = 0;
    while let Some(start) = document[cursor..].find('<') {
        let absolute = cursor + start;
        let tag_end = document[absolute..].find('>')?;
        // Closing tags, comments, declarations and processing instructions all open with `<` too;
        // only an opening tag can start the element being looked for.
        if ["</", "<!--", "<?", "<!"]
            .iter()
            .any(|prefix| document[absolute..].starts_with(prefix))
        {
            cursor = absolute + tag_end + 1;
            continue;
        }

        let tag = &document[absolute..absolute + tag_end + 1];
        if local_name_of(tag) == "Assertion" {
            let end = find_close(document, &tag, absolute)?;
            let raw = document[absolute..end].to_owned();
            let element = Element::parse(raw.as_bytes()).ok()?;
            return Some((element, raw));
        }
        cursor = absolute + tag_end + 1;
    }
    None
}

/// Find the end offset (exclusive) of the element that opens at `start` with `tag`.
fn find_close(document: &str, tag: &str, start: usize) -> Option<usize> {
    if tag.trim_end().ends_with("/>") {
        return None;
    }
    // The closing tag is spelled with the prefix the opening tag used: `<saml:Assertion>` is closed
    // by `</saml:Assertion>`, not by `</Assertion>`.
    let closing = format!("</{}>", qualified_name(tag));
    let relative = document[start..].find(&closing)?;
    let close_start = start + relative;
    let close_end = document[close_start..].find('>')? + close_start;
    Some(close_end + 1)
}

/// The pieces of an XML signature this module verifies, plus the parsed node it came from.
struct SignatureBlock<'a> {
    /// The `SignatureMethod` URI.
    algorithm: String,
    /// The base64 `SignatureValue`.
    signature: String,
    /// The parsed `<ds:Signature>` element.
    node: &'a Element,
}

/// Read the signature block of an assertion.
fn find_signature(element: &Element) -> Option<SignatureBlock<'_>> {
    let node = find_child(element, "Signature")?;
    // `<ds:SignatureMethod Algorithm="…"/>` is an empty element: the algorithm is its attribute,
    // never its text. Reading it as text is the classic SAML mis-parse.
    let method = find_child(node, "SignatureMethod").or_else(|| {
        find_child(node, "SignedInfo").and_then(|info| find_child(info, "SignatureMethod"))
    })?;
    let algorithm = attr(method, "Algorithm")?.to_owned();
    if !ALLOWED_ALGORITHMS.contains(&algorithm.as_str()) {
        return None;
    }
    let signature = find_child(node, "SignatureValue")
        .and_then(|value| value.get_text())
        .map(|text| text.trim().to_owned())
        .filter(|value| !value.is_empty() && value.len() <= MAX_SIGNATURE_FIELD)?;
    Some(SignatureBlock {
        algorithm,
        signature,
        node,
    })
}

/// The signed bytes of the assertion under the **enveloped-signature transform**.
///
/// A document cannot contain a signature over itself, so SAML defines what "itself" means: the
/// digest covers the element with its own `<ds:Signature>` subtree removed. That is the transform
/// every SAML implementation applies, and it is what this function reproduces from the exact
/// posted bytes — so a change anywhere else in the assertion still invalidates the signature.
///
/// The subtree is located from the **end of the opening tag**, so the search cannot land back on
/// the opening tag itself.
fn enveloped_bytes(raw: &str) -> Result<Vec<u8>> {
    let open_at = raw
        .find("<ds:Signature")
        .or_else(|| raw.find("<Signature"))
        .ok_or_else(|| {
            IdentityError::InvalidProvider("the assertion carries no signature element".into())
        })?;

    // The qualified name, then the end of the opening tag.
    let name_end = open_at
        + raw[open_at..].find([' ', '>', '/']).ok_or_else(|| {
            IdentityError::InvalidProvider("the signature element is malformed".into())
        })?;
    let name = raw[open_at..name_end].trim_start_matches('<');
    let body_start = raw[open_at..]
        .find('>')
        .map(|offset| open_at + offset + 1)
        .ok_or_else(|| {
            IdentityError::InvalidProvider("the signature element is malformed".into())
        })?;

    let closing = format!("</{name}>");
    let close_at = raw[body_start..].find(&closing).ok_or_else(|| {
        IdentityError::InvalidProvider("the signature element is not closed".into())
    })?;
    let end = body_start + close_at + closing.len();

    let mut bytes = Vec::with_capacity(raw.len());
    bytes.extend_from_slice(&raw.as_bytes()[..open_at]);
    bytes.extend_from_slice(&raw.as_bytes()[end..]);
    Ok(bytes)
}

/// The signed bytes of the `<ds:SignedInfo>` element — what RSA signs in XML Signature.
///
/// The end is the **end** of the closing tag, not where it starts; slicing at the match index
/// would drop the closing tag and hash a different byte string than the provider signed.
fn signed_info_bytes(raw: &str) -> Option<Vec<u8>> {
    let (open, close) = match raw.find("<ds:SignedInfo") {
        Some(start) => (start, "</ds:SignedInfo>"),
        None => (raw.find("<SignedInfo")?, "</SignedInfo>"),
    };
    let end = raw[open..].find(close)? + close.len();
    Some(raw.as_bytes()[open..open + end].to_vec())
}

/// The `URI` of the reference the signature covers.
fn signed_info_reference(signature: &Element) -> Option<String> {
    let reference =
        find_child(signature, "SignedInfo").and_then(|info| find_child(info, "Reference"))?;
    Some(attr(reference, "URI")?.to_owned())
}

/// The `DigestValue` the signature declares, decoded to bytes.
///
/// Read off the **parsed** signature rather than a raw-string fragment: a `<ds:SignedInfo>` slice
/// on its own is not well-formed XML (it uses the `ds:` prefix without declaring it), so parsing
/// the fragment would fail on a perfectly valid assertion.
fn signed_info_digest(signature: &Element) -> Option<Vec<u8>> {
    let reference =
        find_child(signature, "SignedInfo").and_then(|info| find_child(info, "Reference"))?;
    let value = find_child(reference, "DigestValue")?;
    b64().decode(value.get_text()?.trim().as_bytes()).ok()
}

/// The `ID` of the assertion element, without its `#`.
///
/// Read off the parsed opening tag rather than by scanning the raw text: the attribute map is
/// already separated, so a value that merely contains `ID=` cannot be mistaken for the attribute.
fn assertion_id(element: &Element) -> Option<String> {
    attr(element, "ID").map(|value| value.trim_start_matches('#').to_owned())
}

/// Verify a posted assertion's signature — both halves of the binding.
fn verify_signature(
    block: SignatureBlock<'_>,
    raw: &str,
    element: &Element,
    config: &SamlConfig,
) -> Result<()> {
    let signature = b64()
        .decode(block.signature.as_bytes())
        .map_err(|_| IdentityError::InvalidProvider("the signature value is not base64".into()))?;
    let public_key = certificate_key(&config.certificate_pem)?;

    let padding = if block.algorithm == ALLOWED_ALGORITHMS[1] {
        Pkcs1v15Sign::new::<sha1::Sha1>()
    } else {
        Pkcs1v15Sign::new::<Sha256>()
    };

    // The reference must name this assertion (or the whole document). A reference to some other
    // element would make the digest check meaningless.
    let reference = signed_info_reference(block.node)
        .ok_or_else(|| IdentityError::InvalidProvider("the signature references nothing".into()))?;
    if !(reference.is_empty()
        || assertion_id(element) == Some(reference.trim_start_matches('#').to_owned()))
    {
        return Err(IdentityError::InvalidProvider(
            "the signature covers a different element".into(),
        ));
    }

    // 1. The declared digest must match the referenced element — this is what makes a changed
    //    claim fail.
    let declared = signed_info_digest(block.node)
        .ok_or_else(|| IdentityError::InvalidProvider("the signature declares no digest".into()))?;
    let computed = digest_for(&block.algorithm, &enveloped_bytes(raw)?);
    if !bool::from(computed.ct_eq(&declared)) {
        return Err(IdentityError::InvalidProvider(
            "the assertion does not match its own signature digest".into(),
        ));
    }

    // 2. The signature over the SignedInfo must verify — this is what makes it unforgeable.
    let signed = signed_info_bytes(raw).ok_or_else(|| {
        IdentityError::InvalidProvider("the signature names no SignedInfo".into())
    })?;
    public_key
        .verify(padding, &digest_for(&block.algorithm, &signed), &signature)
        .map_err(|_| {
            IdentityError::InvalidProvider(
                "the assertion signature is not valid for this provider".into(),
            )
        })
}

/// Hash the signed bytes with the algorithm the signature names.
fn digest_for(algorithm: &str, raw: &[u8]) -> Vec<u8> {
    if algorithm == ALLOWED_ALGORITHMS[1] {
        sha1::Sha1::digest(raw).to_vec()
    } else {
        Sha256::digest(raw).to_vec()
    }
}

/// Parse a PEM certificate into the RSA public key it carries.
fn certificate_key(pem: &str) -> Result<RsaPublicKey> {
    let cleaned: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(|line| line.chars())
        .filter(|character| !character.is_whitespace())
        .collect();
    let der = b64().decode(cleaned.as_bytes()).map_err(|_| {
        IdentityError::InvalidProvider("the signing certificate is not valid base64".into())
    })?;

    // `der`/`spki` are not needed here: the reader below walks the fixed RFC 5280 shape directly,
    // and refusing anything that does not match is safer than a lenient parser.
    let (modulus, exponent) = spki_rsa_key(&der).ok_or_else(|| {
        IdentityError::InvalidProvider("the signing certificate is not an RSA certificate".into())
    })?;

    RsaPublicKey::new(
        rsa::BigUint::from_bytes_be(&modulus),
        rsa::BigUint::from_bytes_be(&exponent),
    )
    .map_err(|_| {
        IdentityError::InvalidProvider("the signing certificate is not a valid RSA key".into())
    })
}

/// Read the RSA modulus and exponent out of a DER `SubjectPublicKeyInfo`.
///
/// The structure is fixed by RFC 5280: `SEQUENCE { AlgorithmIdentifier, subjectPublicKey BIT
/// STRING }`, and the bit string wraps `SEQUENCE { modulus INTEGER, publicExponent INTEGER }`.
/// Anything that does not match is refused rather than guessed at.
fn spki_rsa_key(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (contents, _) = der_sequence(der)?;
    let mut reader = Reader::new(contents);
    reader.element(0x30)?; // the algorithm identifier, skipped
    let bit_string = reader.element(0x03)?;
    // A BIT STRING's contents begin with the unused-bit count, which is 0 for a key.
    let key_der = bit_string.strip_prefix(&[0x00])?;
    let (rsa_der, _) = der_sequence(key_der)?;
    let mut rsa_reader = Reader::new(rsa_der);
    let modulus = rsa_reader.integer()?;
    let exponent = rsa_reader.integer()?;
    Some((modulus, exponent))
}

/// Read one DER SEQUENCE, returning its contents and the bytes after it.
fn der_sequence(input: &[u8]) -> Option<(&[u8], &[u8])> {
    if *input.first()? != 0x30 {
        return None;
    }
    let (length, rest) = der_length(&input[1..])?;
    Some((rest.get(..length)?, rest.get(length..)?))
}

/// Read a DER length, short form or long form.
fn der_length(input: &[u8]) -> Option<(usize, &[u8])> {
    let first = *input.first()?;
    if first < 0x80 {
        return Some((usize::from(first), &input[1..]));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > 4 {
        return None;
    }
    let mut length = 0_usize;
    for byte in &input[1..=count] {
        length = (length << 8) | usize::from(*byte);
    }
    Some((length, input.get(count + 1..)?))
}

/// A cursor over a sequence's DER contents.
struct Reader<'a> {
    input: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over a slice.
    fn new(input: &'a [u8]) -> Self {
        Self { input }
    }

    /// Read the next element with the given tag, returning its **contents**; its own tag and
    /// length are consumed here.
    fn element(&mut self, tag: u8) -> Option<&'a [u8]> {
        if *self.input.first()? != tag {
            return None;
        }
        let (length, rest) = der_length(&self.input[1..])?;
        let contents = rest.get(..length)?;
        self.input = rest.get(length..)?;
        Some(contents)
    }

    /// Read an INTEGER as unsigned big-endian bytes, dropping the DER sign byte.
    fn integer(&mut self) -> Option<Vec<u8>> {
        let mut value = self.element(0x02)?.to_vec();
        // A positive DER INTEGER whose top bit is set carries a leading 0x00 so it does not read as
        // negative; that padding byte is not part of the number.
        if value.first() == Some(&0x00) {
            value.remove(0);
        }
        (!value.is_empty()).then_some(value)
    }
}

/// The text of a direct child by local name.
fn child_text(element: &Element, name: &str) -> Option<String> {
    find_child(element, name)
        .and_then(|node| node.get_text())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// A direct child **element** by local name, ignoring any namespace prefix and any text node.
fn find_child<'a>(element: &'a Element, name: &str) -> Option<&'a Element> {
    element
        .children
        .iter()
        .filter_map(|child| match child {
            XMLNode::Element(element) => Some(element),
            _ => None,
        })
        .find(|child| local_name_of(&child.name) == name)
}

/// An element attribute by name, trimmed.
fn attr<'a>(element: &'a Element, name: &str) -> Option<&'a str> {
    element
        .attributes
        .get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

/// The audience a `Conditions/AudienceRestriction` names.
fn read_audience(element: &Element) -> Option<String> {
    let conditions = find_child(element, "Conditions")?;
    let restriction = find_child(conditions, "AudienceRestriction")?;
    find_child(restriction, "Audience")
        .and_then(|node| node.get_text())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The subject's `NameID`, falling back to the confirmation data when the NameID is opaque.
fn read_subject_id(element: &Element) -> Option<String> {
    if let Some(subject) = find_child(element, "Subject")
        && let Some(name_id) = find_child(subject, "NameID")
        && let Some(text) = name_id.get_text()
    {
        let text = text.trim().to_owned();
        if !text.is_empty() {
            return Some(text);
        }
    }
    let subject = find_child(element, "Subject")?;
    let confirmation = find_child(subject, "SubjectConfirmation")?;
    let data = find_child(confirmation, "SubjectConfirmationData")?;
    Some(attr(data, "InResponseTo")?.to_owned())
}

/// Check `Conditions/NotBefore` and `NotOnOrAfter`.
///
/// An assertion with **no** window is refused: an assertion that never expires is a replay
/// invitation, and SAML's model is that the relying party bounds its own acceptance.
fn check_timestamps(element: &Element) -> Result<()> {
    let Some(conditions) = find_child(element, "Conditions") else {
        return Err(IdentityError::InvalidProvider(
            "the assertion has no validity window".into(),
        ));
    };

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Some(not_before) =
        attr(conditions, "NotBefore").and_then(|value| value.parse::<i64>().ok())
        && not_before - CLOCK_SKEW_SECONDS > now
    {
        return Err(IdentityError::InvalidProvider(
            "the assertion is not valid yet".into(),
        ));
    }
    if let Some(until) =
        attr(conditions, "NotOnOrAfter").and_then(|value| value.parse::<i64>().ok())
        && until + CLOCK_SKEW_SECONDS < now
    {
        return Err(IdentityError::InvalidProvider(
            "the assertion has expired".into(),
        ));
    }

    if attr(conditions, "NotOnOrAfter").is_none() && attr(conditions, "NotBefore").is_none() {
        return Err(IdentityError::InvalidProvider(
            "the assertion has no validity window".into(),
        ));
    }
    Ok(())
}

/// Read every `AttributeStatement/Attribute` into a flat claim map.
///
/// A directory sends a multi-valued attribute either as one element with several
/// `AttributeValue` children **or** as several `Attribute`s of the same name. Both mean "a list",
/// and dropping the second form would quietly lose half a group membership.
fn read_attributes(element: &Element) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let Some(statement) = find_child(element, "AttributeStatement") else {
        return map;
    };

    for node in &statement.children {
        let XMLNode::Element(attribute) = node else {
            continue;
        };
        if local_name_of(&attribute.name) != "Attribute" {
            continue;
        }
        let Some(name) = attr(attribute, "Name").map(str::to_owned) else {
            continue;
        };
        let values: Vec<String> = attribute
            .children
            .iter()
            .filter_map(|child| match child {
                XMLNode::Element(element) if local_name_of(&element.name) == "AttributeValue" => {
                    element
                        .get_text()
                        .map(|text| text.trim().to_owned())
                        .filter(|text| !text.is_empty())
                }
                _ => None,
            })
            .collect();
        if values.is_empty() {
            continue;
        }

        let incoming: Vec<serde_json::Value> =
            values.into_iter().map(serde_json::Value::String).collect();
        // A repeated attribute promotes a single-valued entry to a list rather than being dropped.
        let merged = match map.get(&name) {
            Some(serde_json::Value::Array(existing)) => {
                let mut list = existing.clone();
                list.extend(incoming);
                list
            }
            Some(serde_json::Value::String(first)) => {
                let mut list = vec![serde_json::Value::String(first.clone())];
                list.extend(incoming);
                list
            }
            Some(_) => continue,
            None => incoming,
        };
        // A one-element list stays a plain string, so a single-valued attribute reads as the value
        // a person would expect and a list appears only when there really is more than one.
        map.insert(
            name,
            if merged.len() == 1 {
                merged.into_iter().next().unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Array(merged)
            },
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use rsa::RsaPrivateKey;
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;

    use super::*;

    /// A 2048-bit key and a PEM wrapping its public half as a `SubjectPublicKeyInfo`.
    fn test_key() -> (RsaPrivateKey, String) {
        let mut rng = rand::rngs::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("entropy");
        (private.clone(), der_certificate(&private))
    }

    /// Wrap a public key in a DER SubjectPublicKeyInfo, base64'd and PEM-armored.
    fn der_certificate(private: &RsaPrivateKey) -> String {
        let rsa_key = der_wrap(
            0x30,
            &[
                &der_integer(&private.n().to_bytes_be()),
                &der_integer(&private.e().to_bytes_be()),
            ],
        );
        // rsaEncryption OID.
        let algorithm = der_wrap(
            0x30,
            &[
                &[
                    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
                ],
                &[],
            ],
        );
        // A BIT STRING is `unused-bits` followed by the wrapped key.
        let bit_string = der_wrap(0x03, &[&[0x00], &rsa_key]);
        let spki = der_wrap(0x30, &[&algorithm, &bit_string]);
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            b64().encode(&spki)
        )
    }

    /// DER-encode a positive INTEGER, adding the sign byte when needed.
    fn der_integer(bytes: &[u8]) -> Vec<u8> {
        let mut body = bytes.to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        der_wrap(0x02, &[&body])
    }

    /// DER-encode a SEQUENCE (or any tag) around already-encoded elements.
    fn der_wrap(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        let mut out = vec![tag];
        if body.len() < 128 {
            out.push(body.len() as u8);
        } else {
            let mut encoded = Vec::new();
            let mut value = body.len();
            while value > 0 {
                encoded.insert(0, (value & 0xff) as u8);
                value >>= 8;
            }
            out.push(0x80 | encoded.len() as u8);
            out.extend_from_slice(&encoded);
        }
        out.extend_from_slice(&body);
        out
    }

    /// A signed assertion with a window that is valid now.
    fn assertion(
        private: &RsaPrivateKey,
        issuer: &str,
        audience: &str,
        subject: &str,
        attributes: &[(&str, &str)],
    ) -> String {
        assertion_windowed(private, issuer, audience, subject, attributes, -60, 600)
    }

    /// A signed assertion with an explicit window, in seconds from now.
    fn assertion_windowed(
        private: &RsaPrivateKey,
        issuer: &str,
        audience: &str,
        subject: &str,
        attributes: &[(&str, &str)],
        not_before_offset: i64,
        not_after_offset: i64,
    ) -> String {
        let attributes_xml: String = attributes
            .iter()
            .map(|(name, value)| {
                format!(
                    r#"<saml:Attribute Name="{name}"><saml:AttributeValue>{value}</saml:AttributeValue></saml:Attribute>"#
                )
            })
            .collect();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let body = format!(
            r#"<saml:Issuer>{issuer}</saml:Issuer><saml:Subject><saml:NameID>{subject}</saml:NameID></saml:Subject><saml:Conditions NotBefore="{}" NotOnOrAfter="{}"><saml:AudienceRestriction><saml:Audience>{audience}</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AttributeStatement>{attributes_xml}</saml:AttributeStatement>"#,
            now + not_before_offset,
            now + not_after_offset,
        );

        // The reference digest covers the assertion with its own signature removed — the enveloped
        // transform. Computing it here means the test's document is signed the way a provider
        // signs one, and the verifier re-derives the identical bytes.
        let envelope = format!(
            r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="id-1">{body}</saml:Assertion>"#
        );
        let digest = b64().encode(Sha256::digest(envelope.as_bytes()));
        let signed_info = format!(
            r##"<ds:SignedInfo><ds:SignatureMethod Algorithm="{algorithm}"/><ds:Reference URI="#id-1"><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/><ds:DigestValue>{digest}</ds:DigestValue></ds:Reference></ds:SignedInfo>"##,
            algorithm = ALLOWED_ALGORITHMS[0],
            digest = digest,
        );
        // `SLOT` is a word base64 can never spell, so the substitution lands on the value element
        // and nowhere else.
        let signature_xml = format!(
            r#"<ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">{signed_info}<ds:SignatureValue>SLOT</ds:SignatureValue></ds:Signature>"#,
            signed_info = signed_info,
        );
        let document = format!(
            r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="id-1">{signature_xml}{body}</saml:Assertion>"#
        );

        let signature = SigningKey::<Sha256>::new(private.clone())
            .try_sign(signed_info.as_bytes())
            .expect("a signature");
        let signed = document.replace("SLOT", &b64().encode(signature.to_bytes()));

        format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol">{}</samlp:Response>"#,
            signed
        )
    }

    fn config(certificate: &str) -> SamlConfig {
        SamlConfig {
            issuer: "https://idp.example/saml".into(),
            audience: "https://omnion.example".into(),
            certificate_pem: certificate.to_owned(),
            email_attribute: "email".into(),
            group_attribute: Some("groups".into()),
            display_name_attribute: Some("displayName".into()),
        }
    }

    #[test]
    fn a_configuration_probe_reads_back_as_an_identity() {
        // The probe is what the panel's `test` button runs, so it has to survive the *real* reader
        // — an attribute name that the configuration names must come back readable, or an
        // operator has no way to find a typo before the first sign-in.
        let config = SamlConfig {
            issuer: "https://idp.example/saml".into(),
            audience: "https://omnion.example".into(),
            certificate_pem: "unused".into(),
            email_attribute: "email".into(),
            group_attribute: Some("groups".into()),
            display_name_attribute: Some("displayName".into()),
        };
        let probe = probe_document(
            &config.issuer,
            &config.audience,
            &config.email_attribute,
            config.group_attribute.as_deref(),
            config.display_name_attribute.as_deref(),
        );
        let verified = verify_response_unverified(&probe, &config)
            .expect("a probe built from this configuration must read back");
        assert_eq!(verified.email, "probe@omnion.test");
        assert_eq!(verified.subject_id, "probe-subject");
        assert_eq!(verified.display_name.as_deref(), Some("Probe User"));
        assert_eq!(
            verified.groups,
            vec!["probe-group", "probe-group-two"],
            "a repeated attribute is a list, and the probe writes it twice to prove it"
        );
    }

    #[test]
    fn a_probe_survives_a_configuration_that_does_not_match() {
        // The point of the probe: a mismatched entity id or audience is refused *here*, at
        // configuration time, rather than at the first real assertion.
        let probe = probe_document(
            "https://idp.example/saml",
            "https://omnion.example",
            "email",
            None,
            None,
        );
        let wrong_audience = SamlConfig {
            issuer: "https://idp.example/saml".into(),
            audience: "https://other.example".into(),
            certificate_pem: "unused".into(),
            email_attribute: "email".into(),
            group_attribute: None,
            display_name_attribute: None,
        };
        let error = verify_response_unverified(&probe, &wrong_audience)
            .expect_err("a different audience is a configuration error");
        assert!(
            error.to_string().contains("not for this application"),
            "{error}"
        );
    }

    #[test]
    fn a_certificate_is_readable_only_when_it_is_a_key() {
        assert!(
            certificate_is_readable(&der_certificate(
                &rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("entropy")
            ))
            .is_ok()
        );
        for broken in [
            "",
            "not a certificate",
            "-----BEGIN CERTIFICATE-----\nnot base64!!\n-----END CERTIFICATE-----\n",
        ] {
            assert!(
                certificate_is_readable(broken).is_err(),
                "a certificate an operator can mis-paste must be reported: {broken:?}"
            );
        }
    }

    #[test]
    fn escaping_covers_what_changes_the_meaning_of_xml() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn a_signed_assertion_becomes_an_identity() {
        let (private, certificate) = test_key();
        let document = assertion(
            &private,
            "https://idp.example/saml",
            "https://omnion.example",
            "alice-subject",
            &[
                ("email", "Alice@Example.com"),
                ("displayName", "Alice Nguyen"),
                ("groups", "editors"),
                ("groups", "reviewers"),
            ],
        );

        let verified =
            verify_response(&document, &config(&certificate)).expect("a valid assertion");
        assert_eq!(verified.subject_id, "alice-subject");
        assert_eq!(verified.email, "alice@example.com");
        assert_eq!(verified.display_name.as_deref(), Some("Alice Nguyen"));
        assert_eq!(
            verified.groups,
            vec!["editors", "reviewers"],
            "a repeated attribute means a list, not a duplicate to drop"
        );
    }

    #[test]
    fn a_tampered_assertion_fails_the_signature() {
        let (private, certificate) = test_key();
        let document = assertion(
            &private,
            "https://idp.example/saml",
            "https://omnion.example",
            "alice-subject",
            &[("email", "alice@example.com")],
        );
        let tampered = document.replace("alice@example.com", "attacker@evil.example");
        assert!(
            verify_response(&tampered, &config(&certificate)).is_err(),
            "a changed claim must not verify against the original signature"
        );
    }

    #[test]
    fn a_foreign_issuer_or_audience_is_refused() {
        let (private, certificate) = test_key();
        let document = assertion(
            &private,
            "https://idp.example/saml",
            "https://omnion.example",
            "alice",
            &[("email", "alice@example.com")],
        );

        let mut wrong = config(&certificate);
        wrong.issuer = "https://evil.example/saml".into();
        assert!(verify_response(&document, &wrong).is_err());

        let mut wrong = config(&certificate);
        wrong.audience = "https://other.example".into();
        assert!(verify_response(&document, &wrong).is_err());
    }

    #[test]
    fn an_expired_assertion_is_refused_even_with_a_valid_signature() {
        let (private, certificate) = test_key();
        let document = assertion_windowed(
            &private,
            "https://idp.example/saml",
            "https://omnion.example",
            "alice",
            &[("email", "alice@example.com")],
            -10_000,
            -9_000,
        );
        assert!(
            verify_response(&document, &config(&certificate)).is_err(),
            "an assertion outside its window is refused even with a valid signature"
        );
    }

    #[test]
    fn an_unsigned_or_entity_bearing_response_is_refused() {
        let (_, certificate) = test_key();
        let unsigned = r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"><saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Version="2.0" ID="id-1"><saml:Issuer>https://idp.example/saml</saml:Issuer></saml:Assertion></samlp:Response>"#;
        assert!(verify_response(unsigned, &config(&certificate)).is_err());

        let bomb =
            r#"<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;">]><samlp:Response/>"#;
        assert!(verify_response(bomb, &config(&certificate)).is_err());
    }

    #[test]
    fn a_document_signed_by_another_key_is_refused() {
        let (private, _) = test_key();
        let (_, other_certificate) = test_key();
        let document = assertion(
            &private,
            "https://idp.example/saml",
            "https://omnion.example",
            "alice",
            &[("email", "alice@example.com")],
        );
        assert!(
            verify_response(&document, &config(&other_certificate)).is_err(),
            "a different certificate must not verify the signature"
        );
    }

    #[test]
    fn the_der_reader_finds_the_rsa_key_of_a_certificate() {
        let (private, certificate) = test_key();
        let parsed = certificate_key(&certificate).expect("a readable certificate");
        assert_eq!(parsed.n().to_bytes_be(), private.n().to_bytes_be());
        assert_eq!(parsed.e().to_bytes_be(), private.e().to_bytes_be());
    }

    #[test]
    fn the_der_reader_refuses_garbage() {
        assert!(spki_rsa_key(&[0x02, 0x01, 0x05]).is_none());
        assert!(certificate_key("not a certificate").is_err());
    }

    #[test]
    fn an_element_name_is_read_before_its_prefix_is_dropped() {
        assert_eq!(
            local_name_of(
                r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="1">"#
            ),
            "Assertion",
            "a colon inside an attribute value is not a namespace separator"
        );
        assert_eq!(local_name_of("<Assertion>"), "Assertion");
        assert_eq!(
            local_name_of(r#"<ds:SignatureMethod Algorithm="x"/>"#),
            "SignatureMethod"
        );
    }
}
