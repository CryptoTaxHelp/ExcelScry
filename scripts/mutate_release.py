#!/usr/bin/env python3
"""Check that the release test catches a broken release flow.

`scripts/test_release.py` is what makes the release flow's refusals, the release
it makes, and the runs it can finish after they failed part-way into checked
behaviour rather than a description of one. A test is only worth what it
catches, though, and one that has quietly stopped catching something goes on
passing - and keeps passing on the commit that breaks the flow.

So this breaks `scripts/release.py` one deliberate way at a time and runs that
test against the broken copy. Each mutation names the case that is supposed to
notice it. A mutation the test passes, or one it fails somewhere other than the
case that claims it, is a hole in the test, and this exits 1 rather than
reporting a green run.

Nothing here touches this checkout, and nothing is left behind: each worker
copies the files the test reads into a throwaway directory of its own, makes
that copy a repository (the test ends by comparing the `HEAD` and `git status`
of the root it runs from, so it has to be one), and mutates `release.py` inside
its own copy only.

The mutations are text, because a mutation has to be a change to the file: one
whose lines no longer match - because the code it targets was rewritten - is
reported as a failure to fix rather than skipped. A check that stops checking is
worse than one that is red.

    python scripts/mutate_release.py              # every mutation
    python scripts/mutate_release.py --jobs 8     # more of them at once
    python scripts/mutate_release.py 0:4          # a slice, while debugging one

Exit status 1 means the release test does not catch something it claims to.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import queue
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent

# Named rather than run as scripts, so the copy and this checkout cannot
# disagree about which file is being broken.
RELEASE = "scripts/release.py"
TEST = "scripts/test_release.py"
CASE_LABELS = re.compile(r'@case\("([^"]+)"\)')

# What a worker's copy leaves out. Version control, build output, generated
# schemas, and the caches Python and a browser make; the test reads none of
# them, and `src-tauri/target` on its own is gigabytes.
SKIPPED = shutil.ignore_patterns(
    ".git", ".freebuff", "target", "gen", "__pycache__", "dist", "ui-harness.html"
)

# One mutation per entry: what it breaks, the lines it replaces, and the case in
# `test_release.py` that has to fail because of it. A replacement has to match
# the file exactly once - an anchor that matches twice would mutate the wrong
# call, and one that matches nowhere means this list has fallen behind the file.
MUTATIONS: list[tuple[str, list[tuple[str, str]], str]] = [
    (
        "the branch-is-ahead-of-its-remote check removed",
        [('    if missing_from(remote, tracked, "HEAD"):', "    if False:")],
        "a branch that is ahead of its remote",
    ),
    (
        "the behind-its-remote check removed",
        [
            (
                "    if behind(remote, tracked):\n"
                "        raise SystemExit(\n"
                '            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."\n'
                "        )\n"
                "\n"
                "    dirty = dirty_files()",
                "    if False:\n"
                "        raise SystemExit(\n"
                '            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."\n'
                "        )\n"
                "\n"
                "    dirty = dirty_files()",
            )
        ],
        "a branch that is behind its remote",
    ),
    (
        "a version that is not ahead let through",
        [("    if target <= semver(current):", "    if False:")],
        "a version that is not ahead",
    ),
    (
        "the uncommitted-changes check removed",
        [("    dirty = dirty_files()\n    if dirty:", "    dirty = []\n    if dirty:")],
        "an uncommitted change",
    ),
    (
        "an existing tag not noticed",
        [("    if local_tag(tag):", "    if False:")],
        "a tag that already exists",
    ),
    (
        "the empty-Unreleased check removed",
        [("    if not entries.strip():", "    if False:")],
        "an empty Unreleased section",
    ),
    (
        "the changelog's existing version heading not noticed",
        [("    if new_version in headings:", "    if False:")],
        "a changelog that already names the version",
    ),
    (
        "the Unreleased entries left out of the new section",
        [("        + entries\n", "")],
        "a full release",
    ),
    (
        "a stray file written, the allowed-files guard removed, and the commit staged with --all",
        [
            (
                '    (ROOT / CHANGELOG).write_text(rewritten, encoding="utf-8", newline="")',
                '    (ROOT / CHANGELOG).write_text(rewritten, encoding="utf-8", newline="")\n'
                '    (ROOT / "STRAY.txt").write_text("x", encoding="utf-8")',
            ),
            (
                '    if unexpected:\n        raise SystemExit(\n'
                '            "error: the working tree changed in ways a release is not allowed to:',
                '    if False:\n        raise SystemExit(\n'
                '            "error: the working tree changed in ways a release is not allowed to:',
            ),
            ('    git("add", *sorted(ALLOWED & set(dirty_files())))', '    git("add", "--all")'),
        ],
        "a full release",
    ),
    (
        "a lightweight tag instead of an annotated one",
        [('    git("tag", "-a", tag, "-m", f"ExcelScry {new}")', '    git("tag", tag)')],
        "a full release",
    ),
    (
        "the --dry-run early return removed from the release path",
        [
            (
                "    if args.dry_run:\n        print(DRY_RUN)\n        return 0\n\n"
                "    changed = bump_version.apply(new)",
                "    if args.dry_run:\n        print(DRY_RUN)\n\n    changed = bump_version.apply(new)",
            )
        ],
        "--dry-run",
    ),
    (
        "the way out dropped from a failed push",
        [
            (
                '            "       The release commit and its tag are still here, so none of this has\\n"\n'
                '            "       to be made again - finish the release where it stopped with:\\n"\n'
                '            f"         {resume_command()}"\n',
                "",
            )
        ],
        "a tag the remote refuses, and the --resume that finishes it",
    ),
    (
        "--resume's re-assertion of a half-written version removed",
        [("        changed = bump_version.apply(version)", "        changed = []")],
        "a version that was only half written",
    ),
    (
        "--resume's guard on a tag that exists elsewhere removed",
        [("    if tagged and published and tagged[0] != published:", "    if False:")],
        "a tag of this version that exists elsewhere as another tag",
    ),
    (
        "--resume's changelog gate removed",
        [("    if dirty and not has_section(version):", "    if False:")],
        "a version written by hand, with no changelog section for it",
    ),
    (
        "--resume's refusal of a hand edit on top of the release commit removed",
        [("    if dirty and release_head:", "    if False:")],
        "a release commit with a hand edit on top of it",
    ),
    (
        "--resume's refusal of something that is not an interrupted release removed",
        [("    if not (dirty or tagged or release_head):", "    if False:")],
        "--resume over something there is nothing to finish in",
    ),
    (
        "--resume's nothing-to-finish answer removed",
        [("    if not (needs_commit or needs_tag or needs_branch or needs_tag_push):", "    if False:")],
        "a release that is already out, asked to --resume",
    ),
    (
        "--resume's --dry-run early return removed",
        [
            (
                "    if args.dry_run:\n        print(DRY_RUN)\n        return 0\n\n    if needs_commit:",
                "    if args.dry_run:\n        print(DRY_RUN)\n\n    if needs_commit:",
            )
        ],
        "a tag the remote refuses, and the --resume that finishes it",
    ),
    (
        "--resume counts a commit it just made as already pushed",
        [
            (
                '    needs_branch = needs_commit or missing_from(remote, tracked, tagged[1] if tagged else "HEAD")',
                '    needs_branch = missing_from(remote, tracked, tagged[1] if tagged else "HEAD")',
            )
        ],
        "a commit that failed, and the --resume that finishes it",
    ),
    (
        "--resume commits a release with an empty body",
        [
            (
                'f"{release_subject(version)}\\n\\n{section_body(version)}\\n"',
                'f"{release_subject(version)}\\n"',
            )
        ],
        "a commit that failed, and the --resume that finishes it",
    ),
    (
        "--resume commits on top of a release commit that is already there",
        [
            (
                "    needs_commit = bool(dirty) and not unexpected and not release_head",
                "    needs_commit = bool(dirty) and not unexpected",
            )
        ],
        "a release that is already out, asked to --resume",
    ),
    (
        "--resume pushes over a remote that has moved on",
        [
            (
                "    if behind(remote, tracked):\n        raise SystemExit(\n"
                '            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."\n'
                "        )\n"
                "\n"
                "    # The version is what a release writes first, so the one the repository is",
                "    if False:\n        raise SystemExit(\n"
                '            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."\n'
                "        )\n"
                "\n"
                "    # The version is what a release writes first, so the one the repository is",
            )
        ],
        "a tag the remote refuses, and the --resume that finishes it",
    ),
]

# The cases no mutation aims at, and why. Everything else in the test is claimed
# by at least one mutation, so a case that stops failing for the thing it is
# about is caught by this run rather than by the next release.
UNCLAIMED = {
    "the repository it is asked to release":
        "the fixture checking itself, which no mutation of release.py can break",
}


def git(*args: str, cwd: pathlib.Path) -> str:
    """Run git in a worker's copy, or fail loudly: a broken stage is not a result."""
    result = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8"
    )
    if result.returncode != 0:
        raise SystemExit(
            f"error: the harness could not set up its copy: git {' '.join(args)}\n"
            f"{result.stderr.strip()}"
        )
    return result.stdout.strip()


