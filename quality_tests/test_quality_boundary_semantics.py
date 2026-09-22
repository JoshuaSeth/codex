"""Check real entry boundaries and retain genuine architecture failures."""

from __future__ import annotations

import ast
import json
import os
import subprocess
from pathlib import Path

import pytest

from pitchai_quality.analysis_support import ParsedModule
from pitchai_quality.check_nested_event_loops import _check_file
from pitchai_quality.check_no_single_use_one_line_functions import _find_violations


@pytest.mark.parametrize(("source", "count"), [
    ('if __name__ == "__main__":\n    asyncio.run(main())\n', 0),
    ('if "__main__" == __name__:\n    asyncio.run(main())\n', 0),
    ('if __name__ == "__main__":\n    if enabled:\n        asyncio.run(main())\n', 0),
    ('async def handler():\n    asyncio.run(main())\n', 1),
    ('def helper():\n    asyncio.run(main())\n', 1),
    ('if __name__ == "__main__":\n    async def handler():\n        asyncio.run(main())\n', 1),
    ('if __name__ == "__main__":\n    pass\nelse:\n    asyncio.run(main())\n', 1),
    ('if __name__ == "__main__":\n    asyncio.run(asyncio.run(main()))\n', 1),
    ('if __name__ == "__main__":\n    asyncio.new_event_loop()\n', 1),
    ('async def handler():\n    loop.run_until_complete(main())\n', 1),
])
def test_loop_scope(tmp_path: Path, source: str, count: int) -> None:
    path = tmp_path / "entry.py"
    path.write_text(source)
    assert len(_check_file(path)) == count


@pytest.mark.parametrize(("setup", "decorator", "count"), [
    ('from fastapi import FastAPI\napp = FastAPI()\n', '@app.get("/")\n', 0),
    ('from fastapi import APIRouter as Router\nweb = Router()\n', '@web.post("/")\n', 0),
    ('import fastapi as fa\nweb = fa.FastAPI()\n', '@web.get("/")\n', 0),
    ('from fastapi import FastAPI\napp = FastAPI()\napp = something_else\n', '@app.get("/")\n', 1),
    ('app = something_else\n', '@app.get("/")\n', 1),
    ('from fastapi import FastAPI\napp = FastAPI()\n', '@app.arbitrary("/")\n', 1),
    ('', '', 1),
])
@pytest.mark.parametrize("prefix", ["def", "async def"])
def test_framework_registration(tmp_path: Path, setup: str, decorator: str, count: int, prefix: str) -> None:
    tree = ast.parse(setup + decorator + prefix + ' root():\n    return {"ok": True}\n')
    parsed = ParsedModule(path=tmp_path / "routes.py", module="routes", tree=tree)
    assert len(_find_violations((parsed,))) == count


def test_http_and_exception_edges(tmp_path: Path) -> None:
    """Run the real pinned scanner, never import or execute fixture applications."""
    source = '''import httpx
import aiohttp
import requests
from starlette.types import ASGIApp, Scope, Receive, Send
from starlette.middleware.base import BaseHTTPMiddleware, RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import Response

response = httpx.Response(200)
transport = httpx.MockTransport(handler)
error = httpx.ConnectError("offline")
request = httpx.Request("GET", "https://example.invalid")

def gateway(url):
    try:
        with httpx.Client(timeout=10) as client:
            return client.get(url)
    except httpx.RequestError:
        raise GatewayUnavailable()

async def async_gateway(url):
    try:
        async with httpx.AsyncClient(timeout=10) as client:
            return await client.get(url)
    except httpx.RequestError:
        raise GatewayUnavailable()

class IdentityMiddleware:
    app: ASGIApp
    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        try:
            await self.app(scope, receive, send)
        except AuthenticationError:
            await send({"type": "http.response.start", "status": 401})

class WebMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next: RequestResponseEndpoint) -> Response:
        try:
            return await call_next(request)
        except AuthenticationError:
            return Response(status_code=401)

    def calculate(self):
        try:  # expect-exception
            business_calculation()
        except ValueError:
            return None

def business():
    try:  # expect-exception
        calculate()
    except ValueError:
        return None

def unsafe_gateway(url):
    with httpx.Client() as client:
        httpx.get(url)  # expect-http
        try:  # expect-exception
            calculate()
        except ValueError:
            return None

async def session_gateway(url):
    try:
        async with aiohttp.ClientSession() as client:
            async with client.get(url) as response:
                return await response.text()
    except aiohttp.ClientError:
        raise GatewayUnavailable()

def requests_gateway(url):
    try:
        with requests.Session() as client:
            return client.get(url)
    except requests.RequestException:
        raise GatewayUnavailable()

def non_middleware():
    try:  # expect-exception
        authenticate()
    except AuthenticationError:
        return None

client = httpx.Client()  # expect-http
httpx.get("https://example.invalid")  # expect-http
aiohttp.ClientSession()  # expect-http
requests.Session()  # expect-http
requests.get("https://example.invalid")  # expect-http
'''
    fixture = tmp_path / "boundaries.py"
    fixture.write_text(source)
    root = Path(__file__).resolve().parents[1]
    run = subprocess.run([
        os.environ.get("SEMGREP_BINARY", "semgrep"), "scan", "--config", str(root / "quality/.semgrep.yml"),
        "--json", "--metrics=off", "--disable-version-check", "--no-git-ignore", "--disable-nosem", str(fixture),
    ], check=True, capture_output=True, text=True)
    result = json.loads(run.stdout)
    assert result["errors"] == []
    actual = {(row["check_id"].split(".")[-1], row["start"]["line"]) for row in result["results"]}
    expected = set()
    for line, text in enumerate(source.splitlines(), 1):
        if "# expect-exception" in text:
            expected.add(("py-try-except-outside-edges", line))
        if "# expect-http" in text:
            expected.add(("forbid-requests-outside-infra", line))
    assert actual == expected
