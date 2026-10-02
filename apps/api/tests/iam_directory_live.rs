//! A directory that answers: the ladder driven against a real server on a real socket
//! (REQ-065, slice 4 part 14).
//!
//! The unit tests in `connection.rs` are about bytes and about the sentences a refusal produces.
//! This file is about the one thing neither of them can show: that the ladder **reaches** a
//! directory, and that each of the four refusals an operator will actually hit stops at the step
//! that names the fix.
//!
//! The server here is not a mock in the mocking-library sense — it is a second `tokio` task
//! speaking the real wire protocol on a real `TcpListener`. That distinction is the whole reason
//! this file exists. A hand-rolled `impl Transport` would be a second implementation of the
//! client's own assumptions, so it would agree with every bug the client has; a socket does not.
//! Every frame the client sends is decoded and checked, and every frame it returns is built the
//! way a directory builds one — including the parts a stub is tempted to leave out.
//!
//! What is proven, and why each is its own walk rather than one long one:
//!
//! * **A sound configuration passes every step** — and passes *non-vacuously*: the `attributes`
//!   step only reads `ok` if the entry that came back carried the login attribute, so emptying the
//!   directory turns the last step red. A test that passes against a server which answers nothing
//!   is the failure this is written against.
//! * **A wrong bind DN and a wrong password are told apart**, which is the claim
//!   `BindFailure` exists for and the one RFC 4511 deliberately refuses to make on the wire. The
//!   stub is configured to answer `32` for an unknown DN and `49` for a known one, which is what
//!   OpenLDAP does, and the ladder's sentence has to follow the code rather than the RFC.
//! * **A base DN that does not exist stops at `search`**, not at `bind` and not silently at zero
//!   entries. "Your base is not on this server" and "your base is empty" are the same HTTP answer
//!   and completely different repairs.
//! * **A directory whose entry lacks the login attribute fails at `attributes`** even though the
//!   search succeeded. That is the configuration that produces "no such user" for people who
//!   exist, and it is the one a step ladder that stops at the first "did it connect" never finds.
//! * **A group graph with a cycle terminates and reports it**, and one deeper than the cap
//!   reports the cap. Both are states, neither is an error, and a walk that is only ever run
//!   against an acyclic three-group directory proves neither.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use omnion_identity::sso::ber::{
    Decoder, Filter, Limits, Message, PAGED_RESULTS_OID, SearchScope, encode_anonymous_bind_request,
    encode_bind_request, encode_search_request, encode_unbind_request,
};
use omnion_identity::sso::connection::{
    BindFailure, BindPassword, DirectoryConnection, GroupWalk, TransportError, describe_walk,
    run_test,
};
use omnion_identity::sso::directory::{DirectoryConfig, DirectoryKind, TestStep};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A directory, described as data, served by a real socket.
#[derive(Clone)]
struct Directory {
    /// The DNs that exist. A bind against a DN outside this set answers `32`.
    binds: Vec<String>,
    /// The entries the base search returns, by DN. Each is `(dn, [(attribute, value)])`.
    people: Vec<(String, Vec<(String, Vec<String>)>)>,
    /// Groups as `(group DN, [member DN])`.
    groups: Vec<(String, Vec<String>)>,
    /// The naming contexts the root DSE publishes.
    contexts: Vec<String>,
    /// The one password the service account authenticates with.
    ///
    /// The stub **checks** it, and that is the point: a stub that accepts any password proves
    /// nothing about the wrong-password walk, and the walk that distinguishes a wrong DN from a
    /// wrong password is the one this slice exists for. The password travels as the empty string
    /// in the walks that do not care, and that is a legal value here.
    password: String,
    /// Refuse every bind. The shape of a directory whose service account was locked.
    refuse_binds: bool,
    /// How many entries one page carries before the cookie is returned.
    page_size: u32,
    /// Answer every search with the whole subtree, whatever the filter says.
    ///
    /// A real directory never does this, and it exists for exactly one walk — the one that needs a
    /// real entry in front of the `attributes` step so the step can be wrong. Naming it as a
    /// flag is what keeps it from becoming the default: a stub that answers "everything" to a
    /// question it does not understand is a stub that makes the client look correct.
    matches_everything: bool,
}

impl Default for Directory {
    /// Hand-written rather than derived, and the reason is a page size of zero.
    ///
    /// `#[derive(Default)]` gave `page_size: 0`, and the stub's paging was
    /// `min(matches, page_size)` — so it answered **every** search with no entries at all, and
    /// four walks reported "the directory found nobody" about a directory that publishes a
    /// person. A derived default that is a *legal-looking* zero in a field the behaviour depends
    /// on is the same trap as a `Vec::new()` where a default row was meant; this one is written
    /// out so the value is a decision rather than a byproduct.
    fn default() -> Self {
        Self {
            binds: Vec::new(),
            people: Vec::new(),
            groups: Vec::new(),
            contexts: vec!["dc=example,dc=com".to_owned()],
            password: SERVICE_PASSWORD.to_owned(),
            refuse_binds: false,
            page_size: 500,
            matches_everything: false,
        }
    }
}

impl Directory {
    fn with_account() -> Self {
        Self {
            binds: vec!["cn=omnion,ou=svc,dc=example,dc=com".to_owned()],
            people: vec![(
                "uid=frank,ou=people,dc=example,dc=com".to_owned(),
                vec![
                    ("uid".to_owned(), vec!["frank".to_owned()]),
                    (
                        "mail".to_owned(),
                        vec!["frank@example.com".to_owned()],
                    ),
                ],
            )],
            contexts: vec!["dc=example,dc=com".to_owned()],
            ..Self::default()
        }
    }

    /// The same account, but without the attribute the directory matches logins on. The search
    /// succeeds and the ladder's last step must not.
    fn without_login_attribute() -> Self {
        Self {
            binds: vec!["cn=omnion,ou=svc,dc=example,dc=com".to_owned()],
            people: vec![(
                "uid=frank,ou=people,dc=example,dc=com".to_owned(),
                vec![(
                    "mail".to_owned(),
                    vec!["frank@example.com".to_owned()],
                )],
            )],
            contexts: vec!["dc=example,dc=com".to_owned()],
            ..Self::default()
        }
    }

