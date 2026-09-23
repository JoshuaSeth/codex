#!/usr/bin/env python3
from __future__ import annotations

import asyncio
import base64
import contextlib
import json
import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urljoin
from urllib.parse import urlparse
from urllib.parse import urlunparse

import httpx
import websockets
from fastapi import FastAPI
from fastapi import HTTPException
from fastapi import Request
from fastapi import WebSocket
from fastapi.responses import JSONResponse
from fastapi.responses import Response
from fastapi.responses import StreamingResponse
from starlette.websockets import WebSocketDisconnect


LOGGER = logging.getLogger("codex_gateway")
DEFAULT_ENV_FILE = "/etc/auth-token-server/auth-token-server.env"
DEFAULT_BROKER_URL = "http://127.0.0.1:38188"
DEFAULT_CHATGPT_CODEX_BASE_URL = "https://chatgpt.com/backend-api/codex"
WS_BETA_HEADER = "responses_websockets=2026-02-06"
HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "authorization",
    "content-length",
}
UPSTREAM_REQUEST_HEADERS = {
    "accept",
    "accept-encoding",
    "content-type",
    "content-encoding",
    "openai-beta",
    "session_id",
    "user-agent",
    "version",
    "x-codex-beta-features",
    "x-codex-turn-metadata",
    "x-codex-turn-state",
    "x-openai-subagent",
    "x-responsesapi-include-timing-metrics",
}
UPSTREAM_RESPONSE_HEADERS = {
    "content-type",
    "content-encoding",
    "cache-control",
    "openai-model",
    "x-codex-turn-state",
    "x-models-etag",
    "x-reasoning-included",
    "x-request-id",
    "cf-ray",
}
AUTH_INVALID_STATUSES = {401, 403}
USAGE_LIMIT_STATUSES = {429}


@dataclass(frozen=True)
class Settings:
    gateway_token: str
    broker_url: str
    broker_token: str
    chatgpt_codex_base_url: str
    client_name: str
    lease_reason: str
    max_rotation_attempts: int
    http_timeout_seconds: float
    heartbeat_interval_seconds: float


@dataclass(frozen=True)
class BrokerLease:
    lease_id: str
    account_id: str
    account_label: str | None
    auth_json: dict[str, Any]

    @property
    def access_token(self) -> str:
        tokens = self.auth_json.get("tokens")
        if not isinstance(tokens, dict) or not isinstance(tokens.get("access_token"), str):
            raise RuntimeError("leased auth_json does not contain an access token")
        return tokens["access_token"]


def _read_env_file(path: str) -> dict[str, str]:
    env_path = Path(path)
    if not env_path.exists():
        return {}
    values: dict[str, str] = {}
    for raw_line in env_path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip().strip('"').strip("'")
    return values


def _env(name: str, env_file_values: dict[str, str], default: str | None = None) -> str | None:
    value = os.environ.get(name) or env_file_values.get(name) or default
    if value is None:
        return None
    value = value.strip()
    return value or None


