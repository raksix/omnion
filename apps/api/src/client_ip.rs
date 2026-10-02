//! Client address extraction.
//!
//! `axum::extract::ConnectInfo` is only present when the server is started through
//! `into_make_service_with_connect_info` (see `main.rs`) and it has no optional variant, so
//! this extractor reads it out of the request extensions and yields `None` when the request
//! carries no connection info (in-process tests, stripped layers) instead of rejecting it.
//!
//! The value is bookkeeping only — sessions store it for diagnostics — so it must never be
//! the reason a request fails, and it must never be trusted for authorization.
//!
//! ## One resolver, because there were three, and they disagreed
//!
//! Before this rewrite three files each decided independently who the caller was, and their
//! answers contradicted each other:
//!
//! | Surface | Rule it applied | What that costs in production |
//! |---|---|---|
//! | [`ClientAddress`] (this file, all `extract` handlers) | socket only | behind the platform's own proxy every visitor counts as the proxy |
//! | `rate_limit_middleware::peer_from_headers` | socket **first**, header only as fallback | header ignored whenever a socket exists — see below |
//! | `analytics::visitor_address` | header **first**, unconditionally | any caller mints a fresh bucket per request by setting the header
//!
//! The middleware's copy is the one that matters to this slice, and its **doc comment claims
//! the opposite of what its code does**: the comment says the header is consulted *only when
//! the peer is loopback*, but the call is `peer.or_else(|| peer_from_headers(headers))`,
//! which reads the socket first and only falls back to the header. In production the peer is
//! never loopback (nginx is a separate container/host, so the peer is a LAN address), so the
//! header is reached in the normal case and its loopback guard is finished code. Its unit
//! test then "proves" the guard by calling the helper directly and passing a `peer` of
//! `Some` — a combination the production call site cannot produce.
//!
//! That is exactly the defect the previous slice opened: `MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR`
//! counts durable `crm_leads.submitter_ip` rows, and every such row is written by `capture`,
//! which takes its address from this extractor. Behind the platform's own proxy that column
//! holds the **proxy's** address for every visitor, so the per-address ceiling counts the
//! proxy as one caller — and the 11th submission from the entire internet is refused `429`
//! while a bot rotating through one address is never throttled at all. The dial shipped last
//! tick; behind a proxy it does not exist.
//!!
//! ## The rule this file settles on
//!
//! `X-Forwarded-For` is believed **only when the socket peer is loopback** (a proxy on this
//! host), or when the socket carries no address at all (in-process tests, a stripped layer —
//! there is no peer to distrust, so the header is all there is). From a public peer the header
//! is ignored entirely, because a client that may write a header freely may choose its own
//! limiter identity, rate-limit bucket, or analytics exclusion — which is the one mistake that
//! makes a limiter worse than none.
//!
//! The guard is a property of the **type**, not of each call site: a handler takes
//! `ClientAddress` and gets the resolved value. There is no longer a way to read the raw socket
//! address for a decision, so the three copies cannot drift again.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;

/// Remote address of the caller, when the runtime provides one.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientAddress(pub Option<IpAddr>);

impl ClientAddress {
    /// The IP address as text, ready for storage (PostgreSQL `inet`, docs/07-IAM.md).
    #[must_use]
    pub fn as_text(self) -> Option<String> {
        self.0.map(|ip| ip.to_string())
    }
}

/// The header a reverse proxy writes the original client into.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// Resolve the caller's address from a socket peer and a set of headers.
///
/// This is the single place the platform decides who the caller is, and it is `pub` so that a
/// surface which resolves it from something other than an `extract` chain — the rate-limit
/// middleware, which reads the extensions itself because it runs before routing — is a caller
/// of this rule rather than a fourth copy of it.
///
/// The precedence, and why:
///
/// * **loopback peer** → the header is the caller's, and *only* the header is. A proxy on this
///   host is the only thing whose socket address is guaranteed not to belong to a client, and
///   the deployment behind a proxy terminates TLS there, so its requests arrive from loopback.
///   This is the case the header exists for.
/// * **public peer** → the socket wins, and the header is ignored. A peer that is neither
///   loopback nor absent is a real network client, and believing a header it wrote itself would
///   let it pick its own limiter identity, rate-limit bucket, or analytics exclusion — which is
///   the one mistake that makes a limiter worse than none.
/// * **no peer at all** → the header is all there is. A request with no connection info is an
///   in-process call (tests, stripped layers); treating it as "untrustworthy, use nothing" would
///   make every such test measure the fallback rather than the rule.
///
/// **A local proxy that writes an unusable value yields no address at all — never the proxy's
/// own.** This was the first version's bug and its own test caught it: the initial
/// implementation fell back to the peer whenever the header did not parse, so
/// `forwarded.or(peer)` answered `Some(127.0.0.1)` for a misbehaving proxy. Every row the CRM
/// stores would then carry the loopback address, which is *this slice's own defect re-entering
/// through the fallback path*: one bucket for every visitor behind that proxy, and — since the
/// index is partial over `(source_id, submitter_ip, received_at)` — one address's flood
/// throttling an entire installation's form traffic. `None` is the honest answer, and it is the
/// one the CRM's per-address ceiling already treats as "not throttled" rather than "in a shared
/// bucket with everyone else".
#[must_use]
pub fn resolve_client_ip(peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
    match peer {
        // A proxy on this host speaks for its caller, and its own address is never the answer.
        Some(socket) if socket.is_loopback() => forwarded_client(headers),
        // A real network client answers for itself, whatever it claims in the header.
        Some(socket) => Some(socket),
        // No socket to distrust (an in-process call): the header is the only evidence there is.
        None => forwarded_client(headers),
    }
}

