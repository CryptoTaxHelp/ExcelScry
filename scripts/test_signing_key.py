#!/usr/bin/env python3
"""Exercise `scripts/verify_signing_key.py` on keys made for the purpose.

The point of the check is that a release or installer stops before it builds, so
what it has to get right is both directions: a key that cannot sign this build
is refused in a second, and a key that can is not. A check that refused the real key would
be as expensive as the failure it replaced, so the matching case is built from
the pubkey committed in `tauri.conf.json` - the one the release has to sign with
- rather than from a made-up one.

    python scripts/test_signing_key.py

Exit status 1 means the release would not fail early on a missing or wrong key,
or would fail on a good one.
"""

from __future__ import annotations

import base64
import hashlib
import pathlib
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent

sys.path.insert(0, str(HERE))

import verify_release  # noqa: E402  (the path above is what makes this importable)
import verify_signing_key  # noqa: E402


def check(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  ok    {label}")
        return
    print(f"  FAIL  {label}")
    if detail:
        for line in detail.splitlines():
            print(f"        {line}")
    raise SystemExit(1)


def refused(function, says: str, label: str) -> None:
    try:
        function()
    except SystemExit as error:
        check(label, says in str(error), f"said: {error}")
        return
    check(label, False, f"nothing was refused; expected a message containing {says!r}")


def pubkey() -> str:
    return verify_release.read_config()["plugins"]["updater"]["pubkey"]


def key_id_and_material(pubkey_field: str) -> tuple[bytes, bytes]:
    return verify_release.minisign_public_key(pubkey_field)


def secret_key_blob(key_id: bytes, public: bytes, *, encrypted: bool = False) -> bytes:
    """A minisign secret key carrying `key_id` and the Ed25519 secret for `public`.

    A real secret is a 32-byte seed followed by the public key it derives from;
    the check reads the second half, so only that half has to be real for the
    key to identify itself - the seed is never used here.
    """
    prefix = (
        b"Ed"
        + (b"\x01\x00" if encrypted else b"\x00\x00")
        + b"B2"
        + bytes(32)
        + bytes(8)
        + bytes(8)
    )
    secret = bytes(32) + public
    checksum = hashlib.blake2b(prefix[:2] + key_id + secret, digest_size=32).digest()
    return prefix + key_id + secret + checksum


def encrypted(blob: bytes, password: str) -> bytes:
    """`blob` as `tauri signer generate` stores it: xor'd with an scrypt stream.

    The limits are the ones `minisign` writes by default, so the cost is derived
    the same way here as it is read out of a real key.
    """
    memlimit, opslimit = 33_554_432, 1_048_576
    masked = bytearray(blob)
    masked[2:4] = b"\x01\x00"
    masked[38:46] = opslimit.to_bytes(8, "little")
    masked[46:54] = memlimit.to_bytes(8, "little")

    n_log2, r, p = verify_signing_key.scrypt_parameters(memlimit, opslimit)
    check(
        "minisign's default limits give scrypt N=2^15, r=8, p=1",
        (n_log2, r, p) == (15, 8, 1),
        f"got {(n_log2, r, p)}",
    )
    stream = hashlib.scrypt(
        password.encode("utf-8"),
        salt=bytes(masked[6:38]),
        n=1 << n_log2,
        r=r,
        p=p,
        maxmem=memlimit + (1 << 20),
        dklen=104,
    )
    for offset, length in ((54, 8), (62, 64), (126, 32)):
        for index in range(length):
            masked[offset + index] ^= stream[offset - 54 + index]
    return bytes(masked)


def as_secret(blob: bytes) -> str:
    """The environment value the Tauri CLI takes: base64 of a key file."""
    encoded = base64.b64encode(blob).decode()
    text = f"untrusted comment: minisign secret key\n{encoded}\n"
    return base64.b64encode(text.encode()).decode()


def a_key_this_build_trusts_is_accepted() -> list[str]:
    key_id, material = key_id_and_material(pubkey())
    key = verify_signing_key.key_from_secret(
        as_secret(secret_key_blob(key_id, material))
    )
    verify_signing_key.check_against_pubkey(key, pubkey())

    check("a key matching the committed pubkey is accepted", key.key_id == key_id)
    check("it is read as an unencrypted key", not key.encrypted)
    check("its public key is the trusted one", key.public_key == material)
    return []


def a_missing_key_says_what_to_do() -> None:
    refused(
        lambda: verify_signing_key.key_from_secret(""),
        "TAURI_SIGNING_PRIVATE_KEY is empty",
        "an empty secret is refused, naming the secret",
    )
    refused(
        lambda: verify_signing_key.key_from_secret("   \n"),
        "TAURI_SIGNING_PRIVATE_KEY is empty",
        "a whitespace-only secret is refused",
    )


def a_key_file_path_is_read() -> None:
    key_id, material = key_id_and_material(pubkey())
    text = (
        "untrusted comment: minisign secret key\n"
        + base64.b64encode(secret_key_blob(key_id, material)).decode()
        + "\n"
    )
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / "ExcelScry.key"
        path.write_text(text, encoding="utf-8", newline="")
        key = verify_signing_key.key_from_secret(str(path))

    check("a path to the key file is accepted", key.key_id == key_id)
    check("and its public key is read", key.public_key == material)


def a_secret_that_is_not_base64_is_refused() -> None:
    refused(
        lambda: verify_signing_key.key_from_secret(
            "untrusted comment: minisign secret key\nAA=="
        ),
        "not valid base64",
        "a key file pasted instead of its base64 is refused",
    )


def a_secret_that_is_not_a_key_is_refused() -> None:
    refused(
        lambda: verify_signing_key.key_from_secret(
            base64.b64encode(b"not a key at all").decode()
        ),
        "not an untrusted comment",
        "a secret that decodes to something else is refused",
    )


def a_truncated_key_is_refused() -> None:
    key_id, material = key_id_and_material(pubkey())
    truncated = secret_key_blob(key_id, material)[:-10]
    refused(
        lambda: verify_signing_key.key_from_secret(as_secret(truncated)),
        "is 148 bytes",
        "a truncated key is refused, naming its length",
    )


def a_key_with_another_id_is_refused() -> None:
    key_id, material = key_id_and_material(pubkey())
    other = bytes(byte ^ 0xFF for byte in key_id)
    key = verify_signing_key.key_from_secret(as_secret(secret_key_blob(other, material)))
    refused(
        lambda: verify_signing_key.check_against_pubkey(key, pubkey()),
        "not the one this build trusts",
        "a key with another id is refused",
    )


def a_damaged_key_is_refused() -> None:
    key_id, material = key_id_and_material(pubkey())
    damaged = bytearray(secret_key_blob(key_id, material))
    damaged[-1] ^= 0xFF
    refused(
        lambda: verify_signing_key.key_from_secret(as_secret(bytes(damaged))),
        "checksum",
        "a key whose checksum does not cover it is refused",
    )


def an_encrypted_key_is_read_with_its_password() -> None:
    key_id, material = key_id_and_material(pubkey())
    password = "hunter2"
    secret = as_secret(
        encrypted(secret_key_blob(key_id, material), password)
    )

    key = verify_signing_key.key_from_secret(secret, password)
    check("an encrypted key is recognised as one", key.encrypted)
    verify_signing_key.check_against_pubkey(key, pubkey())
    check("and its key id is checked once decrypted", key.key_id == key_id)

    refused(
        lambda: verify_signing_key.key_from_secret(secret, "wrong"),
        "could not be decrypted",
        "the wrong password is refused before the build",
    )

    # The key this project signs with was generated without a password, which
    # `tauri signer generate` still stores encrypted: the empty password is what
    # the CLI uses when none is given, so it is what has to work.
    empty = as_secret(encrypted(secret_key_blob(key_id, material), ""))
    key = verify_signing_key.key_from_secret(empty, "")
    verify_signing_key.check_against_pubkey(key, pubkey())
    check(
        "an empty password is the one assumed when the secret is unset",
        key.key_id == key_id,
    )


def main() -> int:
    print("A key that can sign this release")
    a_key_this_build_trusts_is_accepted()
    a_key_file_path_is_read()
    print("A key that cannot")
    a_missing_key_says_what_to_do()
    a_secret_that_is_not_base64_is_refused()
    a_secret_that_is_not_a_key_is_refused()
    a_truncated_key_is_refused()
    a_key_with_another_id_is_refused()
    a_damaged_key_is_refused()
    an_encrypted_key_is_read_with_its_password()
    print()
    print("A release or installer stops before it builds when the signing key is")
    print("missing, and goes on when the key is the one an installed copy trusts.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
