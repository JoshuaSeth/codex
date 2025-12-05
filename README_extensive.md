# Codex CLI – Extensive Technical Guide

This document grows over multiple **iterations**. Each pass adds more depth, concrete file references, and operational guidance so you can treat Codex as a programmable agent directly from this repository.

---

## Iteration 1 – Foundation

### 1.1 Workspace Topology
- `codex-rs/Cargo.toml` defines a single Rust 2024 workspace with >30 crates ranging from UI (`tui`) to sandbox helpers (`linux-sandbox`, `windows-sandbox-rs`) and remote services (`app-server`, `cloud-tasks`). This makes every binary (CLI, exec mode, MCP server, etc.) build from one toolchain. codex-rs/Cargo.toml:1-120codex-rs/Cargo.toml:121-200
- The TypeScript/PNPM workspace (`pnpm-workspace.yaml`) houses docs tooling, the shell MCP tool, and the TypeScript SDK, so cross-language utilities live beside the Rust core. pnpm-workspace.yaml:1-6

### 1.2 Building & Running
- `docs/install.md` walks the canonical flow: clone, `cd codex/codex-rs`, install Rust via `rustup`, run `cargo build`, then `cargo run --bin codex -- "...prompt..."`. These same steps allowed us to run Codex straight from source without touching the Homebrew/NPM installer. docs/install.md:17-40
- Because the workspace ships all binaries, `cargo run --bin codex` is equivalent to running the globally installed CLI—the binary automatically loads configuration and credentials from `$CODEX_HOME` (defaults to `~/.codex`). See §2.1 for details.

### 1.3 CLI Entry Points
- `codex-rs/cli/src/main.rs` defines the Clap multitool. With no subcommand it forwards args to the interactive TUI (`codex_tui::Cli`), while subcommands expose `exec`, `review`, `login`, `sandbox`, MCP helpers, app-server utilities, etc. codex-rs/cli/src/main.rs:42-220
- The CLI injects `CliConfigOverrides` everywhere, so any binary (TUI, exec mode, headless servers) can honor `-c key=value` flags without editing config files. codex-rs/cli/src/main.rs:57-90

---

## Iteration 2 – Internals & Credentials

### 2.1 Configuration Layer & `$CODEX_HOME`
- `ConfigToml` mirrors `~/.codex/config.toml`. It captures model/provider overrides, sandbox settings, notifications, instruction overrides, forced login modes, and more. Any CLI invocation eventually flows through `Config::load_with_cli_overrides`, so runtime state is a merge of on-disk config and CLI overrides. codex-rs/core/src/config/mod.rs:560-660
- `find_codex_home()` resolves `$CODEX_HOME` or defaults to `~/.codex`, meaning a source-built binary and the packaged CLI share configuration/credential state unless you override this env var. codex-rs/core/src/config/mod.rs:1337-1362
- To isolate dev/test runs without touching the shell environment, pass `--config-home DIR` (mirrors `CODEX_HOME`) or `--config-file FILE` (points at an arbitrary TOML). `CliConfigOverrides` registers both flags and sets the `CODEX_HOME`/`CODEX_CONFIG_FILE` env vars before config loads, while `config_file_path()` ensures every read/write honors the override. codex-rs/common/src/config_override.rs:40-145codex-rs/core/src/config/mod.rs:1346-1384

### 2.2 Authentication Pipeline
- Auth material is stored via `AuthDotJson` (fields: `OPENAI_API_KEY`, OAuth tokens, `last_refresh`) either in `auth.json` or the OS keyring, depending on `cli_auth_credentials_store_mode`. codex-rs/core/src/auth/storage.rs:30-123
- Helper functions allow multiple entry points:
  - `read_openai_api_key_from_env` and `read_codex_api_key_from_env` honor `OPENAI_API_KEY` and `CODEX_API_KEY`. The latter is exec-mode-only and intentionally bypasses persistent auth when present. codex-rs/core/src/auth.rs:294-309
  - `login_with_api_key` writes just the API key to storage, while OAuth flows use the login server described in §2.4. codex-rs/core/src/auth.rs:321-344
- `AuthManager` owns cached credentials for the entire process. It loads once (respecting env overrides), hands cloned `CodexAuth` handles to subsystems, and reloads on demand to avoid inconsistent auth views mid-run. codex-rs/core/src/auth.rs:1080-1180

