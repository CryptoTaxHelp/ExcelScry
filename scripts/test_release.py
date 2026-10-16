#!/usr/bin/env python3
"""Exercise `scripts/release.py` on a repository made for the purpose.

`release.py` is the one script here that cannot be tried on this repository:
running it *is* cutting a release, of this project. So it runs against a
throwaway checkout instead - the real `src-tauri/` and `scripts/` copied in, a
changelog with something in `Unreleased`, and a bare remote to push to - and
everything the release flow is supposed to refuse, the release it is supposed to
make, and the runs it has to be able to finish after they failed part-way, are
checked there.

The scripts are copied rather than reimplemented, so what runs is the code in
this checkout: a test that wrote its own release flow would go on passing while
the real one was broken. Every case gets a repository of its own, because a
failed case usually leaves one half-written.

Nothing here touches this checkout. The last thing the test does is compare this
repository's HEAD and `git status` with what they were before it started.

    python scripts/test_release.py

Exit status 1 means the release flow does not do what it says it does.
"""

from __future__ import annotations

import datetime
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))

import bump_version  # noqa: E402  (the path above is what makes this importable)

# Read from this checkout rather than written down, so the test keeps working
# across a release: the fixture stands at the current version, and the version it
# releases is one ahead of it.
CURRENT = bump_version.version()
_major, _minor, _patch = (int(part) for part in CURRENT.split(".")[:3])
NEXT = f"{_major}.{_minor + 1}.0"
TODAY = datetime.date.today().isoformat()

# What a release is allowed to touch, which is what `release.py` commits.
CHANGED = sorted(
    (
        "CHANGELOG.md",
        "README.md",
        "src-tauri/Cargo.lock",
        "src-tauri/Cargo.toml",
        "src-tauri/tauri.conf.json",
    )
)

# A remote that takes the branch and refuses tags, which is the state a release
# is left in when its tag does not reach the remote.
REFUSES_TAGS = (
    "#!/bin/sh\n"
    "status=0\n"
    "while read -r _old _new ref; do\n"
    '  case "$ref" in\n'
    '    refs/tags/*) echo "tags are refused here" >&2; status=1 ;;\n'
    "  esac\n"
    "done\n"
    "exit $status\n"
)

CHANGELOG = f"""# Changelog

All notable changes to ExcelScry are documented in this file.

## [Unreleased]

### Added

- An entry that has not been released yet.

### Fixed

- A second entry, so that more than one moves at a time.

## [{CURRENT}] - 2026-01-01

### Added

- What the release before this one carried.
"""

CASES: list[tuple[str, ...]] = []
FAILURES: list[str] = []
CASE = ""


def check(label: str, condition: bool, detail: str = "") -> None:
    """One assertion, printed as it is made."""
    if condition:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label}{f'  ({detail})' if detail else ''}")
        FAILURES.append(f"{CASE}: {label}")


def git(*args: str, cwd: pathlib.Path) -> str:
    """Run git, or fail loudly: a broken fixture is not a passing test."""
    result = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8"
    )
    if result.returncode != 0:
        raise SystemExit(
            f"test setup failed: git {' '.join(args)} in {cwd}\n{result.stderr.strip()}"
        )
    return result.stdout.strip()


def remove_tree(path: pathlib.Path) -> None:
    """Delete a repository, including the read-only files git keeps in one."""

    def force(function, target, _exception):
        os.chmod(target, stat.S_IWRITE)
        function(target)

    shutil.rmtree(path, onerror=force)


