use super::before_effect;
use super::invalid_request_before_effect;
use codex_app_server_protocol::JSONRPCErrorError;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn before_effect_preserves_structured_error_details() {
    let original = JSONRPCErrorError {
        code: -32603,
        message: "failed to load configuration".to_owned(),
        data: Some(json!({
            "reason": "cloudConfigBundle",
            "errorCode": "Auth",
            "action": "relogin",
            "statusCode": 401,
            "detail": "refresh token revoked",
        })),
    };
    let expected = JSONRPCErrorError {
        data: Some(json!({
            "reason": "cloudConfigBundle",
            "errorCode": "Auth",
            "action": "relogin",
            "statusCode": 401,
            "detail": "refresh token revoked",
            "effect": "notStarted",
        })),
        ..original.clone()
    };
    assert_eq!(before_effect(original), expected);
    assert_eq!(before_effect(expected.clone()), expected);
}

#[test]
fn before_effect_retains_non_object_data_as_detail() {
    for detail in [json!("failure"), json!([1, 2]), json!(null)] {
        let original = JSONRPCErrorError {
            code: -32603,
            message: "failure".to_owned(),
            data: Some(detail.clone()),
        };
        let expected = JSONRPCErrorError {
            data: Some(json!({"detail": detail, "effect": "notStarted"})),
            ..original.clone()
        };
        assert_eq!(before_effect(original), expected);
    }
}

#[test]
fn invalid_request_before_effect_marks_errors_without_data() {
    assert_eq!(
        invalid_request_before_effect("invalid thread"),
        JSONRPCErrorError {
            code: -32600,
            message: "invalid thread".to_owned(),
            data: Some(json!({"effect": "notStarted"})),
        }
    );
}
