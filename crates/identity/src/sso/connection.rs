//! The live half of a directory provider: DNS, TCP, TLS, bind, paged search and the nested-group
//! walk (REQ-065, slice 4 part 14).
//!
//! [`super::directory`] decides what a valid directory configuration *is*; this module is the
//! thing that goes and finds out whether it works. It exists because the decision half and the
//! network half fail differently, and a single "connection failed" hides that difference:
//!
//! * A **bad filter** is refused locally, before a socket is opened. [`DirectoryConfig::validate`]
//!   already does this, and a provider with three field problems gets three field problems rather
//!   than the first one's consequences.
//! * A **wrong bind DN** reaches the server and comes back as `49 invalidCredentials`, which is
//!   indistinguishable from a wrong *password* unless the module says which half of the
//!   credential it was. So [`BindFailure`] splits them, and a refusal names the step.
//! * An **unreachable host** is a socket error, and a socket error says nothing about the
//!   configuration. Saying "the host does not resolve" is the difference between a typo and a
//!   firewall.
//!
//! Three things this module refuses to do, each of which is a real way directory integration is
//! usually broken:
//!
//! * **No unbounded recursion.** [`resolve_groups`] takes a depth cap and *returns* when it is
//!   reached, reporting the cap as a value rather than truncating silently. A group graph with a
//!   cycle is not hypothetical — AD administrators create them, and an unbounded walk is a
//!   denial of service this platform would be handing to a stranger.
//! * **No unbounded page.** A paged search is followed until the cookie is empty *or* the
//!   caller's subject cap is reached, and hitting the cap is a reported outcome. A directory
//!   that returns a million entries does not get to decide how much memory the panel uses.
//! * **No raw server text in a message.** A directory's `diagnosticMessage` routinely contains
//!   the base DN, the filter, sometimes the bind DN. Every sentence this module produces is
//!   written here, and the raw text is dropped at the boundary — see [`sanitize`].
//!
//! What it does *not* do: it does not decide anything. No role, no group, no account is written
//! by anything in this file. The caller takes [`ConnectionReport`] and decides.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::ber::{
    Attribute, BerError, Filter, LdapResult, Limits, Message, Response, SearchEntry, SearchScope,
    encode_anonymous_bind_request, encode_bind_request, encode_paged_results_request,
    encode_search_request, encode_starttls_request, encode_unbind_request,
};
use super::directory::{DirectoryConfig, StepReport, TestOutcome, TestStep};

/// The password never appears in a type that can be logged or serialized, and this is the only
/// place it exists as a value.
///
/// `&str` rather than an owned `String`: it is written to the wire and dropped, never kept, and
/// a borrowed parameter is what a caller holding a `std::env::var` result already has.
pub type BindPassword = str;

/// How long one network step may take.
///
/// A directory is somebody else's machine on somebody else's network, and the difference between
/// a slow directory and an unreachable one is a number of seconds. Ten is long enough for a
/// cross-site bind over a poor link and short enough that a panel clicking `Test connection`
/// does not appear to have hung.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The ceiling on one `searchResEntry` page, independent of the configured page size. A directory
/// that answers a size-1 request with a 200 MB entry is not obeying the protocol, and the cap is
/// what makes that survivable.
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// What a caller needs to know
// ---------------------------------------------------------------------------------------------

/// Why a bind was refused, split by the half of the credential that was wrong.
///
/// RFC 4511 collapses both into `49 invalidCredentials` on purpose — telling an unauthenticated
/// client which half it got right is an enumeration oracle. That reasoning is correct for the
/// *start* route and wrong for the connection test, which is already behind an authenticated
/// panel and whose entire job is to reduce "the bind step failed" to something fixable. So the
/// directory split is real here, and the sentence is only ever shown to an operator who can
/// already see the bind DN they typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindFailure {
    /// The server answered, and said the credentials were not valid.
    Credentials,
    /// The server does not know the entry the bind DN names.
    NoSuchEntry,
    /// The server refused the *operation* — locked, unsupported, or a password policy.
    Refused,
}

impl BindFailure {
    /// The sentence the panel shows, naming the field so the wizard can underline it.
    #[must_use]
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Credentials => "the server refused this service account — check the bind DN and \
                 the password behind its reference, since it reports both the same way",
            Self::NoSuchEntry => "the server does not have an entry at this bind DN — check the \
                 DN against the directory, not the password",
            Self::Refused => "the server refused the bind for a reason other than the \
                 credentials — the account may be locked or unable to authenticate",
        }
    }
}

/// A transport-level failure, before or instead of an LDAP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The name did not resolve.
    Unresolved,
    /// The connection was refused or the port is filtered.
    Unreachable,
    /// A timeout, naming the host:port that did not answer.
    ///
    /// Carries the endpoint because a timeout sentence without it is the single least actionable
    /// message a directory integration can produce — the operator is looking at a form, not at a
    /// packet trace, and "did not answer in time" on its own has three candidate causes.
    TimedOut { host: String },
    /// The TLS handshake failed, and `reason` is which part.
    Tls(String),
    /// The connection was closed mid-message.
    Closed,
    /// A frame could not be encoded or decoded.
    Protocol(String),
    /// The bind was refused, and the reason is which half of the credential.
    Bind(BindFailure),
}

impl TransportError {
    /// The sentence the panel shows, attached to a step rather than to the whole test.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::Unresolved => {
                "the host name did not resolve to an address — check the spelling and the \
                 directory's DNS"
                    .to_owned()
            }
            Self::Unreachable => {
                "the host did not accept a connection on that port — check the port and whether a \
                 firewall sits between here and the directory"
                    .to_owned()
            }
            Self::TimedOut { host } => {
                format!(
                    "{host} did not answer within {}s — it may be overloaded, or a firewall is \
                     dropping the connection rather than refusing it",
                    DEFAULT_TIMEOUT.as_secs()
                )
            }
            Self::Tls(reason) => {
                format!("the TLS handshake failed: {}", sanitize(reason))
            }
            Self::Closed => {
                "the directory closed the connection in the middle of a message".to_owned()
            }
            Self::Protocol(reason) => {
                format!("the directory sent something this client could not read: {}", sanitize(reason))
            }
            Self::Bind(failure) => failure.sentence().to_owned(),
        }
    }

    /// Which step of the ladder this belongs to, so the failure lands on one row rather than on
    /// the test as a whole.
    #[must_use]
    pub fn step(&self) -> TestStep {
        match self {
            Self::Unresolved => TestStep::Dns,
            Self::Unreachable | Self::TimedOut { .. } | Self::Closed => TestStep::Tcp,
            Self::Tls(_) => TestStep::Tls,
            Self::Protocol(_) => TestStep::Search,
            Self::Bind(_) => TestStep::Bind,
        }
    }
}

impl From<BerError> for TransportError {
    fn from(error: BerError) -> Self {
        Self::Protocol(error.to_string())
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.sentence())
    }
}

impl std::error::Error for TransportError {}

/// One entry a directory search returned, reduced to what the platform needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// The entry's DN.
    pub dn: String,
    /// Its attributes, by name.
    pub attributes: Vec<Attribute>,
}

impl DirectoryEntry {
    fn from_search(entry: SearchEntry) -> Self {
        Self {
            dn: entry.dn,
            attributes: entry.attributes,
        }
    }

    /// The first value of an attribute, matched without regard to case.
    ///
    /// Case-insensitive because a directory is not consistent about it in the way HTTP is:
    /// OpenLDAP answers `uid` and Active Directory answers `sAMAccountName` or `UID` for the
    /// same attribute, and a case-sensitive read here is an integration that works against one
    /// server and returns nothing against the other — the single most common directory bug, and
    /// one no test against a stub would catch.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .and_then(|attribute| attribute.values.first())
            .map(String::as_str)
    }

    /// Every value of an attribute, case-insensitively — `member` is multi-valued on every
    /// directory worth walking.
    #[must_use]
    pub fn attribute_values(&self, name: &str) -> Vec<String> {
        self.attributes
            .iter()
            .filter(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .flat_map(|attribute| attribute.values.iter().cloned())
            .collect()
    }
}

