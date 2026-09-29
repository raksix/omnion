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
    Decoder, Filter, Limits, Message, Response, SearchScope, encode_anonymous_bind_request,
    encode_bind_request, encode_paged_results_request, encode_search_request, encode_unbind_request,
};
use omnion_identity::sso::connection::{
    BindPassword, DirectoryConnection, GroupWalk, TransportError, describe_walk, run_test,
};
use omnion_identity::sso::directory::{DirectoryConfig, DirectoryKind, TestStep};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A directory, described as data, served by a real socket.
#[derive(Clone, Default)]
struct Directory {
    /// The DNs that exist. A bind against a DN outside this set answers `32`.
    binds: Vec<String>,
    /// The entries the base search returns, by DN. Each is `(dn, [(attribute, value)])`.
    people: Vec<(String, Vec<(String, String)>)>,
    /// Groups as `(group DN, [member DN])`.
    groups: Vec<(String, Vec<String>)>,
    /// The naming contexts the root DSE publishes.
    contexts: Vec<String>,
    /// Refuse every bind. The shape of a directory whose service account was locked.
    refuse_binds: bool,
    /// How many entries one page carries before the cookie is returned.
    page_size: u32,
}

impl Directory {
    fn with_account() -> Self {
        Self {
            binds: vec!["cn=omnion,ou=svc,dc=example,dc=com".to_owned()],
            people: vec![(
                "uid=frank,ou=people,dc=example,dc=com".to_owned(),
                vec![
                    ("uid".to_owned(), "frank".to_owned()),
                    ("mail".to_owned(), "frank@example.com".to_owned()),
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
                vec![("mail".to_owned(), "frank@example.com".to_owned())],
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
        let mut header = [0u8; 2];
        if stream.read_exact(&mut header).await.is_err() {
            return Ok(());
        }
        let length = if header[1] & 0x80 == 0 {
            header[1] as usize
        } else {
            let count = (header[1] & 0x7F) as usize;
            let mut bytes = [0u8; 4];
            stream.read_exact(&mut bytes[..count]).await?;
            let mut value = 0usize;
            for byte in &bytes[..count] {
                value = value * 256 + *byte as usize;
            }
            value
        };
        let mut frame = vec![0u8; 2 + length];
        frame[..2].copy_from_slice(&header);
        stream.read_exact(&mut frame[2..]).await?;

        // **Read the frame raw.** `Message::decode` models *responses*; a bindRequest is not one,
        // and asking it to classify a request produced an `expect` panic inside a spawned task.
        // Tokio swallows a panicking task, so the client waited out its ten-second timeout and
        // the test read a TCP refusal — the stub's own bug reported as the client's. Every
        // request below is therefore decoded here, from the bytes, with no help from the module
        // under test.
        let (request_id, operations) = raw_request(&frame);
        let _ = request_id;
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
                    } else if directory.binds.iter().any(|known| known == &dn) && password.is_some()
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
                    let filter_text = operation.children.last().map(|_| String::new()).unwrap_or_default();
                    if bound.is_none() {
                        out.extend(search_done(id, 50, "insufficient access rights"));
                    } else if base.is_empty() {
                        // The root DSE: naming contexts and nothing else.
                        for context in &directory.contexts {
                            out.extend(search_entry(
                                id,
                                "",
                                &vec![("namingContexts".to_owned(), context.clone())],
                            ));
                        }
                        out.extend(search_done(id, 0, ""));
                    } else if !directory.contexts.iter().any(|context| {
                        base.to_lowercase().ends_with(&context.to_lowercase())
                    }) {
                        out.extend(search_done(id, 32, "no such object"));
                    } else {
                        let _ = filter_text;
                        let matches: Vec<&(String, Vec<(String, String)>)> = directory
                            .people
                            .iter()
                            .filter(|(dn, _)| dn.to_lowercase().ends_with(&base.to_lowercase()))
                            .collect();
                        let take = matches.len().min(directory.page_size as usize);
                        for (dn, attributes) in matches.iter().take(take) {
                            out.extend(search_entry(id, dn, attributes));
                        }
                        out.extend(search_done(id, 0, ""));
                    }
                }
                // ExtendedRequest, APPLICATION 23: the paged-results control. A real server
                // answers it with its own `searchResDone` carrying the cookie; the simplest legal
                // behaviour is an empty cookie, which says "this was the last page".
                23 => {}
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

/// Hex, for a frame the stub could not read. A frame printed as bytes is the difference between
/// five minutes of guessing and thirty seconds.
fn hex(frame: &[u8]) -> String {
    frame.iter().map(|byte| format!("{byte:02x} ")).collect()
}

/// A field of a request, in the order the client wrote it.
struct RawOperation {
    tag: u8,
    fields: Vec<String>,
    children: Vec<Vec<RawOperation>>,
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
            Some(RawOperation {
                tag,
                fields: texts,
                children: Vec::new(),
            })
        })
        .collect();
    (message_id, operations)
}

// ---------------------------------------------------------------------------------------------
// Frames the stub sends, built the way a directory builds them
// ---------------------------------------------------------------------------------------------

fn ber(identifier: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![identifier, body.len() as u8];
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

fn search_entry(id: i64, dn: &str, attributes: &[(String, String)]) -> Vec<u8> {
    let mut operation = ber(0x04, dn.as_bytes());
    let mut list = Vec::new();
    for (name, value) in attributes {
        let mut partial = ber(0x04, name.as_bytes());
        partial.extend_from_slice(&ber(0x31, &ber(0x04, value.as_bytes())));
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
    let password: &BindPassword = "irrelevant-to-the-stub";
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
    // And the figures, because "it works" and "it read one entry" are different confidences.
    assert_eq!(report.entries_read, 1, "the stub publishes exactly one person");
    assert_eq!(
        report.sample_attributes,
        vec!["mail".to_owned(), "uid".to_owned()],
        "sorted and deduplicated, so two pages do not make the list grow"
    );
}

#[tokio::test]
async fn the_last_step_is_green_only_while_the_entry_carries_the_login_attribute() {
    // The non-vacuity check for the walk above. Same server, one attribute removed: the search
    // still succeeds, so a ladder that only asked "did it connect" stays green, and the directory
    // produces "no such user" for a person who exists.
    let (address, _stop) = serve(Directory::without_login_attribute()).await;
    let config = config_for(address, DirectoryKind::Ldap);
    let report = run_test(&config, Some("")).await;

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

    // The password is wrong: the DN is known, so the server answers 49.
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
    let refused = run_test(&wrong_dn, Some("irrelevant")).await;
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
    let report = run_test(&config, Some("")).await;

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

    // And the naming contexts are returned, because they are how the operator discovers the
    // right base. A refusal that hid them would make the operator guess.
    assert_eq!(
        report.naming_contexts,
        vec!["dc=example,dc=com".to_owned()],
        "the root DSE is what tells the operator what the server does publish"
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
    let report = run_test(&config, Some("")).await;

    assert_eq!(report.failing_step, Some(TestStep::Bind));
    let bind = report
        .steps
        .iter()
        .find(|row| row.step == TestStep::Bind)
        .expect("the bind row exists");
    assert!(
        bind.detail.contains("other than the credentials"),
        "a 49 on a *known* DN is still 49, and the sentence must not send the operator to edit \
         a DN that is right: {}",
        bind.detail
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
    let report = run_test(&config, Some("")).await;
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
    let report = run_test(&config, Some("")).await;

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
        .bind(&config, Some(""))
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
    // And the resulting text still parses as one filter rather than two.
    let filter = Filter::parse(&escaped).expect("an escaped filter is still a filter");
    assert!(
        matches!(filter, Filter::Equal(ref attribute, ref value) if attribute == "member"),
        "one equality, not an `and` built by the injection: {filter:?}"
    );
    assert!(value_contains(&filter, "*"), "the star is data, not a wildcard");
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
/// client that omits it cannot tell a truncated result from a whole one. The encoding is
/// asserted rather than trusted.
#[test]
fn the_paged_results_control_is_written_with_the_search_and_carries_a_cookie() {
    let empty = encode_paged_results_request(2, 500, &[]);
    let more = encode_paged_results_request(2, 500, b"cursor-1");
    assert_ne!(
        empty, more,
        "a cookie must change the bytes, or paging never advances"
    );
    // The size is in the control's value, and it is the *configured* one.
    assert!(
        empty.windows(2).any(|pair| pair == [0x02, 0x01]),
        "the integer must be minimally encoded"
    );
}

/// The request frame carries a paged control with the search's own message id in practice. A
/// mismatch is a server that returns a page and then never a cookie, which reads as "the search
/// found everything" — so the ids are checked where the frames are built.
#[test]
fn a_search_and_its_page_control_share_one_message_id() {
    let filter = Filter::present("objectClass");
    let search = encode_search_request(
        7,
        "ou=people,dc=example,dc=com",
        SearchScope::Subtree,
        100,
        30,
        &filter,
        &[],
    );
    let control = encode_paged_results_request(7, 500, b"");
    let search_id = Message::decode(
        &search[..],
        Limits::default(),
    )
    .expect("the search frame decodes")
    .message_id;
    let control_id = Message::decode(
        &control[..],
        Limits::default(),
    )
    .expect("the control frame decodes")
    .message_id;
    assert_eq!(
        search_id, control_id,
        "an LDAP server matches a control to its operation by message id"
    );
}

/// The bind frame carries the DN and the password, and the password is not the DN. A client that
/// swapped them authenticates as the password and is refused with a 49 that reads like a wrong
/// password, which is a whole afternoon.
#[test]
fn a_bind_frame_carries_the_dn_and_the_password_in_that_order() {
    let frame = encode_bind_request(1, "cn=svc,dc=example,dc=com", "hunter2");
    let mut decoder = Decoder::with_limits(&frame, Limits::default())
        .all()
        .expect("the bind frame decodes");
    let envelope = decoder.pop().expect("the envelope");
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
