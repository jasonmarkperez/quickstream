//! `kit::big_digits`: the 3-row heavy box-drawing numerals of the Telemetry Now Playing screen
//! (style sheet 2a, mockups 2b and 3f).

mod common;

use ratatui::text::Span;

use quickstream::ui::kit::{self, big_digits, big_digits_width};

/// Columns `text` takes in the terminal.
fn width(text: &str) -> usize {
    Span::raw(text).width()
}

/// The numeral table of the 2a style sheet: `0` to `9`, one column apart.
const SHEET: [&str; 3] = [
    "┏━┓   ╻ ╺━┓ ╺━┓ ╻ ╻ ┏━╸ ┏━╸ ╺━┓ ┏━┓ ┏━┓",
    "┃ ┃   ┃ ┏━┛  ━┫ ┗━┫ ┗━┓ ┣━┓   ┃ ┣━┫ ┗━┫",
    "┗━┛   ╹ ┗━╸ ╺━┛   ╹ ╺━┛ ┗━┛   ╹ ┗━┛ ╺━┛",
];

#[test]
fn test_big_digits_match_the_style_sheet() {
    common::isolate();
    assert_eq!(big_digits("0123456789"), SHEET.map(String::from));
}

#[test]
fn test_each_digit_is_three_columns_of_its_sheet_glyph() {
    common::isolate();
    for (n, digit) in "0123456789".chars().enumerate() {
        let rows = big_digits(&digit.to_string());
        for (row, sheet) in rows.iter().zip(SHEET) {
            let glyph: String = sheet.chars().skip(n * 4).take(3).collect();
            assert_eq!(row, &glyph, "digit {digit}");
            assert_eq!(width(row), 3, "digit {digit}");
        }
    }
}

#[test]
fn test_colon_is_one_column_with_a_dot_on_the_middle_row_only() {
    common::isolate();
    assert_eq!(big_digits(":"), [" ", ":", " "].map(String::from));
    assert_eq!(big_digits_width(":"), 1);
}

#[test]
fn test_elapsed_time_matches_the_now_playing_mockup() {
    common::isolate();
    // 2b at 120×40: `02:41` next to the title.
    assert_eq!(
        big_digits("02:41"),
        ["┏━┓ ╺━┓   ╻ ╻   ╻", "┃ ┃ ┏━┛ : ┗━┫   ┃", "┗━┛ ┗━╸     ╹   ╹"].map(String::from)
    );
}

#[test]
fn test_width_counts_glyphs_and_one_column_gaps() {
    common::isolate();
    assert_eq!(big_digits_width(""), 0);
    assert_eq!(big_digits_width("7"), 3);
    assert_eq!(big_digits_width("02:41"), 17);
    assert_eq!(big_digits_width("1:15:00"), 23);
    for text in ["", "7", "02:41", "1:15:00", "0123456789"] {
        for row in big_digits(text) {
            assert_eq!(width(&row), big_digits_width(text), "{text:?}");
        }
    }
}

#[test]
fn test_big_digits_render_kit_durations() {
    common::isolate();
    // The text is whatever kit::duration gives, so hours work too.
    let text = kit::duration(4500.0);
    assert_eq!(text, "1:15:00");
    let rows = big_digits(&text);
    assert_eq!(rows[1], "  ┃ :   ┃ ┗━┓ : ┃ ┃ ┃ ┃");
}

#[test]
fn test_characters_without_a_glyph_are_left_out() {
    common::isolate();
    assert_eq!(big_digits("-4"), big_digits("4"));
    assert_eq!(big_digits_width("-4"), 3);
}