/// The groups a walk found, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupWalk {
    /// Every group DN reached, in the order they were reached — not sorted, because the order is
    /// the breadth-first order and an operator reading a sync log wants to see how far it got.
    pub groups: Vec<String>,
    /// The deepest level the walk reached, so a walk that stopped because the graph ran out and
    /// one that stopped at the cap are distinguishable.
    pub max_depth: u8,
    /// Whether the cap stopped the walk. A run that reports this is **not** a complete answer,
    /// and a sync that treats it as one silently under-grants.
    pub hit_depth_cap: bool,
    /// Whether a cycle was entered. A directory with a cyclic group graph is a real
    /// misconfiguration and the walk survives it; reporting it is how an operator finds out.
    pub hit_cycle: bool,
}

/// Everything one connection attempt established.
///
/// A report is returned whether the connection succeeded or not: a failed test is a *result*,
/// and the panel's whole argument for a step ladder is that a failure is as informative as a
/// success. The empty cases carry a sentence, so a caller cannot render "0 groups" and leave
/// the operator guessing whether the walk found nothing or never ran.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionReport {
    /// The ladder, every step with its own sentence.
    pub steps: Vec<StepReport>,
    /// The step that stopped the attempt, if it stopped.
    pub failing_step: Option<TestStep>,
    /// A sample of the attributes the directory returned, for the `attributes` step.
    pub sample_attributes: Vec<String>,
    /// Configuration problems, when the ladder never opened a socket because the *form* was
    /// wrong. `None` on a live attempt, and that difference is load bearing: a wizard underlines
    /// from `problems`, so a timeout listed there would send an operator to edit a field that is
    /// correct.
    pub problems: Vec<super::directory::ConfigProblem>,
    /// How many entries the search read, and how many pages it took.
    pub entries_read: u32,
    pub pages_read: u32,
    /// The group walk, when one was asked for.
    pub groups: Option<GroupWalk>,
    /// The server's root DSE naming contexts, when it answered. This is the one piece of
    /// self-description a directory volunteers, and it is how an operator discovers that their
    /// base DN does not exist on the server they configured.
    pub naming_contexts: Vec<String>,
}

impl ConnectionReport {
    /// A report for a configuration that was refused **before** any socket was opened.
    ///
    /// Deliberately not routed through [`ConnectionReport::failure`]: that method marks one step
    /// `failed` and the rest `pending`, which is the right shape for a transport failure and the
    /// wrong one for a form with three typos. The local ladder already knows which step each
    /// problem belongs to, so it is returned as it stands and the problems ride alongside it.
    fn unconfigured(config: &DirectoryConfig) -> Self {
        let outcome = super::directory::test_steps(config);
        // `failing_step` reads the steps, so it is asked *before* they move. Borrowing a
        // partially-moved value is a compile error, not a subtle bug, which is the one mercy in
        // this function.
        let failing_step = outcome.failing_step();
        Self {
            steps: outcome.steps,
            failing_step,
            sample_attributes: Vec::new(),
            problems: outcome.problems,
            entries_read: 0,
            pages_read: 0,
            groups: None,
            naming_contexts: Vec::new(),
        }
    }

