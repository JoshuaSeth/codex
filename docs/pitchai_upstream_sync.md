# PitchAI Codex fork: upstream sync notes

This repository (`origin = JoshuaSeth/codex`) is a fork of upstream (`upstream = openai/codex`).

## Goal

Keep `origin/main` continuously up-to-date with `upstream/main` **while preserving PitchAI-specific features**:

- tool hooks (pre/post tool execution hooks)
- stop hooks (e.g., Telegram “final message” hook)
- config profiles (including repo-local `.codex/*.toml`)
- config-defined custom tools (`[custom_tools.*]`)
- pending-tool “hibernate/shutdown-after-call” behavior used by long-running automations
- PitchAI job runner scripts under `codex-cli/scripts/` (Elise, SharePoint inbox, generic job runner)

## Current sync method (recommended)

Use a **merge-based sync** (no rebase) so `origin/main` can be updated without force pushing.

### Steps

```bash
cd codex
git fetch upstream --tags
git checkout main
git merge --no-ff upstream/main
```

Resolve conflicts (typical hotspots):

- `codex-rs/core/src/config/*` (config layer stack and profile loading)
- `codex-rs/core/src/config_loader/*` (config layer discovery: `/etc/codex`, `~/.codex`, `.codex/config.toml`)
- `codex-rs/exec/src/event_processor_with_human_output.rs` (human output formatting + PitchAI events)
- docs and README changes (usually safe to take upstream wording)

Then validate:

```bash
cd codex-rs
just fmt
just fix -p codex-core
just fix -p codex-exec
cargo test -p codex-core
cargo test -p codex-exec
cargo build -p codex-cli
```

Finally:

```bash
cd ..
git push origin main
```

## Where PitchAI changes live

### Core runtime (Rust)

- Tool hooks: `codex-rs/core/src/tools/hooks.rs`
- Custom tool runtime: `codex-rs/core/src/tools/handlers/custom.rs`
- Tool registry/config plumbing: `codex-rs/core/src/tools/registry.rs`, `codex-rs/core/src/config/types.rs`
- Stop hook plumbing: `codex-rs/core/src/codex.rs`, `codex-rs/core/src/config/profile.rs`
- Pending tool hibernation: `codex-rs/core/src/pending_tools.rs`, `codex-rs/exec/src/pending_tool_ipc.rs`

### Job runners + agents (Python)

- Generic job runner: `codex-cli/scripts/pitchai_run_codex_job.py`
- Elise job runner: `codex-cli/scripts/pitchai_run_elise_job.py`
- SharePoint inbox job runner: `codex-cli/scripts/pitchai_run_sharepoint_inbox_job.sh`

### Profiles/config templates

- `.codex/orchestrator-profile.toml`
- `codex-cli/scripts/pitchai_*_config.toml`

## Why merges over rebases

Rebasing the PitchAI patch series on top of upstream is possible, but:

- it requires force-pushing `origin/main` (riskier operationally)
- it makes history harder to audit when ops scripts/jobs are involved

If we later want a clean patch stack, create a dedicated “patch branch” and cherry-pick only the minimal PitchAI runtime features onto it; keep job runners and docs as separate commits.

