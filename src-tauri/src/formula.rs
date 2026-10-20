//! Turning the formulas a workbook carries into OpenFormula.
//!
//! A cell in an ODS can carry `table:formula="of:=SUM([.A1:.A3])"` beside its
//! cached value, and that attribute is what makes a reopened report recalculate
//! instead of staying frozen at the numbers this app read. Excel packages store
//! the same idea in their own syntax — references bare, arguments separated by
//! commas — so writing an ODS means translating, not reparsing: the job is to
//! find the references and the separators without touching string literals.
//!
//! An ODS that was read in already carries OpenFormula, and its text is handed
//! back untouched, so a workbook can go through this app any number of times
//! without the formulas drifting.
//!
//! Translation is deliberately conservative. Anything that cannot be expressed
//! faithfully — a structured reference, a whole-row range, an unknown prefix —
//! returns `None`, and the export writes the cell as the value it already is
//! rather than writing a formula that would be wrong when recalculated.

/// The namespace every formula this module writes is expressed in.
const PREFIX: &str = "of:=";

/// `table:formula` text for a formula as a reader returned it, or `None` when
/// it has no faithful OpenFormula equivalent.
pub fn open_formula(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((namespace, body)) = split_namespace(text) {
        // `of:` and LibreOffice's older `oooc:` are OpenFormula already.
        if matches!(namespace, "of" | "oooc") {
            return Some(text.to_string());
        }
        // `msoxl:` marks the functions Excel writes when ODF has no name for
        // them; underneath is ordinary Excel syntax.
        return translate(body).map(|f| format!("{PREFIX}{f}"));
    }
    let body = text.strip_prefix('=').unwrap_or(text);
    translate(body).map(|f| format!("{PREFIX}{f}"))
}

/// Split `of:=SUM(...)` into `("of", "SUM(...)")`.
fn split_namespace(text: &str) -> Option<(&str, &str)> {
    let (name, body) = text.split_once(":=")?;
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some((name, body))
}

/// Rewrite Excel-syntax formula text as OpenFormula, without the prefix.
fn translate(source: &str) -> Option<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut scanner = Scanner {
        text: &chars,
        at: 0,
        out: String::with_capacity(source.len() + 16),
        arrays: 0,
    };
    while scanner.at < chars.len() {
        scanner.step()?;
    }
    Some(scanner.out)
}

