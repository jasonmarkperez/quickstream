//! Building blocks of the Telemetry screen chrome: colour depth and palette (`ui::theme`), key
//! row, volume meter, progress bar and text fitting (`ui::kit`), and width tiers (`ui::screen`).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use quickstream::ui::kit::{self, ClockCache, KeyHint};
use quickstream::ui::screen::{Screen, WidthTier};
use quickstream::ui::theme::{self, ColorDepth, Palette};

/// The text of a line, concatenated from its spans.
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Display width of a line in terminal columns.
fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| s.width()).sum()
}

/// Number of `glyph` characters drawn in `fg` across `spans`.
fn count_glyphs(spans: &[Span], glyph: char, fg: Color) -> usize {
    spans
        .iter()
        .filter(|s| s.style.fg == Some(fg))
        .map(|s| s.content.chars().filter(|&c| c == glyph).count())
        .sum()
}

fn hint(keys: &str, label: &str, priority: u8) -> KeyHint {
    KeyHint::new(keys, label, priority)
}

// ---------------------------------------------------------------------------------------------
// theme
// ---------------------------------------------------------------------------------------------

#[test]
fn colorterm_truecolor_values_select_rgb() {
    common::isolate();
    for value in ["truecolor", "24bit", "TrueColor", "TRUECOLOR", "24BIT"] {
        assert_eq!(
            ColorDepth::from_colorterm(Some(value)),
            ColorDepth::TrueColor,
            "COLORTERM={value}"
        );
    }
}

#[test]
fn colorterm_other_values_select_indexed() {
    common::isolate();
    assert_eq!(ColorDepth::from_colorterm(None), ColorDepth::Indexed);
    for value in ["", "256color", "yes", "8bit", "truecolour"] {
        assert_eq!(
            ColorDepth::from_colorterm(Some(value)),
            ColorDepth::Indexed,
            "COLORTERM={value}"
        );
    }
}

#[test]
fn palettes_match_the_style_sheet() {
    common::isolate();
    let rgb = Palette::TRUECOLOR;
    assert_eq!(rgb.ground, Color::Rgb(0x14, 0x14, 0x13));
    assert_eq!(rgb.ink, Color::Rgb(0xE8, 0xE8, 0xE4));
    assert_eq!(rgb.mid, Color::Rgb(0x9A, 0x9A, 0x95));
    assert_eq!(rgb.dim, Color::Rgb(0x5F, 0x5F, 0x5B));
    assert_eq!(rgb.rule, Color::Rgb(0x2E, 0x2E, 0x2C));
    assert_eq!(rgb.error, Color::Rgb(0xED, 0x74, 0x73));

    let indexed = Palette::INDEXED;
    assert_eq!(indexed.ground, Color::Indexed(234));
    assert_eq!(indexed.ink, Color::Indexed(254));
    assert_eq!(indexed.mid, Color::Indexed(247));
    assert_eq!(indexed.dim, Color::Indexed(240));
    assert_eq!(indexed.rule, Color::Indexed(236));
    assert_eq!(indexed.error, Color::Indexed(210));

    assert_eq!(Palette::for_depth(ColorDepth::TrueColor), &Palette::TRUECOLOR);
    assert_eq!(Palette::for_depth(ColorDepth::Indexed), &Palette::INDEXED);
}

#[test]
fn palette_defaults_to_truecolor_when_never_initialised() {
    common::isolate();
    // Nothing in this test binary calls `theme::init`.
    assert_eq!(theme::palette(), &Palette::TRUECOLOR);
}

#[test]
fn styles_use_the_palette_tokens() {
    common::isolate();
    let p = Palette::TRUECOLOR;
    assert_eq!(theme::base(), Style::new().fg(p.ink).bg(p.ground));
    assert_eq!(theme::ink(), Style::new().fg(p.ink));
    assert_eq!(theme::mid(), Style::new().fg(p.mid));
    assert_eq!(theme::dim(), Style::new().fg(p.dim));
    assert_eq!(theme::rule(), Style::new().fg(p.rule));
    assert_eq!(theme::error(), Style::new().fg(p.error));
    assert_eq!(theme::label(true), Style::new().fg(p.ink).add_modifier(Modifier::BOLD));
    assert_eq!(theme::label(false), Style::new().fg(p.mid));
    assert_eq!(theme::keycap(), Style::new().fg(p.ink).bg(p.rule));

    // The cursor names its colours instead of using REVERSED, so it looks the same whatever
    // style the cell had before.
    let cursor = theme::cursor();
    assert_eq!(cursor.fg, Some(p.ground));
    assert_eq!(cursor.bg, Some(p.ink));
    assert!(cursor.add_modifier.contains(Modifier::BOLD));
    assert!(!cursor.add_modifier.contains(Modifier::REVERSED));
}

