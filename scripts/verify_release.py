"""Check that a published release can update an installed copy.

The release job publishes what the build produced. This asks the published
endpoint what an installed app will actually get, which is a different question
and the one that decides whether the release works at all:

* `latest.json` is fetched from the URL in `tauri.conf.json` - the same one the
  app asks - so a release that is not the latest one, or that carries no
  manifest, fails here rather than in someone's status bar;
* the manifest has to announce the version being verified;
* the signature published beside it - `latest.json.sig`, which the release job
  writes with the same key as the installers - has to cover the manifest's own
  bytes. The app checks this before it reads a field out of the manifest, so a
  release whose manifest is unsigned, or whose bytes were rewritten after they
  were signed, is one no installed copy would use;
* the artifact the manifest points at is downloaded, and its signature is
  checked against the public key in `tauri.conf.json`, by the same rules the
  updater applies: the key id, the signature over the artifact, the global
  signature over the trusted comment, and - because `requireSignedVersion` is on
  in that config - the version the trusted comment records;
* the key roll the manifest carries has to be the one this repository publishes
  in `src-tauri/updater-keys.json`. That field is the one value a release
  decides outside the build - an installed copy takes a new key, and the
  retirement of an old one, from the signed manifest and nowhere else - so it is
  checked against the repository's own file, and has to leave at least one key
  that copy still trusts;
* the `.sig` the release publishes beside that installer is fetched too, and has
  to be the same signature the manifest carries: the app verifies the manifest's
  copy, so a `.sig` that disagreed with it would be a file the release offers
  and nothing stands behind;
* the portable executable is downloaded and held to the version the release
  announces. Nothing signs it - it is not what the app updates from - so the
  version resource inside it is the one checkable thing that says which release
  it was built for.

That is every asset the workflow publishes, each checked against the same key
and the same version, so a release is only good when all of them agree.

`--signature-only` stops after the manifest: the version it announces, the
signature published beside it, and the key roll it carries. Nothing is
downloaded, which is what lets the release job run it while the release is still
a draft - a draft's assets are not public, and the release is a draft until this
check has passed.

`tauri.conf.json` is the source for all of those, so a release is measured
against the configuration the shipped app carries rather than against a copy of
it kept here. Both signatures are checked the way the app checks them, and the
manifest's first: the fields this script then uses - the version, and the
installer's URL and signature - are the ones that were signed.

The signature is checked with an independent implementation of the same
minisign/Ed25519 verification the app performs, rather than by linking
`minisign-verify` again: an independent checker is a second opinion, and it runs
where the app cannot - against the assets as served.

Run from the repository root, or let CI run it after the release job:

    python scripts/verify_release.py                 # the version in Cargo.toml
    python scripts/verify_release.py v0.1.2          # a tag name
    python scripts/verify_release.py --manifest latest.json
    python scripts/verify_release.py --manifest latest.json --signature-only
"""

from __future__ import annotations

import argparse
import base64
import binascii
import dataclasses
import hashlib
import mmap
import json
import pathlib
import re
import tempfile
import tomllib
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent

CARGO_TOML = "src-tauri/Cargo.toml"
TAURI_CONF = "src-tauri/tauri.conf.json"
KEYS_FILE = "src-tauri/updater-keys.json"

# The platform key the updater looks for, which is the target this app bundles.
DEFAULT_PLATFORM = "windows-x86_64"

# Generous, because the installer is a few megabytes: this is a guard against a
# manifest pointing somewhere absurd, not a size limit anyone should meet.
MAX_ARTIFACT = 512 * 1024 * 1024

MANIFEST_TIMEOUT = 60
DOWNLOAD_TIMEOUT = 60

CHUNK = 256 * 1024
VERSION = re.compile(r"^(?P<number>\d+\.\d+\.\d+)(?P<rest>[-+].*)?$")


# --------------------------------------------------------------------------
# Ed25519, the signature scheme minisign is built on
# --------------------------------------------------------------------------
#
# The compact reference implementation from RFC 8032's appendix, kept in affine
# coordinates: verification is one scalar multiplication, so the modular inverse
# each addition costs is not worth optimising for a check that runs once per
# release.

P = 2**255 - 19
ORDER = 2**252 + 27742317777372353535851937790883648493

D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def _inverse(value: int) -> int:
    return pow(value % P, P - 2, P)


def _recover_x(y: int, sign: int) -> int | None:
    """The x for this y, or None when no point of the curve has that y."""
    if y >= P:
        return None
    denominator = (D * y * y + 1) % P
    if denominator == 0:
        return None
    xx = (y * y - 1) * _inverse(denominator) % P
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - xx) % P != 0:
        return None
    if x % 2 != sign:
        x = P - x
    return x


