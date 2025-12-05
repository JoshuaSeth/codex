# Web-Agent Config (Chrome DevTools + minimal built-ins)

This example `config.toml` locks Codex down so it behaves like a browser-focused agent:

- Only the Chrome DevTools MCP server is registered.
- Built-in tools are trimmed to `shell`/`shell_command`, `update_plan`, `view_image`, and `web_search`.
- `apply_patch` and other file-editing helpers stay disabled, so Codex can read files but not modify them.
- Developer instructions reinforce that the browser MCP is the primary interface.

Save the snippet below as `~/.codex/web-agent.toml` (or any path you prefer) and launch Codex with:

```bash
codex --config-file ~/.codex/web-agent.toml
# …or for the dev build:
codex-dev --config-home ~/.codex-dev --config-file ~/.codex/web-agent.toml
```

```toml
# Activate the dedicated profile
profile = "web-agent"
sandbox_mode = "danger-full-access"    # disable Seatbelt so Chrome can spawn

################################################################################
# Global feature toggles – only expose what the web agent needs
################################################################################
[features]
view_image_tool = true          # allow attaching local screenshots
web_search_request = true       # permit web_search tool calls
apply_patch_freeform = false    # keep apply_patch disabled
unified_exec = false            # stick to the default shell tool (bash-style)

################################################################################
# Chrome DevTools MCP – the only MCP endpoint we register
################################################################################
[mcp_servers.chrome_devtools]
command = "npx"
args = [
  "-y",
  "chrome-devtools-mcp@latest",
  "--isolated",
  "--logFile", "/tmp/chrome-devtools-mcp.log",
  "--chrome-arg=--user-agent=Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.6668.90 Safari/537.36",
]
enabled = true
startup_timeout_sec = 20
tool_timeout_sec = 30
# No allow/deny lists → all tools exposed by Chrome DevTools MCP stay available.

################################################################################
# Web-agent profile
################################################################################
[profiles.web-agent]
model = "gpt-5.1-codex"
model_provider = "openai"
approval_policy = "on-request"          # still ask before risky commands
sandbox_mode = "danger-full-access"     # disable per-tool sandboxing (required for Chrome)
model_reasoning_effort = "high"         # encourage deeper thinking without max cost
developer_instructions = """
You are operating as the Codex Web Agent. The Chrome DevTools MCP is your
primary actuator: prefer it for every browsing/navigation action. You may use
shell commands solely for lightweight read-only inspection (`cat`, `rg`, etc.).
Do not run build systems, package managers, or mutating commands. Never call
apply_patch or other write tools—they are intentionally disabled.
"""

[profiles.web-agent.features]
view_image_tool = true
web_search_request = true
apply_patch_freeform = false
unified_exec = false

[profiles.web-agent.tools]
view_image = true
web_search = true

[notice]
hide_rate_limit_model_nudge = true
hide_gpt-5.1-codex-max_migration_prompt = true

################################################################################
# Optional: pin the working directory Codex should treat as the workspace.
################################################################################
# [profiles.web-agent.sandbox_workspace_write]
# writable_roots = ["/Users/you/projects/web-agent"]
# network_access = false
```

### Notes

- The Chrome DevTools binary path (`command` + `args`) should match where you installed the MCP server. Replace `/usr/local/bin/chrome-devtools-mcp` and the profile directory with your actual values.
- `enabled_tools` mirrors the tool names advertised by the Chrome DevTools MCP. Trim or extend the allow-list as your automation surface changes.
- Because the `apply_patch` feature is disabled, Codex will not surface patch/edit tools to the LLM, so the prompt only ever lists the shell, update_plan, view_image, and web_search capabilities plus **all** Chrome DevTools MCP tools exposed by the server.
- Chrome must run outside Codex’s Seatbelt sandbox. Leaving `sandbox_mode = "danger-full-access"` in this profile (or launching Codex with `--sandbox danger-full-access`) is required; otherwise the MCP transport closes immediately during startup.
- The `--chrome-arg=--user-agent=…` entry sets a natural desktop user agent to avoid basic bot/geolocation heuristics. Adjust it (or add other `--chrome-arg=` lines) if the target site needs a different profile or language.
- Headful browsing tends to evade additional bot detection heuristics, so leave `--headless` off unless you explicitly need headless mode. Add `--headless` back if the environment doesn’t have a display server.
