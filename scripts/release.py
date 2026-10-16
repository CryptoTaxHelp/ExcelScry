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

A release that fails once the version is written - the commit, the tag, or one
of the two pushes is what can fail - leaves a repository that is neither
released nor ready to be released again: the version is no longer ahead of
itself, and everything the run already wrote is still wanted. `--resume`
finishes that run instead of repeating it. It commits what is still
uncommitted, makes the tag again if there is none, and pushes whatever the
remote does not have, saying which of those each step is before it does it.
Nothing a release has already done is done twice, and a release that is already
out is left alone.

Run from anywhere:

    python scripts/release.py 0.1.2              # do it
    python scripts/release.py 0.1.2 --dry-run    # say what it would do
    python scripts/release.py --resume           # finish a run that stopped part-way
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

# Said by both paths, so a dry run always reads the same.
DRY_RUN = "--dry-run: nothing was written, committed, tagged or pushed."


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


def release_subject(version: str) -> str:
    """What a release commit is called, which is how a later run recognises it."""
    return f"Release ExcelScry {version}"


def head_subject() -> str:
    """The subject of HEAD."""
    return git("log", "-1", "--pretty=%s").strip()


def short(revision: str = "HEAD") -> str:
    """A revision as the short hash to show in a plan."""
    return git("rev-parse", "--short", revision).strip()


def behind(remote: str, tracked: str) -> bool:
    """Whether the remote branch has commits this branch does not."""
    return git("rev-list", "--count", f"HEAD..{remote}/{tracked}").strip() != "0"


def missing_from(remote: str, tracked: str, revision: str) -> bool:
    """Whether `revision` is not on the remote branch yet."""
    return git("rev-list", "--count", f"{remote}/{tracked}..{revision}").strip() != "0"


def local_tag(tag: str) -> tuple[str, str] | None:
    """The tag's own object and the commit behind it, or None when there is none.

    The object is what the remote's copy is compared against: an annotated tag
    and the commit it points at are different objects, and comparing a remote
    tag object against a local commit would call every tag a mismatch.
    """
    if tag not in git("tag", "--list", tag).split():
        return None
    return git("rev-parse", tag).strip(), git("rev-parse", f"{tag}^{{commit}}").strip()


def remote_tag(remote: str, tag: str) -> str | None:
    """The sha of this tag on the remote, or None when the remote has no such tag."""
    for line in git("ls-remote", "--tags", remote, f"refs/tags/{tag}").splitlines():
        sha, _, name = line.partition("\t")
        if name.strip() == f"refs/tags/{tag}":
            return sha.strip()
    return None


def has_section(version: str) -> bool:
    """Whether the changelog already has a section for this version."""
    text = (ROOT / CHANGELOG).read_text(encoding="utf-8")
    return any(match.group("version") == version for match in VERSION_HEADING.finditer(text))


def section_body(version: str) -> str:
    """The changelog section without its heading - what a release commit carries."""
    _heading, _, body = release_notes.changelog_section(version).partition("\n")
    return body.strip()


def resume_command() -> str:
    """How to run this script again, spelled the way it was most likely started."""
    invoked = pathlib.Path(sys.argv[0])
    try:
        return f"python {invoked.resolve().relative_to(ROOT).as_posix()} --resume"
    except ValueError:  # started from outside the repository, so name it in full
        return f"{sys.executable} {invoked} --resume"


def gate(version: str) -> None:
    """The checks CI runs, against the state that is about to be tagged."""
    run(sys.executable, str(SCRIPTS / "bump_version.py"), "--check", "--tag", f"v{version}")
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


def commit_release(version: str, body: str) -> None:
    """Stage exactly what a release may touch and commit it with `body`."""
    git("add", *sorted(ALLOWED & set(dirty_files())))
    sys.stdout.flush()
    result = subprocess.run(
        ["git", "commit", "--file", "-"], cwd=ROOT, input=body, text=True, encoding="utf-8"
    )
    if result.returncode != 0:
        raise SystemExit(
            "error: git commit failed; the changes are staged but not committed.\n"
            "       Nothing was tagged or pushed, and the work is still in the tree.\n"
            f"       Once the tree is as it should be, finish the release with:\n"
            f"         {resume_command()}"
        )


def push(remote: str, refspec: str) -> None:
    """Push one ref. A push is the first thing a release cannot undo, so a
    failure here names the way to finish the release rather than leaving the
    commit and the tag to be worked out by hand."""
    try:
        git("push", remote, refspec)
    except SystemExit as error:
        raise SystemExit(
            f"{error}\n"
            "       The release commit and its tag are still here, so none of this has\n"
            "       to be made again - finish the release where it stopped with:\n"
            f"         {resume_command()}"
        ) from None


def job_notes(remote: str, tag: str) -> None:
    """What the tagged push starts, and what that job needs."""
    print(f"The tag {tag} starts the release job:")
    print(f"  {actions_url(remote)}")
    print()
    print("It needs the repository secret TAURI_SIGNING_PRIVATE_KEY; without it the")
    print("build fails rather than publishing an installer no copy of the app would")
    print("accept. See 'Updates' in README.md.")
    print()
    print("The release stays a draft until its manifest is signed and the signature")
    print("has been checked back out of it, so no copy is ever offered an unsigned")
    print("manifest. When it has published, the `verify-release` job asks the")
    print("endpoint the app asks and checks the installer's signature against the key")
    print("in the app, so a release no installed copy could update from fails its own")
    print("run.")


