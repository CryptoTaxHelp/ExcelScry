//! Turning a cell's format code into an ODF number style for the export.
//!
//! An ODS cell is a value plus a style name; the style is what makes a stored
//! `1234.5` read as `€ 1,234.50` or `09/01/2026`. Without one, LibreOffice
//! shows the raw number and the export silently loses how the sheet looked.
//!
//! The translation goes through the format's parse tree (`ssfmt`), not its
//! text, so `0.00%` becomes a percentage style that scales by 100 and
//! `[$€-407] #,##0.00` becomes a currency style rather than a string of
//! literals. Codes ODF cannot express — fractions, scientific notation,
//! Buddhist-calendar years — return `None` and the cell keeps its value with
//! no style, which is what the export did before.

use std::collections::HashMap;

use spreadsheet_ods::color::Rgb;
use spreadsheet_ods::condition::ValueCondition;
use spreadsheet_ods::format::{
    FormatNumberStyle, ValueFormatCurrency, ValueFormatDateTime, ValueFormatNumber,
    ValueFormatPercentage, ValueFormatText, ValueFormatTimeDuration, ValueFormatTrait,
    ValueStyleMap,
};
use spreadsheet_ods::{CellStyle, CellStyleRef, ValueFormatRef, WorkBook};
use ssfmt::ast::{Color, DatePart, DigitPlaceholder, ElapsedPart, FormatPart, NamedColor, Section};

use crate::model::CellLook;
use crate::number_format::NumberFormat;

/// Register one digit pattern on a value format that has `number:number`.
macro_rules! push_number {
    ($value:expr, $num:expr) => {{
        let num: &Num = $num;
        $value
            .part_number()
            .min_integer_digits(num.integer_digits)
            .decimal_places(num.decimals)
            .min_decimal_places(num.min_decimals)
            .if_then(num.grouping, |part| part.grouping())
            .build();
    }};
}

/// A piece of the display, in the order the code writes it.
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    /// The digits of the number, with their precision and grouping.
    Number(Num),
    /// Literal text: `" USD"`, `(`, `/`...
    Text(String),
    /// The `%` token, which scales the value as well as showing a sign.
    Percent,
    /// A currency symbol from `[$€-407]`.
    Currency(String),
    /// A calendar component: a date cell's day, month or year.
    Date(DateKind, bool),
    /// A clock component of a time of day.
    Clock(DateKind, bool),
    /// An elapsed-time component (`[h]`), which never wraps at 24 hours.
    Elapsed(DateKind, bool),
    /// The `AM/PM` marker of a twelve-hour clock.
    AmPm,
    /// `@`, where a text cell's own text is shown.
    TextContent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Num {
    integer_digits: u8,
    decimals: u8,
    min_decimals: u8,
    grouping: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DateKind {
    Year,
    Month,
    MonthText,
    Day,
    DayOfWeek,
    Hours,
    Minutes,
    Seconds,
}

/// Which ODF style element can carry the pieces, and how.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Family {
    Number,
    Percentage,
    Currency,
    DateTime,
    /// A clock time, or a duration when the format wrote `[h]`.
    Time {
        elapsed: bool,
    },
    Text,
}

/// Everything that decides which ODF cell style a cell needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StyleKey {
    code: Option<String>,
    bold: bool,
    italic: bool,
    color: Option<String>,
    background: Option<String>,
}

/// Builds and caches the styles for one export.
///
/// Formats and appearances both repeat: a column of money is one code for a
/// thousand cells, and its header is one bold/coloured style. Each distinct
/// combination is translated once and reused.
#[derive(Default)]
pub struct FormatStyles {
    cache: HashMap<StyleKey, Option<CellStyleRef>>,
    next: usize,
}

impl FormatStyles {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cell style that shows `format` and looks like `look`, bold if the
    /// cell is in the sheet's first used row.
    ///
    /// `None` means there is nothing to style: no format ODF can express, no
    /// font or colour change, and not a header cell. The caller then writes the
    /// bare value, exactly as an unstyled cell.
    pub fn style_for(
        &mut self,
        book: &mut WorkBook,
        format: Option<&NumberFormat>,
        look: &CellLook,
        header: bool,
    ) -> Option<CellStyleRef> {
        let bold = look.bold || header;
        let key = StyleKey {
            code: format.map(|format| format.code().to_string()),
            bold,
            italic: look.italic,
            color: look.color.clone(),
            background: look.background.clone(),
        };
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }

