#!/usr/bin/env python3
"""Publish the update manifest's assets under their own names.

The manifest `tauri-action` writes names each asset with its GitHub API URL,
`.../releases/assets/<id>`. The updater can fetch that - it asks for
`application/octet-stream` - but nothing else can: `curl`, a browser, a script
that reads the release, all get the asset's JSON metadata instead, and
`<id>.sig` names the same asset again rather than the signature published beside
it. The release page serves every asset under its own file name, which is what a
person reading the manifest expects to see and what every tool already knows how
to fetch.

So the manifest is rewritten here, before it is signed: an installed copy checks
the signature over the exact bytes it downloads, so what it verifies has to be
the manifest this script wrote rather than one that was changed afterwards.

    python scripts/manifest_urls.py latest.json \\
        --repository CryptoTaxHelp/ExcelScry --tag v0.3.2

The asset names come from the release itself, through the API, so the URL that
is written is the URL GitHub already serves that asset at - the id in the
manifest is looked up in the release the tag names rather than guessed from the
platform key. A manifest that names an asset from another repository, or one
that is not attached to the release being published, is refused rather than
published pointing somewhere plausible.

    python scripts/manifest_urls.py --check latest.json \\
        --repository CryptoTaxHelp/ExcelScry --tag v0.3.2

The check form asks the same question of a manifest without rewriting it, and
takes the updater endpoint from `tauri.conf.json` when given no manifest, so the
published release can be checked at the address an installed copy asks. It needs
no token: the URLs are checked for their form, and the verify job goes on to
download what they name.

Exit status 1 means the manifest must not be published, and says why.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import verify_release

GITHUB = "https://github.com"
API = "https://api.github.com"

# An asset named by the release API rather than by its file name, which is what
# `tauri-action` writes into the manifest it uploads.
ASSET = re.compile(
    r"^https://api\.github\.com/repos/(?P<repository>[^/]+/[^/]+)"
    r"/releases/assets/(?P<id>\d+)$"
)


def release_page_url(repository: str, tag: str, name: str) -> str:
    """Where GitHub serves a release's asset by name."""
    return (
        f"{GITHUB}/{repository}/releases/download/"
        f"{urllib.parse.quote(tag, safe='')}/{urllib.parse.quote(name, safe='')}"
    )


def published_name(location: str, repository: str, tag: str) -> str | None:
    """The asset name a release-page URL carries, or `None` for another URL.

    Compared against the URL this repository's release page would serve, so a
    name that only happens to sit under `/releases/download/` - another
    repository's, another tag's, a path that is not a release at all - is not one
    of these.
    """
    parsed = urllib.parse.urlparse(location)
    if parsed.scheme != "https" or parsed.netloc.lower() != "github.com":
        return None
    prefix = (
        f"/{repository}/releases/download/{urllib.parse.quote(tag, safe='')}/"
    ).lower()
    if not parsed.path.lower().startswith(prefix):
        return None
    name = urllib.parse.unquote(parsed.path[len(prefix) :])
    return name or None


def release_json(url: str, headers: dict[str, str]) -> object:
    """A GitHub API response as JSON, or the reason it could not be read."""
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(
            request, timeout=verify_release.MANIFEST_TIMEOUT
        ) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        raise SystemExit(
            f"error: {url} answered {error.code}. The asset names come from the\n"
            "       release the tag names, so that release has to exist and its\n"
            "       assets have to be attached before the manifest is signed."
        ) from None
    except urllib.error.URLError as error:
        raise SystemExit(f"error: {url} could not be reached: {error.reason}") from None
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {url} is not valid JSON: {error}") from None


def release_assets(repository: str, tag: str) -> dict[str, str]:
    """The assets attached to a release, as ids mapped to their file names.

    Releases are listed rather than fetched by tag: the by-tag endpoint answers
    404 for a draft even to an authenticated request, and the release job signs
    the manifest while its release is still a draft. Listing includes drafts
    when a token is used, which the release job has, and the public API lists
    everything a copy can already see.
    """
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"

    page = 1
    while True:
        url = f"{API}/repos/{repository}/releases?per_page=100&page={page}"
        releases = release_json(url, headers)
        if not isinstance(releases, list):
            raise SystemExit(f"error: {url} is not a list of releases")

        for release in releases:
            if not isinstance(release, dict) or release.get("tag_name") != tag:
                continue
            assets = release.get("assets")
            if not isinstance(assets, list) or not assets:
                raise SystemExit(
                    f"error: the release for {repository}@{tag} lists no assets, so\n"
                    "       there is nothing to name. Its assets have to be\n"
                    "       attached before the manifest is signed."
                )
            names = {
                str(asset["id"]): str(asset["name"])
                for asset in assets
                if isinstance(asset, dict) and asset.get("id") and asset.get("name")
            }
            if not names:
                raise SystemExit(
                    f"error: the release for {repository}@{tag} lists no asset ids\n"
                    "       and names"
                )
            return names

        # A full page may leave the release on the next one; a short page is
        # the end of the list, and the tag is not on it.
        if len(releases) < 100:
            raise SystemExit(
                f"error: {repository} has no release for tag {tag}, so there is\n"
                "       nothing to name. The release has to exist and its assets\n"
                "       have to be attached before the manifest is signed."
            )
        page += 1