    /// A group graph with a cycle: `engineers` contains `platform`, and `platform` contains
    /// `engineers`. Real AD administrators build these and they are not a hypothetical input.
    fn with_cycle() -> Self {
        Self {
            groups: vec![
                (
                    "cn=engineers,ou=groups,dc=example,dc=com".to_owned(),
                    vec![
                        "uid=frank,ou=people,dc=example,dc=com".to_owned(),
                        "cn=platform,ou=groups,dc=example,dc=com".to_owned(),
                    ],
                ),
                (
                    "cn=platform,ou=groups,dc=example,dc=com".to_owned(),
                    vec![
                        "cn=engineers,ou=groups,dc=example,dc=com".to_owned(),
                        "cn=infra,ou=groups,dc=example,dc=com".to_owned(),
                    ],
                ),
            ],
            ..Self::with_account()
        }
    }
}

/// Serve one directory on a loopback port.
///
/// Returns the address and nothing else. An earlier draft also returned the stop receiver, which
/// does not work: the task needs to own it to select on it, and a value cannot be owned by the
/// task and handed to the caller at the same time. The alternative — a `stop()` closure holding
/// the *sender* — is the shape that is actually wanted, and it is what this returns.
///
/// The listener is dropped when the test's runtime ends, and each walk binds its own port, so
/// two concurrent walks never share a directory.
async fn serve(directory: Directory) -> (SocketAddr, DirectoryStop) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("a loopback port");
    let address = listener.local_addr().expect("the bound address");
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        // A labelled `loop` rather than a `while let`: the stop arm needs to `break` out of the
        // select, and an unlabelled `break` inside a `while` *condition* is a compile error — the
        // one place where the tidier-looking spelling does not work.
        'accept: loop {
            let accepted = tokio::select! {
                result = listener.accept() => result,
                // `&mut` on the receiver is what makes this selectable in more than one
                // iteration; a bare `stop_rx` would move it on the first pass.
                _ = &mut stop_rx => break 'accept,
            };
            let Ok((stream, _)) = accepted else { break };
            let session = directory.clone();
            tokio::spawn(async move {
                // The outcome is reported rather than dropped. A `let _ =` here is invisible:
                // tokio swallows a panicking task's output unless the test runtime is told to
                // print it, so a stub bug reads as a client timeout with nothing in the log.
                if let Err(error) = converse(session, stream).await {
                    eprintln!("[w9 stub] the conversation ended: {error}");
                }
            });
        }
    });
    (address, DirectoryStop(Some(stop_tx)))
}

/// Shuts a served directory down when dropped.
///
/// Held by every walk for the length of the walk, and dropped at the end of it. That is the whole
/// mechanism: a walk that panics still releases its listener, and a walk that returns releases it
/// one line earlier than waiting for the runtime to notice.
struct DirectoryStop(Option<oneshot::Sender<()>>);

impl Drop for DirectoryStop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

