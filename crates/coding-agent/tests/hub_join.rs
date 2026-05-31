#[allow(dead_code)]
#[path = "../src/hub_auth.rs"]
mod hub_auth;

use hub_auth::{
    HubAuthExchangeCodeResponse, HubAuthSecretFragments, HubAuthStartResponse, HubDiscoverable,
    HubInbox,
};
use serde_json::json;

#[test]
fn auth_start_response_shape_excludes_code_token_and_loopback() {
    let response: HubAuthStartResponse = serde_json::from_value(json!({
        "exchange_request_id": "8fc3aa4b-f9f7-48c4-a6f9-cd1ea6628b70",
        "login_url": "https://pie.0xfefe.me/login?req=8fc3aa4b-f9f7-48c4-a6f9-cd1ea6628b70&state=state-public-nonce",
        "expires_in_seconds": 300
    }))
    .expect("start response shape");

    assert_eq!(response.expires_in_seconds, 300);
    let serialized = serde_json::to_string(&response).unwrap();
    assert!(!serialized.contains("hub_token"), "{serialized}");
    assert!(!serialized.contains("code_verifier"), "{serialized}");
    assert!(!serialized.contains("http://127.0.0.1"), "{serialized}");
}

#[test]
fn auth_exchange_response_shape_is_exact_and_bounded() {
    let response: HubAuthExchangeCodeResponse = serde_json::from_value(json!({
        "agent_id": "018fe23a-1111-4a22-8b33-123456789abc",
        "handle": "alice",
        "namespace": "dongxu",
        "hub_token": "hub_agent_018fe23a-1111-4a22-8b33-secretvalue",
        "expires_at": null,
        "profile": {
            "display_name": "alice",
            "description": null,
            "capabilities": []
        },
        "visibility": {
            "discoverable": "public",
            "inbox": "namespace"
        }
    }))
    .expect("exchange response shape");

    assert_eq!(response.handle, "alice");
    assert_eq!(response.visibility.discoverable, HubDiscoverable::Public);
    assert_eq!(response.visibility.inbox, HubInbox::Namespace);

    let with_extra = json!({
        "agent_id": "018fe23a-1111-4a22-8b33-123456789abc",
        "handle": "alice",
        "namespace": "dongxu",
        "hub_token": "hub_agent_018fe23a-1111-4a22-8b33-secretvalue",
        "expires_at": null,
        "profile": {"display_name": "alice", "description": null, "capabilities": []},
        "visibility": {"discoverable": "public", "inbox": "namespace"},
        "raw_session": "must-not-be-accepted"
    });
    assert!(serde_json::from_value::<HubAuthExchangeCodeResponse>(with_extra).is_err());
}

#[test]
fn ui_surfaces_can_assert_auth_secret_absence() {
    let secrets = HubAuthSecretFragments {
        hub_token: Some("hub_agent_secret_should_not_render"),
        code: Some("hub_code_secret_should_not_render"),
        state: Some("state_secret_should_not_render"),
        code_verifier: Some("pkce_verifier_should_not_render"),
        loopback_redirect_uri: Some("http://127.0.0.1:49152/callback"),
        login_url: Some("https://pie.0xfefe.me/login?req=req-1&state=state-1"),
    };

    let safe = "Joined. You are alice@dongxu. recovery -> /hub join";
    secrets.assert_absent_from("safe output", safe);

    let unsafe_output = "debug: hub_agent_secret_should_not_render";
    assert!(
        !secrets.is_absent_from(unsafe_output),
        "secret detector must catch token-like values"
    );

    let bare_state_output = "debug: state_secret_should_not_render";
    assert!(
        !secrets.is_absent_from(bare_state_output),
        "secret detector must catch bare state values"
    );
}
