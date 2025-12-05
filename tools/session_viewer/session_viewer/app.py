from __future__ import annotations

from pathlib import Path
from typing import Optional

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates

from .config import get_config
from .parser import parse_session
from .search import find_rollout_by_conversation_id, list_recent_sessions

BASE_DIR = Path(__file__).resolve().parent
TEMPLATES_DIR = BASE_DIR.parent / "templates"
STATIC_DIR = BASE_DIR.parent / "static"

app = FastAPI(title="Codex Session Viewer")
app.mount("/static", StaticFiles(directory=STATIC_DIR), name="static")
templates = Jinja2Templates(directory=TEMPLATES_DIR)


def _ensure_file(conversation_id: str) -> Path:
    path = find_rollout_by_conversation_id(conversation_id)
    if not path:
        raise HTTPException(status_code=404, detail=f"Conversation {conversation_id} not found")
    return path


@app.get("/", response_class=HTMLResponse)
async def home(request: Request, conversation_id: Optional[str] = None):
    message = None
    redirect = None
    if conversation_id:
        try:
            path = _ensure_file(conversation_id)
            session = parse_session(path)
            redirect = session.meta.conversation_id
        except HTTPException:
            message = f"Conversation {conversation_id} not found"
        else:
            return RedirectResponse(url=f"/conversations/{redirect}")
    recent = list_recent_sessions(limit=30)
    return templates.TemplateResponse(
        "index.html",
        {
            "request": request,
            "recent": recent,
            "config": get_config(),
            "message": message,
        },
    )


@app.get("/conversations/{conversation_id}", response_class=HTMLResponse)
async def conversation(request: Request, conversation_id: str):
    path = _ensure_file(conversation_id)
    session = parse_session(path)
    return templates.TemplateResponse(
        "conversation.html",
        {
            "request": request,
            "session": session,
            "config": get_config(),
        },
    )


@app.get("/api/conversations/{conversation_id}")
async def conversation_api(conversation_id: str):
    path = _ensure_file(conversation_id)
    session = parse_session(path)
    return session


@app.get("/health")
async def health_check():
    cfg = get_config()
    return {"status": "ok", "codex_home": str(cfg.codex_home), "sessions_dir": cfg.sessions_dir.exists()}
