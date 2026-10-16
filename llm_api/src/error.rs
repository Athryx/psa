//! Crate-wide errors, also suitable for persisting failed model outcomes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Errors are normalized to owned data so they can be cloned into response
/// state and serialized. Conversions retain messages, not native error sources.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    #[error("unsupported feature: {feature}")]
    Unsupported { feature: String },

    #[error("transport error: {message}")]
    Transport { message: String },

    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        /// Parsed Retry-After delay, when supplied by the server.
        retry_after_seconds: Option<u64>,
    },

    #[error("JSON error: {message}")]
    Json { message: String },

    #[error("stream protocol error: {message}")]
    Protocol { message: String },

    #[error("provider error: {message}")]
    Provider {
        code: Option<String>,
        message: String,
        metadata: BTreeMap<String, Value>,
    },
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        // Avoid retaining credentials or sensitive query parameters in URLs.
        let error = error.without_url();
        let message = error.to_string();
        if let Some(status) = error.status() {
            Self::Http {
                status: status.as_u16(),
                message,
                retry_after_seconds: None,
            }
        } else {
            Self::Transport { message }
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn converts_json_errors() {
        let error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(matches!(Error::from(error), Error::Json { .. }));
    }

    #[test]
    fn errors_round_trip() {
        let error = Error::Http {
            status: 429,
            message: "rate limited".into(),
            retry_after_seconds: Some(10),
        };
        let json = serde_json::to_string(&error).unwrap();
        let restored: Error = serde_json::from_str(&json).unwrap();
        assert_eq!(error.to_string(), restored.to_string());
        assert!(matches!(
            restored,
            Error::Http {
                status: 429,
                retry_after_seconds: Some(10),
                ..
            }
        ));
    }
}
