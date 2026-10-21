//! Following a formula to the cells it is calculated from, and to the cells
//! that are calculated from it.
//!
//! A report is a chain: a total reads a column of products, each product reads a
//! quantity and a price, and a margin reads two totals. The grid shows one cell
//! at a time, so a figure that looks wrong gives no hint of where to look next.
//! This module answers the two questions a reader has in front of that figure —
//! which cells feed it, and which cells it feeds — for the sheet it is on.
//!
//! References are read out of the formula text the workbook carries, in either
//! spelling: Excel's `D7*E7` and an ODS's `of:=[.D7]*[.E7]`. Reading both means
//! going through [`formula::open_formula`], which writes every reference as a
//! bracketed, sheet-qualified name — so one parser covers both, and the page an
//! export writes is the same text this reads.
//!
//! Only the sheet the formula is on is followed. A reference to another sheet is
//! a real dependency, but it is not one this panel can show without becoming a
//! workbook browser; it is dropped rather than shown as a cell on this sheet
//! that it is not. Whole-column and whole-row references are dependencies too,
//! and are named in `unlisted` rather than expanded into every cell they cover.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::formula;
use crate::model::SheetData;

/// How many cells one side of the panel lists before it says there are more.
///
/// A range can name a whole block of a sheet, and a panel that tried to draw
/// every cell of it would be a second grid. Two hundred is enough to trace a
/// chain by eye and small enough to arrive as one page.
pub const LIST_CAP: usize = 200;

/// The widest column and the longest row a reference can name (`XFD`, 1048576).
const MAX_COL: usize = 16_384;
const MAX_ROW: usize = 1_048_576;

/// One cell in the chain, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependent {
    pub row: usize,
    pub col: usize,
    /// What the grid shows for the cell, through its own number format.
    pub text: String,
    /// Whether the cell is itself calculated, so the chain can be followed.
    pub formula: bool,
}

/// What feeds one cell, and what that cell feeds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependencies {
    pub precedents: Vec<Dependent>,
    pub dependents: Vec<Dependent>,
    /// References named as a whole column or row, which are not listed cell by
    /// cell: `A:A`, `1:7`.
    pub unlisted: Vec<String>,
    /// Whether a side stopped at [`LIST_CAP`].
    pub truncated: bool,
}

/// The chain around one cell on one sheet.
pub fn of(sheet: &SheetData, row: usize, col: usize) -> Dependencies {
    let mut dependencies = Dependencies::default();
    let mut seen = BTreeSet::new();

    if let Some(raw) = sheet.formulas.get(row, col) {
        for reference in references(raw, &sheet.name) {
            match reference.span {
                Span::Cells {
                    top,
                    left,
                    bottom,
                    right,
                } => {
                    for (r, c) in cells_of(top, left, bottom, right) {
                        // A cell is not fed by itself, whatever its formula
                        // says: a self-reference is a mistake in the file, not
                        // a step in the chain.
                        if (r, c) == (row, col) {
                            continue;
                        }
                        if seen.len() >= LIST_CAP {
                            dependencies.truncated = true;
                            break;
                        }
                        seen.insert((r, c));
                    }
                }
                Span::Columns { .. } | Span::Rows { .. } => {
                    if !dependencies.unlisted.contains(&reference.label) {
                        dependencies.unlisted.push(reference.label);
                    }
                }
            }
            if dependencies.truncated {
                break;
            }
        }
    }

    dependencies.precedents = seen.into_iter().map(|(r, c)| cell(sheet, r, c)).collect();

    // Every calculated cell is a candidate: the formulas are sparse, so this is
    // a walk over the cells the file calculates rather than over the sheet.
    let mut dependents = Vec::new();
    for (r, c, raw) in sheet.formulas.iter() {
        if (r, c) == (row, col) {
            continue;
        }
        let points_here = references(raw, &sheet.name)
            .iter()
            .any(|reference| reference.span.contains(row, col));
        if !points_here {
            continue;
        }
        if dependents.len() >= LIST_CAP {
            dependencies.truncated = true;
            break;
        }
        dependents.push(cell(sheet, r, c));
    }
    dependents.sort_by_key(|dependent| (dependent.row, dependent.col));
    dependencies.dependents = dependents;

    dependencies
}

fn cell(sheet: &SheetData, row: usize, col: usize) -> Dependent {
    Dependent {
        row,
        col,
        text: sheet.text_at(row, col),
        formula: sheet.formulas.get(row, col).is_some(),
    }
}

