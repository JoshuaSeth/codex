# Codex Session Viewer

FastAPI mini-app that visualizes Codex conversation rollouts stored under `$CODEX_HOME/sessions`.

## Installation

```bash
cd tools/session_viewer
pip install -e .
```

## Usage

```bash
cd tools/session_viewer
uvicorn session_viewer.app:app --reload
```

Then open <http://localhost:8000>, enter a conversation ID (UUID) from `~/.codex/sessions`, and explore the timeline.