def _decompress(encoded: bytes) -> tuple[int, int] | None:
    """The point those 32 bytes encode, or None when they encode no point."""
    if len(encoded) != 32:
        return None
    y = int.from_bytes(encoded, "little") & (2**255 - 1)
    x = _recover_x(y, encoded[31] >> 7)
    return None if x is None else (x, y)


def _add(first: tuple[int, int], second: tuple[int, int]) -> tuple[int, int]:
    x1, y1 = first
    x2, y2 = second
    product = D * x1 * x2 * y1 * y2 % P
    x3 = (x1 * y2 + x2 * y1) * _inverse(1 + product) % P
    y3 = (y1 * y2 + x1 * x2) * _inverse(1 - product) % P
    return x3, y3


def _multiply(point: tuple[int, int], scalar: int) -> tuple[int, int]:
    result = (0, 1)
    while scalar:
        if scalar & 1:
            result = _add(result, point)
        point = _add(point, point)
        scalar >>= 1
    return result


BASE_Y = 4 * _inverse(5) % P
BASE = (_recover_x(BASE_Y, 0), BASE_Y)


def ed25519_verifies(public: bytes, signature: bytes, message: bytes) -> bool:
    """Whether `signature` is a valid Ed25519 signature of `message`.

    The standard equation is `s * B == R + h * A`, with `h` derived from the
    signature's first half, the public key and the message.
    """
    if len(public) != 32 or len(signature) != 64:
        return False
    scalar = int.from_bytes(signature[32:], "little")
    if scalar >= ORDER:
        return False
    point = _decompress(public)
    nonce = _decompress(signature[:32])
    if point is None or nonce is None:
        return False
    challenge = int.from_bytes(
        hashlib.sha512(signature[:32] + public + message).digest(), "little"
    ) % ORDER
    return _multiply(BASE, scalar) == _add(nonce, _multiply(point, challenge))


# --------------------------------------------------------------------------
# The minisign layer Tauri signs with
# --------------------------------------------------------------------------

# The two algorithm markers minisign writes, and whether the message signed is
# the artifact itself or its BLAKE2b-512 digest. Tauri signs the digest, so "ED"
# is the case every release here uses; "Ed" is what older versions of minisign
# produced, and the updater accepts those too.
ALGORITHMS = {b"Ed": False, b"ED": True}

PUBLIC_KEY_LENGTH = 42
SIGNATURE_LENGTH = 74
GLOBAL_SIGNATURE_LENGTH = 64
TRUSTED_PREFIX = "trusted comment: "


@dataclasses.dataclass(frozen=True)
class Signature:
    """A minisign signature as the manifest carries it."""

    key_id: bytes
    signature: bytes
    comment: str
    global_signature: bytes
    prehashed: bool


def _decode_base64(value: str, what: str) -> bytes:
    try:
        return base64.b64decode(value.strip(), validate=True)
    except (binascii.Error, ValueError):
        raise SystemExit(f"error: {what} is not valid base64") from None


def minisign_public_key(field: str, what: str = "the updater pubkey") -> tuple[bytes, bytes]:
    """The key id and key from a base64 public key field.

    That field - `tauri.conf.json`'s `pubkey`, or an entry in a key roll - is
    base64 of a minisign public key file, whose second line is base64 of the key
    itself. `what` names it in a message.
    """
    text = _decode_base64(field, what).decode("utf-8", "replace")
    lines = [line for line in text.splitlines() if line.strip()]
    if len(lines) < 2:
        raise SystemExit(f"error: {what} is not a minisign public key")
    blob = _decode_base64(lines[1], what)
    if len(blob) != PUBLIC_KEY_LENGTH:
        raise SystemExit(f"error: {what} is {len(blob)} bytes, not {PUBLIC_KEY_LENGTH}")
    if blob[:2] not in ALGORITHMS:
        raise SystemExit(f"error: {what} is not Ed25519 ({blob[:2]!r})")
    return blob[2:10], blob[10:42]


def minisign_signature(field: str) -> Signature:
    """Parse the base64 `signature` field of the manifest.

    The field is base64 of a minisign signature file: an untrusted comment, the
    signature, the trusted comment, and a signature over the two.
    """
    text = _decode_base64(field, "the manifest signature").decode("utf-8", "replace")
    lines = text.splitlines()
    if len(lines) < 4:
        raise SystemExit(
            f"error: the manifest signature has {len(lines)} lines; a minisign\n"
            "       signature has four"
        )
    blob = _decode_base64(lines[1], "the manifest signature")
    global_blob = _decode_base64(lines[3], "the manifest signature's global signature")
    if len(blob) != SIGNATURE_LENGTH:
        raise SystemExit(
            f"error: the manifest signature is {len(blob)} bytes, not {SIGNATURE_LENGTH}"
        )
    if len(global_blob) != GLOBAL_SIGNATURE_LENGTH:
        raise SystemExit("error: the manifest signature has no global signature")
    if not lines[2].startswith(TRUSTED_PREFIX):
        raise SystemExit("error: the manifest signature has no trusted comment")
    if blob[:2] not in ALGORITHMS:
        raise SystemExit(f"error: the manifest signature is not Ed25519 ({blob[:2]!r})")
    return Signature(
        key_id=blob[2:10],
        signature=blob[10:74],
        comment=lines[2][len(TRUSTED_PREFIX) :],
        global_signature=global_blob,
        prehashed=ALGORITHMS[blob[:2]],
    )