/// The cells of a rectangle, row by row, without going past the sheet limits.
///
/// The caller stops at [`LIST_CAP`]; this stops one cell later so it can tell
/// that there was more, and never walks a rectangle the sheet could not hold.
fn cells_of(top: usize, left: usize, bottom: usize, right: usize) -> Vec<(usize, usize)> {
    let bottom = bottom.min(MAX_ROW - 1);
    let right = right.min(MAX_COL - 1);
    let mut cells = Vec::with_capacity(
        (bottom - top + 1)
            .saturating_mul(right - left + 1)
            .min(LIST_CAP + 1),
    );
    for row in top..=bottom {
        for col in left..=right {
            cells.push((row, col));
            if cells.len() > LIST_CAP {
                return cells;
            }
        }
    }
    cells
}

/// One reference a formula makes: the rectangle it names and how to write it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    span: Span,
    label: String,
}

/// A rectangle of cells a reference names, on this sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    /// A bounded rectangle, with every corner named.
    Cells {
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    },
    /// A run of whole columns (`A:C`), every row of each.
    Columns { first: usize, last: usize },
    /// A run of whole rows (`1:7`), every column of each.
    Rows { first: usize, last: usize },
}

impl Span {
    fn contains(&self, row: usize, col: usize) -> bool {
        match *self {
            Span::Cells {
                top,
                left,
                bottom,
                right,
            } => row >= top && row <= bottom && col >= left && col <= right,
            Span::Columns { first, last } => col >= first && col <= last,
            Span::Rows { first, last } => row >= first && row <= last,
        }
    }
}

/// One side of a reference: a cell, or the column or row a range extends along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Cell { row: usize, col: usize },
    Column(usize),
    Row(usize),
}

/// Every reference in a formula, on the sheet `sheet_name` names.
///
/// The formula is put through [`formula::open_formula`] first, so Excel's bare
/// `A1` and an ODS's `[.A1]` arrive here in the same spelling. A formula this
/// app cannot write faithfully — a structured reference, a whole-row range —
/// contributes no references, because its text carries no bracketed names.
fn references(raw: &str, sheet_name: &str) -> Vec<Reference> {
    let Some(normalized) = formula::open_formula(raw) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(start) = normalized[at..].find('[') {
        let start = at + start + 1;
        let Some(end) = normalized[start..].find(']') else {
            break;
        };
        let end = start + end;
        if let Some(reference) = parse_reference(&normalized[start..end], sheet_name) {
            found.push(reference);
        }
        at = end + 1;
        if at >= normalized.len() {
            break;
        }
    }
    found
}

/// Read one bracketed reference, e.g. `.A1`, `.A1:.B2`, `Sheet1.A1`,
/// `'My Sheet'.A1:'My Sheet'.B2`, `.A:.C`, `.3:.7`.
fn parse_reference(body: &str, sheet_name: &str) -> Option<Reference> {
    let (first, second) = match body.split_once(':') {
        Some((first, second)) => (first, Some(second)),
        None => (body, None),
    };
    let first = parse_endpoint(first, sheet_name)?;
    let second = match second {
        Some(second) => parse_endpoint(second, sheet_name)?,
        None => first,
    };

    let (span, label) = match (first, second) {
        (End::Cell { row: r1, col: c1 }, End::Cell { row: r2, col: c2 }) => {
            let (top, bottom) = (r1.min(r2), r1.max(r2));
            let (left, right) = (c1.min(c2), c1.max(c2));
            let label = if (top, left) == (bottom, right) {
                cell_name(top, left)
            } else {
                format!("{}:{}", cell_name(top, left), cell_name(bottom, right))
            };
            (
                Span::Cells {
                    top,
                    left,
                    bottom,
                    right,
                },
                label,
            )
        }
        (End::Column(c1), End::Column(c2)) => {
            let (first, last) = (c1.min(c2), c1.max(c2));
            (
                Span::Columns { first, last },
                format!("{}:{}", column_name(first), column_name(last)),
            )
        }
        (End::Row(r1), End::Row(r2)) => {
            let (first, last) = (r1.min(r2), r1.max(r2));
            (
                Span::Rows { first, last },
                format!("{}:{}", first + 1, last + 1),
            )
        }
        // A rectangle with only one corner named is not a reference any
        // spreadsheet writes, so it is not one to follow.
        _ => return None,
    };
    Some(Reference { span, label })
}