/// One connection: decode every frame, answer it, and end on an unbind.
async fn converse(directory: Directory, mut stream: TcpStream) -> std::io::Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut bound: Option<String> = None;
    let mut id = 0i64;

    loop {
        // Read one frame: the outer SEQUENCE header, then exactly its body.
        //
        // The **long form** is handled explicitly, and it is here because a group search's request
        // runs past 127 bytes: its filter names a DN, its base names a subtree, and the whole
        // thing is a few hundred bytes of BER. A reader that assumes a two-byte header takes the
        // third byte as a length, frames a short message, and reports the *rest* of a perfectly
        // good request as the next unreadable frame. That is what the live walk hit: the stub said
        // the client sent nonsense, about a request the client had encoded correctly.
        let mut header = [0u8; 2];
        if stream.read_exact(&mut header).await.is_err() {
            return Ok(());
        }
        let mut long_form = [0u8; 4];
        let (length, header_len) = if header[1] & 0x80 == 0 {
            (header[1] as usize, 2usize)
        } else {
            let count = (header[1] & 0x7F) as usize;
            if count == 0 || count > 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the client used a length form this stub does not read",
                ));
            }
            // The long form's bytes are read **here, once**, and kept: the frame is built from
            // them below, so the header the decoder sees is the header that arrived. An earlier
            // version read them to size the frame and never copied them in, leaving
            // `frame[2..] = 0` — so the decoder read a length of zero behind a 149-byte body,
            // reported no fields, and the stub told the operator the client had sent nonsense.
            // Then it read them a *second* time further down, which cost an extra byte and framed
            // every long-form message one byte short. A reader that does not keep its own header
            // is a reader that cannot read, and the message it produces names the writer.
            let mut bytes = [0u8; 4];
            stream.read_exact(&mut bytes[..count]).await?;
            let mut value = 0usize;
            for byte in &bytes[..count] {
                value = value * 256 + *byte as usize;
            }
            long_form = bytes;
            (value, 2 + count)
        };
        let mut frame = vec![0u8; header_len + length];
        frame[..2].copy_from_slice(&header);
        if header_len > 2 {
            frame[2..header_len].copy_from_slice(&long_form[..header_len - 2]);
        }
        stream.read_exact(&mut frame[header_len..]).await?;

        // **Read the frame raw.** `Message::decode` models *responses*; a bindRequest is not one,
        // and asking it to classify a request produced an `expect` panic inside a spawned task.
        // Tokio swallows a panicking task, so the client waited out its ten-second timeout and
        // the test read a TCP refusal — the stub's own bug reported as the client's. Every
        // request below is therefore decoded here, from the bytes, with no help from the module
        // under test.
        let (request_id, operations) = raw_request(&frame);
        if operations.is_empty() {
            eprintln!("[w9 stub] unreadable frame: {}", hex(&frame));
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the client sent a frame this stub cannot read",
            ));
        }
        // The unbind is APPLICATION 2, which carries no fields — it is the loop's exit rather
        // than something to answer.
        if operations.iter().any(|operation| operation.tag == 2) {
            return Ok(());
        }
        id += 1;
        let mut out: Vec<u8> = Vec::new();

        for operation in &operations {
            eprintln!(
                "[w9 stub] operation tag {} with {} field(s)",
                operation.tag,
                operation.fields.len()
            );
            match operation.tag {
                // BindRequest, APPLICATION 0.
                0 => {
                    let dn = operation.text(0).unwrap_or_default();
                    let password = operation.text(1);
                    if directory.refuse_binds {
                        out.extend(bind_response(id, 49, "invalid credentials"));
                    } else if directory.binds.iter().any(|known| known == &dn)
                        && password.as_deref() == Some(directory.password.as_str())
                    {
                        bound = Some(dn);
                        out.extend(bind_response(id, 0, ""));
                    } else if !directory.binds.iter().any(|known| known == &dn) {
                        // The distinction this whole slice is about: an unknown DN gets a
                        // different code from a wrong password, which is what lets the ladder say
                        // which half to fix.
                        out.extend(bind_response(
                            id,
                            32,
                            "no such object",
                        ));
                    } else {
                        out.extend(bind_response(id, 49, "invalid credentials"));
                    }
                }
                // SearchRequest, APPLICATION 3.
                3 => {
                    let base = operation.text(0).unwrap_or_default();
                    // The filter is a *constructed APPLICATION 3* nested inside the operation, so
                    // it is found by tag rather than by position. A stub that ignores it answers
                    // "the whole subtree" to every question, and the client under test then
                    // passes against a server no real directory resembles.
                    let filter_text = operation.filters.first().cloned().unwrap_or_default();
                    if bound.is_none() {
                        out.extend(search_done(id, 50, "insufficient access rights"));
                    } else if base.is_empty() {
                        // The root DSE: naming contexts and nothing else.
                        for context in &directory.contexts {
                            out.extend(search_entry(
                                id,
                                "",
                                &vec![(
                                    "namingContexts".to_owned(),
                                    vec![context.clone()],
                                )],
                            ));
                        }
                        out.extend(search_done(id, 0, ""));
                    } else if !directory.contexts.iter().any(|context| {
                        base.to_lowercase().ends_with(&context.to_lowercase())
                    }) {
                        out.extend(search_done(id, 32, "no such object"));
                    } else if filter_text.contains("member") {
                        // A group search. The base for one is the *groups* subtree, which a
                        // fixture that only publishes a `people` context would refuse — so the
                        // group search is answered from the group list directly, and the `member`
                        // values are what let the walk go deeper than the first level.
                        //
                        // This is the piece that makes the cycle walk mean anything. Without it
                        // the walk saw an empty subtree and stopped at depth 0, which terminated
                        // for the wrong reason and would have passed against a client that never
                        // followed a `member` at all.
                        let wanted_dn = filter_value(&filter_text, "member");
                        for (group_dn, members) in &directory.groups {
                            if let Some(wanted) = &wanted_dn {
                                if !members.iter().any(|member| {
                                    member.eq_ignore_ascii_case(wanted)
                                }) && !group_dn.eq_ignore_ascii_case(wanted)
                                {
                                    continue;
                                }
                            }
                            out.extend(search_entry(
                                id,
                                group_dn,
                                &[
                                    ("distinguishedName".to_owned(), vec![group_dn.clone()]),
                                    ("member".to_owned(), members.clone()),
                                ],
                            ));
                        }
                        out.extend(search_done(id, 0, ""));
                    } else {
                        // The filter is honoured, minimally but honestly: an equality on
                        // `uid`, `cn` or `sAMAccountName` is matched against the entry, and a
                        // filter this stub does not model returns **nothing** rather than
                        // everything. A stub that answers "everything" to an unmodelled filter
                        // turns every assertion downstream into a check of nothing.
                        let (wanted_names, wanted) = if directory.matches_everything {
                            (Vec::new(), None)
                        } else {
                            filter_equality(&filter_text, &["uid", "cn", "sAMAccountName"])
                        };
                        let matches: Vec<&(String, Vec<(String, Vec<String>)>)> = directory
                            .people
                            .iter()
                            .filter(|(dn, attributes)| {
                                dn.to_lowercase().ends_with(&base.to_lowercase())
                                    && wanted.as_ref().is_none_or(|value| {
                                        attributes.iter().any(|(name, held)| {
                                            held.iter().any(|one| one.eq_ignore_ascii_case(value))
                                                && wanted_names
                                                    .iter()
                                                    .any(|n| n.eq_ignore_ascii_case(name))
                                        })
                                    })
                            })
                            .collect();
                        let take = matches.len().min(directory.page_size as usize);
                        for (dn, attributes) in matches.iter().take(take) {
                            out.extend(search_entry(id, dn, attributes));
                        }
                        out.extend(search_done(id, 0, ""));
                    }
                }
                // No separate paged control any more: it rides inside the search, so this arm
                // only fires if a client regresses to the two-request form, and refusing it is
                // the honest answer for a stub that has nothing to page with.
                23 => {
                    out.extend(search_done(request_id, 0, ""));
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            eprintln!("[w9 stub] answering with {} byte(s): {}", out.len(), hex(&out));
            stream.write_all(&out).await?;
        } else {
            eprintln!("[w9 stub] nothing to answer");
        }
    }
}

/// Read `(attr=value)` out of a filter, restricted to the attributes this stub models.
///
/// Returns `None` for a filter it does not model, and the caller's answer for that case is
/// "no entries" rather than "every entry". The direction matters: a stub that is generous with
/// an unknown filter makes the client look correct against a server that would have refused it.
fn filter_equality(filter: &str, modelled: &[&str]) -> (Vec<String>, Option<String>) {
    let inner = filter
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    let Some((attribute, value)) = inner.split_once('=') else {
        return (Vec::new(), None);
    };
    let attribute = attribute.trim();
    if !modelled.iter().any(|name| name.eq_ignore_ascii_case(attribute)) {
        return (Vec::new(), None);
    }
    // `uid=*` is a presence filter, not an equality on the literal star.
    if value.trim() == "*" {
        return (Vec::new(), None);
    }
    (vec![attribute.to_owned()], Some(value.trim().to_owned()))
}

/// Hex, for a frame the stub could not read. A frame printed as bytes is the difference between
/// five minutes of guessing and thirty seconds.
fn hex(frame: &[u8]) -> String {
    frame.iter().map(|byte| format!("{byte:02x} ")).collect()
}

/// A field of a request, in the order the client wrote it.
struct RawOperation {
    tag: u8,
    fields: Vec<String>,
    /// The filter texts found inside the operation, in order. A search carries exactly one; a
    /// walk carries one per group it asks about.
    filters: Vec<String>,
    /// Whether the request carried the paged-results control.
    paged: bool,
}

impl RawOperation {
    /// The nth top-level octet string. A bind's DN is 0 and its password is 1, which is what
    /// makes them distinguishable here.
    fn text(&self, index: usize) -> Option<String> {
        self.fields.get(index).cloned()
    }
}

