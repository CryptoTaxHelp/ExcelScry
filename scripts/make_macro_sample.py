"""Generate a genuine macro-enabled workbook (.xlsm) for testing ExcelScry.

A .xlsm is not an .xlsx with a different extension. Two things differ in the
package itself, and this script builds both:

  * ``[Content_Types].xml`` declares ``xl/workbook.xml`` as
    ``application/vnd.ms-excel.sheet.macroEnabled.main+xml`` instead of the
    plain ``...spreadsheetml.sheet.main+xml``.
  * the package carries an ``xl/vbaProject.bin`` part, wired in through
    ``xl/_rels/workbook.xml.rels``.

``xl/vbaProject.bin`` is an OLE2 compound file (the same container as a legacy
``.xls``). The one written here is structurally valid -- a real header, FAT,
directory tree and streams -- but the VBA streams inside are placeholders, not
compilable macros. Excel would refuse to run them. That is deliberate: this
fixture exists to prove the *viewer* handles a macro-enabled package, and
ExcelScry never reads, executes or extracts the VBA project. Nothing here
should be treated as a working macro.

Run from the repository root:  python scripts/make_macro_sample.py
"""

from __future__ import annotations

import pathlib
import struct
import zipfile

from openpyxl import load_workbook

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
SRC = ROOT / "samples" / "ExcelScry-sample.xlsx"
OUT = ROOT / "samples" / "ExcelScry-macro-sample.xlsm"

PLAIN_MAIN = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
MACRO_MAIN = "application/vnd.ms-excel.sheet.macroEnabled.main+xml"

# --- OLE2 compound file ------------------------------------------------------

SECTOR = 512
MINI_CUTOFF = 4096
FREESECT = 0xFFFFFFFF
ENDOFCHAIN = 0xFFFFFFFE
FATSECT = 0xFFFFFFFD

ROOT_ENTRY = 5
STORAGE = 1
STREAM = 2

# (path, name-as-shown-in-Excel, bytes). Every stream is padded past the
# 4096-byte mini-stream cutoff so the container needs no mini FAT at all.
PADDED = MINI_CUTOFF
VBA_STREAMS: list[tuple[str, bytes]] = [
    ("PROJECT", b"ID=\"{00000000-0000-0000-0000-000000000000}\"\r\n".ljust(PADDED, b"\0")),
    ("PROJECTwm", b"\0" * PADDED),
    ("VBA/dir", b"\0" * PADDED),
    ("VBA/_VBA_PROJECT", b"\0" * PADDED),
    ("VBA/ThisWorkbook", b"\0" * PADDED),
]


def _entry_name(name: str) -> bytes:
    """A directory entry name: UTF-16LE, NUL-terminated, padded to 64 bytes."""
    raw = name.encode("utf-16-le") + b"\0\0"
    if len(raw) > 64:
        raise ValueError(f"name too long for a CFB entry: {name!r}")
    return raw.ljust(64, b"\0")


def _sort_key(name: str) -> tuple[int, str]:
    """CFB orders siblings by name length, then by an uppercase comparison."""
    return (len(name), name.upper())