/// Read one side of a reference. Returns `None` for another sheet's cells,
/// which this panel does not follow.
fn parse_endpoint(text: &str, sheet_name: &str) -> Option<End> {
    let text = text.trim();
    let (sheet, cell) = match text.rfind('.') {
        Some(dot) => (&text[..dot], &text[dot + 1..]),
        None => ("", text),
    };
    if !sheet.is_empty() && !same_sheet(sheet, sheet_name) {
        return None;
    }
    parse_cell(cell)
}

/// Whether a sheet name in a formula names the sheet being read.
///
/// A name may be quoted (`'My Sheet'`, with `''` for a quote of its own) or
/// bare, and the two spellings of the same name are the same sheet.
fn same_sheet(name: &str, sheet_name: &str) -> bool {
    let name = name.trim();
    let unquoted = name
        .strip_prefix('\'')
        .and_then(|name| name.strip_suffix('\''))
        .map(|name| name.replace("''", "'"));
    unquoted.as_deref().unwrap_or(name) == sheet_name
}

/// Read `$A$1`, `A1`, `A` or `1`.
fn parse_cell(text: &str) -> Option<End> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    if chars.first() == Some(&'$') {
        at += 1;
    }
    let letters_from = at;
    while matches!(chars.get(at), Some(c) if c.is_ascii_alphabetic()) {
        at += 1;
    }
    let letters = at - letters_from;
    if letters > 3 {
        // Longer than any column name (`XFD`), so this is not a cell.
        return None;
    }

    if letters == 0 {
        // A whole row, which only makes sense on one side of a range.
        let digits = digits_at(&chars, at);
        if digits == 0 {
            return None;
        }
        let row = parse_number(&chars[at..at + digits])?.checked_sub(1)?;
        return (row < MAX_ROW).then_some(End::Row(row));
    }

    let column = parse_column(&chars[letters_from..at])?;
    if chars.get(at) == Some(&'$') {
        at += 1;
    }
    let digits = digits_at(&chars, at);
    if digits == 0 {
        // A whole column, which only makes sense on one side of a range.
        return Some(End::Column(column));
    }
    // `LOG10` is a function and `A1B` a name: a cell ends at a boundary.
    if let Some(c) = chars.get(at + digits) {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '(' | '$') {
            return None;
        }
    }
    let row = parse_number(&chars[at..at + digits])?.checked_sub(1)?;
    (row < MAX_ROW).then_some(End::Cell { row, col: column })
}

fn digits_at(chars: &[char], from: usize) -> usize {
    let mut at = from;
    while matches!(chars.get(at), Some(c) if c.is_ascii_digit()) {
        at += 1;
    }
    at - from
}

fn parse_number(digits: &[char]) -> Option<usize> {
    digits.iter().try_fold(0usize, |value, c| {
        value
            .checked_mul(10)?
            .checked_add((*c as u8 - b'0') as usize)
    })
}

/// `A` is 0, `Z` is 25, `AA` is 26.
fn parse_column(letters: &[char]) -> Option<usize> {
    let mut value = 0usize;
    for c in letters {
        value = value.checked_mul(26)?;
        value = value.checked_add((*c as u8).to_ascii_uppercase() as usize - b'A' as usize + 1)?;
    }
    let column = value.checked_sub(1)?;
    (column < MAX_COL).then_some(column)
}

fn column_name(mut column: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, (b'A' + (column % 26) as u8) as char);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    name
}