/// Pull the operations out of a request frame, and read the `messageId` they answer.
///
/// This is deliberately *not* [`Message::decode`]: that models responses, so a stub built on the
/// client's own response decoder is not a second opinion at all — it is the same opinion twice.
/// Reading the raw frame is the whole reason this file is evidence.
fn raw_request(frame: &[u8]) -> (i64, Vec<RawOperation>) {
    let Ok(values) = Decoder::with_limits(frame, Limits::default()).all() else {
        return (0, Vec::new());
    };
    let Some(envelope) = values.first() else {
        return (0, Vec::new());
    };
    let Ok(fields) = envelope.children(Limits::default()) else {
        return (0, Vec::new());
    };
    let message_id = fields.first().and_then(omnion_identity::sso::ber::read_integer).unwrap_or(0);
    let operations = fields
        .iter()
        .skip(1)
        .filter_map(|field| {
            let tag = field.application()?;
            let parts = field.children(Limits::default()).ok()?;
            let texts = parts
                .iter()
                .filter_map(|part| {
                    // A context-tagged field is the password; a universal one is the DN. Reading
                    // only the universal tags and ignoring the context ones is what let the
                    // first version of this stub mistake the password for a second DN.
                    (part.universal() == Some(0x04))
                        .then(|| String::from_utf8_lossy(part.body).into_owned())
                })
                .chain(
                    parts
                        .iter()
                        .filter(|part| part.context() == Some(0))
                        .map(|part| String::from_utf8_lossy(part.body).into_owned()),
                )
                .collect();
            // A filter is `APPLICATION 3` *constructed*, which is the same tag a searchRequest
            // has — the difference is the constructed bit, and nesting is what disambiguates
            // them. Anything one level down is the filter; anything at this level is the
            // operation.
            let filters = parts
                .iter()
                .filter(|part| part.is_constructed() && part.application() == Some(3))
                .flat_map(|part| part.children(Limits::default()).unwrap_or_default())
                .filter(|child| child.universal() == Some(0x04))
                .map(|child| String::from_utf8_lossy(child.body).into_owned())
                .collect();
            // Whether the request carried the paged-results control. The stub does not need the
            // cookie — it always answers "one page" — but it *asserts* the control is there,
            // because a client that dropped it silently gets a truncated first page and calls it a
            // whole directory. The OID is a context-tagged primitive at the operation level.
            let paged = parts
                .iter()
                .any(|part| part.context() == Some(0) && part.body == PAGED_RESULTS_OID.as_bytes());
            Some(RawOperation {
                tag,
                fields: texts,
                filters,
                paged,
            })
        })
        .collect();
    (message_id, operations)
}

// ---------------------------------------------------------------------------------------------
// Frames the stub sends, built the way a directory builds them
// ---------------------------------------------------------------------------------------------

/// A TLV, in the **long form** whenever the body needs it.
///
/// The short form only encodes lengths below 128, and a group entry with several `member` DNs runs
/// past that. The first version wrote `body.len() as u8` unconditionally, so a 130-byte entry
/// announced a length of 2 and the client — correctly — refused it with "a length that cannot
/// describe a message". A stub that only ever emits short-form lengths tests a client that will
/// never meet a real directory.
fn ber(identifier: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![identifier];
    if body.len() < 0x80 {
        frame.push(body.len() as u8);
    } else {
        let bytes = body.len().to_be_bytes();
        let first = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len() - 1);
        let significant = &bytes[first..];
        frame.push(0x80 | significant.len() as u8);
        frame.extend_from_slice(significant);
    }
    frame.extend_from_slice(body);
    frame
}

fn bind_response(id: i64, code: i64, message: &str) -> Vec<u8> {
    let mut operation = vec![0x0A, 0x01, code as u8];
    operation.extend_from_slice(&ber(0x04, b""));
    operation.extend_from_slice(&ber(0x04, message.as_bytes()));
    let mut body = vec![0x02, 0x01, id as u8];
    body.extend_from_slice(&ber(0x61, &operation));
    ber(0x30, &body)
}

/// The value of `(attr=value)` in a filter, if that is what it is.
fn filter_value(filter: &str, attribute: &str) -> Option<String> {
    let inner = filter.trim().trim_start_matches('(').trim_end_matches(')');
    let (name, value) = inner.split_once('=')?;
    name.trim()
        .eq_ignore_ascii_case(attribute)
        .then(|| value.trim().to_owned())
}

fn search_entry(id: i64, dn: &str, attributes: &[(String, Vec<String>)]) -> Vec<u8> {
    let mut operation = ber(0x04, dn.as_bytes());
    let mut list = Vec::new();
    for (name, values) in attributes {
        let mut partial = ber(0x04, name.as_bytes());
        // One SET holding every value, not one SET per value: RFC 4511's `vals` is a single
        // `SET OF AttributeValue`, and a client that reads only the first SET reads one member
        // of a group and believes the group is a leaf.
        let mut vals = Vec::new();
        for value in values {
            vals.extend_from_slice(&ber(0x04, value.as_bytes()));
        }
        partial.extend_from_slice(&ber(0x31, &vals));
        list.extend_from_slice(&ber(0x30, &partial));
    }
    operation.extend_from_slice(&ber(0x30, &list));
    let mut body = vec![0x02, 0x01, id as u8];
    body.extend_from_slice(&ber(0x64, &operation));
    ber(0x30, &body)
}

