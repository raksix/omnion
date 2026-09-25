//! Client address extraction.
//!
//! `axum::extract::ConnectInfo` is only present when the server is started through
//! `into_make_service_with_connect_info` (see `main.rs`) and it has no optional variant, so
//! this extractor reads it out of the request extensions and yields `None` when the request
//! carries no connection info (in-process tests, stripped layers) instead of rejecting it.
//!
//! The value is bookkeeping only — sessions store it for diagnostics — so it must never be
//! the reason a request fails, and it must never be trusted for authorization.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
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

impl<S> FromRequestParts<S> for ClientAddress
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let address = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        Ok(Self(address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_connection_info_is_not_an_error() {
        let (mut parts, _) = axum::http::Request::builder()
            .uri("/")
            .body(())
            .expect("request must build")
            .into_parts();

        let address = ClientAddress::from_request_parts(&mut parts, &())
            .await
            .expect("extraction must never fail");
        assert_eq!(address.0, None);
        assert_eq!(address.as_text(), None);
    }

    #[tokio::test]
    async fn reads_the_address_from_the_connect_info_extension() {
        let (mut parts, _) = axum::http::Request::builder()
            .uri("/")
            .body(())
            .expect("request must build")
            .into_parts();
        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 443))));

        let address = ClientAddress::from_request_parts(&mut parts, &())
            .await
            .expect("extraction must never fail");
        assert_eq!(address.as_text().as_deref(), Some("203.0.113.7"));
    }
}
