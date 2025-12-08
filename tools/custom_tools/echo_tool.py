#!/usr/bin/env python3
"""Example config-defined CLI tool that echoes JSON arguments.

Reads the CODEX_TOOL_ARGS_JSON environment variable, expects a JSON object
with a "text" field, and prints a prefixed message. This script is used in
README_extensive.md to demonstrate custom CLI tools.
"""

import json
import os
import sys
from datetime import datetime

PREFIX = os.environ.get("CUSTOM_TOOL_PREFIX", "Custom tool says: ")
ARGS_ENV = os.environ.get("CODEX_TOOL_ARGS_JSON", "{}")

try:
    payload = json.loads(ARGS_ENV)
except json.JSONDecodeError as exc:
    print(f"invalid CODEX_TOOL_ARGS_JSON: {exc}", file=sys.stderr)
    sys.exit(1)

text = payload.get("text", "")
stamp = datetime.utcnow().isoformat()
print(f"{PREFIX}{text} @ {stamp}")