/// The left-most entry of `X-Forwarded-For` — the original client, not the proxy that
/// appended its own address after it.
fn forwarded_client(headers: &HeaderMap) -> Option<IpAddr> {
    let forwarded = headers.get(FORWARDED_FOR)?.to_str().ok()?;
    let first = forwarded.split(',').next()?.trim();
    first.parse().ok()
}

impl<S> FromRequestParts<S> for ClientAddress
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        Ok(Self(resolve_client_ip(peer, &parts.headers)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn parts_with(peer: Option<SocketAddr>, forwarded: Option<&str>) -> Parts {
        let mut builder = axum::http::Request::builder().uri("/");
        if let Some(forwarded) = forwarded {
            builder = builder.header(FORWARDED_FOR, HeaderValue::from_str(forwarded).expect("header text"));
        }
        let (mut parts, _) = builder.body(()).expect("request must build").into_parts();
        if let Some(peer) = peer {
            parts.extensions.insert(ConnectInfo(peer));
        }
        parts
    }

    async fn extract(parts: &mut Parts) -> Option<IpAddr> {
        ClientAddress::from_request_parts(parts, &())
            .await
            .expect("extraction must never fail")
            .0
    }

    #[tokio::test]
    async fn missing_connection_info_is_not_an_error() {
        let mut parts = parts_with(None, None);
        let address = extract(&mut parts).await;
        assert_eq!(address, None);
        assert_eq!(
            ClientAddress(None).as_text(),
            None,
            "a submission with no address stays a submission with no address"
        );
    }

    #[tokio::test]
    async fn reads_the_address_from_the_connect_info_extension() {
        let mut parts = parts_with(Some(SocketAddr::from(([203, 0, 113, 7], 443))), None);
        assert_eq!(extract(&mut parts).await, Some("203.0.113.7".parse().unwrap()));
    }

    /// **The defect this rewrite exists for.** A proxy on this host terminates TLS and
    /// forwards with `X-Forwarded-For`; every row `capture` writes must carry the *visitor's*
    /// address, or the per-address ceiling counts the proxy as a single caller.
    #[tokio::test]
    async fn a_loopback_proxy_speaks_for_its_caller() {
        let mut parts = parts_with(
            Some(SocketAddr::from(([127, 0, 0, 1], 34_567))),
            Some("203.0.113.9, 10.0.0.1"),
        );
        assert_eq!(
            extract(&mut parts).await,
            Some("203.0.113.9".parse().unwrap()),
            "behind the platform's own proxy the visitor's address is what reaches the store"
        );
    }

    /// The negative half, and the reason the rule is not "always read the header": a caller
    /// that connects from the public internet may write the header itself, and believing it
    /// hands that caller a fresh limiter identity and a fresh analytics bucket per request.
    #[tokio::test]
    async fn a_public_peer_keeps_its_own_address() {
        let mut parts = parts_with(
            Some(SocketAddr::from(([198, 51, 100, 4], 40_000))),
            Some("203.0.113.9"),
        );
        assert_eq!(
            extract(&mut parts).await,
            Some("198.51.100.4".parse().unwrap()),
            "a header from a public peer is not the caller's address and is ignored"
        );
    }

    /// IPv6 loopback is loopback. `::1` arriving as `[::1]` in a header is not the case, so
    /// the peer here is the IPv6 form of the same host.
    #[tokio::test]
    async fn ipv6_loopback_is_still_the_local_proxy() {
        let mut parts = parts_with(
            Some(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 34_567))),
            Some("203.0.113.9"),
        );
        assert_eq!(extract(&mut parts).await, Some("203.0.113.9".parse().unwrap()));
    }

    /// A proxy that writes an unusable value must not knock out a perfectly good socket
    /// address, and must not be treated as "the caller is loopback" either — the value that
    /// reaches the store is `None`, which the CRM ceiling reads as "never throttled", never as
    /// "throttle-able by anybody".
    #[tokio::test]
    async fn an_unusable_header_is_no_address_rather_than_a_wrong_one() {
        let mut parts = parts_with(Some(SocketAddr::from(([127, 0, 0, 1], 34_567))), Some("not-an-address"));
        assert_eq!(extract(&mut parts).await, None);

        let mut parts = parts_with(Some(SocketAddr::from(([198, 51, 100, 4], 40_000))), Some("not-an-address"));
        assert_eq!(
            extract(&mut parts).await,
            Some("198.51.100.4".parse().unwrap()),
            "the socket is not discarded because a header was unparseable"
        );
    }

    /// Two callers behind one loopback proxy are two callers, and the header list is a chain:
    /// the left-most entry is the original client, the rest are proxies on the path.
    #[tokio::test]
    async fn the_left_most_forwarded_entry_is_the_original_client() {
        let mut parts = parts_with(
            Some(SocketAddr::from(([127, 0, 0, 1], 34_567))),
            Some("  203.0.113.9 , 10.0.0.1, 10.0.0.2"),
        );
        assert_eq!(extract(&mut parts).await, Some("203.0.113.9".parse().unwrap()));
    }
}