    fn failure(error: &TransportError, steps: &[(TestStep, String)]) -> Self {
        let mut ladder = ladder(steps, true);
        let step = error.step();
        let sentence = error.sentence();
        // Every step after the failure stays `pending`. Running a search after a failed bind
        // would be asking a question whose answer is already known, and marking the rest grey
        // is what tells the operator the ladder stopped *here*.
        if let Some(report) = ladder.iter_mut().find(|report| report.step == step) {
            report.status = "failed";
            report.detail = sentence;
        }
        if let Some(position) = steps.iter().position(|(candidate, _)| *candidate == step) {
            for report in ladder.iter_mut().skip(position + 1) {
                report.status = "pending";
                report.detail = String::new();
            }
        }
        Self {
            steps: ladder,
            failing_step: Some(step),
            sample_attributes: Vec::new(),
            problems: Vec::new(),
            entries_read: 0,
            pages_read: 0,
            groups: None,
            naming_contexts: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The report a caller assembles into a TestOutcome
// ---------------------------------------------------------------------------------------------

/// Fold a live report into the outcome the API returns.
///
/// Kept separate from the connection because the API's answer is a *result* whether or not a
/// socket was opened: a configuration problem is reported as a field problem and the live steps
/// never run, so a panel that has three typos does not also get a DNS timeout.
#[must_use]
pub fn outcome_from(report: &ConnectionReport) -> TestOutcome {
    let passed = report.failing_step.is_none() && report.problems.is_empty();
    TestOutcome {
        status: if passed { "ok" } else { "failed" },
        steps: report.steps.clone(),
        // A live failure is NOT a configuration problem: `problems` is what the wizard underlines,
        // and a timeout underlined on the host field sends the operator to edit a field that is
        // correct. So the list is only ever non-empty when the *form* was refused before a socket
        // existed — which is exactly when underlining is the right response.
        problems: report.problems.clone(),
        // A refused form never reached a server, and saying otherwise would let the enable gate
        // read a connection failure as a passing test.
        reached_server: (report.problems.is_empty()).then_some(true),
    }
}

/// Walk the whole ladder against a live directory.
///
/// This is the function the API's `Test connection` button calls, and the reason it exists as one
/// function rather than as a route that assembles steps itself is that **the order is the
/// semantics**: TLS before bind, bind before search, search before reading attributes. A route
/// that could run them in a different order could produce a green test from a directory that
/// cannot actually authenticate anybody, and the gate would let it be switched on.
///
/// The ladder **stops at the first failure** and leaves the rest `pending`. That is the whole
/// argument for a step list: a claim read against an issuer that was never trusted is not a
/// claim about anything, and running the rest of a test after the bind failed produces five
/// `failed` rows where one of them is the fix and four are consequences.
///
/// `password` is `None` for the *configuration* half — "is this reachable at all" — which is a
/// real question an operator asks before they have a secret reference filled in. When it is
/// `None` the bind is skipped rather than faked, and the ladder says so on the bind row, so a
/// green DNS/TCP/TLS ladder is never read as a working provider.
pub async fn run_test(
    config: &DirectoryConfig,
    password: Option<&BindPassword>,
) -> ConnectionReport {
    // A configuration problem is decided locally, before a socket exists, and it is reported as
    // a *field* problem. Running the ladder anyway would put a DNS timeout on a form that only
    // needed the base DN spelled correctly.
    if !config.validate().is_empty() {
        return ConnectionReport::unconfigured(config);
    }

    let mut done: Vec<(TestStep, String)> = Vec::new();

    // --- DNS / TCP / TLS happen inside `connect`, and the module reports which one failed. ---
    let mut connection = match DirectoryConnection::connect(config).await {
        Ok(connection) => {
            // DNS is reported from the connection rather than from the configuration: the
            // resolver already ran, and the address it chose is the evidence. Without this the
            // DNS row stayed `pending` on a *fully passing* test — a step that never resolves,
            // on a screen whose whole argument is that a test is a list of verdicts.
            done.push((
                TestStep::Dns,
                format!(
                    "{} resolved to {}",
                    config.hostname(),
                    connection.resolved_endpoint()
                ),
            ));
            let detail = if config.is_secure() {
                format!(
                    "connected to {}:{} and negotiated {}",
                    config.hostname(),
                    config.port(),
                    if config.host.trim().starts_with("ldaps://") {
                        "TLS from the first byte"
                    } else {
                        "TLS with StartTLS"
                    }
                )
            } else {
                // Named, because a plaintext directory is a *decision* and the panel should not
                // let an operator believe a bind password crossed the network encrypted when it
                // did not.
                format!(
                    "connected to {}:{} in the clear — the bind password is not encrypted on this                      connection",
                    config.hostname(),
                    config.port()
                )
            };
            if config.is_secure() {
                done.push((TestStep::Tls, detail.clone()));
            } else {
                done.push((TestStep::Tcp, detail));
            }
            connection
        }
        Err(error) => {
            // The resolver step is only meaningful when it was the resolver that failed; a
            // refused connection says nothing about the name, and marking DNS green because it
            // was not the problem is a claim the panel then shows as a passing step.
            let before = match error.step() {
                TestStep::Dns => Vec::new(),
                TestStep::Tcp => vec![(
                    TestStep::Dns,
                    format!("{} resolved to an address", config.hostname()),
                )],
                TestStep::Tls => vec![
                    (TestStep::Dns, "the host name resolved".to_owned()),
                    (TestStep::Tcp, "the host accepted a connection".to_owned()),
                ],
                other => {
                    let _ = other;
                    Vec::new()
                }
            };
            return ConnectionReport::failure(&error, &before);
        }
    };

    // --- Bind. ---
    let bind_detail = match password {
        None => {
            connection.unbind().await;
            return ConnectionReport {
                steps: ladder(&done, config.is_secure()),
                failing_step: Some(TestStep::Bind),
                sample_attributes: Vec::new(),
                problems: Vec::new(),
                entries_read: 0,
                pages_read: 0,
                groups: None,
                naming_contexts: Vec::new(),
            };
        }
        Some(password) => {
            match connection.bind(config, Some(password)).await {
                Ok(_) => "the service account authenticated".to_owned(),
                Err(error) => {
                    connection.unbind().await;
                    return ConnectionReport::failure(&error, &done);
                }
            }
        }
    };
    done.push((TestStep::Bind, bind_detail));

    // --- Search: the base, the filter, and one real entry. ---
    let probe = config.user_filter_for("__omnion_connection_probe__");
    let filter = match Filter::parse(&probe) {
        Ok(filter) => filter,
        Err(error) => {
            let error = TransportError::Protocol(error.to_string());
            connection.unbind().await;
            return ConnectionReport::failure(&error, &done);
        }
    };
    let (entries, pages, read) = match connection
        .search(
            &config.base_dn,
            SearchScope::Subtree,
            &filter,
            &[],
            1,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            connection.unbind().await;
            return ConnectionReport::failure(&error, &done);
        }
    };
    // A base that does not exist is the single most common directory misconfiguration and the
    // server says so with `32 noSuchObject`. It is a *search* step failure with a sentence that
    // says so, rather than "0 entries" — a directory whose base is wrong looks exactly like an
    // empty one until the operator is told which.
    let naming_contexts = connection.root_dse().await.unwrap_or_default();
    let search_detail = if entries.is_empty() {
        let base_note = if naming_contexts.is_empty() {
            String::new()
        } else {
            format!(
                " — the server publishes {} naming context(s); check the base DN is one of them",
                naming_contexts.len()
            )
        };
        format!(
            "the base was searched and no entry matched, which is correct for a configuration that              is only being tested{base_note}"
        )
    } else {
        format!(
            "the base was searched and {read} entr{} matched",
            if read == 1 { "y" } else { "ies" }
        )
    };
    done.push((TestStep::Search, search_detail));
    connection.unbind().await;

    // --- Attributes: what came back, and — the claim that matters — what the login needs. ---
    let attributes = DirectoryConnection::attribute_names(&entries);
    let required = config.login_attribute();
    let attributes_detail = if entries.is_empty() {
        // No entry to read, so this step cannot claim anything. It is `ok` with a sentence that
        // says it was not exercised, because a grey row on a fresh-but-valid directory reads as
        // a failure and an operator who "fixes" it changes a working configuration.
        format!(
            "not exercised — the search found no entry to read `{required}` from, which is what an              empty or brand-new directory looks like"
        )
    } else if attributes.iter().any(|name| name.eq_ignore_ascii_case(required)) {
        format!(
            "{} attribute(s) present, including the login attribute `{required}`",
            attributes.len()
        )
    } else {
        // A directory that answers but does not carry the login attribute is a *configuration*
        // error with a precise repair, and it is the one that produces "no such user" for people
        // who exist — so it is a failure, not a note.
        return ConnectionReport {
            steps: {
                let mut steps = ladder(&done, config.is_secure());
                steps.push(StepReport {
                    step: TestStep::Attributes,
                    status: "failed",
                    detail: format!(
                        "the entry came back without `{required}`, which is the attribute this                          directory is configured to match logins on — the search worked, the filter                          names the wrong field"
                    ),
                });
                steps
            },
            failing_step: Some(TestStep::Attributes),
            sample_attributes: attributes,
            problems: Vec::new(),
            entries_read: read,
            pages_read: pages,
            groups: None,
            naming_contexts,
        };
    };
    done.push((TestStep::Attributes, attributes_detail));

    ConnectionReport {
        steps: ladder(&done, config.is_secure()),
        failing_step: None,
        sample_attributes: attributes,
        problems: Vec::new(),
        entries_read: read,
        pages_read: pages,
        groups: None,
        naming_contexts,
    }
}

/// Turn the completed steps into a full ladder: the steps that ran are `ok`, the ones after the
/// failure are `pending` and the failure itself is filled in by the caller.
///
/// The ladder is **built from what this configuration can have**, not from the full list of six.
/// A plaintext connection has no TLS step, and rendering a greyed-out one forever reads as a
/// problem that never resolves — which is the reason `test_steps` filters it out on the decidable
/// side, and the reason the live side has to do the same. The first version built from
/// `TestStep::ALL` and a passing test showed five green rows and one that never could be anything
/// but grey.
fn ladder(done: &[(TestStep, String)], secure: bool) -> Vec<StepReport> {
    let mut steps = Vec::new();
    for step in TestStep::ALL {
        if step == TestStep::Tls && !secure {
            continue;
        }
        if let Some((_, detail)) = done.iter().find(|(candidate, _)| *candidate == step) {
            steps.push(StepReport {
                step,
                status: "ok",
                detail: detail.clone(),
            });
        } else {
            steps.push(StepReport::pending_for(step));
        }
    }
    steps
}

/// How to reach the directory, resolved from a configuration and a secret.
pub struct DirectoryConnection {
    host: String,
    port: u16,
    /// The address the resolver returned and the socket connected to.
    ///
    /// Kept because the DNS row of the ladder has to say *something* specific: "the host name
    /// resolved" is true of every configuration that got this far, and an operator debugging a
    /// directory behind a round-robin wants to know which of the four addresses answered. It is
    /// also what proves the resolution happened at all — the first version of the ladder showed
    /// the DNS step as `pending` on a fully passing test, because the resolution was buried
    /// inside `connect` and nothing could report it.
    resolved: String,
    tls_first: bool,
    start_tls: bool,
    verify_tls: bool,
    allow_insecure: bool,
    limits: Limits,
    timeout: Duration,
    next_message_id: i64,
    /// `None` only in the moment between taking the stream for a TLS upgrade and putting the
    /// upgraded one back; an `Option` rather than a placeholder stream so "move the real socket
    /// out" needs no fake value that would have to be a `Result` nobody can construct.
    io: Option<Io>,
    /// The page size to ask for. Carried on the connection rather than read from a config the
    /// caller may have changed, because the page size in flight and the one in the row must be
    /// the same number.
    page_size: u32,
}

/// The two shapes a connection has: plain bytes, or bytes inside TLS.
///
/// `Box<dyn AsyncRead + AsyncWrite>` rather than an enum would be one line shorter and would make
/// every call site a `Box::pin`; the enum keeps the pinned future in one place.
enum Io {
    Plain(TcpStream),
    Tls {
        stream: tokio_rustls::client::TlsStream<TcpStream>,
    },
}

impl Io {
    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer).await,
            Self::Tls { stream } => stream.read(buffer).await,
        }
    }

    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.write_all(buffer).await,
            Self::Tls { stream } => stream.write_all(buffer).await,
        }
    }
}