// ---------------------------------------------------------------------------------------------
// kit: key row
// ---------------------------------------------------------------------------------------------

fn now_playing_hints() -> Vec<KeyHint> {
    vec![
        hint("SPC", "PAUSE", 5),
        hint("←→", "TRACK", 4),
        hint("[ ]", "SEEK 10S", 3),
        hint("+−", "VOL", 2),
        hint("N", "QUEUE", 9),
        hint("?", "KEYS", 9),
    ]
}

#[test]
fn key_row_draws_keycaps_and_labels_four_columns_apart() {
    common::isolate();
    let hints = vec![hint("SPC", "PAUSE", 1), hint("?", "KEYS", 9)];
    let line = kit::key_row(&hints, 40, None);

    let p = Palette::TRUECOLOR;
    let spc = line.spans.iter().find(|s| s.content == " SPC ").expect("SPC keycap");
    assert_eq!(spc.style, theme::keycap());
    let pause = line.spans.iter().find(|s| s.content == " PAUSE").expect("PAUSE label");
    assert_eq!(pause.style.fg, Some(p.mid));
    assert!(line_text(&line).starts_with(" SPC  PAUSE     ?  KEYS"));
    assert_eq!(line_width(&line), 40, "the row fills the width it is given");
}

/// Columns the six hints above take with 4-column gaps.
const NOW_PLAYING_HINTS_WIDTH: usize = 80;

#[test]
fn key_row_keeps_every_hint_when_it_fits() {
    common::isolate();
    let line = kit::key_row(&now_playing_hints(), NOW_PLAYING_HINTS_WIDTH as u16, None);
    assert_eq!(line_width(&line), NOW_PLAYING_HINTS_WIDTH);
    let text = line_text(&line);
    for label in ["PAUSE", "TRACK", "SEEK 10S", "VOL", "QUEUE", "KEYS"] {
        assert!(text.contains(label), "{label} missing from {text:?}");
    }
}

