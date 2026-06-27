#!/usr/bin/env node
// Entry point for PitchAI Codex Privacy npm packages.

import { spawn } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const require = createRequire(import.meta.url);

const PLATFORM_PACKAGE_BY_TARGET = {
  "x86_64-unknown-linux-gnu": "@pitchai/codex-privacy-linux-x64",
  "x86_64-unknown-linux-musl": "@pitchai/codex-privacy-linux-x64",
  "aarch64-unknown-linux-gnu": "@pitchai/codex-privacy-linux-arm64",
  "aarch64-unknown-linux-musl": "@pitchai/codex-privacy-linux-arm64",
  "x86_64-apple-darwin": "@pitchai/codex-privacy-darwin-x64",
  "aarch64-apple-darwin": "@pitchai/codex-privacy-darwin-arm64",
  "x86_64-pc-windows-msvc": "@pitchai/codex-privacy-win32-x64",
  "aarch64-pc-windows-msvc": "@pitchai/codex-privacy-win32-arm64",
};

const TARGET_BY_PLATFORM_ARCH = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
  "win32-arm64": "aarch64-pc-windows-msvc",
};

const targetTriple = TARGET_BY_PLATFORM_ARCH[`${process.platform}-${process.arch}`];
if (!targetTriple) {
  throw new Error(`Unsupported platform: ${process.platform} (${process.arch})`);
}

const packageRoot = findPackageRoot(targetTriple);
const resolvedTargetTriple = resolveInstalledTarget(packageRoot, targetTriple);
const packageJsonPath = path.join(
  packageRoot,
  "vendor",
  resolvedTargetTriple,
  "codex-package.json",
);
if (!existsSync(packageJsonPath)) {
  throw new Error(`Missing Codex package metadata: ${packageJsonPath}`);
}

const packageJson = require(packageJsonPath);
const entrypoint = path.join(
  packageRoot,
  "vendor",
  resolvedTargetTriple,
  packageJson.entrypoint,
);
const privacyFilter = path.join(
  packageRoot,
  "vendor",
  resolvedTargetTriple,
  packageJson.resourcesDir,
  "privacy",
  "privacy_filter_openai.py",
);

if (!existsSync(entrypoint)) {
  throw new Error(`Missing Codex executable: ${entrypoint}`);
}
if (!existsSync(privacyFilter)) {
  throw new Error(`Missing privacy filter adapter: ${privacyFilter}`);
}

const detectorCommand = [
  "uv",
  "run",
  "--python",
  "3.12",
  "--with",
  "transformers>=4.53.0",
  "--with",
  "torch",
  "--with",
  "accelerate",
  "python",
  shlexQuote(toPortablePath(privacyFilter)),
].join(" ");

const env = {
  ...process.env,
  CODEX_MANAGED_BY_NPM: "1",
  CODEX_MANAGED_PACKAGE_ROOT: realpathSync(packageRoot),
  PITCHAI_CODEX_PRIVACY_MIDDLEWARE:
    process.env.PITCHAI_CODEX_PRIVACY_MIDDLEWARE || "1",
  PITCHAI_CODEX_PRIVACY_FILTER_CMD:
    process.env.PITCHAI_CODEX_PRIVACY_FILTER_CMD || detectorCommand,
};

const child = spawn(entrypoint, process.argv.slice(2), {
  stdio: "inherit",
  env,
  windowsHide: false,
});

child.on("error", (err) => {
  console.error(err);
  process.exit(1);
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (!child.killed) {
      try {
        child.kill(signal);
      } catch {
        // The child may have exited between the killed check and signal send.
      }
    }
  });
}

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});

function findPackageRoot(target) {
  const platformPackage = PLATFORM_PACKAGE_BY_TARGET[target];
  try {
    const packageJson = require.resolve(`${platformPackage}/package.json`);
    return path.dirname(packageJson);
  } catch {
    return path.resolve(__dirname, "..");
  }
}

function resolveInstalledTarget(packageRoot, preferredTarget) {
  const preferredMetadata = path.join(
    packageRoot,
    "vendor",
    preferredTarget,
    "codex-package.json",
  );
  if (existsSync(preferredMetadata)) {
    return preferredTarget;
  }

  const vendorRoot = path.join(packageRoot, "vendor");
  for (const candidate of compatibleTargets(preferredTarget)) {
    if (existsSync(path.join(vendorRoot, candidate, "codex-package.json"))) {
      return candidate;
    }
  }

  return preferredTarget;
}

function compatibleTargets(preferredTarget) {
  switch (preferredTarget) {
    case "x86_64-unknown-linux-musl":
      return ["x86_64-unknown-linux-gnu"];
    case "aarch64-unknown-linux-musl":
      return ["aarch64-unknown-linux-gnu"];
    default:
      return [];
  }
}

function toPortablePath(value) {
  return process.platform === "win32" ? value.replaceAll("\\", "/") : value;
}

function shlexQuote(value) {
  return `'${value.replaceAll("'", "'\"'\"'")}'`;
}
