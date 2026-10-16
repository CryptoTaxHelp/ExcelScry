"""Cut a release: bump, move the changelog, commit, tag, push.

The pieces already existed - `bump_version.py` owns the four copies of the
version, `release_notes.py` owns the notes the release page shows, and the
`release` job publishes whatever a `v*` tag points at. What was missing was the
sequence itself, which meant a release was four commands and a hand-edited
changelog, each with a chance to be done in the wrong order.

Everything before the push is local and reversible, and the push is the step
that starts the release job, so nothing happens until the checks pass:

* the tree is clean, and the branch is in step with its upstream (a release
  commit on top of someone else's work is not a release);
* the new version is a semantic version ahead of the current one, and no such
  tag exists locally or on the remote;
* `CHANGELOG.md` has `Unreleased` entries to move, and the version heading is
  not already there;
* afterwards, the same check CI runs - `bump_version.py --check --tag` - has to
  pass, and only the files a release may touch are allowed to have changed.

Run from anywhere:

    python scripts/release.py 0.1.2              # do it
    python scripts/release.py 0.1.2 --dry-run    # say what it would do
"""

from __future__ import annotations

import argparse
import datetime
import pathlib
import re
import subprocess
import sys

SCRIPTS = pathlib.Path(__file__).resolve().parent
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

import bump_version  # noqa: E402  (the path above is what makes this importable)
import release_notes  # noqa: E402

CHANGELOG = "CHANGELOG.md"
UNRELEASED = re.compile(r"(?m)^## \[Unreleased\][ \t]*$")
NEXT_SECTION = re.compile(r"(?m)^## \[")
VERSION_HEADING = re.compile(r"(?m)^## \[(?P<version>[^\]]+)\]")

# A release commit may only carry these: the four copies of the version and the
# changelog. Anything else in the tree means the run started with something
# uncommitted, which the clean-tree check should have caught first.
ALLOWED = {
    bump_version.CARGO_TOML,
    bump_version.TAURI_CONF,
    bump_version.CARGO_LOCK,
    bump_version.README,
    CHANGELOG,
}


def git(*args: str) -> str:
    """Run a git command in the repository, or fail with git's own message."""
    return release_notes.git(*args)


def semver(value: str) -> tuple[int, ...]:
    if not bump_version.SEMVER.match(value):
        raise SystemExit(f"error: {value!r} is not a semantic version (like 0.2.0)")
    return tuple(int(part) for part in value.split(".")[:3])


def upstream() -> tuple[str, str, str]:
    """The tracking remote, the branch there, and the local branch.

    Both names are needed: the remote branch is what the commit history is
    compared against, and the local one is what gets pushed to it. They are the
    same name in this repository, but a local branch may track a differently
    named one, and pushing then needs an explicit `local:remote` refspec - which
    also keeps the push from depending on a `push.default` the user may have
    set.
    """
    try:
        track = git("rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}").strip()
        local = git("rev-parse", "--abbrev-ref", "HEAD").strip()
    except SystemExit:
        raise SystemExit(
            "error: the current branch has no upstream, so there is nothing to push to.\n"
            "       Push it once by hand and this script can take over."
        ) from None
    remote, _, tracked = track.partition("/")
    return remote, tracked, local


def moved_changelog(new_version: str, date: str) -> tuple[str, str]:
    """Rewrite the changelog, returning the new text and the entries moved.

    The `Unreleased` heading stays where it is and a version heading is inserted
    under it, which is how every released section in this file already reads.
    """
    path = ROOT / CHANGELOG
    if not path.is_file():
        raise SystemExit(f"error: {CHANGELOG} does not exist")
    text = path.read_text(encoding="utf-8")

    headings = {match.group("version") for match in VERSION_HEADING.finditer(text)}
    if new_version in headings:
        raise SystemExit(f"error: {CHANGELOG} already has a `## [{new_version}]` section")

    unreleased = UNRELEASED.search(text)
    if not unreleased:
        raise SystemExit(f"error: no `## [Unreleased]` heading in {CHANGELOG}")

    rest = NEXT_SECTION.search(text, unreleased.end())
    end = rest.start() if rest else len(text)
    entries = text[unreleased.end() : end].strip("\n")
    if not entries.strip():
        raise SystemExit(
            f"error: the `Unreleased` section of {CHANGELOG} is empty, so there is\n"
            f"       nothing to release. Write what changed before cutting a release."
        )

    rewritten = (
        text[: unreleased.end()]
        + f"\n\n## [{new_version}] - {date}\n\n"
        + entries
        + "\n\n"
        + text[end:].lstrip("\n")
    )
    return rewritten, entries


def dirty_files() -> list[str]:
    listing = git("status", "--porcelain")
    # "XY path" - the status letters and the space before the path are fixed.
    return [line[3:].strip().strip('"') for line in listing.splitlines() if line.strip()]


def run(*args: str, quiet: bool = False) -> None:
    """Run a command, or fail with its own output.

    `quiet` is for commands that report by their exit code and print pages of
    detail otherwise, such as `cargo metadata`; the detail is kept back until
    something goes wrong. Output is flushed first, so a redirected run does not
    show the command's output ahead of the plan that led to it.
    """
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        result = subprocess.run(
            args, cwd=ROOT, text=True, encoding="utf-8", capture_output=quiet
        )
    except FileNotFoundError:
        raise SystemExit(
            f"error: {args[0]} is not on PATH, and the release flow needs it.\n"
            f"       See 'Building' in README.md for what a working checkout has."
        ) from None
    if result.returncode != 0:
        if quiet:
            sys.stdout.write(result.stdout or "")
            sys.stderr.write(result.stderr or "")
        raise SystemExit(f"error: {' '.join(args)} failed with exit code {result.returncode}")