        let index = self.next;
        self.next += 1;
        let value = format
            .and_then(|format| build_value_format(book, format, &format!("scry_value_{index}")));

        let styled = value.is_some()
            || bold
            || look.italic
            || look.color.is_some()
            || look.background.is_some();
        let style = styled.then(|| {
            let mut style = CellStyle::new_empty();
            style.set_name(format!("scry_cell_{index}"));
            if let Some(value) = &value {
                style.set_value_format(value);
            }
            if bold {
                style.set_font_bold();
            }
            if look.italic {
                style.set_font_italic();
            }
            if let Some(rgb) = look.color.as_deref().and_then(rgb_of) {
                style.set_color(rgb);
            }
            if let Some(rgb) = look.background.as_deref().and_then(rgb_of) {
                style.set_background_color(rgb);
            }
            book.add_cellstyle(style)
        });

        self.cache.insert(key, style.clone());
        style
    }
}

/// `#RRGGBB` as the RGB triple ODF writes.
fn rgb_of(value: &str) -> Option<Rgb<u8>> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |start: usize| u8::from_str_radix(&hex[start..start + 2], 16).ok();
    Some(Rgb::new(channel(0)?, channel(2)?, channel(4)?))
}

/// Translate one format into a registered ODF value format.
fn build_value_format(
    book: &mut WorkBook,
    format: &NumberFormat,
    name: &str,
) -> Option<ValueFormatRef> {
    let sections = format.sections();
    let positive = sections.first()?;
    let pieces = translate(positive)?;
    let family = family(&pieces)?;

    // Excel's extra sections are the negative and zero ones. ODF cannot branch
    // on a sign inside a single style, so each becomes a second style that a
    // `style:map` on the first one selects.
    let mut maps = Vec::new();
    if let Some(negative) = sections.get(1) {
        if let Some(map) = mapped_section(
            book,
            negative,
            &family,
            &format!("{name}_negative"),
            ValueCondition::value_lt(0),
        ) {
            maps.push(map);
        }
    }
    if let Some(zero) = sections.get(2) {
        if let Some(map) = mapped_section(
            book,
            zero,
            &family,
            &format!("{name}_zero"),
            ValueCondition::value_eq(0),
        ) {
            maps.push(map);
        }
    }

    build_family(
        book,
        &pieces,
        &family,
        name,
        &maps,
        section_colour(positive),
    )
}

/// Register a section that a `style:map` selects, if ODF can use it.
fn mapped_section(
    book: &mut WorkBook,
    section: &Section,
    positive: &Family,
    name: &str,
    condition: ValueCondition,
) -> Option<ValueStyleMap> {
    let pieces = translate(section)?;
    if pieces.is_empty() {
        return None;
    }
    let mut family = family(&pieces)?;
    // A literal-only section (`"-"` for zero) borrows the positive family:
    // a plain number style would render a percentage without its scale.
    if literal_only(&pieces) {
        family = match positive {
            Family::Number | Family::Percentage | Family::Currency => *positive,
            _ => return None,
        };
    }
    // A map can only apply a style of the same kind as the one that carries
    // it; a number shown with a date style would be a different value.
    if !same_family(positive, &family) {
        return None;
    }
    build_family(book, &pieces, &family, name, &[], section_colour(section))?;
    Some(ValueStyleMap::new(condition, name))
}

/// A section's `[Red]`/`[ColorN]` as the colour the ODF style carries.
fn section_colour(section: &Section) -> Option<Rgb<u8>> {
    match section.color? {
        Color::Named(named) => Some(named_rgb(named)),
        Color::Indexed(index) => crate::cell_styles::color_palette_index(index)
            .and_then(crate::cell_styles::default_indexed)
            .and_then(|hex| rgb_of(&hex)),
    }
}

fn named_rgb(color: NamedColor) -> Rgb<u8> {
    let (r, g, b) = match color {
        NamedColor::Black => (0, 0, 0),
        NamedColor::Blue => (0, 0, 255),
        NamedColor::Cyan => (0, 255, 255),
        NamedColor::Green => (0, 255, 0),
        NamedColor::Magenta => (255, 0, 255),
        NamedColor::Red => (255, 0, 0),
        NamedColor::White => (255, 255, 255),
        NamedColor::Yellow => (255, 255, 0),
    };
    Rgb::new(r, g, b)
}