def load_settings() -> Settings:
    env_file = os.environ.get("CODEX_GATEWAY_AUTH_BROKER_ENV_FILE", DEFAULT_ENV_FILE)
    env_file_values = _read_env_file(env_file)
    gateway_token = _env("CODEX_GATEWAY_TOKEN", env_file_values)
    broker_token = (
        _env("CODEX_GATEWAY_AUTH_BROKER_TOKEN", env_file_values)
        or _env("CODEX_AUTH_BROKER_TOKEN", env_file_values)
        or _env("AUTH_TOKEN_SERVER_CLIENT_TOKEN", env_file_values)
    )
    if not gateway_token:
        raise RuntimeError("CODEX_GATEWAY_TOKEN is required")
    if not broker_token:
        raise RuntimeError("CODEX_GATEWAY_AUTH_BROKER_TOKEN or AUTH_TOKEN_SERVER_CLIENT_TOKEN is required")
    return Settings(
        gateway_token=gateway_token,
        broker_url=(_env("CODEX_GATEWAY_AUTH_BROKER_URL", env_file_values) or DEFAULT_BROKER_URL).rstrip("/"),
        broker_token=broker_token,
        chatgpt_codex_base_url=(
            _env("CODEX_GATEWAY_CHATGPT_CODEX_BASE_URL", env_file_values)
            or DEFAULT_CHATGPT_CODEX_BASE_URL
        ).rstrip("/"),
        client_name=_env("CODEX_GATEWAY_CLIENT_NAME", env_file_values, "codex-gateway") or "codex-gateway",
        lease_reason=_env("CODEX_GATEWAY_LEASE_REASON", env_file_values, "gateway-request") or "gateway-request",
        max_rotation_attempts=int(_env("CODEX_GATEWAY_MAX_ROTATION_ATTEMPTS", env_file_values, "8") or "8"),
        http_timeout_seconds=float(_env("CODEX_GATEWAY_HTTP_TIMEOUT_SECONDS", env_file_values, "60") or "60"),
        heartbeat_interval_seconds=float(
            _env("CODEX_GATEWAY_HEARTBEAT_INTERVAL_SECONDS", env_file_values, "60") or "60"
        ),
    )


def _bearer_token(value: str | None) -> str | None:
    if not value:
        return None
    prefix = "Bearer "
    if not value.startswith(prefix):
        return None
    token = value[len(prefix) :].strip()
    return token or None


def _require_gateway_token_from_header(authorization: str | None, settings: Settings) -> None:
    if _bearer_token(authorization) != settings.gateway_token:
        raise HTTPException(status_code=403, detail="invalid gateway token")


def _websocket_authorized(websocket: WebSocket, settings: Settings) -> bool:
    token = _bearer_token(websocket.headers.get("authorization"))
    if token is None:
        token = websocket.query_params.get("token")
    return token == settings.gateway_token


def _filtered_request_headers(headers: Any) -> dict[str, str]:
    out: dict[str, str] = {}
    for raw_name, raw_value in headers.items():
        if not isinstance(raw_name, str) or not isinstance(raw_value, str):
            continue
        name = raw_name.lower()
        if name in HOP_BY_HOP_HEADERS:
            continue
        if name in UPSTREAM_REQUEST_HEADERS:
            out[name] = raw_value
    return out


def _filtered_response_headers(headers: Any) -> dict[str, str]:
    out: dict[str, str] = {}
    for raw_name, raw_value in headers.items():
        name = raw_name.lower()
        if name in HOP_BY_HOP_HEADERS:
            continue
        if name in UPSTREAM_RESPONSE_HEADERS:
            out[name] = raw_value
    return out


def _ws_url(base_url: str, path: str) -> str:
    parsed = urlparse(urljoin(f"{base_url.rstrip('/')}/", path.lstrip("/")))
    if parsed.scheme == "https":
        parsed = parsed._replace(scheme="wss")
    elif parsed.scheme == "http":
        parsed = parsed._replace(scheme="ws")
    return urlunparse(parsed)


def _http_url(base_url: str, path: str) -> str:
    return urljoin(f"{base_url.rstrip('/')}/", path.lstrip("/"))


def _path_with_query(path: str, query: str) -> str:
    if not query:
        return path
    return f"{path}?{query}"


def _safe_detail(text: str, limit: int = 1000) -> str:
    text = " ".join(text.split())
    if len(text) <= limit:
        return text
    return text[:limit] + "..."


def _json_error(message: str, *, status: int = 503, code: str = "gateway_error") -> dict[str, Any]:
    return {"type": "error", "status": status, "error": {"type": code, "message": message}}


def _classify_error_payload(payload: str) -> tuple[str, str] | None:
    try:
        data = json.loads(payload)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict) or data.get("type") != "error":
        return None
    status = data.get("status") or data.get("status_code")
    error = data.get("error") if isinstance(data.get("error"), dict) else {}
    text = json.dumps(data, separators=(",", ":"))
    if status in USAGE_LIMIT_STATUSES or "usage_limit" in text or "rate_limit" in text:
        return "usage_limit_reached", _safe_detail(error.get("message") or text)
    if status in AUTH_INVALID_STATUSES:
        return "unauthorized", _safe_detail(error.get("message") or text)
    return None


