#!/usr/bin/env python3
"""Guard the W0-4 Vitest baseline (docs/decisions.md W0-4).

The repository ships 15 spec files that are NOT green upstream: 4 failed files /
3 failed tests / 171 passed (174 total), captured 2026-11-25 on Node v22.23.2 /
vitest 0.32.4.  Fixing upstream tests is not P0, so CI must fail only on NEW
failures:

  * any failing file that is not in scripts/ci/baselines/vitest-baseline.json;
  * more failed tests than the baseline;
  * fewer passed tests than the baseline.

Equal-or-better than the baseline is PASS (a known-broken file that starts
passing is fine).

Modes:
  --run            run `npx --no-install vitest run --reporter=json` and compare
  --report PATH    compare an existing vitest JSON report (debugging)
  --self-test      prove the guard rejects an injected extra failure; no vitest run

Exit codes: 0 = equal-or-better / self-test OK, 1 = regression, 2 = usage or
runtime error (missing report, no vitest, broken baseline, ...).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_BASELINE = os.path.join(ROOT, "scripts", "ci", "baselines", "vitest-baseline.json")


def _norm_path(name: str) -> str:
    """Normalize a vitest testResults[].name to a repo-relative path."""
    name = str(name or "").replace("\\", "/")
    root = ROOT.replace("\\", "/").rstrip("/")
    if name.startswith(root + "/"):
        return name[len(root) + 1 :]
    marker = "/spec/"
    if marker in name:
        return "spec/" + name.split(marker, 1)[1]
    return name.lstrip("./")


def _load_json(path: str) -> dict:
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def _load_baseline(path: str) -> dict:
    baseline = _load_json(path)
    missing = [k for k in ("failed_files", "failed_tests", "passed_tests") if k not in baseline]
    if missing:
        raise ValueError(f"baseline {path} lacks required keys: {', '.join(missing)}")
    baseline["failed_files"] = [_norm_path(p) for p in baseline["failed_files"]]
    baseline["failed_tests"] = int(baseline["failed_tests"])
    baseline["passed_tests"] = int(baseline["passed_tests"])
    return baseline


def extract_report(report: dict) -> dict:
    """Extract failing files and test counts from a vitest JSON report."""
    results = report.get("testResults")
    if not isinstance(results, list):
        raise ValueError("vitest report has no testResults[] array")

    failing = []
    for test_result in results:
        assertions = test_result.get("assertionResults") or []
        # account_timeline.spec.js fails at suite level with status "passed" and a
        # message ("document is not defined"), so a message means failure too.
        failed = (
            test_result.get("status") == "failed"
            or bool(str(test_result.get("message") or "").strip())
            or any(a.get("status") == "failed" for a in assertions)
        )
        if failed:
            failing.append(_norm_path(test_result.get("name", "")))

    if "numFailedTests" in report and "numPassedTests" in report:
        failed_tests = int(report["numFailedTests"])
        passed_tests = int(report["numPassedTests"])
        total_tests = int(report.get("numTotalTests", failed_tests + passed_tests))
    else:  # fallback for hand-written reports
        failed_tests = 0
        passed_tests = 0
        for test_result in results:
            for assertion in test_result.get("assertionResults") or []:
                if assertion.get("status") == "failed":
                    failed_tests += 1
                elif assertion.get("status") == "passed":
                    passed_tests += 1
        total_tests = failed_tests + passed_tests

    return {
        "files_total": len(results),
        "failing_files": sorted(set(failing)),
        "failed_tests": failed_tests,
        "passed_tests": passed_tests,
        "total_tests": total_tests,
    }


def evaluate(current: dict, baseline: dict) -> tuple[list[str], list[str], list[str]]:
    """Return (new_failing_files, fixed_files, problems)."""
    known = set(baseline["failed_files"])
    current_failing = set(current["failing_files"])
    new_files = sorted(current_failing - known)
    fixed_files = sorted(known - current_failing)

    problems: list[str] = []
    if new_files:
        problems.append("new failing file(s): " + ", ".join(new_files))
    if current["failed_tests"] > baseline["failed_tests"]:
        problems.append(
            f"failed tests {current['failed_tests']} > baseline {baseline['failed_tests']}"
        )
    if current["passed_tests"] < baseline["passed_tests"]:
        problems.append(
            f"passed tests {current['passed_tests']} < baseline {baseline['passed_tests']}"
        )
    return new_files, fixed_files, problems


def report_current(current: dict, baseline: dict) -> None:
    known = set(baseline["failed_files"])
    failing = set(current["failing_files"])
    new_files = failing - known
    print(
        "vitest baseline: files {total}: {failed} failed ({new} new, {known} known), "
        "{passed} passed".format(
            total=current["files_total"],
            failed=len(failing),
            new=len(new_files),
            known=len(failing & known),
            passed=current["files_total"] - len(failing),
        )
    )
    if failing:
        print("vitest baseline: failing files: " + ", ".join(sorted(failing)))
    print(
        "vitest baseline: tests {total}: {failed} failed / {passed} passed; "
        "committed baseline: {bfiles} files / {bfailed} failed / {bpassed} passed".format(
            total=current["total_tests"],
            failed=current["failed_tests"],
            passed=current["passed_tests"],
            bfiles=len(known),
            bfailed=baseline["failed_tests"],
            bpassed=baseline["passed_tests"],
        )
    )


def compare_report(report: dict, baseline: dict) -> int:
    current = extract_report(report)
    report_current(current, baseline)
    new_files, fixed_files, problems = evaluate(current, baseline)
    if fixed_files:
        print("vitest baseline: known failing files now passing: " + ", ".join(fixed_files))
    if problems:
        for problem in problems:
            print("vitest baseline: REGRESSION: " + problem)
        print("vitest baseline: verdict: FAIL (new failures vs committed baseline)")
        return 1
    print("vitest baseline: verdict: PASS (equal-or-better than committed baseline)")
    return 0


def run_vitest() -> int:
    with tempfile.TemporaryDirectory(prefix="lampa-vitest-") as tmpdir:
        report_path = os.path.join(tmpdir, "vitest-report.json")
        cmd = [
            "npx",
            "--no-install",
            "vitest",
            "run",
            "--reporter=json",
            f"--outputFile={report_path}",
        ]
        try:
            proc = subprocess.run(
                cmd, cwd=ROOT, capture_output=True, text=True, timeout=900
            )
        except FileNotFoundError:
            print("vitest baseline: FAIL: npx not found (Node.js is required)", file=sys.stderr)
            return 2
        except subprocess.TimeoutExpired:
            print("vitest baseline: FAIL: vitest timed out after 900 s", file=sys.stderr)
            return 2

        if not os.path.exists(report_path):
            print(
                "vitest baseline: FAIL: vitest produced no JSON report "
                f"(exit {proc.returncode}); install deps with `npm ci`? stderr tail:",
                file=sys.stderr,
            )
            tail = "\n".join((proc.stderr or "").strip().splitlines()[-15:])
            if tail:
                print(tail, file=sys.stderr)
            return 2

        print(f"vitest baseline: vitest exit {proc.returncode} (1 is expected while the baseline has known failures)")
        report = _load_json(report_path)
    return compare_report(report, _load_baseline(DEFAULT_BASELINE))


def synthetic_report(failing_files: list[str], failed_tests: int, passed_tests: int) -> dict:
    """Build a minimal report that exercises both failure-detection paths."""
    results: list[dict] = []
    for index, name in enumerate(failing_files):
        if index == 0:
            # Same shape as the upstream account_timeline failure: suite-level
            # message, status "passed", no assertions.
            results.append(
                {
                    "name": name,
                    "status": "passed",
                    "message": "document is not defined",
                    "assertionResults": [],
                }
            )
        else:
            results.append(
                {
                    "name": name,
                    "status": "failed",
                    "message": "",
                    "assertionResults": [{"status": "failed"}],
                }
            )
    for i in range(3):
        results.append(
            {
                "name": f"spec/self_test_passing_{i}.spec.js",
                "status": "passed",
                "message": "",
                "assertionResults": [{"status": "passed"}],
            }
        )
    return {
        "numTotalTestSuites": len(results),
        "numFailedTests": failed_tests,
        "numPassedTests": passed_tests,
        "numTotalTests": failed_tests + passed_tests,
        "success": False,
        "testResults": results,
    }


def self_test() -> int:
    baseline = _load_baseline(DEFAULT_BASELINE)
    known = list(baseline["failed_files"])
    failed = baseline["failed_tests"]
    passed = baseline["passed_tests"]

    cases = [
        ("baseline-equal", synthetic_report(known, failed, passed), True),
        (
            "injected-new-failing-file",
            synthetic_report(known + ["spec/ci_guard_self_test_injected.spec.js"], failed + 1, passed),
            False,
        ),
        ("extra-failed-test", synthetic_report(known, failed + 1, passed), False),
        ("fewer-passed-tests", synthetic_report(known, failed, passed - 1), False),
        ("known-file-fixed", synthetic_report(known[:-1], max(failed - 1, 0), passed), True),
    ]

    ok_count = 0
    for name, report, expect_accept in cases:
        current = extract_report(report)
        _new, _fixed, problems = evaluate(current, baseline)
        accepted = not problems
        if accepted == expect_accept:
            ok_count += 1
            action = "accepted" if accepted else "rejected"
            print(f"self-test: ok: {name} {action} as expected")
        else:
            action = "accepted" if accepted else "rejected"
            print(
                f"self-test: FAIL: {name} was {action}, expected "
                f"{'accept' if expect_accept else 'reject'}; problems={problems}"
            )

    if ok_count == len(cases):
        print(f"baseline guard self-test OK ({ok_count}/{len(cases)} cases)")
        return 0
    print(f"baseline guard self-test FAILED ({ok_count}/{len(cases)} cases)")
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Fail only on Vitest failures that are new vs the W0-4 baseline."
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--run", action="store_true", help="run vitest and compare with the baseline")
    mode.add_argument("--report", metavar="PATH", help="compare an existing vitest JSON report")
    mode.add_argument("--self-test", action="store_true", help="prove the guard rejects an injected failure")
    args = parser.parse_args(argv)

    try:
        baseline = _load_baseline(DEFAULT_BASELINE)
    except (OSError, ValueError) as exc:
        print(f"vitest baseline: FAIL: cannot load baseline: {exc}", file=sys.stderr)
        return 2

    if args.self_test:
        return self_test()
    if args.run:
        return run_vitest()
    try:
        report = _load_json(args.report)
    except (OSError, ValueError) as exc:
        print(f"vitest baseline: FAIL: cannot read report {args.report}: {exc}", file=sys.stderr)
        return 2
    return compare_report(report, baseline)


if __name__ == "__main__":
    sys.exit(main())
