#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${ROOT}/dist/codex-privacy-linux-x86_64"
ARCHIVE="${TARGET_DIR}.tar.gz"

cargo build --manifest-path "${ROOT}/codex-rs/Cargo.toml" -p codex-cli --bin codex --release

rm -rf "${TARGET_DIR}"
mkdir -p "${TARGET_DIR}"

cp "${ROOT}/codex-rs/target/release/codex" "${TARGET_DIR}/codex"
cp "${ROOT}/scripts/codex-privacy" "${TARGET_DIR}/codex-privacy"
cp "${ROOT}/scripts/privacy_filter_openai.py" "${TARGET_DIR}/privacy_filter_openai.py"
cp "${ROOT}/scripts/privacy_filter_gliner.py" "${TARGET_DIR}/privacy_filter_gliner.py"
cp "${ROOT}/scripts/privacy_filter_fixture.py" "${TARGET_DIR}/privacy_filter_fixture.py"
cp "${ROOT}/docs/privacy_mode.md" "${TARGET_DIR}/README-privacy.md"
chmod +x "${TARGET_DIR}/codex" "${TARGET_DIR}/codex-privacy"

tar -C "${ROOT}/dist" -czf "${ARCHIVE}" "$(basename "${TARGET_DIR}")"
printf '%s\n' "${ARCHIVE}"