class Repository:
    """A throwaway checkout of this project, with a remote to push to."""

    def __init__(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="excelscry-release-test-"))
        self.bare = self.root / "origin.git"
        self.work = self.root / "work"

        git("init", "--bare", "--quiet", str(self.bare), cwd=self.root)
        git("init", "--quiet", str(self.work), cwd=self.root)
        self.configure(self.work, "ExcelScry test", "test@example.invalid")

        shutil.copytree(
            ROOT / "src-tauri",
            self.work / "src-tauri",
            # Build output, generated schemas and icons: nothing the release flow
            # reads, and copying a `target/` would take minutes.
            ignore=shutil.ignore_patterns("target", "gen", "icons"),
        )
        shutil.copytree(
            ROOT / "scripts",
            self.work / "scripts",
            ignore=shutil.ignore_patterns("__pycache__"),
        )
        for name in ("README.md", ".gitignore", ".gitattributes"):
            shutil.copy2(ROOT / name, self.work / name)
        self.write("CHANGELOG.md", CHANGELOG)

        git("add", "--all", cwd=self.work)
        git("commit", "--quiet", "--message", f"ExcelScry {CURRENT}", cwd=self.work)
        # A previous tag, so the release notes have a range to describe.
        git(
            "tag",
            "--annotate",
            f"v{CURRENT}",
            "--message",
            f"ExcelScry {CURRENT}",
            cwd=self.work,
        )

        self.branch = git("rev-parse", "--abbrev-ref", "HEAD", cwd=self.work)
        git("remote", "add", "origin", str(self.bare), cwd=self.work)
        git("push", "--quiet", "--set-upstream", "origin", self.branch, cwd=self.work)
        git("push", "--quiet", "origin", "--tags", cwd=self.work)

    @staticmethod
    def configure(where: pathlib.Path, name: str, email: str) -> None:
        """Identity and policies of the machine running this must not leak in."""
        for key, value in (
            ("user.name", name),
            ("user.email", email),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ):
            git("config", key, value, cwd=where)

    # -- the state of the repository ---------------------------------------

    def head(self) -> str:
        return git("rev-parse", "HEAD", cwd=self.work)

    def status(self) -> str:
        return git("status", "--porcelain", cwd=self.work)

    def tags(self) -> str:
        return git("tag", "--list", cwd=self.work)

    def read(self, relative: str) -> str:
        return (self.work / relative).read_text(encoding="utf-8")

    def write(self, relative: str, text: str) -> None:
        (self.work / relative).write_text(text, encoding="utf-8", newline="")

    def commit_all(self, message: str, push: bool = True) -> None:
        """Commit everything. Pushed by default, so that a case about the
        changelog is refused for the changelog and not for a branch ahead of its
        remote, which is checked first."""
        git("add", "--all", cwd=self.work)
        git("commit", "--quiet", "--message", message, cwd=self.work)
        if push:
            git("push", "--quiet", cwd=self.work)

    # -- running the thing under test --------------------------------------

    def run_release(self, *args: str):
        """Run the copy of `release.py` inside the fixture, as a person would."""
        return subprocess.run(
            [sys.executable, str(self.work / "scripts" / "release.py"), *args],
            cwd=self.work,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def release(self, version: str, *extra: str):
        """Cut a release of `version`."""
        return self.run_release(version, *extra)

    def resume(self, *extra: str):
        """Finish the release a run before this one left behind."""
        return self.run_release("--resume", *extra)

    def hook(self, name: str, script: str) -> pathlib.Path:
        """Install a git hook, to fail the step it guards."""
        where = self.work / ".git" / "hooks" / name
        where.write_text(script, encoding="utf-8", newline="\n")
        where.chmod(0o755)
        return where

    def check_versions(self, tag: str | None = None):
        """The check CI runs, in the fixture - as a release, if given a tag."""
        return subprocess.run(
            [
                sys.executable,
                str(self.work / "scripts" / "bump_version.py"),
                "--check",
                *(["--tag", tag] if tag else []),
            ],
            cwd=self.work,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def close(self) -> None:
        remove_tree(self.root)


def output(result) -> str:
    return result.stdout + result.stderr


def case(label: str):
    """Name one case; it runs in `main`, in a repository of its own."""

    def decorate(function):
        CASES.append((label, function))
        return function

    return decorate


def refused(result, says: str, where: str = "") -> None:
    """The run has to fail, and say why in its own words."""
    what = where or says
    check(f"refused ({what}) with an exit status", result.returncode != 0, f"exit {result.returncode}")
    check(f"and said {says!r}", says in output(result), output(result).strip()[:160])


def run_case(label: str, function) -> None:
    global CASE
    CASE = label
    print()
    print(label)
    repository = Repository()
    try:
        function(repository)
    finally:
        repository.close()


# ---------------------------------------------------------------------------


@case("the repository it is asked to release")
def the_fixture_is_a_repository_that_could_be_released(repository: Repository) -> None:
    """If the fixture is wrong, every case below would fail for the wrong reason."""
    checked = repository.check_versions()
    check("the version copies agree", checked.returncode == 0, output(checked)[-200:])
    check(
        "and no tag is claimed yet",
        repository.check_versions(f"v{NEXT}").returncode != 0,
    )
    check("the tree is clean", repository.status() == "", repository.status())
    check(
        "the remote has the branch",
        git("rev-parse", repository.branch, cwd=repository.bare) == repository.head(),
    )
    check("the remote has the previous tag", f"v{CURRENT}" in git("tag", "--list", cwd=repository.bare))


@case("a full release")
def a_full_release_does_everything_it_planned(repository: Repository) -> None:
    before = repository.head()
    result = repository.release(NEXT)
    text = output(result)
    check("exit status 0", result.returncode == 0, text[-400:])

    # The plan it printed is what it then did.
    for line in (
        f"Release ExcelScry {NEXT} (currently {CURRENT})",
        f"changelog    CHANGELOG.md: the Unreleased entries move under ## [{NEXT}] - {TODAY}",
        f"commit       Release ExcelScry {NEXT}",
        f"tag          v{NEXT}",
        f"push         origin {repository.branch} and v{NEXT}",
    ):
        check(f"planned {line.split()[0]!r}", line in text)

    # The four copies of the version, checked the way CI checks them.
    checked = repository.check_versions(f"v{NEXT}")
    check("the version copies say the new version", checked.returncode == 0, output(checked)[-300:])
    check(
        "and the source of truth is the one that was written",
        f'version = "{NEXT}"' in repository.read(bump_version.CARGO_TOML),
    )

    # The changelog: the entries moved under a dated heading, leaving Unreleased
    # in place and empty, and the previous section untouched.
    changelog = repository.read("CHANGELOG.md")
    check("the new section is dated", f"## [{NEXT}] - {TODAY}" in changelog)
    moved = changelog.split(f"## [{NEXT}]", 1)[1].split(f"## [{CURRENT}]", 1)[0]
    check("the first Unreleased entry moved into it", "An entry that has not been released yet." in moved)
    check("and so did the second", "A second entry, so that more than one moves at a time." in moved)
    between = changelog.split("## [Unreleased]", 1)[1].split("## [", 1)[0]
    check("Unreleased is still there, and now empty", between.strip() == "", between.strip()[:60])
    check("the previous release is untouched", f"## [{CURRENT}] - 2026-01-01" in changelog)
    check("and the new section appears once", changelog.count(f"## [{NEXT}]") == 1)

    # The commit: exactly one, whose subject is the release, carrying five files.
    count = git("rev-list", "--count", f"{before}..HEAD", cwd=repository.work)
    check("the branch has one new commit", count == "1", count)
    subject = git("log", "-1", "--pretty=%s", cwd=repository.work)
    check("whose subject names the release", subject == f"Release ExcelScry {NEXT}", subject)
    body = git("log", "-1", "--pretty=%b", cwd=repository.work)
    check("and whose body carries the entries", "An entry that has not been released yet." in body)
    changed = sorted(
        git("show", "--name-only", "--pretty=format:", "HEAD", cwd=repository.work).split()
    )
    check("and only the files a release may change", changed == CHANGED, str(changed))

    # The tag: annotated, on the release commit, named for the version.
    check("the tag exists and is annotated", git("cat-file", "-t", f"v{NEXT}", cwd=repository.work) == "tag")
    check(
        "and points at the release commit",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.work) == repository.head(),
    )
    message = git("tag", "--list", "--format=%(contents:subject)", f"v{NEXT}", cwd=repository.work)
    check("with a message naming the version", message == f"ExcelScry {NEXT}", message)

    # The push: the branch and the tag are on the remote, and the checkout is
    # clean afterwards, because a release commit that left something behind would
    # make the next run refuse.
    check(
        "the remote has the release commit",
        git("rev-parse", repository.branch, cwd=repository.bare) == repository.head(),
    )
    check(
        "the remote has the tag",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.bare) == repository.head(),
    )
    check("the tree is clean afterwards", repository.status() == "", repository.status())

    # The notes it showed, which are what the release page will carry.
    check("the release page was previewed", "What the release page will say:" in text)
    check("with the new version's section", f"## [{NEXT}]" in text)
    check("and the commits since the previous tag", f"### Commits since v{CURRENT}" in text)
    check("listing the release commit first", f"- Release ExcelScry {NEXT}" in text)
    check("it said what the tag starts", f"The tag v{NEXT} starts the release job:" in text)
    check("and what the job needs to sign with", "TAURI_SIGNING_PRIVATE_KEY" in text)


