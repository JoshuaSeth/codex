# Copyright (c) 2026 PitchAI. All rights reserved.
"""Fail when runtime code creates nested asyncio event loops."""

from __future__ import annotations

import ast
from dataclasses import dataclass
from typing import TYPE_CHECKING, cast

from pitchai_quality.analysis_support import (
    checker_parser,
    collect_file_violations,
    relative_path,
    scan_roots,
    write_failure,
    write_success,
)

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path


@dataclass(frozen=True)
class Violation:
    """One forbidden event-loop construction call."""

    path: Path
    line: int
    expression: str


def _call_name(node: ast.AST) -> str:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        parent = _call_name(node.value)
        return f"{parent}.{node.attr}" if parent else node.attr
    return ""


def _check_file(path: Path) -> list[Violation]:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    parents = {child: parent for parent in ast.walk(tree) for child in ast.iter_child_nodes(parent)}
    violations: list[Violation] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        expression = _call_name(node.func)
        forbidden_creation = expression in {
            "asyncio.run",
            "asyncio.new_event_loop",
            "asyncio.get_event_loop",
            "asyncio.get_event_loop_policy",
        }
        if (forbidden_creation or expression.endswith(".run_until_complete")) and not (
            expression == "asyncio.run" and _is_module_entry(node, parents)
        ):
            violations.append(Violation(path, node.lineno, expression))
    return violations


def _is_module_entry(node: ast.AST, parents: dict[ast.AST, ast.AST]) -> bool:
    """Recognize a synchronous main guard, never a function defined within it."""
    current = node
    guarded = False
    while current in parents:
        parent = parents[current]
        if isinstance(parent, ast.Call) and _call_name(parent.func) == "asyncio.run":
            return False
        if isinstance(parent, ast.FunctionDef | ast.AsyncFunctionDef | ast.Lambda | ast.ClassDef):
            return False
        if isinstance(parent, ast.If) and current in parent.body:
            test = parent.test
            if (
                isinstance(test, ast.Compare) and len(test.ops) == 1
                and isinstance(test.ops[0], ast.Eq) and len(test.comparators) == 1
            ):
                operands = (test.left, test.comparators[0])
                guarded = guarded or (
                    any(isinstance(value, ast.Name) and value.id == "__name__" for value in operands)
                    and any(isinstance(value, ast.Constant) and value.value == "__main__" for value in operands)
                )
        current = parent
    return guarded


def main(argv: Sequence[str] | None = None) -> int:
    """Run the nested-event-loop checker.

    Returns:
        Zero when no violations exist; otherwise one.
    """
    args = checker_parser("Reject nested event-loop construction.").parse_args(list(argv) if argv is not None else None)
    raw_paths = cast("list[str]", args.paths)
    violations = collect_file_violations(scan_roots(raw_paths), _check_file)

    if violations:
        for violation in violations:
            write_failure(
                f"{relative_path(violation.path)}:{violation.line}: "
                f"nested event-loop creation is forbidden: {violation.expression}",
            )
        return 1

    write_success("ok nested_event_loops")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
