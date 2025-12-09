# Codex Profiles & Custom Configs

Codex lets you load alternate configuration files at launch, so you can swap between
specialized “profiles” without editing `~/.codex/config.toml`. This document tracks the
profiles we maintain in this repository and how to run them with the CLI.

## How profile configs work

- Every config lives as a standalone TOML (for example `docs/web-agent.toml` can be
  copied straight to `~/.codex/web-agent.toml`).
- Launch Codex with `--config-file /path/to/profile.toml` to use that file without
  touching `~/.codex/config.toml`.
- Optional: add `--config-home DIR` if you want the profile to use a dedicated
  `$CODEX_HOME` (auth, logs, sessions).
  ```bash
  codex-dev --config-home ~/.codex-web --config-file ~/.codex/web-agent.toml
  ```
- Profiles can also set `sandbox_mode` or `notice` flags, so everything (tool policy,
  sandbox, hooks) is recorded in one file.

## Current profiles

### Web Agent (Chrome DevTools MCP)

- **Location:** see `docs/web-agent.toml`.
- **Purpose:** give Codex a browser-focused toolset (Chrome DevTools MCP + shell/read
  tools) for on-screen automation.
- **Key settings:**
  - `sandbox_mode = "danger-full-access"` so Chrome can spawn its own sandbox.
  - Chrome DevTools MCP command: `npx -y chrome-devtools-mcp@latest --isolated ...` with
    a custom user agent and headful Chrome to avoid anti-bot heuristics.
  - Built-in tools limited to `shell`, `update_plan`, `view_image`, and `web_search`.
  - Notices to hide the max-model upgrade nudge.
- **Usage:**
  1. Copy `docs/web-agent.toml` to `~/.codex/web-agent.toml`.
  2. Launch Codex dev build:
     ```bash
     codex-dev --config-file ~/.codex/web-agent.toml
     ```
  3. Optional: add `--config-home ~/.codex-web` if you want separate auth/session logs
     for browser automation.

### Web Agent AIPC

- **Location:** see `docs/web-agent-aipc.toml`.
- **Purpose:** same browsing-centric surface as the default Web Agent, but scoped to a
  separate profile name (`web-agent-aipc`) so you can run two configurations side by
-  side (for example, one tuned for an AIPC workspace).
- **Key settings:** identical Chrome DevTools MCP command + tool restrictions, plus
  an explicit base prompt extension that requires the agent to end every reply with
  `<status>SUCCESS</status>` or `<status>FAILURE</status>`. The profile also
  registers both `tool_hook_command` and `stop_hook_command` so that every tool call
  and final turn summary is appended to `~/.codex/aipc_tool_calls.jsonl` and
  `~/.codex/aipc_turns.jsonl` via the bundled `tool_hook_logger.py` helper.
- **Usage:** copy the TOML to `~/.codex/web-agent-aipc.toml` and launch with
  `codex-dev --config-file ~/.codex/web-agent-aipc.toml` (plus `--config-home` if you
  want isolated auth/logs for the AIPC profile).

### Orchestrator / ask_colleague

- **Location:** see `docs/orchestrator-profile.toml`.
- **Purpose:** adds both the `ask_colleague` helper (spawns `codex-dev` recursively) and
  the `web_form_colleague` helper (dispatches the Chrome DevTools web agent Elise uses), so
  you can coordinate multi-agent work or farm out browser automation without leaving the CLI.
- **Key settings:** mirrors the default CLI config, keeps the sandbox at `workspace-write`, and
  registers both helpers under `[custom_tools]` (`ask_colleague` for parallel Codex threads,
  `web_form_colleague` for the DevTools workflow).
- **Usage:** launch Codex with `codex-dev --config-file ~/.codex/orchestrator-profile.toml`
  (plus `--config-home` if you want an isolated Codex home). Inside the session, call
  `ask_colleague(prompt="Draft release notes")` to spin up a new colleague or
  `ask_colleague(prompt="Please continue", id="gentle-otter")` to resume. When you need
  hands-on UI work, call `web_form_colleague(instructions="Fill the onboarding form", context="..." )`
  and wait for the hibernated turn to resume automatically once the DevTools agent reports back.

## Tips

- Keep profile files under version control (for example in `~/.codex/profiles/`) so you
  can audit changes.
- Use `codex-dev --config-file profile.toml --config foo.bar=value` to temporarily tweak
  a profile without editing the file.
- When a profile needs custom MCP servers, test them with `codex mcp list` and
  `codex mcp get <name>` using the same `--config-file` arguments to confirm they’re
  registered correctly.

Contribute new profiles by adding their docs (similar to the web-agent file) and listing
them here so future users know how to load them.***
