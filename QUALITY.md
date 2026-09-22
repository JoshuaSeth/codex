# Python Quality Gate

Run the repository's complete fail-closed Python gate with:

```sh
uv run --project quality --python 3.12 --frozen check
```

The aggregate runs the anti-bypass policy, all five PitchAI Python preference
checkers, Ruff with every rule selected, BasedPyright in strict mode with fatal
warnings, full Pylint with a score floor of 10 and a 250-line module ceiling,
and Semgrep `ERROR` rules. It checks every repository Python and stub file,
including tests and tools. Only generated caches, virtual environments, build
outputs, and other non-source directories are omitted from discovery.

The read-only GitHub Actions workflow runs for every pull request and every
push branch. It intentionally has no branch-name filter, so repositories whose
default branch is `main`, `master`, `staging`, or another project-specific name
cannot silently skip the gate.

The read-only GitHub workflow is the external CI trust root. Before installing
or executing the quality project, it compares the anti-bypass verifier with an
exact SHA-256 digest committed in the workflow and fails loudly on a mismatch.
The verifier hardcodes the exact digest of the required-file manifest. The
manifest, in turn, hashes the runner, all five preference checkers, every
semantic helper, the complete-source resolver, `strict_policy.py`, locked
dependencies, tool configuration, Semgrep policy, and this documentation. The
workflow, verifier, and manifest are intentionally excluded from the manifest
to keep the digest chain acyclic.

The verifier rejects missing or unexpected portable files, altered manifest
membership, and every manifested-file hash mismatch. It also validates the
workflow's all-PR/all-push triggers, read-only permissions, trust-anchor step,
frozen install and aggregate commands, full-SHA action pins, and absence of
`continue-on-error: true`. The unavoidable boundary is a coordinated edit to
the external workflow trust root and the chain it anchors: that change must be
identified and judged in code review. Repository-local files cannot
self-authenticate such a coordinated change, this design makes no claim of
mutually authenticated local files, and it uses no operational signing secret.

Ruff always receives `--config quality/pyproject.toml`, BasedPyright receives
`--project quality/pyproject.toml`, Pylint receives
`--rcfile quality/pyproject.toml`, and Semgrep receives the absolute strict
config path. Installation and execution both select the frozen `quality`
project. Consequently root dependency files cannot change the quality
environment, and root tool configs cannot weaken these invocations. The
anti-bypass gate also rejects alternate root tool configs and any other GitHub
Actions workflow that invokes the quality project or a direct checker.
Root `pyproject.toml` must not contain Ruff, BasedPyright, Pyright, Pylint or
Semgrep tool sections. Test, packaging and application configuration stay in
place. Root tool tables and TOML parsing fail loudly when malformed.
Python suppression detection inspects comment tokens, not strings containing
analyzer fixtures or examples. Real comments in both `.py` and `.pyi` files
remain forbidden; malformed token streams fail loudly. Config-file scanning
is unchanged, and Semgrep's inline-suppression handling remains disabled.
Across every workflow, non-local `uses:` references must be pinned to a full
40-hex commit SHA and `continue-on-error` is forbidden. Mutable third-party
action tags and ignored workflow failures are reported as anti-bypass debt;
deployment workflows receive no exemption.

Ruff recognizes Pydantic models and FastAPI route decorators as consumers of
runtime annotations, using its documented `runtime-evaluated-base-classes` and
`runtime-evaluated-decorators` settings. Moving their UUID, request, or form
types into `TYPE_CHECKING` can break actual schema construction. Ordinary
type-only imports remain subject to TC001–TC003; no source path or diagnostic
is excluded by this framework configuration. Any change to this declaration
updates the same reviewed configuration and digest chain above.

The pinned Semgrep CLI needs `--experimental` to select its native scanner
when using `--x-ignore-semgrepignore-files`. Its Python compatibility scanner
rejects that option before scanning any file. The native scanner retains all
rules and the existing ignore-file bypass, so ignored source is still checked.

A nonzero result is an enforcement result, not permission to narrow coverage.
Repair violations with real dependencies or stubs and explicit boundary
architecture. Do not add inline suppressions, diagnostic downgrades, source
exclusions, ignored failures, or checker wrappers that hide a tool result.

## Execution boundaries

The reviewed Infrastructure PR636 correction recognizes `asyncio.run` in
module main guards, locally registered FastAPI/APIRouter handlers, typed
ASGI/Starlette middleware, and managed HTTPX/aiohttp/requests client lifetimes.
Response, request, mock transport and exception constructors are data rather
than network operations. Nested loops, ordinary unused helpers, unmanaged
transport calls and non-edge exception handlers remain findings. Structural
recognition does not certify business ownership or approve a product release.
This command uses a separate locked quality environment and does not launch
Codex, its integration helpers, application tests or services. Existing Rust,
Bazel, SDK and package validators remain in their original workflows. Their
PR triggers omit changes confined to the Python quality profile; changes to
application source or those workflows still trigger their existing checks.
The Python workflow checks all source on every push and pull request.