def _is_response_completed(payload: str) -> bool:
    try:
        data = json.loads(payload)
    except json.JSONDecodeError:
        return False
    return isinstance(data, dict) and data.get("type") == "response.completed"


def _affinity_from_http(request: Request, body: bytes) -> str | None:
    if request.headers.get("session_id"):
        return request.headers["session_id"]
    try:
        payload = json.loads(body)
    except json.JSONDecodeError:
        return None
    if isinstance(payload, dict):
        prompt_cache_key = payload.get("prompt_cache_key")
        if isinstance(prompt_cache_key, str) and prompt_cache_key:
            return prompt_cache_key
    return None


def _affinity_from_tunnel_payload(payload: dict[str, Any]) -> str | None:
    affinity_key = payload.get("affinity_key")
    if isinstance(affinity_key, str) and affinity_key:
        return affinity_key
    request_id = payload.get("id")
    if isinstance(request_id, str) and request_id:
        return request_id
    return None


def _affinity_from_response_create_payload(payload: dict[str, Any]) -> str | None:
    prompt_cache_key = payload.get("prompt_cache_key")
    if isinstance(prompt_cache_key, str) and prompt_cache_key:
        return prompt_cache_key
    return None


def _affinity_from_websocket(websocket: WebSocket) -> str | None:
    return websocket.headers.get("session_id") or websocket.query_params.get("affinity_key")


def _broker_headers(settings: Settings) -> dict[str, str]:
    return {"Authorization": f"Bearer {settings.broker_token}", "Content-Type": "application/json"}


async def acquire_lease(settings: Settings, affinity_key: str | None) -> BrokerLease:
    timeout = httpx.Timeout(settings.http_timeout_seconds)
    async with httpx.AsyncClient(timeout=timeout) as client:
        response = await client.post(
            f"{settings.broker_url}/v1/leases",
            headers=_broker_headers(settings),
            json={
                "client_name": settings.client_name,
                "affinity_key": affinity_key,
                "lease_reason": settings.lease_reason,
            },
        )
        response.raise_for_status()
        payload = response.json()
    raw_auth = base64.b64decode(payload["auth_json_b64"].encode("utf-8"))
    auth_json = json.loads(raw_auth.decode("utf-8"))
    return BrokerLease(
        lease_id=payload["lease_id"],
        account_id=payload["account_id"],
        account_label=payload.get("account_label"),
        auth_json=auth_json,
    )


async def report_lease(settings: Settings, lease: BrokerLease, outcome: str, detail: str | None = None) -> None:
    try:
        timeout = httpx.Timeout(settings.http_timeout_seconds)
        async with httpx.AsyncClient(timeout=timeout) as client:
            await client.post(
                f"{settings.broker_url}/v1/leases/{lease.lease_id}/report",
                headers=_broker_headers(settings),
                json={"outcome": outcome, "detail": _safe_detail(detail or "") or None},
            )
    except Exception as exc:  # noqa: BLE001
        LOGGER.warning("failed to report broker lease %s: %s", lease.lease_id, exc)


async def heartbeat_loop(settings: Settings, lease: BrokerLease, stop: asyncio.Event) -> None:
    try:
        while not stop.is_set():
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(stop.wait(), timeout=settings.heartbeat_interval_seconds)
            if stop.is_set():
                break
            try:
                timeout = httpx.Timeout(settings.http_timeout_seconds)
                async with httpx.AsyncClient(timeout=timeout) as client:
                    await client.post(
                        f"{settings.broker_url}/v1/leases/{lease.lease_id}/heartbeat",
                        headers=_broker_headers(settings),
                    )
            except Exception as exc:  # noqa: BLE001
                LOGGER.warning("broker heartbeat failed for lease %s: %s", lease.lease_id, exc)
    except asyncio.CancelledError:
        pass


