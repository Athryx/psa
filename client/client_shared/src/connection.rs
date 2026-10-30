use serde::{Deserialize, Serialize};
use std::time::Duration;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    AuthenticationFailed,
}
impl ConnectionState {
    /// Only connected summaries can be presented as confidently idle.
    pub fn is_live(self) -> bool {
        self == Self::Connected
    }
}
/// Runtime-independent reconnect schedule; caller owns asynchronous sleep/cancellation.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    attempt: u32,
    pub initial: Duration,
    pub maximum: Duration,
}
impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            attempt: 0,
            initial: Duration::from_millis(250),
            maximum: Duration::from_secs(30),
        }
    }
}
impl ReconnectBackoff {
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    pub fn next_delay(&mut self) -> Duration {
        let factor = 1_u32.checked_shl(self.attempt.min(31)).unwrap_or(u32::MAX);
        self.attempt = self.attempt.saturating_add(1);
        self.initial.saturating_mul(factor).min(self.maximum)
    }
}
/// Retry only subscription/read operations. Never retry ambiguous message submissions.
pub fn can_retry_connection(error: &psa_api::ClientError) -> bool {
    match error {
        psa_api::ClientError::Http(_) | psa_api::ClientError::Stream(_) => true,
        psa_api::ClientError::Api { status, .. } => {
            *status == 408 || *status == 429 || *status >= 500
        }
        _ => false,
    }
}
/// Shared draft ownership is transcript-scoped, deliberately not serializable.
#[derive(Debug, Clone, Default)]
pub struct Drafts {
    values: std::collections::HashMap<crate::TranscriptTarget, String>,
}
impl Drafts {
    pub fn get(&self, target: crate::TranscriptTarget) -> &str {
        self.values.get(&target).map(String::as_str).unwrap_or("")
    }
    pub fn set(&mut self, target: crate::TranscriptTarget, value: String) {
        if value.is_empty() {
            self.values.remove(&target);
        } else {
            self.values.insert(target, value);
        }
    }
    pub fn remove(&mut self, target: crate::TranscriptTarget) -> Option<String> {
        self.values.remove(&target)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_backoff_and_reset() {
        let mut b = ReconnectBackoff::default();
        assert_eq!(b.next_delay(), Duration::from_millis(250));
        for _ in 0..100 {
            assert!(b.next_delay() <= Duration::from_secs(30));
        }
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(250));
    }
}