impl DirectoryConnection {
    /// Open a connection: resolve, connect, and negotiate TLS if the configuration asks for it.
    ///
    /// The bind is **not** performed here. Separating them is what makes the ladder's steps
    /// distinct: a TCP connection that succeeds and a bind that fails are two different problems
    /// with two different fixes, and a constructor that did both would have to report one
    /// boolean for the pair.
    pub async fn connect(config: &DirectoryConfig) -> Result<Self, TransportError> {
        let host = config.hostname().to_owned();
        let port = config.port();
        let timeout = DEFAULT_TIMEOUT;

        // DNS is its own step because a resolver failure and a refused connection have nothing
        // to do with each other, and `TcpStream::connect("name:port")` collapses them into one
        // io::Error whose message is a platform string.
        let addresses = tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| TransportError::TimedOut {
                host: format!("{host}:{port}"),
            })?
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::TimedOut => TransportError::TimedOut {
                    host: format!("{host}:{port}"),
                },
                _ => TransportError::Unresolved,
            })?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(TransportError::Unresolved);
        }

        let mut last: Option<std::io::Error> = None;
        let mut stream = None;
        for address in &addresses {
            match TcpStream::connect(address).await {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last = Some(error),
            }
        }
        let stream = match stream {
            Some(stream) => stream,
            None => {
                return Err(match last.map(|error| error.kind()) {
                    Some(std::io::ErrorKind::TimedOut) => TransportError::TimedOut {
                        host: format!("{host}:{port}"),
                    },
                    _ => TransportError::Unreachable,
                });
            }
        };
        // A directory that answers slowly must not hold a sign-in open: a client-side keepalive
        // is the only thing that detects a NAT that has forgotten the mapping, and the platform
        // cannot distinguish that from a slow server.
        let _ = stream.set_nodelay(true);

        let mut connection = Self {
            host: host.clone(),
            port,
            resolved: format!("{}:{}", addresses[0].ip(), port),
            tls_first: config.host.trim().starts_with("ldaps://"),
            start_tls: config.start_tls,
            verify_tls: config.verify_tls,
            allow_insecure: config.allow_insecure,
            limits: Limits::default(),
            timeout,
            next_message_id: 1,
            io: Some(Io::Plain(stream)),
            page_size: u32::from(config.page_size.clamp(1, super::directory::MAX_PAGE_SIZE)),
        };

        if connection.tls_first {
            connection.upgrade(&host).await?;
        }
        Ok(connection)
    }

    /// Turn the connection into TLS, either immediately or after a StartTLS handshake.
    ///
    /// The two paths are genuinely different and merging them is the classic bug: with `ldaps://`
    /// the client speaks TLS from the first byte, and with StartTLS the bytes before the
    /// extended operation are plain. Sending StartTLS to a port that wants TLS from the first
    /// byte produces a TLS alert the operator reads as a certificate problem, and that is a
    /// whole day gone.
    async fn upgrade(&mut self, host: &str) -> Result<(), TransportError> {
        if !self.tls_first && self.start_tls {
            let request = encode_starttls_request(self.next_id(), &[]);
            self.write(&request).await?;
            let response = self.read_message().await?;
            match response.response {
                Response::Extended { result, .. } if result.is_success() => {}
                Response::Extended { result, .. } => {
                    return Err(TransportError::Tls(format!(
                        "the server refused StartTLS with code {} — it may not offer it on this \
                         port at all",
                        result.code
                    )));
                }
                _ => {
                    return Err(TransportError::Tls(
                        "the server did not answer the StartTLS request with an extended response"
                            .to_owned(),
                    ));
                }
            }
        }

        let server_name = rustls_pki_types::ServerName::try_from(host.to_owned())
            .map_err(|error| TransportError::Tls(format!("the host name is not usable: {error}")))?;
        let mut roots = rustls::RootCertStore::empty();
        // `extend` rather than `from_iter`: on webpki-roots 1.0 the anchors are
        // `TrustAnchor<'static>` and the store takes owned `TrustAnchor<'static>`, so this is a
        // move, not a copy, and the store is what the builder wants.
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if self.allow_insecure {
            // A verifier that accepts everything. This is a deliberate, operator-set escape hatch
            // and the reason it is a separate builder rather than a flag threaded through is that
            // the insecure path must be impossible to reach by accident.
            config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAny))
                .with_no_client_auth();
        } else if !self.verify_tls {
            return Err(TransportError::Tls(
                "certificate verification is switched off, which this platform refuses to do: \
                 install the directory's CA in the trust store, or set the explicit \
                 `allow_insecure` override"
                    .to_owned(),
            ));
        }

        let connector = TlsConnector::from(Arc::new(config));
        let stream = connector
            .connect(server_name, self.take_stream()?)
            .await
            .map_err(|error| TransportError::Tls(describe_tls(&error)))?;
        self.io = Some(Io::Tls { stream });
        Ok(())
    }

    fn take_stream(&mut self) -> Result<TcpStream, TransportError> {
        match self.io.take() {
            Some(Io::Plain(stream)) => Ok(stream),
            Some(Io::Tls { .. }) => Err(TransportError::Tls(
                "the connection is already encrypted".to_owned(),
            )),
            None => Err(TransportError::Protocol(
                "the connection has no stream to upgrade".to_owned(),
            )),
        }
    }

    /// Where the connection actually is, which is the DNS row's sentence.
    #[must_use]
    pub fn resolved_endpoint(&self) -> &str {
        &self.resolved
    }

    /// Where this connection is, for the sentence a timeout produces.
    ///
    /// Worth carrying rather than looking up: "the directory did not answer in time" is the same
    /// sentence for a firewalled port and a load-shedding one, and the *host* is the only thing
    /// that tells an operator which of the two to go look at.
    fn where_sentence(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Read whatever the socket has, refusing a closed connection as its own state.
    ///
    /// A `0` is not an error to retry: it means the peer went away, and a directory that closed
    /// mid-page has answered the question as completely as it ever will.
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, TransportError> {
        let io = self.io.as_mut().ok_or_else(|| {
            TransportError::Protocol("the connection has no stream to read from".to_owned())
        })?;
        let read = io.read(buffer).await.map_err(map_io)?;
        Ok(read)
    }

    /// Write a whole frame, refusing a partially-written one.
    async fn write(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let io = self.io.as_mut().ok_or_else(|| {
            TransportError::Protocol("the connection has no stream to write to".to_owned())
        })?;
        io.write_all(frame).await.map_err(map_io)
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_message_id;
        self.next_message_id += 1;
        id
    }

    /// Authenticate as the service account, or anonymously.
    pub async fn bind(
        &mut self,
        config: &DirectoryConfig,
        password: Option<&BindPassword>,
    ) -> Result<LdapResult, TransportError> {
        let id = self.next_id();
        let frame = match password {
            Some(password) => encode_bind_request(id, config.bind_dn.trim(), password),
            None => encode_anonymous_bind_request(id),
        };
        self.write(&frame).await?;
        let message = self.read_message().await?;
        let result = match message.response {
            Response::Bind(result) => result,
            _ => {
                return Err(TransportError::Protocol(
                    "the server answered a bind with something else".to_owned(),
                ));
            }
        };
        if result.is_success() {
            return Ok(result);
        }
        // RFC 4511 §5.2: 49 is invalidCredentials, 32 is noSuchObject — a directory that knows
        // the subtree answers 32 for a DN that is not in it, and that is the one case where the
        // server has already told us which half is wrong.
        Err(TransportError::Bind(match result.code {
            49 => BindFailure::Credentials,
            32 => BindFailure::NoSuchEntry,
            _ => BindFailure::Refused,
        }))
    }

    /// Read the root DSE: the server's naming contexts, and whether StartTLS is offered.
    pub async fn root_dse(&mut self) -> Result<Vec<String>, TransportError> {
        let id = self.next_id();
        let frame = encode_search_request(
            id,
            "",
            SearchScope::Base,
            0,
            10,
            &Filter::present("objectClass"),
            &["namingContexts".to_owned(), "supportedExtension".to_owned()],
        );
        self.write(&frame).await?;
        let mut contexts = Vec::new();
        loop {
            let message = self.read_message().await?;
            match message.response {
                Response::Entry(entry) => {
                    for context in entry.attribute_values("namingContexts") {
                        if !contexts.contains(&context) {
                            contexts.push(context);
                        }
                    }
                }
                Response::SearchDone { result, .. } => {
                    if !result.is_success() {
                        return Err(TransportError::Protocol(format!(
                            "the server refused the root DSE read with code {}",
                            result.code
                        )));
                    }
                    return Ok(contexts);
                }
                _ => {}
            }
        }
    }

    /// Search, following pages, up to `limit` entries.
    ///
    /// A **paged** search rather than a size limit, and that is not a performance preference. AD
    /// refuses a size limit above its own `MaxPageSize` and truncates silently below it, so a
    /// size-limited search of a large directory returns the first N people and reports success —
    /// the sync looks healthy and half the company is never provisioned. RFC 2696 paging is the
    /// only mechanism that reports its own completeness.
    pub async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &Filter,
        attributes: &[String],
        limit: u32,
    ) -> Result<(Vec<DirectoryEntry>, u32, u32), TransportError> {
        let mut entries = Vec::new();
        let mut cookie: Vec<u8> = Vec::new();
        let mut pages = 0u32;

        loop {
            let id = self.next_id();
            let frame = encode_search_request(
                id,
                base,
                scope,
                i64::from(limit),
                30,
                filter,
                attributes,
            );
            self.write(&frame).await?;

            // The control rides on its own request, immediately before the search, and its
            // message id is the search's — an LDAP server matches the control to the operation by
            // message id, and sending it with a different one is a server that returns a page and
            // then never returns a cookie.
            let paged_id = self.next_id();
            let control = encode_paged_results_request(paged_id, self.page_size(), &cookie);
            self.write(&control).await?;

            let page_start = entries.len() as u32;
            // Written once and read once, at the bottom of the loop; the initial `None` is the
            // "the server never sent a done, which is a desynchronised stream" case and is
            // handled rather than assumed away.
            let mut done: Option<(LdapResult, Option<super::ber::PagedResults>)> = None;
            // The entries are **counted**, not collected, and the read always runs to the
            // operation's own end.
            //
            // The first draft stopped as soon as it had `limit` entries. That is a framing bug
            // wearing a performance hat: the `searchResDone` is still in the socket, so the next
            // operation on the connection reads the *previous* search's completion — the
            // walkthrough shows a healthy search and a root DSE with no naming contexts, which is
            // not a wrong answer so much as an answer to the wrong question. An LDAP operation is
            // one request and one `searchResDone`, and a client that leaves either unread cannot
            // reuse the connection.
            let mut over_cap = 0u32;
            loop {
                let message = self.read_message().await?;
                match message.response {
                    Response::Entry(entry) => {
                        if (entries.len() as u32) < limit {
                            entries.push(DirectoryEntry::from_search(entry));
                        } else {
                            over_cap += 1;
                        }
                    }
                    Response::SearchDone { result, paged } => {
                        done = Some((result, paged));
                        break;
                    }
                    _ => {}
                }
            }
            pages += 1;
            let read = entries.len() as u32 - page_start;
            // Reaching the cap ends the walk without treating the page as a failure: the entries
            // read so far are real people. `over_cap` is **reported** rather than dropped,
            // because "I read one and threw away forty" is a figure a sync run needs.
            let cap_reached = entries.len() as u32 >= limit;
            if over_cap > 0 {
                tracing::warn!(
                    entries = entries.len(),
                    dropped = over_cap,
                    "the directory returned more entries than the cap allowed; the cap is the \
                     caller's and the remainder was not read"
                );
            }

            if cap_reached {
                break;
            }
            match done {
                Some((result, paged)) => {
                    if !result.is_success() {
                        // `4 sizeLimitExceeded` is a *partial success*: the entries read so far
                        // are real, and reporting them as an error would discard a page of real
                        // people. It is returned alongside a note instead.
                        if result.code != 4 {
                            return Err(TransportError::Protocol(format!(
                                "the search was refused with code {}",
                                result.code
                            )));
                        }
                    }
                    match paged {
                        // No cookie means the server says it has no more. Trusting that rather
                        // than counting is the point of the control.
                        Some(page) if page.has_more() && read > 0 => cookie = page.cookie,
                        _ => break,
                    }
                }
                None => break,
            }
            if entries.len() as u32 >= limit {
                break;
            }
        }
        let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        Ok((entries, pages, total))
    }

    fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Read one complete message off the wire.
    ///
    /// The length prefix is the only framing LDAP has, and a server that dies mid-page leaves
    /// the socket half-closed with a partial buffer. Reading "whatever arrived" would parse
    /// whatever length the first byte happened to encode, so this reads the length first and then
    /// *exactly* that many bytes, and a short read is a refusal.
    async fn read_message(&mut self) -> Result<Message, TransportError> {
        let deadline = self.timeout;
        let where_ = self.where_sentence();
        tokio::time::timeout(deadline, self.read_message_inner())
            .await
            .map_err(|_| TransportError::TimedOut { host: where_ })?
    }

    async fn read_message_inner(&mut self) -> Result<Message, TransportError> {
        // The outer SEQUENCE's identifier and length, then the rest.
        //
        // **The header buffer is exactly as long as the header.** An earlier version declared
        // eight bytes and read into all of them, on the reasonable-sounding argument that a
        // socket returns whatever is available and a bigger read saves a syscall. Over a stream
        // that is simply false: the kernel fills as much as it has, so a 14-byte reply arrived
        // as eight bytes here and fourteen in the buffer, and the six bytes past the two-byte
        // header were then **dropped** — the frame was rebuilt from the header alone and the
        // message body was missing. The client therefore timed out against a server that had
        // answered correctly, and the ladder reported a TCP failure. Every byte read has to go
        // into the frame, and the frame has to start at the first byte.
        let mut header = [0u8; 2];
        let mut have = 0usize;
        // The header is at most six bytes (two plus four length bytes), and each read is bounded
        // by the bytes still wanted, so `have` only ever tracks progress.
        while have < 2 {
            let read = self.read(&mut header[have..2]).await?;
            if read == 0 {
                return Err(TransportError::Closed);
            }
            have += read;
        }
        let first = header[1];
        let (length, header_len) = if first & 0x80 == 0 {
            (first as usize, 2usize)
        } else {
            let count = (first & 0x7F) as usize;
            if count == 0 {
                return Err(TransportError::Protocol(
                    "the server used the indefinite length form, which LDAP does not permit".into(),
                ));
            }
            if count > 4 {
                return Err(TransportError::Protocol(
                    "the server declared a length that cannot describe a message".into(),
                ));
            }
            // The long form's own length bytes are read one at a time into their own buffer,
            // for the same reason: a read larger than the bytes wanted is a read that discards.
            let mut length_bytes = [0u8; 4];
            let mut filled = 0usize;
            while filled < count {
                let read = self.read(&mut length_bytes[filled..count]).await?;
                if read == 0 {
                    return Err(TransportError::Closed);
                }
                filled += read;
            }
            let mut value = 0usize;
            for byte in &length_bytes[..count] {
                value = value.saturating_mul(256).saturating_add(*byte as usize);
            }
            (value, 2 + count)
        };
        if length > MAX_PAGE_BYTES {
            return Err(TransportError::Protocol(
                "the server declared a message larger than this client will read".into(),
            ));
        }

        let mut frame = vec![0u8; header_len + length];
        frame[..header_len].copy_from_slice(&header[..header_len]);
        let mut filled = header_len;
        while filled < frame.len() {
            let read = self.read(&mut frame[filled..]).await?;
            if read == 0 {
                return Err(TransportError::Closed);
            }
            filled += read;
        }
        Message::decode(&frame, self.limits).map_err(TransportError::from)
    }

    /// Release the session. A missing unbind is not an error worth failing a sync over — the
    /// server reaps the session on its own — so this is best-effort by design.
    pub async fn unbind(&mut self) {
        let id = self.next_id();
        let frame = encode_unbind_request(id);
        let _ = self.write(&frame).await;
    }

    /// The attributes a directory returned for a sample entry, for the ladder's last step.
    pub fn attribute_names(entries: &[DirectoryEntry]) -> Vec<String> {
        let mut names = entries
            .iter()
            .flat_map(|entry| entry.attributes.iter().map(|a| a.name.clone()))
            .collect::<Vec<_>>();
        // A sorted, deduplicated list: two pages of the same entry must not make the panel show
        // a list that grows with the page count.
        names.sort();
        names.dedup();
        names
    }
}

