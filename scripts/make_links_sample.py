"""Generate a workbook that carries external links and other active content.

The macro fixture (`make_macro_sample.py`) covers the VBA project. This one
covers the rest of what ExcelScry reports on: links to other workbooks, an
external data connection, an embedded OLE object and an ActiveX control. It
takes the plain sample workbook and adds those parts, the content-type
overrides that declare them, and the workbook relationships that make them
belong to the workbook.

The reference targets are deliberately recognisable -- a local path for one
link, an HTTPS URL for the other -- because the point of the fixture is to see
what the viewer says about a file before opening it anywhere else.

Note that this is a package assembled to exercise the inspector rather than a
file Excel produced: the parts are real, but nothing in it is wired to a
worksheet's drawing layer, so Excel would ignore them. ExcelScry never reads
their contents, only the member list, and calamine ignores them entirely, so
the workbook still opens normally.

Run from the repository root:  python scripts/make_links_sample.py
"""

from __future__ import annotations

import pathlib
import zipfile

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
SRC = ROOT / "samples" / "ExcelScry-sample.xlsx"
OUT = ROOT / "samples" / "ExcelScry-links-sample.xlsx"

EXTERNAL_LINK_TYPE = (
    "application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"
)
CONNECTIONS_TYPE = (
    "application/vnd.openxmlformats-officedocument.spreadsheetml.connections+xml"
)
ACTIVEX_TYPE = "application/vnd.ms-excel.controlproperties+xml"
EXTERNAL_LINK_REL = (
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink"
)
CONNECTIONS_REL = (
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/connections"
)

# (part name, relationship type) pairs to declare in [Content_Types].xml.
OVERRIDES: list[tuple[str, str]] = [
    ("/xl/externalLinks/externalLink1.xml", EXTERNAL_LINK_TYPE),
    ("/xl/externalLinks/externalLink2.xml", EXTERNAL_LINK_TYPE),
    ("/xl/connections.xml", CONNECTIONS_TYPE),
    ("/xl/activeX/activeX1.xml", ACTIVEX_TYPE),
]

# (relationship id, type, target) to add to xl/_rels/workbook.xml.rels.
WORKBOOK_RELS: list[tuple[str, str, str]] = [
    ("rIdLinks1", EXTERNAL_LINK_REL, "externalLinks/externalLink1.xml"),
    ("rIdLinks2", EXTERNAL_LINK_REL, "externalLinks/externalLink2.xml"),
    ("rIdConns", CONNECTIONS_REL, "connections.xml"),
]

NEW_PARTS: list[tuple[str, bytes]] = [
    (
        "xl/externalLinks/externalLink1.xml",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        b' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        b'<externalBook r:id="rId1"/></externalLink>',
    ),
    (
        "xl/externalLinks/_rels/externalLink1.xml.rels",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        b'<Relationship Id="rId1"'
        b' Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath"'
        b' Target="file:///C:/Work/Budget.xlsx" TargetMode="External"/>'
        b"</Relationships>",
    ),
    (
        "xl/externalLinks/externalLink2.xml",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        b' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        b'<externalBook r:id="rId1"/></externalLink>',
    ),
    (
        "xl/externalLinks/_rels/externalLink2.xml.rels",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        b'<Relationship Id="rId1"'
        b' Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath"'
        b' Target="https://example.com/report.xlsx" TargetMode="External"/>'
        b"</Relationships>",
    ),
    (
        "xl/connections.xml",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<connections xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        b'<connection id="1" name="Sales extract" type="5" refreshedVersion="8">'
        b'<dbPr connection="Provider=MSDASQL;Data Source=SalesWarehouse" command="SELECT *"/>'
        b"</connection></connections>",
    ),
    (
        "xl/activeX/activeX1.xml",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<ax:ocx xmlns:ax="http://schemas.microsoft.com/office/2006/activeX"'
        b' ax:classid="{0002E510-0000-0000-C000-000000000046}"/>',
    ),
    (
        "xl/activeX/_rels/activeX1.xml.rels",
        b'<?xml version="1.0" encoding="UTF-8"?>'
        b'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>',
    ),
    # An OLE object: these are real files stowed inside the workbook, which is
    # exactly why a viewer should say it is there.
    ("xl/embeddings/oleObject1.bin", b"\xd0\xcf\x11\xe0" + b"\x00" * 60),
]


def override_tags() -> bytes:
    return b"".join(
        f'<Override PartName="{part}" ContentType="{ctype}"/>'.encode()
        for part, ctype in OVERRIDES
    )


def rel_tags() -> bytes:
    return b"".join(
        f'<Relationship Id="{rid}" Type="{rtype}" Target="{target}"/>'.encode()
        for rid, rtype, target in WORKBOOK_RELS
    )


def main() -> None:
    if not SRC.exists():
        raise SystemExit(f"missing {SRC}; run scripts/make_sample.py first")

    with zipfile.ZipFile(SRC) as src:
        existing = src.namelist()
        names = set(existing)
        for part, _ in NEW_PARTS:
            if part in names:
                raise SystemExit(f"{SRC.name} already contains {part}")
        for part, _ in OVERRIDES:
            if part.lstrip("/") not in names | {p for p, _ in NEW_PARTS}:
                raise SystemExit(f"override for a part that will not exist: {part}")

        has_bin_default = b'Extension="bin"' in src.read("[Content_Types].xml")

        OUT.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(OUT, "w", zipfile.ZIP_DEFLATED) as dst:
            for item in src.infolist():
                data = src.read(item.filename)
                if item.filename == "[Content_Types].xml":
                    text = data.decode()
                    extra = override_tags().decode()
                    if not has_bin_default:
                        extra += (
                            '<Default Extension="bin" '
                            'ContentType="application/vnd.openxmlformats-officedocument.oleObject"/>'
                        )
                    data = text.replace("</Types>", extra + "</Types>").encode()
                elif item.filename == "xl/_rels/workbook.xml.rels":
                    text = data.decode()
                    data = text.replace("</Relationships>", rel_tags().decode() + "</Relationships>").encode()
                dst.writestr(item, data)
            for part, body in NEW_PARTS:
                dst.writestr(part, body)

    print(
        f"wrote {OUT.relative_to(ROOT)} ({OUT.stat().st_size:,} bytes): "
        f"2 external links, 1 data connection, 1 embedded object, 1 ActiveX control"
    )


if __name__ == "__main__":
    main()