def asset_name(location: str) -> str:
    """The file name in a URL or a path, for saying which asset is which."""
    return pathlib.PurePath(location.split("?")[0].split("#")[0]).name


def signed_version(comment: str) -> str | None:
    """The version the artifact was signed for, from the trusted comment.

    The comment is tab separated `key:value` fields, as `tauri build` writes
    them - for example `timestamp:...<tab>file:...<tab>version:0.1.2`.
    """
    for field in comment.split("\t"):
        if field.startswith("version:"):
            return field[len("version:") :]
    return None


def _parsed_version(value: str) -> tuple[str, str] | None:
    match = VERSION.match(value.strip().removeprefix("v"))
    return (match.group("number"), match.group("rest") or "") if match else None


def same_version(first: str, second: str) -> bool:
    """Whether two version strings name the same release.

    Compared as versions when both parse, so `0.1.2` and `v0.1.2` match and a
    pre-release does not match its final release, and literally otherwise: the
    same rule the updater applies.
    """
    one, two = _parsed_version(first), _parsed_version(second)
    if one is not None and two is not None:
        return one == two
    return first.strip() == second.strip()


# --------------------------------------------------------------------------
# What an executable says it is
# --------------------------------------------------------------------------

# The structure Explorer reads a file version out of, and the resource type it
# lives in.
VS_FIXED_FILE_INFO = 0xFEEF04BD
RT_VERSION = 16

# The architectures tauri-action spells differently in an asset name from the
# way the manifest's platform keys spell them.
ACTION_ARCH = {"x86_64": "x64", "arm64": "arm64", "aarch64": "arm64"}


def _u16(image, offset: int) -> int:
    """A little-endian word, zero past the end rather than an error.

    Every field read here comes out of a file this script downloaded from the
    internet, so a malformed or truncated one is reported as malformed rather
    than raising from somewhere deep in the parse.
    """
    return int.from_bytes(image[offset : offset + 2], "little")


def _u32(image, offset: int) -> int:
    return int.from_bytes(image[offset : offset + 4], "little")


def _section_for(sections, rva: int) -> int | None:
    """The file offset an address inside the image maps to, or None."""
    for virtual_address, virtual_size, raw_pointer in sections:
        if virtual_address <= rva < virtual_address + virtual_size:
            return raw_pointer + (rva - virtual_address)
    return None


def _resource_entries(image, directory: int) -> dict[int, int]:
    """A resource directory's numbered entries: id -> offset within it.

    The offset keeps its top bit, which says whether the entry is another
    directory or a leaf that points at the data.
    """
    named = _u16(image, directory + 12)
    ids = _u16(image, directory + 14)
    entries: dict[int, int] = {}
    for index in range(ids):
        entry = directory + 16 + (named + index) * 8
        entries[_u32(image, entry)] = _u32(image, entry + 4)
    return entries


def _first_resource(image, base: int, type_id: int) -> tuple[int, int] | None:
    """The (address, size) of the first resource of `type_id`, or None."""
    kind = _resource_entries(image, base).get(type_id)
    if kind is None:
        return None
    names = _resource_entries(image, base + (kind & 0x7FFFFFFF))
    if not names:
        return None
    languages = _resource_entries(image, base + (next(iter(names.values())) & 0x7FFFFFFF))
    if not languages:
        return None
    leaf = base + (next(iter(languages.values())) & 0x7FFFFFFF)
    return _u32(image, leaf), _u32(image, leaf + 4)


