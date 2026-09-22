# Copyright (c) 2026 PitchAI. All rights reserved.
"""Reject alternate root tool profiles without rejecting application settings."""

from __future__ import annotations

import tomllib
from pathlib import Path
from unittest.mock import patch

import pytest

from pitchai_quality import check_no_validation_bypasses as verifier
from pitchai_quality.strict_policy import PolicyConfigurationError, root_tool_sections


@pytest.mark.parametrize("name", ["ruff", "basedpyright", "pyright", "pylint", "semgrep"])
def test_root_tool_sections_fail_integrity(tmp_path: Path, name: str) -> None:
    """Even empty alternate sections must fail the actual integrity collector."""
    configuration = tmp_path / "pyproject.toml"
    configuration.write_text(f"[tool.{name}]\n", encoding="utf-8")
    with (
        patch.object(verifier, "REPOSITORY_ROOT", tmp_path),
        patch.object(
            verifier,
            "_STRICT_WORKFLOW_PATH",
            tmp_path / ".github/workflows/python-strict.yml",
        ),
    ):
        violations = verifier._workflow_and_toolchain_violations()
    matching = [item for item in violations if item.path == configuration]
    assert len(matching) == 1
    assert matching[0].reason == f"alternate root tool.{name} configuration is forbidden"


def test_application_and_test_configuration_remains_valid(tmp_path: Path) -> None:
    """Packaging and test configuration must remain usable at the root."""
    path = tmp_path / "pyproject.toml"
    path.write_text(
        '[tool.pytest.ini_options]\npythonpath = ["ops"]\n[tool.setuptools]\npackages = ["scripts"]\n', encoding="utf-8"
    )
    assert root_tool_sections(path) == ()


def test_absent_root_configuration_is_valid(tmp_path: Path) -> None:
    """Repositories using only the portable quality project need no root TOML."""
    assert root_tool_sections(tmp_path / "pyproject.toml") == ()


def test_scalar_tool_table_fails_loudly(tmp_path: Path) -> None:
    """A scalar must not silently stand in for the expected tool table."""
    path = tmp_path / "pyproject.toml"
    path.write_text("tool = 1\n", encoding="utf-8")
    with pytest.raises(PolicyConfigurationError):
        root_tool_sections(path)


def test_malformed_toml_fails_loudly(tmp_path: Path) -> None:
    """TOML errors must propagate rather than report an empty clean profile."""
    path = tmp_path / "pyproject.toml"
    path.write_text("[tool.ruff\n", encoding="utf-8")
    with pytest.raises(tomllib.TOMLDecodeError):
        root_tool_sections(path)