def rewrite(manifest: dict, repository: str, tag: str, names: dict[str, str]) -> list[str]:
    """Point every platform entry at the release page, in place.

    Returns what changed, one `platform: url` line each, so a release log says
    which asset the manifest now names rather than only that it was rewritten.
    """
    platforms = manifest.get("platforms")
    if not isinstance(platforms, dict) or not platforms:
        raise SystemExit("error: the manifest has no platforms, so it names no assets")

    changed = []
    for platform, entry in platforms.items():
        if not isinstance(entry, dict):
            raise SystemExit(f"error: the manifest's {platform} entry is not an object")
        url = entry.get("url")
        if not isinstance(url, str) or not url:
            raise SystemExit(f"error: the manifest's {platform} entry names no url")

        if published_name(url, repository, tag):
            changed.append(f"{platform}: already named by the release page ({url})")
            continue

        match = ASSET.match(url)
        if not match:
            raise SystemExit(
                f"error: the {platform} entry's url is neither a release-page url\n"
                f"       for {repository}@{tag} nor an asset id on this repository:\n"
                f"       {url}\n"
                "       Publishing it would name an asset nobody can look up."
            )
        if match["repository"].lower() != repository.lower():
            raise SystemExit(
                f"error: the {platform} entry names an asset from {match['repository']},\n"
                f"       but this release publishes {repository}. A manifest that\n"
                "       points at another release would install something else."
            )
        name = names.get(match["id"])
        if name is None:
            raise SystemExit(
                f"error: the {platform} entry names asset {match['id']}, which is not\n"
                f"       attached to {repository}@{tag}. The manifest and the release\n"
                "       disagree about what was published, and the name to publish\n"
                "       the asset under is the one the release lists."
            )

        entry["url"] = release_page_url(repository, tag, name)
        changed.append(f"{platform}: {name} -> {entry['url']}")
    return changed


def updater_endpoint() -> str:
    """The manifest URL an installed copy asks, from `tauri.conf.json`.

    Read rather than repeated, so a check with no manifest checks the address the
    app actually updates from.
    """
    endpoints = (
        verify_release.read_config().get("plugins", {}).get("updater", {}).get("endpoints")
        or []
    )
    if not endpoints:
        raise SystemExit(
            "error: tauri.conf.json has no updater endpoint, so there is no "
            "manifest to check without being given one"
        )
    return endpoints[0]


def read_manifest(location: str) -> dict:
    """A manifest from a path or a URL, as parsed JSON."""
    if "://" in location:
        document = verify_release.fetch(
            location,
            "The release job attaches that manifest, so either it has not\n"
            "       finished publishing or it published no updater artifacts.",
        )
    else:
        path = pathlib.Path(location)
        if not path.is_file():
            raise SystemExit(f"error: {location} does not exist")
        document = path.read_bytes()
    try:
        manifest = json.loads(document)
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {location} is not valid JSON: {error}") from None
    if not isinstance(manifest, dict):
        raise SystemExit(f"error: {location} is not a release manifest")
    return manifest


def publish(location: str, repository: str, tag: str) -> None:
    """Rewrite the manifest in place, then leave it to be signed."""
    path = pathlib.Path(location)
    manifest = read_manifest(location)
    changed = rewrite(manifest, repository, tag, release_assets(repository, tag))
    # `ensure_ascii=False` keeps the notes and the version they were written
    # with byte for byte: escaping them would rewrite text this script has no
    # business touching, and the manifest is read as UTF-8 anyway.
    # Written with LF rather than the platform's ending: this file is published
    # and signed, so what it contains should not depend on which machine wrote
    # it.
    path.write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    print(f"{path} now names its assets by file name:")
    for line in changed:
        print(f"  {line}")


def check(location: str, repository: str, tag: str) -> list[str]:
    """Every platform url is one this release publishes by name."""
    problems = []
    manifest = read_manifest(location)
    platforms = manifest.get("platforms")
    if not isinstance(platforms, dict) or not platforms:
        return ["the manifest has no platforms, so it names no assets"]

    for platform, entry in platforms.items():
        url = entry.get("url") if isinstance(entry, dict) else None
        if not isinstance(url, str) or not url:
            problems.append(f"the {platform} entry names no url")
            continue
        name = published_name(url, repository, tag)
        if name is None:
            problems.append(
                f"the {platform} entry is not a {repository}@{tag} release-page url:\n"
                f"          {url}"
            )
            continue
        print(f"  {platform}: {name}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Name the release manifest's assets by file rather than by asset id."
    )
    parser.add_argument(
        "manifest",
        nargs="?",
        help="the manifest to rewrite, or to check with --check; defaults to the "
        "updater endpoint for --check",
    )
    parser.add_argument(
        "--repository",
        default=os.environ.get("GITHUB_REPOSITORY"),
        help="the repository being released, owner/name; defaults to GITHUB_REPOSITORY",
    )
    parser.add_argument(
        "--tag",
        default=os.environ.get("GITHUB_REF_NAME"),
        help="the tag being released; defaults to GITHUB_REF_NAME",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="check the manifest instead of rewriting it, downloading nothing "
        "but the manifest itself",
    )
    args = parser.parse_args()

    if not args.repository:
        raise SystemExit("error: give --repository owner/name, or set GITHUB_REPOSITORY")
    if not args.tag:
        raise SystemExit("error: give --tag v1.2.3, or set GITHUB_REF_NAME")

    if args.check:
        location = args.manifest or updater_endpoint()
        print(f"Checking the manifest at {location}")
        problems = check(location, args.repository, args.tag)
        if problems:
            print(
                "error: the manifest does not name its assets the way a release "
                "page does."
            )
            for problem in problems:
                print(f"  - {problem}")
            print()
            print("An installed copy would still update from it, but nothing else can")
            print("read it, and the .sig it implies names the same asset again.")
            return 1
        print(
            f"Every asset is named under {args.repository}@{args.tag} by the file name"
        )
        print("it is published as.")
        return 0

    if not args.manifest:
        raise SystemExit("error: give the manifest to rewrite, or --check")
    publish(args.manifest, args.repository, args.tag)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
