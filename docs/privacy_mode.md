# Codex Privacy Mode

Privacy mode runs inside the vendored Codex runtime before request construction. When
`PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1` is set, outbound user text is scanned locally with
OpenAI `openai/privacy-filter` spans, exact span text is replaced with stable realistic
fake values for the session, and inbound assistant text is restored locally before it
is shown.

The local mapping table and session secret remain in process memory only. They are not
included in request payloads.

## Use From A Checkout

```bash
export PITCHAI_CODEX_PRIVACY_MIDDLEWARE=1
codex exec "Jane Smith lives at 14 Pearl St. Email jane.smith@example.com."
```

If `PITCHAI_CODEX_PRIVACY_FILTER_CMD` is not set, the runtime uses the bundled OpenAI
privacy-filter adapter at `scripts/privacy_filter_openai.py`.

To override the detector command:

```bash
export PITCHAI_CODEX_PRIVACY_FILTER_CMD="uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py"
```

## Packaged Artifact

Build the current Linux release artifact:

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

The validated archive is:

```text
dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
sha256 f4200df4692d85184e5536c5f2b32dd3c41c1a5d6c3f6fe664e8b8512982847b
```

After unpacking, run:

```bash
tar -xzf pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz
./pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64/install.sh "$HOME/.local"
codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

The wrapper enables privacy mode and points Codex at the bundled OpenAI
privacy-filter adapter. The optional GLiNER adapter is packaged only as an alternate
detector for development; the primary supported path is `openai/privacy-filter`.
The fixture adapter is packaged for local network-capture tests only.

Target machines do not need Rust for the packaged artifact. They need `uv`,
Python 3.12 support, and network/cache access the first time the bundled
Transformers adapter downloads `openai/privacy-filter`. The package contains no
secrets or mapping data; reversible mappings and the per-session secret are
created only in local process memory.

## Proof Commands

```bash
uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python scripts/privacy_lane_proof.py
PITCHAI_CODEX_PRIVACY_FILTER_CMD="uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" cargo test --manifest-path codex-rs/Cargo.toml -p codex-core privacy --lib
scripts/privacy_network_probe.py \
  --codex codex-rs/target-privacy-release/release/codex \
  --detector-cmd "uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" \
  --out docs/privacy_network_probe_20260627.json \
  --timeout 1200
python3 scripts/validate_privacy_distribution.py \
  dist/privacy-release/pitchai-codex-privacy-v0.0.0-privacy.20260627-linux-x86_64.tar.gz \
  --detector-cmd "uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" \
  --timeout 1200
```

The built-binary proof artifact is
`docs/privacy_network_probe_20260627.json`. It records that the captured
outbound request and backend-like fake response contained none of the original
PII, while local stdout restored `Jane Smith`, `14 Pearl St`,
`jane.smith@example.com`, and `(415) 555-1212`.