fn cell_name(row: usize, col: usize) -> String {
    format!("{}{}", column_name(col), row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cell;

    fn sheet(rows: &[&[&str]], formulas: &[((usize, usize), &str)]) -> SheetData {
        let rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|text| Cell::Text(text.to_string()))
                    .collect()
            })
            .collect();
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut sheet = SheetData {
            name: "Sales".to_string(),
            rows,
            cols,
            formats: Default::default(),
            looks: Default::default(),
            layout: Default::default(),
            formulas: Default::default(),
            original_order: None,
        };
        for &((row, col), formula) in formulas {
            sheet.formulas.insert(row, col, formula);
        }
        sheet
    }

    fn names(cells: &[Dependent]) -> Vec<String> {
        cells
            .iter()
            .map(|cell| cell_name(cell.row, cell.col))
            .collect()
    }

    #[test]
    fn a_product_lists_the_two_cells_it_multiplies() {
        let sheet = sheet(
            &[
                &["Region", "Units", "Price", "Revenue"],
                &["North", "4", "2", "8"],
            ],
            &[((1, 3), "B2*C2")],
        );

        let chain = of(&sheet, 1, 3);
        assert_eq!(names(&chain.precedents), ["B2", "C2"]);
        assert_eq!(chain.precedents[0].text, "4");
        assert!(chain.dependents.is_empty());
    }

    #[test]
    fn the_cells_that_read_a_cell_are_listed_too() {
        // C2 is calculated from A2 and B2; D2 is calculated from C2.
        let sheet = sheet(
            &[
                &["Units", "Price", "Revenue", "Rounded"],
                &["4", "2", "8", "8"],
            ],
            &[((1, 2), "=A2*B2"), ((1, 3), "=C2")],
        );

        let chain = of(&sheet, 1, 0);
        assert_eq!(names(&chain.dependents), ["C2"]);
        assert!(
            chain.precedents.is_empty(),
            "A2 is typed in, not calculated"
        );

        let chain = of(&sheet, 1, 2);
        assert_eq!(names(&chain.precedents), ["A2", "B2"]);
        assert_eq!(names(&chain.dependents), ["D2"]);
        assert!(chain.precedents.iter().all(|cell| !cell.formula));
        assert!(chain.dependents[0].formula, "D2 can be followed further");
    }

    #[test]
    fn a_range_lists_its_cells_row_by_row() {
        let sheet = sheet(
            &[&["A", "B"], &["1", "2"], &["3", "4"]],
            &[((0, 0), "=SUM(B1:B2)")],
        );

        let chain = of(&sheet, 0, 0);
        assert_eq!(names(&chain.precedents), ["B1", "B2"]);
        assert!(chain.unlisted.is_empty());
    }

    #[test]
    fn an_opendocument_formula_is_read_the_same_way() {
        let sheet = sheet(
            &[&["A", "B"], &["1", "2"], &["3", "4"]],
            &[((0, 0), "of:=SUM([.B1:.B2])")],
        );

        let chain = of(&sheet, 0, 0);
        assert_eq!(names(&chain.precedents), ["B1", "B2"]);
    }

    #[test]
    fn absolute_references_are_the_same_cell() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((1, 1), "=$A$1+A1")]);

        let chain = of(&sheet, 1, 1);
        assert_eq!(names(&chain.precedents), ["A1"]);
    }

    #[test]
    fn another_sheets_cells_are_not_pretended_to_be_here() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((1, 1), "=Budget!A1+A1")]);

        let chain = of(&sheet, 1, 1);
        assert_eq!(
            names(&chain.precedents),
            ["A1"],
            "the other sheet's A1 is not this one's"
        );
    }

    #[test]
    fn a_whole_column_is_named_rather_than_listed() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((0, 0), "=SUM(A:A)")]);

        let chain = of(&sheet, 0, 0);
        assert!(chain.precedents.is_empty());
        assert_eq!(chain.unlisted, ["A:A"]);
    }

    #[test]
    fn a_cell_is_not_its_own_dependency() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((1, 0), "=A2+1")]);

        let chain = of(&sheet, 1, 0);
        assert!(chain.precedents.is_empty(), "it is not fed by itself");
        assert!(chain.dependents.is_empty(), "and nothing reads it");
        assert!(chain.unlisted.is_empty());
    }

    #[test]
    fn a_long_range_stops_at_the_cap_and_says_so() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((0, 0), "=SUM(B1:B1000)")]);

        let chain = of(&sheet, 0, 0);
        assert_eq!(chain.precedents.len(), LIST_CAP);
        assert!(chain.truncated, "the panel can say there are more");
        // Every cell of the range is a precedent, including the empty ones the
        // sheet never populated: the formula reads them all.
        assert_eq!(chain.precedents[0].text, "B", "B1, the first of the range");
    }

    #[test]
    fn a_cell_without_a_formula_still_knows_what_reads_it() {
        let sheet = sheet(&[&["A", "B"], &["1", "2"]], &[((1, 1), "=A2")]);

        let chain = of(&sheet, 1, 0);
        assert!(chain.precedents.is_empty());
        assert_eq!(names(&chain.dependents), ["B2"]);
    }
}