def remove_tree(path: pathlib.Path) -> None:
    """Delete a copy, including the read-only files git keeps in one."""

    def force(function, target, _exception):
        os.chmod(target, stat.S_IWRITE)
        function(target)

    shutil.rmtree(path, onerror=force)


def plural(count: int, noun: str) -> str:
    """'1 mutation', '23 mutations' - a slice of one is a run someone will make."""
    return f"{count} {noun}" if count == 1 else f"{count} {noun}s"


def cases() -> set[str]:
    """The cases `test_release.py` declares, read out of the file itself.

    So that a mutation naming a case that no longer exists - or never did - is a
    failure here, rather than a run that reports the mutation as caught because
    it looked for the wrong heading.
    """
    text = (ROOT / TEST).read_text(encoding="utf-8")
    found = CASE_LABELS.findall(text)
    if not found:
        raise SystemExit(f"error: no `@case(...)` labels found in {TEST}")
    return set(found)


def text_of(release: bytes) -> str:
    """The file as the mutations are written: text, with LF line endings.

    Bytes are what is put back, so a checkout with CRLF ends up with exactly the
    bytes it started with; the mutations themselves are matched against one form
    rather than two.
    """
    return release.decode("utf-8").replace("\r\n", "\n")


def apply_mutation(text: str, replacements: list[tuple[str, str]], label: str) -> str:
    """The file with one mutation applied, or a failure saying why it cannot be."""
    for old, new in replacements:
        found = text.count(old)
        if found != 1:
            raise SystemExit(
                f"error: the mutation {label!r} no longer applies to {RELEASE}:\n"
                f"       {found} matches for {old.splitlines()[0].strip()[:70]!r}.\n"
                "       The lines this mutation targets have changed. Update the\n"
                "       mutation in this file to match, or drop it and the case it\n"
                "       claims if the behaviour is gone."
            )
        text = text.replace(old, new, 1)
    return text