### 2.3 Runtime Orchestration
- `ConversationManager` is the façade used by the CLI, exec mode, MCP server, and app server. It takes an `AuthManager`, spawns new conversations, resumes existing rollouts, and tracks `ConversationId` → `CodexConversation` mappings. codex-rs/core/src/conversation_manager.rs:27-200
- `Codex::spawn` wires the deeper stack: builds a `SessionConfiguration` (model/provider/instructions/sandbox/CWD), loads execpolicy/tool config, starts a `Session`, and launches the async submission loop that feeds the agent while emitting `Event` structs. codex-rs/core/src/codex.rs:120-230
- Each `Session` owns `TurnContext` state (sandbox policies, tool config, cwd) and wraps `ModelClient` interactions; the CLI simply sends `Op::UserInput` submissions and streams turn events back. codex-rs/core/src/codex.rs:263-420

### 2.4 Login Modes & Headless Strategies
- Documentation (`docs/authentication.md`) formally supports two flows: ChatGPT login via a localhost OAuth helper (`codex login` spawns the server on port 1455) and usage-based `OPENAI_API_KEY` piped through `codex login --with-api-key`. The same instructions explain copying `auth.json` to headless machines because the file is host-agnostic. docs/authentication.md:3-68
- The CLI login implementation (`codex-rs/login/src/server.rs`) sets up PKCE, opens a browser, receives tokens, and persists them with whatever credential backend Config selected. (See module for deeper study in future iterations.)

---

## Iteration 3 – Agent Behaviors, Integrations, and Ops

### 3.1 Treating Codex as an Agent
- Codex always runs inside a sandbox+approval policy pair. Documentation clarifies presets such as read-only, workspace-write (a.k.a. `--full-auto`), and the intentionally dangerous full-access mode. Platform-specific enforcement uses Seatbelt (macOS), Landlock+seccomp (Linux), and restricted tokens (Windows). docs/sandbox.md:1-80
- CLI flags (`--sandbox`, `--ask-for-approval`) and config keys (`sandbox_mode`, `[sandbox_workspace_write]`) give the agent autonomy without sacrificing guardrails. This is vital when running `cargo run --bin codex` directly in your repo: the behavior mirrors the packaged CLI, including sandbox warnings (see the command run in §4).
- New hook support lets you set `tool_hook_command` to any executable (for example, a Python logger). Codex runs it before and after every tool call with JSON on stdin describing the tool name, arguments, and eventual result. The config knob lives at the top level (`tool_hook_command = ["python3", "~/scripts/log_tool_calls.py"]`); see `docs/config.md` plus the runtime implementation in `codex-rs/core/src/tools/hooks.rs`. docs/config.md:351-369codex-rs/core/src/tools/hooks.rs:1-154

#### Hook payload details
- **Phases:** The hook receives `{ "phase": "before_execution", ... }` right before the tool runs and `{ "phase": "after_execution", ... }` afterwards. If execution fails, `outcome` is `{"error": {"message": "..."}}`; otherwise you get `{"success": {"response": <ResponseInputItem>}}`. codex-rs/core/src/tools/hooks.rs:1-164
- **Call snapshot:** Each payload includes `call = { tool_name, call_id, payload }`. The payload mirrors the original request: function calls include raw arguments *and* a best-effort `parsed_arguments` (JSON value), custom tools carry their raw `input`, local shell tools expose `command`, `workdir`, and timeout metadata, while MCP calls include `server`, `tool`, and `raw_arguments`. codex-rs/core/src/tools/hooks.rs:60-118
- **Transport:** Hooks are invoked via `tool_hook_command` (array of argv tokens). Codex writes the JSON payload to stdin and inherits stdout/stderr; non-zero exits are logged but never halt the agent. This makes it safe to point the hook at scripts that append to JSONL, forward to sockets, etc. codex-rs/core/src/tools/hooks.rs:1-80

### 3.2 Model Providers & Extensibility
- Built-in providers include OpenAI (requires login) and two OSS slots aimed at Ollama/LM Studio. Users extend this via `[model_providers.*]` blocks in `config.toml`, optionally supplying custom env vars, headers, or alternative wire APIs (Responses vs Chat Completions). The `ModelProviderInfo` registry handles env overrides like `OPENAI_BASE_URL` as well. docs/config.md:60-160codex-rs/core/src/model_provider_info.rs:220-320
- Because providers live in config, the same binary can flip between ChatGPT credentials and API-key-backed OSS models simply by changing configuration or CLI overrides—no rebuild required.