#[test]
fn key_row_drops_lowest_priority_hints_first_and_keeps_order() {
    common::isolate();
    let hints = now_playing_hints();
    let without_vol = NOW_PLAYING_HINTS_WIDTH - " +−  VOL    ".chars().count();

    let line = kit::key_row(&hints, without_vol as u16, None);
    let text = line_text(&line);
    assert!(
        !text.contains("VOL"),
        "VOL (lowest priority) is dropped first: {text:?}"
    );
    assert!(text.contains("SEEK 10S"), "{text:?}");
    let order: Vec<usize> = ["PAUSE", "TRACK", "SEEK", "QUEUE", "KEYS"]
        .iter()
        .map(|l| text.find(l).unwrap_or_else(|| panic!("{l} missing from {text:?}")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "order kept: {text:?}");

    // Narrower: SEEK goes next, then TRACK, then PAUSE.
    let line = kit::key_row(&hints, (without_vol - 8) as u16, None);
    let text = line_text(&line);
    assert!(!text.contains("SEEK") && text.contains("TRACK"), "{text:?}");
}

#[test]
fn key_row_always_keeps_the_highest_priority_hints() {
    common::isolate();
    for width in [80u16, 40, 30, 20, 10, 0] {
        let line = kit::key_row(&now_playing_hints(), width, None);
        let text = line_text(&line);
        assert!(text.contains(" N  QUEUE"), "width {width}: {text:?}");
        assert!(text.contains(" ?  KEYS"), "width {width}: {text:?}");
    }
    let text = line_text(&kit::key_row(&now_playing_hints(), 24, None));
    assert!(!text.contains("PAUSE"), "{text:?}");
}

#[test]
fn key_row_right_part_is_right_aligned() {
    common::isolate();
    let hints = vec![hint("?", "KEYS", 9)];
    let right = vec![Span::styled("SAVED", theme::mid())];
    let line = kit::key_row(&hints, 30, Some(right));
    let text = line_text(&line);
    assert_eq!(line_width(&line), 30);
    assert!(text.starts_with(" ?  KEYS"), "{text:?}");
    assert!(text.ends_with("SAVED"), "{text:?}");
}

#[test]
fn key_row_drops_hints_to_make_room_for_the_right_part() {
    common::isolate();
    let right = vec![Span::styled(
        "Couldn't read /music/a very long folder name",
        theme::error(),
    )];
    let right_width: usize = right.iter().map(|s| s.width()).sum();
    let line = kit::key_row(&now_playing_hints(), 80, Some(right));
    let text = line_text(&line);
    assert!(line_width(&line) <= 80, "{text:?}");
    assert!(
        text.ends_with("Couldn't read /music/a very long folder name"),
        "{text:?}"
    );
    assert!(text.contains("KEYS") && text.contains("QUEUE"), "{text:?}");
    assert!(!text.contains("VOL"), "{text:?}");
    // Something had to go to fit the message.
    assert!(NOW_PLAYING_HINTS_WIDTH + 4 + right_width > 80);
}

#[test]
fn key_row_truncates_the_right_part_when_only_top_hints_remain() {
    common::isolate();
    let hints = vec![hint("?", "KEYS", 9)];
    let right = vec![Span::styled("x".repeat(60), theme::error())];
    let line = kit::key_row(&hints, 30, Some(right));
    let text = line_text(&line);
    assert_eq!(line_width(&line), 30, "{text:?}");
    assert!(text.starts_with(" ?  KEYS"), "{text:?}");
    assert!(text.ends_with('…'), "{text:?}");
    let last = line.spans.last().unwrap();
    assert_eq!(last.style, theme::error(), "the cut part keeps its style");
}

#[test]
fn top_hints_keeps_the_n_most_important_in_order() {
    common::isolate();
    let kept = kit::top_hints(&now_playing_hints(), 4);
    let labels: Vec<&str> = kept.iter().map(|h| h.label.as_str()).collect();
    assert_eq!(labels, vec!["PAUSE", "TRACK", "QUEUE", "KEYS"]);
    assert_eq!(kit::top_hints(&now_playing_hints(), 10).len(), 6);
}

// ---------------------------------------------------------------------------------------------
// kit: meters
// ---------------------------------------------------------------------------------------------

#[test]
fn volume_meter_draws_eleven_cells() {
    common::isolate();
    let p = Palette::TRUECOLOR;
    for (vol, filled) in [(0u8, 0usize), (4, 0), (5, 1), (50, 6), (65, 7), (100, 11), (150, 11)] {
        let spans = kit::volume_meter(vol);
        assert_eq!(count_glyphs(&spans, '▮', p.ink), filled, "volume {vol}");
        assert_eq!(count_glyphs(&spans, '▯', p.dim), 11 - filled, "volume {vol}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("VOL "), "{text:?}");
        assert!(text.ends_with(&format!(" {vol}")), "{text:?}");
    }
    let spans = kit::volume_meter(65);
    assert_eq!(spans[0].style.fg, Some(p.dim), "VOL is DIM");
    let number = spans.last().unwrap();
    assert_eq!(number.style.fg, Some(p.ink));
    assert!(number.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn progress_bar_splits_ink_and_rule() {
    common::isolate();
    let p = Palette::TRUECOLOR;
    for (width, ratio, ink) in [
        (42usize, 0.46, 19usize),
        (40, 0.5, 20),
        (26, 0.0, 0),
        (26, 1.0, 26),
        (10, 1.7, 10),
        (10, -0.3, 0),
        (10, f64::NAN, 0),
    ] {
        let spans = kit::progress_bar(width, ratio);
        assert_eq!(count_glyphs(&spans, '╱', p.ink), ink, "{width} at {ratio}");
        assert_eq!(count_glyphs(&spans, '╱', p.rule), width - ink, "{width} at {ratio}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with('▕') && text.ends_with('▏'), "{text:?}");
        assert_eq!(text.chars().count(), width + 2);
        assert_eq!(spans.first().unwrap().style.fg, Some(p.ink));
        assert_eq!(spans.last().unwrap().style.fg, Some(p.ink));
    }
}

// ---------------------------------------------------------------------------------------------
// kit: text
// ---------------------------------------------------------------------------------------------

#[test]
fn upper_uses_full_unicode_case_mapping() {
    common::isolate();
    assert_eq!(kit::upper("Halcyon Drift"), "HALCYON DRIFT");
    assert_eq!(kit::upper("Straße"), "STRASSE");
    assert_eq!(kit::upper("étoile"), "ÉTOILE");
}

#[test]
fn fit_pads_and_truncates_by_display_width() {
    common::isolate();
    assert_eq!(kit::fit("ABC", 6), "ABC   ");
    assert_eq!(kit::fit("ABCDEF", 6), "ABCDEF");
    assert_eq!(kit::fit("ABCDEFG", 6), "ABCDE…");
    assert_eq!(kit::fit("ABC", 0), "");
    assert_eq!(kit::fit("ABC", 1), "…");

    // Uppercasing can lengthen a string: ß → SS.
    assert_eq!(kit::fit(&kit::upper("straße"), 7), "STRASSE");
    assert_eq!(kit::fit(&kit::upper("straße"), 6), "STRAS…");

    // CJK characters take two columns; a cut never splits one and the result is always exactly
    // `width` columns.
    assert_eq!(kit::fit("日本語", 6), "日本語");
    assert_eq!(kit::fit("日本語", 8), "日本語  ");
    assert_eq!(kit::fit("日本語", 5), "日本…");
    assert_eq!(kit::fit("日本語", 4), "日… ");
    for width in 0..10 {
        assert_eq!(
            Span::raw(kit::fit("日本語のうた", width)).width(),
            width,
            "width {width}"
        );
    }
}

#[test]
fn visible_tail_keeps_the_end_of_long_text() {
    common::isolate();
    assert_eq!(kit::visible_tail("Road Trip", 20), "Road Trip");
    assert_eq!(kit::visible_tail("Road Trip", 9), "Road Trip");
    assert_eq!(kit::visible_tail("Road Trip", 5), "…Trip");
    assert_eq!(kit::visible_tail("日本語", 4), "…語");
    assert_eq!(kit::visible_tail("日本語", 5), "…本語");
    assert_eq!(kit::visible_tail("abc", 0), "…");
    assert_eq!(kit::char_width('a'), 1);
    assert_eq!(kit::char_width('語'), 2);
}

// ---------------------------------------------------------------------------------------------
// kit: clock
// ---------------------------------------------------------------------------------------------

static CLOCK_READS: AtomicUsize = AtomicUsize::new(0);

fn counting_clock() -> String {
    let n = CLOCK_READS.fetch_add(1, Ordering::SeqCst);
    format!("00:{n:02}")
}

#[test]
fn clock_cache_reads_the_clock_at_most_once_per_second() {
    common::isolate();
    let mut clock = ClockCache::with_source(counting_clock);
    let start = Instant::now();
    assert_eq!(clock.get_at(start), "00:00");
    assert_eq!(clock.get_at(start + Duration::from_millis(500)), "00:00");
    assert_eq!(clock.get_at(start + Duration::from_millis(999)), "00:00");
    assert_eq!(CLOCK_READS.load(Ordering::SeqCst), 1);
    assert_eq!(clock.get_at(start + Duration::from_secs(1)), "00:01");
    assert_eq!(clock.get_at(start + Duration::from_millis(1500)), "00:01");
    assert_eq!(CLOCK_READS.load(Ordering::SeqCst), 2);
}

#[test]
fn clock_cache_formats_local_time_as_hh_mm() {
    common::isolate();
    let mut clock = ClockCache::new();
    let text = clock.get().to_string();
    let bytes = text.as_bytes();
    assert_eq!(bytes.len(), 5, "{text:?}");
    assert!(bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit(), "{text:?}");
    assert_eq!(bytes[2], b':', "{text:?}");
    assert!(bytes[3].is_ascii_digit() && bytes[4].is_ascii_digit(), "{text:?}");
}

// ---------------------------------------------------------------------------------------------
// screen
// ---------------------------------------------------------------------------------------------

#[test]
fn width_tiers_follow_the_breakpoints() {
    common::isolate();
    for (width, tier) in [
        (0u16, WidthTier::Narrow),
        (40, WidthTier::Narrow),
        (79, WidthTier::Narrow),
        (80, WidthTier::Compact),
        (89, WidthTier::Compact),
        (90, WidthTier::Medium),
        (109, WidthTier::Medium),
        (110, WidthTier::Full),
        (200, WidthTier::Full),
    ] {
        assert_eq!(WidthTier::from_width(width), tier, "width {width}");
    }
    assert!(WidthTier::Narrow < WidthTier::Compact);
    assert!(WidthTier::Compact < WidthTier::Medium);
    assert!(WidthTier::Medium < WidthTier::Full);
}

#[test]
fn screen_records_tier_height_and_area() {
    common::isolate();
    let area = Rect::new(0, 0, 120, 40);
    let screen = Screen::new(area);
    assert_eq!(screen.tier, WidthTier::Full);
    assert!(!screen.short);
    assert_eq!(screen.area, area);

    let screen = Screen::new(Rect::new(0, 0, 80, 24));
    assert_eq!(screen.tier, WidthTier::Compact);
    assert!(!screen.short, "24 rows is not short");

    let screen = Screen::new(Rect::new(0, 0, 70, 23));
    assert_eq!(screen.tier, WidthTier::Narrow);
    assert!(screen.short);
}