struct Scanner<'a> {
    text: &'a [char],
    at: usize,
    out: String,
    /// Brace depth of an array constant, where `;` separates rows.
    arrays: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<char> {
        self.text.get(self.at).copied()
    }

    fn peek_at(&self, at: usize) -> Option<char> {
        self.text.get(at).copied()
    }

    /// The character just consumed, which decides whether a letter can begin a
    /// reference: `1E5` is a number, not column E of row 5.
    fn previous(&self) -> Option<char> {
        self.at
            .checked_sub(1)
            .and_then(|at| self.text.get(at))
            .copied()
    }

    fn step(&mut self) -> Option<()> {
        let c = self.peek()?;
        match c {
            '"' => self.copy_string()?,
            '#' => self.copy_error(),
            '{' => {
                self.arrays += 1;
                self.out.push('{');
                self.at += 1;
            }
            '}' => {
                self.arrays = self.arrays.saturating_sub(1);
                self.out.push('}');
                self.at += 1;
            }
            ',' => {
                // Arguments are separated by `;` in OpenFormula, and array
                // columns take the same separator.
                self.out.push(';');
                self.at += 1;
            }
            ';' => {
                // Inside an array constant `;` starts the next row.
                self.out.push(if self.arrays > 0 { '|' } else { ';' });
                self.at += 1;
            }
            // The implicit-intersection marker has no OpenFormula spelling.
            '@' => self.at += 1,
            // A single quote only ever starts a sheet name here.
            '\'' => self.reference()?,
            // A structured reference (`Table1[Qty]`) has no equivalent here.
            '[' => return None,
            _ if Self::could_start_reference(c) && !self.inside_word() => {
                if self.reference().is_none() {
                    self.copy_name()?;
                }
            }
            _ => {
                // A row-only range (`1:1`) is a reference this translator does
                // not write; the value is safer than a wrong formula.
                if c.is_ascii_digit()
                    && !self.inside_word()
                    && self.peek_at(self.at + 1) == Some(':')
                {
                    return None;
                }
                self.out.push(c);
                self.at += 1;
            }
        }
        Some(())
    }

    fn could_start_reference(c: char) -> bool {
        c.is_ascii_alphabetic() || c == '$' || c == '_'
    }

    /// True when the character at `at` continues a number or a name, so a cell
    /// reference cannot begin there.
    fn inside_word(&self) -> bool {
        matches!(
            self.previous(),
            Some(c) if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '$')
        )
    }

    /// Copy a double-quoted string, where `""` stands for one quote. Excel and
    /// OpenFormula spell strings the same way, so the text is left alone.
    fn copy_string(&mut self) -> Option<()> {
        self.out.push('"');
        self.at += 1;
        while let Some(c) = self.peek() {
            if c == '"' {
                if self.peek_at(self.at + 1) == Some('"') {
                    self.out.push_str("\"\"");
                    self.at += 2;
                    continue;
                }
                self.out.push('"');
                self.at += 1;
                return Some(());
            }
            self.out.push(c);
            self.at += 1;
        }
        // An unterminated string is not a formula worth writing.
        None
    }

    /// Copy an error literal such as `#DIV/0!` or `#N/A` verbatim.
    fn copy_error(&mut self) {
        self.out.push('#');
        self.at += 1;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, '/' | '!' | '?' | '_') {
                self.out.push(c);
                self.at += 1;
            } else {
                break;
            }
        }
    }

    /// Copy a function name or defined name, dropping Excel's `_xlfn.` marker
    /// for functions newer than its own file format.
    fn copy_name(&mut self) -> Option<()> {
        let start = self.at;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$') {
                self.at += 1;
            } else {
                break;
            }
        }
        let name: String = self.text[start..self.at].iter().collect();
        // A sheet qualifier with nothing usable after it, or a structured
        // reference, would be written as-is and then misread.
        if self.peek() == Some('!') || self.peek() == Some('[') {
            return None;
        }
        let trimmed = name
            .strip_prefix("_xlfn.")
            .or_else(|| name.strip_prefix("_xlws."))
            .or_else(|| name.strip_prefix("_xludf."))
            .unwrap_or(&name);
        self.out.push_str(trimmed);
        Some(())
    }

    /// Write one reference, or a `first:last` range, and leave `at` after it.
    /// Returns `None` when no reference begins here.
    fn reference(&mut self) -> Option<()> {
        let first = self.parse_reference(self.at, false).or_else(|| {
            // A whole column (`A:A`) is only a reference as part of a range.
            let (end, sheet, cell) = self.parse_reference(self.at, true)?;
            (self.peek_at(end) == Some(':')).then_some((end, sheet, cell))
        })?;

        let (mut end, sheet, cell) = first;
        let mut text = String::from("[");
        push_reference(&mut text, sheet.as_deref(), &cell);
        if self.peek_at(end) == Some(':') {
            if let Some((last_end, last_sheet, last_cell)) = self.parse_reference(end + 1, true) {
                text.push(':');
                // A range's second half belongs to the first half's sheet
                // unless it names one of its own.
                push_reference(
                    &mut text,
                    last_sheet.as_deref().or(sheet.as_deref()),
                    &last_cell,
                );
                end = last_end;
            }
        }
        text.push(']');
        self.out.push_str(&text);
        self.at = end;
        Some(())
    }

    /// Read `[sheet!]cell` at `from`, returning where it ends along with its
    /// sheet name and cell text. `partial` allows the bare column or row that
    /// only makes sense on one side of a range.
    fn parse_reference(
        &self,
        from: usize,
        partial: bool,
    ) -> Option<(usize, Option<String>, String)> {
        let mut at = from;
        let mut sheet = None;

        if self.peek_at(at) == Some('\'') {
            let (name, next) = self.quoted_name(at)?;
            if self.peek_at(next) != Some('!') {
                return None;
            }
            sheet = Some(name);
            at = next + 1;
        } else if let Some((name, next)) = self.bare_name(at) {
            if self.peek_at(next) == Some('!') {
                sheet = Some(name);
                at = next + 1;
            }
        }

        let (cell, end) = self.cell_text(at, partial)?;
        Some((end, sheet, cell))
    }

    /// A single-quoted sheet name, where `''` stands for one quote.
    fn quoted_name(&self, from: usize) -> Option<(String, usize)> {
        let mut at = from + 1;
        let mut name = String::from("'");
        loop {
            let c = self.peek_at(at)?;
            if c == '\'' {
                if self.peek_at(at + 1) == Some('\'') {
                    name.push_str("''");
                    at += 2;
                    continue;
                }
                name.push('\'');
                at += 1;
                break;
            }
            name.push(c);
            at += 1;
        }
        Some((name, at))
    }

    /// An unquoted sheet name: letters, digits, underscores and dots.
    fn bare_name(&self, from: usize) -> Option<(String, usize)> {
        let mut at = from;
        let mut name = String::new();
        while let Some(c) = self.peek_at(at) {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.') {
                name.push(c);
                at += 1;
            } else {
                break;
            }
        }
        (!name.is_empty()).then_some((name, at))
    }

    /// `$A$1`, `A1`, and — for one side of a range — `A` or `1`.
    fn cell_text(&self, from: usize, partial: bool) -> Option<(String, usize)> {
        let mut at = from;
        let mut text = String::new();

        if self.peek_at(at) == Some('$') {
            text.push('$');
            at += 1;
        }
        let letters_from = at;
        while matches!(self.peek_at(at), Some(c) if c.is_ascii_alphabetic()) {
            at += 1;
        }
        let letters = at - letters_from;

        if letters > 3 {
            // Longer than any column (`XFD` is the widest), so this is a name.
            return None;
        }

        if letters == 0 {
            // Only a row: valid as one side of a whole-row range.
            if !partial || text.is_empty() {
                return None;
            }
            let digits_from = at;
            while matches!(self.peek_at(at), Some(c) if c.is_ascii_digit()) {
                at += 1;
            }
            if at == digits_from {
                return None;
            }
            return Some((self.text[from..at].iter().collect(), at));
        }

        text.extend(&self.text[letters_from..at]);
        if self.peek_at(at) == Some('$') {
            text.push('$');
            at += 1;
        }
        let digits_from = at;
        while matches!(self.peek_at(at), Some(c) if c.is_ascii_digit()) {
            at += 1;
        }

        if at == digits_from {
            // A bare column, valid only on one side of a range.
            return partial.then_some((text, at));
        }
        text.extend(&self.text[digits_from..at]);

        // `LOG10` is a function, `A1B` a name: a reference ends at a boundary.
        if let Some(c) = self.peek_at(at) {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '(' | '$') {
                return None;
            }
        }
        Some((text, at))
    }
}