def read_pe_version(image) -> tuple[int, int, int, int] | None:
    """The file version a Windows executable records, or None.

    This is the number Windows' own properties page shows and the number the app
    reports as its own version (`app_version`, read from the bundle's manifest),
    so it is what the portable build can be held to: nothing signs that file, and
    the version it carries is the one checkable thing that says which release it
    was built for.

    Read out of the resource section rather than searched for as text. The
    version lives in a `VS_FIXEDFILEINFO` structure, the one Explorer reads, and
    a `0.1.2` appearing somewhere inside a 20 MB image would prove nothing about
    it.
    """
    if image[:2] != b"MZ":
        return None
    pe = _u32(image, 0x3C)
    if image[pe : pe + 4] != b"PE\0\0":
        return None

    count = _u16(image, pe + 6)
    optional = pe + 24
    optional_size = _u16(image, pe + 20)
    magic = _u16(image, optional)
    if magic == 0x10B:  # 32-bit
        directories = optional + 96
    elif magic == 0x20B:  # 64-bit
        directories = optional + 112
    else:
        return None

    # Entry 2 of the data directories is the resource section.
    resource_rva = _u32(image, directories + 16)
    if not resource_rva:
        return None

    section_table = optional + optional_size
    sections = [
        (_u32(image, section_table + index * 40 + 12),
         max(_u32(image, section_table + index * 40 + 8),
             _u32(image, section_table + index * 40 + 16)),
         _u32(image, section_table + index * 40 + 20))
        for index in range(count)
    ]
    base = _section_for(sections, resource_rva)
    if base is None:
        return None

    found = _first_resource(image, base, RT_VERSION)
    if found is None:
        return None
    data = _section_for(sections, found[0])
    if data is None:
        return None

    # VS_VERSIONINFO: wLength, wValueLength, wType, then the key as UTF-16
    # (`VS_VERSION_INFO`), padded to a four-byte boundary, and then the fixed
    # file information this is after.
    cursor = data + 6
    while cursor < data + 1024 and _u16(image, cursor) != 0:
        cursor += 2
    fixed = (cursor + 2 + 3) & ~3
    if _u32(image, fixed) != VS_FIXED_FILE_INFO:
        return None

    high, low = _u32(image, fixed + 8), _u32(image, fixed + 12)
    return (high >> 16, high & 0xFFFF, low >> 16, low & 0xFFFF)


def portable_name(config: dict, platform: str) -> str:
    """The name `tauri-action` gives the plain binary it publishes.

    `uploadPlainBinary` uploads the built executable as
    `<main binary name>_<platform>_<arch><ext>`: the binary name comes from
    `mainBinaryName` in `tauri.conf.json` or, as here, from the Cargo package;
    the architecture is spelled the action's way, so `x86_64` becomes `x64`; and
    the version is deliberately *not* in the name, which is why this is built
    from the project rather than from the manifest. If the action's rule ever
    changes, this asks for a file that is not there and fails, rather than
    quietly checking nothing.
    """
    os_name, _, arch = platform.partition("-")
    binary = config.get("mainBinaryName") or package_name()
    extension = ".exe" if os_name == "windows" else ""
    return f"{binary}_{os_name}_{ACTION_ARCH.get(arch, arch)}{extension}"


# --------------------------------------------------------------------------
# Fetching what the app fetches
# --------------------------------------------------------------------------


def read_config() -> dict:
    try:
        return json.loads((ROOT / TAURI_CONF).read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"error: {TAURI_CONF} does not exist") from None
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {TAURI_CONF} is not valid JSON: {error}") from None


def read_key_roll() -> dict:
    """The key roll this repository publishes, as `updater-keys.json` writes it.

    The file holds the two lists a manifest carries, so that what a release
    hands over and retires is reviewable in a pull request. It is read here, and
    written into a manifest before it is signed, by `scripts/manifest_keys.py`;
    these are the rules both of them apply.
    """
    path = ROOT / KEYS_FILE
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"error: {KEYS_FILE} does not exist") from None
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {KEYS_FILE} is not valid JSON: {error}") from None
    if not isinstance(document, dict):
        raise SystemExit(f"error: {KEYS_FILE} is not an object")

    unknown = sorted(set(document) - {"added", "retired"})
    if unknown:
        raise SystemExit(
            f"error: {KEYS_FILE} carries {', '.join(unknown)}; a key roll has `added` and\n"
            "       `retired`, and nothing else"
        )
    for name in ("added", "retired"):
        entries = document.get(name)
        if entries is None:
            raise SystemExit(f"error: {KEYS_FILE} has no `{name}` list")
        if not isinstance(entries, list) or not all(isinstance(e, str) for e in entries):
            raise SystemExit(f"error: {KEYS_FILE}'s `{name}` is not a list of keys")
    return {"added": document["added"], "retired": document["retired"]}


