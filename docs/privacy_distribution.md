# Privacy Codex Distribution

## Recommendation

Use GitHub Release assets as the internal source of truth, then expose the same
asset through Homebrew for macOS/Linux developers. Keep npm as a convenience
wrapper for Node-oriented environments, not the canonical binary store.

- Linux servers: install the GitHub Release tarball with `install.sh`.
- macOS developers: install from a PitchAI Homebrew tap formula that downloads
  the GitHub Release tarball and verifies SHA-256.
- Node/npm users: install the internal npm tarball or package after it is
  published to a private registry; the npm package wraps the same bundled
  `codex-privacy` executable.

This keeps one binary artifact per target and avoids divergent Brew/npm builds.

## Build

```bash
cargo build --manifest-path codex-rs/Cargo.toml -p codex-cli --bin codex
python3 scripts/build_privacy_distribution.py --codex-bin codex-rs/target/debug/codex
```

The builder emits:

```text
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.manifest.json
dist/privacy-release/homebrew/pitchai-codex-privacy.rb
dist/privacy-release/pitchai-codex-privacy-0.0.0-privacy.20260618.tgz
```

Published prerelease:

```text
https://github.com/JoshuaSeth/codex/releases/tag/v0.0.0-privacy.20260618
```

Linux x86_64 tarball:

```text
https://github.com/JoshuaSeth/codex/releases/download/v0.0.0-privacy.20260618/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz
```

The validated artifact in this worktree is built from a debug-profile Codex
binary because the full release ThinLTO build was CPU-active for an extended
period. The distribution path itself is release-compatible: pass a release-built
`--codex-bin` to the same builder when CI or a release host produces one.

## Install From Tarball

```bash
tar -xzf pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz
./pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64/install.sh "$HOME/.local"
codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

The wrapper enables `PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1` and points
`PITCHAI_CODEX_PRIVACY_FILTER_CMD` at the bundled OpenAI
`openai/privacy-filter` adapter.

## Homebrew

Commit `dist/privacy-release/homebrew/pitchai-codex-privacy.rb` to a private tap
such as `JoshuaSeth/homebrew-pitchai`, then install:

```bash
brew tap JoshuaSeth/pitchai
brew install pitchai-codex-privacy
```

For private GitHub assets, users need a token that Homebrew can use to download
the release asset.

The committed formula for this prerelease is:

```text
packaging/homebrew/pitchai-codex-privacy.rb
```

## npm

The local npm tarball can be installed directly:

```bash
npm install -g ./dist/privacy-release/pitchai-codex-privacy-0.0.0-privacy.20260618.tgz
codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

For normal npm distribution, publish the package to a private registry after the
binary artifact is produced for the target platform.

## Validation

```bash
python3 scripts/validate_privacy_distribution.py \
  dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz \
  --detector-cmd "python3 /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_fixture.py"
```

The clean-install validation uses a deterministic fixture detector for speed and
network capture. The primary detector remains the bundled OpenAI
`openai/privacy-filter` adapter, validated separately by
`scripts/privacy_lane_proof.py` and the Rust detector contract test.

Clean Linux container validation was run with:

```bash
docker run --rm \
  -v /code/pitchai-cli-new/vendor/codex/dist/privacy-release:/dist:ro \
  -v /code/pitchai-cli-new/vendor/codex/scripts/privacy_network_probe.py:/repo/scripts/privacy_network_probe.py:ro \
  -v /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_fixture.py:/fixture.py:ro \
  python:3.12-slim \
  bash -lc 'set -euo pipefail; mkdir -p /work /proof; tar -xzf /dist/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64.tar.gz -C /work; /work/pitchai-codex-privacy-v0.0.0-privacy.20260618-linux-x86_64/install.sh /tmp/prefix; python3 /repo/scripts/privacy_network_probe.py --codex /tmp/prefix/lib/pitchai-codex-privacy/bin/codex --detector-cmd "python3 /fixture.py" --out /proof/container_probe.json --timeout 180'
```
