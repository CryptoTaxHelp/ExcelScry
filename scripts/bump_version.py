"""Keep every copy of the version in step.

`src-tauri/Cargo.toml` is the source of truth: Tauri's bundler, the portable
build and `cargo --locked` all follow it. Three other files repeat the version,
and every one of them has to agree or a release ends up mislabelled:

    src-tauri/Cargo.toml        the [package] version - the source of truth
    src-tauri/tauri.conf.json   what the bundler names the installer after
    src-tauri/Cargo.lock        cargo's own record; missing it fails `--locked`
    README.md                   the artifact table shows the installer's name

Both build scripts deliberately hold no version literal, so there is nothing
there to update - a stale one would silently mislabel the installer.
`--check` asserts that stays true.

Run from the repository root:

    python scripts/bump_version.py                    # report, changes nothing
    python scripts/bump_version.py --check            # fail if a copy disagrees
    python scripts/bump_version.py --check --tag v0.1.0
    python scripts/bump_version.py --print            # just the version, for a script
    python scripts/bump_version.py 0.2.0              # write 0.2.0 to every copy
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

CARGO_TOML = "src-tauri/Cargo.toml"
TAURI_CONF = "src-tauri/tauri.conf.json"
CARGO_LOCK = "src-tauri/Cargo.lock"
README = "README.md"
BUILD_SCRIPTS = ("scripts/build.sh", "scripts/build.ps1")

SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# The [package] version: the only line of Cargo.toml that starts with `version`.
# Dependency versions are inline tables (`tauri = { version = "..." }`) and
# `rust-version` is a different key, so neither can match.
CARGO_TOML_PATTERN = re.compile(r'(?m)^(version\s*=\s*")([^"]+)(")')
# The top level of tauri.conf.json, which is indented by two spaces.
TAURI_CONF_PATTERN = re.compile(r'(?m)^(  "version":\s*")([^"]+)(")')
# cargo writes the root package first; match it by name rather than by position.
CARGO_LOCK_PATTERN = re.compile(
    r'(?m)^(\[\[package\]\]\nname = "ExcelScry"\nversion = ")([^"]+)(")'
)
# Version-agnostic so a stale README is corrected rather than skipped.
README_PATTERN = re.compile(
    r"(ExcelScry-)(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(-setup\.exe)"
)
# Any literal N.N.N in a build script is a stale copy waiting to happen.
STRAY_VERSION = re.compile(r"\b\d+\.\d+\.\d+\b")


def read_text(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


def write_text(relative: str, text: str) -> None:
    (ROOT / relative).write_text(text, encoding="utf-8", newline="")


def version() -> str:
    """The source of truth, read by parsing Cargo.toml rather than by regex."""
    with (ROOT / CARGO_TOML).open("rb") as handle:
        return tomllib.load(handle)["package"]["version"]


def copies() -> dict[str, str | None]:
    """What each copy of the version currently says.

    Read with a real parser where one exists, so a malformed file is reported
    as `None` rather than being silently misread.
    """
    found: dict[str, str | None] = {CARGO_TOML: version()}

    try:
        found[TAURI_CONF] = json.loads(read_text(TAURI_CONF)).get("version")
    except (json.JSONDecodeError, KeyError):
        found[TAURI_CONF] = None

    try:
        with (ROOT / CARGO_LOCK).open("rb") as handle:
            packages = tomllib.load(handle)["package"]
        found[CARGO_LOCK] = next(p["version"] for p in packages if p["name"] == "ExcelScry")
    except (tomllib.TOMLDecodeError, StopIteration):
        found[CARGO_LOCK] = None

    versions = {match.group(2) for match in README_PATTERN.finditer(read_text(README))}
    if not versions:
        found[README] = None
    elif len(versions) == 1:
        found[README] = versions.pop()
    else:  # a stale README that names two different installers
        found[README] = ", ".join(sorted(versions))
    return found


def stray_literals() -> list[str]:
    """Build scripts that carry a hardcoded version."""
    return [name for name in BUILD_SCRIPTS if STRAY_VERSION.search(read_text(name))]


def apply(new: str) -> list[str]:
    """Write `new` to every copy. Returns the files that changed."""
    changed: list[str] = []

    for relative, pattern in (
        (CARGO_TOML, CARGO_TOML_PATTERN),
        (TAURI_CONF, TAURI_CONF_PATTERN),
        (CARGO_LOCK, CARGO_LOCK_PATTERN),
    ):
        text = read_text(relative)
        rewritten, count = pattern.subn(rf"\g<1>{new}\g<3>", text, count=1)
        if count != 1:
            raise SystemExit(f"error: could not find the version in {relative}")
        if rewritten != text:
            write_text(relative, rewritten)
            changed.append(relative)

    readme = read_text(README)
    rewritten = README_PATTERN.sub(rf"\g<1>{new}\g<3>", readme)
    if rewritten != readme:
        write_text(README, rewritten)
        changed.append(README)

    return changed


def report(new_value: str | None = None) -> int:
    """Print every copy and return 0 when they all agree."""
    current = version()
    found = copies()
    print(f"ExcelScry {current}{' -> ' + new_value if new_value else ''}")
    print()

    disagree = []
    for name, value in found.items():
        marks = "source" if name == CARGO_TOML else "copy  "
        if value == current:
            print(f"  ok        {marks} {name:<26} {value}")
        else:
            print(f"  MISMATCH  {marks} {name:<26} {value}")
            disagree.append(name)

    for name in stray_literals():
        print(f"  MISMATCH  copy   {name:<26} hardcoded version literal")
        disagree.append(name)

    if disagree:
        print()
        print(f"error: {len(disagree)} cop{'y' if len(disagree) == 1 else 'ies'} of the")
        print(f"version do not match {CARGO_TOML} ({current}):")
        for name in disagree:
            print(f"  - {name}: {found.get(name)}")
        print()
        print("Run `python scripts/bump_version.py <version>` to bring them into step.")
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Bump the version everywhere, or check that the copies agree."
    )
    parser.add_argument("new_version", nargs="?", help="version to write, e.g. 0.2.0")
    parser.add_argument("--check", action="store_true", help="fail unless every copy agrees")
    parser.add_argument("--print", action="store_true", help="print the version and stop")
    parser.add_argument("--tag", help="with --check: also require a tag naming this version")
    args = parser.parse_args()

    if args.print:
        print(version())
        return 0

    if args.tag and args.tag != f"v{version()}":
        print(
            f"error: tag {args.tag!r} does not name the version in {CARGO_TOML} "
            f"({version()}) - a release tag must match the build it points at",
            file=sys.stderr,
        )
        return 1

    if args.new_version is None:
        return report()

    if not SEMVER.match(args.new_version):
        print(f"error: {args.new_version!r} is not a semantic version", file=sys.stderr)
        return 2
    if args.check:
        print("error: --check inspects the current state; give it no version", file=sys.stderr)
        return 2

    changed = apply(args.new_version)
    status = report()
    if status != 0:
        return status

    print()
    if changed:
        print(f"Updated: {', '.join(changed)}")
    else:
        print("Nothing to change: every copy already said " + args.new_version)
    print()
    print("Cargo.lock is updated above, and cargo confirms it:")
    print("  cargo metadata --manifest-path src-tauri/Cargo.toml --locked")
    print()
    print("Next: add the CHANGELOG entry, then commit and tag `v" + args.new_version + "`.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
