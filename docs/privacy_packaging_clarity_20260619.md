# Privacy Codex packaging clarification

Date: 2026-06-19

## Current artifact

Published prerelease:

https://github.com/JoshuaSeth/codex/releases/tag/v0.0.0-privacy.20260618

Current Linux artifact:

https://github.com/JoshuaSeth/codex/releases/download/v0.0.0-privacy.20260618/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz

SHA-256:

```text
5b3885bb0a7399a1568a27b2436f9bc4780199a736805bac11f1f93b7e486f61
```

The tarball contains exactly this top-level layout:

```text
pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64/
  README.md
  bin/codex
  bin/codex-privacy
  install.sh
  privacy/privacy_filter_fixture.py
  privacy/privacy_filter_gliner.py
  privacy/privacy_filter_openai.py
```

This is not a Rust source distribution. It bundles a compiled `bin/codex`
runtime, a `codex-privacy` shell wrapper, install script, README, and the local
privacy-filter adapter scripts. Rust is not required on the target machine.

The target machine does need:

- Linux x86_64 with compatible glibc/OpenSSL runtime libraries for the current
  GNU-linked debug artifact.
- `uv`, because the wrapper launches the OpenAI privacy-filter adapter through
  `uv run`.
- Python/model dependencies resolved by `uv` on first privacy-filter execution.

The current wrapper sets:

```bash
PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1
PITCHAI_CODEX_PRIVACY_FILTER_CMD='uv run --python 3.12 --with transformers>=4.53.0 --with torch --with accelerate python .../privacy_filter_openai.py'
```

Then it execs the bundled `bin/codex`.

## Build profile status

As of 2026-06-19, the current tarball uses
`codex-rs/target/release/codex`, confirmed by the distribution manifest:

```json
"codex_binary": "/code/pitchai-cli-new/vendor/codex/codex-rs/target/release/codex"
```

Measured locally:

```text
codex-rs/target/release/codex: 1.2G
distribution directory:         1.2G
tar.gz archive:                 279M
```

The previous prerelease tarball used a debug-profile binary. That was still a
normal compiled Rust executable, not source code and not a runtime JIT. The
practical differences versus the current release-profile artifact were:

- Performance: slower startup/runtime than an optimized release build.
- Size: much larger binary and archive because debug builds retain more
  unoptimized code/debug information.
- Safety/privacy: the privacy behavior is the same code path already validated
  by tests and network probes. Debug profile does not by itself disable the PII
  middleware or send mappings upstream.
- Distribution quality: acceptable as an internal proof artifact, not the final
  fleet artifact.

The current artifact fixes the main debug-profile issue by using Rust's
`release` profile (`opt-level=3`, ThinLTO). The remaining distribution-quality
gap is that it is still a GNU Linux package produced by our custom privacy
builder, not yet the upstream-style musl package with package checksum bundle
and signing/notarization flow.

## Did release build fail?

No. The earlier release build did not fail with a Rust compiler error. It was
unfinished/too slow during the previous local packaging pass, at the final
`codex-cli` compile/link stage with `-C opt-level=3 -C lto=thin`.

On 2026-06-19, rerunning the same command from the partially warm target cache
completed successfully:

```text
cargo build -p codex-cli --bin codex --release
Finished `release` profile [optimized + debuginfo] target(s) in 9m 32s
```

Current evidence from this worktree:

- `codex-rs/Cargo.toml` sets `[profile.release] lto = "thin"` and
  `codegen-units = 4`.
- `codex-rs/target/release/codex` now exists and is the binary used by the
  rebuilt tarball.
- A fresh 2026-06-19 release attempt reached `codex-cli`, showed an active
  `rustc` process with `-C opt-level=3 -C lto=thin`, then completed with exit 0.

So the precise statement is: the release build did not fail; earlier it was too
slow/unfinished locally, and the follow-up run completed successfully.

## Upstream OpenAI Codex distribution

Official Codex manual:

- The Codex CLI is open source and built in Rust.
- OpenAI serves standalone installer scripts from
  `https://chatgpt.com/codex/install.sh` and
  `https://chatgpt.com/codex/install.ps1`.
- Installer environment variables include `CODEX_INSTALL_DIR`,
  `CODEX_NON_INTERACTIVE`, and `CODEX_HOME`.

Official OpenAI Codex release metadata:

- Latest checked upstream release: `rust-v0.141.0`, published 2026-06-18.
- Release URL: https://github.com/openai/codex/releases/tag/rust-v0.141.0
- It ships standalone package archives such as
  `codex-package-x86_64-unknown-linux-musl.tar.gz`.
- It also ships `codex-package_SHA256SUMS`, installer scripts, direct native
  binary archives, npm tarballs such as `codex-npm-linux-x64-0.141.0.tgz`, and
  Python wheel packages such as
  `openai_codex_cli_bin-0.141.0-py3-none-manylinux_2_17_x86_64.whl`.

The upstream installer logic in `scripts/install/install.sh` prefers:

1. `codex-package-$target.tar.gz` plus `codex-package_SHA256SUMS`.
2. Legacy platform npm package, e.g. `codex-npm-linux-x64-$version.tgz`, only if
   the package archive is unavailable.

The upstream package builder documents the canonical package directory:

```text
codex-package.json
bin/<entrypoint>
codex-resources/
codex-path/rg
```

It says release jobs should pass `--cargo-profile release` and an explicit
target. Linux defaults to a musl target for release artifacts.

The upstream `rust-release` GitHub Actions workflow is tag-triggered, not
manually dispatched:

```text
on:
  push:
    tags:
      - "rust-v*.*.*"
```

It validates that the tag version matches `codex-rs/Cargo.toml`, uses 90-minute
build jobs, and builds Linux release artifacts on named large runners such as
`${repo}-linux-x64-xl` for `x86_64-unknown-linux-musl`. That is why the correct
PitchAI release path is a dedicated release tag/workflow or equivalent large
build host, not an ad hoc interactive shell build.

## Recommendation

For PitchAI privacy Codex, match upstream's release model:

1. Build a release-profile Codex binary/package in CI or on a large build host,
   not in the interactive operator session.
2. Target Linux as `x86_64-unknown-linux-musl` for server distribution and build
   macOS artifacts separately for Apple Silicon and Intel developer machines.
3. Publish GitHub Release assets as the source of truth:
   `pitchai-codex-privacy-package-x86_64-unknown-linux-musl.tar.gz` and
   `pitchai-codex-privacy-package_SHA256SUMS`.
4. Keep Homebrew and npm as wrappers/install channels around those release
   assets, not as the authoritative build output.
5. Verify each artifact in a clean container or clean VM by installing it,
   running `codex-privacy --version`, and running the privacy network probe that
   proves outbound payloads contain fake PII only and inbound text restores to
   original local PII.

Concrete release command shape:

```bash
python3 scripts/build_codex_package.py \
  --target x86_64-unknown-linux-musl \
  --cargo-profile release \
  --archive-output dist/privacy-release/pitchai-codex-privacy-package-x86_64-unknown-linux-musl.tar.gz \
  --force
```

Then stage the privacy wrapper/adapters into that package, publish checksums,
and update the Homebrew formula/npm wrapper to consume the package asset.

The immediate blocker to the final fleet-grade artifact is no longer the Rust
release binary itself. That binary now builds locally. What remains is producing
the upstream-shaped package on suitable build infrastructure:

- musl Linux target for servers, not this GNU Linux tarball;
- package layout with bundled resources/checksum file matching upstream's
  `codex-package-*` model;
- macOS release builds on macOS runners, including the signing/notarization
  path if distributing outside a private tap;
- clean-machine validation of each published asset.
