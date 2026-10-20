#!/usr/bin/env python3
"""Prove `verify_workflow.py` refuses a workflow with a broken key preflight.

The release path depends on both builds checking the updater signing key before
they compile. `verify_workflow.py` is the only thing that reads that ordering,
and a check that has stopped reading it would go on passing, including on the
commit that removes or moves the step. So this mutates a copy of the real
workflow - never the checkout - one deliberate way at a time, and requires the
verifier to reject it with the refusal that names the problem.

The copies are written to a temporary directory and the verifier is run against
the copy through its optional path argument. The repository's own workflow is
only read.

    python scripts/test_verify_workflow.py

Exit status 1 means the verifier accepted a workflow it should have refused, or
refused the real one.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile

try:
    import yaml
except ModuleNotFoundError:  # pragma: no cover - a clear failure beats a skip
    raise SystemExit(
        "error: PyYAML is needed to read the workflow (pip install pyyaml)"
    ) from None

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
VERIFY = HERE / "verify_workflow.py"

KEY_CHECK = "Check that the updater signing key is the one this build trusts"
RELEASE_BUILD = "Build the release, as a draft"
INSTALLER_BUILD = "Build the app and the installer"


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  ok    {label}")
        return
    print(f"  FAIL  {label}")
    if detail:
        for line in detail.splitlines():
            print(f"        {line}")
    raise SystemExit(1)


def workflow() -> dict:
    document = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise SystemExit(f"error: {WORKFLOW} is not a workflow document")
    return document


def steps(document: dict, job: str) -> list[dict]:
    return document["jobs"][job]["steps"]


def index_of(steps: list[dict], name: str) -> int:
    for index, step in enumerate(steps):
        if step.get("name") == name:
            return index
    raise SystemExit(f"error: the workflow has no step named {name!r}")


def without_key_check(job: str) -> dict:
    """The workflow with the key preflight deleted from one job."""
    document = workflow()
    job_steps = steps(document, job)
    del job_steps[index_of(job_steps, KEY_CHECK)]
    return document


def key_check_moved_after(job: str, build: str) -> dict:
    """The workflow with the key preflight after the build it protects."""
    document = workflow()
    job_steps = steps(document, job)
    step = job_steps.pop(index_of(job_steps, KEY_CHECK))
    job_steps.insert(index_of(job_steps, build) + 1, step)
    return document


def run(document: dict) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / "ci.yml"
        path.write_text(
            yaml.safe_dump(document, sort_keys=False), encoding="utf-8", newline="\n"
        )
        return subprocess.run(
            [sys.executable, str(VERIFY), str(path)],
            capture_output=True,
            text=True,
        )


def refused(label: str, document: dict, *phrases: str) -> None:
    """The verifier exits 1 and says every part of the reason."""
    result = run(document)
    output = f"{result.stdout or ''}{result.stderr or ''}"
    check(
        label,
        result.returncode == 1,
        f"verify_workflow.py exited {result.returncode}, expected 1\n{output}",
    )
    for phrase in phrases:
        check(f"it says {phrase!r}", phrase in output, output)


def main() -> int:
    print("The committed workflow passes")
    passed = run(workflow())
    check(
        "the real workflow is accepted",
        passed.returncode == 0,
        f"verify_workflow.py exited {passed.returncode}\n"
        f"{passed.stdout or ''}{passed.stderr or ''}",
    )

    print("A missing signing-key check is refused")
    refused(
        "no key check in the release job",
        without_key_check("release"),
        "the release job has no step named",
        KEY_CHECK,
    )
    refused(
        "no key check in the installer job",
        without_key_check("installer"),
        "the installer job has no step named",
        KEY_CHECK,
    )

    print("A signing-key check after the build is refused")
    refused(
        "the release job checks the key too late",
        key_check_moved_after("release", RELEASE_BUILD),
        "has to run before",
        RELEASE_BUILD,
    )
    refused(
        "the installer job checks the key too late",
        key_check_moved_after("installer", INSTALLER_BUILD),
        "installer job's",
        "has to run before it builds",
    )

    print()
    print("A missing or misplaced signing-key check is refused in both build jobs.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