### 3.3 Non-interactive & Programmatic Interfaces
- `codex exec` (documented in `docs/exec.md`) is the headless automation surface. It defaults to read-only sandboxing, streams structured JSON events, supports JSON Schema constrained output, and can resume previous sessions. Exporting `CODEX_API_KEY` lets you run exec jobs without touching stored ChatGPT credentials. docs/exec.md:1-114
- The app server (`codex-rs/app-server/README.md`) exposes a JSON-RPC protocol over stdio with thread/turn/item primitives—the same infrastructure used by the official IDE extension, so you can embed Codex in other clients without the CLI front-end. codex-rs/app-server/README.md:1-140
- The TypeScript SDK (`sdk/typescript/README.md`) wraps the CLI binary, spawning it and exchanging JSONL events. It keeps the CLI’s required env vars (e.g., `OPENAI_BASE_URL`, `CODEX_API_KEY`) so Node/Electron apps inherit the same agent behavior. sdk/typescript/README.md:1-132

### 3.4 Authentication & Secrets in Depth
- For API-key billing, official guidance is to pipe the key into `codex login --with-api-key` or feed `OPENAI_API_KEY`/`CODEX_API_KEY` via environment variables to avoid shell history leaks. Copying `auth.json` between hosts is the supported shortcut for headless servers. docs/authentication.md:3-57
- OAuth logins rely on the helper server described in `codex-rs/login/src/server.rs` (PKCE, localhost callback). Credentials are saved via `AuthCredentialsStoreMode`, so you can choose `file`, `keyring`, or `auto` per environment. (Full PKCE details can be cited in a future iteration if needed.)

### 3.5 Runtime Ops & Observability
- `Codex::spawn` wires in `codex_execpolicy`, `codex_otel`, and `TurnDiffTracker`, which is why you can capture OpenTelemetry traces and enforce execpolicy rules across both CLI and app-server launches. codex-rs/core/src/codex.rs:120-210
- Session logs and rollouts go under `$CODEX_HOME` (log dir via `log_dir()`), so provenance is consistent whether you run from source or prebuilt binaries. codex-rs/core/src/config/mod.rs:1344-1369

---

## Iteration 4 – Field Notes & Ongoing Expansion

1. **Future Deep Dives** (planned):  
   - Login server internals (`codex-rs/login/src/server.rs`) to document PKCE, token refresh, and forced workspace enforcement.  
   - Execpolicy evaluation flow to explain how policy files gate tool execution.  
   - MCP client/server wiring for advanced toolchains.

2. **Hands-on Verification** (latest run):  
   - After installing Rust via `rustup`, we executed `source ~/.cargo/env && cargo run --bin codex -- exec "how many files are in current dir?"` inside `/Users/sethvanderbijl/codex/codex-rs`. Codex launched with the expected agent banner, honored the sandbox, and returned “53 entries,” proving the source build picked up the existing ChatGPT credentials automatically.

3. **Contributions Welcome**: add new sections directly below this iteration, referencing files/lines so the README stays actionable for future maintainers.

---


## Iteration 5 – Packaging & Config Isolation

### 5.1 Shipping a Local `codex-dev`
- Treat `codex` as the official distribution and `codex-dev` as “whatever binary was produced from the working tree.” Keeping them separate avoids collisions with the npm/brew releases while letting you dogfood experimental patches.
- Build/rebuild with:
  ```bash
  cd codex/codex-rs
  cargo build -p codex-cli --release             # or omit --release for faster debug builds
  ln -sf $(pwd)/target/release/codex ~/.local/bin/codex-dev
  codex-dev --version
  ```
  Running the `cargo build` step after *every* Rust edit is the only way to ensure `codex-dev` picks up your latest code. Release builds mimic the shipped binary; debug builds are faster to iterate on but slower at runtime.
- Both CLIs read from `~/.codex` by default. Keep their state isolated by running `codex-dev --config-home ~/.codex-dev` (or by pointing `CODEX_CONFIG_FILE` at a dev-only TOML). This also makes it easy to test new knobs like `[custom_tools]` without polluting your primary install. codex-rs/common/src/config_override.rs:17-120
- If you prefer a one-liner installer, `cargo install --locked --path codex-rs/cli --bin codex --root ~/.local` followed by `mv ~/.local/bin/codex ~/.local/bin/codex-dev` reproduces the same layout as above. Re-run the `cargo install` command whenever you need to refresh the binary.