def stage(copy: pathlib.Path, release: bytes) -> None:
    """Copy this checkout into `copy`, and make that copy a repository."""
    shutil.copytree(ROOT, copy, ignore=SKIPPED, symlinks=True)
    if (copy / RELEASE).read_bytes() != release:
        raise SystemExit(
            f"error: the copy at {copy} does not hold the bytes of {RELEASE} that\n"
            "       this checkout does, so it is not the release flow being tested."
        )
    for setting in (
        ("init", "--quiet"),
        ("config", "user.name", "ExcelScry mutation harness"),
        ("config", "user.email", "mutations@example.invalid"),
        ("config", "commit.gpgsign", "false"),
        ("config", "tag.gpgsign", "false"),
        ("config", "core.autocrlf", "false"),
    ):
        git(*setting, cwd=copy)
    git("add", "--all", cwd=copy)
    git("commit", "--quiet", "--message", "This checkout, copied", cwd=copy)


def run_test(copy: pathlib.Path) -> subprocess.CompletedProcess:
    """The release test, run from the copy, so it reads the mutated file."""
    return subprocess.run(
        [sys.executable, TEST],
        cwd=copy,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )


def failures_by_case(stdout: str, labels: set[str]) -> dict[str, list[str]]:
    """Each case's failures, read from the FAIL lines under its own heading.

    Not from the summary at the end: a mutation can break a later case badly
    enough that its fixture setup fails, and the run stops before printing that
    summary - leaving the failures it did report unattributable. The heading
    each FAIL line sits under is the answer either way, and it also keeps a
    check's own printed detail from being read as another failure.
    """
    current = ""
    found: dict[str, list[str]] = {}
    for line in stdout.splitlines():
        stripped = line.strip()
        if stripped in labels:
            current = stripped
        elif stripped.startswith("FAIL "):
            found.setdefault(current, []).append(stripped.split(None, 1)[1].strip())
    return found


