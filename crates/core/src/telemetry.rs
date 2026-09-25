//! Structured logging bootstrap.
//!
//! Omnion logs through `tracing`, so applications, workers and the CLI all emit the same
//! shape of events. Development runs get human-readable lines; production runs get JSON so
//! a collector can ship them (docs/02-ARCHITECTURE.md, "Observability").
//!
//! OpenTelemetry hook: the trace exporter attaches as one more `tracing` layer in the
//! observability phase — [`Telemetry::shutdown`] is the place that will flush it on
//! graceful shutdown.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::{LogConfig, LogFormat};
use crate::error::CoreError;

/// Boot-time handle for the telemetry pipeline.
///
/// Dropping it is harmless; call [`Telemetry::shutdown`] during graceful shutdown so
/// buffered exporters can flush once they exist.
#[derive(Debug, Default)]
pub struct Telemetry {
    _private: (),
}

impl Telemetry {
    /// Flush the telemetry pipeline.
    ///
    /// The current pipeline writes through the local `tracing` subscriber, which needs no
    /// flush; when the OpenTelemetry layer lands, this method shuts its provider down.
    pub fn shutdown(self) {}
}

/// Install the global tracing subscriber described by `config`.
pub fn init(config: &LogConfig) -> Result<Telemetry, CoreError> {
    let filter = EnvFilter::try_new(&config.filter).map_err(|err| {
        CoreError::Telemetry(format!("invalid log filter {:?}: {err}", config.filter))
    })?;
    let registry = tracing_subscriber::registry().with(filter);
    let installed = match config.format {
        LogFormat::Json => registry.with(fmt::layer().json()).try_init(),
        LogFormat::Pretty => registry.with(fmt::layer()).try_init(),
    };
    installed
        .map_err(|err| CoreError::Telemetry(format!("subscriber already installed: {err}")))?;
    Ok(Telemetry::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_filter_is_rejected() {
        let config = LogConfig::new("info,omnion[=info", LogFormat::Pretty);
        let error = init(&config).expect_err("malformed filter must be rejected");
        assert!(matches!(error, CoreError::Telemetry(_)));
    }

    #[test]
    fn shutdown_is_safe_to_call() {
        Telemetry::default().shutdown();
    }
}