@case("--dry-run")
def a_dry_run_changes_nothing(repository: Repository) -> None:
    changelog = repository.read("CHANGELOG.md")
    cargo = repository.read(bump_version.CARGO_TOML)
    before = repository.head()
    result = repository.release(NEXT, "--dry-run")
    check("exit status 0", result.returncode == 0, output(result)[-200:])
    check("it said nothing was written", "--dry-run: nothing was written" in output(result))
    check("no commit was made", repository.head() == before)
    check("no tag was created", repository.tags() == f"v{CURRENT}", repository.tags())
    check("the changelog is untouched", repository.read("CHANGELOG.md") == changelog)
    check("the version is untouched", repository.read(bump_version.CARGO_TOML) == cargo)
    check("the tree is still clean", repository.status() == "", repository.status())


@case("an uncommitted change")
def an_uncommitted_change_is_refused(repository: Repository) -> None:
    before = repository.head()
    repository.write("notes.txt", "half-finished work\n")
    refused(repository.release(NEXT), "the working tree has uncommitted changes")
    check("nothing was committed", repository.head() == before)
    check("nothing was tagged", repository.tags() == f"v{CURRENT}")
    check("and the version was not written", NEXT not in repository.read(bump_version.CARGO_TOML))


@case("a branch that is ahead of its remote")
def a_branch_ahead_of_its_remote_is_refused(repository: Repository) -> None:
    after = repository.head()
    repository.write("notes.txt", "a change nobody else has\n")
    repository.commit_all("Work that has not been pushed", push=False)
    refused(repository.release(NEXT), f"commits that are not on origin/{repository.branch} yet")
    check("nothing was tagged", repository.tags() == f"v{CURRENT}")
    check("and the tip is still that commit", repository.head() != after)


