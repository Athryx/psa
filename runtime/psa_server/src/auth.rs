//! Request authentication against the PSA keys kept in `auth.toml`
//! (see `psa_config::auth`).
use subtle::ConstantTimeEq;

pub(crate) fn has_bearer_key(headers: &axum::http::HeaderMap, key: &str) -> bool {
    if key.trim().is_empty() {
        return false;
    }

    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(key.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_key_requires_exact_header_and_credential() {
        use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

        let mut headers = HeaderMap::new();
        assert!(!has_bearer_key(&headers, "secret"));
        for (value, expected) in [
            ("Bearer secret", true),
            ("Bearer wrong!", false),
            ("Bearer secre", false),
            ("Bearer secret-extra", false),
            ("Bearer Secret", false),
            ("bearer secret", false),
            ("Basic secret", false),
            ("secret", false),
            ("Bearer", false),
            ("Bearer ", false),
            ("Bearer  secret", false),
            ("Bearer secret ", false),
        ] {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(has_bearer_key(&headers, "secret"), expected);
        }
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
        );
        assert!(!has_bearer_key(&headers, "secret"));
        for key in ["", " ", "\t"] {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
            );
            assert!(!has_bearer_key(&headers, key));
        }
    }
}
