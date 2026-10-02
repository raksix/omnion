//! What "local" means, and the one client that refuses to leave (REQ-106, slice 1).
//!
//! # Why this is a function and not a helper in a route
//!
//! Three call sites need the same answer — the endpoint save path, the air-gap check (slice 2) and
//! the doctor (slice 4) — and they need it to be the **same** answer. Three copies of "is this
//! host private?" would drift the first time one of them learned a new rule, and the drift would
//! show up as an installation whose air-gap switch refuses a local model or lets a remote one
//! through: both failures are silent, and the second is the dangerous one. So the rule set lives
//! here, is pure, and is proven by unit tests with no database in the picture.
//!
//! # "Local" is a claim about the *host*, checked at write time and re-checked at call time
//!
//! A base URL of `http://192.168.1.10:11434/v1` is local because of what its host resolves to,
//! not because the operator typed the word "local". Three rules admit a host, in order:
//!
//! 1. **loopback** — `localhost`, `127.0.0.0/8`, `::1`. The strongest claim: the request cannot
//!    leave this machine.
//! 2. **private** — RFC 1918 (`10/8`, `172.16/12`, `192.168/16`), the carrier-grade NAT range
//!    `100.64/10`, and IPv6 unique-local `fc00::/7`. An installation on a private network treats
//!    these as "ours"; a public IP is never one of them.
//! 3. **allow-listed** — a host the operator added by name. This **widens** the rule set and
//!    never replaces it, which is why a test for the first two still passes with an empty
//!    allow-list.
//!
//! The metadata endpoints (`169.254.169.254` and friends) are refused as a *fourth* answer even
//! though `169.254.0.0/16` is link-local rather than private: a provider base URL is never a
//! cloud metadata service, and treating one as local would hand an air-gapped installation a
//! path to the host's own credentials.
//!
//! # The redirect refusal is the control that makes the switch worth trusting
//!
//! `is_local_host` answers a question about a *URL*. A local URL can answer `302` to
//! `https://api.openai.com/`, and a client that follows redirects happily would then be the thing
//! that crosses the air gap — the check passed, the check was right about the URL it was given,
//! and the bytes went out anyway. So [`local_http`] builds a client with
//! [`reqwest::redirect::Policy::none`], and a `3xx` is turned into a refusal that **names the
//! redirect target**. Silently not following would leave an operator staring at a mystery
//! `200` with an empty body; the request's own wording is that "the refusal names the redirect
//! target", and the name is the only thing that distinguishes a moved endpoint from a proxy.

use std::time::Duration;

use crate::error::{AiHubError, Result};

/// Host names that always mean "this machine".
const LOOPBACK_NAMES: &[&str] = &["localhost", "localhost.localdomain", "ip6-localhost"];

/// The hosts a *host.docker.internal*-style name refers to on the usual container runtimes.
///
/// Docker Desktop resolves it on macOS and Windows; on Linux it is not defined by default, which
/// is why [`is_local_host`] treats the bare name as loopback-class rather than failing to resolve
/// it. The platform is deployed as containers far more often than not, and "is my Ollama local?"
/// answered "no" because of a DNS convention would be a wrong answer in the common case.
const CONTAINER_ALIASES: &[&str] = &[
    "host.docker.internal",
    "gateway.docker.internal",
    "docker.for.mac.localhost",
    "docker.for.win.localhost",
];

/// Cloud instance-metadata endpoints: never a provider, and never "our network".
const METADATA_HOSTS: &[&str] = &[
    "169.254.169.254",
    "metadata.google.internal",
    "metadata.goog",
    "metadata",
    "100.100.100.200",
];

/// Why a host counts as local — the value stored in `ai_providers.host_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// The host is this machine.
    Loopback,
    /// The host is on the installation's own private network.
    Private,
    /// The host was added to the installation's internal-host allow-list by name.
    Allowlisted,
}

impl HostKind {
    /// Wire name, as stored in `host_kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::Allowlisted => "allowlisted",
        }
    }
}