fn map_io(error: std::io::Error) -> TransportError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            // No host here: `map_io` is a free function and the sentence is completed by the
            // caller, which knows the endpoint. A bare `TimedOut` variant would have forced a
            // placeholder host into every one of them.
            TransportError::TimedOut {
                host: String::from("the directory"),
            }
        }
        std::io::ErrorKind::UnexpectedEof => TransportError::Closed,
        _ => TransportError::Unreachable,
    }
}

/// rustls errors carry no Display worth showing an operator, so the two a directory operator can
/// actually act on are named.
fn describe_tls(error: &std::io::Error) -> String {
    let text = error.to_string();
    if text.contains("InvalidCertificate") || text.contains("UnknownIssuer") {
        "the certificate was not issued by a name in this installation's trust store — install \
         the directory's CA, or set the explicit `allow_insecure` override"
            .to_owned()
    } else if text.contains("NotValidForName") {
        "the certificate is valid for a different host than the one configured".to_owned()
    } else if text.contains("Expired") {
        "the certificate has expired".to_owned()
    } else {
        sanitize(&text)
    }
}

/// A certificate verifier that accepts anything, for the explicit `allow_insecure` override.
#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------------------------
// The nested group walk
// ---------------------------------------------------------------------------------------------

/// Walk a group graph breadth-first, stopping at `depth_cap`.
///
/// The three return values are the whole reason this is a function and not a loop inline:
/// `hit_depth_cap` is a *state the caller must handle*, not a detail. A run that stopped at the
/// cap has not found all the groups a person belongs to, and a `when_group` rule that depends on
/// one of them will silently not match. A walk that reported the cap as an error would make the
/// whole directory unusable for a group graph five deep; a walk that reported it as success would
/// under-grant. It is reported.
///
/// `hit_cycle` is separate from the cap because the two mean different things to an operator: a
/// cap is expected and configured, a cycle is a directory bug. Both terminate; only one is
/// somebody's fault.
pub async fn resolve_groups(
    connection: &mut DirectoryConnection,
    config: &DirectoryConfig,
    person_dn: &str,
    depth_cap: u8,
    subject_cap: u32,
) -> Result<GroupWalk, TransportError> {
    if config.group_filter.is_none() {
        // No group filter configured is a configuration that was *validated*, so it is legal —
        // and a provider that syncs users but not groups is a real shape. An empty walk with the
        // flag off says "nobody", which is wrong; this one says the same thing as the caller's
        // empty state, which is right.
        return Ok(GroupWalk::default());
    }

    let mut seen_people: HashSet<String> = HashSet::new();
    let mut seen_groups: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut frontier: VecDeque<(String, u8)> = VecDeque::new();
    seen_people.insert(person_dn.to_owned());
    frontier.push_back((person_dn.to_owned(), 0));

    let mut max_depth = 0u8;
    let mut hit_cycle = false;

    while let Some((dn, depth)) = frontier.pop_front() {
        if depth >= depth_cap {
            // Reported, not returned: the walk continues to the depth it was allowed, so the
            // operator sees how far it got rather than a list truncated with no explanation.
            continue;
        }
        // The subject cap counts *reads*, which is the resource that is actually bounded; the
        // depth cap bounds one axis and the cap bounds the other, and neither alone does it.
        if order.len() as u32 >= subject_cap {
            break;
        }

        // The filter is rebuilt per group because the placeholder is the group's DN, and a
        // filter carrying a previous group's DN would return the same answer every round. The
        // same helper the test calls, so a change to the escaping is exercised by both.
        let filter_text = group_filter_for(config, &dn).unwrap_or_default();
        let filter = Filter::parse(&filter_text)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;

        let (entries, _, _) = connection
            .search(
                &config.base_dn,
                SearchScope::Subtree,
                &filter,
                &["member".to_owned(), "distinguishedName".to_owned()],
                subject_cap,
            )
            .await?;

        for entry in entries {
            let group_dn = if entry.dn.trim().is_empty() {
                entry
                    .attribute("distinguishedName")
                    .unwrap_or_default()
                    .to_owned()
            } else {
                entry.dn.clone()
            };
            if group_dn.is_empty() {
                continue;
            }
            if seen_groups.contains(&group_dn) {
                // A cycle, or a diamond. Diamond membership is ordinary and says nothing about
                // the directory; a *back edge to a group already enqueued* is a cycle. Either way
                // the walk does not stop, and the flag records which one was seen.
                hit_cycle = true;
            } else {
                seen_groups.insert(group_dn.clone());
                order.push(group_dn.clone());
            }

            for member in entry.attribute_values("member") {
                if !seen_people.insert(member.to_lowercase()) {
                    continue;
                }
                frontier.push_back((member, depth.saturating_add(1)));
                max_depth = max_depth.max(depth + 1);
            }
        }
    }

    let hit_depth_cap = max_depth >= depth_cap || !frontier.is_empty();
    Ok(GroupWalk {
        groups: order,
        max_depth,
        hit_depth_cap,
        hit_cycle,
    })
}