@case("a branch that is behind its remote")
def a_branch_behind_its_remote_is_refused(repository: Repository) -> None:
    # Someone else's commit, made the way they would make it: a clone, a
    # commit, a push.
    other = repository.root / "someone-else"
    git("clone", "--quiet", str(repository.bare), str(other), cwd=repository.root)
    repository.configure(other, "Someone Else", "other@example.invalid")
    (other / "their-work.txt").write_text("a commit from someone else\n", encoding="utf-8")
    git("add", "--all", cwd=other)
    git("commit", "--quiet", "--message", "Their work", cwd=other)
    git("push", "--quiet", cwd=other)

    before = repository.head()
    refused(repository.release(NEXT), "has commits this branch does not. Pull them first")
    check("nothing was committed", repository.head() == before)
    check("nothing was tagged", repository.tags() == f"v{CURRENT}")


@case("a version that is not ahead")
def a_version_that_is_not_ahead_is_refused(repository: Repository) -> None:
    before = repository.head()
    for version, says in (
        (CURRENT, "is not ahead of the current version"),
        ("not-a-version", "is not a semantic version"),
        ("1.0", "is not a semantic version"),
    ):
        refused(repository.release(version), says, f"release {version}")
    check("nothing was committed", repository.head() == before)
    check("nothing was tagged", repository.tags() == f"v{CURRENT}")
    check("and no version was written", NEXT not in repository.read(bump_version.CARGO_TOML))


@case("a tag that already exists")
def a_tag_that_already_exists_is_refused(repository: Repository) -> None:
    git("tag", "--annotate", f"v{NEXT}", "--message", "already released", cwd=repository.work)
    before = repository.head()
    refused(repository.release(NEXT), f"tag v{NEXT} already exists locally")
    check("nothing was committed", repository.head() == before)
    check("and the version was not written", NEXT not in repository.read(bump_version.CARGO_TOML))


@case("an empty Unreleased section")
def an_empty_unreleased_section_is_refused(repository: Repository) -> None:
    empty = (
        CHANGELOG[: CHANGELOG.index("## [Unreleased]")]
        + "## [Unreleased]\n\n"
        + CHANGELOG[CHANGELOG.index(f"## [{CURRENT}]") :]
    )
    repository.write("CHANGELOG.md", empty)
    repository.commit_all("A changelog with nothing unreleased in it")
    before = repository.head()
    refused(repository.release(NEXT), "is empty, so there is")
    check("nothing was committed", repository.head() == before)
    check("nothing was tagged", repository.tags() == f"v{CURRENT}")