def classify(result: subprocess.CompletedProcess, claimed: str, labels: set[str]) -> dict:
    """What the test did about the mutation, and the failure to fix if it missed it."""
    said = result.stdout + result.stderr
    by_case = failures_by_case(result.stdout, labels)
    caught_in_claimed = bool(by_case.get(claimed))
    elsewhere = [f"{case}: {failure}" for case, failures in by_case.items() for failure in failures]

    if caught_in_claimed:
        return {"status": "caught", "elsewhere": elsewhere}
    tail = "\n".join(said.splitlines()[-12:])
    if result.returncode == 0:
        return {
            "status": "passed",
            "complaint": f"the test passed a broken {RELEASE}",
            "elsewhere": elsewhere,
            "tail": tail,
        }
    return {
        "status": "misfired",
        "complaint": f"the test failed, but {claimed!r} reported nothing",
        "elsewhere": elsewhere,
        "tail": tail,
    }


def worker(
    copy: pathlib.Path,
    release: bytes,
    work: queue.Queue,
    results: list,
    lock: threading.Lock,
    labels: set[str],
    total: int,
) -> None:
    """Run mutations one at a time in this worker's own copy.

    `total` is the whole list, not the slice: the number printed in front of
    each mutation names its place in `MUTATIONS`, so a run of one slice still
    says which mutation it is looking at.

    The mutated bytes go in, the test runs, and the copy's `release.py` goes
    straight back to the bytes of this checkout - so the next mutation starts
    from the release flow as written, whatever the last one did.
    """
    while True:
        try:
            slot, index, label, replacements, claimed = work.get_nowait()
        except queue.Empty:
            return
        mutated = apply_mutation(text_of(release), replacements, label)
        (copy / RELEASE).write_text(mutated, encoding="utf-8", newline="\n")
        try:
            result = run_test(copy)
        finally:
            (copy / RELEASE).write_bytes(release)
        outcome = classify(result, claimed, labels)
        with lock:
            results[slot] = outcome
            print(f"{index + 1:>3}/{total}  {label}")
            if outcome["status"] == "caught":
                print(f"         caught in {claimed!r}")
            else:
                print(f"         {outcome['complaint']}")
                for line in outcome["elsewhere"][:3]:
                    print(f"           elsewhere: {line}")
                for line in outcome["tail"].splitlines():
                    print(f"           {line}")
            print()
            sys.stdout.flush()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Break release.py one way at a time, and require its test to notice."
    )
    parser.add_argument(
        "slice", nargs="?", default="", help="a 'start:end' slice of the mutation list"
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=0,
        help="how many mutations to run at once (default: 4, or fewer on a smaller machine)",
    )
    args = parser.parse_args()

    labels = cases()
    start, _, finish = args.slice.partition(":")
    start = int(start) if start else 0
    end = int(finish) if finish else len(MUTATIONS)
    selected = MUTATIONS[start:end]
    if not selected:
        raise SystemExit(f"error: no mutations in the slice {args.slice!r}")
    for _label, _replacements, claimed in selected:
        if claimed not in labels:
            raise SystemExit(
                f"error: a mutation claims the case {claimed!r}, which {TEST} does\n"
                "       not declare. Fix the case name in this file."
            )

    release = (ROOT / RELEASE).read_bytes()
    text = text_of(release)
    # Every mutation is applied once before anything runs, so a list that has
    # fallen behind the file fails in a second rather than after a long run.
    prepared = []
    for index, (label, replacements, claimed) in enumerate(selected, start=start):
        if apply_mutation(text, replacements, label) == text:
            raise SystemExit(f"error: the mutation {label!r} changes nothing")
        prepared.append((index, label, replacements, claimed))

    jobs = args.jobs or max(1, min(4, os.cpu_count() or 1))
    jobs = max(1, min(jobs, len(prepared)))
    print("ExcelScry release mutation check")
    print()
    print(f"{plural(len(prepared), 'mutation')} of {RELEASE}, each of which one case in")
    print(f"{TEST} has to catch. {jobs} of them at a time.")
    print()

    root = pathlib.Path(tempfile.mkdtemp(prefix="excelscry-mutations-"))
    try:
        copies = []
        for number in range(jobs):
            copy = root / f"copy-{number}"
            stage(copy, release)
            copies.append(copy)
        print(f"Each worker has a copy of this checkout under {root}.")

        # A mutation run over a test that is already failing proves nothing, and
        # would report every mutation as caught.
        baseline = run_test(copies[0])
        by_case = failures_by_case(baseline.stdout, labels)
        if baseline.returncode != 0 or by_case:
            print()
            print("The release test does not pass on an unmutated checkout, so there")
            print("is nothing here to falsify. The end of what it said:")
            print()
            for line in (baseline.stdout + baseline.stderr).splitlines()[-20:]:
                print(f"  {line}")
            return 1
        print(f"It passes unmutated: {baseline.stdout.count('  ok    ')} checks, no failures.")
        print()

        results: list[dict] = [None] * len(prepared)
        work: queue.Queue = queue.Queue()
        for slot, entry in enumerate(prepared):
            work.put((slot, *entry))
        lock = threading.Lock()
        threads = [
            threading.Thread(
                target=worker,
                args=(copy, release, work, results, lock, labels, len(MUTATIONS)),
                daemon=True,
            )
            for copy in copies
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()

        holes = [
            (label, outcome)
            for (_index, label, _replacements, _claimed), outcome in zip(prepared, results)
            if outcome["status"] != "caught"
        ]
        print(
            f"{plural(len(prepared), 'mutation')}, {len(prepared) - len(holes)} of them"
            " caught in the"
        )
        print("case that claims them." if not holes else f"case that claims them; {len(holes)} not:")
        for label, outcome in holes:
            print(f"  hole: {label}")
            print(f"        {outcome['complaint']}")

        aimed_at = {claimed for _label, _replacements, claimed in MUTATIONS}
        aimed_at |= set(UNCLAIMED)
        unclaimed = [case for case in sorted(labels) if case not in aimed_at]
        if unclaimed:
            print()
            print(f"{TEST} has {len(unclaimed)} case(s) no mutation aims at, so nothing")
            print("here would notice if one stopped failing for what it is about:")
            for case in unclaimed:
                print(f"  - {case}")

        if holes:
            return 1
        print()
        print(f"All {len(prepared)} mutations were caught, each in the case that claims")
        print("it: the release test fails when the release flow is broken, and it goes")
        print("on passing when it is not.")
        return 0
    finally:
        remove_tree(root)


if __name__ == "__main__":
    raise SystemExit(main())
