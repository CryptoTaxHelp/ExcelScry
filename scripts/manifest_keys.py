#!/usr/bin/env python3
"""Put this repository's updater key roll into the release manifest.

An installed copy learns a new updater key - and the retirement of an old one -
from the signed manifest and from nothing else. Changing `pubkey` in
`tauri.conf.json` reaches only the copies that install the release carrying it,
which is why a key that has leaked could not be retired without one release
every installed copy had to take first. The roll therefore travels inside the
bytes the release job signs, and those are the bytes `tauri-action` wrote, so it
is added here, before the signing step covers them.

`src-tauri/updater-keys.json` is that roll: the same `added` and `retired` lists
a manifest carries, so what a release hands over is reviewable in a pull request
rather than only in a tag's assets. The lists are published as they are, and a
`retired` entry has to stay listed in every later release - a copy that skips a
version still has to learn that the key was retired, and a roll is merged into
what a copy already knows rather than replacing it.

    python scripts/manifest_keys.py --check
    python scripts/manifest_keys.py updater-manifest/latest.json

The first form checks the file on its own, which is what the checks job runs, so
a roll that could not be carried out fails a pull request instead of a release.
The second writes it into the manifest, in place. A run with nothing to publish
leaves the manifest exactly as the build wrote it.

Exit status 1 means the release must not be published, and says why.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import verify_release

KEYS_FILE = "src-tauri/updater-keys.json"


def publish(manifest_path: pathlib.Path, roll: dict) -> None:
    """Write the roll into a release manifest, or leave it alone.

    The manifest is rewritten with the same fields it arrived with and the roll
    added last. Its bytes are signed after this, so what an installed copy
    verifies is what was written here.
    """
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"error: {manifest_path} does not exist") from None
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {manifest_path} is not valid JSON: {error}") from None
    if not isinstance(manifest, dict) or "version" not in manifest:
        raise SystemExit(f"error: {manifest_path} is not a release manifest")

    if not roll["added"] and not roll["retired"]:
        print(f"{manifest_path} is published with no key roll: {KEYS_FILE} holds none.")
        return

    manifest["keys"] = {"added": roll["added"], "retired": roll["retired"]}
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"{manifest_path} now carries the key roll from {KEYS_FILE}:")
    verify_release.report_key_roll(roll)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Write the repository's updater key roll into a release manifest."
    )
    parser.add_argument(
        "manifest",
        nargs="?",
        help="the release manifest to write the roll into, in place",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="check the key roll on its own, without a manifest",
    )
    args = parser.parse_args()

    if args.check and args.manifest:
        raise SystemExit("error: --check checks the key roll alone, without a manifest")
    if not args.check and not args.manifest:
        raise SystemExit("error: give the manifest to write into, or --check")

    config = verify_release.read_config()
    roll = verify_release.read_key_roll()
    verify_release.check_key_roll(roll, config)

    if args.check:
        print(f"{KEYS_FILE} holds a roll an installed copy can carry out.")
        verify_release.report_key_roll(roll)
        return 0

    publish(pathlib.Path(args.manifest), roll)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