def check_key_roll(roll: dict, config: dict) -> None:
    """Apply the app's own rules to a key roll, before it is published.

    The app retires a key by its id, refuses a roll that names something that is
    not a key, and refuses one that would leave it trusting nothing - so a roll
    that broke any of those would be a release that either cannot be carried out
    or leaves the next one unverifiable.
    """
    added = {key_id(entry): entry for entry in roll["added"]}
    retired = {key_id(entry): entry for entry in roll["retired"]}

    both = sorted(added.keys() & retired.keys())
    if both:
        raise SystemExit(
            f"error: the key roll hands over and retires the key {both[0].hex()} at once.\n"
            "       An installed copy retires it, so this is a mistake rather than a\n"
            "       rotation."
        )

    trusted = {key_id(config["plugins"]["updater"]["pubkey"])} | set(added)
    if not trusted - set(retired):
        raise SystemExit(
            "error: this key roll retires every key a copy of this build trusts.\n"
            "       The release carrying it would be the last one those copies could\n"
            "       update from, and the app refuses the roll for that reason."
        )


def key_id(entry: str) -> bytes:
    """The id of the key a key-roll entry names."""
    return minisign_public_key(entry, "a key in the key roll")[0]


def check_published_key_roll(manifest: dict, expected: dict, config: dict, location: str) -> dict:
    """The roll a release publishes, held to the repository's own.

    An installed copy acts on the manifest's roll - it is the only way a key is
    handed over or retired - so this is the one field of a release that decides
    something outside the build, and it has to be the file in this repository.
    """
    published = manifest.get("keys") or {}
    if not isinstance(published, dict):
        raise SystemExit(f"error: {location} carries a `keys` that is not a key roll")
    unknown = sorted(set(published) - {"added", "retired"})
    if unknown:
        raise SystemExit(
            f"error: {location} publishes a key roll with {', '.join(unknown)} in it;\n"
            "       an installed copy reads `added` and `retired`"
        )
    published = {
        "added": published.get("added") or [],
        "retired": published.get("retired") or [],
    }
    if not all(isinstance(entry, str) for entry in published["added"] + published["retired"]):
        raise SystemExit(f"error: {location} publishes a key roll that is not a list of keys")

    if published != expected:
        raise SystemExit(
            f"error: {location} publishes a different key roll than {KEYS_FILE}:\n"
            f"       the manifest says  {published}\n"
            f"       the repository says {expected}\n"
            "       An installed copy acts on the manifest's roll, so a release with\n"
            "       the wrong one hands over keys this repository never agreed to."
        )

    # The roll the release carries has to be one an installed copy can carry
    # out, not merely one this repository wrote down once: a release whose roll
    # retired every trusted key would be the last one those copies could take.
    check_key_roll(published, config)
    return published


def report_key_roll(roll: dict) -> None:
    """Say what the release hands over and retires."""
    if not roll["added"] and not roll["retired"]:
        print("  keys       no roll: this release hands over and retires nothing")
        return
    for entry in roll["added"]:
        print(f"  keys       hands over {key_id(entry).hex()}")
    for entry in roll["retired"]:
        print(f"  keys       retires    {key_id(entry).hex()}")


def cargo_package() -> dict:
    with (ROOT / CARGO_TOML).open("rb") as handle:
        return tomllib.load(handle)["package"]


def current_version() -> str:
    return cargo_package()["version"]


def package_name() -> str:
    """The name the built executable carries, which is what it is published as."""
    return cargo_package()["name"]


def fetch(location: str, missing: str) -> bytes:
    """The bytes at `location`, which is a URL or a path.

    `missing` is what a 404 means for this particular file: the manifest and the
    signature published beside it are absent for different reasons.
    """
    if "://" not in location:
        path = pathlib.Path(location)
        if not path.is_file():
            raise SystemExit(f"error: {location} is not a file, and is not a URL")
        return path.read_bytes()

    try:
        with urllib.request.urlopen(location, timeout=MANIFEST_TIMEOUT) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            raise SystemExit(f"error: {location} answers 404. {missing}") from None
        raise SystemExit(f"error: {location} answered {error.code}") from None
    except urllib.error.URLError as error:
        raise SystemExit(f"error: {location} could not be reached: {error.reason}") from None


def fetch_manifest(location: str) -> tuple[bytes, dict]:
    """The manifest at `location`, as it was served and as parsed JSON.

    The bytes are kept because the signature published beside the manifest
    covers those bytes: a document parsed and written out again is not the one
    that was signed.
    """
    document = fetch(
        location,
        "The release job attaches that\n"
        "       manifest, so either it has not finished publishing or it\n"
        "       published no updater artifacts (uploadUpdaterJson).",
    )
    try:
        manifest = json.loads(document)
    except json.JSONDecodeError as error:
        raise SystemExit(f"error: {location} is not valid JSON: {error}") from None
    if not isinstance(manifest, dict):
        raise SystemExit(f"error: {location} is not a release manifest")
    return document, manifest