@case("a changelog that already names the version")
def a_changelog_that_already_names_the_version_is_refused(repository: Repository) -> None:
    text = repository.read("CHANGELOG.md")
    repository.write(
        "CHANGELOG.md",
        text.replace("## [Unreleased]", f"## [Unreleased]\n\n## [{NEXT}] - 2026-01-01", 1),
    )
    repository.commit_all("A changelog that already has this version")
    refused(repository.release(NEXT), f"already has a `## [{NEXT}]` section")


@case("a tag the remote refuses, and the --resume that finishes it")
def a_tag_that_cannot_be_pushed_says_how_to_finish(repository: Repository) -> None:
    # A hook that takes the branch and rejects tags, which is the state a
    # half-finished release leaves: the commit is on the remote, the tag is not.
    hook = repository.bare / "hooks" / "pre-receive"
    hook.write_text(REFUSES_TAGS, encoding="utf-8", newline="\n")
    hook.chmod(0o755)

    before = repository.head()
    result = repository.release(NEXT)
    text = output(result)
    # The push of the branch happens before the tag, so this is the failure that
    # leaves a release half-made - and the message has to say how to finish it.
    check("it did not pretend to have finished", result.returncode != 0, f"exit {result.returncode}")
    check(
        "it said how the release is finished",
        "finish the release where it stopped" in text,
        text[-240:],
    )
    check("naming --resume", "scripts/release.py --resume" in text, text[-240:])
    subject = git("log", "-1", "--pretty=%s", cwd=repository.work)
    check("the release commit was made", subject == f"Release ExcelScry {NEXT}", subject)
    check("the tag exists locally", f"v{NEXT}" in repository.tags())
    check(
        "the branch was pushed",
        git("rev-parse", repository.branch, cwd=repository.bare) == repository.head(),
    )
    check("and the tag did not reach the remote", git("tag", "--list", cwd=repository.bare) == f"v{CURRENT}")

    # What a resume would do, without doing any of it.
    result = repository.resume("--dry-run")
    text = output(result)
    check("--resume --dry-run exits 0", result.returncode == 0, text[-300:])
    check("it planned the tag's push only", f"push         origin v{NEXT} - not there yet" in text, text)
    check("and left the tag where it is", f"tag          v{NEXT} already exists" in text, text)
    check("and said nothing would be pushed", "--dry-run: nothing was written" in text)
    check(
        "nothing was tagged or pushed",
        set(repository.tags().split()) == {f"v{CURRENT}", f"v{NEXT}"},
        repository.tags(),
    )
    check("and the remote is unchanged", git("tag", "--list", cwd=repository.bare) == f"v{CURRENT}")

    # With the hook gone, --resume is the way out the failure pointed at.
    hook.unlink()
    result = repository.resume()
    text = output(result)
    check("--resume finished it", result.returncode == 0, text[-400:])
    check("saying the commit was already made", "already made" in text, text[:400])
    check("and that the tag above it stays put", f"tag          v{NEXT} already exists" in text, text[:400])
    check("with the push that never landed", f"push         origin v{NEXT} - not there yet" in text, text[:400])
    check(
        "without a second commit",
        git("rev-list", "--count", f"{before}..HEAD", cwd=repository.work) == "1",
    )
    check(
        "the tag is on the release commit",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.work) == repository.head(),
    )
    check(
        "the remote has it now",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.bare) == repository.head(),
    )
    check("and the tree is clean", repository.status() == "", repository.status())

    # Someone else pushes while the release is being finished: that is refused,
    # not pushed over, even though the release itself is already complete.
    other = repository.root / "someone-else"
    git("clone", "--quiet", str(repository.bare), str(other), cwd=repository.root)
    repository.configure(other, "Someone Else", "other@example.invalid")
    (other / "their-work.txt").write_text("a commit from someone else\n", encoding="utf-8")
    git("add", "--all", cwd=other)
    git("commit", "--quiet", "--message", "Their work", cwd=other)
    git("push", "--quiet", cwd=other)
    refused(repository.resume(), "has commits this branch does not. Pull them first")