/// The host part of a URL, lowercased, without a port.
///
/// Returns `None` for a URL that has no host at all (`/v1`, `file:///tmp/x`), which is a
/// relative or non-network URL rather than a local one — the caller is told it is not local
/// rather than being handed a `None` it has to interpret.
#[must_use]
pub fn host_of(base_url: &str) -> Option<String> {
    // The scheme is case-insensitive per RFC 3986, and an operator pasting a URL from a config
    // file can easily carry `HTTP://`. Stripping only the lowercase form made `host_of` answer
    // `None` for a perfectly good URL, which the save path then reports as "not a network URL".
    let lowered = base_url.to_ascii_lowercase();
    let rest = lowered
        .strip_prefix("http://")
        .or_else(|| lowered.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip the credentials and the port. `user:pass@host:1234` is legal in a URL and would
    // otherwise compare as an unknown host name.
    let host = authority.rsplit('@').next()?;
    let host = match host.rfind(']') {
        // An IPv6 literal keeps its brackets: `[::1]` → split on the last colon, not the first.
        Some(close) if close > host.find(':').unwrap_or(usize::MAX) => &host[..=close],
        _ => host.split(':').next().unwrap_or(host),
    };
    let host = host.trim_matches(['[', ']']);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Decide whether a host is local, given the installation's allow-list.
///
/// The allow-list is `&[String]` rather than `&HashSet` because it is a handful of rows read once
/// per save and once per check; a linear scan of five names is cheaper than building a hash of
/// them, and it keeps the call sites free of a collection they would not otherwise need.
///
/// # Errors
///
/// Returns `Err` only for a metadata endpoint, and the message names the host: the caller turns
/// this into a field error on the base URL, and "that host is refused because it is a metadata
/// service" is a fix, where "invalid base URL" is a shrug.
pub fn classify_host(host: &str, allowlist: &[String]) -> Result<Option<HostKind>> {
    let host = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    if host.is_empty() {
        return Ok(None);
    }
    if METADATA_HOSTS.contains(&host.as_str()) {
        return Err(AiHubError::InvalidProvider(format!(
            "\"{host}\" is a cloud instance-metadata endpoint. A provider base URL is never a \
             metadata service, and treating it as local would let an air-gapped installation \
             reach the host's own credentials."
        )));
    }
    if LOOPBACK_NAMES.contains(&host.as_str()) || CONTAINER_ALIASES.contains(&host.as_str()) {
        return Ok(Some(HostKind::Loopback));
    }
    if host == "::1" || host == "0:0:0:0:0:0:0:1" {
        return Ok(Some(HostKind::Loopback));
    }
    // **Loopback addresses are checked before the private ranges, not inside them.** `127.0.0.1`
    // satisfies "private" too (it is in `is_private_ip`'s list), and checking private first
    // classified every loopback endpoint as `private` — which is *not* a wrong answer for the
    // air-gap check, since both are local, and is exactly a wrong answer for `host_kind`, where
    // the whole point is being able to say *why* a host was admitted. A local Ollama on
    // 127.0.0.1 reading as a private-network host is a fact about a different machine, so the
    // two answers stay distinct.
    if is_loopback_ip(&host) {
        return Ok(Some(HostKind::Loopback));
    }
    if is_private_ip(&host) {
        return Ok(Some(HostKind::Private));
    }
    if allowlist
        .iter()
        .any(|entry| entry.trim().eq_ignore_ascii_case(&host))
    {
        return Ok(Some(HostKind::Allowlisted));
    }
    Ok(None)
}

/// `true` when a literal address is this machine's loopback range.
///
/// Split out of [`is_private_ip`] rather than sharing its match arms: `127.0.0.0/8` is *both* a
/// loopback range and, under a permissive reading, a private one, and the classification the
/// caller stores has to name one. Loopback is the stronger claim ("cannot leave this host"), so
/// it is decided first and the private list keeps the RFC ranges.
fn is_loopback_ip(host: &str) -> bool {
    matches!(parse_ipv4(host), Some([127, ..]))
}

/// `true` when a literal address is on the installation's own networks.
///
/// **A literal only.** A hostname that resolves to a private address is not admitted here, and
/// that is deliberate rather than an omission: resolving it would make a save-time check depend on
/// a DNS answer, so the same base URL could be "local" on Tuesday and "remote" on Wednesday
/// depending on a resolver the operator does not control. A hostname is admitted by the
/// allow-list, which is a deliberate act with a name attached to it.
fn is_private_ip(host: &str) -> bool {
    if let Some(octets) = parse_ipv4(host) {
        // Loopback is decided in `is_loopback_ip` before this function is reached, so /8 is
        // deliberately absent here: keeping it would make the two answers disagree again the
        // moment the call order changed.
        return match octets {
            [10, ..] => true,
            // RFC 1918: 172.16.0.0/12.
            [172, second, ..] => (16..=31).contains(&second),
            [192, 168, ..] => true,
            // Carrier-grade NAT, which a cloud VPC commonly hands out and which is as internal as
            // RFC 1918 as far as a request from inside can tell.
            [100, second, ..] => (64..=127).contains(&second),
            _ => false,
        };
    }
    if let Some(head) = parse_ipv6_head(host) {
        // fc00::/7 covers fc.. and fd.., which is unique-local space.
        return head == 0xfc || head == 0xfd;
    }
    false
}

/// Parse a dotted-quad, rejecting anything with a leading-zero octet or a wrong count.
///
/// `010.0.0.1` parses as ten in most languages and as eight in others; a host string that means
/// two different addresses depending on the parser is not a host this function should answer for.
fn parse_ipv4(host: &str) -> Option<[u16; 4]> {
    let mut octets = [0u16; 4];
    let mut count = 0;
    for part in host.split('.') {
        if count == 4
            || part.is_empty()
            || part.len() > 3
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        octets[count] = part.parse().ok()?;
        if octets[count] > 255 {
            return None;
        }
        count += 1;
    }
    if count == 4 { Some(octets) } else { None }
}

/// The first hextet of an IPv6 literal, or `None` when the string is not one.
///
/// Written by hand rather than pulled in as a dependency because the answer is one byte: the
/// crate would be parsed, formatted and audited to produce a single comparison that only has to
/// distinguish `fc..`/`fd..` from everything else.
fn parse_ipv6_head(host: &str) -> Option<u8> {
    if !host.contains(':') {
        return None;
    }
    // An IPv4-mapped literal (`::ffff:127.0.0.1`) carries its address in dotted form; that tail is
    // what the loopback and private checks need, and the head `0` is not unique-local.
    let head = host.split(':').next()?;
    if head.is_empty() {
        // A leading `::` means the first hextet is zero. Written as an `if` rather than a
        // `&& then_some` chain: the short-circuit arm returns `bool` while the other returns
        // `Option<u8>`, and the compiler picks neither for the whole expression.
        if host.starts_with(':') && host.split(':').count() > 2 {
            return Some(0);
        }
        return None;
    }
    if head.len() > 4 {
        return None;
    }
    u16::from_str_radix(head, 16)
        .ok()
        .map(|value| (value >> 8) as u8)
}

/// The shared HTTP client for local endpoints, with redirects refused.
///
/// **Separate from the shared client on purpose.** [`crate::client::chat`] uses a pooled client
/// with reqwest's default redirect policy, which is right for a provider the operator configured
/// and wrong for a local one: the whole point of a local endpoint is that the platform knows where
/// the bytes go, and following a `302` means the destination is a host nobody classified. Two
/// clients, two policies, no shared state to get wrong.
pub fn local_http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        // The refusal itself, not a silent non-follow.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Turn a `3xx` answer into a refusal that names where it pointed.
///
/// A provider that redirects is normal; a *local* provider that redirects off-box is the one
/// event that must never be followed silently, so the caller hands the location here rather than
/// logging it and continuing.
pub fn refuse_redirect(url: &str, location: &str) -> AiHubError {
    AiHubError::InvalidProvider(format!(
        "the local endpoint `{url}` answered a redirect to `{location}`. A local endpoint must \
         serve the request itself: Omnion does not follow redirects from a local URL, because the \
         destination is a host no locality check has classified. Point the base URL at the address \
         that actually serves the API."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn loopback_names_and_containers_are_local() {
        for host in [
            "localhost",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
            "host.docker.internal",
            "0:0:0:0:0:0:0:1",
        ] {
            let kind = classify_host(host, &[]).expect("not a metadata host");
            assert_eq!(kind, Some(HostKind::Loopback), "{host} should be loopback");
        }
    }

    #[test]
    fn private_ranges_are_local_and_their_neighbours_are_not() {
        for host in [
            "10.0.0.5",
            "172.16.0.1",
            "172.20.10.1",
            "172.31.255.254",
            "192.168.1.10",
            "100.64.0.7",
            "fd00::1",
            "fcab::5",
        ] {
            let kind = classify_host(host, &[]).expect("not a metadata host");
            assert_eq!(kind, Some(HostKind::Private), "{host} should be private");
        }
        // The boundaries are where a hand-rolled range check usually slips: 172.15 and 172.32
        // are outside 172.16/12, and 100.128 is outside the CGNAT range.
        for host in [
            "172.15.0.1",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "11.0.0.1",
        ] {
            assert_eq!(
                classify_host(host, &[]).expect("not a metadata host"),
                None,
                "{host} is public and must not be admitted"
            );
        }
    }

    #[test]
    fn public_hosts_are_not_local() {
        for host in ["api.openai.com", "1.1.1.1", "8.8.8.8", "ollama.example.com"] {
            assert_eq!(
                classify_host(host, &[]).expect("not a metadata host"),
                None,
                "{host} is public"
            );
        }
    }

    #[test]
    fn metadata_endpoints_are_refused_rather_than_merely_demoted() {
        // Link-local, so a range check would call it private. It is refused with a reason, which
        // is what lets the endpoint form show a fixable field error.
        let err = classify_host("169.254.169.254", &[]).expect_err("metadata must be refused");
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
        assert!(err.to_string().contains("metadata"), "{err}");
    }

    #[test]
    fn the_allow_list_widens_the_rules_and_does_not_replace_them() {
        let hosts = allow(&["ai.corp.internal"]);
        assert_eq!(
            classify_host("ai.corp.internal", &hosts).expect("not a metadata host"),
            Some(HostKind::Allowlisted)
        );
        // Case-insensitive, and whitespace around an operator-typed entry is tolerated.
        let padded = allow(&["  AI.Corp.Internal "]);
        assert_eq!(
            classify_host("ai.corp.internal", &padded).expect("not a metadata host"),
            Some(HostKind::Allowlisted)
        );
        // The built-in rules still work with a non-empty allow-list: the third rule widens.
        assert_eq!(
            classify_host("192.168.5.5", &hosts).expect("not a metadata host"),
            Some(HostKind::Private)
        );
        // And an unrelated public host is still refused by the allow-list being narrow.
        assert_eq!(
            classify_host("evil.example.com", &hosts).expect("not a metadata host"),
            None
        );
    }

    #[test]
    fn host_of_strips_scheme_port_and_credentials() {
        assert_eq!(
            host_of("http://127.0.0.1:11434/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            host_of("https://ai.corp.internal/v1").as_deref(),
            Some("ai.corp.internal")
        );
        assert_eq!(
            host_of("http://user:pass@ollama.lan:8080/v1").as_deref(),
            Some("ollama.lan")
        );
        assert_eq!(
            host_of("HTTP://Example.COM/v1").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:11434/v1").as_deref(), Some("::1"));
        assert_eq!(host_of("http://[fc00::1]:8080").as_deref(), Some("fc00::1"));
        // Not a network URL: no host to classify, so the answer is None rather than a guess.
        assert_eq!(host_of("/v1"), None);
        assert_eq!(host_of("file:///tmp/model"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn odd_addresses_are_not_treated_as_private() {
        // A leading-zero octet means two different addresses depending on the parser, so the
        // answer is "not private" rather than a coin toss.
        assert_eq!(
            classify_host("010.0.0.1", &[]).expect("not a metadata host"),
            None
        );
        assert_eq!(
            classify_host("192.168.1", &[]).expect("not a metadata host"),
            None
        );
        assert_eq!(
            classify_host("192.168.1.1.1", &[]).expect("not a metadata host"),
            None
        );
        assert_eq!(
            classify_host("192.168.1.256", &[]).expect("not a metadata host"),
            None
        );
        assert_eq!(
            classify_host("192.168.01.1", &[]).expect("not a metadata host"),
            None
        );
    }

    #[test]
    fn a_redirect_refusal_names_the_target() {
        let err = refuse_redirect("http://127.0.0.1:11434/v1", "https://api.openai.com/v1");
        let text = err.to_string();
        assert!(text.contains("api.openai.com"), "{text}");
        assert!(text.contains("127.0.0.1"), "{text}");
        assert!(text.contains("does not follow redirects"), "{text}");
    }

    #[tokio::test]
    async fn the_local_client_refuses_redirects() {
        // The policy is the control, so it is proven by **behaviour**: a listener that answers
        // `302` must not be followed, and the caller must be able to turn the answer into a
        // refusal that names the destination. Asserting that the builder was told `none` would
        // pass on a client that ignored the setting, which is the exact regression this guards.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let port = listener.local_addr().expect("a bound address").port();
        tokio::spawn(async move {
            // One request, answered with a redirect to a name that is not this listener.
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = "redirecting";
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: https://api.openai.com/v1\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/models");
        let answer = local_http().get(&url).send().await;
        let answer = answer.expect("the local client reaches its own listener");

        assert_eq!(
            answer.status(),
            302,
            "the redirect answer must survive to the caller"
        );
        // And the caller has everything it needs to refuse with a name in the message.
        let location = answer
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("a 302 carries Location");
        let refusal = refuse_redirect(&url, location).to_string();
        assert!(refusal.contains("api.openai.com"), "{refusal}");
    }
}
