use super::*;

pub(crate) fn build_jwt(claims: Value) -> String {
    let encode =
        |value: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    format!(
        "{}.{}.sig",
        encode(&json!({"alg": "none"})),
        encode(&claims)
    )
}

#[test]
fn credential_takes_account_and_expiry_from_claims() {
    let access = build_jwt(json!({
        "exp": 1_900_000_000,
        ACCOUNT_CLAIMS: {"chatgpt_account_id": "from-access"},
    }));
    let id = build_jwt(json!({ ACCOUNT_CLAIMS: {"chatgpt_account_id": "from-id"} }));
    let credential = build_credential(
        TokenResponse {
            access_token: Some(access.clone()),
            refresh_token: Some("refresh".into()),
            id_token: Some(id),
            expires_in: Some(10),
        },
        None,
    )
    .unwrap();
    assert!(credential.token == access);
    assert_eq!(credential.account_id.as_deref(), Some("from-id"));
    assert_eq!(credential.expires_at, Some(1_900_000_000));
    assert_eq!(credential.refresh_token.as_deref(), Some("refresh"));
}

#[test]
fn refresh_keeps_fields_the_response_omits() {
    let previous = ProviderCredential {
        token: "old".into(),
        refresh_token: Some("kept".into()),
        account_id: Some("account".into()),
        expires_at: Some(1),
    };
    let credential = build_credential(
        TokenResponse {
            access_token: Some("opaque".into()),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
        },
        Some(&previous),
    )
    .unwrap();
    assert_eq!(credential.refresh_token.as_deref(), Some("kept"));
    assert_eq!(credential.account_id.as_deref(), Some("account"));
    assert!(credential.expires_at.unwrap() > OffsetDateTime::now_utc().unix_timestamp());

    let missing = TokenResponse {
        access_token: None,
        refresh_token: None,
        id_token: None,
        expires_in: None,
    };
    assert!(matches!(
        build_credential(missing, None),
        Err(OAuthError::Invalid)
    ));
}

#[test]
fn refresh_is_needed_only_near_expiry_with_a_refresh_token() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let margin = Duration::from_secs(300);
    let credential = |expires_at, refresh: bool| ProviderCredential {
        token: "t".into(),
        refresh_token: refresh.then(|| "r".into()),
        account_id: None,
        expires_at,
    };
    assert!(needs_refresh(&credential(Some(now + 60), true), margin));
    assert!(!needs_refresh(&credential(Some(now + 3600), true), margin));
    assert!(!needs_refresh(&credential(Some(now - 60), false), margin));
    assert!(!needs_refresh(&credential(None, true), margin));
    assert!(is_expired(&credential(Some(now - 1), false)));
    assert!(!is_expired(&credential(None, false)));
}
