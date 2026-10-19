"""Generate a sample OpenDocument spreadsheet for trying out ExcelScry's ODS support.

The ODF package is written by hand rather than through the app's own exporter,
so the viewer is exercised against XML of the shape LibreOffice writes rather
than against the output of `spreadsheet-ods`. It carries the cases that break
naive viewers: a table that starts at C6, a blank row inside the used range, a
time of day (ODF's `office:time-value`, which calamine surfaces on Excel's day
zero), a date, a boolean, text that looks like markup, and a sheet wider than
the viewport.

Run from the repository root:  python scripts/make_ods_sample.py
"""

from __future__ import annotations

import pathlib
import zipfile
from xml.sax.saxutils import escape

OUT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "samples"
    / "ExcelScry-sample.ods"
)

MIMETYPE = "application/vnd.oasis.opendocument.spreadsheet"

CONTENT_HEADER = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
  xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
  xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
  office:version="1.2">
 <office:automatic-styles>
  <number:currency-style style:name="Nmoney">
   <number:currency-symbol>$</number:currency-symbol>
   <number:number number:decimal-places="2" number:min-integer-digits="1"
     number:grouping="true"/>
  </number:currency-style>
  <number:number-style style:name="Nunits">
   <number:number number:decimal-places="0" number:min-integer-digits="1"
     number:grouping="true"/>
  </number:number-style>
  <number:percentage-style style:name="Nmargin">
   <number:number number:decimal-places="1" number:min-decimal-places="1"/>
   <number:text>%</number:text>
  </number:percentage-style>
  <number:date-style style:name="Ndate">
   <number:day number:style="long"/>
   <number:text>/</number:text>
   <number:month number:style="long"/>
   <number:text>/</number:text>
   <number:year number:style="long"/>
  </number:date-style>
  <number:time-style style:name="Ntime">
   <number:hours number:style="long"/>
   <number:text>:</number:text>
   <number:minutes number:style="long"/>
  </number:time-style>
  <style:style style:name="cemoney" style:family="table-cell"
    style:data-style-name="Nmoney"/>
  <style:style style:name="ceunits" style:family="table-cell"
    style:data-style-name="Nunits"/>
  <style:style style:name="cemargin" style:family="table-cell"
    style:data-style-name="Nmargin"/>
  <style:style style:name="cedate" style:family="table-cell"
    style:data-style-name="Ndate"/>
  <style:style style:name="cetime" style:family="table-cell"
    style:data-style-name="Ntime"/>
 </office:automatic-styles>
 <office:body>
  <office:spreadsheet>
"""

CONTENT_FOOTER = """  </office:spreadsheet>
 </office:body>
</office:document-content>
"""

MANIFEST = """<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest
  xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"
  manifest:version="1.2">
 <manifest:file-entry manifest:full-path="/" manifest:version="1.2"
   manifest:media-type="%s"/>
 <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
 <manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>
 <manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>
</manifest:manifest>
""" % MIMETYPE

META = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"
  office:version="1.2">
 <office:meta>
  <meta:generator>ExcelScry sample generator</meta:generator>
 </office:meta>
</office:document-meta>
"""

STYLES = """<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  office:version="1.2"/>
"""


def style(name: str = "") -> str:
    """The cell-style reference that carries a number format."""
    return f' table:style-name="{name}"' if name else ""


def text(value: str, cell_style: str = "") -> str:
    """A string cell. `text:p` is the only paragraph ODF needs here."""
    return (
        f'<table:table-cell office:value-type="string"{style(cell_style)}>'
        f"<text:p>{escape(value)}</text:p>"
        "</table:table-cell>"
    )


def number(value: float | int, cell_style: str = "") -> str:
    """A numeric cell. ODF stores every number as a double."""
    return (
        f'<table:table-cell office:value-type="float"{style(cell_style)} '
        f'office:value="{value}"/>'
    )


def date(value: str, cell_style: str = "") -> str:
    return (
        f'<table:table-cell office:value-type="date"{style(cell_style)} '
        f'office:date-value="{value}">'
        f"<text:p>{value}</text:p>"
        "</table:table-cell>"
    )


def time_of_day(value: str, cell_style: str = "") -> str:
    """A bare time of day, e.g. `PT09H30M00S` — not a date in 1899."""
    return (
        f'<table:table-cell office:value-type="time"{style(cell_style)} '
        f'office:time-value="{value}">'
        f"<text:p>{value}</text:p>"
        "</table:table-cell>"
    )