def signature_location(location: str) -> str:
    """Where the signature over a manifest is published: `location`, `.sig`.

    The rule the app applies, so this asks for the file the app would ask for
    rather than a path of its own. The suffix goes before a query string or
    fragment, which are not part of the name.
    """
    if "://" not in location:
        return f"{location}.sig"
    end = min(
        (index for index, character in enumerate(location) if character in "?#"),
        default=len(location),
    )
    return f"{location[:end]}.sig{location[end:]}"


def sibling(location: str, name: str) -> str:
    """The URL or path of another asset published with `location`.

    A release publishes its assets side by side, so the other files a release
    carries are found from the manifest's own location: same directory, another
    name. Nothing here needs a token or the release API, and it works the same
    against a local directory as against the release page.
    """
    if "://" not in location:
        return str(pathlib.Path(location).parent / name)
    end = min(
        (index for index, character in enumerate(location) if character in "?#"),
        default=len(location),
    )
    head, tail = location[:end], location[end:]
    directory = head[: head.rindex("/") + 1]
    return f"{directory}{name}{tail}"


def fetch_signature(location: str) -> str:
    """The signature published beside the manifest, as it is written there."""
    return fetch(
        location,
        "The app verifies the manifest before it\n"
        "       believes anything the manifest says, so a release without that\n"
        "       signature cannot update an installed copy: the release job signs\n"
        "       the manifest right after it is published, and that step either\n"
        "       failed or never ran.",
    ).decode("utf-8", "replace")


def download(url: str, missing: str = "") -> tuple[pathlib.Path, int, str, bytes]:
    """Fetch an asset, returning its file, size, SHA-256 and BLAKE2b digest.

    The BLAKE2b digest is what a prehashed signature covers, so both hashes are
    computed as the bytes stream past rather than by reading the file again.

    `url` is what the manifest names the asset with: a URL on the release page,
    or a path when this is reading a release directory. Every asset the release
    publishes is fetched through here, so pointing this at a directory checks
    the same things as pointing it at the release.

    `missing` is what an absent asset means for this particular one, for a
    caller that has something more useful to say about it than the status code.
    """
    if "://" not in url:
        local = pathlib.Path(url)
        if not local.is_file():
            raise SystemExit(f"error: {url} does not exist. {missing}".strip())
        source = local.open("rb")
    else:
        try:
            source = urllib.request.urlopen(url, timeout=DOWNLOAD_TIMEOUT)
        except urllib.error.HTTPError as error:
            if error.code == 404 and missing:
                raise SystemExit(f"error: {url} answers 404. {missing}") from None
            raise SystemExit(f"error: {url} answered {error.code}") from None
        except urllib.error.URLError as error:
            raise SystemExit(f"error: {url} could not be reached: {error.reason}") from None

    blake = hashlib.blake2b(digest_size=64)
    sha = hashlib.sha256()
    written = 0
    with source:
        with tempfile.NamedTemporaryFile(delete=False, suffix=".artifact") as handle:
            path = pathlib.Path(handle.name)
            try:
                while True:
                    chunk = source.read(CHUNK)
                    if not chunk:
                        break
                    written += len(chunk)
                    if written > MAX_ARTIFACT:
                        raise SystemExit(
                            f"error: {url} is over {MAX_ARTIFACT} bytes, and this is a\n"
                            "       release artifact, not a disk image"
                        )
                    blake.update(chunk)
                    sha.update(chunk)
                    handle.write(chunk)
            except BaseException:
                path.unlink(missing_ok=True)
                raise
    return path, written, sha.hexdigest(), blake.digest()


def covered(message: bytes, signature: Signature) -> bytes:
    """The bytes a signature covers.

    `tauri build` and `tauri signer sign` both write a prehashed signature, which
    covers the message's BLAKE2b-512 digest rather than the message; the legacy
    form covers the message itself, and the updater accepts that too. The
    download path below computes the same digest as the bytes stream past, so it
    hands over a digest in place of this.
    """
    if not signature.prehashed:
        return message
    return hashlib.blake2b(message, digest_size=64).digest()


def check_signature(
    message: bytes, signature: Signature, key_id: bytes, key: bytes, subject: str
) -> None:
    """Apply the updater's signature checks to `message`.

    `subject` names what is being checked - the manifest, or the installer it
    points at - because the app refuses either the same way, and which of the two
    failed is what a reader has to act on.
    """
    if signature.key_id != key_id:
        raise SystemExit(
            f"error: {subject} was signed by a different key:\n"
            f"       key id {signature.key_id.hex()}, but the app trusts "
            f"{key_id.hex()}.\n"
            "       An app built from this checkout would refuse it."
        )

    if not ed25519_verifies(key, signature.signature, message):
        raise SystemExit(
            f"error: {subject} does not match its signature\n"
            "       The bytes being served are not the bytes that were signed."
        )

    # The global signature is what makes the trusted comment - and so the
    # version in it - trustworthy.
    global_message = signature.signature + signature.comment.encode("utf-8")
    if not ed25519_verifies(key, signature.global_signature, global_message):
        raise SystemExit(
            "error: the signature's trusted comment is not covered by its global\n"
            "       signature, so the version it records proves nothing"
        )


