//! Omnion API binary entrypoint.

use std::net::SocketAddr;

use tokio::net::TcpListener;

/// Port used when the `PORT` environment variable is not set.
const DEFAULT_PORT: u16 = 8080;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = match std::env::var("PORT") {
        Ok(raw) => raw
            .parse::<u16>()
            .map_err(|_| format!("PORT must be a valid port number, got {raw:?}"))?,
        Err(_) => DEFAULT_PORT,
    };

    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(address).await?;

    println!("omnion-api listening on http://{address}");

    axum::serve(listener, omnion_api::routes::router()).await?;

    Ok(())
}
