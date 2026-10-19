"""Generate the delimited-text samples ExcelScry can open.

A CSV has no types, no styles and no package, so the cases that matter are the
ones a naive splitter gets wrong: a field containing the delimiter, a field
containing a doubled quote, a field spanning two lines, a blank record, a row
shorter than the header, a value that only looks numeric (`000123`), and text
that looks like markup. The `.tsv` sample is the same table on tabs, so the
tab reader is exercised against a file a spreadsheet would actually write.

Both files are written CRLF with the standard library's csv writer, which is
the shape Excel, LibreOffice and Sheets produce.

Run from the repository root:  python scripts/make_csv_sample.py
"""

from __future__ import annotations

import csv
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CSV_OUT = ROOT / "samples" / "ExcelScry-sample.csv"
TSV_OUT = ROOT / "samples" / "ExcelScry-sample.tsv"

HEADERS = ["Region", "Product", "Units", "Unit price", "Revenue", "Ordered", "Notes"]

ROWS = [
    ["North", "Widget", "1200", "19.99", "23988.00", "15/01/2026", "repeat customer"],
    ["South", "Widget", "940", "19.99", "18790.60", "03/02/2026", ""],
    ["East", "Gadget", "0", "249.50", "0.00", "20/02/2026", "backordered, with a comma"],
    ["West", "Gadget", "37", "249.50", "9231.50", "01/03/2026", 'said "ship it"'],
    # A record whose field spans two lines, and a value that looks numeric but
    # is an identifier: 000123 must stay exactly that.
    ["North", "Doohickey", "000123", "0.50", "9223.37", "14/03/2026", "two\nlines"],
    # A blank record between real ones: the grid must not collapse it.
    [],
    # Shorter than the header, and text that must not be interpreted.
    ["South", "Doohickey", "8", "0.10", "0.80", "02/04/2026", "café ✅ <b>not bold</b>"],
]


def write(path: pathlib.Path, delimiter: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as file:
        writer = csv.writer(file, delimiter=delimiter, lineterminator="\r\n")
        writer.writerow(HEADERS)
        writer.writerows(ROWS)
    print(f"wrote {path.relative_to(ROOT)} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    write(CSV_OUT, ",")
    write(TSV_OUT, "\t")
