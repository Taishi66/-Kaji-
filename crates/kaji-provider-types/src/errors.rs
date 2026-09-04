use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

use crate::conversation::message::MessageErrorKind;
use crate::request_log::LogError;

/// `Serialize`/`Deserialize` : le journal du replay écrit la variante exacte
/// d'un appel provider qui a échoué, pour que le rejeu prenne le même bras de
/// `match` que l'enregistrement (`replay::provider`).
#[derive(Error, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderError {
    #[error("Provider is not configured")]
    NotConfigured,

    #[error("Authentication error: {0}")]
    Authentication(String),

    #[error("Context length exceeded: {0}")]
    ContextLengthExceeded(String),

    #[error("Rate limit exceeded: {details}")]
    RateLimitExceeded {
        details: String,
        retry_delay: Option<Duration>,
    },

    #[error("Server error: {0}")]
    ServerError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Request failed: {0}")]
    RequestFailed(String),

    #[error("Execution error: {0}")]
    ExecutionError(String),

    #[error("Usage data error: {0}")]
    UsageError(String),

    #[error("Unsupported operation: {0}")]
    NotImplemented(String),

    #[error("Endpoint not found (404): {0}")]
    EndpointNotFound(String),

    #[error("Credits exhausted: {details}")]
    CreditsExhausted {
        details: String,
        top_up_url: Option<String>,
    },

    #[error("Provider refused request: {details}")]
    Refusal {
        details: String,
        category: Option<String>,
    },
}

impl ProviderError {
    pub fn stream_decode_error(error: impl std::fmt::Display) -> Self {
        ProviderError::NetworkError(format!("Stream decode error: {error}"))
    }

    pub fn kind(&self) -> MessageErrorKind {
        match self {
            ProviderError::NotConfigured => MessageErrorKind::NotConfigured,
            ProviderError::Authentication(_) => MessageErrorKind::Authentication,
            ProviderError::ContextLengthExceeded(_) => MessageErrorKind::ContextLengthExceeded,
            ProviderError::RateLimitExceeded { .. } => MessageErrorKind::RateLimited,
            ProviderError::ServerError(_) => MessageErrorKind::ServerError,
            ProviderError::NetworkError(_) => MessageErrorKind::Network,
            ProviderError::RequestFailed(_) => MessageErrorKind::InvalidRequest,
            ProviderError::ExecutionError(_) => MessageErrorKind::Execution,
            ProviderError::UsageError(_) => MessageErrorKind::Usage,
            ProviderError::NotImplemented(_) => MessageErrorKind::NotImplemented,
            ProviderError::EndpointNotFound(_) => MessageErrorKind::EndpointNotFound,
            ProviderError::CreditsExhausted { .. } => MessageErrorKind::CreditsExhausted,
            ProviderError::Refusal { .. } => MessageErrorKind::Refusal,
        }
    }

    pub fn telemetry_type(&self) -> &'static str {
        self.kind().as_str()
    }

    pub fn is_endpoint_not_found(&self) -> bool {
        matches!(self, ProviderError::EndpointNotFound(_))
    }

    /// Recover a typed `ProviderError` from a streaming decode error, falling
    /// back to a retryable stream decode error for errors that did not
    /// originate as one.
    pub fn from_stream_error(error: anyhow::Error) -> Self {
        error
            .downcast()
            .unwrap_or_else(ProviderError::stream_decode_error)
    }
}

fn is_network_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || (err.status().is_none() && err.is_request())
}

fn provider_error_from_reqwest(error: &reqwest::Error) -> ProviderError {
    if is_network_error(error) {
        let msg = if error.is_timeout() {
            "Request timed out — check your network connection and try again.".to_string()
        } else if error.is_connect() {
            if let Some(url) = error.url() {
                if let Some(host) = url.host_str() {
                    let port_info = url.port().map(|p| format!(":{}", p)).unwrap_or_default();
                    format!(
                        "Could not connect to {}{} — check your network connection and try again.",
                        host, port_info
                    )
                } else {
                    "Could not connect to the provider — check your network connection and try again.".to_string()
                }
            } else {
                "Could not connect to the provider — check your network connection and try again."
                    .to_string()
            }
        } else {
            "Network error — check your network connection and try again.".to_string()
        };
        return ProviderError::NetworkError(msg);
    }

    let mut details = vec![];
    if let Some(status) = error.status() {
        details.push(format!("status: {}", status));
    }
    let msg = if details.is_empty() {
        error.to_string()
    } else {
        format!("{} ({})", error, details.join(", "))
    };
    ProviderError::RequestFailed(msg)
}