/// Append one side of a reference. `.A1` means "on this sheet"; any other name
/// is written before the dot (`Sheet1.A1`).
fn push_reference(out: &mut String, sheet: Option<&str>, cell: &str) {
    out.push_str(sheet.unwrap_or(""));
    out.push('.');
    out.push_str(cell);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_bracketed() {
        assert_eq!(open_formula("=A1").as_deref(), Some("of:=[.A1]"));
        assert_eq!(open_formula("A1").as_deref(), Some("of:=[.A1]"));
        assert_eq!(open_formula("=$B$2").as_deref(), Some("of:=[.$B$2]"));
        assert_eq!(open_formula("=$B2").as_deref(), Some("of:=[.$B2]"));
        assert_eq!(open_formula("=B$2").as_deref(), Some("of:=[.B$2]"));
    }

    #[test]
    fn ranges_are_one_bracket() {
        assert_eq!(
            open_formula("=SUM(A1:B2)").as_deref(),
            Some("of:=SUM([.A1:.B2])")
        );
        assert_eq!(
            open_formula("=SUM($A$1:$B$2)").as_deref(),
            Some("of:=SUM([.$A$1:.$B$2])")
        );
        assert_eq!(
            open_formula("=SUM(A:A)").as_deref(),
            Some("of:=SUM([.A:.A])")
        );
    }

    #[test]
    fn sheet_names_qualify_the_reference() {
        assert_eq!(
            open_formula("=Sheet1!A1").as_deref(),
            Some("of:=[Sheet1.A1]")
        );
        assert_eq!(
            open_formula("='My Sheet'!A1:B2").as_deref(),
            Some("of:=['My Sheet'.A1:'My Sheet'.B2]")
        );
        assert_eq!(
            open_formula("=SUM(Data!A1:A3)").as_deref(),
            Some("of:=SUM([Data.A1:Data.A3])")
        );
    }

    #[test]
    fn arguments_separated_by_commas_become_semicolons() {
        assert_eq!(
            open_formula("=IF(A1>0,\"yes\",\"no\")").as_deref(),
            Some("of:=IF([.A1]>0;\"yes\";\"no\")")
        );
        assert_eq!(
            open_formula("=ROUND(A1/B1,2)").as_deref(),
            Some("of:=ROUND([.A1]/[.B1];2)")
        );
    }

    #[test]
    fn string_literals_are_left_alone() {
        // The comma inside the text is text, not an argument separator.
        assert_eq!(
            open_formula("=CONCATENATE(A1,\", \",B1)").as_deref(),
            Some("of:=CONCATENATE([.A1];\", \";[.B1])")
        );
        // A doubled quote stays doubled.
        assert_eq!(
            open_formula("=\"He said \"\"hi\"\"\"").as_deref(),
            Some("of:=\"He said \"\"hi\"\"\"")
        );
    }

    #[test]
    fn array_constants_keep_their_shape() {
        assert_eq!(open_formula("={1,2;3,4}").as_deref(), Some("of:={1;2|3;4}"));
    }

    #[test]
    fn numbers_are_not_mistaken_for_references() {
        assert_eq!(open_formula("=1E5").as_deref(), Some("of:=1E5"));
        assert_eq!(open_formula("=A1*2.5").as_deref(), Some("of:=[.A1]*2.5"));
    }

    #[test]
    fn errors_and_names_are_kept() {
        assert_eq!(
            open_formula("=IFERROR(A1,#N/A)").as_deref(),
            Some("of:=IFERROR([.A1];#N/A)")
        );
        assert_eq!(open_formula("=Total*2").as_deref(), Some("of:=Total*2"));
        // Functions newer than Excel's file format carry a marker at read time.
        assert_eq!(
            open_formula("=_xlfn.CONCAT(A1,B1)").as_deref(),
            Some("of:=CONCAT([.A1];[.B1])")
        );
    }

    #[test]
    fn openformula_is_returned_as_it_was_read() {
        // An ODS that was read in already says what the writer needs.
        assert_eq!(
            open_formula("of:=SUM([.A1:.A3])").as_deref(),
            Some("of:=SUM([.A1:.A3])")
        );
        assert_eq!(
            open_formula("oooc:=SUM([.A1:.A3])").as_deref(),
            Some("oooc:=SUM([.A1:.A3])")
        );
        // Excel's own namespace holds Excel syntax and is translated.
        assert_eq!(
            open_formula("msoxl:=_xlfn.CONCAT(A1,B1)").as_deref(),
            Some("of:=CONCAT([.A1];[.B1])")
        );
    }

    #[test]
    fn what_cannot_be_written_faithfully_is_refused() {
        // Structured references, whole-row ranges and unterminated strings
        // have no faithful OpenFormula form here.
        assert_eq!(open_formula("=SUM(Table1[Qty])"), None);
        assert_eq!(open_formula("=SUM(1:1)"), None);
        assert_eq!(open_formula("=\"unterminated"), None);
        assert_eq!(open_formula("   "), None);
    }
}
