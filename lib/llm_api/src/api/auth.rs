//! Request authentication: a bearer credential plus provider-specific headers.

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

use crate::Error;

/// Headers that authenticate every request a client sends. Values are
/// validated when a client is built and marked sensitive. Plain strings
/// convert into a bearer-only `Auth`.
///
/// Intentionally does not implement Debug: it holds secrets.
#[derive(Clone)]
pub struct Auth {
    bearer: String,
    headers: Vec<(String, String)>,
}

impl Auth {
    /// Send `Authorization: Bearer <token>`.
    pub fn bearer(token: impl Into<String>) -> Self {
        Self {
            bearer: token.into(),
            headers: Vec::new(),
        }
    }

    /// Add another header sent with every request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub(super) fn build_headers(&self) -> Result<HeaderMap, Error> {
        let key = &self.bearer;
        if key.is_empty() || key.trim() != key {
            return Err(Error::InvalidConfig {
                field: "api_key".into(),
                message: "key must be nonempty without surrounding whitespace".into(),
            });
        }

        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| Error::InvalidConfig {
                field: "api_key".into(),
                message: "key contains invalid header characters".into(),
            })?;
        authorization.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        for (name, value) in &self.headers {
            let invalid = || Error::InvalidConfig {
                field: "auth_headers".into(),
                message: format!("invalid authentication header {name:?}"),
            };
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
            if name == AUTHORIZATION || value.is_empty() {
                return Err(invalid());
            }
            let mut value = HeaderValue::from_str(value).map_err(|_| invalid())?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }

        Ok(headers)
    }
}

impl From<String> for Auth {
    fn from(token: String) -> Self {
        Self::bearer(token)
    }
}

impl From<&str> for Auth {
    fn from(token: &str) -> Self {
        Self::bearer(token)
    }
}
