#!/usr/bin/env python3
"""Verify the declared microbenchmark path matrix and its source coverage floor."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
INVENTORY_PATH = ROOT / "benches" / "path_inventory.json"
SUPPORT_PATH = ROOT / "benches" / "support" / "mod.rs"
COVERAGE_PATH = ROOT / "target" / "microbench-coverage.json"
BENCHMARK_RE = re.compile(r"^(.+): benchmark$")
CONSTANT_RE = re.compile(r"^pub const (WARM_UP_MS|MEASUREMENT_MS): u64 = (\d+);$", re.MULTILINE)


def fail(message: str) -> None:
    print(f"microbench-paths: {message}", file=sys.stderr)
    raise SystemExit(1)


def run(command: list[str]) -> str:
    print("+", " ".join(command), flush=True)
    try:
        completed = subprocess.run(
            command,
            cwd=ROOT,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=600,
        )
    except subprocess.TimeoutExpired as error:
        fail(f"command exceeded the 600-second harness timeout: {' '.join(command)}")
    if completed.returncode != 0:
        print(completed.stdout, file=sys.stderr)
        fail(f"command exited with {completed.returncode}: {' '.join(command)}")
    return completed.stdout


def load_inventory() -> dict[str, Any]:
    with INVENTORY_PATH.open(encoding="utf-8") as inventory_file:
        inventory = json.load(inventory_file)
    if inventory.get("schema") != 1:
        fail("unsupported path inventory schema")

    constants = {name: int(value) for name, value in CONSTANT_RE.findall(SUPPORT_PATH.read_text())}
    configured_budget = constants.get("WARM_UP_MS", 0) + constants.get("MEASUREMENT_MS", 0)
    if configured_budget != inventory.get("case_budget_ms"):
        fail(
            "Criterion timing and path_inventory.json disagree: "
            f"{configured_budget}ms != {inventory.get('case_budget_ms')}ms"
        )
    if configured_budget >= 2_000:
        fail(f"configured per-case benchmark budget is not below 2 seconds: {configured_budget}ms")
    return inventory


def expected_benchmarks(inventory: dict[str, Any], target: str) -> set[str]:
    expected: set[str] = set()
    for group in inventory["targets"][target]:
        if not group["covers"] and not group.get("support", False):
            fail(f"{target}/{group['group']} has no source owner and is not a support benchmark")
        for case in group["cases"]:
            benchmark = f"{group['group']}/{case}"
            if benchmark in expected:
                fail(f"duplicate benchmark inventory entry: {target}/{benchmark}")
            expected.add(benchmark)
    return expected


def verify_benchmark_inventory(inventory: dict[str, Any]) -> None:
    for target in inventory["targets"]:
        output = run(["cargo", "bench", "--bench", target, "--", "--list"])
        actual = {
            match.group(1)
            for line in output.splitlines()
            if (match := BENCHMARK_RE.match(line.strip()))
        }
        expected = expected_benchmarks(inventory, target)
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        if missing or unexpected:
            details = []
            if missing:
                details.append(f"missing={missing}")
            if unexpected:
                details.append(f"unowned={unexpected}")
            fail(f"benchmark inventory mismatch for {target}: {'; '.join(details)}")
        print(f"  {target}: {len(actual)} declared paths")


def normalize_source(filename: str) -> str | None:
    path = Path(filename)
    if not path.is_absolute():
        path = ROOT / path
    try:
        relative = path.resolve().relative_to(ROOT.resolve())
    except ValueError:
        return None
    source = relative.as_posix()
    return source if source.startswith("src/") else None


def collect_coverage() -> dict[str, dict[str, float | int]]:
    run(["rustup", "run", "stable", "cargo", "llvm-cov", "clean", "--workspace"])
    for target in load_inventory()["targets"]:
        run(
            [
                "rustup",
                "run",
                "stable",
                "cargo",
                "llvm-cov",
                "--no-report",
                "--bench",
                target,
                "--",
                "--test",
            ]
        )
    run(
        [
            "rustup",
            "run",
            "stable",
            "cargo",
            "llvm-cov",
            "report",
            "--json",
            "--output-path",
            str(COVERAGE_PATH),
        ]
    )

    with COVERAGE_PATH.open(encoding="utf-8") as coverage_file:
        report = json.load(coverage_file)
    summaries: dict[str, dict[str, float | int]] = {}
    for entry in report["data"][0]["files"]:
        source = normalize_source(entry["filename"])
        if source is None:
            continue
        lines = entry["summary"]["lines"]
        functions = entry["summary"]["functions"]
        summaries[source] = {
            "covered_lines": lines["covered"],
            "lines": lines["count"],
            "line_percent": lines["percent"],
            "uncovered_lines": lines["count"] - lines["covered"],
            "covered_functions": functions["covered"],
            "functions": functions["count"],
            "function_percent": functions["percent"],
            "uncovered_functions": functions["count"] - functions["covered"],
        }
    return summaries


def verify_source_matrix(inventory: dict[str, Any]) -> None:
    owners = {
        source
        for groups in inventory["targets"].values()
        for group in groups
        for source in group["covers"]
    }
    guarded = set(inventory["coverage"])
    exemptions = set(inventory["exempt_sources"])
    source_files = {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "src").rglob("*.rs")
    }

    if owners != guarded:
        fail(
            "source owners and coverage guards differ: "
            f"owners_without_guards={sorted(owners - guarded)}, "
            f"guards_without_owners={sorted(guarded - owners)}"
        )
    unknown = (guarded | exemptions) - source_files
    unclassified = source_files - guarded - exemptions
    if unknown or unclassified:
        fail(
            "source classification is incomplete: "
            f"unknown={sorted(unknown)}, unclassified={sorted(unclassified)}"
        )


def platform_guard(value: Any) -> Any:
    if not isinstance(value, dict):
        return value
    if sys.platform in value:
        return value[sys.platform]
    if "default" in value:
        return value["default"]
    fail(f"coverage guard has no value for platform {sys.platform}: {value}")


def verify_coverage(
    inventory: dict[str, Any], summaries: dict[str, dict[str, float | int]]
) -> None:
    failures: list[str] = []
    print("\nMicrobenchmark source coverage:")
    print("  source                              lines          functions")
    for source, guard in inventory["coverage"].items():
        summary = summaries.get(source)
        if summary is None:
            failures.append(f"{source}: absent from coverage report")
            continue
        line_percent = float(summary["line_percent"])
        function_percent = float(summary["function_percent"])
        uncovered_lines = int(summary["uncovered_lines"])
        uncovered_functions = int(summary["uncovered_functions"])
        print(
            f"  {source:<35} {line_percent:6.1f}% "
            f"({uncovered_lines:3} open)   {function_percent:6.1f}% "
            f"({uncovered_functions:3} open)"
        )
        minimum_lines = float(platform_guard(guard["min_line_percent"]))
        if line_percent + 1e-9 < minimum_lines:
            failures.append(
                f"{source}: line coverage {line_percent:.1f}% is below {minimum_lines:.1f}%"
            )
        if "max_uncovered_lines" in guard:
            maximum_uncovered_lines = int(
                platform_guard(guard["max_uncovered_lines"])
            )
            if uncovered_lines > maximum_uncovered_lines:
                failures.append(
                    f"{source}: {uncovered_lines} uncovered lines exceeds "
                    f"the locked floor of {maximum_uncovered_lines}"
                )
        if "max_uncovered_functions" in guard:
            maximum_uncovered_functions = int(
                platform_guard(guard["max_uncovered_functions"])
            )
            if uncovered_functions > maximum_uncovered_functions:
                failures.append(
                    f"{source}: {uncovered_functions} uncovered functions exceeds "
                    f"the locked floor of {maximum_uncovered_functions}"
                )
    if failures:
        fail("coverage floor failed:\n  " + "\n  ".join(failures))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-only",
        action="store_true",
        help="verify the semantic path inventory without collecting source coverage",
    )
    arguments = parser.parse_args()

    inventory = load_inventory()
    verify_source_matrix(inventory)
    verify_benchmark_inventory(inventory)
    if arguments.list_only:
        print("microbench-paths: semantic path inventory verified")
        return
    summaries = collect_coverage()
    verify_coverage(inventory, summaries)
    print(f"microbench-paths: all paths verified; report={COVERAGE_PATH.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