### 5.2 CLI-Level Config Overrides
- Every entry point now accepts `--config-home DIR` (override `$CODEX_HOME`) and `--config-file FILE` (load an arbitrary TOML). These flags live on `CliConfigOverrides`, propagate via `prepend_from`, and therefore affect the interactive CLI, `codex exec`, the MCP/app servers, etc. codex-rs/common/src/config_override.rs:17-165codex-rs/cli/src/main.rs:431-690codex-rs/exec/src/main.rs:17-45codex-rs/tui/src/main.rs:9-34
- Under the hood they call `set_codex_home_override` / `set_config_file_override`, which cache absolute paths in `OnceLock`s. That means *all* downstream code—`Config::load_with_cli_overrides`, `ConfigEditsBuilder`, managed-config loaders—resolve paths through the override without mutating global env vars. codex-rs/core/src/config/mod.rs:46-210codex-rs/core/src/config/edit.rs:1-120codex-rs/core/src/config_loader/mod.rs:1-140
- Practical recipes:  
  1. `codex-dev --config-home ~/.codex-dev exec "status"` isolates auth, session logs, and hook output for the dev build.  
  2. `codex --config-file ./ci/replay.toml exec "run smoke tests"` pins CI runs to a checked-in config while still using the default home for auth/keyrings.

### 5.3 Conversation Visualizer
- Added `tools/session_viewer/`, a FastAPI micro-app that parses `$CODEX_HOME/sessions/**/rollout-*.jsonl` and renders them as a clickable timeline with icons, deltas, and raw payloads. tools/session_viewer/session_viewer/app.py:1-90tools/session_viewer/session_viewer/parser.py:1-67tools/session_viewer/session_viewer/templates/conversation.html:1-39
- Install and run:
  ```bash
  cd tools/session_viewer
  pip install -e .
  uvicorn session_viewer.app:app --reload --port 8001
  ```
  Open `http://localhost:8001`, paste a conversation UUID (or pick from the recent list), and the visualizer loads from your local session logs.

### 5.4 Config-defined CLI Tools
- You can now declare first-class tools directly inside `config.toml` under `[custom_tools.<name>]`. Each entry mirrors an OpenAI function tool: provide an argv array, optional JSON Schema, description, env vars, and timeout/escalation knobs. During startup `Config::load_with_cli_overrides` resolves the table into `CustomToolConfig` structs, and `ToolsConfig::new` emits `ToolSpec::Function` definitions plus a dedicated handler that shells out via the same sandbox used by the builtin shell tool. codex-rs/core/src/config/mod.rs:580-720codex-rs/core/src/tools/spec.rs:33-230codex-rs/core/src/tools/handlers/custom.rs:1-96
- Minimal example (drop this into `~/.codex/config.toml` or a dev override file) demonstrating the bundled `tools/custom_tools/echo_tool.py` helper:
  ```toml
  [custom_tools.echo_demo]
  command = ["python3", "/path/to/codex/tools/custom_tools/echo_tool.py"]
  description = "Echoes text using CODEX_TOOL_ARGS_JSON"
  # Optional JSON schema; omit to accept an empty object.
  parameters = { type = "object", properties = { text = { type = "string" } }, required = ["text"] }
  timeout_ms = 4000
  [custom_tools.echo_demo.env]
  CUSTOM_TOOL_PREFIX = "dev-build: "
  ```
  At runtime Codex injects three env vars—`CODEX_TOOL_ARGS_JSON`, `CODEX_TOOL_NAME`, and `CODEX_TOOL_CALL_ID`—before running the command under the session sandbox/approval policy. The script can log/emit anything; Codex captures stdout/stderr, renders it in rollouts, and surfaces the JSON-formatted result back to the model as a normal function call output.
- Validation: we added an integration test (`config_defined_custom_tool_runs_command`) that wires a custom Python helper, lets the mock model request `custom.echo`, and asserts the tool output contains both the agent-provided text and the config-specified prefix. Running `cargo test -p codex-core config_defined_custom_tool_runs_command` exercises the entire pipeline (config parsing, tool registry, sandbox exec, telemetry, and SSE plumbing) without needing a live OpenAI connection.

*Last updated: \`README_extensive.md\` created as part of the deep-study task; extend it in further iterations as the system evolves.*
