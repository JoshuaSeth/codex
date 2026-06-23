#!/usr/bin/env python3
"""Stage npm packages for PitchAI Codex Privacy.

This keeps the npm layout close to upstream Codex:

- a small main package with the `codex-privacy` launcher;
- platform-specific packages containing the native Codex package payload;
- no Cargo target directories, source trees, debug sidecars, or secrets.

The platform package can also be installed directly, which gives PitchAI an
immediate private/internal install path before a registry publish step exists.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PRIVACY_SCRIPTS = (
    "privacy_filter_openai.py",
    "privacy_filter_gliner.py",
    "privacy_filter_fixture.py",
)
MAIN_PACKAGE_NAME = "@pitchai/codex-privacy"
PLATFORM_PACKAGES: dict[str, dict[str, str]] = {
    "x86_64-unknown-linux-gnu": {
        "name": "@pitchai/codex-privacy-linux-x64",
        "tag": "linux-x64",
        "os": "linux",
        "cpu": "x64",
    },
    "x86_64-unknown-linux-musl": {
        "name": "@pitchai/codex-privacy-linux-x64",
        "tag": "linux-x64",
        "os": "linux",
        "cpu": "x64",
    },
    "aarch64-unknown-linux-gnu": {
        "name": "@pitchai/codex-privacy-linux-arm64",
        "tag": "linux-arm64",
        "os": "linux",
        "cpu": "arm64",
    },
    "aarch64-unknown-linux-musl": {
        "name": "@pitchai/codex-privacy-linux-arm64",
        "tag": "linux-arm64",
        "os": "linux",
        "cpu": "arm64",
    },
    "x86_64-apple-darwin": {
        "name": "@pitchai/codex-privacy-darwin-x64",
        "tag": "darwin-x64",
        "os": "darwin",
        "cpu": "x64",
    },
    "aarch64-apple-darwin": {
        "name": "@pitchai/codex-privacy-darwin-arm64",
        "tag": "darwin-arm64",
        "os": "darwin",
        "cpu": "arm64",
    },
    "x86_64-pc-windows-msvc": {
        "name": "@pitchai/codex-privacy-win32-x64",
        "tag": "win32-x64",
        "os": "win32",
        "cpu": "x64",
    },
    "aarch64-pc-windows-msvc": {
        "name": "@pitchai/codex-privacy-win32-arm64",
        "tag": "win32-arm64",
        "os": "win32",
        "cpu": "arm64",
    },
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--package-dir", type=Path, action="append", default=[])
    parser.add_argument("--out-dir", type=Path, default=ROOT / "dist" / "privacy-npm")
    parser.add_argument("--main-only", action="store_true")
    parser.add_argument("--skip-main", action="store_true")
    parser.add_argument("--pack", action="store_true")
    parser.add_argument("--force", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    out_dir = args.out_dir.resolve()
    if out_dir.exists() and args.force:
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    manifest: dict[str, object] = {
        "version": args.version,
        "main_package": MAIN_PACKAGE_NAME,
        "platform_packages": [],
    }

    staged_platforms: list[tuple[str, dict[str, str], Path, Path | None]] = []
    if not args.main_only:
        for package_dir in args.package_dir:
            package_dir = package_dir.resolve()
            target = read_package_target(package_dir)
            platform = PLATFORM_PACKAGES.get(target)
            if platform is None:
                supported = ", ".join(sorted(PLATFORM_PACKAGES))
                raise RuntimeError(f"Unsupported package target {target!r}; supported: {supported}")
            stage_dir = out_dir / f"{package_basename(platform['name'])}-{args.version}"
            stage_platform_package(stage_dir, package_dir, args.version, target, platform)
            tgz = pack(stage_dir, out_dir) if args.pack else None
            staged_platforms.append((target, platform, stage_dir, tgz))
            manifest["platform_packages"].append(
                package_manifest(stage_dir, target, platform, tgz)
            )

    if not args.skip_main:
        main_stage = out_dir / f"codex-privacy-{args.version}"
        stage_main_package(main_stage, args.version)
        main_tgz = pack(main_stage, out_dir) if args.pack else None
        manifest["main"] = package_manifest(main_stage, "any", {"name": MAIN_PACKAGE_NAME}, main_tgz)

    manifest_path = out_dir / "privacy-npm-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


def read_package_target(package_dir: Path) -> str:
    metadata_path = package_dir / "codex-package.json"
    if not metadata_path.is_file():
        raise RuntimeError(f"Missing codex-package.json in {package_dir}")
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    target = metadata.get("target")
    if not isinstance(target, str) or not target:
        raise RuntimeError(f"Invalid target in {metadata_path}")
    return target


def stage_main_package(stage_dir: Path, version: str) -> None:
    prepare_stage(stage_dir)
    bin_dir = stage_dir / "bin"
    bin_dir.mkdir()
    shutil.copy2(ROOT / "codex-cli" / "bin" / "codex-privacy.js", bin_dir / "codex-privacy.js")
    write_json(
        stage_dir / "package.json",
        {
            "name": MAIN_PACKAGE_NAME,
            "version": version,
            "description": "PitchAI Codex CLI with local OpenAI privacy-filter anonymization.",
            "license": "UNLICENSED",
            "type": "module",
            "bin": {"codex-privacy": "bin/codex-privacy.js"},
            "files": ["bin/codex-privacy.js"],
            "engines": {"node": ">=18"},
            "optionalDependencies": {
                platform["name"]: version_for_platform(version, platform)
                for platform in PLATFORM_PACKAGES.values()
            },
        },
    )
    copy_readme(stage_dir)


def stage_platform_package(
    stage_dir: Path,
    package_dir: Path,
    version: str,
    target: str,
    platform: dict[str, str],
) -> None:
    prepare_stage(stage_dir)
    bin_dir = stage_dir / "bin"
    vendor_dir = stage_dir / "vendor" / target
    privacy_dir = vendor_dir / "codex-resources" / "privacy"
    bin_dir.mkdir(parents=True)
    shutil.copy2(ROOT / "codex-cli" / "bin" / "codex-privacy.js", bin_dir / "codex-privacy.js")
    shutil.copytree(package_dir, vendor_dir, ignore=privacy_payload_ignore)
    privacy_dir.mkdir(parents=True, exist_ok=True)
    for script in PRIVACY_SCRIPTS:
        shutil.copy2(ROOT / "scripts" / script, privacy_dir / script)
    write_json(
        stage_dir / "package.json",
        {
            "name": platform["name"],
            "version": version_for_platform(version, platform),
            "description": "PitchAI Codex Privacy native package for " + platform["tag"],
            "license": "UNLICENSED",
            "type": "module",
            "os": [platform["os"]],
            "cpu": [platform["cpu"]],
            "bin": {"codex-privacy": "bin/codex-privacy.js"},
            "files": ["bin/codex-privacy.js", "vendor"],
            "engines": {"node": ">=18"},
            "repository": {
                "type": "git",
                "url": "git+https://github.com/JoshuaSeth/codex.git",
            },
        },
    )
    copy_readme(stage_dir)


def privacy_payload_ignore(_directory: str, names: list[str]) -> set[str]:
    ignored = {
        "__pycache__",
        ".DS_Store",
        "*.pdb",
        "*.dSYM",
        "*.rlib",
        "*.rmeta",
        "target",
    }
    return {name for name in names if name in ignored}


def prepare_stage(stage_dir: Path) -> None:
    if stage_dir.exists():
        shutil.rmtree(stage_dir)
    stage_dir.mkdir(parents=True)


def copy_readme(stage_dir: Path) -> None:
    readme = ROOT / "docs" / "privacy_distribution.md"
    if readme.exists():
        shutil.copy2(readme, stage_dir / "README.md")


def package_basename(package_name: str) -> str:
    return package_name.split("/")[-1]


def version_for_platform(version: str, platform: dict[str, str]) -> str:
    return f"{version}-{platform['tag']}"


def pack(stage_dir: Path, out_dir: Path) -> Path:
    npm = resolve_npm()
    with tempfile.TemporaryDirectory(prefix="pitchai-codex-privacy-npm-") as tmp:
        cache = Path(tmp) / "cache"
        logs = Path(tmp) / "logs"
        cache.mkdir()
        logs.mkdir()
        env = os.environ.copy()
        env["NPM_CONFIG_CACHE"] = str(cache)
        env["NPM_CONFIG_LOGS_DIR"] = str(logs)
        output = subprocess.check_output(
            [npm, "pack", "--json", "--pack-destination", str(out_dir)],
            cwd=stage_dir,
            env=env,
            text=True,
        )
    payload = json.loads(output)
    if not payload:
        raise RuntimeError("npm pack produced no output")
    path = out_dir / payload[0]["filename"]
    if not path.exists():
        raise RuntimeError(f"npm pack output missing: {path}")
    return path


def resolve_npm() -> str:
    candidates = ["npm.cmd", "npm"] if sys.platform == "win32" else ["npm"]
    for candidate in candidates:
        resolved = shutil.which(candidate)
        if resolved is not None:
            return resolved
    raise RuntimeError("npm was not found on PATH")


def package_manifest(
    stage_dir: Path,
    target: str,
    platform: dict[str, str],
    tgz: Path | None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "name": platform["name"],
        "target": target,
        "stage_dir": str(stage_dir),
        "stage_size_bytes": directory_size(stage_dir),
    }
    if tgz is not None:
        result["tarball"] = str(tgz)
        result["tarball_size_bytes"] = tgz.stat().st_size
        result["tarball_sha256"] = sha256(tgz)
    return result


def directory_size(path: Path) -> int:
    return sum(item.stat().st_size for item in path.rglob("*") if item.is_file())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