@case("a release commit with a hand edit on top of it")
def resume_refuses_a_change_on_top_of_the_release_commit(repository: Repository) -> None:
    # A run that got as far as its commit and its tag and then failed to push
    # the tag, and then a hand edit to one of the files that release carries -
    # which is somebody's change, not part of the release, and must not be folded
    # into it by a resume.
    hook = repository.bare / "hooks" / "pre-receive"
    hook.write_text(REFUSES_TAGS, encoding="utf-8", newline="\n")
    hook.chmod(0o755)
    failed = repository.release(NEXT)
    check("the release got as far as its commit", failed.returncode != 0, f"exit {failed.returncode}")
    head = repository.head()

    repository.write(
        "CHANGELOG.md",
        repository.read("CHANGELOG.md") + "\nA line somebody added by hand.\n",
    )
    refused(
        repository.resume(),
        f"Release ExcelScry {NEXT} is already committed, and the tree has",
    )
    check("nothing was committed on top of it", repository.head() == head)
    check(
        "nothing was tagged again",
        set(repository.tags().split()) == {f"v{CURRENT}", f"v{NEXT}"},
        repository.tags(),
    )
    check("and the tag was not pushed", git("tag", "--list", cwd=repository.bare) == f"v{CURRENT}")
    check("the edit is still there", "A line somebody added by hand." in repository.read("CHANGELOG.md"))


@case("a commit that failed, and the --resume that finishes it")
def a_commit_that_failed_is_finished_by_resume(repository: Repository) -> None:
    # The interruption a release really has: the version and the changelog are
    # written, and the commit that would carry them never happens.
    hook = repository.hook("pre-commit", "#!/bin/sh\necho 'no commits today' >&2\nexit 1\n")
    before = repository.head()

    result = repository.release(NEXT)
    text = output(result)
    check("the release failed at the commit", result.returncode != 0, f"exit {result.returncode}")
    check("saying where it stopped", "the changes are staged but not committed" in text, text[-300:])
    check("and how to finish", "scripts/release.py --resume" in text, text[-300:])
    check("the version was written", f'version = "{NEXT}"' in repository.read(bump_version.CARGO_TOML))
    check("the changelog was moved", f"## [{NEXT}] - {TODAY}" in repository.read("CHANGELOG.md"))
    check("no commit was made", repository.head() == before)
    check("nothing was tagged", repository.tags() == f"v{CURRENT}", repository.tags())
    check(
        "and nothing was pushed",
        git("rev-parse", repository.branch, cwd=repository.bare) == before,
    )
    check("so the tree is not clean", repository.status() != "")

    hook.unlink()
    result = repository.resume()
    text = output(result)
    check("--resume finished it", result.returncode == 0, text[-400:])
    check("planning the commit it stopped at", "written but not committed" in text, text[:400])
    check(
        "it made one commit",
        git("rev-list", "--count", f"{before}..HEAD", cwd=repository.work) == "1",
    )
    subject = git("log", "-1", "--pretty=%s", cwd=repository.work)
    check("the release commit", subject == f"Release ExcelScry {NEXT}", subject)
    body = git("log", "-1", "--pretty=%b", cwd=repository.work)
    check("carrying the entries the release moved", "An entry that has not been released yet." in body, body)
    check("with no heading of its own", "## [" not in body, body)
    changed = sorted(
        git("show", "--name-only", "--pretty=format:", "HEAD", cwd=repository.work).split()
    )
    check("and only the files a release may change", changed == CHANGED, str(changed))
    # The changelog was already moved by the run that died: moving it twice
    # would put the same entries in a second section.
    changelog = repository.read("CHANGELOG.md")
    check("the new section appears once", changelog.count(f"## [{NEXT}]") == 1, changelog[:200])
    between = changelog.split("## [Unreleased]", 1)[1].split("## [", 1)[0]
    check("and Unreleased is empty", between.strip() == "", between.strip()[:60])
    check("the previous release is untouched", f"## [{CURRENT}] - 2026-01-01" in changelog)
    checked = repository.check_versions(f"v{NEXT}")
    check("the version copies agree", checked.returncode == 0, output(checked)[-200:])
    check(
        "the tag is on the release commit",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.work) == repository.head(),
    )
    check(
        "the remote has the release commit",
        git("rev-parse", repository.branch, cwd=repository.bare) == repository.head(),
    )
    check(
        "and the tag",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.bare) == repository.head(),
    )
    check("and the tree is clean", repository.status() == "", repository.status())


