//! Rendering a cell with the number format its workbook gives it.
//!
//! A spreadsheet cell is a value plus a *format code* — `#,##0.00`,
//! `0.00%`, `dd/mm/yyyy`, `[$€-407]#,##0.00` — that decides how the sheet
//! displays it. `calamine` hands back the value only, so the codes are read
//! out of the package (see [`crate::cell_styles`]) and applied here by
//! `ssfmt`, which implements Excel's format-code grammar. ODF number styles
//! are translated into that same grammar on the way in, so both file families
//! render through one path.

use std::sync::Arc;

use chrono::{NaiveDateTime, NaiveTime, Timelike};
use ssfmt::{DateSystem, FormatOptions, NumberFormat as Parsed};

use crate::model::Cell;

/// A format code parsed once and shared by every cell that uses it.
#[derive(Debug)]
pub struct NumberFormat {
    parsed: Parsed,
    system: DateSystem,
    options: FormatOptions,
}

impl NumberFormat {
    /// Parse a format code, or `None` when it cannot change a cell's display.
    ///
    /// `General` and a bare `@` (Excel's "show it as it is") are deliberately
    /// rejected: the grid already renders those, and going through the format
    /// engine would only be able to differ from [`Cell::display`].
    pub fn parse(code: &str, system: DateSystem) -> Option<Arc<Self>> {
        let trimmed = code.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("general") || trimmed == "@" {
            return None;
        }
        let parsed = Parsed::parse(trimmed).ok()?;
        Some(Arc::new(Self {
            parsed,
            system,
            options: FormatOptions {
                date_system: system,
                ..FormatOptions::default()
            },
        }))
    }

    /// Text under a text section.
    ///
    /// `ssfmt` only looks for a fourth section, which is how Excel stores a
    /// format that differs for text; ODF writes `"prefix"@` as a single
    /// section, which Excel honours too.
    fn text(&self, text: &str) -> String {
        let sections = self.parsed.sections();
        let rendered = self.parsed.format_text(text, &self.options);
        if rendered != text || sections.len() >= 4 {
            return rendered;
        }
        let Some(section) = sections.first() else {
            return rendered;
        };
        if !section.has_text_placeholder() {
            return rendered;
        }
        let mut out = String::new();
        for part in &section.parts {
            match part {
                ssfmt::ast::FormatPart::TextPlaceholder => out.push_str(text),
                ssfmt::ast::FormatPart::Literal(literal)
                | ssfmt::ast::FormatPart::EscapedLiteral(literal) => out.push_str(literal),
                _ => {}
            }
        }
        out
    }

    /// Format `cell`, or `None` when the format must not be applied to it.
    ///
    /// The fallback cases are the ones where formatting would *lose* something
    /// the cell actually holds: an integer too large for `f64` (formatting
    /// goes through a float and would print different digits), and values that
    /// have no numeric meaning to this format, such as booleans and errors.
    pub fn render(&self, cell: &Cell) -> Option<String> {
        match cell {
            Cell::Int(v) => {
                // 2^53 is where f64 stops representing consecutive integers;
                // past it a formatted cell would show digits the file does not
                // contain, so the exact rendering wins over the decoration.
                if v.unsigned_abs() <= 9_007_199_254_740_992 {
                    Some(self.parsed.format(*v as f64, &self.options))
                } else {
                    None
                }
            }
            Cell::Float(v) => Some(self.parsed.format(*v, &self.options)),
            Cell::DateTime(dt) => Some(
                self.parsed
                    .format(serial_of(dt, self.system), &self.options),
            ),
            Cell::Time(t) => Some(self.parsed.format(serial_of_time(t), &self.options)),
            Cell::Duration(d) => {
                let seconds = d.num_seconds() as f64 + f64::from(d.subsec_nanos()) * 1e-9;
                Some(self.parsed.format(seconds / 86_400.0, &self.options))
            }
            // A text section (`@" units"`) decorates text; without one the
            // text comes back unchanged, as it does in the sheet.
            Cell::Text(s) => Some(self.text(s)),
            Cell::Empty | Cell::Bool(_) | Cell::Error(_) => None,
        }
    }
}

/// The serial number a date occupies on the workbook's own calendar.
///
/// Excel counts days from a fixed epoch, with 1900 minutes apart the two
/// systems; the serial is what format codes are written against (`yyyy` on
/// serial 46031 is 2026-01-09). The leap-year quirk of the 1900 system only
/// affects serials below 61, i.e. dates in January and February 1900.
fn serial_of(dt: &NaiveDateTime, system: DateSystem) -> f64 {
    let epoch = match system {
        DateSystem::Date1900 => chrono::NaiveDate::from_ymd_opt(1899, 12, 30),
        DateSystem::Date1904 => chrono::NaiveDate::from_ymd_opt(1904, 1, 1),
    }
    .expect("the epochs are valid dates");
    let days = (dt.date() - epoch).num_days() as f64;
    days + serial_of_time(&dt.time())
}