def build_vba_project() -> bytes:
    """Assemble a structurally valid OLE2 container holding the VBA streams."""
    # Directory entries, in allocation order: root, VBA storage, then streams.
    names = ["Root Entry", "VBA"] + [p for p, _ in VBA_STREAMS]
    index = {name: i for i, name in enumerate(names)}

    # Root's children: PROJECT, PROJECTwm and the VBA storage.
    root_children = ["PROJECT", "PROJECTwm", "VBA"]
    # VBA's children are the three streams inside it, shown by their leaf name.
    vba_children = ["VBA/dir", "VBA/_VBA_PROJECT", "VBA/ThisWorkbook"]

    def pointers(members: list[str]) -> tuple[dict[str, list[int]], int]:
        """Sibling pointers for a height-balanced tree over `members`.

        Sibling pointers are directory-entry indexes, not positions in the
        input list, so the tree is built over `index`, not over the given
        order. Colouring every node black makes the result a valid red-black
        tree, since every path then has the same black height -- which is all
        the CFB format requires of the tree.
        """
        links = {m: [-1, -1, -1] for m in members}
        ordered = sorted(members, key=lambda m: _sort_key(m.split("/")[-1]))

        def build(lo: int, hi: int) -> int:
            if lo > hi:
                return -1
            mid = (lo + hi) // 2
            node = index[ordered[mid]]
            links[ordered[mid]][0] = build(lo, mid - 1)
            links[ordered[mid]][1] = build(mid + 1, hi)
            return node

        return links, build(0, len(ordered) - 1)

    root_links, root_child = pointers(root_children)
    vba_links, vba_child = pointers(vba_children)

    # --- sector plan ---
    sectors_per_stream = PADDED // SECTOR
    data_sectors = len(VBA_STREAMS) * sectors_per_stream
    dir_dirs = (len(names) * 128 + SECTOR - 1) // SECTOR
    dir_start = data_sectors
    first_fat_sector = dir_start + dir_dirs
    # One FAT sector holds 128 entries, so the whole container must fit in 128
    # sectors for a single-sector FAT to describe it.
    total = first_fat_sector + 1
    assert total <= SECTOR // 4, "this fixture is sized for a single FAT sector"

    # --- stream placement ---
    starts: dict[str, int] = {}
    cursor = 0
    for path, _ in VBA_STREAMS:
        starts[path] = cursor
        cursor += sectors_per_stream

    # --- FAT ---
    fat = [FREESECT] * (SECTOR // 4)
    for path, _ in VBA_STREAMS:
        start = starts[path]
        for i in range(sectors_per_stream - 1):
            fat[start + i] = start + i + 1
        fat[start + sectors_per_stream - 1] = ENDOFCHAIN
    for i in range(dir_dirs - 1):
        fat[dir_start + i] = dir_start + i + 1
    fat[dir_start + dir_dirs - 1] = ENDOFCHAIN
    fat[first_fat_sector] = FATSECT

    # --- directory entries ---
    def entry(name: str, kind: int, left: int, right: int, child: int,
              start: int, size: int) -> bytes:
        return b"".join([
            _entry_name(name),
            struct.pack("<H", len(name.encode("utf-16-le")) + 2),
            struct.pack("<B", kind),
            struct.pack("<B", 1),  # black
            struct.pack("<i", left),
            struct.pack("<i", right),
            struct.pack("<i", child),
            b"\0" * 16,  # CLSID
            struct.pack("<I", 0),  # state bits
            b"\0" * 8,  # created
            b"\0" * 8,  # modified
            struct.pack("<I", start & 0xFFFFFFFF),
            struct.pack("<Q", size),
        ])

    entries = [
        entry("Root Entry", ROOT_ENTRY, -1, -1, root_child, ENDOFCHAIN, 0),
        entry("VBA", STORAGE, -1, -1, vba_child, 0, 0),
    ]
    for path, blob in VBA_STREAMS:
        links = root_links if path in root_links else vba_links
        left, right, child = links[path]
        entries.append(entry(path.split("/")[-1], STREAM, left, right, child,
                             starts[path], len(blob)))

    directory = b"".join(entries).ljust(dir_dirs * SECTOR, b"\0")

    # --- header ---
    difat = [first_fat_sector] + [FREESECT] * 108
    header = b"".join([
        bytes.fromhex("d0cf11e0a1b11ae1"),
        b"\0" * 16,  # CLSID, must be zero
        struct.pack("<H", 0x003E),  # minor version
        struct.pack("<H", 0x0003),  # major version
        struct.pack("<H", 0xFFFE),  # little endian
        struct.pack("<H", 9),  # sector shift: 512 bytes
        struct.pack("<H", 6),  # mini sector shift: 64 bytes
        b"\0" * 6,  # reserved
        struct.pack("<I", 0),  # directory sector count (0 for v3)
        struct.pack("<I", 1),  # FAT sector count
        struct.pack("<I", dir_start),
        struct.pack("<I", 0),  # transaction signature
        struct.pack("<I", MINI_CUTOFF),
        struct.pack("<I", ENDOFCHAIN),  # first mini FAT sector
        struct.pack("<I", 0),  # mini FAT sector count
        struct.pack("<I", ENDOFCHAIN),  # first DIFAT sector
        struct.pack("<I", 0),  # DIFAT sector count
        b"".join(struct.pack("<I", s) for s in difat),
    ])
    assert len(header) == SECTOR

    # --- assemble ---
    body = b"".join(blob for _, blob in VBA_STREAMS)
    fat_bytes = b"".join(struct.pack("<I", s) for s in fat)
    return header + body + directory + fat_bytes


# --- OOXML package -----------------------------------------------------------


def main() -> None:
    if not SRC.exists():
        raise SystemExit(f"missing {SRC}; run scripts/make_sample.py first")

    book = load_workbook(SRC)
    vba = build_vba_project()

    OUT.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(SRC) as src, zipfile.ZipFile(OUT, "w", zipfile.ZIP_DEFLATED) as dst:
        for item in src.infolist():
            data = src.read(item.filename)
            if item.filename == "[Content_Types].xml":
                text = data.decode()
                if PLAIN_MAIN not in text:
                    raise SystemExit("sample workbook content type changed; update this script")
                data = text.replace(PLAIN_MAIN, MACRO_MAIN).encode()
            elif item.filename == "xl/_rels/workbook.xml.rels":
                text = data.decode()
                if "vbaProject.bin" in text:
                    raise SystemExit("sample workbook already references a VBA project")
                rel = (
                    '<Relationship Id="rIdVba" '
                    'Type="http://schemas.microsoft.com/office/2006/relationships/vbaProject" '
                    'Target="vbaProject.bin"/>'
                )
                data = text.replace("</Relationships>", rel + "</Relationships>").encode()
            dst.writestr(item, data)
        dst.writestr("xl/vbaProject.bin", vba)

    size = OUT.stat().st_size
    print(f"wrote {OUT.relative_to(ROOT)} ({size:,} bytes, {book.sheetnames}, "
          f"vbaProject.bin {len(vba):,} bytes)")


if __name__ == "__main__":
    main()
