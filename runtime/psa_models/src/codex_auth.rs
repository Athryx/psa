//! ChatGPT OAuth for the Codex subscription provider: device-code sign-in and
//! access-token refresh against `auth.openai.com`, using the Codex CLI's
//! public OAuth client.
//!
//! Errors never include response bodies or tokens.

use base64::Engine;
use psa_config::auth::ProviderCredential;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use time::OffsetDateTime;

pub(crate) const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// How long a device code stays valid.
pub(crate) const DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);
const ACCOUNT_CLAIMS: &str = "https://api.openai.com/auth";

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("could not reach the sign-in service")]
    Transport,
    #[error("the sign-in service returned HTTP {0}")]
    Http(u16),
    #[error("the sign-in service rejected the saved credential; sign in again")]
    Rejected,
    #[error("unexpected response from the sign-in service")]
    Invalid,
}

impl From<reqwest::Error> for OAuthError {
    fn from(_: reqwest::Error) -> Self {
        Self::Transport
    }
}

#[derive(Clone)]
pub(crate) struct CodexOAuth {
    http: reqwest::Client,
    issuer: String,
}

/// A code the user enters at `verification_url` to approve the sign-in.
pub(crate) struct DeviceCode {
    device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
    pub verification_url: String,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

impl CodexOAuth {
    pub(crate) fn new(issuer: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .expect("default HTTP client");
        Self {
            http,
            issuer: issuer.into().trim_end_matches('/').to_owned(),
        }
    }

    /// Ask for a new device code to show to the user.
    pub(crate) async fn request_device_code(&self) -> Result<DeviceCode, OAuthError> {
        let response = self
            .http
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&json!({ "client_id": CLIENT_ID }))
            .send()
            .await?;
        let response = require_success(response)?;
        let code: DeviceCodeResponse = response.json().await.map_err(|_| OAuthError::Invalid)?;

        // The interval may be sent as a number or a numeric string.
        let interval = code
            .interval
            .and_then(|value| match value {
                Value::Number(number) => number.as_u64(),
                Value::String(text) => text.trim().parse().ok(),
                _ => None,
            })
            .unwrap_or(5)
            .clamp(1, 60);
        Ok(DeviceCode {
            device_auth_id: code.device_auth_id,
            user_code: code.user_code,
            interval: Duration::from_secs(interval),
            verification_url: format!("{}/codex/device", self.issuer),
        })
    }

    /// Check whether the user approved the code. `None` means still pending.
    pub(crate) async fn poll_device_code(
        &self,
        code: &DeviceCode,
    ) -> Result<Option<ProviderCredential>, OAuthError> {
        let response = self
            .http
            .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
            .json(&json!({
                "device_auth_id": code.device_auth_id,
                "user_code": code.user_code,
            }))
            .send()
            .await?;
        if matches!(response.status().as_u16(), 403 | 404) {
            return Ok(None);
        }
        let response = require_success(response)?;
        let approved: DeviceTokenResponse =
            response.json().await.map_err(|_| OAuthError::Invalid)?;

        let redirect_uri = format!("{}/deviceauth/callback", self.issuer);
        let response = self
            .http
            .post(format!("{}/oauth/token", self.issuer))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", approved.authorization_code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", CLIENT_ID),
                ("code_verifier", approved.code_verifier.as_str()),
            ])
            .send()
            .await?;
        let tokens = parse_tokens(require_success(response)?).await?;
        build_credential(tokens, None).map(Some)
    }

    /// Exchange a refresh token for a new token set. Refresh tokens are
    /// single-use: the returned credential must be saved before the next call.
    pub(crate) async fn refresh(
        &self,
        current: &ProviderCredential,
    ) -> Result<ProviderCredential, OAuthError> {
        let refresh_token = current
            .refresh_token
            .as_deref()
            .ok_or(OAuthError::Rejected)?;
        let response = self
            .http
            .post(format!("{}/oauth/token", self.issuer))
            .json(&json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await?;
        if matches!(response.status().as_u16(), 400 | 401) {
            return Err(OAuthError::Rejected);
        }
        let tokens = parse_tokens(require_success(response)?).await?;
        build_credential(tokens, Some(current))
    }
}

/// Whether a credential should be refreshed before it is used.
pub(crate) fn needs_refresh(credential: &ProviderCredential, margin: Duration) -> bool {
    let Some(expires_at) = credential.expires_at else {
        return false;
    };
    credential.refresh_token.is_some()
        && expires_at <= OffsetDateTime::now_utc().unix_timestamp() + margin.as_secs() as i64
}

pub(crate) fn is_expired(credential: &ProviderCredential) -> bool {
    credential
        .expires_at
        .is_some_and(|expires_at| expires_at <= OffsetDateTime::now_utc().unix_timestamp())
}

fn require_success(response: reqwest::Response) -> Result<reqwest::Response, OAuthError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(OAuthError::Http(response.status().as_u16()))
    }
}

async fn parse_tokens(response: reqwest::Response) -> Result<TokenResponse, OAuthError> {
    response.json().await.map_err(|_| OAuthError::Invalid)
}

/// Build the stored credential, keeping fields of `previous` the response
/// does not replace.
fn build_credential(
    tokens: TokenResponse,
    previous: Option<&ProviderCredential>,
) -> Result<ProviderCredential, OAuthError> {
    let token = tokens.access_token.ok_or(OAuthError::Invalid)?;
    let access_claims = parse_jwt_claims(&token);
    let id_claims = tokens.id_token.as_deref().and_then(parse_jwt_claims);

    let account_id = [&id_claims, &access_claims]
        .into_iter()
        .flatten()
        .find_map(|claims| {
            claims
                .get(ACCOUNT_CLAIMS)?
                .get("chatgpt_account_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| previous.and_then(|previous| previous.account_id.clone()));
    let expires_at = access_claims
        .as_ref()
        .and_then(|claims| claims.get("exp"))
        .and_then(Value::as_i64)
        .or_else(|| {
            tokens
                .expires_in
                .map(|seconds| OffsetDateTime::now_utc().unix_timestamp() + seconds)
        });
    let refresh_token = tokens
        .refresh_token
        .or_else(|| previous.and_then(|previous| previous.refresh_token.clone()));

    Ok(ProviderCredential {
        token,
        refresh_token,
        account_id,
        expires_at,
    })
}

/// Decode a JWT's payload without verifying it; the claims only select
/// headers and refresh timing, and the provider verifies the token itself.
fn parse_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
#[path = "tests/codex_auth/tests.rs"]
pub(crate) mod tests;
