#!/usr/bin/env python3
"""Exercise `scripts/manifest_urls.py` on manifests written for the purpose.

What the script gets right is mostly what it refuses: an asset id that is not
attached to the release, an asset from another repository, a URL that is neither
form. A manifest published with any of those names an asset nobody can look up,
and by then it is signed and reachable, so the refusals are checked here rather
than discovered from a release page.

The release API is not called: the names of a release's assets are passed in, the
way the script takes them, so this runs without a token or a published release,
and the release lookup is exercised against a fake list of releases. What the
real API returns is checked by the release job, which rewrites the manifest of
the release it is publishing.

    python scripts/test_manifest_urls.py

Exit status 1 means the manifest cannot be trusted to name its assets.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent

sys.path.insert(0, str(HERE))

import manifest_urls  # noqa: E402  (the path above is what makes this importable)

REPOSITORY = "CryptoTaxHelp/ExcelScry"
TAG = "v0.3.2"
INSTALLER = "ExcelScry_0.3.2_x64-setup.exe"
PORTABLE = "ExcelScry_windows_x64.exe"
ASSETS = {"656365426": INSTALLER, "656365476": PORTABLE}


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  ok    {label}")
        return
    print(f"  FAIL  {label}")
    if detail:
        for line in detail.splitlines():
            print(f"        {line}")
    raise SystemExit(1)


def api_url(asset_id: str, repository: str = REPOSITORY) -> str:
    return f"https://api.github.com/repos/{repository}/releases/assets/{asset_id}"


def manifest(**platforms: str) -> dict:
    return {
        "version": "0.3.2",
        "platforms": {
            platform: {"signature": "c2lnbmF0dXJl", "url": url}
            for platform, url in platforms.items()
        },
    }


def refused(function, says: str, label: str) -> None:
    """`function` stops with a message naming the problem."""
    try:
        function()
    except SystemExit as error:
        check(label, says in str(error), f"said: {error}")
        return
    check(label, False, f"nothing was refused; expected a message containing {says!r}")


def an_asset_id_is_named_by_the_file_the_release_publishes_it_as() -> None:
    document = manifest(**{"windows-x86_64": api_url("656365426")})
    changed = manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS)

    url = document["platforms"]["windows-x86_64"]["url"]
    check(
        "an asset id becomes the release page's file name",
        url == f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{INSTALLER}",
        url,
    )
    check(
        "the change is reported with the file it names",
        len(changed) == 1 and INSTALLER in changed[0],
        repr(changed),
    )


def every_platform_is_named_even_when_they_share_an_asset() -> None:
    document = manifest(
        **{
            "windows-x86_64": api_url("656365426"),
            "windows-x86_64-nsis": api_url("656365426"),
        }
    )
    manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS)

    urls = {entry["url"] for entry in document["platforms"].values()}
    check(
        "both platform keys are rewritten, and agree",
        urls == {f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{INSTALLER}"},
        repr(urls),
    )


def a_draft_release_is_found_by_listing_releases() -> None:
    """The by-tag endpoint hides drafts, so the lookup has to walk the list.

    The release job signs the manifest while the release is a draft. Asking for
    the release by tag answers 404 at that point even with a token, which is
    what made the first 0.4.0 release stop before its manifest was signed.
    """
    first = [
        {"tag_name": f"v0.0.{index}", "draft": False, "assets": []}
        for index in range(100)
    ]
    second = [
        {
            "tag_name": TAG,
            "draft": True,
            "assets": [
                {"id": int(asset_id), "name": name}
                for asset_id, name in ASSETS.items()
            ],
        }
    ]
    listing = f"https://api.github.com/repos/{REPOSITORY}/releases?per_page=100&page="
    pages = {f"{listing}1": first, f"{listing}2": second}
    seen = []
    original = manifest_urls.release_json

    def fake(url: str, headers: dict[str, str]) -> object:
        seen.append(url)
        return pages.get(url, [])

    manifest_urls.release_json = fake
    try:
        names = manifest_urls.release_assets(REPOSITORY, TAG)
    finally:
        manifest_urls.release_json = original

    check(
        "a draft release on a later page is found by listing releases",
        names == ASSETS,
        repr(names),
    )
    check(
        "the lookup does not use the by-tag endpoint",
        seen == [f"{listing}1", f"{listing}2"],
        repr(seen),
    )


def a_tag_with_no_release_is_refused() -> None:
    original = manifest_urls.release_json
    manifest_urls.release_json = lambda url, headers: []
    try:
        refused(
            lambda: manifest_urls.release_assets(REPOSITORY, TAG),
            "no release for tag",
            "a tag with no release is refused",
        )
    finally:
        manifest_urls.release_json = original


def a_manifest_that_already_names_its_assets_is_left_alone() -> None:
    url = f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{INSTALLER}"
    document = manifest(**{"windows-x86_64": url})
    changed = manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS)

    check(
        "a release-page url is kept",
        document["platforms"]["windows-x86_64"]["url"] == url,
    )
    check("and reported as already named", "already named" in changed[0], repr(changed))

    manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS)
    check(
        "running it again changes nothing",
        document["platforms"]["windows-x86_64"]["url"] == url,
    )


def an_asset_id_from_another_release_is_refused() -> None:
    document = manifest(**{"windows-x86_64": api_url("1")})
    refused(
        lambda: manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS),
        "is not",
        "an asset id the release does not list is refused",
    )


def an_asset_from_another_repository_is_refused() -> None:
    document = manifest(**{"windows-x86_64": api_url("656365426", "someone/else")})
    refused(
        lambda: manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS),
        "someone/else",
        "an asset from another repository is refused",
    )


def a_url_that_is_neither_form_is_refused() -> None:
    document = manifest(**{"windows-x86_64": "https://example.com/setup.exe"})
    refused(
        lambda: manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS),
        "neither",
        "a url that names no release asset is refused",
    )


def a_manifest_with_nothing_to_name_is_refused() -> None:
    refused(
        lambda: manifest_urls.rewrite({"version": "0.3.2"}, REPOSITORY, TAG, ASSETS),
        "no platforms",
        "a manifest with no platforms is refused",
    )
    refused(
        lambda: manifest_urls.rewrite(manifest(), REPOSITORY, TAG, ASSETS),
        "no platforms",
        "a manifest whose platforms map is empty is refused",
    )
    document = {"version": "0.3.2", "platforms": {"windows-x86_64": {"signature": "x"}}}
    refused(
        lambda: manifest_urls.rewrite(document, REPOSITORY, TAG, ASSETS),
        "no url",
        "a platform without a url is refused",
    )


def a_release_page_url_is_read_as_the_name_it_carries() -> None:
    url = f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{INSTALLER}"
    check(
        "a release page url names its file",
        manifest_urls.published_name(url, REPOSITORY, TAG) == INSTALLER,
    )
    check(
        "another tag's url does not",
        manifest_urls.published_name(url.replace(TAG, "v9.9.9"), REPOSITORY, TAG) is None,
    )
    check(
        "another repository's url does not",
        manifest_urls.published_name(
            url.replace(REPOSITORY, "someone/else"), REPOSITORY, TAG
        )
        is None,
    )
    check(
        "an api url is not a release page url",
        manifest_urls.published_name(api_url("656365426"), REPOSITORY, TAG) is None,
    )
    check(
        "a path that is not a release is not one either",
        manifest_urls.published_name(
            f"https://github.com/{REPOSITORY}/tree/{TAG}/{INSTALLER}", REPOSITORY, TAG
        )
        is None,
    )
    check(
        "the trailing slash names nothing",
        manifest_urls.published_name(
            f"https://github.com/{REPOSITORY}/releases/download/{TAG}/", REPOSITORY, TAG
        )
        is None,
    )


def the_check_form_reads_a_manifest_and_says_what_it_finds() -> None:
    good = f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{INSTALLER}"
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / "latest.json"

        path.write_text(json.dumps(manifest(**{"windows-x86_64": good})), encoding="utf-8")
        check(
            "a manifest naming its assets by file passes",
            manifest_urls.check(str(path), REPOSITORY, TAG) == [],
        )

        path.write_text(
            json.dumps(manifest(**{"windows-x86_64": api_url("656365426")})),
            encoding="utf-8",
        )
        problems = manifest_urls.check(str(path), REPOSITORY, TAG)
        check(
            "a manifest naming an asset by id is reported",
            len(problems) == 1 and "release-page url" in problems[0],
            repr(problems),
        )

        # And the command line exits 1 for it, which is what the verify job sees.
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "manifest_urls.py"),
                "--check",
                str(path),
                "--repository",
                REPOSITORY,
                "--tag",
                TAG,
            ],
            capture_output=True,
            text=True,
        )
        check(
            "the check exits 1 and says why",
            result.returncode == 1 and "release-page url" in result.stdout,
            f"exit {result.returncode}\n{result.stdout}{result.stderr}",
        )


def main() -> int:
    print("Finding the release whose assets are named")
    a_draft_release_is_found_by_listing_releases()
    a_tag_with_no_release_is_refused()
    print("Rewriting the manifest's asset urls")
    an_asset_id_is_named_by_the_file_the_release_publishes_it_as()
    every_platform_is_named_even_when_they_share_an_asset()
    a_manifest_that_already_names_its_assets_is_left_alone()
    a_release_page_url_is_read_as_the_name_it_carries()
    print("Refusing a manifest that names an asset nobody can look up")
    an_asset_id_from_another_release_is_refused()
    an_asset_from_another_repository_is_refused()
    a_url_that_is_neither_form_is_refused()
    a_manifest_with_nothing_to_name_is_refused()
    print("Checking a published manifest")
    the_check_form_reads_a_manifest_and_says_what_it_finds()
    print()
    print("The manifest names every asset by the file the release publishes it as,")
    print("and refuses the ones it cannot name.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