def boolean(value: bool) -> str:
    return (
        '<table:table-cell office:value-type="boolean" '
        f'office:boolean-value="{str(value).lower()}">'
        f"<text:p>{'TRUE' if value else 'FALSE'}</text:p>"
        "</table:table-cell>"
    )


def empty(count: int = 1) -> str:
    repeat = f' table:number-columns-repeated="{count}"' if count > 1 else ""
    return f"<table:table-cell{repeat}/>"


def row(cells: list[str], leading_blanks: int = 0) -> str:
    prefix = empty(leading_blanks) if leading_blanks else ""
    return f"<table:table-row>{prefix}{''.join(cells)}</table:table-row>"


def blank_row() -> str:
    """An empty row. ODF rows are sequential, so padding rows are how a table
    starts below row 1 — there is no row index to set."""
    return "<table:table-row><table:table-cell/></table:table-row>"


def sales_report() -> str:
    out = ['<table:table table:name="Sales report">']
    # Five empty rows put the table at C6 rather than C1, the same offset the
    # .xlsx sample uses.
    out.extend(blank_row() for _ in range(5))

    headers = [
        "Region",
        "Product",
        "Units",
        "Unit price",
        "Revenue",
        "Ordered",
        "Notes",
        "Start time",
        "Margin",
    ]
    out.append(row([text(h) for h in headers], leading_blanks=2))  # C6

    records = [
        ["North", "Widget", 1200, 19.99, 23988.0, "2026-01-15", "repeat customer", "PT09H30M00S", 0.35],
        ["South", "Widget", 940, 19.99, 18790.6, "2026-02-03", "", "PT13H05M15S", 0.28],
        ["East", "Gadget", 0, 249.5, 0.0, "2026-02-20", "backordered", "PT23H59M59S", 0.0],
        ["West", "Gadget", 37, 249.5, 9231.5, "2026-03-01", "expedited", "PT08H00M00S", 0.42],
    ]
    for region, product, units, price, revenue, ordered, notes, start, margin in records:
        cells = [
            text(region),
            text(product),
            number(units, "ceunits"),
            number(price, "cemoney"),
            number(revenue, "cemoney"),
            date(ordered, "cedate"),
        ]
        cells.append(boolean(True) if notes == "backordered" else text(notes))
        cells.append(time_of_day(start, "cetime"))
        cells.append(number(margin, "cemargin"))
        out.append(row(cells, leading_blanks=2))

    # A blank row inside the used range: the grid must not collapse it.
    out.append(blank_row())

    # Text that looks like markup has to be shown, not interpreted.
    out.append(
        row(
            [
                text("North"),
                text("Doohickey"),
                number(8, "ceunits"),
                number(0.1, "cemoney"),
                number(0.8, "cemoney"),
                date("2026-04-02", "cedate"),
                text("<b>not bold</b> & a bare ampersand"),
                time_of_day("PT00H00M00S", "cetime"),
                number(0.5, "cemargin"),
            ],
            leading_blanks=2,
        )
    )

    out.append("</table:table>")
    return "".join(out)


def wide_sheet() -> str:
    """Wider than the viewport, so horizontal virtual scrolling is exercised."""
    out = ['<table:table table:name="Wide sheet">']
    out.append(row([text(f"col {c}") for c in range(1, 61)]))
    for r in range(2, 26):
        out.append(row([number((r * c) % 97) for c in range(1, 61)]))
    out.append("</table:table>")
    return "".join(out)


def build() -> pathlib.Path:
    content = CONTENT_HEADER + sales_report() + wide_sheet() + CONTENT_FOOTER
    OUT.parent.mkdir(parents=True, exist_ok=True)

    with zipfile.ZipFile(OUT, "w", zipfile.ZIP_DEFLATED) as zf:
        # The mimetype entry has to come first and must not be compressed, or
        # the package is not a valid ODF document for consumers that sniff it.
        zf.writestr(
            zipfile.ZipInfo("mimetype"), MIMETYPE, compress_type=zipfile.ZIP_STORED
        )
        zf.writestr("content.xml", content)
        zf.writestr("styles.xml", STYLES)
        zf.writestr("meta.xml", META)
        zf.writestr("META-INF/manifest.xml", MANIFEST)

    return OUT


if __name__ == "__main__":
    path = build()
    print(f"Wrote {path} ({path.stat().st_size} bytes)")
