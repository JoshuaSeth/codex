"""Regression contracts for strict entrypoint and suppression detection."""

from __future__ import annotations

import tokenize
from pathlib import Path
from unittest.mock import patch

import pytest

from pitchai_quality import check_no_validation_bypasses as verifier


@pytest.mark.parametrize("suffix", [".py", ".pyi"])
@pytest.mark.parametrize(
    "directive",
    [
        "# noqa: F401",
        "# type: ignore",
        "# ruff: noqa",
        "# pylint: disable=all",
        "# pylint: disable-next=all",
        "# pylint: skip-file",
        "# nosemgrep",
        "# pyright: basic",
    ],
)
def test_real_comment_suppressions_fail(tmp_path: Path, suffix: str, directive: str) -> None:
    """Every suppression family remains forbidden in Python and stub comments."""
    source = tmp_path / f"sample{suffix}"
    source.write_text(f"value: int = 1  {directive}\n", encoding="utf-8")
    with (
        patch.object(verifier, "iter_python_files", return_value=(source,)),
        patch.object(verifier, "_CONFIG_PATHS", ()),
    ):
        violations = verifier._inline_violations()
    assert len(violations) == 1
    assert violations[0].path == source
    assert violations[0].line == 1


@pytest.mark.parametrize(
    "source_text",
    [
        'example = "# noqa: F401"\n',
        "example = '''# pylint: disable=all\n# type: ignore\n'''\n",
        'pattern = r"# nosemgrep"\n',
        'example = f"# noqa {1}"\n',
    ],
)
def test_literal_examples_are_not_directives(tmp_path: Path, source_text: str) -> None:
    """Preserve analyzer fixtures and documentation strings without exemptions."""
    source = tmp_path / "fixture.py"
    source.write_text(source_text, encoding="utf-8")
    with (
        patch.object(verifier, "iter_python_files", return_value=(source,)),
        patch.object(verifier, "_CONFIG_PATHS", ()),
    ):
        assert verifier._inline_violations() == []


def test_comment_after_literal_still_fails(tmp_path: Path) -> None:
    """A preceding string cannot hide a real trailing suppression comment."""
    source = tmp_path / "fixture.py"
    source.write_text('example = "# noqa: F401"  # type: ignore\n', encoding="utf-8")
    with (
        patch.object(verifier, "iter_python_files", return_value=(source,)),
        patch.object(verifier, "_CONFIG_PATHS", ()),
    ):
        assert len(verifier._inline_violations()) == 1


def test_malformed_token_stream_fails_loudly(tmp_path: Path) -> None:
    """Incomplete syntax must not turn suppression scanning into silent success."""
    source = tmp_path / "fixture.py"
    source.write_text('example = """unterminated\n# noqa: F401\n', encoding="utf-8")
    with (
        patch.object(verifier, "iter_python_files", return_value=(source,)),
        patch.object(verifier, "_CONFIG_PATHS", ()),
    ):
        with pytest.raises(tokenize.TokenError):
            verifier._inline_violations()


def test_configuration_suppression_still_fails(tmp_path: Path) -> None:
    """The Python lexer change does not change non-Python configuration checks."""
    source = tmp_path / "pyproject.toml"
    source.write_text("value = 1 # noqa: F401\n", encoding="utf-8")
    with (
        patch.object(verifier, "iter_python_files", return_value=()),
        patch.object(verifier, "_CONFIG_PATHS", (source,)),
    ):
        assert len(verifier._inline_violations()) == 1