def check_signed_version(
    signature: Signature, announced: str, require_signed_version: bool, subject: str
) -> str | None:
    """Check the version a signature records against the announced one.

    Returns it, so a caller can say what was signed for, or `None` when the
    signature records no version - which only passes while the configuration
    allows that.
    """
    signed = signed_version(signature.comment)
    if signed is None:
        if require_signed_version:
            raise SystemExit(
                f"error: {subject} was signed without a version, and this build\n"
                "       requires one (requireSignedVersion). Pass --app-version\n"
                "       when signing updater artifacts and the update manifest."
            )
        return None
    if not same_version(signed, announced):
        raise SystemExit(
            f"error: {subject} was signed for {signed}, but the release announces\n"
            f"       {announced}. An installed copy would refuse this update."
        )
    return signed


def fetch_published_signature(installer: str, signature_field: str) -> str:
    """Fetch the installer's published `.sig`, and check it is the manifest's.

    The manifest carries a copy of this signature in its `signature` field, and
    that copy is the one the updater verifies. Requiring the two to agree is
    what keeps a `.sig` somebody downloads by hand from meaning something
    different from the signature the app actually installs under.

    Returns the published signature, for verifying the installer against, so
    every check below is about the file the release serves rather than a copy of
    it in a document.
    """
    location = signature_location(installer)
    name = asset_name(location)
    published = fetch(
        location,
        "The release job publishes the installer's signature\n"
        "       there (uploadUpdaterSignatures), so a release without it is\n"
        "       incomplete.",
    ).decode("utf-8", "replace")

    if published.strip() != signature_field.strip():
        raise SystemExit(
            f"error: {name} is not the signature the manifest carries.\n"
            "       One of the two is wrong, and an installed copy verifies the\n"
            "       manifest's copy, so the file the release offers proves nothing."
        )
    print(f"  .sig       {name} is the signature the manifest carries")
    return published


def check_portable(portable: str, announced: str) -> None:
    """Download the portable build and hold it to the announced version.

    Nothing signs this file: it is the plain executable, carried on the release
    page for people who would rather not install anything, and the app never
    updates from it. Its version resource is therefore the one thing that can be
    checked - and the one thing that catches a portable build left behind by an
    earlier release while the installer moved on.
    """
    name = asset_name(portable)
    print(f"  portable   {portable}")

    path, size, sha256, _ = download(
        portable,
        "The release job publishes the portable build\n"
        "       there (uploadPlainBinary), so a release without it is incomplete.",
    )
    try:
        print(f"  downloaded {size:,} bytes, sha256 {sha256}")
        if not size:
            raise SystemExit(f"error: {name} is empty")
        with path.open("rb") as handle:
            with mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_READ) as image:
                carried = read_pe_version(image)

        wanted = _parsed_version(announced)
        if wanted is None:
            raise SystemExit(
                f"error: {announced!r} is not a version this check can compare."
            )
        if carried is None:
            raise SystemExit(
                f"error: {name} is not a Windows executable with a version\n"
                "       resource, so there is nothing to hold it to this release."
            )

        # The resource records four numbers; a release name has three, and the
        # fourth is a build counter whose meaning is the project's own. A
        # pre-release suffix cannot live in the resource at all, which is why
        # this compares the three the release is named by.
        version = ".".join(str(part) for part in carried)
        if ".".join(str(part) for part in carried[:3]) != wanted[0]:
            raise SystemExit(
                f"error: {name} is version {version}, but this release is\n"
                f"       {announced}. The portable build does not belong to this\n"
                "       release, and it is what people download by hand."
            )
        print(f"  version    {version}, from the executable's version resource")
    finally:
        path.unlink(missing_ok=True)


def report_signature(key_id: bytes, signed_for: str | None) -> None:
    """Say which key signed the item above, and for which version."""
    print(f"  signature  minisign, key {key_id.hex()}")
    if signed_for is None:
        print("  version    the signature records none, and this build allows that")
    else:
        print(f"  version    signed for {signed_for}")