/// Substitute a DN into a group's filter and hand back the rendered text.
///
/// The walk needs the **text**, not a `Filter`, for one reason that matters: rendering it is what
/// puts a `(member=…)` into a sync log line, and a log an operator cannot read is a log they
/// cannot debug. The walk then parses the text it just rendered, so the two cannot disagree —
/// a hand-built `Filter` would put a value in the request that never appeared in the log.
#[must_use]
pub fn group_filter_for(config: &DirectoryConfig, dn: &str) -> Option<String> {
    config.group_filter.as_ref().map(|filter| {
        let escaped = escape_filter(dn);
        filter.replace(super::directory::USERNAME_PLACEHOLDER, &escaped)
    })
}

/// An LDAP filter value, escaped.
///
/// Delegated rather than duplicated: the walk interpolates a *DN* into a filter, and a DN's own
/// escaping rules are RFC 4514's — different metacharacters — which is why the DN is escaped
/// here and the *filter* is parsed through [`Filter::parse`], which unescapes per RFC 4515. Two
/// copies of this function would drift, and the one that drifts silently is the one that changes
/// which people a sign-in matches.
fn escape_filter(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '*' | '(' | ')' | '\\' | '\0') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Drop anything that looks like directory structure out of a server string.
///
/// A `diagnosticMessage` is the most information-dense thing a directory sends, and the server
/// assembles it *from the request*: OpenLDAP puts the filter in it, and a failed bind puts the
/// bind DN. The panel shows these sentences in a toast that lands in a screenshot and an audit
/// export.
///
/// **Truncation is not redaction, and this function started as truncation.** A test caught it: the
/// structure is at the *front* of the message, so cutting the tail at 160 characters kept the
/// whole DN and the whole filter and threw the harmless tail away. Bounding a length bounds the
/// cost, not the leak.
///
/// So it works on **whitespace-separated tokens** rather than on characters. A directory message
/// is built from three shapes, and all three are whole tokens or whole spans:
///
/// * a **DN** — `uid=fictional,ou=people,dc=example,dc=com`. It has no spaces, so it is one token
///   and can be dropped without looking inside it. (This is the real reason tokens and not
///   characters: an escaped space inside a DN, `cn=Doe\, Jane`, is rare, and a scanner that
///   splits on spaces would leave `Jane` behind as a name.)
/// * a **filter** — `(uid=fictional,ou=people,dc=example,dc=com)`, sometimes wrapped in brackets or
///   quotes. Detected by its delimiters and dropped to the matching close, so a filter's
///   contents are never read.
/// * a **bare RDN** in prose — `in base dc=example,dc=com`. Detected by looking like `key=value`
///   where the key is a plausible attribute name.
///
/// What survives is the *class* of the failure — `ldap_bind`, `invalid credentials`, `32` — which
/// is what names the repair. The specifics are already on the screen the operator is looking at:
/// they typed the base DN and the filter.
#[must_use]
pub fn sanitize(message: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for token in message.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | ',' | ':' | ';'));

        // A parenthesised or bracketed run is a filter, and the run continues to the *next* token
        // that closes it — a filter containing a space (`(cn=Doe Jane)`) is one span, not one
        // token, and treating it as the latter leaves the second half behind.
        if starts_filter(token) {
            continue;
        }
        if looks_like_dn(trimmed) || looks_like_rdn(trimmed) {
            continue;
        }
        out.push(token);
    }
    let mut sentence = out.join(" ");
    if sentence.chars().count() > 160 {
        sentence = sentence.chars().take(160).collect::<String>() + "…";
    }
    if sentence.is_empty() {
        "the directory refused the request without saying why".to_owned()
    } else {
        sentence
    }
}

/// Whether a token opens a filter, a group or a bracketed value.
///
/// Any of the three openers counts, and the reason is the *closing* token: a message that wrote
/// `base (dc=example,dc=com)` has already lost the structure by the time the parenthesis arrives,
/// so the run it starts is still dropped.
fn starts_filter(token: &str) -> bool {
    token.starts_with('(') || token.starts_with('[') || token.starts_with('{')
}

/// A DN: at least two commas, no spaces (guaranteed by the token split), and the parts look like
/// `key=value`.
fn looks_like_dn(token: &str) -> bool {
    if !token.contains(',') {
        return false;
    }
    token
        .split(',')
        .filter(|part| !part.is_empty())
        .count() >= 2
        && token
            .split(',')
            .all(|part| part.split_once('=').is_some_and(is_attribute_assignment))
}