def actions_url(remote: str) -> str:
    """The workflow page for this repository, derived from the remote's URL."""
    url = git("remote", "get-url", remote).strip()
    match = re.search(r"github\.com[:/](?P<slug>[^/]+/[^/]+?)(?:\.git)?$", url)
    return f"https://github.com/{match.group('slug')}/actions" if match else url


def release(args) -> int:
    """Cut a release: bump, move the changelog, commit, tag and push."""
    new = args.version
    tag = f"v{new}"
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
    if missing_from(remote, tracked, "HEAD"):
        raise SystemExit(
            f"error: {tracked} has commits that are not on {remote}/{tracked} yet.\n"
            f"       Push or drop them before cutting a release."
        )
    if behind(remote, tracked):
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

    if local_tag(tag):
        raise SystemExit(f"error: tag v{new} already exists locally")
    if remote_tag(remote, tag):
        raise SystemExit(f"error: tag v{new} already exists on {remote}")

    rewritten, entries = moved_changelog(new, date)

    print(f"Release ExcelScry {new} (currently {current})")
    print()
    print(f"  version      {', '.join(sorted(ALLOWED - {CHANGELOG}))}")
    print(f"  changelog    {CHANGELOG}: the Unreleased entries move under ## [{new}] - {date}")
    print(f"  commit       {release_subject(new)}")
    print(f"  tag          {tag}")
    destination = f"{remote} {local}" if local == tracked else f"{remote} {local}:{tracked}"
    print(f"  push         {destination} and {tag}")
    print()
    print("The Unreleased entries going into the release:")
    print()
    for line in entries.splitlines():
        print(f"  {line}" if line.strip() else "")
    print()

    if args.dry_run:
        print(DRY_RUN)
        return 0

    changed = bump_version.apply(new)
    (ROOT / CHANGELOG).write_text(rewritten, encoding="utf-8", newline="")
    print(
        f"Updated: {', '.join([*changed, CHANGELOG])}" if changed else f"Updated: {CHANGELOG}",
        flush=True,
    )

    # The check CI runs, on the state that is about to be committed.
    gate(new)
    print()

    unexpected = [name for name in dirty_files() if name not in ALLOWED]
    if unexpected:
        raise SystemExit(
            "error: the working tree changed in ways a release is not allowed to:\n"
            + "".join(f"       {name}\n" for name in unexpected)
            + "       Nothing has been committed; inspect the diff and try again."
        )

    commit_release(new, f"{release_subject(new)}\n\n{entries}\n")

    git("tag", "-a", tag, "-m", f"ExcelScry {new}")
    print()
    print(f"Committed {short()} and tagged {tag}")
    print()
    # Read after the commit so this is what the release job will compute from the
    # tagged commit, down to the release commit it lists first.
    print("What the release page will say:")
    print()
    print(release_notes.notes(new), end="")

    push(remote, f"{local}:{tracked}")
    push(remote, tag)

    print()
    print(f"Pushed {tracked} and {tag}.")
    job_notes(remote, tag)
    return 0