def verify_signed(
    message: bytes,
    signature: Signature,
    key_id: bytes,
    key: bytes,
    require_signed_version: bool,
    announced: str,
    subject: str,
) -> None:
    """Apply the app's checks to something signed, and report what passed.

    The manifest and the installer are signed with the same key and checked the
    same way - which is the point of signing the manifest - so they share this;
    `subject` only names which one failed.
    """
    check_signature(message, signature, key_id, key, subject)
    signed = check_signed_version(signature, announced, require_signed_version, subject)
    report_signature(signature.key_id, signed)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that the published release can update an installed copy."
    )
    parser.add_argument(
        "version", nargs="?", help="the version to expect, e.g. v0.1.2 or 0.1.2"
    )
    parser.add_argument(
        "--manifest",
        help="the manifest to read, a URL or a path; defaults to the updater endpoint",
    )
    parser.add_argument(
        "--platform", default=DEFAULT_PLATFORM, help=f"default: {DEFAULT_PLATFORM}"
    )
    parser.add_argument("--keep", help="copy the artifact here instead of discarding it")
    parser.add_argument(
        "--signature-only",
        action="store_true",
        help=(
            "check the manifest, its signature and its key roll, and stop: "
            "nothing is downloaded, so this runs while the release is a draft"
        ),
    )
    args = parser.parse_args()

    config = read_config()
    updater = config.get("plugins", {}).get("updater", {})
    endpoints = updater.get("endpoints") or []
    pubkey = updater.get("pubkey")
    if not endpoints or not pubkey:
        raise SystemExit(
            f"error: {TAURI_CONF} has no updater endpoint or pubkey, so there is\n"
            "       nothing for a release to have to satisfy."
        )
    require_signed_version = bool(updater.get("requireSignedVersion"))

    expected = args.version or current_version()
    location = args.manifest or endpoints[0]
    print(f"ExcelScry {expected}")
    print()
    print(f"  manifest   {location}")

    document, manifest = fetch_manifest(location)
    announced = manifest.get("version")
    if not announced:
        raise SystemExit(f"error: {location} announces no version")
    if not same_version(str(announced), expected):
        raise SystemExit(
            f"error: the endpoint announces {announced}, not {expected}.\n"
            "       The app asks this endpoint for the newest published release, so an\n"
            "       installed copy would not be offered this version at all: the release\n"
            "       is a draft or a pre-release, or an older one is still the newest."
        )
    print(f"  announced  {announced} ({manifest.get('pub_date') or 'no publication date'})")

    key_id, key = minisign_public_key(pubkey)

    # The manifest is verified before any field of it is used, exactly as the
    # app does it: the entry checked below is the one that decides which
    # installer this script goes on to download.
    signature = minisign_signature(fetch_signature(signature_location(location)))
    verify_signed(
        covered(document, signature),
        signature,
        key_id,
        key,
        require_signed_version,
        str(announced),
        "the manifest",
    )

    # What the release says about the updater key, against the repository's own
    # roll: the app takes one from this manifest and nothing else, so a release
    # that published a roll nobody agreed to would hand over a key of its own.
    expected_roll = read_key_roll()
    check_key_roll(expected_roll, config)
    report_key_roll(check_published_key_roll(manifest, expected_roll, config, location))

    if args.signature_only:
        print()
        print(f"{str(announced)} is signed, and publishes the key roll this repository does.")
        print("The artifacts it points at were not downloaded or checked.")
        return 0

    platforms = manifest.get("platforms") or {}
    entry = platforms.get(args.platform)
    if not entry:
        available = ", ".join(sorted(platforms)) or "none"
        raise SystemExit(
            f"error: the manifest has no {args.platform} artifact; it has: {available}.\n"
            "       An installed copy looks up its own platform and would find nothing."
        )
    url, signature_field = entry.get("url"), entry.get("signature")
    if not url or not signature_field:
        raise SystemExit(f"error: the {args.platform} entry has no url or signature")
    print(f"  artifact   {url}")

    # Fetched before the installer rather than after it: a release that does not
    # publish a signature for its installer is incomplete, and there is no point
    # downloading the installer to find that out.
    published = fetch_published_signature(url, signature_field)

    path, size, sha256, digest = download(url)
    try:
        print(f"  downloaded {size:,} bytes, sha256 {sha256}")
        # The published signature, which the check above proved is the one the
        # manifest carries.
        signature = minisign_signature(published)
        # The digest the download computed while the bytes streamed past, when the
        # signature covers one; the artifact's own bytes when it does not.
        message = digest if signature.prehashed else path.read_bytes()
        verify_signed(
            message,
            signature,
            key_id,
            key,
            require_signed_version,
            str(announced),
            "the artifact",
        )
        if args.keep:
            target = pathlib.Path(args.keep)
            target.write_bytes(path.read_bytes())
            print(f"  kept       {target}")
    finally:
        path.unlink(missing_ok=True)

    check_portable(sibling(location, portable_name(config, args.platform)), str(announced))

    print()
    print(f"The published {announced} release can update an installed copy, and every")
    print("asset the workflow publishes was checked against it.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
