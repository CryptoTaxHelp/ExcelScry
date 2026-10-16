#!/usr/bin/env python3
"""Check that this workflow cannot publish a release before its manifest is signed.

The app will not use a manifest nothing signed, so a release that became visible
with an unsigned one would be refused by every copy that asked, until the
signature landed. That is why the release job builds the release as a **draft**
and publishes it in its last step, after the manifest has been signed and the
signature has been read back out of the draft and checked. A draft is invisible
to `.../releases/latest/download/latest.json`, so while all of that is happening
the endpoint goes on answering with the previous release's signed manifest.

None of that is something a release can be tested for afterwards: the
`verify-release` job runs once the release is public, and it passes whether or
not the manifest was ever briefly unsigned. So the workflow itself is read here
instead, and the ordering it has to have is asserted:

* the release job's build creates a draft, not a published release;
* the manifest is signed after that build and before anything is published;
* the step that publishes runs the release check over the manifest and its
  signature, and only then marks the release published;
* nothing else in the workflow can make a release visible at all.

Run from the repository root, or let the checks job run it on every push:

    python scripts/verify_workflow.py

Exit status 1 means the workflow would publish something a copy could read before
its manifest was signed.
"""

from __future__ import annotations

import pathlib
import sys

try:
    import yaml
except ModuleNotFoundError:  # pragma: no cover - a clear failure beats a skip
    raise SystemExit(
        "error: PyYAML is needed to read the workflow (pip install pyyaml)"
    ) from None

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"

# What the steps are called, and what the step that publishes has to contain.
BUILD = "Build the release, as a draft"
SIGN = "Sign the update manifest"
PUBLISH = "Publish the signed release"
VISIBLE = "--draft=false"
CHECK = "verify_release.py"


def step_name(step: dict) -> str:
    return step.get("name", step.get("uses", "?"))


def main() -> int:
    text = WORKFLOW.read_text(encoding="utf-8")
    workflow = yaml.safe_load(text)

    problems: list[str] = []

    def require(condition: bool, complaint: str) -> None:
        if not condition:
            problems.append(complaint)

    jobs = workflow["jobs"]
    steps = jobs["release"]["steps"]
    names = [step_name(step) for step in steps]

    for missing in (BUILD, SIGN, PUBLISH):
        if missing not in names:
            raise SystemExit(
                f"error: the release job has no step named {missing!r}. If it was\n"
                "       renamed, rename it here too: these checks are about the\n"
                "       order the release becomes visible in, and they cannot see\n"
                "       it if they cannot find the steps."
            )

    build, sign, publish = (names.index(name) for name in (BUILD, SIGN, PUBLISH))

    # The draft, which is what keeps the release out of reach while the manifest
    # is unsigned.
    release_build = steps[build]
    require(
        release_build.get("with", {}).get("releaseDraft") is True,
        f"the {BUILD!r} step does not ask for a draft release: a release that is\n"
        "       published here is one the endpoint serves while `latest.json` is\n"
        "       still unsigned",
    )

    # The order, and the check in the step that publishes.
    require(build < sign, f"{SIGN!r} has to run after {BUILD!r}")
    require(sign < publish, f"{PUBLISH!r} has to run after the manifest is signed")
    require(publish == len(steps) - 1, f"{PUBLISH!r} has to be the last step of the job")

    publish_run = steps[publish].get("run", "")
    require(
        CHECK in publish_run and "--signature-only" in publish_run,
        f"the {PUBLISH!r} step has to check the signed manifest with\n"
        "       `verify_release.py --signature-only` before it publishes",
    )
    require(
        "--pattern 'latest.json*'" in publish_run,
        f"the {PUBLISH!r} step has to fetch the manifest and its signature",
    )
    if CHECK in publish_run and VISIBLE in publish_run:
        require(
            publish_run.index(CHECK) < publish_run.index(VISIBLE),
            f"the {PUBLISH!r} step has to check the manifest before publishing it",
        )
    require(
        VISIBLE in publish_run,
        f"the {PUBLISH!r} step has to be what publishes the release ({VISIBLE})",
    )

    # And nothing else can make a release visible: no step creates one, and no
    # other job asks the action for one.
    require("gh release create" not in text, "nothing in this workflow should create a release")
    published = [line for line in text.splitlines() if VISIBLE in line]
    require(len(published) == 1, f"{VISIBLE} should appear exactly once, in {PUBLISH!r}")

    for job, spec in jobs.items():
        if job == "release":
            continue
        for step in spec["steps"]:
            if "tauri-apps/tauri-action" not in str(step.get("uses", "")):
                continue
            asked = step.get("with", {})
            require(
                "tagName" not in asked and "releaseId" not in asked,
                f"the {job!r} job asks tauri-action for a release, which would\n"
                "       publish one without this job's signing steps",
            )

    if problems:
        print("error: this workflow can publish a release whose manifest is not signed yet.")
        for problem in problems:
            print(f"  - {problem}")
        print()
        print("The app refuses an unsigned manifest, so a copy that checks in that")
        print("window is offered nothing; see 'Updates' in README.md.")
        return 1

    print("The release job builds a draft, signs its manifest, checks the signature")
    print("back out of the draft and publishes it last; nothing else can publish one.")
    for index, name in enumerate(names):
        marker = " <- publishes" if index == publish else ""
        print(f"  {index}: {name}{marker}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
