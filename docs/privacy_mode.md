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

Build a downloadable Linux artifact:

```bash
scripts/package_privacy_artifact.sh
```

The archive is written to:

```text
dist/codex-privacy-linux-x86_64.tar.gz
```

After unpacking, run:

```bash
./codex-privacy exec "Jane Smith lives at 14 Pearl St."
```

The wrapper enables privacy mode and points Codex at the bundled OpenAI
privacy-filter adapter. The optional GLiNER adapter is packaged only as an alternate
detector for development; the primary supported path is `openai/privacy-filter`.
The fixture adapter is packaged for local network-capture tests only.

The validated artifact for the 2026-06-18 proof run is:

```text
dist/codex-privacy-linux-x86_64-debug.tar.gz
```

## Proof Commands

```bash
uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python scripts/privacy_lane_proof.py
PITCHAI_CODEX_PRIVACY_FILTER_CMD="uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate python /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_openai.py" cargo test --manifest-path codex-rs/Cargo.toml -p codex-core privacy::tests::model_backed_detector_contract_when_configured --lib -- --nocapture
scripts/privacy_network_probe.py --codex codex-rs/target/debug/codex --detector-cmd "python3 /code/pitchai-cli-new/vendor/codex/scripts/privacy_filter_fixture.py"
```
