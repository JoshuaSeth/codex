"""Path filters are metadata; executable quality bypasses remain forbidden."""

import json
from pathlib import Path
from unittest.mock import patch

import pytest

from pitchai_quality import check_no_validation_bypasses as verifier


def collect(tmp_path: Path, source: str):
    directory = tmp_path / ".github/workflows"
    directory.mkdir(parents=True)
    workflow = directory / "native.yml"
    workflow.write_text(source)
    with (
        patch.object(verifier, "REPOSITORY_ROOT", tmp_path),
        patch.object(
            verifier,
            "_STRICT_WORKFLOW_PATH",
            directory / "python-strict.yml",
        ),
    ):
        return verifier._workflow_and_toolchain_violations()


@pytest.mark.parametrize("event", ["push", "pull_request", "pull_request_target"])
@pytest.mark.parametrize("field", ["paths", "paths-ignore"])
def test_literal_path_metadata_is_not_an_executable_toolchain(tmp_path, event, field):
    source = (
        f'on:\n  {event}:\n    {field}: [".semgrep-ios.yml"]\njobs:\n  native:\n    steps:\n      - run: echo ready\n'
    )
    assert collect(tmp_path, source) == []


@pytest.mark.parametrize(
    "command",
    [
        "semgrep --error .",
        "ruff check .",
        "pylint app.py",
        "basedpyright",
        "uv run --project quality check",
        "python -m pitchai_quality.check",
    ],
)
def test_metadata_does_not_hide_an_actual_toolchain(tmp_path, command):
    source = f'on:\n  push:\n    paths: [".semgrep-ios.yml"]\njobs:\n  other:\n    steps:\n      - run: {command}\n'
    assert any("alternate workflow quality-tool entrypoint" in item.reason for item in collect(tmp_path, source))


@pytest.mark.parametrize(
    "source",
    [
        "on:\n  workflow_dispatch:\n    inputs:\n      command:\n        default: semgrep --error .\njobs: {}\n",
        "on:\n  workflow_call:\n    inputs:\n      command:\n        default: pylint app.py\njobs: {}\n",
        "on: push\nenv:\n  COMMAND: ruff check .\njobs: {}\n",
        "on: push\njobs:\n  other:\n    strategy:\n      matrix:\n        command: [basedpyright]\n",
    ],
)
def test_indirect_command_values_remain_checked(tmp_path, source):
    assert any("alternate workflow quality-tool entrypoint" in item.reason for item in collect(tmp_path, source))


def test_action_pin_check_still_applies_with_path_metadata(tmp_path):
    source = 'on:\n  push:\n    paths: [".semgrep-ios.yml"]\njobs:\n  other:\n    steps:\n      - uses: actions/checkout@v4\n'
    assert any("mutable workflow action reference" in item.reason for item in collect(tmp_path, source))


def test_malformed_workflow_is_not_silently_accepted():
    with pytest.raises(ValueError, match="workflow must be a mapping"):
        verifier._workflow_toolchain_text("not a mapping")


def test_path_filter_alias_cannot_erase_an_executable_value(tmp_path):
    source = 'on:\n  push: &shared\n    paths: ["semgrep"]\njobs:\n  other:\n    env: *shared\n    steps:\n      - run: echo ready\n'
    assert any("alternate workflow quality-tool entrypoint" in item.reason for item in collect(tmp_path, source))


@pytest.mark.parametrize(
    "command",
    [
        "ruff check .",
        "basedpyright",
        "pyright",
        "pylint app.py",
        "semgrep --error .",
        "uv run --project quality check",
        "python -m pitchai_quality.check",
    ],
)
@pytest.mark.parametrize("separator", ["\n", "\r\n", "\t"])
@pytest.mark.parametrize("style", ["quoted", "block"])
def test_raw_scalar_boundaries_preserve_tool_detection(
    tmp_path: Path,
    command: str,
    separator: str,
    style: str,
) -> None:
    """Every shell line and tab boundary must retain executable tool names."""
    value = "echo ready" + separator + command
    if style == "quoted":
        scalar = json.dumps(value)
    else:
        scalar = "|\n" + "\n".join("          " + line for line in value.splitlines())
    source = "on: push\njobs:\n  other:\n    steps:\n      - run: " + scalar + "\n"
    assert any("alternate workflow quality-tool entrypoint" in item.reason for item in collect(tmp_path, source))


def test_literal_escaped_text_is_not_a_new_shell_command(tmp_path: Path) -> None:
    """Literal backslash text is distinct from a decoded scalar newline."""
    value = r"echo ready\nruff"
    source = "on: push\njobs:\n  other:\n    steps:\n      - run: " + json.dumps(value) + "\n"
    assert collect(tmp_path, source) == []
