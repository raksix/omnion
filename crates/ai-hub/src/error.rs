//! Errors of the AI Hub.

/// What can go wrong while the AI Hub talks to a provider — or while the platform reads its own
/// rows.
///
/// The variants are the taxonomy the API surface maps onto HTTP: a provider that was never
/// connected (`ProviderNotFound`), one the operator switched off (`ProviderDisabled`), a model
/// the registry does not carry (`ModelNotFound`), an installation without a default model
/// (`NoDefaultModel`), a request the platform (or the provider) refuses before the model sees it
/// (`Invalid*`), and the two ways a provider can fail at the far end — it did not answer at all
/// (`Transport`) or it answered with a refusal (`Upstream`).
#[derive(Debug, thiserror::Error)]
pub enum AiHubError {
    /// The database refused the read or the write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// No provider with that id.
    #[error("no AI provider carries that id")]
    ProviderNotFound,
    /// Another provider already uses the name.
    #[error("an AI provider named \"{0}\" already exists")]
    ProviderNameTaken(String),
    /// No model with that id, or no model with that key on that provider.
    #[error("no AI model matches that request")]
    ModelNotFound,
    /// The installation has no (enabled) default model to route to.
    #[error("no default AI model is configured")]
    NoDefaultModel,
    /// The resolved provider is switched off.
    #[error("the AI provider \"{0}\" is disabled")]
    ProviderDisabled(String),
    /// The provider definition is unusable.
    #[error("invalid provider: {0}")]
    InvalidProvider(String),
    /// The model definition (or the model selection) is unusable.
    #[error("invalid model: {0}")]
    InvalidModel(String),
    /// The chat request itself is unusable.
    #[error("invalid chat request: {0}")]
    InvalidChatRequest(String),
    /// The provider could not be reached.
    #[error("the AI provider did not answer: {0}")]
    Transport(String),
    /// The provider answered, but refused the request.
    #[error("the AI provider answered with status {status}: {message}")]
    Upstream {
        /// HTTP status the provider answered with.
        status: u16,
        /// Provider message, trimmed to a sane length.
        message: String,
    },
    /// The answer stream broke halfway through.
    #[error("the AI provider's answer stream failed: {0}")]
    Stream(String),
    /// The provider answered with something the platform cannot read.
    #[error("the AI provider answered with an unusable body: {0}")]
    Malformed(String),
}

impl AiHubError {
    /// `true` when the failure happened at the provider, not inside Omnion.
    #[must_use]
    pub fn is_upstream(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Upstream { .. } | Self::Stream(_) | Self::Malformed(_)
        )
    }

    /// Stable, machine-readable code for this failure.
    ///
    /// The API puts it in its error bodies and in the `error` frame of a chat stream, so a
    /// client can branch on the reason without reading prose.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::ProviderNotFound => "provider_not_found",
            Self::ProviderNameTaken(_) => "provider_name_taken",
            Self::ModelNotFound => "model_not_found",
            Self::NoDefaultModel => "no_default_model",
            Self::ProviderDisabled(_) => "provider_disabled",
            Self::InvalidProvider(_) => "invalid_provider",
            Self::InvalidModel(_) => "invalid_model",
            Self::InvalidChatRequest(_) => "invalid_chat_request",
            Self::Transport(_) => "provider_unreachable",
            Self::Upstream { .. } => "provider_error",
            Self::Stream(_) => "stream_failed",
            Self::Malformed(_) => "provider_malformed",
        }
    }
}

/// Result alias of the AI Hub.
pub type Result<T> = std::result::Result<T, AiHubError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_failures_are_recognised() {
        assert!(AiHubError::Transport("connect refused".to_owned()).is_upstream());
        assert!(
            AiHubError::Upstream {
                status: 429,
                message: "slow down".to_owned()
            }
            .is_upstream()
        );
        assert!(AiHubError::Stream("half a frame".to_owned()).is_upstream());
        assert!(!AiHubError::ProviderNotFound.is_upstream());
        assert!(!AiHubError::NoDefaultModel.is_upstream());
    }

    #[test]
    fn messages_carry_the_provider_detail() {
        let error = AiHubError::Upstream {
            status: 401,
            message: "bad key".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "the AI provider answered with status 401: bad key"
        );
    }
}