fn same_family(a: &Family, b: &Family) -> bool {
    matches!(
        (a, b),
        (Family::Number, Family::Number)
            | (Family::Percentage, Family::Percentage)
            | (Family::Currency, Family::Currency)
            | (Family::DateTime, Family::DateTime)
            | (Family::Time { .. }, Family::Time { .. })
            | (Family::Text, Family::Text)
    )
}

/// Build and register the value format one piece list describes.
fn build_family(
    book: &mut WorkBook,
    pieces: &[Piece],
    family: &Family,
    name: &str,
    maps: &[ValueStyleMap],
    colour: Option<Rgb<u8>>,
) -> Option<ValueFormatRef> {
    Some(match family {
        Family::Number => {
            let mut value = ValueFormatNumber::new_named(name);
            for piece in pieces {
                match piece {
                    Piece::Number(num) => push_number!(value, num),
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_number_format(value)
        }
        Family::Percentage => {
            let mut value = ValueFormatPercentage::new_named(name);
            for piece in pieces {
                match piece {
                    Piece::Number(num) => push_number!(value, num),
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    // The percent sign is the scaling token of this style
                    // type, so it goes in as the style's own text.
                    Piece::Percent => value.part_text("%").build(),
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_percentage_format(value)
        }
        Family::Currency => {
            let mut value = ValueFormatCurrency::new_named(name);
            for piece in pieces {
                match piece {
                    Piece::Number(num) => push_number!(value, num),
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    Piece::Currency(symbol) => {
                        value.part_currency().symbol(symbol.as_str()).build();
                    }
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_currency_format(value)
        }
        Family::DateTime => {
            let mut value = ValueFormatDateTime::new_named(name);
            for piece in pieces {
                match piece {
                    Piece::Date(kind, long) => push_date(&mut value, *kind, *long),
                    Piece::Clock(kind, long) => push_date_clock(&mut value, *kind, *long),
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    Piece::AmPm => value.part_am_pm().build(),
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_datetime_format(value)
        }
        Family::Time { elapsed } => {
            let elapsed = *elapsed;
            let mut value = ValueFormatTimeDuration::new_named(name);
            // A duration must not wrap at 24 hours: `[h]` is what tells the
            // reader that 1.5 days is 36:00 and not 12:00.
            value.set_truncate_on_overflow(!elapsed);
            for piece in pieces {
                match piece {
                    Piece::Clock(kind, long) | Piece::Elapsed(kind, long) => {
                        push_clock(&mut value, *kind, *long)
                    }
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    Piece::AmPm => value.part_am_pm().build(),
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_timeduration_format(value)
        }
        Family::Text => {
            let mut value = ValueFormatText::new_named(name);
            for piece in pieces {
                match piece {
                    Piece::TextContent => value.part_text_content().build(),
                    Piece::Text(text) => {
                        value.part_text(text.as_str()).build();
                    }
                    _ => return None,
                }
            }
            decorate(&mut value, maps, colour);
            book.add_text_format(value)
        }
    })
}

/// Attach the section maps and the section's own colour, if it has one.
fn decorate<T: ValueFormatTrait>(value: &mut T, maps: &[ValueStyleMap], colour: Option<Rgb<u8>>) {
    for map in maps {
        value.push_stylemap(map.clone());
    }
    if let Some(colour) = colour {
        value.textstyle_mut().set_attr(
            "fo:color",
            format!("#{:02x}{:02x}{:02x}", colour.r, colour.g, colour.b),
        );
    }
}

fn push_date(value: &mut ValueFormatDateTime, kind: DateKind, long: bool) {
    let style = if long {
        FormatNumberStyle::Long
    } else {
        FormatNumberStyle::Short
    };
    match kind {
        DateKind::Year => {
            value.part_year().style(style).build();
        }
        DateKind::Month => {
            value.part_month().style(style).build();
        }
        DateKind::MonthText => {
            value.part_month().style(style).textual().build();
        }
        DateKind::Day => {
            value.part_day().style(style).build();
        }
        DateKind::DayOfWeek => {
            value.part_day_of_week().style(style).build();
        }
        DateKind::Hours | DateKind::Minutes | DateKind::Seconds => {}
    }
}

fn push_date_clock(value: &mut ValueFormatDateTime, kind: DateKind, long: bool) {
    let style = if long {
        FormatNumberStyle::Long
    } else {
        FormatNumberStyle::Short
    };
    match kind {
        DateKind::Hours => {
            value.part_hours().style(style).build();
        }
        DateKind::Minutes => {
            value.part_minutes().style(style).build();
        }
        DateKind::Seconds => {
            value.part_seconds().style(style).build();
        }
        _ => {}
    }
}

fn push_clock(value: &mut ValueFormatTimeDuration, kind: DateKind, long: bool) {
    let style = if long {
        FormatNumberStyle::Long
    } else {
        FormatNumberStyle::Short
    };
    match kind {
        DateKind::Hours => {
            value.part_hours().style(style).build();
        }
        DateKind::Minutes => {
            value.part_minutes().style(style).build();
        }
        DateKind::Seconds => {
            value.part_seconds().style(style).build();
        }
        _ => {}
    }
}

/// Decide which ODF element can express the pieces, or `None` when none can.
fn family(pieces: &[Piece]) -> Option<Family> {
    let mut number = false;
    let mut percent = false;
    let mut currency = false;
    let mut date = false;
    let mut clock = false;
    let mut elapsed = false;
    let mut elapsed_hours = false;
    let mut text_content = false;

    for piece in pieces {
        match piece {
            Piece::Number(_) => number = true,
            Piece::Percent => percent = true,
            Piece::Currency(_) => currency = true,
            Piece::Date(kind, _) => match kind {
                DateKind::Year
                | DateKind::Month
                | DateKind::MonthText
                | DateKind::Day
                | DateKind::DayOfWeek => date = true,
                DateKind::Hours | DateKind::Minutes | DateKind::Seconds => clock = true,
            },
            Piece::Clock(_, _) | Piece::AmPm => clock = true,
            Piece::Elapsed(kind, _) => {
                elapsed = true;
                if *kind == DateKind::Hours {
                    elapsed_hours = true;
                }
            }
            Piece::TextContent => text_content = true,
            Piece::Text(_) => {}
        }
    }

    if elapsed {
        // ODF marks a whole time style as non-wrapping; it cannot say "count
        // minutes here". A format led by `[mm]` would silently show hours.
        if !elapsed_hours || date || percent || currency || text_content || number {
            return None;
        }
        return Some(Family::Time { elapsed: true });
    }
    if date || clock {
        if percent || currency || text_content || number {
            // A date style has no digits; mixing them would change the value.
            return None;
        }
        return Some(if date {
            Family::DateTime
        } else {
            Family::Time { elapsed: false }
        });
    }
    if text_content {
        if number || percent || currency {
            return None;
        }
        return Some(Family::Text);
    }
    if percent && currency {
        return None;
    }
    if percent {
        return Some(Family::Percentage);
    }
    if currency {
        return Some(Family::Currency);
    }
    if number {
        return Some(Family::Number);
    }
    // A section that is only a literal (`"-"` for zero, `"n/a"` for a
    // missing value) still has to be written: ODF number styles may consist of
    // text alone, and dropping it would make the zero fall back to the
    // positive section.
    if !pieces.is_empty() {
        return Some(Family::Number);
    }
    None
}

/// Whether a section is only literal text, so it can borrow the positive
/// section's family without changing what the value means.
fn literal_only(pieces: &[Piece]) -> bool {
    pieces.iter().all(|piece| matches!(piece, Piece::Text(_)))
}

/// The pieces of one section, or `None` if any of it has no ODF equivalent.
fn translate(section: &Section) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut number_seen = false;
    let mut index = 0;

    while index < section.parts.len() {
        match &section.parts[index] {
            FormatPart::Digit(_) | FormatPart::DecimalPoint | FormatPart::ThousandsSeparator => {
                // A second run of digits would print the value twice, which no
                // Excel format does on purpose.
                if number_seen {
                    return None;
                }
                let (num, used) = number_run(&section.parts[index..])?;
                pieces.push(Piece::Number(num));
                number_seen = true;
                index += used;
                continue;
            }
            FormatPart::Literal(text) | FormatPart::EscapedLiteral(text) => {
                if !text.is_empty() {
                    pieces.push(Piece::Text(text.clone()));
                }
            }
            FormatPart::Percent => pieces.push(Piece::Percent),
            FormatPart::Locale(locale) => {
                // `[$-409]` only picks a locale; `[$€-407]` also names a
                // symbol, which the currency style has to carry.
                if let Some(symbol) = &locale.currency {
                    if !symbol.is_empty() {
                        pieces.push(Piece::Currency(symbol.clone()));
                    }
                }
            }
            FormatPart::DatePart(part) => {
                let (kind, long) = date_part(*part)?;
                // A calendar style cannot show a clock time and a time style
                // cannot show a date, so the two kinds are kept apart until
                // `family` has decided which element is being built.
                pieces.push(match kind {
                    DateKind::Hours | DateKind::Minutes | DateKind::Seconds => {
                        Piece::Clock(kind, long)
                    }
                    _ => Piece::Date(kind, long),
                });
            }
            FormatPart::Elapsed(part) => {
                let (kind, long) = elapsed_part(*part)?;
                pieces.push(Piece::Elapsed(kind, long));
            }
            FormatPart::AmPm(_) => pieces.push(Piece::AmPm),
            FormatPart::TextPlaceholder => pieces.push(Piece::TextContent),
            // `_x` reserves the width of a character and `*x` fills the cell;
            // neither changes which characters are shown.
            FormatPart::Skip(_) | FormatPart::Fill(_) => {}
            // Scientific, fraction, and "General" mixed with literals have no
            // faithful ODF form here.
            _ => return None,
        }
        index += 1;
    }

    Some(pieces)
}

/// Read one run of digit placeholders, decimal point and grouping mark.
fn number_run(parts: &[FormatPart]) -> Option<(Num, usize)> {
    let mut integer = 0u8;
    let mut required = 0u8;
    let mut decimals = 0u8;
    let mut required_decimals = 0u8;
    let mut after_point = false;
    let mut grouping = false;
    let mut used = 0usize;

    for part in parts {
        match part {
            FormatPart::Digit(place) => {
                used += 1;
                if after_point {
                    decimals = decimals.saturating_add(1);
                    if *place == DigitPlaceholder::Zero {
                        required_decimals = required_decimals.saturating_add(1);
                    }
                } else {
                    integer = integer.saturating_add(1);
                    if *place == DigitPlaceholder::Zero {
                        required = required.saturating_add(1);
                    }
                }
            }
            FormatPart::DecimalPoint => {
                if after_point {
                    return None;
                }
                after_point = true;
                used += 1;
            }
            FormatPart::ThousandsSeparator => {
                // A comma with no digits before it (or after the point) in
                // Excel scales the value by a thousand; ODF has no equivalent.
                if after_point || integer == 0 {
                    return None;
                }
                grouping = true;
                used += 1;
            }
            _ => break,
        }
    }

    if integer == 0 && decimals == 0 {
        return None;
    }
    Some((
        Num {
            integer_digits: required.max(1),
            decimals,
            min_decimals: required_decimals,
            grouping,
        },
        used,
    ))
}

/// The ODF element a calendar part becomes, and whether it is long-form.
fn date_part(part: DatePart) -> Option<(DateKind, bool)> {
    Some(match part {
        DatePart::Year2 => (DateKind::Year, false),
        DatePart::Year3 | DatePart::Year4 => (DateKind::Year, true),
        DatePart::Month => (DateKind::Month, false),
        DatePart::Month2 => (DateKind::Month, true),
        DatePart::MonthAbbr => (DateKind::MonthText, false),
        DatePart::MonthFull => (DateKind::MonthText, true),
        DatePart::Day => (DateKind::Day, false),
        DatePart::Day2 => (DateKind::Day, true),
        DatePart::DayAbbr => (DateKind::DayOfWeek, false),
        DatePart::DayFull => (DateKind::DayOfWeek, true),
        DatePart::Hour => (DateKind::Hours, false),
        DatePart::Hour2 => (DateKind::Hours, true),
        DatePart::Minute => (DateKind::Minutes, false),
        DatePart::Minute2 => (DateKind::Minutes, true),
        DatePart::Second => (DateKind::Seconds, false),
        DatePart::Second2 => (DateKind::Seconds, true),
        // `mmmmm` (J), sub-second precision and the Buddhist calendar have no
        // ODF equivalent that would not change the displayed text.
        _ => return None,
    })
}

fn elapsed_part(part: ElapsedPart) -> Option<(DateKind, bool)> {
    Some(match part {
        ElapsedPart::Hours => (DateKind::Hours, false),
        ElapsedPart::Hours2 => (DateKind::Hours, true),
        ElapsedPart::Minutes => (DateKind::Minutes, false),
        ElapsedPart::Minutes2 => (DateKind::Minutes, true),
        ElapsedPart::Seconds => (DateKind::Seconds, false),
        ElapsedPart::Seconds2 => (DateKind::Seconds, true),
    })
}
