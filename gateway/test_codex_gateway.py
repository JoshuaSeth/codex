from __future__ import annotations

import json

from codex_gateway import BrokerLease
from codex_gateway import Settings
from codex_gateway import _classify_error_payload
from codex_gateway import _filtered_request_headers
from codex_gateway import _filtered_response_headers
from codex_gateway import _http_url
from codex_gateway import _is_response_completed
from codex_gateway import _path_with_query
from codex_gateway import _safe_detail
from codex_gateway import _ws_url
from codex_gateway import upstream_headers


def _settings() -> Settings:
    return Settings(
        gateway_token="gateway-token",
        broker_url="http://127.0.0.1:38188",
        broker_token="broker-token",
        chatgpt_codex_base_url="https://chatgpt.com/backend-api/codex",
        client_name="codex-gateway",
        lease_reason="gateway-request",
        max_rotation_attempts=8,
        http_timeout_seconds=60,
        heartbeat_interval_seconds=60,
    )


def test_ws_url_maps_https_responses_endpoint() -> None:
    assert _ws_url("https://chatgpt.com/backend-api/codex", "responses") == (
        "wss://chatgpt.com/backend-api/codex/responses"
    )


def test_http_url_maps_relative_codex_endpoint() -> None:
    assert _http_url("https://chatgpt.com/backend-api/codex", "models?client_version=dev") == (
        "https://chatgpt.com/backend-api/codex/models?client_version=dev"
    )


def test_path_with_query_preserves_codex_endpoint_query() -> None:
    assert _path_with_query("models", "client_version=dev") == "models?client_version=dev"
    assert _path_with_query("models", "") == "models"


def test_classify_usage_limit_error_payload() -> None:
    payload = json.dumps(
        {
            "type": "error",
            "status": 429,
            "error": {"type": "usage_limit_reached", "message": "limit reached"},
        }
    )
    assert _classify_error_payload(payload) == ("usage_limit_reached", "limit reached")


def test_classify_rate_limit_marker_in_error_text() -> None:
    payload = json.dumps({"type": "error", "status": 500, "error": {"message": "rate_limit_exceeded"}})
    assert _classify_error_payload(payload) == ("usage_limit_reached", "rate_limit_exceeded")


def test_classify_unauthorized_error_payload() -> None:
    payload = json.dumps({"type": "error", "status": 401, "error": {"message": "bad token"}})
    assert _classify_error_payload(payload) == ("unauthorized", "bad token")


def test_response_completed_detection() -> None:
    assert _is_response_completed('{"type":"response.completed","response":{"id":"resp-1"}}')
    assert not _is_response_completed('{"type":"response.output_text.delta"}')


def test_filtered_request_headers_removes_gateway_secrets() -> None:
    assert _filtered_request_headers(
        {
            "Authorization": "Bearer gateway-token",
            "Host": "codex-cowork.pitchai.net",
            "OpenAI-Beta": "responses_websockets=2026-02-06",
            "Session_ID": "session-1",
            "X-Unknown": "drop-me",
        }
    ) == {
        "openai-beta": "responses_websockets=2026-02-06",
        "session_id": "session-1",
    }


def test_filtered_response_headers_removes_hop_by_hop_headers() -> None:
    assert _filtered_response_headers(
        {
            "Content-Type": "application/json",
            "Transfer-Encoding": "chunked",
            "X-Request-ID": "req-1",
            "Server": "drop-me",
        }
    ) == {
        "content-type": "application/json",
        "x-request-id": "req-1",
    }


def test_upstream_headers_injects_broker_auth_not_gateway_auth() -> None:
    lease = BrokerLease(
        lease_id="lease-1",
        account_id="account-1",
        account_label="Account 1",
        auth_json={"tokens": {"access_token": "leased-token"}},
    )

    assert upstream_headers(
        _settings(),
        {"Authorization": "Bearer gateway-token", "Accept": "application/json"},
        lease,
    ) == {
        "accept": "application/json",
        "authorization": "Bearer leased-token",
        "chatgpt-account-id": "account-1",
        "openai-beta": "responses_websockets=2026-02-06",
        "version": "codex-gateway",
    }


def test_safe_detail_compacts_and_truncates() -> None:
    assert _safe_detail("one\n two\tthree", limit=100) == "one two three"
    assert _safe_detail("abcdef", limit=3) == "abc..."