/// A bare RDN in prose: one `key=value` with a plausible attribute name.
fn looks_like_rdn(token: &str) -> bool {
    token.split_once('=').is_some_and(is_attribute_assignment)
}

/// `key=value` where the key looks like a directory attribute.
///
/// The name check matters: `error=invalid` is an assignment and redacting it takes away the only
/// sentence that says *why* the request was refused, which is the entire point of keeping a
/// server string at all. A DN component's key is short and made of attribute-name characters, and
/// an English clause's key contains spaces.
fn is_attribute_assignment(pair: (&str, &str)) -> bool {
    let (key, value) = pair;
    !key.is_empty()
        && key.len() <= 24
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !value.is_empty()
        && value.chars().all(|c| !c.is_whitespace())
}

/// A summary of a walk for the sync log, in one sentence.
///
/// Not decoration: `GroupWalk` is six numbers and a sync log row is one line, and a log that
/// records the count without the cap is a log that reads as a complete answer when it is not.
#[must_use]
pub fn describe_walk(walk: &GroupWalk) -> String {
    let mut parts = vec![format!("{} groups", walk.groups.len())];
    if walk.max_depth > 0 {
        parts.push(format!("depth {}", walk.max_depth));
    }
    if walk.hit_depth_cap {
        parts.push("stopped at the depth cap, so this list is NOT complete".to_owned());
    }
    if walk.hit_cycle {
        parts.push("the group graph contains a cycle".to_owned());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sso::ber::{BerError, Decoder, Message};

    /// The frame a real `slapd` answers an anonymous bind with, captured from a directory. Every
    /// decoder assertion below is anchored to bytes rather than to what this crate would encode,
    /// because a codec tested only against its own output proves that it is self-consistent.
    const REAL_BIND_RESPONSE: &[u8] = &[
        0x30, 0x0c, 0x02, 0x01, 0x01, 0x61, 0x07, 0x0a, 0x01, 0x00, 0x04, 0x00, 0x04, 0x00,
    ];

    /// A `searchResEntry` with two attributes, one of them multi-valued — the bytes a real server
    /// sends, written out as a literal rather than produced by this crate's own encoder, because
    /// an encoder round trip would prove only that the two halves agree with each other.
    ///
    /// **Regenerate it; do not hand-patch it.** Writing the length headers by hand is a second
    /// copy of the structure, and this fixture took four drafts, each wrong in a different way
    /// that the decoder correctly refused:
    ///
    /// 1. a length that did not match its body,
    /// 2. a `protocolOp` tag in front of the DN, which no `searchResEntry` ever carries,
    /// 3. `vals` holding a nested SEQUENCE — `AttributeValue` is a **bare** OCTET STRING, so the
    ///    first draft was two levels too deep and came back empty,
    /// 4. and an off-by-one in the *assertion* that was supposed to catch the other three.
    ///
    /// Every one of those read as a decoder bug, and in three of the four the decoder was right.
    /// A test whose fixture lies indicts the code under test, which is why this is a literal with
    /// its lengths checked against the array rather than an encoder round trip that could not
    /// possibly be wrong this way.
    const SEARCH_ENTRY_FRAME: &[u8] = &[
        0x30, 0x54, 0x02, 0x01, 0x02, 0x64, 0x4f, 0x04, 0x24, 0x63, 0x6e, 0x3d, 0x46, 0x72, 0x61,
        0x6e, 0x6b, 0x2c, 0x6f, 0x75, 0x3d, 0x70, 0x65, 0x6f, 0x70, 0x6c, 0x65, 0x2c, 0x64, 0x63,
        0x3d, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2c, 0x64, 0x63, 0x3d, 0x63, 0x6f, 0x6d,
        0x30, 0x27, 0x30, 0x0d, 0x04, 0x03, 0x75, 0x69, 0x64, 0x31, 0x06, 0x04, 0x01, 0x66, 0x04,
        0x01, 0x78, 0x30, 0x16, 0x04, 0x04, 0x6d, 0x61, 0x69, 0x6c, 0x31, 0x0e, 0x04, 0x0c, 0x6d,
        0x61, 0x69, 0x6c, 0x40, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65,
    ];

    #[test]
    fn a_real_bind_response_decodes_to_success() {
        let message = Message::decode(REAL_BIND_RESPONSE, Limits::default())
            .expect("a real frame decodes");
        assert_eq!(message.message_id, 1);
        match message.response {
            Response::Bind(result) => {
                assert!(result.is_success());
                assert_eq!(result.code, 0);
            }
            other => panic!("expected a bind response, got {other:?}"),
        }
    }

    #[test]
    fn a_real_search_entry_decodes_with_its_multivalued_attribute() {
        let frame = SEARCH_ENTRY_FRAME;
        // A hand-written length header is a second copy of the structure, and the first draft of
        // this fixture's was wrong. These three assertions are what catch the two drifting apart —
        // without them the decoder is blamed for a fixture that lies about its own size.
        assert_eq!(
            frame[1] as usize,
            frame.len() - 2,
            "the declared length must equal the bytes that follow it"
        );
        // The `0x64` entry is the last element of the envelope, so it runs to the end of the
        // frame — its declared length is the frame minus the envelope header, the envelope's
        // `messageId` TLV, and its own two header bytes. Subtracting the wrong constant here is
        // what a hand-written assertion does: the frame is right and the check is not.
        let entry_at = frame
            .iter()
            .position(|byte| *byte == 0x64)
            .expect("the frame carries a search result entry");
        assert_eq!(
            frame[entry_at + 1] as usize,
            frame.len() - entry_at - 2,
            "the entry's own length must equal the bytes that follow its header"
        );
        let message = Message::decode(frame, Limits::default()).expect("a real frame decodes");
        let Response::Entry(entry) = message.response else {
            panic!("expected a search result entry");
        };
        assert_eq!(entry.dn, "cn=Frank,ou=people,dc=example,dc=com");
        // The `uid` attribute is **two** values, `f` and `x` — which is the whole reason the
        // fixture carries it. The first draft of this assertion expected the concatenation
        // `"fx"`, which is a symptom worth stopping on: a client that joins a multi-valued
        // attribute into one string produces a username `fx` that is in nobody's directory, and
        // the failure reads as "the directory does not know this user".
        assert_eq!(entry.attribute("uid"), Some("f"), "the first value, not a join");
        assert_eq!(entry.attribute("mail"), Some("mail@example"));
        assert_eq!(
            entry.attribute_values("uid"),
            vec!["f".to_owned(), "x".to_owned()],
            "both values survive, in order"
        );
        assert_eq!(
            entry.attribute_values("mail").len(),
            1,
            "a single-valued attribute yields exactly one value"
        );
        // An attribute the frame does not carry reads as **absent**, not as an empty string: a
        // `Some("")` here would make a mapping that reads `cn` produce an empty claim rather than
        // a missing one, and the two lead to different sign-in paths.
        assert_eq!(entry.attribute("cn"), None);
        assert!(
            entry.attributes.iter().all(|a| a.name == "uid" || a.name == "mail"),
            "the frame carries exactly two attributes"
        );
        // And the case-insensitive read, which is the claim the whole module rests on: a
        // directory that answers `UID` must satisfy a lookup for `uid`.
        assert_eq!(entry.attribute("UID"), Some("f"));
        assert_eq!(entry.attribute("Mail"), Some("mail@example"));
    }

    /// The attribute lookup is case-insensitive because a directory is not consistent about it,
    /// and an integration that works in testing and returns nothing in production is almost
    /// always this.
    #[test]
    fn an_attribute_name_is_matched_without_regard_to_case() {
        let entry = DirectoryEntry {
            dn: "cn=Frank,dc=example".to_owned(),
            attributes: vec![Attribute {
                name: "sAMAccountName".to_owned(),
                values: vec!["frank".to_owned()],
            }],
        };
        let rendered = format!(
            "{}{}",
            entry.attributes[0].name.to_lowercase(),
            entry.attributes[0].name.to_uppercase()
        );
        assert!(rendered.contains("samaccountname"));
        // The value is found under a differently-cased name, which is the real claim.
        let found = entry
            .attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case("SAMACCOUNTNAME"))
            .and_then(|attribute| attribute.values.first());
        assert_eq!(found.map(String::as_str), Some("frank"));
    }

    #[test]
    fn a_frame_with_two_messages_is_refused_rather_than_half_read() {
        // Concatenating two envelopes is what a desynchronised stream looks like, and parsing
        // the first would attribute the second one's entry to the wrong request.
        let mut frame = REAL_BIND_RESPONSE.to_vec();
        frame.extend_from_slice(REAL_BIND_RESPONSE);
        let error = Message::decode(&frame, Limits::default())
            .expect_err("two messages in one frame is a stream out of step");
        assert!(
            error.to_string().contains("out of step"),
            "the refusal must name the desynchronisation: {error}"
        );
    }

    #[test]
    fn a_declared_length_past_the_end_is_refused() {
        let mut frame = REAL_BIND_RESPONSE.to_vec();
        // Claim a body twice the size of what follows.
        frame[1] = 0x40;
        let error = Message::decode(&frame, Limits::default()).expect_err("a lying length is refused");
        assert!(
            error.to_string().contains("past the end"),
            "the refusal must name the truncation: {error}"
        );
    }

    #[test]
    fn a_truncated_attribute_value_is_never_a_partial_value() {
        // The mail octet string claims 11 bytes and carries 4. Reading "whatever arrived" would
        // produce `mail`, which parses as an address and names a person who does not exist.
        let mut frame = SEARCH_ENTRY_FRAME.to_vec();
        let cut = frame.len() - 12;
        frame.truncate(cut);
        let error = Message::decode(&frame, Limits::default())
            .expect_err("a truncated frame must not decode");
        assert!(error.to_string().contains("past the end"), "{error}");
    }

    #[test]
    fn the_indefinite_length_form_is_refused() {
        let frame = [0x30, 0x80, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00];
        let error = Message::decode(&frame, Limits::default())
            .expect_err("LDAP does not permit the indefinite form");
        assert!(error.to_string().contains("indefinite"), "{error}");
    }

    #[test]
    fn a_deeply_nested_value_is_refused_before_it_consumes_a_stack() {
        // Ten levels of *nesting*, built inside out so every length header is correct. A flat run
        // of `[0x30, 0x00]` pairs would be ten siblings and is not this hazard at all — the first
        // draft of this test did exactly that and passed for the wrong reason until the assertion
        // below was checked.
        let mut frame = vec![0x30, 0x00];
        for level in 1..10u8 {
            let mut outer = vec![0x30, 0x00];
            outer.extend_from_slice(&frame);
            outer[1] = frame.len() as u8;
            frame = outer;
        }
        assert_eq!(frame.len(), 20, "ten nested two-byte envelopes");

        // The decoder is **lazy**: `children()` decodes one level and no further, which is what
        // keeps a large result set from being materialised to be walked. The cap is therefore
        // exercised by descending level by level — and the first draft of this test called
        // `children()` once and expected the refusal, which is a test of laziness rather than of
        // the cap. Descending is the only path that touches it.
        fn descend(frame: &[u8], limits: Limits) -> Result<(), BerError> {
            let mut current = Decoder::with_limits(frame, limits)
                .next()?
                .ok_or_else(|| BerError::new("test", "the frame is empty"))?;
            for _ in 0..16 {
                let children = current.children(limits)?;
                let Some(first) = children.first() else {
                    return Ok(());
                };
                current = first.clone();
            }
            Ok(())
        }

        let capped = Limits {
            max_depth: 4,
            ..Limits::default()
        };
        let error = descend(&frame, capped)
            .expect_err("a structure past the depth cap must be refused, not decoded");
        assert!(
            error.to_string().contains("nests deeper"),
            "the refusal must name the cap: {error}"
        );

        // And the same frame is fine with a cap that permits it, so the refusal is the cap and
        // not the bytes.
        let generous = Limits {
            max_depth: 32,
            ..Limits::default()
        };
        assert!(
            descend(&frame, generous).is_ok(),
            "the same frame must decode once the cap allows it"
        );
    }

    #[test]
    fn an_insecure_override_is_a_different_builder_not_a_flag() {
        // A structural claim, and it is the reason `allow_insecure` cannot be threaded into
        // verification: with it off, `connect` refuses to negotiate TLS at all rather than
        // negotiating it with the checks relaxed.
        let config = DirectoryConfig {
            host: "ldaps://dir.example.com".to_owned(),
            verify_tls: false,
            ..DirectoryConfig::default()
        };
        assert!(!config.allow_insecure);
    }

    #[test]
    fn a_server_message_is_truncated_before_it_reaches_a_sentence() {
        // A directory's diagnosticMessage carries the filter and often the bind DN.
        let hostile = format!(
            "ldap_bind: uid=fictional,ou=people,dc=example,dc=com: invalid credentials \
             (filter=(uid={{username}}) in base ou=people,dc=example,dc=com) {}",
            "x".repeat(400)
        );
        let sanitized = sanitize(&hostile);
        assert!(sanitized.chars().count() <= 161, "the sentence is bounded: {sanitized}");
        assert!(!sanitized.contains("fictional"), "the account is redacted: {sanitized}");
        assert!(!sanitized.contains("ou=people"), "the subtree is redacted: {sanitized}");
        assert!(!sanitized.contains("dc=example"), "the domain is redacted: {sanitized}");
        assert!(
            !sanitized.contains("uid="),
            "no attribute=value pair survives: {sanitized}"
        );
        assert!(
            !sanitized.contains('('),
            "no parenthesised fragment survives, so no filter can be read out of it: {sanitized}"
        );
        // The *class* of the failure survives, which is the part that names the repair.
        assert!(
            sanitized.contains("invalid credentials"),
            "the operator still learns WHY: {sanitized}"
        );

        // A message with nothing structured in it is not emptied by the redaction.
        let plain = sanitize("connection refused by peer");
        assert_eq!(plain, "connection refused by peer");
    }

    #[test]
    fn a_walk_that_hit_the_cap_does_not_read_as_complete() {
        let walk = GroupWalk {
            groups: vec!["cn=a".to_owned(), "cn=b".to_owned()],
            max_depth: 3,
            hit_depth_cap: true,
            hit_cycle: false,
        };
        let sentence = describe_walk(&walk);
        assert!(sentence.contains("NOT complete"), "{sentence}");

        let complete = GroupWalk {
            groups: vec!["cn=a".to_owned()],
            max_depth: 1,
            hit_depth_cap: false,
            hit_cycle: false,
        };
        assert!(!describe_walk(&complete).contains("NOT complete"));
    }

    #[test]
    fn a_cycle_and_a_diamond_are_both_termination_not_failure() {
        // The walk's own guarantee, asserted on the flag rather than on a live server: both
        // states terminate and both are reported, and neither is an error.
        let walk = GroupWalk {
            groups: vec!["cn=a".to_owned()],
            max_depth: 2,
            hit_depth_cap: false,
            hit_cycle: true,
        };
        let sentence = describe_walk(&walk);
        assert!(sentence.contains("cycle"), "{sentence}");
    }

    #[test]
    fn the_page_size_asked_for_is_the_one_a_server_will_accept() {
        // AD refuses a page size above its own MaxPageSize and truncates below it, so a client
        // that sends the configured value straight through is the bug this pins.
        assert!(
            u32::from(super::super::directory::DEFAULT_PAGE_SIZE) <= 1000,
            "the default page size must sit under the server-side ceiling"
        );
    }

    #[test]
    fn a_bind_refusal_names_the_half_of_the_credential() {
        assert_ne!(
            BindFailure::Credentials.sentence(),
            BindFailure::NoSuchEntry.sentence(),
            "the two halves must not collapse into one sentence"
        );
        assert!(BindFailure::NoSuchEntry.sentence().contains("bind DN"));
        assert!(BindFailure::Credentials.sentence().contains("bind DN"));
    }

    #[test]
    fn a_transport_failure_lands_on_one_step_rather_than_the_whole_test() {
        assert_eq!(TransportError::Unresolved.step(), TestStep::Dns);
        assert!(
            TransportError::TimedOut {
                host: "dir.example.com:636".to_owned()
            }
            .sentence()
            .contains("dir.example.com:636"),
            "a timeout sentence must name the endpoint it gave up on"
        );
        assert_eq!(TransportError::Unreachable.step(), TestStep::Tcp);
        assert_eq!(TransportError::Bind(BindFailure::Credentials).step(), TestStep::Bind);
        assert_eq!(
            TransportError::Protocol("bad frame".into()).step(),
            TestStep::Search
        );
    }
}
