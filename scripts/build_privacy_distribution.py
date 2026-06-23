#!/usr/bin/env python3
"""Build installable PitchAI Codex privacy distribution artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist"


def write_executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build(args: argparse.Namespace) -> int:
    codex_bin = args.codex_bin.resolve()
    if not codex_bin.exists():
        raise FileNotFoundError(f"codex binary not found: {codex_bin}")

    version = args.version
    target = args.target
    package_name = f"pitchai-codex-privacy-{version}-{target}"
    package_dir = (args.out_dir / package_name).resolve()
    archive = args.out_dir / f"{package_name}.tar.gz"
    npm_dir = args.out_dir / f"{package_name}-npm"

    if package_dir.exists():
        shutil.rmtree(package_dir)
    if npm_dir.exists():
        shutil.rmtree(npm_dir)
    archive.unlink(missing_ok=True)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    (package_dir / "bin").mkdir(parents=True)
    (package_dir / "privacy").mkdir()
    shutil.copy2(codex_bin, package_dir / "bin" / "codex")
    for script in [
        "privacy_filter_openai.py",
        "privacy_filter_gliner.py",
        "privacy_filter_fixture.py",
    ]:
        shutil.copy2(ROOT / "scripts" / script, package_dir / "privacy" / script)
    shutil.copy2(ROOT / "docs" / "privacy_mode.md", package_dir / "README.md")

    write_executable(
        package_dir / "bin" / "codex-privacy",
        """#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
export PITCHAI_CODEX_PRIVACY_MIDDLEWARE="${PITCHAI_CODEX_PRIVACY_MIDDLEWARE:-1}"
export PITCHAI_CODEX_PRIVACY_FILTER_CMD="${PITCHAI_CODEX_PRIVACY_FILTER_CMD:-uv run --python 3.12 --with transformers>=4.53.0 --with torch --with accelerate python ${ROOT}/privacy/privacy_filter_openai.py}"
exec "${HERE}/codex" "$@"
""",
    )

    write_executable(
        package_dir / "install.sh",
        """#!/usr/bin/env bash
set -euo pipefail
PREFIX="${1:-${PREFIX:-$HOME/.local}}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "${PREFIX}/bin" "${PREFIX}/lib/pitchai-codex-privacy"
cp -R "${ROOT}/bin" "${ROOT}/privacy" "${ROOT}/README.md" "${PREFIX}/lib/pitchai-codex-privacy/"
cat > "${PREFIX}/bin/codex-privacy" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "${PREFIX}/lib/pitchai-codex-privacy/bin/codex-privacy" "\\$@"
EOF
chmod +x "${PREFIX}/bin/codex-privacy"
printf 'Installed codex-privacy to %s/bin/codex-privacy\\n' "${PREFIX}"
""",
    )

    subprocess.run(
        [
            "tar",
            "-C",
            str(args.out_dir.resolve()),
            "-I",
            "gzip -1",
            "-cf",
            str(archive),
            package_name,
        ],
        check=True,
    )

    archive_sha = sha256(archive)
    release_url = (
        args.release_url
        or f"https://github.com/JoshuaSeth/codex/releases/download/{version}/{archive.name}"
    )

    formula_dir = args.out_dir / "homebrew"
    formula_dir.mkdir(exist_ok=True)
    formula = formula_dir / "pitchai-codex-privacy.rb"
    formula.write_text(
        f'''class PitchaiCodexPrivacy < Formula
  desc "PitchAI Codex CLI with local OpenAI privacy-filter span anonymization"
  homepage "https://github.com/JoshuaSeth/codex/pull/6"
  url "{release_url}"
  sha256 "{archive_sha}"
  version "{version}"

  depends_on "uv"
  depends_on "python@3.12"

  def install
    libexec.install Dir["*"]
    (bin/"codex-privacy").write <<~EOS
      #!/usr/bin/env bash
      set -euo pipefail
      exec "#{{libexec}}/bin/codex-privacy" "$@"
    EOS
  end

  test do
    assert_match "codex", shell_output("#{{bin}}/codex-privacy --version")
  end
end
'''
    )

    npm_package: str | None = None
    if not args.skip_npm:
        npm_dir.mkdir()
        (npm_dir / "package").mkdir()
        shutil.copytree(package_dir, npm_dir / "package" / "vendor")
        (npm_dir / "package" / "bin").mkdir()
        write_executable(
            npm_dir / "package" / "bin" / "codex-privacy.js",
            """#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const executable = resolve(here, "..", "vendor", "bin", "codex-privacy");
const result = spawnSync(executable, process.argv.slice(2), { stdio: "inherit" });
process.exit(result.status ?? 1);
""",
        )
        (npm_dir / "package" / "package.json").write_text(
            json.dumps(
                {
                    "name": "@pitchai/codex-privacy",
                    "version": version.lstrip("v"),
                    "description": "PitchAI Codex privacy-mode wrapper with bundled runtime artifact.",
                    "license": "UNLICENSED",
                    "private": True,
                    "type": "module",
                    "bin": {"codex-privacy": "bin/codex-privacy.js"},
                    "files": ["bin", "vendor"],
                    "engines": {"node": ">=18"},
                },
                indent=2,
            )
            + "\n"
        )
        npm_pack = (
            subprocess.run(
                ["npm", "pack", "--pack-destination", str(args.out_dir.resolve())],
                cwd=npm_dir / "package",
                check=True,
                text=True,
                stdout=subprocess.PIPE,
            )
            .stdout.strip()
            .splitlines()[-1]
        )
        npm_package = str(args.out_dir / npm_pack)

    manifest = {
        "version": version,
        "target": target,
        "archive": str(archive),
        "archive_sha256": archive_sha,
        "release_url": release_url,
        "homebrew_formula": str(formula),
        "npm_package": npm_package,
        "codex_binary": str(codex_bin),
        "primary_detector": "openai/privacy-filter",
    }
    manifest_path = args.out_dir / f"{package_name}.manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--codex-bin", type=Path, default=ROOT / "codex-rs/target/debug/codex"
    )
    parser.add_argument("--version", default="v0.0.0-privacy.20260618")
    parser.add_argument("--target", default="linux-x86_64")
    parser.add_argument("--out-dir", type=Path, default=DIST / "privacy-release")
    parser.add_argument("--release-url")
    parser.add_argument("--skip-npm", action="store_true")
    return build(parser.parse_args())


if __name__ == "__main__":
    raise SystemExit(main())