/// A time of day as the fraction of a day Excel stores.
fn serial_of_time(t: &NaiveTime) -> f64 {
    (f64::from(t.num_seconds_from_midnight()) + f64::from(t.nanosecond()) * 1e-9) / 86_400.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveDate};
    use ssfmt::DateSystem::{Date1900, Date1904};

    fn format(code: &str) -> Arc<NumberFormat> {
        NumberFormat::parse(code, Date1900).expect("a usable format")
    }

    fn rendered(code: &str, cell: Cell) -> String {
        format(code)
            .render(&cell)
            .expect("this cell can be formatted")
    }

    #[test]
    fn currency_is_shown_as_currency() {
        assert_eq!(
            rendered("[$€-407] #,##0.00", Cell::Float(1234.5)),
            "€ 1,234.50"
        );
        assert_eq!(rendered("$#,##0.00", Cell::Int(19)), "$19.00");
        assert_eq!(
            rendered("#,##0.00;[Red](#,##0.00)", Cell::Float(-1234.5)),
            "(1,234.50)"
        );
    }

    #[test]
    fn percentages_are_scaled_and_marked() {
        assert_eq!(rendered("0.00%", Cell::Float(0.1234)), "12.34%");
        assert_eq!(rendered("0%", Cell::Float(0.075)), "8%");
    }

    #[test]
    fn dates_follow_the_format_not_this_apps_conventions() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 9)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(rendered("dd/mm/yyyy", Cell::DateTime(day)), "09/01/2026");
        assert_eq!(rendered("yyyy-mm-dd", Cell::DateTime(day)), "2026-01-09");
        assert_eq!(
            rendered("mmmm d, yyyy", Cell::DateTime(day)),
            "January 9, 2026"
        );
        // A date with a time only shows what its format asks for.
        let with_time = day + Duration::hours(18);
        assert_eq!(
            rendered("dd/mm/yyyy\\ hh:mm", Cell::DateTime(with_time)),
            "09/01/2026 18:00"
        );
    }

    #[test]
    fn times_of_day_render_as_clock_times() {
        let nine_thirty = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
        assert_eq!(rendered("h:mm AM/PM", Cell::Time(nine_thirty)), "9:30 AM");
        assert_eq!(rendered("hh:mm:ss", Cell::Time(nine_thirty)), "09:30:00");
    }

    #[test]
    fn durations_keep_their_hours_past_a_day() {
        assert_eq!(
            rendered("[h]:mm:ss", Cell::Duration(Duration::hours(36))),
            "36:00:00"
        );
    }

    #[test]
    fn the_date_system_decides_the_serial() {
        // The same calendar day is a different serial in each system, so a
        // format applied with the wrong one would shift the date by years.
        let day = NaiveDate::from_ymd_opt(2026, 1, 9)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let f = NumberFormat::parse("dd/mm/yyyy", Date1904).unwrap();
        assert_eq!(f.render(&Cell::DateTime(day)).unwrap(), "09/01/2026");
        assert!(serial_of(&day, Date1900) - serial_of(&day, Date1904) == 1462.0);
    }

    #[test]
    fn text_sections_decorate_text() {
        assert_eq!(
            rendered(r#"@" units""#, Cell::Text("12".into())),
            "12 units"
        );
        // Text under a numeric format is left alone, as in the sheet.
        assert_eq!(rendered("0.00", Cell::Text("n/a".into())), "n/a");
    }

    #[test]
    fn formats_that_show_nothing_new_are_not_kept() {
        for code in ["General", "general", "@", "  "] {
            assert!(
                NumberFormat::parse(code, Date1900).is_none(),
                "{code} should not override the grid's own rendering"
            );
        }
        // An ordinary numeric code is still kept, so the rejection above is
        // about those two forms and not about the parser.
        assert!(NumberFormat::parse("0", Date1900).is_some());
    }

    #[test]
    fn huge_integers_are_not_formatted_through_a_float() {
        // 2^53+1 has no exact f64, and printing 18446744073709552 for it would
        // show a number the file does not contain.
        let cell = Cell::Int(9_007_199_254_740_993);
        assert_eq!(format("#,##0").render(&cell), None);
        assert_eq!(
            format("#,##0").render(&Cell::Int(1_000_000)),
            Some("1,000,000".into())
        );
    }

    #[test]
    fn value_kinds_the_format_cannot_describe_are_left_alone() {
        assert_eq!(format("#,##0.00").render(&Cell::Bool(true)), None);
        assert_eq!(format("#,##0.00").render(&Cell::Error("#N/A".into())), None);
        assert_eq!(format("#,##0.00").render(&Cell::Empty), None);
    }
}