@case("a version that was only half written")
def resume_writes_the_copies_a_killed_run_left_behind(repository: Repository) -> None:
    # A run killed between its own writes: Cargo.toml is written first, so the
    # file that names the installer can still be behind it. The heading a
    # release writes into the changelog is put here by hand, because that is the
    # other half of what such a run has already done.
    started = subprocess.run(
        [sys.executable, str(repository.work / "scripts" / "bump_version.py"), NEXT],
        cwd=repository.work,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    check("the first half of the bump ran", started.returncode == 0, output(started)[-200:])
    repository.write(
        bump_version.TAURI_CONF,
        repository.read(bump_version.TAURI_CONF).replace(f'"{NEXT}"', f'"{CURRENT}"', 1),
    )
    repository.write(
        "CHANGELOG.md",
        repository.read("CHANGELOG.md").replace(
            "## [Unreleased]", f"## [Unreleased]\n\n## [{NEXT}] - {TODAY}", 1
        ),
    )
    check("one copy is behind", f'"{CURRENT}"' in repository.read(bump_version.TAURI_CONF))

    result = repository.resume()
    text = output(result)
    check("--resume finished it", result.returncode == 0, text[-400:])
    check("writing the copy that was behind", f'"{NEXT}"' in repository.read(bump_version.TAURI_CONF))
    checked = repository.check_versions(f"v{NEXT}")
    check("every copy agrees now", checked.returncode == 0, output(checked)[-200:])
    subject = git("log", "-1", "--pretty=%s", cwd=repository.work)
    check("the release commit was made", subject == f"Release ExcelScry {NEXT}", subject)
    check(
        "the tag is on it",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.work) == repository.head(),
    )
    check(
        "and the remote has it",
        git("rev-parse", f"v{NEXT}^{{commit}}", cwd=repository.bare) == repository.head(),
    )
    check("and the tree is clean", repository.status() == "", repository.status())


@case("a release that is already out, asked to --resume")
def resume_on_a_release_that_is_out_changes_nothing(repository: Repository) -> None:
    released = repository.release(NEXT)
    check("the release went through", released.returncode == 0, output(released)[-300:])
    head, tags = repository.head(), repository.tags()
    remote_head = git("rev-parse", repository.branch, cwd=repository.bare)

    # An edit to one of the files a release carries, on top of the release
    # commit, is still somebody's change: the release is as finished as it was,
    # and there is nothing here to commit into it.
    changelog = repository.read("CHANGELOG.md")
    repository.write("CHANGELOG.md", changelog + "\nA line added after the release.\n")

    result = repository.resume()
    text = output(result)
    check("--resume exits 0", result.returncode == 0, text[-300:])
    check("saying there is nothing to finish", f"Nothing to finish: ExcelScry {NEXT}" in text, text[:200])
    check("and pointing at the run the tag started", f"The tag v{NEXT} starts the release job:" in text)
    check("nothing was committed", repository.head() == head)
    check("no tag was made again", repository.tags() == tags, repository.tags())
    check("nothing was pushed", git("rev-parse", repository.branch, cwd=repository.bare) == remote_head)
    check(
        "and the edit was left alone",
        repository.read("CHANGELOG.md") == changelog + "\nA line added after the release.\n",
    )

    # Work in the tree that a release has nothing to do with changes nothing
    # about that either.
    repository.write("notes.txt", "unrelated work in progress\n")
    result = repository.resume()
    check(
        "--resume still says there is nothing to finish",
        result.returncode == 0 and f"Nothing to finish: ExcelScry {NEXT}" in output(result),
        output(result)[:200],
    )
    check("and still committed nothing", repository.head() == head)
    check("with the notes still there", repository.read("notes.txt") == "unrelated work in progress\n")


