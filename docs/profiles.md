# Codex Profiles & Custom Configs

Codex lets you load alternate configuration files at launch, so you can swap between
specialized “profiles” without editing `~/.codex/config.toml`. This document tracks the
profiles we maintain in this repository and how to run them with the CLI.

## How profile configs work

- Every config lives as a standalone TOML (for example `docs/web-agent-config.md` has a
  copy-paste block that you can save as `~/.codex/web-agent.toml`).
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

- **Location:** see `docs/web-agent-config.md`.
- **Purpose:** give Codex a browser-focused toolset (Chrome DevTools MCP + shell/read
  tools) for on-screen automation.
- **Key settings:**
  - `sandbox_mode = "danger-full-access"` so Chrome can spawn its own sandbox.
  - Chrome DevTools MCP command: `npx -y chrome-devtools-mcp@latest --isolated ...` with
    a custom user agent and headful Chrome to avoid anti-bot heuristics.
  - Built-in tools limited to `shell`, `update_plan`, `view_image`, and `web_search`.
  - Notices to hide the max-model upgrade nudge.
- **Usage:**
  1. Copy the TOML snippet in `docs/web-agent-config.md` to `~/.codex/web-agent.toml`.
  2. Launch Codex dev build:
     ```bash
     codex-dev --config-file ~/.codex/web-agent.toml
     ```
  3. Optional: add `--config-home ~/.codex-web` if you want separate auth/session logs
     for browser automation.

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