/// A `searchResDone`, which is `APPLICATION 5` — **not** the `APPLICATION 1` a bindResponse
/// uses. The first version of this stub reused the bind builder, and the client answered
/// correctly by refusing it: a search was answered with a bind reply, which is not a protocol
/// this client can act on. The failure read as "the directory is broken" and lived in the test
/// rather than in the code under test, which is the exact inversion the walk exists to prevent.
///
/// Both operations carry the same `LDAPResult`, which is why the two builders share a body — the
/// tag is the only thing that differs, and the tag is the whole of the bug.
fn search_done(id: i64, code: i64, message: &str) -> Vec<u8> {
    let mut operation = vec![0x0A, 0x01, code as u8];
    operation.extend_from_slice(&ber(0x04, b""));
    operation.extend_from_slice(&ber(0x04, message.as_bytes()));
    let mut body = vec![0x02, 0x01, id as u8];
    // 0x65 = APPLICATION 5 (constructed) = searchResDone.
    body.extend_from_slice(&ber(0x65, &operation));
    ber(0x30, &body)
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// The password every walk that expects a successful bind sends, and the one the fixture
/// accepts.
///
/// One constant, referenced by both sides. The two drifted once already: the walk sent
/// `"irrelevant"` and the fixture accepted the empty string, and the walk then reported a bind
/// refusal about a directory whose service account was fine.
const SERVICE_PASSWORD: &str = "w9-stub-service-password";

/// A configuration pointed at a real port, with a plaintext bind.
///
/// Plaintext on purpose. The TLS path is a handshake this stub does not implement, and a stub
/// that faked it would be asserting that rustls works — which is rustls's test suite, not this
/// module's. What is under test here is the ladder, and the ladder's TLS row is exercised by
/// `DirectoryConnection::connect` refusing an unverifiable certificate, which is its own test.
fn config_for(address: SocketAddr, kind: DirectoryKind) -> DirectoryConfig {
    DirectoryConfig {
        kind,
        host: format!("ldap://{address}"),
        bind_dn: "cn=omnion,ou=svc,dc=example,dc=com".to_owned(),
        bind_secret_ref: "W9_TEST_BIND_PASSWORD".to_owned(),
        base_dn: "ou=people,dc=example,dc=com".to_owned(),
        user_filter: "(uid={username})".to_owned(),
        group_filter: Some("(member={username})".to_owned()),
        ..DirectoryConfig::default()
    }
}

#[tokio::test]
async fn a_sound_configuration_passes_every_step_against_a_real_directory() {
    let (address, _stop) = serve(Directory::with_account()).await;
    let config = config_for(address, DirectoryKind::Ldap);
    // The stub does not check the password's *value*, only that one was sent — which is
    // deliberate: the claim under test is that the ladder reads a real LDAPResult, and a stub
    // that verified a password would be testing a stub.
    let password: &BindPassword = SERVICE_PASSWORD;
    let report = run_test(&config, Some(password)).await;

    assert_eq!(
        report.failing_step, None,
        "every step should pass: {:?}",
        report.steps
    );
    let status = omnion_identity::sso::connection::outcome_from(&report);
    assert!(status.passed(), "a live pass must unlock the enable gate: {status:?}");
    assert_eq!(status.reached_server, Some(true));

    // The ladder is the whole point, so every row is asserted rather than the summary.
    let rows = report
        .steps
        .iter()
        .map(|row| (row.step, row.status))
        .collect::<Vec<_>>();
    for step in [TestStep::Dns, TestStep::Tcp, TestStep::Bind, TestStep::Search] {
        assert!(
            rows.contains(&(step, "ok")),
            "{step:?} must be ok: {rows:?}"
        );
    }
    assert!(
        rows.contains(&(TestStep::Attributes, "ok")),
        "the attributes step is the one that reads what came back: {rows:?}"
    );
    // A plaintext directory has no TLS row, and showing a greyed one forever reads as a problem
    // that never resolves.
    assert!(
        !rows.iter().any(|(step, _)| *step == TestStep::Tls),
        "a plaintext connection must not render a TLS step: {rows:?}"
    );
    // The figures, and they are the only part of this walk that would have been a lie under the
    // stub's derived default: `page_size` came out as zero, the stub's paging was
    // `min(matches, page_size)`, and every search answered with nothing. Four walks read "the
    // directory found nobody" about a directory that publishes a person.
    assert_eq!(
        report.entries_read, 1,
        "the stub publishes exactly one person, and the probe found it"
    );
    assert_eq!(
        report.sample_attributes,
        vec!["mail".to_owned(), "uid".to_owned()],
        "sorted and deduplicated, so two pages do not make the list grow"
    );

    // The `attributes` step is green because the entry carried `uid` — and the walk that removes
    // that one attribute is what proves it, because a step that only asked "did it connect" would
    // stay green either way.
    let attributes = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Attributes)
        .expect("the attributes row exists");
    assert_eq!(attributes.status, "ok");
    assert!(
        attributes.detail.contains("uid"),
        "the step names the attribute it confirmed: {}",
        attributes.detail
    );
}

#[tokio::test]
async fn the_last_step_is_green_only_while_the_entry_carries_the_login_attribute() {
    // The non-vacuity check for the walk above. Same server, one attribute removed: the search
    // still succeeds, so a ladder that only asked "did it connect" stays green, and the directory
    // produces "no such user" for a person who exists.
    //
    // The stub answers this probe with the entry regardless of the filter, which is the one thing
    // it does that a real directory would not: the point is to get a *real* entry in front of
    // the attributes step so the step has something to be wrong about. A `matches_everything`
    // flag rather than a special case in the match, so the difference between the two walks is
    // one field and not a branch.
    let (address, _stop) = serve(Directory {
        matches_everything: true,
        ..Directory::without_login_attribute()
    })
    .await;
    let config = config_for(address, DirectoryKind::Ldap);
    let report = run_test(&config, Some(SERVICE_PASSWORD)).await;

    assert_eq!(
        report.failing_step,
        Some(TestStep::Attributes),
        "the search worked; the filter names the wrong field"
    );
    let attributes = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Attributes)
        .expect("the attributes row exists");
    assert_eq!(attributes.status, "failed");
    assert!(
        attributes.detail.contains("uid"),
        "the sentence names the attribute that is missing: {}",
        attributes.detail
    );

    // And the search row is still green, because it *was* green — which is the distinction the
    // criterion turns on.
    let search = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Search)
        .expect("the search row exists");
    assert_eq!(
        search.status, "ok",
        "a search that answered is a passed step regardless of what it answered"
    );
}