def upstream_headers(settings: Settings, source_headers: Any, lease: BrokerLease) -> dict[str, str]:
    headers = _filtered_request_headers(source_headers)
    headers["authorization"] = f"Bearer {lease.access_token}"
    headers["chatgpt-account-id"] = lease.account_id
    headers.setdefault("openai-beta", WS_BETA_HEADER)
    headers.setdefault("version", "codex-gateway")
    return headers


async def _send_ws_text(websocket: WebSocket, payload: dict[str, Any]) -> None:
    await websocket.send_text(json.dumps(payload, separators=(",", ":")))


async def proxy_http_request(
    settings: Settings,
    method: str,
    path: str,
    source_headers: Any,
    body: bytes,
    affinity_key: str | None,
    *,
    stream_response: bool,
):
    last_error = "gateway request failed"
    for attempt in range(1, max(1, settings.max_rotation_attempts) + 1):
        lease: BrokerLease | None = None
        client: httpx.AsyncClient | None = None
        response: httpx.Response | None = None
        try:
            lease = await acquire_lease(settings, affinity_key)
            headers = upstream_headers(settings, source_headers, lease)
            client = httpx.AsyncClient(timeout=httpx.Timeout(None))
            upstream_request = client.build_request(
                method,
                _http_url(settings.chatgpt_codex_base_url, path),
                headers=headers,
                content=body,
            )
            response = await client.send(upstream_request, stream=True)
            if response.status_code in USAGE_LIMIT_STATUSES | AUTH_INVALID_STATUSES:
                raw_error = (await response.aread()).decode("utf-8", errors="replace")
                outcome = "usage_limit_reached" if response.status_code in USAGE_LIMIT_STATUSES else "unauthorized"
                await report_lease(settings, lease, outcome, raw_error)
                last_error = _safe_detail(raw_error or f"upstream status {response.status_code}")
                await response.aclose()
                await client.aclose()
                LOGGER.info("upstream HTTP returned %s on attempt %s", response.status_code, attempt)
                continue

            if not stream_response:
                response_body = await response.aread()
                await report_lease(settings, lease, "success")
                status_code = response.status_code
                headers = _filtered_response_headers(response.headers)
                headers.pop("content-encoding", None)
                await response.aclose()
                await client.aclose()
                return Response(response_body, status_code=status_code, headers=headers)

            async def stream_chunks() -> Any:
                try:
                    assert response is not None
                    async for chunk in response.aiter_raw():
                        yield chunk
                    if lease is not None:
                        await report_lease(settings, lease, "success")
                except Exception as exc:  # noqa: BLE001
                    if lease is not None:
                        await report_lease(settings, lease, "gateway_error", str(exc))
                    raise
                finally:
                    if response is not None:
                        await response.aclose()
                    if client is not None:
                        await client.aclose()

            return StreamingResponse(
                stream_chunks(),
                status_code=response.status_code,
                headers=_filtered_response_headers(response.headers),
            )
        except Exception as exc:  # noqa: BLE001
            last_error = _safe_detail(str(exc))
            if lease is not None:
                await report_lease(settings, lease, "gateway_error", last_error)
            if response is not None:
                await response.aclose()
            if client is not None:
                await client.aclose()
            LOGGER.warning("HTTP proxy attempt %s failed: %s", attempt, last_error)

    return JSONResponse(_json_error(last_error), status_code=503)


