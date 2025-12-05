# Codex Conversation Visualizer Plan

## Goals
- Serve a lightweight FastAPI UI that renders any Codex session (JSONL) by conversation ID.
- Keep implementation isolated from the Rust workspace.
- Provide both server-rendered HTML and JSON for future use.
- Verify end-to-end: parse real session logs, render via browser, capture screenshot.

## Architecture
1. **Python mini-project** under `tools/session_viewer/` with FastAPI + Jinja2.
2. **Config loader** resolves `$CODEX_HOME` (default `~/.codex`) and session roots.
3. **Session finder** locates rollout JSONL files by conversation ID; optional listing by date.
4. **Parser layer** streams JSONL events into typed dataclasses (Pydantic models).
5. **Summarizer** builds timeline entries (event type, timestamp, relative delta, highlights).
6. **FastAPI routes**:
   - `GET /` – search form + recent sessions list.
   - `GET /conversations/{conversation_id}` – HTML timeline view.
   - `GET /api/conversations/{conversation_id}` – raw JSON payload (used by UI for progressive enhancement).
7. **Templates**: base layout + conversation timeline with collapsible event details.
8. **Static assets**: minimal CSS for “gorgeous” timeline, optional JS for toggles.
9. **Packaging**: `pyproject.toml` (hatch/uv) for FastAPI, Jinja2, uvicorn.

## Implementation Steps
1. Scaffold `tools/session_viewer/` structure, pyproject, package init.
2. Implement `config.py` to resolve Codex home and session directories.
3. Build `search.py` to map conversation IDs to file paths (glob through sessions hierarchy) and to list recent sessions.
4. Create `models.py` + `parser.py` to parse JSONL into structured events (SessionMeta, UserMessage, ModelReply, ToolCall, ExecCommand, Notice, Error, etc.).
5. Write `summary.py` to produce timeline cards with:
   - timestamp (absolute + relative)
   - icon/color per event type
   - title/body text
   - optional payload preview (command, tool, outputs) with truncation.
6. Build FastAPI `app.py` with routes, dependency injection for config, and Jinja templates.
7. Author templates (`base.html`, `conversation.html`, `index.html`) and CSS for the timeline.
8. Add CLI entry (`python -m tools.session_viewer.app` or `uvicorn ...`).
9. Document usage in README_extensive (how to install deps, run server, open browser).
10. Verification:
    - Launch server via uvicorn (maybe `@start_server.sh` if needed).
    - Call API + HTML routes with existing session ID.
    - Use Chrome DevTools MCP (or Playwright) to render page, capture PNG, and inspect.

## Validation Checklist
- ✅ JSONL parsing handles all event types (log warnings for unknown types, display raw JSON).
- ✅ HTML timeline loads for actual session from `~/.codex/sessions`.
- ✅ Screenshot demonstrating final UI is captured and reviewed.
- ✅ README_extensive updated with instructions.
- ✅ All scripts formatted/linted (ruff/black optional if configured).