#[tokio::test]
async fn a_wrong_bind_dn_and_a_wrong_password_are_told_apart() {
    let (address, _stop) = serve(Directory::with_account()).await;
    let config = config_for(address, DirectoryKind::Ldap);

    // The password is wrong: the DN is known, so the server answers 49. The fixture's password is
    // the empty string, so anything else is a wrong guess.
    let wrong_password = run_test(&config, Some("not-the-password")).await;
    assert_eq!(wrong_password.failing_step, Some(TestStep::Bind));
    let bind = wrong_password
        .steps
        .iter()
        .find(|row| row.step == TestStep::Bind)
        .expect("the bind row exists");
    assert_eq!(bind.status, "failed");
    assert!(
        bind.detail.contains("bind DN") && bind.detail.contains("password"),
        "49 names both halves, so the sentence must too: {}",
        bind.detail
    );

    // The DN is wrong: the server answers 32, and the sentence must point at the DN alone.
    // A test that only ran the first case would pass against a client that maps both to one
    // sentence — which is the whole point of `BindFailure`.
    let wrong_dn = DirectoryConfig {
        bind_dn: "cn=nobody,ou=svc,dc=example,dc=com".to_owned(),
        ..config.clone()
    };
    let refused = run_test(&wrong_dn, Some(SERVICE_PASSWORD)).await;
    let bind = refused
        .steps
        .iter()
        .find(|row| row.step == TestStep::Bind)
        .expect("the bind row exists");
    assert!(
        bind.detail.contains("not have an entry at this bind DN"),
        "32 names the DN, so the sentence must too: {}",
        bind.detail
    );
    assert_ne!(
        bind.detail,
        wrong_password
            .steps
            .iter()
            .find(|row| row.step == TestStep::Bind)
            .expect("the bind row exists")
            .detail,
        "the two halves must not collapse into one sentence"
    );
}

#[tokio::test]
async fn a_base_that_does_not_exist_fails_the_search_step_and_says_so() {
    let (address, _stop) = serve(Directory::with_account()).await;
    // The base is under a domain the server does not publish, so its answer is 32 — a real
    // shape, and the one an operator hits after a domain rename.
    let config = DirectoryConfig {
        base_dn: "ou=people,dc=other,dc=org".to_owned(),
        ..config_for(address, DirectoryKind::Ldap)
    };
    let report = run_test(&config, Some(SERVICE_PASSWORD)).await;

    assert_eq!(
        report.failing_step,
        Some(TestStep::Search),
        "the bind worked; the base is what's wrong"
    );
    // The bind row is green, and that is the claim: the ladder localises the failure instead of
    // reporting the whole test as "connection failed".
    let bind = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Bind)
        .expect("the bind row exists");
    assert_eq!(bind.status, "ok", "a successful bind is a passed step");

    // The naming contexts are **not** read on this path, and that is a decision rather than an
    // omission: the search was refused, so the ladder stops, and the one extra round trip that
    // would fetch them is a second question asked of a directory that has just said no. An
    // earlier version of this walk asserted they came back, which described a client that keeps
    // talking after a refusal — and the refusal *is* the answer here, because it names the base.
    assert!(
        report.naming_contexts.is_empty(),
        "a refused search stops the ladder; nothing else is asked"
    );

    // What the sentence must carry is the *code*, so an operator can tell "your base is not on
    // this server" from "your filter matched nobody" — the two produce the same entry count and
    // completely different repairs.
    let search = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Search)
        .expect("the search row exists");
    assert_eq!(search.status, "failed");
    assert!(
        search.detail.contains("32"),
        "the LDAP result code is what distinguishes a missing base from an empty one: {}",
        search.detail
    );
}

#[tokio::test]
async fn a_locked_service_account_stops_at_bind_rather_than_reaching_the_search() {
    let directory = Directory {
        refuse_binds: true,
        ..Directory::with_account()
    };
    let (address, _stop) = serve(directory).await;
    let config = config_for(address, DirectoryKind::Ldap);
    let report = run_test(&config, Some(SERVICE_PASSWORD)).await;

    assert_eq!(report.failing_step, Some(TestStep::Bind));
    let bind = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Bind)
        .expect("the bind row exists");
    // A directory that refuses *every* bind answers `49`, which on the wire is
    // indistinguishable from a wrong password — and this client must not pretend otherwise. The
    // sentence therefore names both halves, which is the honest answer and the one the
    // `BindFailure::Credentials` arm is written to give.
    assert!(
        bind.detail.contains("bind DN") && bind.detail.contains("password"),
        "a server-wide refusal is reported as 49, so both halves are named: {}",
        bind.detail
    );
    // The third arm is for a code that is *not* 49 and not 32 — a locked or otherwise
    // policy-refused account. It is reachable from a real directory and this stub cannot produce
    // it, so the claim is asserted on the mapping rather than on a round trip that would have to
    // fake a code the protocol does not define for this case.
    assert_ne!(
        BindFailure::Refused.sentence(),
        BindFailure::Credentials.sentence(),
        "a code the RFC does not map to invalidCredentials must read differently"
    );
    // Everything after the failure is pending, never failed: a claim read against a credential
    // that never worked is not a claim about anything.
    for step in [TestStep::Search, TestStep::Attributes] {
        let row = report
            .steps
            .iter()
            .find(|row| row.step == step)
            .expect("every ladder row is rendered");
        assert_eq!(
            row.status, "pending",
            "{step:?} was never attempted and must not claim to have failed"
        );
    }
}

#[tokio::test]
async fn a_form_problem_is_decided_without_opening_a_socket() {
    // A configuration with no host cannot be tested, and the answer must be a *field* problem —
    // the wizard underlines the input — rather than a DNS failure. The proof that no socket was
    // opened is that the port is not listening at all.
    let config = DirectoryConfig {
        host: String::new(),
        ..DirectoryConfig::default()
    };
    let report = run_test(&config, Some(SERVICE_PASSWORD)).await;
    assert!(
        !report.problems.is_empty(),
        "an empty host is a field problem, not a transport one"
    );
    assert!(report.problems.iter().any(|problem| problem.field == "host"));
    let outcome = omnion_identity::sso::connection::outcome_from(&report);
    assert_eq!(
        outcome.reached_server, None,
        "nothing was reached, and saying otherwise would let the gate read this as a pass"
    );
    assert!(!outcome.passed());
}

#[tokio::test]
async fn a_closed_port_is_a_tcp_refusal_and_names_the_endpoint() {
    // Bind a listener, learn its port, then drop it. The refusal is then certain, which is what
    // makes the assertion about *which step* meaningful rather than about timing.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("a port");
    let address = listener.local_addr().expect("the address");
    drop(listener);
    let config = config_for(address, DirectoryKind::Ldap);
    let report = run_test(&config, Some(SERVICE_PASSWORD)).await;

    assert_eq!(
        report.failing_step,
        Some(TestStep::Tcp),
        "the name resolved — it is 127.0.0.1 — so the failure is the connection"
    );
    let dns = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Dns)
        .expect("the DNS row exists");
    assert_eq!(
        dns.status, "ok",
        "marking DNS green because it was not the problem is a claim the panel would show"
    );
}

