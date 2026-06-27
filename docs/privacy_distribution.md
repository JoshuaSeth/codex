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
env RUSTUP_TOOLCHAIN=1.95.0-x86_64-unknown-linux-gnu \
  CARGO_TARGET_DIR=codex-rs/target-privacy-release \
  CARGO_PROFILE_RELEASE_LTO=false \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo build --manifest-path codex-rs/Cargo.toml -p codex-cli --bin codex --release

python3 scripts/build_privacy_distribution.py \
  --codex-bin codex-rs/target-privacy-release/release/codex \
  --version v0.0.0-privacy.20260627 \
  --target linux-x86_64 \
  --out-dir dist/privacy-release
```

The builder emits:

```text
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.manifest.json
dist/privacy-release/homebrew/pitchai-codex-privacy.rb
dist/privacy-release/pitchai-codex-privacy-0.0.0-privacy.20260627.tgz
```

Current internal artifact:

```text
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
sha256 f4200df4692d85184e5536c5f2b32dd3c41c1a5d6c3f6fe664e8b8512982847b
```

Canonical GitHub Release URL:

```text
https://github.com/JoshuaSeth/codex/releases/download/v0.0.0-privacy.20260627/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
```

Release page:

```text
https://github.com/JoshuaSeth/codex/releases/tag/v0.0.0-privacy.20260627
```

The release-profile binary inside the archive is
`codex-rs/target-privacy-release/release/codex`, size `1381427024`, SHA-256
`35ffe2040133bd2a4d2aedf501acb7d691028c9d9921d8ec96a9b86c922049c0`.
Rust is not required on target machines for this artifact.

## Install From Tarball

```bash
tar -xzf pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
./pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64/install.sh "$HOME/.local"
codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

The wrapper enables `PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1` and points
`PITCHAI_CODEX_PRIVACY_FILTER_CMD` at the bundled OpenAI
`openai/privacy-filter` adapter.

Target machines need `uv`, Python 3.12 support, and internet or a prewarmed
cache for the first `openai/privacy-filter` model load. The archive contains
the compiled Codex binary, wrapper, install script, README, and privacy adapter
scripts. It does not contain secrets, model weights, or reversible mapping data.

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

The current generated npm tarball is internal/private and contains the same
compiled binary plus wrapper. It contains no secrets.

```bash
npm install -g ./dist/privacy-release/pitchai-codex-privacy-0.0.0-privacy.20260627.tgz
codex-privacy --version
codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

For normal npm distribution, publish this package to a private PitchAI registry
or split it into upstream-style platform packages. The npm launcher requires
Node 18+, `uv`, and Python 3.12 support on the target machine.

## Validation

```bash
python3 scripts/validate_privacy_distribution.py \
  dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz \
  --detector-cmd "uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" \
  --timeout 1200
```

The clean-install validation installs into a temporary prefix, runs the installed
binary against the local network-capture mock, uses the OpenAI
`openai/privacy-filter` adapter, verifies outbound payloads contain no original
PII, and verifies local stdout restores the original PII.

Release-binary network proof:

```bash
scripts/privacy_network_probe.py \
  --codex codex-rs/target-privacy-release/release/codex \
  --detector-cmd "uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" \
  --out docs/privacy_network_probe_20260627.json \
  --timeout 1200
```

`docs/privacy_network_probe_20260627.json` records:

- `captured_request_contains_real_values: []`
- `captured_relevant_user_texts_contains_real_values: []`
- fake outbound text such as `Quinn Bennett`, `7722 Walnut St`, and
  `casey.brooks@example.net`
- restored stdout values `Jane Smith`, `14 Pearl St`,
  `jane.smith@example.com`, and `(415) 555-1212`