def actions_url(remote: str) -> str:
    """The workflow page for this repository, derived from the remote's URL."""
    url = git("remote", "get-url", remote).strip()
    match = re.search(r"github\.com[:/](?P<slug>[^/]+/[^/]+?)(?:\.git)?$", url)
    return f"https://github.com/{match.group('slug')}/actions" if match else url


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Bump, move the changelog, commit, tag and push a release."
    )
    parser.add_argument("version", help="the version to release, e.g. 0.1.2")
    parser.add_argument(
        "--dry-run", action="store_true", help="report what would happen and change nothing"
    )
    args = parser.parse_args()

    new = args.version
    target = semver(new)
    current = bump_version.version()
    date = datetime.date.today().isoformat()

    if target <= semver(current):
        raise SystemExit(
            f"error: {new} is not ahead of the current version ({current}).\n"
            f"       A release has to name a version that is not out yet."
        )

    # Checks that need the remote come first, while nothing has been written.
    git("fetch", "--tags", "--quiet")
    remote, tracked, local = upstream()
    if git("rev-list", "--count", f"{remote}/{tracked}..HEAD").strip() != "0":
        raise SystemExit(
            f"error: {tracked} has commits that are not on {remote}/{tracked} yet.\n"
            f"       Push or drop them before cutting a release."
        )
    if git("rev-list", "--count", f"HEAD..{remote}/{tracked}").strip() != "0":
        raise SystemExit(
            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."
        )

    dirty = dirty_files()
    if dirty:
        raise SystemExit(
            "error: the working tree has uncommitted changes:\n"
            + "".join(f"       {name}\n" for name in dirty)
            + "       A release commit should carry the version and nothing else."
        )

    if f"v{new}" in git("tag", "--list", f"v{new}").split():
        raise SystemExit(f"error: tag v{new} already exists locally")
    if git("ls-remote", "--tags", remote, f"refs/tags/v{new}").strip():
        raise SystemExit(f"error: tag v{new} already exists on {remote}")

    rewritten, entries = moved_changelog(new, date)

    print(f"Release ExcelScry {new} (currently {current})")
    print()
    print(f"  version      {', '.join(sorted(ALLOWED - {CHANGELOG}))}")
    print(f"  changelog    {CHANGELOG}: the Unreleased entries move under ## [{new}] - {date}")
    print(f"  commit       Release ExcelScry {new}")
    print(f"  tag          v{new}")
    destination = f"{remote} {local}" if local == tracked else f"{remote} {local}:{tracked}"
    print(f"  push         {destination} and v{new}")
    print()
    print("The Unreleased entries going into the release:")
    print()
    for line in entries.splitlines():
        print(f"  {line}" if line.strip() else "")
    print()

    if args.dry_run:
        print("--dry-run: nothing was written, committed, tagged or pushed.")
        return 0

    changed = bump_version.apply(new)
    (ROOT / CHANGELOG).write_text(rewritten, encoding="utf-8", newline="")
    print(
        f"Updated: {', '.join([*changed, CHANGELOG])}" if changed else f"Updated: {CHANGELOG}",
        flush=True,
    )

    # The check CI runs, on the state that is about to be committed.
    run(sys.executable, str(SCRIPTS / "bump_version.py"), "--check", "--tag", f"v{new}")
    run(
        "cargo",
        "metadata",
        "--manifest-path",
        bump_version.CARGO_TOML,
        "--locked",
        "--format-version",
        "1",
        quiet=True,
    )
    print()

    unexpected = [name for name in dirty_files() if name not in ALLOWED]
    if unexpected:
        raise SystemExit(
            "error: the working tree changed in ways a release is not allowed to:\n"
            + "".join(f"       {name}\n" for name in unexpected)
            + "       Nothing has been committed; inspect the diff and try again."
        )

    body = f"Release ExcelScry {new}\n\n{entries}\n"
    git("add", *sorted(ALLOWED & set(dirty_files())))
    sys.stdout.flush()
    result = subprocess.run(
        ["git", "commit", "--file", "-"], cwd=ROOT, input=body, text=True, encoding="utf-8"
    )
    if result.returncode != 0:
        raise SystemExit("error: git commit failed; the changes are staged but not committed")

    git("tag", "-a", f"v{new}", "-m", f"ExcelScry {new}")
    print()
    print(f"Committed {git('rev-parse', '--short', 'HEAD').strip()} and tagged v{new}")
    print()
    # Read after the commit so this is what the release job will compute from the
    # tagged commit, down to the release commit it lists first.
    print("What the release page will say:")
    print()
    print(release_notes.notes(new), end="")

    git("push", remote, f"{local}:{tracked}")
    try:
        git("push", remote, f"v{new}")
    except SystemExit as error:
        raise SystemExit(
            f"{error}\n       The tag exists locally; finish with: git push {remote} v{new}"
        ) from None

    print()
    print(f"Pushed {tracked} and v{new}. The tag starts the release job:")
    print(f"  {actions_url(remote)}")
    print()
    print("It needs the repository secret TAURI_SIGNING_PRIVATE_KEY; without it the")
    print("build fails rather than publishing an installer no copy of the app would")
    print("accept. See 'Updates' in README.md.")
    print()
    print("When it has published, the `verify-release` job asks the endpoint the app")
    print("asks and checks the installer's signature against the key in the app, so a")
    print("release no installed copy could update from fails its own run.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