def resume(args) -> int:
    """Finish a release that stopped after its version was written.

    The version is written to four files and the changelog moved before there is
    anything to commit, so a run that dies anywhere in that sequence - a failed
    commit, a rejected tag push, a machine that went away mid-push - leaves a
    repository that no longer reads as ready to release (the version is not
    ahead of itself any more) but is not released either. This picks up exactly
    where that run stopped: it commits what is uncommitted, makes the tag again
    if there is none, and pushes what the remote does not have. Every step is
    reported before it happens, and what has already been done is left alone.

    Nothing here guesses at intent. A tag that exists and points elsewhere, a
    change the release did not make, or a HEAD that is no release commit are all
    refused rather than resolved.
    """
    git("fetch", "--quiet")
    remote, tracked, local = upstream()
    if behind(remote, tracked):
        raise SystemExit(
            f"error: {remote}/{tracked} has commits this branch does not. Pull them first."
        )

    # The version is what a release writes first, so the one the repository is
    # on is the one an interrupted run was making.
    version = bump_version.version()
    if args.version and args.version != version:
        raise SystemExit(
            f"error: there is no interrupted release for {args.version}: "
            f"{bump_version.CARGO_TOML}\n"
            f"       is on {version}, and a release writes its version there before it\n"
            f"       writes anything else."
        )

    tag = f"v{version}"
    tagged = local_tag(tag)
    published = remote_tag(remote, tag)
    if tagged and published and tagged[0] != published:
        raise SystemExit(
            f"error: {tag} on {remote} ({published[:9]}) is not the tag held here\n"
            f"       ({tagged[0][:9]}). Nothing here moves a tag that already exists: check\n"
            f"       which of the two is the release you meant first."
        )

    dirty = dirty_files()
    unexpected = [name for name in dirty if name not in ALLOWED]
    release_head = head_subject() == release_subject(version)
    # A release tail is the version and the changelog written and not yet
    # committed; on top of the release commit it is somebody's own change, which
    # this will not fold into a release.
    needs_commit = bool(dirty) and not unexpected and not release_head
    needs_tag = tagged is None
    needs_branch = needs_commit or missing_from(remote, tracked, tagged[1] if tagged else "HEAD")
    needs_tag_push = published is None
    destination = f"{remote} {local}" if local == tracked else f"{remote} {local}:{tracked}"

    # Asked before anything is refused: a release that is already out is worth
    # saying so about, whatever else the tree has going on in it.
    if not (needs_commit or needs_tag or needs_branch or needs_tag_push):
        print(f"Nothing to finish: ExcelScry {version} is tagged locally and on {remote},")
        print(f"and {remote}/{tracked} has the release commit.")
        print()
        job_notes(remote, tag)
        return 0

    if unexpected:
        raise SystemExit(
            "error: the working tree has changes a release did not make:\n"
            + "".join(f"       {name}\n" for name in unexpected)
            + "       Commit or drop them, then finish the release."
        )
    if dirty and release_head:
        raise SystemExit(
            f"error: {release_subject(version)} is already committed, and the tree has\n"
            "       changes on top of it:\n"
            + "".join(f"       {name}\n" for name in dirty)
            + "       A release commit carries the version and nothing else."
        )
    if dirty and not has_section(version):
        raise SystemExit(
            f"error: {CHANGELOG} has no `## [{version}]` section, so the version was\n"
            f"       written but the changelog was never moved. Put the Unreleased\n"
            f"       entries under that heading, or drop the change and cut the release\n"
            f"       from the start with `python scripts/release.py <version>`."
        )
    if not (dirty or tagged or release_head):
        raise SystemExit(
            f"error: nothing here looks interrupted: {tag} does not exist, and HEAD is\n"
            f"       not a release commit for {version}:\n"
            f"         {head_subject()}\n"
            f"       --resume finishes a release that stopped part-way. To cut one, run\n"
            f"       this script again with the version to release."
        )

    # The tag goes on the commit this run will make, when it still has to make
    # it, and otherwise on the commit the tag already names.
    where = "the release commit" if needs_commit else short(tagged[1] if tagged else "HEAD")
    print(f"Finish ExcelScry {version}, interrupted after its version was written")
    print()
    if needs_commit:
        print(f"  commit       the version and {CHANGELOG} are written but not committed")
    else:
        print(f"  commit       already made: {short()} is {release_subject(version)}")
    if needs_tag:
        print(f"  tag          {tag} is missing, and will be made again on {where}")
    else:
        print(f"  tag          {tag} already exists, on {where}")
    print(
        f"  push         {destination} - "
        + ("the release commit is not there yet" if needs_branch else "already in step")
    )
    print(
        f"  push         {remote} {tag} - "
        + ("not there yet" if needs_tag_push else "already there")
    )
    print()

    if args.dry_run:
        print(DRY_RUN)
        return 0

    if needs_commit:
        # Re-asserting the version is what makes a run killed between its own
        # writes recoverable: any copy still behind is written here.
        changed = bump_version.apply(version)
        if changed:
            print(f"Updated: {', '.join(changed)}", flush=True)
        gate(version)
        stray = [name for name in dirty_files() if name not in ALLOWED]
        if stray:
            raise SystemExit(
                "error: the working tree changed in ways a release is not allowed to:\n"
                + "".join(f"       {name}\n" for name in stray)
                + "       Nothing has been committed; inspect the diff and finish again."
            )
        commit_release(version, f"{release_subject(version)}\n\n{section_body(version)}\n")
        print(f"Committed {short()} as {release_subject(version)}")
    else:
        # The same check CI runs, on the commit the tag is about to point at.
        gate(version)

    if needs_tag:
        git("tag", "-a", tag, "-m", f"ExcelScry {version}")
        print(f"Made {tag} on {short()}")
    if needs_branch:
        push(remote, f"{local}:{tracked}")
    if needs_tag_push:
        push(remote, tag)

    done = ["committed"] if needs_commit else []
    done += [f"made {tag}"] if needs_tag else []
    done += [f"pushed {tracked}"] if needs_branch else []
    done += [f"pushed {tag}"] if needs_tag_push else []
    print()
    print(f"Finished ExcelScry {version}: {', '.join(done)}.")
    print()
    job_notes(remote, tag)
    return 0


def main() -> int:
    """Cut a release, or finish the one a previous run left half-made."""
    parser = argparse.ArgumentParser(
        description="Bump, move the changelog, commit, tag and push a release, "
        "or finish one that was interrupted."
    )
    parser.add_argument(
        "version", nargs="?", help="the version to release, e.g. 0.1.2 (not with --resume)"
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="finish a release that stopped after its version was written",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="report what would happen and change nothing"
    )
    args = parser.parse_args()

    if args.resume:
        return resume(args)
    if not args.version:
        parser.error("give the version to release, or --resume to finish an interrupted one")
    return release(args)


if __name__ == "__main__":
    raise SystemExit(main())