@case("--resume over something there is nothing to finish in")
def resume_refuses_what_it_cannot_finish(repository: Repository) -> None:
    # A version no run has written anywhere.
    refused(repository.resume("9.9.9"), "there is no interrupted release for 9.9.9")

    # A repository with no release in it: the tag is what makes this version
    # released, so take it away, from here and from the remote.
    git("push", "--quiet", "--delete", "origin", f"v{CURRENT}", cwd=repository.work)
    git("tag", "--delete", f"v{CURRENT}", cwd=repository.work)
    before = repository.head()
    refused(repository.resume(), "nothing here looks interrupted")
    check("nothing was committed", repository.head() == before)
    check("and no tag was made", repository.tags() == "", repository.tags())

    # A change of the user's own is not something a release folds in.
    repository.write("notes.txt", "unrelated work in progress\n")
    refused(repository.resume(), "the working tree has changes a release did not make")
    check("and it is still there", repository.read("notes.txt") == "unrelated work in progress\n")


@case("a tag of this version that exists elsewhere as another tag")
def resume_refuses_a_tag_that_is_not_its_own(repository: Repository) -> None:
    # One version tagged twice with two different tag objects, which is what a
    # release looks like when the same tag was made somewhere else as well: both
    # name the same commit, and only one of them is the tag made here.
    other = repository.root / "someone-else"
    git("clone", "--quiet", str(repository.bare), str(other), cwd=repository.root)
    repository.configure(other, "Someone Else", "other@example.invalid")
    # The clone brought the tag with it, so this is another tag of that name.
    git("tag", "--delete", f"v{CURRENT}", cwd=other)
    git(
        "tag",
        "--annotate",
        f"v{CURRENT}",
        "--message",
        "the same version, tagged elsewhere",
        cwd=other,
    )
    git("push", "--quiet", "--force", "origin", f"v{CURRENT}", cwd=other)

    local = git("rev-parse", f"v{CURRENT}", cwd=repository.work)
    check(
        "the two tags of one version are different objects",
        local != git("rev-parse", f"v{CURRENT}", cwd=repository.bare),
    )
    check("and both name the same commit", git("rev-parse", f"v{CURRENT}^{{commit}}", cwd=repository.work) == repository.head())

    refused(repository.resume(), f"v{CURRENT} on origin")
    check("the tag held here was not moved", git("rev-parse", f"v{CURRENT}", cwd=repository.work) == local)


@case("a version written by hand, with no changelog section for it")
def resume_refuses_a_version_with_no_changelog_to_match(repository: Repository) -> None:
    # `bump_version.py` on its own is how a person bumps a version; the changelog
    # entry a release moves into a dated section is the other half of what an
    # interrupted run has already done, and without it there is nothing to
    # release.
    bumped = subprocess.run(
        [sys.executable, str(repository.work / "scripts" / "bump_version.py"), NEXT],
        cwd=repository.work,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    check("the bump ran", bumped.returncode == 0, output(bumped)[-200:])
    check("with no section for it", f"## [{NEXT}]" not in repository.read("CHANGELOG.md"))

    before = repository.head()
    refused(repository.resume(), f"CHANGELOG.md has no `## [{NEXT}]` section")
    check("nothing was committed", repository.head() == before)
    check("nothing was tagged", f"v{NEXT}" not in repository.tags(), repository.tags())
    check(
        "and the bump is still in the tree, for the person to look at",
        f'version = "{NEXT}"' in repository.read(bump_version.CARGO_TOML),
    )


def main() -> int:
    print("ExcelScry release test")
    print()
    print(f"This checkout is at {CURRENT}; each fixture releases {NEXT}.")
    print(f"Repositories are made under {tempfile.gettempdir()} and removed afterwards.")

    before_head = git("rev-parse", "HEAD", cwd=ROOT)
    before_status = git("status", "--porcelain", cwd=ROOT)

    for label, function in CASES:
        run_case(label, function)

    global CASE
    CASE = "this checkout"
    print()
    print(CASE)
    check("HEAD is where it was", git("rev-parse", "HEAD", cwd=ROOT) == before_head)
    check("and the working tree is unchanged", git("status", "--porcelain", cwd=ROOT) == before_status)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} check(s) failed:")
        for failure in FAILURES:
            print(f"  - {failure}")
        return 1
    print("The release flow refuses what it should, and a release in a throwaway")
    print("repository bumps, moves the changelog, commits, tags and pushes - and a")
    print("release that stopped part-way is finished from where it stopped.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