async def proxy_tunnel_http_request(
    websocket: WebSocket,
    settings: Settings,
    payload: dict[str, Any],
) -> None:
    request_id = payload.get("id")
    method = payload.get("method")
    path = payload.get("path")
    if not isinstance(request_id, str) or not request_id:
        await _send_ws_text(websocket, _json_error("http.request id is required", status=400))
        return
    if not isinstance(method, str) or not method:
        await _send_ws_text(websocket, _json_error("http.request method is required", status=400))
        return
    if not isinstance(path, str) or not path or path.startswith("/") or "://" in path:
        await _send_ws_text(websocket, _json_error("http.request path must be a relative upstream path", status=400))
        return
    raw_headers = payload.get("headers")
    if raw_headers is None:
        raw_headers = {}
    if not isinstance(raw_headers, dict):
        await _send_ws_text(websocket, _json_error("http.request headers must be an object", status=400))
        return
    if not any(isinstance(name, str) and name.lower() == "accept-encoding" for name in raw_headers):
        raw_headers["accept-encoding"] = "identity"
    body_b64 = payload.get("body_b64")
    if body_b64 is None:
        body = b""
    elif isinstance(body_b64, str):
        try:
            body = base64.b64decode(body_b64.encode("utf-8"), validate=True)
        except ValueError:
            await _send_ws_text(websocket, _json_error("http.request body_b64 is not valid base64", status=400))
            return
    else:
        await _send_ws_text(websocket, _json_error("http.request body_b64 must be a string", status=400))
        return

    response = await proxy_http_request(
        settings,
        method.upper(),
        path,
        raw_headers,
        body,
        _affinity_from_tunnel_payload(payload),
        stream_response=False,
    )
    if isinstance(response, JSONResponse):
        body_bytes = response.body
        headers = dict(response.headers)
        status_code = response.status_code
    else:
        body_bytes = response.body
        headers = dict(response.headers)
        status_code = response.status_code
    await _send_ws_text(
        websocket,
        {
            "type": "http.response",
            "id": request_id,
            "status": status_code,
            "headers": headers,
            "body_b64": base64.b64encode(body_bytes).decode("ascii"),
        },
    )


async def proxy_single_ws_request(
    websocket: WebSocket,
    settings: Settings,
    source_headers: Any,
    message: str,
    affinity_key: str | None,
) -> None:
    last_error = "gateway request failed"
    max_attempts = max(1, settings.max_rotation_attempts)
    for attempt in range(1, max_attempts + 1):
        lease: BrokerLease | None = None
        stop_heartbeat: asyncio.Event | None = None
        heartbeat_task: asyncio.Task[None] | None = None
        sent_any_event = False
        try:
            lease = await acquire_lease(settings, affinity_key)
            stop_heartbeat = asyncio.Event()
            heartbeat_task = asyncio.create_task(heartbeat_loop(settings, lease, stop_heartbeat))
            headers = upstream_headers(settings, source_headers, lease)
            upstream_url = _ws_url(settings.chatgpt_codex_base_url, "responses")
            async with websockets.connect(
                upstream_url,
                additional_headers=headers,
                compression="deflate",
                open_timeout=settings.http_timeout_seconds,
                ping_interval=20,
                ping_timeout=20,
                max_size=None,
            ) as upstream:
                await upstream.send(message)
                async for upstream_message in upstream:
                    if isinstance(upstream_message, bytes):
                        sent_any_event = True
                        await websocket.send_bytes(upstream_message)
                        continue
                    classified = _classify_error_payload(upstream_message)
                    if classified is not None and not sent_any_event:
                        outcome, detail = classified
                        await report_lease(settings, lease, outcome, detail)
                        last_error = detail
                        LOGGER.info(
                            "upstream websocket returned %s for %s on attempt %s/%s",
                            outcome,
                            lease.account_label or lease.account_id,
                            attempt,
                            max_attempts,
                        )
                        break
                    sent_any_event = True
                    await websocket.send_text(upstream_message)
                    if _is_response_completed(upstream_message):
                        await report_lease(settings, lease, "success")
                        return
                else:
                    await report_lease(settings, lease, "gateway_error", "upstream websocket closed")
                    last_error = "upstream websocket closed"
        except Exception as exc:  # noqa: BLE001
            last_error = _safe_detail(str(exc))
            if lease is not None:
                outcome = "gateway_error"
                if "401" in last_error or "403" in last_error:
                    outcome = "unauthorized"
                elif "429" in last_error or "usage_limit" in last_error:
                    outcome = "usage_limit_reached"
                await report_lease(settings, lease, outcome, last_error)
            LOGGER.warning("websocket proxy attempt %s/%s failed: %s", attempt, max_attempts, last_error)
        finally:
            if stop_heartbeat is not None:
                stop_heartbeat.set()
            if heartbeat_task is not None:
                heartbeat_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await heartbeat_task
    await _send_ws_text(websocket, _json_error(last_error))