impl From<anyhow::Error> for ProviderError {
    fn from(error: anyhow::Error) -> Self {
        if let Some(provider_error) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ProviderError>())
        {
            return provider_error.clone();
        }
        if let Some(reqwest_err) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        {
            return provider_error_from_reqwest(reqwest_err);
        }
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        }) {
            return ProviderError::NetworkError(
                "Request timed out — check your network connection and try again.".to_string(),
            );
        }
        ProviderError::ExecutionError(error.to_string())
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(error: reqwest::Error) -> Self {
        provider_error_from_reqwest(&error)
    }
}

impl From<LogError> for ProviderError {
    fn from(value: LogError) -> Self {
        ProviderError::ExecutionError(value.to_string())
    }
}

#[derive(Debug)]
pub enum GoogleErrorCode {
    BadRequest = 400,
    Unauthorized = 401,
    Forbidden = 403,
    NotFound = 404,
    TooManyRequests = 429,
    InternalServerError = 500,
    ServiceUnavailable = 503,
}

impl GoogleErrorCode {
    pub fn to_status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn from_code(code: u64) -> Option<Self> {
        match code {
            400 => Some(Self::BadRequest),
            401 => Some(Self::Unauthorized),
            403 => Some(Self::Forbidden),
            404 => Some(Self::NotFound),
            429 => Some(Self::TooManyRequests),
            500 => Some(Self::InternalServerError),
            503 => Some(Self::ServiceUnavailable),
            _ => Some(Self::InternalServerError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<(ProviderError, MessageErrorKind)> {
        vec![
            (
                ProviderError::NotConfigured,
                MessageErrorKind::NotConfigured,
            ),
            (
                ProviderError::Authentication("bad key".to_string()),
                MessageErrorKind::Authentication,
            ),
            (
                ProviderError::ContextLengthExceeded("too long".to_string()),
                MessageErrorKind::ContextLengthExceeded,
            ),
            (
                ProviderError::RateLimitExceeded {
                    details: "slow down".to_string(),
                    retry_delay: Some(Duration::from_secs(1)),
                },
                MessageErrorKind::RateLimited,
            ),
            (
                ProviderError::ServerError("boom".to_string()),
                MessageErrorKind::ServerError,
            ),
            (
                ProviderError::NetworkError("offline".to_string()),
                MessageErrorKind::Network,
            ),
            (
                ProviderError::RequestFailed("bad payload".to_string()),
                MessageErrorKind::InvalidRequest,
            ),
            (
                ProviderError::ExecutionError("panic".to_string()),
                MessageErrorKind::Execution,
            ),
            (
                ProviderError::UsageError("no usage".to_string()),
                MessageErrorKind::Usage,
            ),
            (
                ProviderError::NotImplemented("no tools".to_string()),
                MessageErrorKind::NotImplemented,
            ),
            (
                ProviderError::EndpointNotFound("/v1/chat".to_string()),
                MessageErrorKind::EndpointNotFound,
            ),
            (
                ProviderError::CreditsExhausted {
                    details: "empty".to_string(),
                    top_up_url: None,
                },
                MessageErrorKind::CreditsExhausted,
            ),
            (
                ProviderError::Refusal {
                    details: "nope".to_string(),
                    category: Some("safety".to_string()),
                },
                MessageErrorKind::Refusal,
            ),
        ]
    }

    #[test]
    fn every_provider_error_maps_to_a_kind() {
        for (error, expected) in every_variant() {
            assert_eq!(error.kind(), expected, "{error}");
        }
    }

    #[test]
    fn telemetry_type_is_the_kind_wire_name() {
        for (error, expected) in every_variant() {
            assert_eq!(error.telemetry_type(), expected.as_str(), "{error}");
        }
    }
}