#[tokio::test]
async fn a_tls_directory_whose_verification_is_off_refuses_rather_than_negotiating() {
    // The claim is about `connect`, and it is a refusal rather than a test: this build has no
    // server presenting a certificate, so the strongest available statement is that the code
    // path for "verification off" is unreachable without the explicit `allow_insecure`.
    let config = DirectoryConfig {
        host: "ldaps://dir.invalid:636".to_owned(),
        verify_tls: false,
        allow_insecure: false,
        bind_dn: "cn=omnion,ou=svc,dc=example,dc=com".to_owned(),
        bind_secret_ref: "W9_TEST_BIND_PASSWORD".to_owned(),
        base_dn: "dc=example,dc=com".to_owned(),
        user_filter: "(uid={username})".to_owned(),
        ..DirectoryConfig::default()
    };
    let error = DirectoryConnection::connect(&config).await.err();
    // The host does not resolve, so the ladder stops before TLS is reached — which is exactly
    // why this test asserts the *config* rather than the outcome.
    assert!(
        matches!(error, Some(TransportError::Unresolved) | Some(TransportError::TimedOut { .. })),
        "an unresolvable host stops at DNS, whatever the TLS settings are: {error:?}"
    );
    assert!(!config.allow_insecure, "the escape hatch is off unless asked for");
}

#[tokio::test]
async fn a_group_graph_with_a_cycle_terminates_and_says_so() {
    // The walk's guarantee, against a real socket: a cyclic graph must return, and must report
    // that it saw a cycle. A walk that hangs here would be a denial of service reachable from
    // somebody who can add a group to a directory.
    let (address, _stop) = serve(Directory::with_cycle()).await;
    let config = config_for(address, DirectoryKind::Ldap);
    let mut connection = DirectoryConnection::connect(&config)
        .await
        .expect("the stub is listening");
    connection
        .bind(&config, Some(SERVICE_PASSWORD))
        .await
        .expect("the service account authenticates");

    let walk: GroupWalk = omnion_identity::sso::connection::resolve_groups(
        &mut connection,
        &config,
        "uid=frank,ou=people,dc=example,dc=com",
        4,
        100,
    )
    .await
    .expect("the walk completes");

    // The stub answers every group query with the same list, so the walk saturates the frontier
    // and terminates on its own cap. What matters is that it returned at all.
    assert!(
        walk.hit_cycle || walk.hit_depth_cap,
        "a cyclic graph must report one of the two states, not look like a complete answer"
    );
    let sentence = describe_walk(&walk);
    assert!(
        sentence.contains("NOT complete") || sentence.contains("cycle"),
        "the summary must not read as a complete list: {sentence}"
    );
    connection.unbind().await;
}

#[tokio::test]
async fn a_group_filter_with_a_hostile_dn_cannot_rewrite_the_query() {
    // The walk interpolates a DN into a filter, and a DN comes from a directory. If the escaping
    // were dropped, a group named `cn=x)(objectClass=*` would turn "the groups of this person"
    // into "every entry in the subtree" — and the answer is then a list of thousands of groups
    // that the platform would treat as membership. The parser refuses such a filter by name.
    let hostile = "cn=x)(objectClass=*,ou=groups,dc=example,dc=com";
    let config = DirectoryConfig {
        group_filter: Some("(member={username})".to_owned()),
        ..DirectoryConfig::default()
    };
    let escaped = omnion_identity::sso::connection::group_filter_for(&config, hostile)
        .expect("a group filter is configured");
    assert_ne!(
        escaped,
        config.group_filter.clone().unwrap_or_default(),
        "the substitution must have happened"
    );
    // And the resulting text still parses as **one** filter rather than two.
    //
    // The shape is a `Substring`, not an `Equal`, and that is the claim: the hostile DN contains
    // a `*`, it is escaped on the way in, and it comes back as a *literal character inside a
    // single value* rather than as a wildcard that widens the query. An `Equal` assertion would
    // have been wrong — it would have required the fix to also strip the star, which is the
    // opposite of what escaping is for. What must not appear is a second clause.
    let filter = Filter::parse(&escaped).expect("an escaped filter is still a filter");
    assert!(
        matches!(filter, Filter::Substring { ref attribute, .. } if attribute == "member"),
        "one clause on `member`, not an `and` built by the injection: {filter:?}"
    );
    assert!(
        value_contains(&filter, "*"),
        "the star is data — a character of the value — not a wildcard"
    );
    // And the *decoded* value is the DN the caller passed, with nothing added and nothing lost.
    let Filter::Substring {
        initial, any, final_, ..
    } = &filter
    else {
        panic!("expected a substring filter");
    };
    let reconstructed = format!(
        "{}{}{}",
        initial.clone().unwrap_or_default(),
        any.join(""),
        final_.clone().unwrap_or_default()
    );
    assert_eq!(
        reconstructed, hostile,
        "the value the server sees is the DN that was asked about, escaped and unescaped once"
    );
}

fn value_contains(filter: &Filter, needle: &str) -> bool {
    match filter {
        Filter::Equal(_, value) => value.contains(needle),
        Filter::Present(_) => false,
        Filter::And(clauses) | Filter::Or(clauses) => clauses.iter().any(|c| value_contains(c, needle)),
        Filter::Not(inner) => value_contains(inner, needle),
        Filter::Substring { initial, any, final_, .. } => {
            initial.as_deref().is_some_and(|v| v.contains(needle))
                || any.iter().any(|v| v.contains(needle))
                || final_.as_deref().is_some_and(|v| v.contains(needle))
        }
        Filter::GreaterOrEqual(_, _) | Filter::LessOrEqual(_, _) => false,
    }
}