def create_app() -> FastAPI:
    app = FastAPI(title="codex-gateway")

    @app.get("/healthz")
    async def healthz() -> dict[str, Any]:
        settings = load_settings()
        return {
            "status": "ok",
            "broker_url": settings.broker_url,
            "chatgpt_codex_base_url": settings.chatgpt_codex_base_url,
            "max_rotation_attempts": settings.max_rotation_attempts,
        }

    @app.post("/v1/responses", response_model=None)
    async def responses_http(request: Request):
        settings = load_settings()
        _require_gateway_token_from_header(request.headers.get("authorization"), settings)
        body = await request.body()
        affinity_key = _affinity_from_http(request, body)
        return await proxy_http_request(
            settings,
            "POST",
            "responses",
            request.headers,
            body,
            affinity_key,
            stream_response=True,
        )

    @app.api_route(
        "/v1/{path:path}",
        methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"],
        response_model=None,
    )
    async def codex_http_proxy(path: str, request: Request):
        settings = load_settings()
        _require_gateway_token_from_header(request.headers.get("authorization"), settings)
        body = await request.body()
        return await proxy_http_request(
            settings,
            request.method,
            _path_with_query(path, request.url.query),
            request.headers,
            body,
            _affinity_from_http(request, body),
            stream_response=True,
        )

    async def responses_websocket(websocket: WebSocket) -> None:
        settings = load_settings()
        if not _websocket_authorized(websocket, settings):
            await websocket.close(code=1008)
            return
        await websocket.accept()
        affinity_key = _affinity_from_websocket(websocket)
        try:
            while True:
                event = await websocket.receive()
                kind = event.get("type")
                if kind == "websocket.disconnect":
                    return
                if kind != "websocket.receive":
                    continue
                if event.get("bytes") is not None:
                    await _send_ws_text(websocket, _json_error("binary websocket requests are not supported", status=400))
                    continue
                text = event.get("text")
                if not isinstance(text, str):
                    continue
                try:
                    payload = json.loads(text)
                except json.JSONDecodeError:
                    await _send_ws_text(websocket, _json_error("websocket message must be JSON", status=400))
                    continue
                if isinstance(payload, dict) and payload.get("type") == "ping":
                    await _send_ws_text(websocket, {"type": "pong"})
                    continue
                if isinstance(payload, dict) and payload.get("type") == "http.request":
                    await proxy_tunnel_http_request(websocket, settings, payload)
                    continue
                if not isinstance(payload, dict) or payload.get("type") != "response.create":
                    await _send_ws_text(websocket, _json_error("only response.create websocket messages are supported", status=400))
                    continue
                request_affinity_key = _affinity_from_response_create_payload(payload) or affinity_key
                await proxy_single_ws_request(websocket, settings, websocket.headers, text, request_affinity_key)
        except WebSocketDisconnect:
            return

    app.add_api_websocket_route("/v1/responses", responses_websocket)
    app.add_api_websocket_route("/v1/codex-tunnel", responses_websocket)
    return app


app = create_app()


if __name__ == "__main__":
    import uvicorn

    logging.basicConfig(level=os.environ.get("CODEX_GATEWAY_LOG_LEVEL", "INFO"))
    uvicorn.run(
        "codex_gateway:app",
        host=os.environ.get("CODEX_GATEWAY_BIND", "127.0.0.1"),
        port=int(os.environ.get("CODEX_GATEWAY_PORT", "38288")),
        reload=False,
    )
