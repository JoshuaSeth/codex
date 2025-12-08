# Config-Defined Exec Tools Plan

## Objective
Let Codex expose bespoke CLI tools by reading `[custom_tools.<name>]` entries from `config.toml`. Each entry becomes a native function tool without relying on MCP servers. Tools must inherit sandbox/approval policies, support JSON arguments, and work across both the stock `codex` binary and our repo-built `codex-dev`.

## Constraints & Inputs
- Config parsing already loads TOML → `ConfigToml` → `Config`.
- Tool registry is built inside `ToolsConfig::new()`; specs + handlers live under `core/src/tools/`.
- We want ergonomic helpers so users can keep simple Python scripts under `tools/custom_tools/` and reference them from config.
- Hooks/loggers should stay opt-in (configured via `[profiles.<name>]` or root keys).

## Implementation Steps
1. **Config schema**
   - Extend `ConfigToml` with a `HashMap<String, CustomToolToml>` describing command, description, parameters (JSON schema), cwd, env, timeout, `with_escalated_permissions`, and `parallel` hints.
   - Normalize entries into deterministic `BTreeMap<String, CustomToolConfig>` on `Config` so downstream code has sorted iteration order.
2. **Tool spec registration**
   - Update `ToolsConfig::new` to iterate over `config.custom_tools`. For each entry emit a `ToolSpec::Function` with the provided description + JSON schema.
   - Generate `ToolId`s that match the config name (`custom.<name>` or similar) so the model can call them deterministically.
3. **Execution handler**
   - Implement `core/src/tools/handlers/custom.rs` that shells out using the stored command. Inject `CODEX_TOOL_ARGS_JSON`, `CODEX_TOOL_NAME`, and `CODEX_TOOL_CALL_ID` env vars before exec, capture stdout/stderr, and return it as a `ResponseInputItem::FunctionCallOutput` string payload. Honor cwd/timeouts/escalation flags consistent with `local_shell`.
4. **Docs & samples**
   - Ship helper scripts in `tools/custom_tools/` (echo, count files, send email stub) plus README instructions.
   - Update `README_extensive.md` + `docs/config.md` to teach people how to wire configs and how `codex-dev --config-home ~/.codex-dev --config-file docs/web-agent-aipc.toml` can load them.
5. **Validation**
   - Add targeted tests (e.g., `config_defined_custom_tool_runs_command`) ensuring argument plumbing + env var injection.
   - Manual smoke tests: start `codex-dev` with a config referencing `tools/custom_tools/echo_tool.py`; confirm `/mcp list tools` shows it and the agent can call it without being explicitly reminded.
6. **Future hooks**
   - Build opt-in hook commands (`tool_hook_command`, `stop_hook_command`) so ops teams can tap into tool executions and final turn summaries for auditing.

Status: Steps 1–5 implemented in the current branch; Step 6 is ongoing as we add stop-hook plumbing and richer config overrides.