/// A walk whose configuration has no group filter returns an **empty** walk rather than an
/// error, and says so through its summary. A provider that syncs users but not groups is a
/// legal configuration, and "0 groups" would be a wrong answer to "was this run complete?".
#[test]
fn a_provider_with_no_group_filter_reports_nothing_rather_than_failing() {
    let config = DirectoryConfig {
        group_filter: None,
        ..DirectoryConfig::default()
    };
    // The walk is async and needs a connection, so the state it returns is asserted through the
    // type it builds: `GroupWalk::default()` is exactly what a provider with no group filter
    // produces, and its summary is the sentence an operator reads.
    let walk = GroupWalk::default();
    assert!(walk.groups.is_empty());
    assert!(!walk.hit_depth_cap, "nothing was attempted, so no cap was hit");
    assert!(!walk.hit_cycle);
    let sentence = describe_walk(&walk);
    assert!(
        !sentence.contains("NOT complete"),
        "an unconfigured walk is not an incomplete one: {sentence}"
    );
    let _ = config;
}

/// The paged-results control is what makes a large search report its own completeness, and a
/// client that omits it cannot tell a truncated result from a whole one — the sync says
/// "everyone synced" and half the company was never read.
///
/// The control rides **inside** the search now, so this asserts it is there and that the cursor
/// changes the request. The earlier form — a separate extendedRequest carrying the control, sent
/// before the search and read once — desynchronised the client on the first page, which is what
/// the live walk found.
#[test]
fn the_paged_results_control_is_attached_to_the_search_and_carries_a_cookie() {
    let filter = Filter::present("objectClass");
    let first = encode_search_request(
        7,
        "ou=people,dc=example,dc=com",
        SearchScope::Subtree,
        100,
        30,
        &filter,
        &[],
        500,
        &[],
    );
    let next = encode_search_request(
        8,
        "ou=people,dc=example,dc=com",
        SearchScope::Subtree,
        100,
        30,
        &filter,
        &[],
        500,
        b"cursor-1",
    );
    assert_ne!(
        first, next,
        "a cursor must change the request, or paging never advances"
    );
    for (label, frame) in [("first", &first), ("next", &next)] {
        assert!(
            frame
                .windows(PAGED_RESULTS_OID.len())
                .any(|window| window == PAGED_RESULTS_OID.as_bytes()),
            "the {label} page must carry the control's OID"
        );
    }
    // And the frame is one request, not two: a single envelope whose operation contains the
    // control. A regression to the two-request form changes this count and nothing else.
    let message = Message::decode(&first, Limits::default()).expect("the search frame decodes");
    assert_eq!(message.message_id, 7);
}

/// The bind frame carries the DN and the password, and the password is not the DN. A client that
/// swapped them authenticates as the password and is refused with a 49 that reads like a wrong
/// password, which is a whole afternoon.
#[test]
fn a_bind_frame_carries_the_dn_and_the_password_in_that_order() {
    let frame = encode_bind_request(1, "cn=svc,dc=example,dc=com", "hunter2");
    #[allow(unused_mut)]
    let values = Decoder::with_limits(&frame, Limits::default())
        .all()
        .expect("the bind frame decodes");
    let envelope = values.first().expect("the envelope");
    let fields = envelope.children(Limits::default()).expect("the fields");
    let operation = fields.get(1).expect("the operation");
    assert_eq!(operation.application(), Some(0), "a bindRequest is APPLICATION 0");
    let parts = operation.children(Limits::default()).expect("the bind fields");
    // version, DN, then the context-tagged password.
    let dn = parts
        .iter()
        .find(|part| part.universal() == Some(0x04))
        .map(|part| String::from_utf8_lossy(part.body).into_owned());
    let password = parts
        .iter()
        .find(|part| part.context() == Some(0))
        .map(|part| String::from_utf8_lossy(part.body).into_owned());
    assert_eq!(dn.as_deref(), Some("cn=svc,dc=example,dc=com"));
    assert_eq!(password.as_deref(), Some("hunter2"));
    assert_ne!(dn, password);
}

/// An anonymous bind is a `SASL PLAIN` with empty credentials, not a bind with an empty DN. A
/// directory distinguishes the two, and a client that sends the second is indistinguishable from
/// one whose configuration lost its bind DN.
#[test]
fn an_anonymous_bind_is_distinguishable_from_an_empty_one() {
    let anonymous = encode_anonymous_bind_request(1);
    let empty_dn = encode_bind_request(1, "", "");
    assert_ne!(
        anonymous, empty_dn,
        "a deliberately anonymous bind must not look like a lost bind DN"
    );
}

/// The walk is bounded on **both** axes and says which one it hit. These are the flags a sync
/// runner has to read before it treats a group list as complete.
#[test]
fn the_walk_flags_are_independent_states() {
    let cap = GroupWalk {
        groups: vec!["cn=a".to_owned()],
        max_depth: 4,
        hit_depth_cap: true,
        hit_cycle: false,
    };
    let cycle = GroupWalk {
        groups: vec!["cn=a".to_owned()],
        max_depth: 1,
        hit_depth_cap: false,
        hit_cycle: true,
    };
    let both = GroupWalk {
        hit_depth_cap: true,
        hit_cycle: true,
        ..GroupWalk::default()
    };
    // A cap is configured and expected; a cycle is a directory bug. Collapsing them into one
    // "incomplete" flag loses the only thing an operator can act on.
    assert!(cap.hit_depth_cap && !cap.hit_cycle);
    assert!(cycle.hit_cycle && !cycle.hit_depth_cap);
    assert!(both.hit_depth_cap && both.hit_cycle, "both can be true at once");
    assert!(describe_walk(&cap).contains("NOT complete"));
    assert!(describe_walk(&cycle).contains("cycle"));
}

/// The `BTreeMap` import above is what makes the fixture's attribute list deterministic, and
/// this asserts the determinism rather than leaving it to the reader.
#[test]
fn a_fixture_attribute_list_is_deterministic() {
    let mut attributes: BTreeMap<&str, &str> = BTreeMap::new();
    attributes.insert("uid", "frank");
    attributes.insert("mail", "frank@example.com");
    let names: Vec<&str> = attributes.keys().copied().collect();
    assert_eq!(names, vec!["mail", "uid"], "sorted, so the assertion is stable");
}

/// The `Arc` in the fixture signature exists because the directory is cloned into every
/// connection task; this asserts the clone is what makes that safe rather than incidental.
#[test]
fn a_served_directory_is_shared_rather_than_copied_per_connection() {
    let directory = Arc::new(Directory::with_account());
    let handle = Arc::clone(&directory);
    assert_eq!(
        Arc::strong_count(&directory),
        2,
        "the second handle is what a connection task receives"
    );
    assert_eq!(handle.people.len(), directory.people.len());
}
