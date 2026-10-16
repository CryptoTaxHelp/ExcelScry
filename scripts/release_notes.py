"""Assemble the release notes for a version.

Two sources are combined, because they answer different questions:

* the curated `CHANGELOG.md` entry says what changed for someone using the app,
  and it is the part edited by hand before a release;
* the commit history since the previous tag says what actually went in, taken
  straight from `git log` so it cannot drift from the repository.

The result is what the release job hands to `tauri-action` as the release body,
with GitHub's own generated notes appended after it. Run it before tagging to
check the entry reads well, or let CI call it.

Run from the repository root:

    python scripts/release_notes.py              # the version in Cargo.toml
    python scripts/release_notes.py 0.1.0
    python scripts/release_notes.py --out release-notes.md
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

CARGO_TOML = "src-tauri/Cargo.toml"
CHANGELOG = "CHANGELOG.md"


def current_version() -> str:
    """The version the manifests are on - the source of truth for the release."""
    with (ROOT / CARGO_TOML).open("rb") as handle:
        return tomllib.load(handle)["package"]["version"]


def git(*args: str) -> str:
    """Run a git command, or fail with git's own message."""
    result = subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8"
    )
    if result.returncode != 0:
        message = result.stderr.strip() or result.stdout.strip()
        raise SystemExit(f"error: git {' '.join(args)} failed: {message}")
    return result.stdout


def changelog_section(version: str) -> str:
    """The CHANGELOG entry for `version`, heading included."""
    path = ROOT / CHANGELOG
    if not path.is_file():
        raise SystemExit(f"error: {CHANGELOG} does not exist")
    pattern = re.compile(rf"(?ms)^## \[{re.escape(version)}\][^\n]*\n.*?(?=^## |\Z)")
    match = pattern.search(path.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(
            f"error: no `## [{version}]` section in {CHANGELOG}.\n"
            f"       Move the Unreleased items into a {version} section before releasing."
        )
    return match.group(0).strip() + "\n"


def previous_tag(version: str) -> str | None:
    """The highest `v*` tag that is not this version, or None for a first release."""
    tags = [t for t in git("tag", "--list", "v*", "--sort=-v:refname").split() if t]
    remaining = [t for t in tags if t != f"v{version}"]
    return remaining[0] if remaining else None


def commits(previous: str | None) -> list[str]:
    """One line per commit, newest first."""
    revision = f"{previous}..HEAD" if previous else "HEAD"
    log = git("log", revision, "--no-merges", "--pretty=format:- %s (%h)")
    return [line for line in log.splitlines() if line.strip()]


def notes(version: str) -> str:
    """The full release body for `version`."""
    previous = previous_tag(version)
    body = changelog_section(version)

    if previous:
        body += f"\n### Commits since {previous}\n\n"
        lines = commits(previous)
    else:
        body += "\n### Commits in this release\n\n"
        body += "_No earlier tag: this is the whole history._\n\n"
        lines = commits(None)

    if lines:
        body += "\n".join(lines) + "\n"
    else:
        body += "_No commits since the previous tag._\n"
        print("warning: no commits since the previous tag", file=sys.stderr)
    return body


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the release notes for a version.")
    parser.add_argument("version", nargs="?", help="defaults to the version in Cargo.toml")
    parser.add_argument("--out", help="write to this file instead of stdout")
    args = parser.parse_args()

    version = args.version or current_version()
    body = notes(version)

    if args.out:
        (ROOT / args.out).write_text(body, encoding="utf-8", newline="")
        print(f"Wrote {args.out} ({len(body.splitlines())} lines)")
    else:
        print(body, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
